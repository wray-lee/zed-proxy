//! Outbound HTTP client for Zed's four upstream endpoints
//! (spec `10-zed-upstream.md` §1–§6, §8–§10).
//!
//! The provider layer only ever sees [`UpstreamClient`]: it builds a request
//! envelope and asks for a stream of upstream lines. Timeouts, TLS, the proxy
//! and the response size caps all live here so no provider can diverge.

use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::config::UpstreamBase;
use crate::error::{ErrorKind, GatewayError};

/// `Zed/<version> (<os>; <arch>)` — the version is a compile-time constant
/// because Zed rejects odd client versions with `trial_blocked` (spec 10 §2.1).
pub const ZED_VERSION: &str = "1.8.2";
pub const USER_AGENT: &str = "Zed/1.8.2 (linux; x86_64)";

/// Capability headers that make Zed report failures as HTTP 200
/// `{"status":{"failed":…}}` and end streams with `{"status":"stream_ended"}`
/// (spec 10 §2.2).
pub const X_ZED_VERSION: &str = "x-zed-version";
pub const X_ZED_CLIENT_SUPPORTS_STATUS_MESSAGES: &str = "x-zed-client-supports-status-messages";
pub const X_ZED_CLIENT_SUPPORTS_STREAM_ENDED: &str =
    "x-zed-client-supports-stream-ended-request-completion-status";
pub const X_ZED_SYSTEM_ID: &str = "x-zed-system-id";

pub const COMPLETIONS_TIMEOUT: Duration = Duration::from_secs(120);
/// Stream timeout, named by spec 16 §3 (`proxy::stream::STREAM_TIMEOUT_SECS`).
pub const COMPLETIONS_STREAM_TIMEOUT: Duration =
    Duration::from_secs(crate::proxy::stream::STREAM_TIMEOUT_SECS);
pub const LLM_TOKEN_TIMEOUT: Duration = Duration::from_secs(15);
pub const MODELS_TIMEOUT: Duration = Duration::from_secs(15);
pub const USER_ME_TIMEOUT: Duration = Duration::from_secs(15);

/// Non-streaming `completions` buffer cap (spec 10 §8).
pub const MAX_COMPLETIONS_BUFFER: usize = 4 * 1024 * 1024;
/// `llm_tokens` / `models` / `users/me` buffer cap (spec 10 §8).
pub const MAX_META_BUFFER: usize = 1024 * 1024;

/// Boxed async byte stream with a uniform error type.
pub type ByteStream = std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, GatewayError>> + Send>>;

/// A stream of upstream bytes plus the HTTP status that opened it.
pub struct UpstreamStream {
    pub status: u16,
    pub body: ByteStream,
}

impl std::fmt::Debug for UpstreamStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamStream")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// A buffered upstream response.
#[derive(Debug, Clone)]
pub struct UpstreamResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl UpstreamResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Which endpoint a request targets; selects URL, headers and timeout
/// (spec 10 §1, §2, §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    Completions,
    LlmTokens,
    Models,
    UserMe,
}

impl Endpoint {
    pub fn path(self) -> &'static str {
        match self {
            Endpoint::Completions => "/completions",
            Endpoint::LlmTokens => "/client/llm_tokens",
            Endpoint::Models => "/models",
            Endpoint::UserMe => "/client/users/me",
        }
    }

    pub fn timeout(self, stream: bool) -> Duration {
        match self {
            Endpoint::Completions => {
                if stream {
                    COMPLETIONS_STREAM_TIMEOUT
                } else {
                    COMPLETIONS_TIMEOUT
                }
            }
            Endpoint::LlmTokens => LLM_TOKEN_TIMEOUT,
            Endpoint::Models => MODELS_TIMEOUT,
            Endpoint::UserMe => USER_ME_TIMEOUT,
        }
    }

    pub fn buffer_limit(self) -> usize {
        match self {
            Endpoint::Completions => MAX_COMPLETIONS_BUFFER,
            _ => MAX_META_BUFFER,
        }
    }
}

/// One outbound request. Headers are lowercase on the wire (spec 10 §2).
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    pub endpoint: Endpoint,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Use the long streaming timeout and stream the response body.
    pub stream: bool,
}

