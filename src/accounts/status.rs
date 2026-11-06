//! Scheduler-facing status values shared by `/zed/accounts` and
//! `/zed/accounts/status` (spec 50 §8, spec 80 §3), plus the passive
//! token/billing check and the exact response bodies (spec 70 §3.6–§3.7).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::error::{ErrorKind, GatewayError};
use crate::upstream::client::UpstreamClient;
use crate::upstream::token::{cached_or_minted, me_request, TokenCache};

use super::store::{AccountSnapshot, AccountStore};

/// Probe state, separate from the scheduler cooldown (spec 50 §1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccountModelState {
    #[default]
    Unchecked,
    Healthy,
    AuthError,
    RateLimited,
    UpstreamError,
}

impl AccountModelState {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountModelState::Unchecked => "unchecked",
            AccountModelState::Healthy => "healthy",
            AccountModelState::AuthError => "auth_error",
            AccountModelState::RateLimited => "rate_limited",
            AccountModelState::UpstreamError => "upstream_error",
        }
    }

    /// Probe failures are charged with the same classification as requests
    /// (spec 80 §2.3).
    pub fn from_gateway_error(err: &GatewayError) -> Self {
        match err.kind {
            ErrorKind::Auth => AccountModelState::AuthError,
            ErrorKind::RateLimit => AccountModelState::RateLimited,
            _ => AccountModelState::UpstreamError,
        }
    }
}

/// Scheduler half of one `/zed/accounts` entry (spec 50 §8.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerStatus {
    pub name: String,
    pub user_id: String,
    pub current: bool,
    pub healthy: bool,
    pub cooldown_s: i64,
    pub consecutive_failures: u32,
    pub last_status: u16,
}

/// Probe half of one `/zed/accounts/status` entry (spec 50 §8.2, spec 80 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountStatusReport {
    pub name: String,
    pub user_id: String,
    pub current: bool,
    pub scheduler_healthy: bool,
    pub cooldown_s: i64,
    pub last_status: u16,
    pub model_state: AccountModelState,
    pub model_ok: bool,
    pub model_checked_at: i64,
    pub model_latency_ms: u64,
    pub orb_configured: bool,
    /// Credit object; `None` when no Orb link is configured.
    pub credit: Option<Value>,
    pub check_ok: bool,
    pub token_ok: bool,
    pub billing_ok: bool,
    pub usable: bool,
    pub quota_state: &'static str,
    pub plan: Option<String>,
    pub used: Option<Value>,
    pub limit: Option<Value>,
    pub remaining: Option<Value>,
    pub subscription_ends_at: Option<String>,
    pub usage_based_billing: bool,
    pub overdue: bool,
    pub account_too_young: bool,
    pub error: Option<&'static str>,
}

/// Credential-free billing summary of one `client/users/me` response
/// (spec 70 §3.7). Fields are optional because Zed plan versions differ.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BillingSummary {
    pub plan: Option<String>,
    pub used: Option<i64>,
    pub limit: Option<i64>,
    pub subscription_ends_at: Option<String>,
    pub usage_based_billing: bool,
    pub overdue: bool,
    pub account_too_young: bool,
    pub restricted: bool,
}

// ---------------------------------------------------------------------------
// Response bodies (spec 70 §3.6, §3.7: fixed key order, compact JSON)
// ---------------------------------------------------------------------------

/// `GET /zed/accounts` (spec 70 §3.6).
pub fn accounts_body(store: &AccountStore, now: i64) -> Vec<u8> {
    let rows = scheduler_rows(store, now);
    let current = store.current_name();
    let mut out = String::from("{\"accounts\":[");
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        push_string(&mut out, &row.name);
        out.push_str(",\"user_id\":");
        push_string(&mut out, &row.user_id);
        out.push_str(&format!(
            ",\"current\":{},\"healthy\":{},\"cooldown_s\":{},\"consecutive_failures\":{},\"last_status\":{}}}",
            row.current, row.healthy, row.cooldown_s, row.consecutive_failures, row.last_status
        ));
    }
    out.push_str("],\"current\":");
    push_string(&mut out, &current);
    out.push('}');
    out.into_bytes()
}

