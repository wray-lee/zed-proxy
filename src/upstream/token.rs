//! Per-account LLM token cache and exchange (spec 00 §5, spec 10 §2.3,
//! spec 30 §3.4).

use std::sync::OnceLock;

use serde_json::Value;

use super::client::{classify_attempt, Endpoint, UpstreamClient, UpstreamRequest};
use super::jwt::jwt_exp;
use crate::error::{ErrorKind, GatewayError};

/// A token is refreshed this many seconds before its `exp` (spec 00 §5).
pub const REFRESH_MARGIN_S: i64 = 60;

/// Cached token for one account. `exp == 0` means "unknown", which is treated
/// as always stale so a malformed token is never reused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenCache {
    token: Option<String>,
    exp: i64,
}

impl TokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self) -> Option<&str> {
        self.token.as_deref()
    }

    #[cfg(test)]
    pub fn exp(&self) -> i64 {
        self.exp
    }

    /// Whether the cached token may still be used at `now`.
    pub fn is_fresh(&self, now: i64) -> bool {
        match &self.token {
            Some(token) if !token.is_empty() => self.exp != 0 && !needs_refresh(now, self.exp),
            _ => false,
        }
    }

    /// Store a freshly minted token; `exp` is read from the JWT payload and
    /// defaults to 0 (always stale) when absent.
    #[cfg(test)]
    pub fn store(&mut self, token: String, now: i64) {
        self.exp = jwt_exp(&token).unwrap_or(0);
        if self.exp <= now {
            // A token that is already expired must not be cached as fresh.
            self.exp = 0;
        }
        self.token = Some(token);
    }

    pub fn store_with_exp(&mut self, token: String, exp: i64) {
        self.token = Some(token);
        self.exp = exp;
    }

    pub fn clear(&mut self) {
        self.token = None;
        self.exp = 0;
    }
}

/// Refresh when the clock has reached `exp - 60` (spec 00 §5).
pub fn needs_refresh(now: i64, exp: i64) -> bool {
    now >= exp - REFRESH_MARGIN_S
}