impl UpstreamRequest {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            headers: vec![("user-agent".to_string(), USER_AGENT.to_string())],
            body: Vec::new(),
            stream: false,
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_ascii_lowercase(), value.into()));
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("authorization", format!("Bearer {token}"))
    }

    pub fn json_body(mut self, body: Vec<u8>) -> Self {
        self.headers
            .push(("content-type".to_string(), "application/json".to_string()));
        self.body = body;
        self
    }

    /// `x-zed-version` only (`/models`, spec 10 §2.4).
    pub fn zed_version(self) -> Self {
        self.header(X_ZED_VERSION, ZED_VERSION)
    }

    /// `x-zed-version` plus the two capability headers (spec 10 §2.2).
    pub fn zed_client(self) -> Self {
        self.header(X_ZED_VERSION, ZED_VERSION)
            .header(X_ZED_CLIENT_SUPPORTS_STATUS_MESSAGES, "true")
            .header(X_ZED_CLIENT_SUPPORTS_STREAM_ENDED, "true")
    }

    /// `x-zed-system-id` only when configured and non-empty (spec 10 §2.3).
    pub fn system_id(self, system_id: Option<&str>) -> Self {
        match system_id {
            Some(value) if !value.is_empty() => self.header(X_ZED_SYSTEM_ID, value),
            _ => self,
        }
    }

    /// `<user_id> <credential JSON>` for the two credential endpoints
    /// (spec 10 §2.3, §2.5).
    pub fn credentials(self, user_id: &str, credential: &serde_json::Value) -> Self {
        let compact = serde_json::to_string(credential).unwrap_or_else(|_| "null".to_string());
        self.header("authorization", format!("{user_id} {compact}"))
    }
}

/// Boxed future used by [`UpstreamClient`] so the trait stays dyn-compatible
/// without pulling in the `async-trait` proc macro.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Everything the rest of the crate may ask of the network. Implemented by
/// [`HttpUpstreamClient`] in production and by fakes in tests.
pub trait UpstreamClient: Send + Sync {
    /// Buffered request with the endpoint's size cap.
    fn execute<'a>(
        &'a self,
        request: UpstreamRequest,
    ) -> BoxFuture<'a, Result<UpstreamResponse, GatewayError>>;

    /// Streaming request; the caller owns line splitting (spec 16 §2).
    fn execute_stream<'a>(
        &'a self,
        request: UpstreamRequest,
    ) -> BoxFuture<'a, Result<UpstreamStream, GatewayError>>;
}

/// Concrete client: one `reqwest::Client`, built once (spec 10 §9, §10).
#[derive(Debug, Clone)]
pub struct HttpUpstreamClient {
    client: reqwest::Client,
    base: UpstreamBase,
}

impl HttpUpstreamClient {
    pub fn new(
        base: UpstreamBase,
        proxy: Option<&crate::config::ProxyConfig>,
    ) -> Result<Self, GatewayError> {
        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .https_only(false)
            // Idle-read cap so a stalled upstream cannot hang a stream forever
            // (spec 10 §6); the per-request timeout still bounds the total.
            .read_timeout(COMPLETIONS_STREAM_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(30));
        if let Some(proxy) = proxy {
            let url = format!("{}://{}:{}", proxy.scheme, proxy.host, proxy.port);
            let proxy = reqwest::Proxy::all(&url)
                .map_err(|err| GatewayError::internal(format!("invalid proxy: {err}")))?;
            builder = builder.proxy(proxy);
        }
        let client = builder
            .build()
            .map_err(|err| GatewayError::internal(format!("cannot build HTTP client: {err}")))?;
        Ok(Self { client, base })
    }

    fn prepare(
        &self,
        request: UpstreamRequest,
    ) -> (String, reqwest::RequestBuilder, Duration, usize) {
        let url = self.base.join(request.endpoint.path());
        let timeout = request.endpoint.timeout(request.stream);
        let limit = request.endpoint.buffer_limit();
        let method = match request.endpoint {
            // The two GET endpoints carry no body (spec 10 §2.4, §2.5).
            Endpoint::Models | Endpoint::UserMe => reqwest::Method::GET,
            Endpoint::Completions | Endpoint::LlmTokens => reqwest::Method::POST,
        };
        let is_post = method == reqwest::Method::POST;
        let mut builder = self.client.request(method, &url).timeout(timeout);
        if is_post {
            builder = builder.body(request.body.clone());
        }
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        (url, builder, timeout, limit)
    }
}