/// Scheduler rows for both account endpoints, in file order (spec 50 §8).
pub fn scheduler_rows(store: &AccountStore, now: i64) -> Vec<SchedulerStatus> {
    let snapshots = store.snapshots();
    let current = store.current_name();
    snapshots
        .iter()
        .map(|account| SchedulerStatus {
            name: account.name.clone(),
            user_id: account.user_id.clone(),
            current: account.name == current,
            healthy: now >= account.disabled_until,
            cooldown_s: (account.disabled_until - now).max(0),
            consecutive_failures: account.consecutive_failures,
            last_status: account.last_status,
        })
        .collect()
}

/// `GET /zed/accounts/status` (spec 70 §3.7). Accounts are checked one at a
/// time; the Orb credit is fetched once per configured link (spec 65 §5).
pub async fn status_body(store: &AccountStore, upstream: &dyn UpstreamClient, now: i64) -> Vec<u8> {
    let snapshots = store.snapshots();
    let current = store.current_name();
    let mut out = String::from("{\"checked_at\":");
    out.push_str(&now.to_string());
    out.push_str(",\"accounts\":[");
    for (index, account) in snapshots.iter().enumerate() {
        let credit = match &account.orb_url {
            Some(url) => Some(match crate::orb::fetch_balance(upstream, url).await {
                Ok(credit) => credit.to_json(),
                Err(error) => crate::orb::Credit::Err(error).to_json(),
            }),
            None => None,
        };
        let report = check_account(store, upstream, account, &current, now, credit).await;
        if index > 0 {
            out.push(',');
        }
        push_report(&mut out, &report);
    }
    out.push_str("]}");
    out.into_bytes()
}

fn push_string(out: &mut String, value: &str) {
    match serde_json::to_string(value) {
        Ok(text) => out.push_str(&text),
        Err(_) => out.push_str("\"\""),
    }
}

fn push_opt_string(out: &mut String, value: &Option<String>) {
    match value {
        Some(text) => push_string(out, text),
        None => out.push_str("null"),
    }
}

fn push_opt_number(out: &mut String, value: &Option<Value>) {
    match value {
        Some(value) => match serde_json::to_string(value) {
            Ok(text) => out.push_str(&text),
            Err(_) => out.push_str("null"),
        },
        None => out.push_str("null"),
    }
}

fn push_report(out: &mut String, report: &AccountStatusReport) {
    out.push_str("{\"name\":");
    push_string(out, &report.name);
    out.push_str(",\"user_id\":");
    push_string(out, &report.user_id);
    out.push_str(&format!(
        ",\"current\":{},\"scheduler_healthy\":{},\"cooldown_s\":{},\"last_status\":{},\"model_state\":\"{}\",\"model_ok\":{},\"model_checked_at\":{},\"model_latency_ms\":{},\"orb_configured\":{}",
        report.current,
        report.scheduler_healthy,
        report.cooldown_s,
        report.last_status,
        report.model_state.as_str(),
        report.model_ok,
        report.model_checked_at,
        report.model_latency_ms,
        report.orb_configured
    ));
    if let Some(credit) = &report.credit {
        out.push_str(",\"credit\":");
        match serde_json::to_string(credit) {
            Ok(text) => out.push_str(&text),
            Err(_) => out.push_str("null"),
        }
    }
    out.push_str(&format!(
        ",\"check_ok\":{},\"token_ok\":{},\"billing_ok\":{},\"usable\":{},\"quota_state\":\"{}\"",
        report.check_ok, report.token_ok, report.billing_ok, report.usable, report.quota_state
    ));
    // A failed token refresh ends the entry: no billing fields follow
    // (spec 70 §3.7).
    if report.error == Some("llm_token_refresh_failed") {
        out.push_str(",\"error\":\"llm_token_refresh_failed\"}");
        return;
    }
    out.push_str(",\"plan\":");
    push_opt_string(out, &report.plan);
    out.push_str(",\"used\":");
    push_opt_number(out, &report.used);
    out.push_str(",\"limit\":");
    push_opt_number(out, &report.limit);
    out.push_str(",\"remaining\":");
    push_opt_number(out, &report.remaining);
    out.push_str(",\"subscription_ends_at\":");
    push_opt_string(out, &report.subscription_ends_at);
    out.push_str(&format!(
        ",\"usage_based_billing\":{},\"overdue\":{},\"account_too_young\":{}",
        report.usage_based_billing, report.overdue, report.account_too_young
    ));
    if let Some(error) = report.error {
        out.push_str(",\"error\":");
        push_string(out, error);
    }
    out.push('}');
}

// ---------------------------------------------------------------------------
// Passive check: reuse a fresh LLM token, then pull the billing profile
// ---------------------------------------------------------------------------

