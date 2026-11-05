//! Orb portal link validation, balance fetch, aggregation and cache
//! (spec `65-orb-quota.md`).
//!
//! The link rules and the aggregation output shape below are fixed.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use crate::error::GatewayError;
use crate::proxy::Services;
use crate::upstream::UpstreamClient;

pub const PORTAL_BASE: &str = "https://portal.withorb.com";
pub const PORTAL_HOST: &str = "portal.withorb.com";
/// Cache TTLs (spec 65 §5).
pub const CACHE_OK_SECS: i64 = 300;
pub const CACHE_FAIL_SECS: i64 = 60;
/// Refresh cooldown per account (spec 65 §6.2).
pub const REFRESH_MIN_INTERVAL_SECS: i64 = 5;
pub const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// Portal request timeout: connect and total (spec 65 §2.3).
const PORTAL_TIMEOUT: Duration = Duration::from_secs(10);
/// Portal response cap (spec 65 §2.3, spec 70 §6).
const MAX_PORTAL_BODY: usize = 1024 * 1024;
/// Fake-portal override for local verification, the Orb analogue of
/// `ZED_UPSTREAM_BASE` (spec 75 §1). Never set in production.
const PORTAL_BASE_ENV: &str = "ZED_ORB_BASE";

/// Validation verdict for a pasted link (spec 65 §1). The stored value is the
/// raw input, unchanged.
pub fn validate_link(raw: &str) -> Result<String, OrbError> {
    if raw.is_empty() {
        return Err(OrbError::InvalidLink);
    }
    let parsed = url::Url::parse(raw).map_err(|_| OrbError::InvalidLink)?;
    if !parsed.scheme().eq_ignore_ascii_case("https") {
        return Err(OrbError::InvalidLink);
    }
    // `https://portal.withorb.com@evil.example/` parses as userinfo, so this
    // also rejects the look-alike form (spec 65 §1 row 3).
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(OrbError::InvalidLink);
    }
    // `:443` is the default and allowed; any other explicit port is not.
    if parsed.port().is_some_and(|port| port != 443) {
        return Err(OrbError::InvalidLink);
    }
    let host_ok = parsed
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case(PORTAL_HOST));
    if !host_ok {
        return Err(OrbError::InvalidLink);
    }
    if extract_token(raw).is_none() {
        return Err(OrbError::InvalidLink);
    }
    Ok(raw.to_string())
}

/// Extract the raw `token` query value, first occurrence wins, leaving its
/// percent-encoding untouched (spec 65 §1.1).
pub fn extract_token(raw: &str) -> Option<String> {
    let head = raw.split('#').next().unwrap_or("");
    let (_, query) = head.split_once('?')?;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key != "token" {
            continue;
        }
        if value.is_empty() || value.bytes().any(rejected_token_byte) {
            return None;
        }
        return Some(value.to_string());
    }
    None
}

/// Control characters, DEL, `"` and `\` never reach the portal URL
/// (spec 65 §1.1).
fn rejected_token_byte(byte: u8) -> bool {
    byte < 0x20 || byte == 0x7f || byte == b'"' || byte == b'\\'
}

/// Re-encode a token for the portal URL (spec 65 §1.2).
pub fn encode_token(token: &str) -> String {
    let mut encoded = String::with_capacity(token.len());
    for byte in token.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'%') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrbError {
    InvalidLink,
    Timeout,
    ParseError,
    FetchFailed,
}

impl OrbError {
    pub fn as_str(self) -> &'static str {
        match self {
            OrbError::InvalidLink => "invalid_link",
            OrbError::Timeout => "timeout",
            OrbError::ParseError => "parse_error",
            OrbError::FetchFailed => "fetch_failed",
        }
    }
}

/// The `credit` object: success (spec 65 §4.1) or failure (spec 65 §4.2).
#[derive(Debug, Clone, PartialEq)]
pub enum Credit {
    Ok {
        balance_usd: f64,
        cap_usd: f64,
        used_usd: f64,
        used_pct: f64,
        period_start: String,
        resets_at: String,
        total_balance_usd: f64,
    },
    Err(OrbError),
}

impl Credit {
    /// Value form used by `/zed/accounts/status` (spec 65 §4.1). `Value`
    /// cannot keep trailing zeros; the fixed-decimal form is produced by
    /// [`credit_json`] for the refresh endpoint.
    pub fn to_json(&self) -> Value {
        match self {
            Credit::Ok {
                balance_usd,
                cap_usd,
                used_usd,
                used_pct,
                period_start,
                resets_at,
                total_balance_usd,
            } => serde_json::json!({
                "ok": true,
                "balance_usd": round_to(*balance_usd, 4),
                "cap_usd": round_to(*cap_usd, 4),
                "used_usd": round_to(*used_usd, 4),
                "used_pct": round_to(*used_pct, 2),
                "period_start": period_start,
                "resets_at": resets_at,
                "total_balance_usd": round_to(*total_balance_usd, 4),
            }),
            Credit::Err(error) => serde_json::json!({"ok": false, "error": error.as_str()}),
        }
    }
}

fn round_to(value: f64, digits: i32) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    let factor = 10f64.powi(digits);
    (value * factor).round() / factor
}

fn usd(value: f64) -> String {
    format!("{:.4}", if value.is_finite() { value } else { 0.0 })
}