impl UpstreamClient for HttpUpstreamClient {
    fn execute<'a>(
        &'a self,
        request: UpstreamRequest,
    ) -> BoxFuture<'a, Result<UpstreamResponse, GatewayError>> {
        Box::pin(async move {
            let (url, builder, _timeout, limit) = self.prepare(request);
            let response = builder
                .send()
                .await
                .map_err(|err| transport_error(&url, &err))?;
            let status = response.status().as_u16();
            let bytes = read_capped(response, limit).await?;
            Ok(UpstreamResponse {
                status,
                body: bytes,
            })
        })
    }

    fn execute_stream<'a>(
        &'a self,
        request: UpstreamRequest,
    ) -> BoxFuture<'a, Result<UpstreamStream, GatewayError>> {
        Box::pin(async move {
            let (url, builder, _timeout, _limit) = self.prepare(request);
            let response = builder
                .send()
                .await
                .map_err(|err| transport_error(&url, &err))?;
            let status = response.status().as_u16();
            let stream = response.bytes_stream().map(|item| {
                item.map_err(|err| {
                    GatewayError::transient(None, format!("upstream stream error: {err}"))
                })
            });
            Ok(UpstreamStream {
                status,
                body: Box::pin(stream),
            })
        })
    }
}

async fn read_capped(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, GatewayError> {
    if let Some(length) = response.content_length() {
        if length as usize > limit {
            return Err(GatewayError::transient(
                None,
                format!("upstream response exceeds {limit} bytes"),
            ));
        }
    }
    let mut collected = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|err| GatewayError::transient(None, format!("upstream read error: {err}")))?;
        if collected.len() + chunk.len() > limit {
            return Err(GatewayError::transient(
                None,
                format!("upstream response exceeds {limit} bytes"),
            ));
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(collected)
}

fn transport_error(url: &str, err: &reqwest::Error) -> GatewayError {
    if err.is_timeout() {
        // Timeouts fold into 502, never 504 (spec 30 §7 E9).
        return GatewayError::transient(None, "upstream timeout");
    }
    // The URL contains no credentials (they live in headers).
    GatewayError::transient(None, format!("upstream request to {url} failed: {err}"))
}

// ---------------------------------------------------------------------------
// Status classification, retry decisions and the HTTP-200 failure wrapper
// (spec 10 §4.5–§4.6, §7; spec 30 §3).
// ---------------------------------------------------------------------------

/// Non-streaming attempts per account (spec 10 §7).
pub const MAX_ATTEMPTS: u32 = 3;
/// Backoff after attempt 1 and 2 for 5xx/network failures (spec 10 §7).
pub const TRANSIENT_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];
/// Backoff between 429 attempts (spec 10 §7).
pub const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(3);

/// `{"status":{"failed":{…}}}` — the HTTP-200 failure wrapper (spec 10 §4.6).
/// `message` is preserved verbatim; `retry_after` non-null means rate limiting.
pub fn parse_status_failed(value: &serde_json::Value) -> Option<GatewayError> {
    let failed = value.get("status")?.get("failed")?;
    let message = failed
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("upstream rejected request")
        .to_string();
    let retry_after = failed
        .get("retry_after")
        .map(|value| !value.is_null())
        .unwrap_or(false);
    let status = failed
        .get("code")
        .and_then(serde_json::Value::as_str)
        .and_then(|code| code.strip_prefix("upstream_http_"))
        .and_then(|suffix| suffix.parse::<u16>().ok());
    let plan_hint = mentions_plan(&message);
    let mut kind = match status {
        Some(status) => crate::error::classify_status(status, plan_hint),
        None if retry_after => ErrorKind::RateLimit,
        None => ErrorKind::Transient,
    };
    // Rate limiting wins over the code, except for request-level rejections
    // (spec 30 §3.2 判定顺序).
    if retry_after && kind != ErrorKind::Request {
        kind = ErrorKind::RateLimit;
    }
    let mut err = GatewayError::new(kind, message);
    err.status = status;
    Some(err)
}