async fn check_account(
    store: &AccountStore,
    upstream: &dyn UpstreamClient,
    account: &AccountSnapshot,
    current: &str,
    now: i64,
    credit: Option<Value>,
) -> AccountStatusReport {
    let mut report = AccountStatusReport {
        name: account.name.clone(),
        user_id: account.user_id.clone(),
        current: account.name == current,
        scheduler_healthy: now >= account.disabled_until,
        cooldown_s: (account.disabled_until - now).max(0),
        last_status: account.last_status,
        model_state: account.model_state,
        model_ok: account.model_state == AccountModelState::Healthy,
        model_checked_at: account.model_checked_at,
        model_latency_ms: account.model_latency_ms,
        orb_configured: account.orb_url.is_some(),
        credit,
        check_ok: false,
        token_ok: false,
        billing_ok: false,
        usable: false,
        quota_state: "unavailable",
        plan: Some("unknown".to_string()),
        used: None,
        limit: None,
        remaining: None,
        subscription_ends_at: None,
        usage_based_billing: false,
        overdue: false,
        account_too_young: false,
        error: Some("llm_token_refresh_failed"),
    };

    // No fresh cached token and the exchange failed: the credential is dead
    // (spec 70 §3.7). Everything below is a billing problem only.
    if status_token(store, upstream, account, now).await.is_err() {
        return report;
    }
    report.token_ok = true;
    report.check_ok = true;
    report.billing_ok = true;
    report.usable = true;
    report.error = None;

    let profile = match fetch_billing_profile(upstream, account).await {
        Ok(profile) => profile,
        Err(BillingFailure::Unreachable) => {
            // The account still schedules; only the report is incomplete
            // (spec 50 §8.2.1 item 6).
            report.check_ok = false;
            report.billing_ok = false;
            report.quota_state = "unknown";
            report.error = Some("billing_check_failed");
            return report;
        }
        Err(BillingFailure::Unreadable) => {
            report.check_ok = false;
            report.billing_ok = false;
            report.quota_state = "unknown";
            report.error = Some("invalid_billing_response");
            return report;
        }
    };
    apply_billing(&mut report, parse_billing(&profile));
    report
}

// ---------------------------------------------------------------------------
// Token cache (spec 50 §1.2, spec 70 §3.7)
// ---------------------------------------------------------------------------

type TokenKey = (PathBuf, String);

fn token_pool() -> &'static Mutex<HashMap<TokenKey, TokenCache>> {
    static POOL: OnceLock<Mutex<HashMap<TokenKey, TokenCache>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pool_lock() -> std::sync::MutexGuard<'static, HashMap<TokenKey, TokenCache>> {
    token_pool()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Fresh cached token for the passive check, otherwise mint one (spec 70 §3.7).
///
/// shortcut: `proxy::account_token` takes `&Services`, which `status_body` does
/// not receive (passing it would change `http/router.rs`), so the status path
/// keeps its own pool. Polls still reuse the token; the proxy pool is separate.
async fn status_token(
    store: &AccountStore,
    upstream: &dyn UpstreamClient,
    account: &AccountSnapshot,
    now: i64,
) -> Result<String, GatewayError> {
    let key = (store.data_dir().to_path_buf(), account.name.clone());
    let mut cache = pool_lock().get(&key).cloned().unwrap_or_default();
    let token = cached_or_minted(
        upstream,
        &mut cache,
        &account.user_id,
        &account.credential,
        now,
    )
    .await?;
    pool_lock().insert(key, cache);
    Ok(token)
}

enum BillingFailure {
    /// Network error, timeout, non-2xx, oversize (spec 70 §3.7).
    Unreachable,
    /// Fetched, but not a JSON object (spec 70 §3.7, §3.7.1 item 8).
    Unreadable,
}

/// The billing half of `client/users/me` (spec 10 §2.5).
async fn fetch_billing_profile(
    upstream: &dyn UpstreamClient,
    account: &AccountSnapshot,
) -> Result<Value, BillingFailure> {
    let me = upstream
        .execute(me_request(&account.user_id, &account.credential))
        .await
        .map_err(|_| BillingFailure::Unreachable)?;
    // Only "not fetched" counts as an outage; a 2xx with a bad body is a
    // parse failure (spec 70 §3.7).
    if !(200..300).contains(&me.status) {
        return Err(BillingFailure::Unreachable);
    }
    let profile: Value =
        serde_json::from_slice(&me.body).map_err(|_| BillingFailure::Unreadable)?;
    if !profile.is_object() {
        return Err(BillingFailure::Unreadable);
    }
    Ok(profile)
}