fn pct(value: f64) -> String {
    format!("{:.2}", if value.is_finite() { value } else { 0.0 })
}

/// Exact success/failure bytes, fixed key order and fixed decimals
/// (spec 65 §4.1, §4.2).
fn credit_json(credit: &Credit) -> String {
    match credit {
        Credit::Ok {
            balance_usd,
            cap_usd,
            used_usd,
            used_pct,
            period_start,
            resets_at,
            total_balance_usd,
        } => format!(
            r#"{{"ok":true,"balance_usd":{},"cap_usd":{},"used_usd":{},"used_pct":{},"period_start":{},"resets_at":{},"total_balance_usd":{}}}"#,
            usd(*balance_usd),
            usd(*cap_usd),
            usd(*used_usd),
            pct(*used_pct),
            quote(period_start),
            quote(resets_at),
            usd(*total_balance_usd)
        ),
        Credit::Err(error) => format!(r#"{{"ok":false,"error":"{}"}}"#, error.as_str()),
    }
}

fn quote(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

// ---------------------------------------------------------------------------
// Parsing (spec 65 §3)
// ---------------------------------------------------------------------------

/// `customer.id` and `customer.pricing_unit.id`, both already checked against
/// the injection rule (spec 65 §2.1, §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Customer {
    pub customer_id: String,
    pub pricing_unit_id: String,
}

pub fn parse_customer(value: &Value) -> Result<Customer, OrbError> {
    let customer = value.get("customer").ok_or(OrbError::ParseError)?;
    let customer_id = non_empty_str(customer.get("id")).ok_or(OrbError::ParseError)?;
    let unit = customer
        .get("pricing_unit")
        .and_then(Value::as_object)
        .ok_or(OrbError::ParseError)?;
    let pricing_unit_id = non_empty_str(unit.get("id")).ok_or(OrbError::ParseError)?;
    for id in [&customer_id, &pricing_unit_id] {
        let safe = id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if id.len() > 128 || !safe {
            return Err(OrbError::ParseError);
        }
    }
    Ok(Customer {
        customer_id,
        pricing_unit_id,
    })
}

fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// JSON number or decimal string; non-finite and overflowing values are
/// treated as missing (spec 65 §3.2).
fn parse_amount(value: Option<&Value>) -> Option<f64> {
    let number = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.parse::<f64>().ok()?,
        _ => return None,
    };
    number.is_finite().then_some(number)
}

/// Aggregate `credit_blocks` (spec 65 §3.2).
pub fn aggregate(ledger: &Value) -> Result<Credit, OrbError> {
    let root = ledger.as_object().ok_or(OrbError::ParseError)?;
    let blocks = root
        .get("credit_blocks")
        .and_then(Value::as_array)
        .ok_or(OrbError::ParseError)?;

    let mut balance = 0.0;
    let mut cap = 0.0;
    let mut counted = 0usize;
    let mut period_start: Option<String> = None;
    let mut resets_at: Option<String> = None;

    for block in blocks.iter().filter_map(Value::as_object) {
        let Some(block_balance) = parse_amount(block.get("balance")) else {
            continue;
        };
        counted += 1;
        balance += block_balance;
        cap += parse_amount(block.get("maximum_initial_balance")).unwrap_or(0.0);
        if let Some(date) = usable_date(block.get("effective_date")) {
            period_start = Some(period_start.map_or(date.clone(), |old| old.min(date)));
        }
        if let Some(date) = usable_date(block.get("expiry_date")) {
            resets_at = Some(resets_at.map_or(date.clone(), |old| old.min(date)));
        }
    }

    if counted == 0 {
        return Err(OrbError::ParseError);
    }
    let used = (cap - balance).max(0.0);
    let used_pct = if cap > 0.0 { used / cap * 100.0 } else { 0.0 };
    let total = parse_amount(root.get("credits_balance")).unwrap_or(balance);
    Ok(Credit::Ok {
        balance_usd: balance,
        cap_usd: cap,
        used_usd: used,
        used_pct,
        period_start: period_start.unwrap_or_default(),
        resets_at: resets_at.unwrap_or_default(),
        total_balance_usd: total,
    })
}

/// Non-empty string of at most 40 bytes (spec 65 §3.2).
fn usable_date(value: Option<&Value>) -> Option<String> {
    non_empty_str(value).filter(|date| date.len() <= 40)
}

// ---------------------------------------------------------------------------
// Portal requests (spec 65 §2)
// ---------------------------------------------------------------------------

fn portal_base() -> String {
    // Debug builds only: release binaries always use the fixed portal base
    // (spec 65 §2.1). The hook exists so local verification never touches the
    // real portal.
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var(PORTAL_BASE_ENV) {
        let trimmed = value.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    PORTAL_BASE.to_string()
}

fn portal_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(PORTAL_TIMEOUT)
            .timeout(PORTAL_TIMEOUT);
        if let Some(proxy) = crate::config::ProxyConfig::from_env() {
            let url = format!("{}://{}:{}", proxy.scheme, proxy.host, proxy.port);
            if let Ok(proxy) = reqwest::Proxy::all(url) {
                builder = builder.proxy(proxy);
            }
        }
        builder.build().unwrap_or_default()
    })
}