/// Scan a buffered non-streaming body for the HTTP-200 failure line
/// (spec 10 §4.6): only lines containing `"failed"` are parsed.
pub fn find_buffered_failure(body: &[u8]) -> Option<GatewayError> {
    for line in String::from_utf8_lossy(body).lines() {
        if !line.contains("\"failed\"") {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            if let Some(err) = parse_status_failed(&value) {
                return Some(err);
            }
        }
    }
    None
}

/// `message` from an upstream error body, kept verbatim (spec 30 §4.3). Falls
/// back to the body itself (whitespace-collapsed) when it is not JSON.
pub fn upstream_message(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        for path in [
            &["error", "message"][..],
            &["message"][..],
            &["error"][..],
            &["detail"][..],
        ] {
            if let Some(message) = dig(&value, path).and_then(serde_json::Value::as_str) {
                return message.to_string();
            }
        }
    }
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        "upstream error".to_string()
    } else {
        collapsed
    }
}

fn dig<'a>(value: &'a serde_json::Value, path: &[&str]) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

pub fn mentions_plan(text: &str) -> bool {
    text.to_ascii_lowercase().contains("plan")
}

/// One upstream attempt's classification (spec 30 §3.1, §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptFailure {
    pub error: GatewayError,
    /// Retry the same account before moving on (spec 10 §7).
    pub retry_same_account: bool,
    /// How long to wait before that retry.
    pub backoff: Duration,
}

impl AttemptFailure {
    pub fn new(error: GatewayError, retry_same_account: bool, backoff: Duration) -> Self {
        Self {
            error,
            retry_same_account,
            backoff,
        }
    }

    /// Whether the next attempt may still use this account at `attempt`
    /// (1-based; `MAX_ATTEMPTS` total).
    pub fn tries_again(&self, attempt: u32) -> bool {
        self.retry_same_account && attempt < MAX_ATTEMPTS
    }
}

/// Classify one completed upstream exchange (spec 10 §7, spec 30 §3). `stream`
/// requests never retry on non-2xx; only the buffered path does. `None` means
/// the exchange succeeded.
pub fn classify_attempt(status: u16, body: &[u8], stream: bool) -> Option<AttemptFailure> {
    if (200..300).contains(&status) {
        // A 200 can still hide a failure wrapper (spec 10 §4.6).
        if let Some(err) = find_buffered_failure(body) {
            return Some(AttemptFailure::new(err, false, Duration::ZERO));
        }
        // Buffered 200 with an empty or HTML body is a transient failure, not
        // an empty success (spec 30 §3.3).
        if !stream && body.is_empty() {
            return Some(AttemptFailure::new(
                GatewayError::transient(Some(status), "upstream response body is empty"),
                true,
                TRANSIENT_BACKOFF[0],
            ));
        }
        if !stream && looks_like_html(body) {
            return Some(AttemptFailure::new(
                GatewayError::transient(Some(status), "upstream returned an HTML page"),
                true,
                TRANSIENT_BACKOFF[0],
            ));
        }
        return None;
    }
    let message = upstream_message(body);
    let kind = crate::error::classify_status(status, mentions_plan(&message));
    let error = match kind {
        ErrorKind::Request => GatewayError::request(message),
        ErrorKind::Auth => GatewayError::auth(status, message),
        ErrorKind::Plan => GatewayError::plan(status, message),
        ErrorKind::RateLimit => GatewayError::rate_limit(message),
        _ => GatewayError::transient(Some(status), message),
    };
    let (retry_same_account, backoff) = match kind {
        ErrorKind::RateLimit if !stream => (true, RATE_LIMIT_BACKOFF),
        ErrorKind::Transient if !stream && exit_status_retries(status) => {
            (true, TRANSIENT_BACKOFF[0])
        }
        _ => (false, Duration::ZERO),
    };
    Some(AttemptFailure::new(error, retry_same_account, backoff))
}

/// A 200 whose body starts with a markup tag is an HTML error page from an
/// intermediary, not a completion (spec 30 §3.3).
fn looks_like_html(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
    let lowered = trimmed.get(..16).unwrap_or(trimmed).to_ascii_lowercase();
    lowered.starts_with("<html") || lowered.starts_with("<!doctype")
}