// ---------------------------------------------------------------------------
// Billing summary (spec 70 §3.7)
// ---------------------------------------------------------------------------

/// Parse the plan block of a `client/users/me` response (spec 70 §3.7.1).
/// The real shape keeps everything under a top-level `plan` object; older Zed
/// versions put a `plan_v3`/`plan_v2`/… object at the root, so those are the
/// fallback. A legal JSON body without any plan object is "plan unknown", not
/// a parse failure (spec 70 §3.7.1 item 7).
pub fn parse_billing(profile: &Value) -> BillingSummary {
    let plan = ["plan", "plan_v3", "plan_v2", "plan_info", "planInfo"]
        .iter()
        .find_map(|key| profile.get(*key).filter(|value| value.is_object()));

    let mut summary = BillingSummary::default();
    let Some(plan) = plan else {
        summary.plan = Some("unknown".to_string());
        return summary;
    };

    summary.plan = Some(
        ["plan_v3", "plan_v2", "plan"]
            .iter()
            .find_map(|key| {
                plan.get(*key)
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
            })
            .unwrap_or("unknown")
            .to_string(),
    );
    if let Some(requests) = plan
        .get("usage")
        .and_then(|usage| usage.get("model_requests"))
        .filter(|value| value.is_object())
    {
        summary.used = requests.get("used").and_then(as_i64);
        // An integer limit or `{"limited": n}`; 0, a negative value or a
        // missing value means the plan does not publish a number (spec 70
        // §3.7.1 item 5).
        summary.limit = requests
            .get("limit")
            .and_then(limit_value)
            .filter(|limit| *limit > 0);
    }
    summary.subscription_ends_at = plan
        .get("subscription_period")
        .and_then(|period| period.get("ended_at"))
        .or_else(|| plan.get("subscription_ends_at"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    summary.usage_based_billing = flag(
        plan,
        profile,
        &[
            "is_usage_based_billing_enabled",
            "usage_based_billing",
            "usage_based",
        ],
    );
    summary.overdue = flag(
        plan,
        profile,
        &[
            "has_overdue_invoices",
            "overdue",
            "payment_overdue",
            "billing_overdue",
        ],
    );
    summary.account_too_young = flag(
        plan,
        profile,
        &["is_account_too_young", "account_too_young", "too_young"],
    );
    summary.restricted = flag(plan, plan, &["restricted", "model_access_restricted"])
        || summary.plan.as_deref() == Some("restricted");
    summary
}

/// The first key that carries a boolean decides; the plan object is checked
/// before the root, real key names before legacy ones (spec 70 §3.7.1 item 3).
fn flag(plan: &Value, root: &Value, keys: &[&str]) -> bool {
    [Some(plan), Some(root)]
        .into_iter()
        .flatten()
        .find_map(|value| {
            keys.iter()
                .find_map(|key| value.get(*key).and_then(Value::as_bool))
        })
        .unwrap_or(false)
}

/// Integer fields accept JSON integers and decimal strings (Zed has shipped
/// both); a float truncates, a non-finite value is "missing".
fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64().or_else(|| {
            let float = number.as_f64()?;
            float.is_finite().then(|| float.trunc() as i64)
        }),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn limit_value(value: &Value) -> Option<i64> {
    match value {
        Value::Object(map) => map.get("limited").and_then(as_i64),
        other => as_i64(other),
    }
}

fn apply_billing(report: &mut AccountStatusReport, billing: BillingSummary) {
    report.plan = billing.plan;
    report.used = billing.used.map(Value::from);
    report.limit = billing.limit.map(Value::from);
    // `used` missing counts as 0 (spec 50 §8.2.1 item 5).
    report.remaining = billing
        .limit
        .map(|limit| Value::from((limit - billing.used.unwrap_or(0)).max(0)));
    report.subscription_ends_at = billing.subscription_ends_at;
    report.usage_based_billing = billing.usage_based_billing;
    report.overdue = billing.overdue;
    report.account_too_young = billing.account_too_young;

    // A too-young account counts as restricted (spec 50 §8.2.1 item 2).
    let restricted = billing.restricted || billing.account_too_young;
    match (restricted, billing.limit) {
        (true, _) => {
            report.quota_state = "restricted";
            report.usable = false;
        }
        (false, None) => {
            report.quota_state = "unmetered";
            report.usable = true;
        }
        (false, Some(limit)) => {
            if billing.used.unwrap_or(0) >= limit {
                report.quota_state = "exhausted";
                report.usable = false;
            } else {
                report.quota_state = "available";
                report.usable = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::client::{
        BoxFuture, Endpoint, UpstreamRequest, UpstreamResponse, UpstreamStream,
    };

    const NOW: i64 = 1_762_300_000;
    const TOKEN_EXP: i64 = 1_900_000_000;

    /// The real `client/users/me` shape (spec 70 §3.7.1), fake data only.
    fn profile(plan: Value) -> Value {
        serde_json::json!({
            "default_organization_id": "org-1",
            "organizations": [{"id": "org-1", "is_personal": true}],
            "plan": plan,
        })
    }

    fn summary(plan: Value) -> BillingSummary {
        parse_billing(&profile(plan))
    }

    #[test]
    fn real_shape_reads_every_billing_field() {
        let billing = summary(serde_json::json!({
            "plan_v2": "zed_student",
            "plan": "zed_free",
            "plan_v3": "zed_pro",
            "has_overdue_invoices": true,
            "is_account_too_young": false,
            "is_usage_based_billing_enabled": true,
            "subscription_period": {"started_at": "2026-07-20T00:00:00Z",
                                    "ended_at": "2026-08-20T00:00:00Z"},
            "usage": {"model_requests": {"used": 12, "limit": {"limited": 50}}},
        }));
        assert_eq!(billing.plan.as_deref(), Some("zed_pro"));
        assert_eq!(billing.used, Some(12));
        assert_eq!(billing.limit, Some(50));
        assert_eq!(
            billing.subscription_ends_at.as_deref(),
            Some("2026-08-20T00:00:00Z")
        );
        assert!(billing.overdue);
        assert!(billing.usage_based_billing);
        assert!(!billing.account_too_young);
        assert!(!billing.restricted);
    }

    #[test]
    fn real_flag_names_win_over_legacy_ones() {
        // `is_account_too_young:false` wins over a stale legacy `too_young`.
        let billing = summary(serde_json::json!({
            "is_account_too_young": false,
            "too_young": true,
        }));
        assert!(!billing.account_too_young);

        // An empty plan name keeps looking down the chain.
        let billing = summary(serde_json::json!({"plan_v3": "", "plan_v2": "zed_pro"}));
        assert_eq!(billing.plan.as_deref(), Some("zed_pro"));
    }

    #[test]
    fn integer_limit_is_read_and_remaining_is_derived() {
        let billing = summary(serde_json::json!({
            "plan_v3": "zed_pro",
            "usage": {"model_requests": {"used": 12, "limit": 50}},
        }));
        assert_eq!(billing.plan.as_deref(), Some("zed_pro"));
        assert_eq!(billing.used, Some(12));
        assert_eq!(billing.limit, Some(50));
    }

    #[test]
    fn object_limit_uses_the_limited_field() {
        let billing = summary(serde_json::json!({
            "usage": {"model_requests": {"used": 3, "limit": {"limited": 20}}},
        }));
        assert_eq!(billing.limit, Some(20));
    }

    #[test]
    fn zero_and_missing_limits_mean_unmetered() {
        for limit in [
            serde_json::json!(0),
            serde_json::json!(-5),
            serde_json::json!(null),
            serde_json::json!("0"),
        ] {
            let billing = summary(serde_json::json!({
                "usage": {"model_requests": {"used": 3, "limit": limit}},
            }));
            assert_eq!(billing.limit, None, "{limit}");
        }
        let billing = summary(serde_json::json!({"usage": {}}));
        assert_eq!(billing.used, None);
        assert_eq!(billing.limit, None);
    }

    #[test]
    fn old_shape_falls_back_when_there_is_no_top_level_plan_object() {
        let profile = serde_json::json!({
            "plan_v2": {
                "plan": "zed_free",
                "usage": {"model_requests": {"used": 1, "limit": 5}},
                "overdue": true,
                "usage_based": true,
                "account_too_young": false,
            }
        });
        let billing = parse_billing(&profile);
        assert_eq!(billing.plan.as_deref(), Some("zed_free"));
        assert_eq!(billing.limit, Some(5));
        assert!(billing.overdue);
        assert!(billing.usage_based_billing);
        assert!(!billing.account_too_young);

        // Root `plan_v3` wins over `plan_v2` in the old shape too.
        let profile = serde_json::json!({
            "plan_v2": {"plan": "zed_free"},
            "plan_v3": {"plan": "zed_pro"},
        });
        assert_eq!(parse_billing(&profile).plan.as_deref(), Some("zed_pro"));

        // A string `plan` is not the real shape: keep looking at the root.
        let profile = serde_json::json!({
            "plan": "pro",
            "plan_v3": {"plan": "zed_pro"},
        });
        assert_eq!(parse_billing(&profile).plan.as_deref(), Some("zed_pro"));
    }

    #[test]
    fn json_without_a_plan_object_is_plan_unknown_not_a_parse_failure() {
        for profile in [
            serde_json::json!({}),
            serde_json::json!({"plan": "pro"}),
            serde_json::json!({"plan_v3": []}),
        ] {
            let billing = parse_billing(&profile);
            assert_eq!(billing.plan.as_deref(), Some("unknown"), "{profile}");
            assert_eq!(billing.used, None);
            assert_eq!(billing.limit, None);
            assert!(!billing.overdue && !billing.account_too_young);
            assert!(!billing.restricted);
        }
    }

    fn blank_report() -> AccountStatusReport {
        AccountStatusReport {
            name: "a".into(),
            user_id: "1".into(),
            current: false,
            scheduler_healthy: true,
            cooldown_s: 0,
            last_status: 0,
            model_state: AccountModelState::Unchecked,
            model_ok: false,
            model_checked_at: 0,
            model_latency_ms: 0,
            orb_configured: false,
            credit: None,
            check_ok: true,
            token_ok: true,
            billing_ok: true,
            usable: true,
            quota_state: "available",
            plan: None,
            used: None,
            limit: None,
            remaining: None,
            subscription_ends_at: None,
            usage_based_billing: false,
            overdue: false,
            account_too_young: false,
            error: None,
        }
    }

    #[test]
    fn account_too_young_is_restricted_and_unusable() {
        let mut report = blank_report();
        apply_billing(
            &mut report,
            summary(serde_json::json!({
                "plan_v3": "zed_pro",
                "has_overdue_invoices": true,
                "is_account_too_young": true,
                "is_usage_based_billing_enabled": true,
                "usage": {"model_requests": {"used": 12, "limit": {"limited": 50}}},
            })),
        );
        assert_eq!(report.quota_state, "restricted");
        assert!(!report.usable);
        assert!(report.account_too_young);
        assert!(report.overdue);
        assert!(report.usage_based_billing);
        assert_eq!(report.plan.as_deref(), Some("zed_pro"));
        assert_eq!(report.used, Some(Value::from(12)));
        assert_eq!(report.limit, Some(Value::from(50)));
        assert_eq!(report.remaining, Some(Value::from(38)));
    }

    #[test]
    fn quota_state_covers_available_exhausted_restricted_and_unmetered() {
        let mut report = blank_report();
        let cases = [
            (
                serde_json::json!({"usage": {"model_requests": {"used": 1, "limit": 10}}}),
                "available",
                true,
            ),
            (
                serde_json::json!({"usage": {"model_requests": {"used": 10, "limit": 10}}}),
                "exhausted",
                false,
            ),
            (
                serde_json::json!({"usage": {"model_requests": {"used": 11, "limit": 10}}}),
                "exhausted",
                false,
            ),
            (
                serde_json::json!({"usage": {"model_requests": {"used": 1}}}),
                "unmetered",
                true,
            ),
            (
                serde_json::json!({"usage": {"model_requests": {"limit": 10}}}),
                "available",
                true,
            ),
            (
                serde_json::json!({"usage": {"model_requests": {"used": 1, "limit": 0}}}),
                "unmetered",
                true,
            ),
            (
                serde_json::json!({"restricted": true, "usage": {"model_requests": {"used": 1, "limit": 10}}}),
                "restricted",
                false,
            ),
        ];
        for (plan, state, usable) in cases {
            apply_billing(&mut report, summary(plan));
            assert_eq!(report.quota_state, state);
            assert_eq!(report.usable, usable, "{state}");
        }
        // Remaining never goes negative.
        apply_billing(
            &mut report,
            summary(serde_json::json!({
                "usage": {"model_requests": {"used": 99, "limit": 10}}
            })),
        );
        assert_eq!(report.remaining, Some(Value::from(0)));
        // A missing `used` counts as 0 for `remaining` (spec 50 §8.2.1 item 5).
        apply_billing(
            &mut report,
            summary(serde_json::json!({
                "usage": {"model_requests": {"limit": 10}}
            })),
        );
        assert_eq!(report.remaining, Some(Value::from(10)));
        assert_eq!(report.used, None);
    }

    fn jwt(exp: i64) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#).as_bytes());
        format!("{header}.{payload}.sig")
    }

    /// Fake upstream; records which endpoints were called.
    struct FakeUpstream {
        users_me_status: u16,
        users_me_body: Vec<u8>,
        token_status: u16,
        calls: std::sync::Mutex<Vec<Endpoint>>,
    }

    impl FakeUpstream {
        fn new(users_me_status: u16, users_me: Value, token_status: u16) -> Self {
            Self::raw(
                users_me_status,
                &serde_json::to_vec(&users_me).unwrap(),
                token_status,
            )
        }

        fn raw(users_me_status: u16, users_me_body: &[u8], token_status: u16) -> Self {
            Self {
                users_me_status,
                users_me_body: users_me_body.to_vec(),
                token_status,
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Endpoint> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl UpstreamClient for FakeUpstream {
        fn execute<'a>(
            &'a self,
            request: UpstreamRequest,
        ) -> BoxFuture<'a, Result<UpstreamResponse, GatewayError>> {
            self.calls.lock().unwrap().push(request.endpoint);
            let status = match request.endpoint {
                Endpoint::UserMe => self.users_me_status,
                Endpoint::LlmTokens => self.token_status,
                _ => 404,
            };
            let body = match request.endpoint {
                Endpoint::UserMe => self.users_me_body.clone(),
                Endpoint::LlmTokens if self.token_status == 200 => {
                    format!(r#"{{"token":"{}"}}"#, jwt(TOKEN_EXP)).into_bytes()
                }
                _ => Vec::new(),
            };
            Box::pin(async move { Ok(UpstreamResponse { status, body }) })
        }

        fn execute_stream<'a>(
            &'a self,
            _request: UpstreamRequest,
        ) -> BoxFuture<'a, Result<UpstreamStream, GatewayError>> {
            Box::pin(async move { Err(GatewayError::internal("not used")) })
        }
    }

    fn store_with_account(dir: &std::path::Path) -> AccountStore {
        let store = AccountStore::load(dir, 0);
        store
            .add_account("alice", "42", serde_json::json!({"access_token": "x"}))
            .unwrap();
        store
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zprs-status-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn accounts_list_body_has_the_fixed_shape() {
        let dir = temp_dir("list");
        let store = store_with_account(&dir);
        let body = String::from_utf8(accounts_body(&store, 100)).unwrap();
        assert_eq!(
            body,
            r#"{"accounts":[{"name":"alice","user_id":"42","current":true,"healthy":true,"cooldown_s":0,"consecutive_failures":0,"last_status":0}],"current":"alice"}"#
        );
        // Cooldown state is reflected in the same shape (spec 50 §8.1).
        store.record_failure("alice", crate::error::ErrorKind::RateLimit, 429, 100);
        let body = String::from_utf8(accounts_body(&store, 110)).unwrap();
        assert!(
            body.contains(
                r#""healthy":false,"cooldown_s":20,"consecutive_failures":1,"last_status":429"#
            ),
            "{body}"
        );
        let empty_dir = temp_dir("empty");
        let empty = AccountStore::load(&empty_dir, 0);
        assert_eq!(
            String::from_utf8(accounts_body(&empty, 0)).unwrap(),
            r#"{"accounts":[],"current":""}"#
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty_dir);
    }

    #[tokio::test]
    async fn status_body_fills_billing_fields_and_flags_a_failed_token() {
        let dir = temp_dir("status");
        let store = store_with_account(&dir);

        let ok = FakeUpstream::new(
            200,
            profile(serde_json::json!({
                "plan_v3": "zed_pro",
                "has_overdue_invoices": true,
                "is_account_too_young": false,
                "is_usage_based_billing_enabled": false,
                "usage": {"model_requests": {"used": 12, "limit": {"limited": 50}}},
                "subscription_period": {"ended_at": "2026-08-20T00:00:00Z"},
            })),
            200,
        );
        let body = String::from_utf8(status_body(&store, &ok, NOW).await).unwrap();
        assert!(body.starts_with(r#"{"checked_at":1762300000,"accounts":[{"name":"alice","user_id":"42","current":true,"scheduler_healthy":true,"cooldown_s":0,"last_status":0,"model_state":"unchecked","model_ok":false,"model_checked_at":0,"model_latency_ms":0,"orb_configured":false,"#), "{body}");
        assert!(body.contains(r#""check_ok":true,"token_ok":true,"billing_ok":true,"usable":true,"quota_state":"available","plan":"zed_pro","used":12,"limit":50,"remaining":38,"subscription_ends_at":"2026-08-20T00:00:00Z","usage_based_billing":false,"overdue":true,"account_too_young":false}"#), "{body}");
        assert!(serde_json::from_str::<Value>(&body).is_ok());
        let _ = std::fs::remove_dir_all(&dir);

        // A second poll reuses the fresh token: one users/me request only
        // (spec 70 §3.7).
        let body = String::from_utf8(status_body(&store, &ok, NOW + 1).await).unwrap();
        assert!(body.contains(r#""token_ok":true"#), "{body}");
        assert_eq!(
            ok.calls().len(),
            4,
            "users/me + llm_tokens + users/me, then users/me only"
        );
    }

    /// A too-young account in the real shape must come back restricted and
    /// unusable (spec 50 §8.2.1 item 2).
    #[tokio::test]
    async fn status_body_marks_a_too_young_account_restricted() {
        let dir = temp_dir("too-young");
        let store = store_with_account(&dir);
        let young = FakeUpstream::new(
            200,
            profile(serde_json::json!({
                "plan_v3": "zed_pro",
                "has_overdue_invoices": false,
                "is_account_too_young": true,
                "is_usage_based_billing_enabled": true,
                "usage": {"model_requests": {"used": 12, "limit": {"limited": 50}}},
            })),
            200,
        );
        let body = String::from_utf8(status_body(&store, &young, NOW).await).unwrap();
        assert!(body.contains(r#""check_ok":true,"token_ok":true,"billing_ok":true,"usable":false,"quota_state":"restricted","plan":"zed_pro","used":12,"limit":50,"remaining":38,"subscription_ends_at":null,"usage_based_billing":true,"overdue":false,"account_too_young":true}"#), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A billing outage must not mark a working account as unusable
    /// (spec 70 §3.7, spec 50 §8.2.1 item 6).
    #[tokio::test]
    async fn billing_failure_keeps_the_account_usable() {
        let dir = temp_dir("billing-down");
        let store = store_with_account(&dir);
        // Warm the token cache first: an exchange needs users/me for the
        // organization id, so a cold cache would legitimately fail.
        let warm = FakeUpstream::new(200, profile(serde_json::json!({})), 200);
        let _ = status_body(&store, &warm, NOW).await;
        let down = FakeUpstream::new(503, serde_json::json!({}), 200);
        let body = String::from_utf8(status_body(&store, &down, NOW).await).unwrap();
        assert!(body.contains(r#""check_ok":false,"token_ok":true,"billing_ok":false,"usable":true,"quota_state":"unknown","plan":"unknown","used":null,"limit":null,"remaining":null,"subscription_ends_at":null,"usage_based_billing":false,"overdue":false,"account_too_young":false,"error":"billing_check_failed"}"#), "{body}");

        // A 200 whose body is not JSON is a parse failure, not an outage.
        let garbage = FakeUpstream::raw(200, b"<html>nope</html>", 200);
        let body = String::from_utf8(status_body(&store, &garbage, NOW).await).unwrap();
        assert!(body.contains(r#""token_ok":true,"billing_ok":false,"usable":true,"quota_state":"unknown","plan":"unknown""#), "{body}");
        assert!(
            body.contains(r#""error":"invalid_billing_response""#),
            "{body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dead credential ends the entry at the token fields (spec 70 §3.7).
    #[tokio::test]
    async fn dead_credential_reports_llm_token_refresh_failed() {
        let dir = temp_dir("dead");
        let store = store_with_account(&dir);
        let dead = FakeUpstream::new(200, profile(serde_json::json!({})), 401);
        let body = String::from_utf8(status_body(&store, &dead, 5).await).unwrap();
        assert!(body.contains(r#""check_ok":false,"token_ok":false,"billing_ok":false,"usable":false,"quota_state":"unavailable","error":"llm_token_refresh_failed"}"#), "{body}");
        assert!(!body.contains("\"plan\""), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn orb_configured_tracks_the_stored_link() {
        let dir = temp_dir("orb-flag");
        let store = store_with_account(&dir);
        assert!(!store.snapshot("alice").unwrap().orb_url.is_some());
        store
            .set_orb_url("alice", Some("https://portal.withorb.com/?token=t"))
            .unwrap();
        assert!(store.snapshot("alice").unwrap().orb_url.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