/// `organization_id` for the token exchange: `default_organization_id`, else the
/// first `organizations[].is_personal == true`, else the first organization
/// (spec 10 §2.3). `None` means the exchange cannot be attempted.
pub fn organization_id(me: &Value) -> Option<String> {
    if let Some(id) = me.get("default_organization_id").and_then(Value::as_str) {
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    let organizations = me.get("organizations").and_then(Value::as_array)?;
    let personal = organizations
        .iter()
        .find(|org| org.get("is_personal").and_then(Value::as_bool) == Some(true));
    personal
        .or_else(|| organizations.first())
        .and_then(|org| org.get("id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// `ZED_SYSTEM_ID` (spec 75 §1): parsed once, sent only when non-empty.
pub fn system_id() -> Option<&'static str> {
    static SYSTEM_ID: OnceLock<Option<String>> = OnceLock::new();
    SYSTEM_ID
        .get_or_init(|| {
            std::env::var("ZED_SYSTEM_ID")
                .ok()
                .filter(|v| !v.is_empty())
        })
        .as_deref()
}

/// Build the `client/llm_tokens` request for one account (spec 10 §2.3).
pub fn token_request(user_id: &str, credential: &Value, organization_id: &str) -> UpstreamRequest {
    let body = format!(
        "{{\"organization_id\":{}}}",
        serde_json::to_string(organization_id).unwrap_or_else(|_| "\"\"".to_string())
    );
    UpstreamRequest::new(Endpoint::LlmTokens)
        .credentials(user_id, credential)
        .json_body(body.into_bytes())
        .system_id(system_id())
}

/// Build the `client/users/me` request for one account (spec 10 §2.5).
pub fn me_request(user_id: &str, credential: &Value) -> UpstreamRequest {
    UpstreamRequest::new(Endpoint::UserMe)
        .credentials(user_id, credential)
        .header("accept", "application/json")
        .header("content-type", "application/json")
}

/// One minted token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedToken {
    pub token: String,
    pub exp: i64,
}

/// Exchange the account credential for a fresh LLM token (spec 10 §2.3).
/// Failures are classified per spec 30 §3.4: a `users/me` or `llm_tokens`
/// transport/5xx problem is transient; only 401/403 are credential failures.
pub async fn mint_token(
    client: &dyn UpstreamClient,
    user_id: &str,
    credential: &Value,
) -> Result<MintedToken, GatewayError> {
    let me = client.execute(me_request(user_id, credential)).await?;
    if let Some(failure) = classify_attempt(me.status, &me.body, false) {
        return Err(failure.error);
    }
    let me_value: Value = serde_json::from_slice(&me.body).map_err(|_| {
        GatewayError::new(
            ErrorKind::Transient,
            "cannot parse client/users/me response",
        )
    })?;
    let Some(organization_id) = organization_id(&me_value) else {
        let mut err = GatewayError::auth(401, "no organization id for this account");
        err.code = Some("no_organization");
        return Err(err);
    };

    let response = client
        .execute(token_request(user_id, credential, &organization_id))
        .await?;
    if let Some(failure) = classify_attempt(response.status, &response.body, false) {
        return Err(failure.error);
    }
    let value: Value = serde_json::from_slice(&response.body).map_err(|_| {
        GatewayError::new(
            ErrorKind::Auth,
            "client/llm_tokens response is not valid JSON",
        )
    })?;
    let Some(token) = value
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
    else {
        return Err(GatewayError::auth(
            401,
            "client/llm_tokens response has no token",
        ));
    };
    let exp = jwt_exp(token).unwrap_or(0);
    Ok(MintedToken {
        token: token.to_string(),
        exp,
    })
}

/// Cached token when still fresh, otherwise mint and store a new one
/// (spec 00 §5, spec 30 §3.4). The caller clears the cache on an upstream 401
/// and calls this again, which mints exactly one new token.
pub async fn cached_or_minted(
    client: &dyn UpstreamClient,
    cache: &mut TokenCache,
    user_id: &str,
    credential: &Value,
    now: i64,
) -> Result<String, GatewayError> {
    if cache.is_fresh(now) {
        if let Some(token) = cache.get() {
            return Ok(token.to_string());
        }
    }
    let minted = mint_token(client, user_id, credential).await?;
    cache.store_with_exp(minted.token.clone(), minted.exp);
    Ok(minted.token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_window_starts_sixty_seconds_before_expiry() {
        let exp = 1_762_300_000;
        assert!(!needs_refresh(exp - 61, exp));
        assert!(needs_refresh(exp - 60, exp));
        assert!(needs_refresh(exp, exp));
    }

    #[test]
    fn empty_or_expired_cache_is_never_fresh() {
        let mut cache = TokenCache::new();
        assert!(!cache.is_fresh(100));
        cache.store_with_exp("t".into(), 1_000);
        assert!(cache.is_fresh(900));
        assert!(!cache.is_fresh(940));
        cache.clear();
        assert!(!cache.is_fresh(900));
    }

    #[test]
    fn tokens_without_exp_are_treated_as_stale() {
        let mut cache = TokenCache::new();
        cache.store("not-a-jwt".into(), 100);
        assert_eq!(cache.exp(), 0);
        assert!(!cache.is_fresh(100));
    }

    #[test]
    fn organization_id_prefers_default_then_personal_then_first() {
        let me = serde_json::json!({"default_organization_id": "org-default",
            "organizations": [{"id": "org-a"}, {"id": "org-b", "is_personal": true}]});
        assert_eq!(organization_id(&me).as_deref(), Some("org-default"));

        let me = serde_json::json!({"organizations": [{"id": "org-a"}, {"id": "org-b", "is_personal": true}]});
        assert_eq!(organization_id(&me).as_deref(), Some("org-b"));

        let me = serde_json::json!({"organizations": [{"id": "org-a"}, {"id": "org-b"}]});
        assert_eq!(organization_id(&me).as_deref(), Some("org-a"));

        assert!(organization_id(&serde_json::json!({"organizations": []})).is_none());
        assert!(organization_id(&serde_json::json!({})).is_none());
        assert!(organization_id(&serde_json::json!({"default_organization_id": ""})).is_none());
    }

    /// Fake upstream that serves a queued list of (status, body) responses in
    /// order and records each request path.
    struct FakeUpstream {
        responses: std::sync::Mutex<std::collections::VecDeque<(u16, Vec<u8>)>>,
        paths: std::sync::Mutex<Vec<String>>,
    }

    impl FakeUpstream {
        fn new(responses: Vec<(u16, Vec<u8>)>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses.into()),
                paths: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn paths(&self) -> Vec<String> {
            self.paths.lock().unwrap().clone()
        }
    }

    impl UpstreamClient for FakeUpstream {
        fn execute<'a>(
            &'a self,
            request: UpstreamRequest,
        ) -> super::super::client::BoxFuture<
            'a,
            Result<super::super::client::UpstreamResponse, GatewayError>,
        > {
            self.paths
                .lock()
                .unwrap()
                .push(request.endpoint.path().to_string());
            let next = self.responses.lock().unwrap().pop_front();
            Box::pin(async move {
                let (status, body) = next.ok_or_else(|| GatewayError::internal("no response"))?;
                Ok(super::super::client::UpstreamResponse { status, body })
            })
        }

        fn execute_stream<'a>(
            &'a self,
            _request: UpstreamRequest,
        ) -> super::super::client::BoxFuture<
            'a,
            Result<super::super::client::UpstreamStream, GatewayError>,
        > {
            Box::pin(async move { Err(GatewayError::internal("not used")) })
        }
    }

    fn jwt(exp: i64) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#).as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[tokio::test]
    async fn mint_token_uses_the_default_organization_and_reads_exp() {
        let token = jwt(1_900_000_000);
        let upstream = FakeUpstream::new(vec![
            (
                200,
                br#"{"default_organization_id":"org-d","organizations":[{"id":"org-x"}]}"#.to_vec(),
            ),
            (200, format!(r#"{{"token":"{token}"}}"#).into_bytes()),
        ]);
        let minted = mint_token(&upstream, "42", &serde_json::json!({"k": "v"}))
            .await
            .unwrap();
        assert_eq!(minted.token, token);
        assert_eq!(minted.exp, 1_900_000_000);
        assert_eq!(
            upstream.paths(),
            vec!["/client/users/me", "/client/llm_tokens"]
        );
    }

    #[tokio::test]
    async fn mint_token_classifies_failures() {
        // users/me 5xx is transient, never a credential failure (spec 30 D3-A).
        let upstream = FakeUpstream::new(vec![(503, b"down".to_vec())]);
        let err = mint_token(&upstream, "42", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Transient);
        // users/me 401 is a credential failure.
        let upstream = FakeUpstream::new(vec![(401, b"nope".to_vec())]);
        let err = mint_token(&upstream, "42", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Auth);
        // llm_tokens without a token is a credential failure.
        let upstream = FakeUpstream::new(vec![
            (200, br#"{"organizations":[{"id":"org-x"}]}"#.to_vec()),
            (200, br#"{"nothing":true}"#.to_vec()),
        ]);
        let err = mint_token(&upstream, "42", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Auth);
        assert_eq!(err.message, "client/llm_tokens response has no token");
    }

    #[tokio::test]
    async fn cached_or_minted_reuses_fresh_tokens_and_remints_after_clear() {
        let token = jwt(1_900_000_000);
        let upstream = FakeUpstream::new(vec![
            (200, br#"{"default_organization_id":"org-d"}"#.to_vec()),
            (200, format!(r#"{{"token":"{token}"}}"#).into_bytes()),
        ]);
        let mut cache = TokenCache::new();
        let first = cached_or_minted(&upstream, &mut cache, "42", &Value::Null, 100)
            .await
            .unwrap();
        assert_eq!(first, token);
        let second = cached_or_minted(&upstream, &mut cache, "42", &Value::Null, 101)
            .await
            .unwrap();
        assert_eq!(second, token);
        // Cached hits never touch the upstream.
        assert_eq!(upstream.paths().len(), 2);
        // A 401 clears the cache; the next call mints again.
        cache.clear();
        let _ = cached_or_minted(&upstream, &mut cache, "42", &Value::Null, 102).await;
        assert!(upstream.paths().len() > 2);
    }

    #[test]
    fn token_request_body_is_only_the_organization() {
        let request = token_request("42", &serde_json::json!({"k": "v"}), "org-1");
        assert_eq!(request.endpoint, Endpoint::LlmTokens);
        assert_eq!(
            String::from_utf8(request.body).unwrap(),
            r#"{"organization_id":"org-1"}"#
        );
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| { name == "authorization" && value == r#"42 {"k":"v"}"# }));
    }
}