/// 3xx and 409 fail over without a same-account retry (spec 30 §3.1).
fn exit_status_retries(status: u16) -> bool {
    !(300..400).contains(&status) && status != 409
}

/// Backoff before attempt `next` (2-based) of the same account: 1s then 2s for
/// transient failures, 3s flat for rate limits (spec 10 §7).
pub fn backoff_before(kind: ErrorKind, next_attempt: u32) -> Duration {
    match kind {
        ErrorKind::RateLimit => RATE_LIMIT_BACKOFF,
        ErrorKind::Transient => TRANSIENT_BACKOFF
            .get((next_attempt as usize).saturating_sub(2))
            .copied()
            .unwrap_or(*TRANSIENT_BACKOFF.last().unwrap()),
        _ => Duration::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProxyConfig;

    #[test]
    fn user_agent_matches_the_required_shape() {
        assert_eq!(USER_AGENT, "Zed/1.8.2 (linux; x86_64)");
    }

    #[test]
    fn endpoint_paths_and_timeouts() {
        assert_eq!(Endpoint::Completions.path(), "/completions");
        assert_eq!(Endpoint::LlmTokens.path(), "/client/llm_tokens");
        assert_eq!(Endpoint::Models.path(), "/models");
        assert_eq!(Endpoint::UserMe.path(), "/client/users/me");
        assert_eq!(Endpoint::Completions.timeout(false), COMPLETIONS_TIMEOUT);
        assert_eq!(
            Endpoint::Completions.timeout(true),
            COMPLETIONS_STREAM_TIMEOUT
        );
        assert_eq!(Endpoint::Models.timeout(false), Duration::from_secs(15));
    }

    #[test]
    fn client_builds_with_and_without_a_proxy() {
        let base = UpstreamBase::parse("http://127.0.0.1:9").unwrap();
        assert!(HttpUpstreamClient::new(base.clone(), None).is_ok());
        let proxy = ProxyConfig {
            scheme: "http".into(),
            host: "127.0.0.1".into(),
            port: 7890,
        };
        assert!(HttpUpstreamClient::new(base, Some(&proxy)).is_ok());
    }

    #[test]
    fn request_builder_sets_lowercase_headers() {
        let request = UpstreamRequest::new(Endpoint::Models)
            .bearer("tok")
            .json_body(b"{}".to_vec());
        assert_eq!(request.headers[0].0, "user-agent");
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer tok"));
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value == "application/json"));
    }

    #[test]
    fn zed_client_and_system_id_headers_follow_the_spec() {
        let request = UpstreamRequest::new(Endpoint::LlmTokens)
            .zed_client()
            .system_id(None)
            .system_id(Some(""));
        let names: Vec<&str> = request
            .headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert!(names.contains(&"x-zed-version"));
        assert!(names.contains(&X_ZED_CLIENT_SUPPORTS_STATUS_MESSAGES));
        assert!(names.contains(&X_ZED_CLIENT_SUPPORTS_STREAM_ENDED));
        assert!(!names.contains(&X_ZED_SYSTEM_ID));

        let request = UpstreamRequest::new(Endpoint::LlmTokens).system_id(Some("sys-1"));
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == X_ZED_SYSTEM_ID && value == "sys-1"));
    }

    #[test]
    fn credential_authorization_is_user_then_compact_json() {
        let credential = serde_json::json!({"id": 7});
        let request = UpstreamRequest::new(Endpoint::UserMe).credentials("42", &credential);
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == r#"42 {"id":7}"#));
    }

    #[test]
    fn status_failed_requires_the_nested_shape() {
        let failed: serde_json::Value = serde_json::from_str(
            r#"{"status":{"failed":{"code":"upstream_http_429","message":"slow down","retry_after":3}}}"#,
        )
        .unwrap();
        let err = parse_status_failed(&failed).unwrap();
        assert_eq!(err.kind, ErrorKind::RateLimit);
        assert_eq!(err.message, "slow down");
        // `retry_after` wins even when the code says 400.
        let failed: serde_json::Value = serde_json::from_str(
            r#"{"status":{"failed":{"code":"upstream_http_400","message":"x","retry_after":1}}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_status_failed(&failed).unwrap().kind,
            ErrorKind::Request
        );
        for text in [
            r#"{"status":"stream_ended"}"#,
            r#"{"status":"failed"}"#,
            r#"{"delta":"failed"}"#,
        ] {
            assert!(parse_status_failed(&serde_json::from_str(text).unwrap()).is_none());
        }
    }

    #[test]
    fn buffered_failure_scan_finds_embedded_lines() {
        let body = b"{\"type\":\"response.created\"}\n{\"status\":{\"failed\":{\"code\":\"upstream_http_422\",\"message\":\"bad\",\"retry_after\":null}}}\n";
        let err = find_buffered_failure(body).unwrap();
        assert_eq!(err.kind, ErrorKind::Request);
        assert_eq!(err.message, "bad");
        assert!(find_buffered_failure(b"{\"type\":\"response.created\"}\n").is_none());
    }

    #[test]
    fn request_level_statuses_never_retry() {
        let failure =
            classify_attempt(400, b"{\"error\":{\"message\":\"bad thing\"}}", false).unwrap();
        assert_eq!(failure.error.kind, ErrorKind::Request);
        assert_eq!(failure.error.message, "bad thing");
        assert!(!failure.retry_same_account);
        assert!(!failure.tries_again(1));
        for status in [404, 405, 413, 415, 422] {
            let failure = classify_attempt(status, b"{}", false).unwrap();
            assert_eq!(failure.error.kind, ErrorKind::Request, "{status}");
            assert!(!failure.retry_same_account, "{status}");
        }
    }

    #[test]
    fn credentials_plan_and_rate_limit_classification() {
        let failure = classify_attempt(401, b"", false).unwrap();
        assert_eq!(failure.error.kind, ErrorKind::Auth);
        assert!(!failure.retry_same_account);
        let failure = classify_attempt(403, b"{\"message\":\"model not in PLAN\"}", false).unwrap();
        assert_eq!(failure.error.kind, ErrorKind::Plan);
        assert_eq!(failure.error.message, "model not in PLAN");
        assert!(!failure.retry_same_account);
        let failure = classify_attempt(429, b"{}", false).unwrap();
        assert_eq!(failure.error.kind, ErrorKind::RateLimit);
        assert_eq!(failure.backoff, RATE_LIMIT_BACKOFF);
        assert!(failure.tries_again(1) && failure.tries_again(2) && !failure.tries_again(3));
        // Streams do not retry 429 on the same account (spec 10 §7).
        assert!(
            !classify_attempt(429, b"{}", true)
                .unwrap()
                .retry_same_account
        );
    }

    #[test]
    fn server_errors_retry_three_times_when_buffered() {
        let failure = classify_attempt(503, b"", false).unwrap();
        assert_eq!(failure.error.kind, ErrorKind::Transient);
        assert_eq!(failure.error.status, Some(503));
        assert!(failure.tries_again(1));
        assert!(!failure.tries_again(MAX_ATTEMPTS));
        assert!(!classify_attempt(503, b"", true).unwrap().retry_same_account);
        assert_eq!(
            backoff_before(ErrorKind::Transient, 2),
            Duration::from_secs(1)
        );
        assert_eq!(
            backoff_before(ErrorKind::Transient, 3),
            Duration::from_secs(2)
        );
        assert_eq!(backoff_before(ErrorKind::RateLimit, 3), RATE_LIMIT_BACKOFF);
        assert_eq!(backoff_before(ErrorKind::Request, 2), Duration::ZERO);
    }

    #[test]
    fn http_200_failure_wrapper_is_a_failure_not_an_empty_success() {
        let failure = classify_attempt(
            200,
            br#"{"status":{"failed":{"code":"upstream_http_400","message":"nope","retry_after":null}}}"#,
            false,
        )
        .unwrap();
        assert_eq!(failure.error.kind, ErrorKind::Request);
        assert_eq!(failure.error.message, "nope");
        assert!(classify_attempt(200, b"{\"type\":\"response.created\"}", false).is_none());
    }

    #[test]
    fn buffered_empty_and_html_200_bodies_are_transient() {
        let failure = classify_attempt(200, b"", false).unwrap();
        assert_eq!(failure.error.kind, ErrorKind::Transient);
        assert!(failure.retry_same_account);
        for body in [
            "<html><body>oops</body></html>",
            "  <!DOCTYPE html><html>",
            "\r\n<!doctype HTML>",
        ] {
            let failure = classify_attempt(200, body.as_bytes(), false).unwrap();
            assert_eq!(failure.error.kind, ErrorKind::Transient, "{body}");
        }
        // A stream body is never judged as a whole.
        assert!(classify_attempt(200, b"", true).is_none());
    }

    #[test]
    fn upstream_message_prefers_error_message_and_never_panics() {
        assert_eq!(upstream_message(br#"{"error":{"message":"boom"}}"#), "boom");
        assert_eq!(upstream_message(b"plain   text\nhere"), "plain text here");
        assert_eq!(upstream_message(b""), "upstream error");
    }

    /// Minimal one-shot HTTP/1.1 server: records the request (request line,
    /// headers, body) and answers with fixed bytes. No network access beyond
    /// loopback.
    fn serve_once(response: &'static [u8]) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            let header_end = loop {
                let read = socket.read(&mut chunk).unwrap();
                if read == 0 {
                    return String::from_utf8_lossy(&request).into_owned();
                }
                request.extend_from_slice(&chunk[..read]);
                if let Some(position) = find_subslice(&request, b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < header_end + length {
                let read = socket.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            socket.write_all(response).unwrap();
            socket.flush().unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (format!("http://{address}"), handle)
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[tokio::test]
    async fn http_client_sends_the_get_headers_and_returns_the_status() {
        let (base, server) = serve_once(
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 28\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"nope\"}}",
        );
        let client = HttpUpstreamClient::new(UpstreamBase::parse(&base).unwrap(), None).unwrap();
        let request = UpstreamRequest::new(Endpoint::Models)
            .bearer("tok-1")
            .zed_version();
        let response = client.execute(request).await.unwrap();
        let recorded = server.join().unwrap().to_ascii_lowercase();
        assert!(recorded.starts_with("get /models http/1.1"));
        assert!(recorded.contains("authorization: bearer tok-1"));
        assert!(recorded.contains("x-zed-version: 1.8.2"));
        assert!(recorded.contains("user-agent: zed/1.8.2 (linux; x86_64)"));
        // `/models` carries the version header only (spec 10 §2.4).
        assert!(!recorded.contains("x-zed-client-supports-status-messages"));
        assert_eq!(response.status, 400);
        let failure = classify_attempt(response.status, &response.body, false).unwrap();
        assert_eq!(failure.error.kind, ErrorKind::Request);
        assert_eq!(failure.error.message, "nope");
    }

    #[tokio::test]
    async fn http_client_posts_the_body_and_streams_lines() {
        let (base, server) = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{\"status\":\"stream_ended\"}\n",
        );
        let client = HttpUpstreamClient::new(UpstreamBase::parse(&base).unwrap(), None).unwrap();
        let request = UpstreamRequest::new(Endpoint::LlmTokens)
            .credentials("42", &serde_json::json!({"k": "v"}))
            .json_body(br#"{"organization_id":"org-1"}"#.to_vec())
            .system_id(Some("sys-9"));
        let stream = client.execute_stream(request).await.unwrap();
        let recorded = server.join().unwrap().to_ascii_lowercase();
        assert!(recorded.starts_with("post /client/llm_tokens http/1.1"));
        assert!(recorded.contains(r#"authorization: 42 {"k":"v"}"#));
        assert!(recorded.contains("x-zed-system-id: sys-9"));
        assert!(recorded.contains("content-type: application/json"));
        assert!(recorded.contains(r#"{"organization_id":"org-1"}"#));
        assert_eq!(stream.status, 200);
        let chunks: Vec<Result<Bytes, GatewayError>> = StreamExt::collect(stream.body).await;
        let bytes: Vec<u8> = chunks
            .into_iter()
            .flat_map(|item| item.unwrap().to_vec())
            .collect();
        assert_eq!(
            String::from_utf8(bytes).unwrap().trim(),
            r#"{"status":"stream_ended"}"#
        );
    }
}