/// The `Referer` is the pasted link unless it cannot travel as a header
/// (spec 65 §2.2).
fn referer_for(link: &str) -> String {
    let unsafe_bytes = link
        .bytes()
        .any(|b| b < 0x20 || b == 0x7f || b == b'"' || b == b'\\');
    if unsafe_bytes || !link.is_ascii() {
        PORTAL_BASE.to_string()
    } else {
        link.to_string()
    }
}

async fn get_json(url: &str, referer: &str, non_2xx: OrbError) -> Result<Value, OrbError> {
    let response = portal_client()
        .get(url)
        .header(reqwest::header::USER_AGENT, BROWSER_USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/json, text/plain, */*")
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .header(reqwest::header::ORIGIN, PORTAL_BASE)
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-dest", "empty")
        .header("x-requested-with", "XMLHttpRequest")
        .header(reqwest::header::REFERER, referer)
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                OrbError::Timeout
            } else {
                OrbError::FetchFailed
            }
        })?;
    let status = response.status().as_u16();
    if let Some(length) = response.content_length() {
        if length as usize > MAX_PORTAL_BODY {
            return Err(OrbError::FetchFailed);
        }
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| {
            if err.is_timeout() {
                OrbError::Timeout
            } else {
                OrbError::FetchFailed
            }
        })?;
        if body.len() + chunk.len() > MAX_PORTAL_BODY {
            return Err(OrbError::FetchFailed);
        }
        body.extend_from_slice(&chunk);
    }
    if !(200..300).contains(&status) {
        return Err(non_2xx);
    }
    serde_json::from_slice(&body).map_err(|_| OrbError::ParseError)
}

/// Two GETs, no cache, no clock (spec 65 §2).
async fn portal_credit(base: &str, link: &str) -> Credit {
    let Some(token) = extract_token(link) else {
        return Credit::Err(OrbError::InvalidLink);
    };
    let encoded = encode_token(&token);
    let referer = referer_for(link);
    let customer_url = format!("{base}/api/v1/customer_from_link?token={encoded}");
    // The first request's non-2xx means the link itself is unusable; the
    // second one only means the ledger is unavailable (spec 65 §2.4).
    let customer = match get_json(&customer_url, &referer, OrbError::InvalidLink).await {
        Ok(value) => match parse_customer(&value) {
            Ok(customer) => customer,
            Err(error) => return Credit::Err(error),
        },
        Err(error) => return Credit::Err(error),
    };
    let ledger_url = format!(
        "{base}/api/v1/customers/{}/ledger_summary?pricing_unit_id={}&token={encoded}",
        customer.customer_id, customer.pricing_unit_id
    );
    match get_json(&ledger_url, &referer, OrbError::FetchFailed).await {
        Ok(value) => match aggregate(&value) {
            Ok(credit) => credit,
            Err(error) => Credit::Err(error),
        },
        Err(error) => Credit::Err(error),
    }
}

// ---------------------------------------------------------------------------
// Cache (spec 65 §5)
// ---------------------------------------------------------------------------

/// One cached portal result. Keyed by (portal base, stored link): an account
/// owns at most one link, so this is the per-account index without threading
/// the name through `fetch_balance`, and saving or clearing a link changes the
/// key (which is exactly the required invalidation). The base is part of the
/// key so the local fake portal cannot answer from production entries.
#[derive(Debug, Clone)]
struct CacheEntry {
    credit: Credit,
    fetched_at: i64,
    /// Last moment a real portal request was started, success or failure
    /// (spec 65 §6.2).
    forced_at: i64,
}

impl CacheEntry {
    fn is_fresh(&self, now: i64) -> bool {
        let ttl = if matches!(self.credit, Credit::Ok { .. }) {
            CACHE_OK_SECS
        } else {
            CACHE_FAIL_SECS
        };
        now.saturating_sub(self.fetched_at) < ttl
    }
}

type CacheKey = (String, String);

fn cache() -> MutexGuard<'static, HashMap<CacheKey, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, CacheEntry>>> = OnceLock::new();
    CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Fetch a balance for one account, cache first (spec 65 §5). The
/// `UpstreamClient` argument is fixed by the module contract but unused: the
/// portal is not a Zed endpoint, so this module owns its own HTTP client.
pub async fn fetch_balance(
    upstream: &dyn UpstreamClient,
    orb_url: &str,
) -> Result<Credit, OrbError> {
    let _ = upstream;
    fetch_balance_at(&portal_base(), orb_url, now_unix()).await
}

pub(crate) async fn fetch_balance_at(base: &str, link: &str, now: i64) -> Result<Credit, OrbError> {
    if extract_token(link).is_none() {
        return Err(OrbError::InvalidLink);
    }
    let key = (base.to_string(), link.to_string());
    if let Some(entry) = cache().get(&key) {
        if entry.is_fresh(now) {
            return Ok(entry.credit.clone());
        }
    }
    // No lock across the await: a slow portal never blocks other accounts
    // (spec 65 §5); a race costs at most one extra request.
    let credit = portal_credit(base, link).await;
    cache().insert(
        key,
        CacheEntry {
            credit: credit.clone(),
            fetched_at: now,
            forced_at: now,
        },
    );
    Ok(credit)
}

// ---------------------------------------------------------------------------
// Forced refresh: `POST /zed/accounts/orb/refresh` (spec 65 §6)
// ---------------------------------------------------------------------------

pub enum RefreshOutcome {
    Ok(Vec<u8>),
    BadRequest(&'static str),
    NotFound,
}

pub async fn refresh(services: &Services, body: &Value) -> RefreshOutcome {
    refresh_at(services, body, &portal_base(), now_unix()).await
}

/// One refresh decision for one account (spec 65 §6.2).
struct Refreshed {
    account: String,
    refreshed: bool,
    retry_after_s: i64,
    credit: Credit,
}

async fn refresh_one(base: &str, account: &str, link: &str, now: i64) -> Refreshed {
    let key = (base.to_string(), link.to_string());
    // 5 s floor per account, measured from the last real portal request
    // (spec 65 §6.2 step 2). The first refresh is never blocked.
    let (forced_at, cached) = match cache().get(&key) {
        Some(entry) => (entry.forced_at, entry.credit.clone()),
        None => (i64::MIN, Credit::Err(OrbError::FetchFailed)),
    };
    let elapsed = now.saturating_sub(forced_at);
    if elapsed < REFRESH_MIN_INTERVAL_SECS {
        return Refreshed {
            account: account.to_string(),
            refreshed: false,
            // Spec 65 §6.3 bounds this to 0–5; a backwards clock clamps.
            retry_after_s: (REFRESH_MIN_INTERVAL_SECS - elapsed)
                .clamp(0, REFRESH_MIN_INTERVAL_SECS),
            credit: cached,
        };
    }
    // Stamp before the request: a hung portal still occupies the window.
    cache()
        .entry(key.clone())
        .or_insert_with(|| CacheEntry {
            credit: cached.clone(),
            fetched_at: i64::MIN,
            forced_at: now,
        })
        .forced_at = now;
    let credit = portal_credit(base, link).await;
    {
        let mut guard = cache();
        let keep_old = matches!(credit, Credit::Err(_))
            && guard
                .get(&key)
                .is_some_and(|entry| matches!(entry.credit, Credit::Ok { .. }));
        if keep_old {
            // A failure keeps the last good numbers for the page (spec 65 §6.2
            // step 3); the response still reports the failure code.
            if let Some(entry) = guard.get_mut(&key) {
                entry.forced_at = now;
            }
        } else {
            guard.insert(
                key,
                CacheEntry {
                    credit: credit.clone(),
                    fetched_at: now,
                    forced_at: now,
                },
            );
        }
    }
    Refreshed {
        account: account.to_string(),
        refreshed: true,
        retry_after_s: 0,
        credit,
    }
}

pub(crate) async fn refresh_at(
    services: &Services,
    body: &Value,
    base: &str,
    now: i64,
) -> RefreshOutcome {
    let accounts = services.accounts.snapshots();
    let targets: Vec<(String, String)> = match body.get("account") {
        None | Some(Value::Null) => accounts
            .iter()
            .filter_map(|account| {
                account
                    .orb_url
                    .clone()
                    .map(|url| (account.name.clone(), url))
            })
            .collect(),
        Some(Value::String(name)) => {
            let Some(account) = accounts.iter().find(|account| &account.name == name) else {
                return RefreshOutcome::NotFound;
            };
            let Some(url) = account.orb_url.clone() else {
                return RefreshOutcome::BadRequest("orb link not configured");
            };
            vec![(account.name.clone(), url)]
        }
        Some(_) => return RefreshOutcome::BadRequest("account must be a string"),
    };

    let mut rows = Vec::with_capacity(targets.len());
    for (account, link) in &targets {
        rows.push(refresh_one(base, account, link, now).await);
    }

    let single = matches!(body.get("account"), Some(Value::String(_)));
    let any_refreshed = rows.iter().any(|row| row.refreshed);
    let mut out = String::new();
    if single {
        let row = rows.first();
        out.push_str(&format!(
            r#"{{"refreshed":{},"account":{},"retry_after_s":{},"credit":"#,
            any_refreshed,
            quote(row.map(|row| row.account.as_str()).unwrap_or_default()),
            row.map(|row| row.retry_after_s).unwrap_or(0)
        ));
        out.push_str(&credit_json(
            &row.map(|row| row.credit.clone())
                .unwrap_or(Credit::Err(OrbError::FetchFailed)),
        ));
        out.push('}');
    } else {
        out.push_str(&format!(r#"{{"refreshed":{any_refreshed},"accounts":["#));
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                r#"{{"account":{},"refreshed":{},"retry_after_s":{},"credit":{}}}"#,
                quote(&row.account),
                row.refreshed,
                row.retry_after_s,
                credit_json(&row.credit)
            ));
        }
        out.push_str("]}");
    }
    RefreshOutcome::Ok(out.into_bytes())
}

fn now_unix() -> i64 {
    crate::http::server::now_unix()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::client::{BoxFuture, UpstreamRequest, UpstreamResponse, UpstreamStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // ---- link validation (spec 65 §9, migrated) ----

    #[test]
    fn accepted_links() {
        for link in [
            "https://portal.withorb.com/?token=abc123",
            "https://portal.withorb.com/?x=1&token=abc123",
            "https://PORTAL.WITHORB.COM/?token=abc123",
            "https://portal.withorb.com:443/?token=abc123",
            "https://portal.withorb.com/portal/x?token=a%2Bb-c_d",
        ] {
            assert_eq!(validate_link(link).as_deref(), Ok(link), "{link}");
        }
    }

    #[test]
    fn rejected_links() {
        for link in [
            "http://portal.withorb.com/?token=abc",
            "https://portal.withorb.com.evil.test/?token=abc",
            "https://evil.portal.withorb.com/?token=abc",
            "https://portal.withorb.com/",
            "https://portal.withorb.com/?token=",
            "https://portal.withorb.com/?tok=abc",
            "not a url",
            "",
            "https://portal.withorb.com@evil.example/?token=abc",
            "https://user:pw@portal.withorb.com/?token=abc",
            "https://portal.withorb.com:8443/?token=abc",
        ] {
            assert_eq!(validate_link(link), Err(OrbError::InvalidLink), "{link:?}");
        }
    }

    #[test]
    fn stored_link_is_the_raw_input() {
        let link = "https://PORTAL.withorb.com/?b=2&token=XyZ%2F";
        assert_eq!(validate_link(link).as_deref(), Ok(link));
    }

    // ---- token extraction and encoding (spec 65 §9, migrated) ----

    #[test]
    fn token_extraction_keeps_the_encoding() {
        assert_eq!(
            extract_token("https://h/?token=abc123").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            extract_token("https://h/?x=1&token=abc").as_deref(),
            Some("abc")
        );
        assert_eq!(
            extract_token("https://h/?token=a%2Bb-c_d").as_deref(),
            Some("a%2Bb-c_d")
        );
        assert_eq!(extract_token("https://h/?token="), None);
        assert_eq!(
            extract_token("https://h/?token=a&token=b").as_deref(),
            Some("a")
        );
        assert_eq!(extract_token("https://h/?Token=abc"), None);
        // Values may contain `=`.
        assert_eq!(
            extract_token("https://h/?token=a=b").as_deref(),
            Some("a=b")
        );
        // `%22` is accepted, a bare quote is not.
        assert_eq!(
            extract_token("https://h/?token=%22").as_deref(),
            Some("%22")
        );
        assert_eq!(extract_token("https://h/?token=a\"b"), None);
        assert_eq!(extract_token("https://h/?token=a\\b"), None);
        assert_eq!(extract_token("https://h/?token=a\tb"), None);
    }

    #[test]
    fn token_encoding_preserves_already_encoded_bytes() {
        assert_eq!(encode_token("abc-123_4.5~6"), "abc-123_4.5~6");
        assert_eq!(encode_token("a&b#c=d e"), "a%26b%23c%3Dd%20e");
        assert_eq!(encode_token("a%2Bb"), "a%2Bb");
    }

    #[test]
    fn error_codes_are_the_four_documented_values() {
        assert_eq!(OrbError::InvalidLink.as_str(), "invalid_link");
        assert_eq!(OrbError::Timeout.as_str(), "timeout");
        assert_eq!(OrbError::ParseError.as_str(), "parse_error");
        assert_eq!(OrbError::FetchFailed.as_str(), "fetch_failed");
    }

    #[test]
    fn failure_credit_has_fixed_shape() {
        assert_eq!(
            Credit::Err(OrbError::Timeout).to_json(),
            serde_json::json!({"ok": false, "error": "timeout"})
        );
        assert_eq!(
            credit_json(&Credit::Err(OrbError::Timeout)),
            r#"{"ok":false,"error":"timeout"}"#
        );
    }

    // ---- customer parsing (spec 65 §9, migrated) ----

    #[test]
    fn customer_parsing_needs_both_ids() {
        let value = serde_json::json!({"customer": {"id": "cust_1", "pricing_unit": {"id": "pu_9"}, "name": "x"}});
        assert_eq!(
            parse_customer(&value),
            Ok(Customer {
                customer_id: "cust_1".into(),
                pricing_unit_id: "pu_9".into()
            })
        );
        for bad in [
            serde_json::json!({"customer": {}}),
            serde_json::json!({"customer": {"id": "c"}}),
            serde_json::json!({"customer": {"id": "c", "pricing_unit": {}}}),
            serde_json::json!({"customer": {"id": "", "pricing_unit": {"id": "p"}}}),
            serde_json::json!({"customer": {"id": "c/../x", "pricing_unit": {"id": "p"}}}),
            serde_json::json!({"customer": {"id": "c", "pricing_unit": {"id": "p q"}}}),
            serde_json::json!([]),
        ] {
            assert_eq!(parse_customer(&bad), Err(OrbError::ParseError), "{bad}");
        }
        let long = "a".repeat(129);
        assert_eq!(
            parse_customer(
                &serde_json::json!({"customer": {"id": long, "pricing_unit": {"id": "p"}}})
            ),
            Err(OrbError::ParseError)
        );
    }

    // ---- ledger aggregation (spec 65 §9, migrated) ----

    fn sample_ledger() -> Value {
        serde_json::json!({
            "credit_blocks": [
                {"balance":"9.9597","maximum_initial_balance":"10.00","effective_date":"2026-11-04T00:00:00+00:00","expiry_date":"2026-12-04T00:00:00+00:00"},
                {"balance":null,"maximum_initial_balance":"5.00","effective_date":"2026-10-01T00:00:00+00:00","expiry_date":"2026-11-01T00:00:00+00:00"},
                {"balance":2.5,"maximum_initial_balance":5,"effective_date":"2026-10-20T00:00:00+00:00","expiry_date":"2026-11-20T00:00:00+00:00"}
            ],
            "credits_balance":"12.4597"
        })
    }

    #[test]
    fn aggregation_matches_the_documented_sample() {
        let credit = aggregate(&sample_ledger()).unwrap();
        let Credit::Ok {
            balance_usd,
            cap_usd,
            used_usd,
            used_pct,
            period_start,
            resets_at,
            total_balance_usd,
        } = credit
        else {
            panic!("expected ok");
        };
        assert!((balance_usd - 12.4597).abs() < 1e-9);
        assert!((cap_usd - 15.0).abs() < 1e-9);
        assert!((used_usd - 2.5403).abs() < 1e-9);
        assert!((used_pct - 16.935_333_333_3).abs() < 1e-6);
        assert_eq!(period_start, "2026-10-20T00:00:00+00:00");
        assert_eq!(resets_at, "2026-11-20T00:00:00+00:00");
        assert!((total_balance_usd - 12.4597).abs() < 1e-9);
        let json = credit_json(&aggregate(&sample_ledger()).unwrap());
        assert_eq!(
            json,
            r#"{"ok":true,"balance_usd":12.4597,"cap_usd":15.0000,"used_usd":2.5403,"used_pct":16.94,"period_start":"2026-10-20T00:00:00+00:00","resets_at":"2026-11-20T00:00:00+00:00","total_balance_usd":12.4597}"#
        );
    }

    #[test]
    fn aggregation_falls_back_and_rejects_empty_data() {
        let credit = aggregate(&serde_json::json!({
            "credit_blocks": [{"balance": 1.5}]
        }))
        .unwrap();
        assert!(matches!(
            credit,
            Credit::Ok { total_balance_usd, .. } if (total_balance_usd - 1.5).abs() < 1e-9
        ));
        assert_eq!(
            aggregate(&serde_json::json!({"credit_blocks": []})).unwrap_err(),
            OrbError::ParseError
        );
        assert_eq!(
            aggregate(&serde_json::json!([])).unwrap_err(),
            OrbError::ParseError
        );
        assert_eq!(
            aggregate(&serde_json::json!({"credit_blocks": [{"balance": null}]})).unwrap_err(),
            OrbError::ParseError
        );
    }

    #[test]
    fn non_finite_amounts_are_ignored() {
        let credit = aggregate(&serde_json::json!({
            "credit_blocks": [
                {"balance": "nan", "maximum_initial_balance": "inf"},
                {"balance": "inf"},
                {"balance": 2.5, "maximum_initial_balance": "nan"}
            ]
        }))
        .unwrap();
        let json = credit_json(&credit);
        assert!(json.contains(r#""balance_usd":2.5000"#), "{json}");
        assert!(json.contains(r#""cap_usd":0.0000"#), "{json}");
        assert!(json.contains(r#""used_pct":0.00"#), "{json}");
        assert!(!json.contains("NaN") && !json.contains("inf"), "{json}");
    }

    #[test]
    fn dates_are_only_read_from_short_strings() {
        let credit = aggregate(&serde_json::json!({
            "credit_blocks": [
                {"balance": 1, "effective_date": 5, "expiry_date": ""},
                {"balance": 1, "effective_date": "2026-01-01", "expiry_date": "x".repeat(41)}
            ]
        }))
        .unwrap();
        let Credit::Ok {
            period_start,
            resets_at,
            ..
        } = credit
        else {
            panic!("expected ok");
        };
        assert_eq!(period_start, "2026-01-01");
        assert_eq!(resets_at, "");
    }

    // ---- behaviour tests against a fake portal (spec 65 §9, new) ----

    /// Fake portal serving canned responses per path on the current tokio
    /// runtime; counts requests so cache hits are observable.
    struct FakePortal {
        base: String,
        hits: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakePortal {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn fake_portal(responses: Vec<(&'static str, u16, &'static str)>) -> FakePortal {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let read = socket.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let text = String::from_utf8_lossy(&request);
                let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
                let (status, body) = responses
                    .iter()
                    .find(|(needle, _, _)| path.contains(needle))
                    .map(|(_, status, body)| (*status, *body))
                    .unwrap_or((404, "{}"));
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        FakePortal {
            base: format!("http://{address}"),
            hits,
            task,
        }
    }

    const CUSTOMER_OK: &str = r#"{"customer":{"id":"cust_1","pricing_unit":{"id":"pu_9"}}}"#;
    const LEDGER_OK: &str = r#"{"credit_blocks":[{"balance":"7.5","maximum_initial_balance":"10.00","effective_date":"2026-11-01","expiry_date":"2026-12-01"}],"credits_balance":"7.5"}"#;

    struct NoUpstream;

    impl UpstreamClient for NoUpstream {
        fn execute<'a>(
            &'a self,
            _: UpstreamRequest,
        ) -> BoxFuture<'a, Result<UpstreamResponse, GatewayError>> {
            Box::pin(async { Err(GatewayError::internal("not used")) })
        }
        fn execute_stream<'a>(
            &'a self,
            _: UpstreamRequest,
        ) -> BoxFuture<'a, Result<UpstreamStream, GatewayError>> {
            Box::pin(async { Err(GatewayError::internal("not used")) })
        }
    }

    const LINK: &str = "https://portal.withorb.com/?token=abc123";

    #[tokio::test]
    async fn first_request_4xx_is_invalid_link_and_second_is_fetch_failed() {
        let portal = fake_portal(vec![("customer_from_link", 401, "{}")]).await;
        let credit = fetch_balance_at(&portal.base, LINK, 1).await.unwrap();
        assert_eq!(credit, Credit::Err(OrbError::InvalidLink));
        drop(portal);

        let portal = fake_portal(vec![
            ("customer_from_link", 200, CUSTOMER_OK),
            ("ledger_summary", 500, "{}"),
        ])
        .await;
        let credit = fetch_balance_at(&portal.base, LINK, 1).await.unwrap();
        assert_eq!(credit, Credit::Err(OrbError::FetchFailed));
    }

    #[tokio::test]
    async fn successful_fetch_is_cached_for_300s_and_failures_for_60s() {
        let portal = fake_portal(vec![
            ("customer_from_link", 200, CUSTOMER_OK),
            ("ledger_summary", 200, LEDGER_OK),
        ])
        .await;
        let credit = fetch_balance_at(&portal.base, LINK, 1_000).await.unwrap();
        assert!(matches!(credit, Credit::Ok { .. }));
        // Cached: a second read inside 300 s makes no extra connection.
        let again = fetch_balance_at(&portal.base, LINK, 1_000 + CACHE_OK_SECS - 1)
            .await
            .unwrap();
        assert_eq!(again, credit);
        assert_eq!(portal.hits.load(Ordering::SeqCst), 2);

        let failing = fake_portal(vec![
            ("customer_from_link", 200, CUSTOMER_OK),
            ("ledger_summary", 500, "{}"),
        ])
        .await;
        let credit = fetch_balance_at(&failing.base, LINK, 5_000).await.unwrap();
        assert_eq!(credit, Credit::Err(OrbError::FetchFailed));
        let again = fetch_balance_at(&failing.base, LINK, 5_000 + CACHE_FAIL_SECS - 1)
            .await
            .unwrap();
        assert_eq!(again, Credit::Err(OrbError::FetchFailed));
        // Fetched once (two requests), then served from the 60 s cache.
        assert_eq!(failing.hits.load(Ordering::SeqCst), 2);
    }

    /// A new link is a new cache key, which is exactly the "save clears the
    /// cache" behaviour of spec 65 §5.
    #[tokio::test]
    async fn a_new_link_is_not_served_from_the_old_cache() {
        let portal = fake_portal(vec![
            ("customer_from_link", 200, CUSTOMER_OK),
            ("ledger_summary", 200, LEDGER_OK),
        ])
        .await;
        let _ = fetch_balance_at(&portal.base, LINK, 10).await;
        let _ = fetch_balance_at(&portal.base, "https://portal.withorb.com/?token=other", 10).await;
        assert_eq!(portal.hits.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn empty_and_malformed_portal_bodies_are_parse_errors() {
        let portal = fake_portal(vec![("customer_from_link", 200, "not json")]).await;
        let credit = fetch_balance_at(&portal.base, LINK, 20).await.unwrap();
        assert_eq!(credit, Credit::Err(OrbError::ParseError));

        let portal = fake_portal(vec![
            ("customer_from_link", 200, CUSTOMER_OK),
            ("ledger_summary", 200, r#"{"credit_blocks":[]}"#),
        ])
        .await;
        let credit = fetch_balance_at(&portal.base, LINK, 21).await.unwrap();
        assert_eq!(credit, Credit::Err(OrbError::ParseError));
    }

    #[tokio::test]
    async fn unreachable_portal_is_fetch_failed() {
        // Bind then drop the listener so the port refuses connections.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let credit = fetch_balance_at(&base, LINK, 30).await.unwrap();
        assert_eq!(credit, Credit::Err(OrbError::FetchFailed));
    }

    #[tokio::test]
    async fn plain_validate_runs_before_any_request() {
        let upstream = NoUpstream;
        let credit = fetch_balance(&upstream, "http://portal.withorb.com/?token=a")
            .await
            .unwrap();
        assert_eq!(credit, Credit::Err(OrbError::InvalidLink));
    }

    // ---- forced refresh (spec 65 §6) ----

    /// Account store with two linked accounts in file order.
    fn refresh_services(dir: &std::path::Path) -> Services {
        let accounts = crate::accounts::AccountStore::load(dir, 0);
        accounts
            .add_account("alice", "42", serde_json::json!({"access_token": "x"}))
            .unwrap();
        accounts
            .add_account("bob", "7", serde_json::json!({"access_token": "y"}))
            .unwrap();
        accounts.set_orb_url("alice", Some(LINK)).unwrap();
        accounts
            .set_orb_url("bob", Some("https://portal.withorb.com/?token=bob-token"))
            .unwrap();
        Services {
            catalog: crate::catalog::CatalogHandle::new(crate::catalog::Catalog::builtin_or_empty()),
            accounts,
            upstream: Box::new(NoUpstream),
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zprs-orb-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn body_text(outcome: RefreshOutcome) -> String {
        match outcome {
            RefreshOutcome::Ok(bytes) => String::from_utf8(bytes).unwrap(),
            RefreshOutcome::BadRequest(message) => panic!("bad request: {message}"),
            RefreshOutcome::NotFound => panic!("not found"),
        }
    }

    #[tokio::test]
    async fn refresh_bypasses_the_ok_cache_but_respects_the_five_second_floor() {
        let dir = temp_dir("refresh-floor");
        let services = refresh_services(&dir);
        let portal = fake_portal(vec![
            ("customer_from_link", 200, CUSTOMER_OK),
            ("ledger_summary", 200, LEDGER_OK),
        ])
        .await;
        // Prime the cache through the passive path.
        let _ = fetch_balance_at(&portal.base, LINK, 1_000).await;
        assert_eq!(portal.hits.load(Ordering::SeqCst), 2);

        // A forced refresh 1 s later is blocked by the 5 s floor.
        let body = serde_json::json!({"account": "alice"});
        let text = body_text(refresh_at(&services, &body, &portal.base, 1_001).await);
        assert_eq!(portal.hits.load(Ordering::SeqCst), 2, "{text}");
        assert!(
            text.starts_with(
                r#"{"refreshed":false,"account":"alice","retry_after_s":4,"credit":{"ok":true"#
            ),
            "{text}"
        );

        // At 5 s the refresh really goes to the portal.
        let text = body_text(refresh_at(&services, &body, &portal.base, 1_005).await);
        assert!(
            text.starts_with(
                r#"{"refreshed":true,"account":"alice","retry_after_s":0,"credit":{"ok":true"#
            ),
            "{text}"
        );
        assert_eq!(portal.hits.load(Ordering::SeqCst), 4);
        assert_eq!(
            text,
            r#"{"refreshed":true,"account":"alice","retry_after_s":0,"credit":{"ok":true,"balance_usd":7.5000,"cap_usd":10.0000,"used_usd":2.5000,"used_pct":25.00,"period_start":"2026-11-01","resets_at":"2026-12-01","total_balance_usd":7.5000}}"#
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failed_refresh_keeps_the_last_good_credit() {
        let dir = temp_dir("refresh-keep");
        let services = refresh_services(&dir);
        let good = fake_portal(vec![
            ("customer_from_link", 200, CUSTOMER_OK),
            ("ledger_summary", 200, LEDGER_OK),
        ])
        .await;
        let body = serde_json::json!({"account": "alice"});
        let _ = fetch_balance_at(&good.base, LINK, 100).await;

        // Same base for the failure: the cache survives the portal going away.
        let down = "http://127.0.0.1:1";
        let text = body_text(refresh_at(&services, &body, down, 200).await);
        assert!(
            text.contains(r#""refreshed":true"#)
                && text.contains(r#""credit":{"ok":false,"error":"fetch_failed"}"#),
            "{text}"
        );
        // The page still has the old numbers: the passive read serves them.
        let credit = fetch_balance_at(&good.base, LINK, 250).await.unwrap();
        assert!(matches!(credit, Credit::Ok { .. }));
        assert_eq!(good.hits.load(Ordering::SeqCst), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn refresh_all_reports_every_linked_account_in_file_order() {
        let dir = temp_dir("refresh-all");
        let services = refresh_services(&dir);
        let portal = fake_portal(vec![
            ("customer_from_link", 200, CUSTOMER_OK),
            ("ledger_summary", 200, LEDGER_OK),
        ])
        .await;
        let text = body_text(refresh_at(&services, &Value::Null, &portal.base, 10).await);
        assert!(
            text.starts_with(r#"{"refreshed":true,"accounts":[{"account":"alice","refreshed":true,"retry_after_s":0,"#),
            "{text}"
        );
        assert!(
            text.contains(r#"{"account":"bob","refreshed":true,"retry_after_s":0,"#),
            "{text}"
        );
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["accounts"].as_array().unwrap().len(), 2);

        // No account carries a link: 200 with an empty list, no portal access.
        let empty_dir = temp_dir("refresh-empty");
        let empty = crate::accounts::AccountStore::load(&empty_dir, 0);
        empty
            .add_account("carol", "9", serde_json::json!({"access_token": "z"}))
            .unwrap();
        let services = Services {
            catalog: crate::catalog::CatalogHandle::new(crate::catalog::Catalog::builtin_or_empty()),
            accounts: empty,
            upstream: Box::new(NoUpstream),
        };
        assert_eq!(
            body_text(refresh_at(&services, &Value::Null, &portal.base, 20).await),
            r#"{"refreshed":false,"accounts":[]}"#
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty_dir);
    }

    #[tokio::test]
    async fn refresh_rejects_unknown_and_unlinked_accounts() {
        let dir = temp_dir("refresh-errors");
        let services = refresh_services(&dir);
        let base = "http://127.0.0.1:1";
        assert!(matches!(
            refresh_at(&services, &serde_json::json!({"account": "ghost"}), base, 1).await,
            RefreshOutcome::NotFound
        ));
        assert!(matches!(
            refresh_at(&services, &serde_json::json!({"account": 5}), base, 1).await,
            RefreshOutcome::BadRequest("account must be a string")
        ));
        services.accounts.set_orb_url("alice", None).unwrap();
        assert!(matches!(
            refresh_at(&services, &serde_json::json!({"account": "alice"}), base, 1).await,
            RefreshOutcome::BadRequest("orb link not configured")
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The response bytes never carry the link or the token (spec 65 §4.3).
    #[test]
    fn credit_json_never_contains_the_token() {
        let credit = aggregate(&sample_ledger()).unwrap();
        let json = credit_json(&credit);
        assert!(!json.contains("token"), "{json}");
        assert!(!json.contains("withorb"), "{json}");
        let failure = credit_json(&Credit::Err(OrbError::InvalidLink));
        assert_eq!(failure, r#"{"ok":false,"error":"invalid_link"}"#);
    }
}
