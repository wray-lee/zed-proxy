//! Error classification (spec `30-error-taxonomy.md`) and client-facing error
//! rendering (spec `30` §4, spec `70` §8).

use std::fmt::Write as _;

/// Coarse failure class. The whole retry / cooldown / failover decision is
/// driven by this enum (spec 30 §2, §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The request itself is wrong; retrying on another account cannot help.
    Request,
    /// Credential problem for one account (401, non-plan 403).
    Auth,
    /// Plan-level 403; another account may have the model.
    Plan,
    /// Upstream rate limit.
    RateLimit,
    /// Network / 5xx / unclassifiable.
    Transient,
    /// The client went away; neither success nor failure for the account.
    ClientDisconnect,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Request => "request",
            ErrorKind::Auth => "auth",
            ErrorKind::Plan => "plan",
            ErrorKind::RateLimit => "rate_limit",
            ErrorKind::Transient => "transient",
            ErrorKind::ClientDisconnect => "client_disconnect",
        }
    }

    /// Whether the failure is charged to the account's cooldown state
    /// (spec 50 §5.4). Plan-level and request-level failures are not.
    pub fn is_account_level(self) -> bool {
        matches!(
            self,
            ErrorKind::Auth | ErrorKind::RateLimit | ErrorKind::Transient
        )
    }
}

/// One error travelling through the gateway. Kept small on purpose: it is the
/// `Err` half of nearly every `Result` in this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayError {
    pub kind: ErrorKind,
    /// Upstream HTTP status, when the failure came from an upstream response.
    pub status: Option<u16>,
    pub message: String,
    /// Stable machine-readable code for the client error body (spec 30 §4.1).
    pub code: Option<&'static str>,
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for GatewayError {}

impl GatewayError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            status: None,
            message: message.into(),
            code: None,
        }
    }

    fn with_status(kind: ErrorKind, status: u16, message: impl Into<String>) -> Self {
        Self {
            kind,
            status: Some(status),
            message: message.into(),
            code: None,
        }
    }

    pub fn request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Request, message)
    }

    pub fn auth(status: u16, message: impl Into<String>) -> Self {
        Self::with_status(ErrorKind::Auth, status, message)
    }

    pub fn plan(status: u16, message: impl Into<String>) -> Self {
        Self::with_status(ErrorKind::Plan, status, message)
    }

    pub fn rate_limit(message: impl Into<String>) -> Self {
        Self::with_status(ErrorKind::RateLimit, 429, message)
    }

    pub fn transient(status: Option<u16>, message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Transient,
            status,
            message: message.into(),
            code: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        let mut err = Self::new(ErrorKind::Transient, message);
        err.code = Some("internal_error");
        err
    }

    /// Placeholder used by the scaffolded modules that later implementation
    /// tasks fill in. Never reached by a wired route.
    pub fn not_implemented(what: &str) -> Self {
        let mut err = Self::new(ErrorKind::Transient, format!("not implemented: {what}"));
        err.code = Some("not_implemented");
        err
    }

    /// Final status returned to the client when every account failed
    /// (spec 30 §3.6).
    pub fn http_status(&self) -> u16 {
        match self.kind {
            ErrorKind::Request => 400,
            ErrorKind::Auth => match self.status {
                Some(403) => 403,
                _ => 401,
            },
            ErrorKind::Plan => 403,
            ErrorKind::RateLimit => 429,
            ErrorKind::Transient => {
                if self.code == Some("not_implemented") {
                    501
                } else {
                    502
                }
            }
            ErrorKind::ClientDisconnect => 499,
        }
    }

    pub fn openai_type(&self) -> &'static str {
        match self.kind {
            ErrorKind::Request => "invalid_request_error",
            ErrorKind::Auth => match self.status {
                Some(403) => "permission_error",
                _ => "authentication_error",
            },
            ErrorKind::Plan => "permission_error",
            ErrorKind::RateLimit => "rate_limit_error",
            ErrorKind::Transient => match self.status {
                Some(_) => "upstream_error",
                None => "api_error",
            },
            ErrorKind::ClientDisconnect => "api_error",
        }
    }

    pub fn anthropic_type(&self) -> &'static str {
        match self.kind {
            ErrorKind::Request => "invalid_request_error",
            ErrorKind::Auth => "authentication_error",
            ErrorKind::Plan => "permission_error",
            ErrorKind::RateLimit => "rate_limit_error",
            ErrorKind::Transient | ErrorKind::ClientDisconnect => "api_error",
        }
    }
}

/// Classify an upstream HTTP status (spec 30 §3.1). `plan_hint` is true when the
/// upstream body mentions `plan` (spec 30 §3.1, 403 row).
pub fn classify_status(status: u16, plan_hint: bool) -> ErrorKind {
    match status {
        400 | 404 | 405 | 413 | 415 | 422 => ErrorKind::Request,
        401 => ErrorKind::Auth,
        403 => {
            if plan_hint {
                ErrorKind::Plan
            } else {
                ErrorKind::Auth
            }
        }
        408 | 409 => ErrorKind::Transient,
        429 => ErrorKind::RateLimit,
        500..=599 => ErrorKind::Transient,
        _ => ErrorKind::Transient,
    }
}

/// `{"error":{"message":…,"type":…,"param":null[,"code":…]}}` (spec 30 §4.1).
pub fn openai_error_body(err: &GatewayError) -> Vec<u8> {
    let message = json_string(&err.message);
    let mut body = String::new();
    let _ = write!(
        body,
        "{{\"error\":{{\"message\":{message},\"type\":\"{}\",\"param\":null",
        err.openai_type()
    );
    if let Some(code) = err.code {
        let _ = write!(body, ",\"code\":\"{}\"", escape(code));
    }
    body.push_str("}}");
    body.into_bytes()
}

/// `{"type":"error","error":{"type":"…","message":"…"}}` (spec 30 §4.2).
pub fn anthropic_error_body(err: &GatewayError) -> Vec<u8> {
    let message = json_string(&err.message);
    format!(
        "{{\"type\":\"error\",\"error\":{{\"type\":\"{}\",\"message\":{message}}}}}",
        err.anthropic_type()
    )
    .into_bytes()
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// `code` values are compile-time constants, so only quotes/backslashes need
/// escaping; still run them through serde to stay valid JSON.
fn escape(value: &str) -> String {
    json_string(value).trim_matches('"').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_level_statuses_are_not_retried() {
        for status in [400, 404, 405, 413, 415, 422] {
            assert_eq!(classify_status(status, false), ErrorKind::Request);
            assert!(!ErrorKind::Request.is_account_level());
        }
    }

    #[test]
    fn credential_and_plan_403_are_distinguished() {
        assert_eq!(classify_status(401, false), ErrorKind::Auth);
        assert_eq!(classify_status(403, true), ErrorKind::Plan);
        assert_eq!(classify_status(403, false), ErrorKind::Auth);
        assert!(!ErrorKind::Plan.is_account_level());
    }

    #[test]
    fn rate_limit_and_transient_statuses() {
        assert_eq!(classify_status(429, false), ErrorKind::RateLimit);
        for status in [408, 409, 500, 502, 503, 504, 529] {
            assert_eq!(classify_status(status, false), ErrorKind::Transient);
        }
    }

    #[test]
    fn error_bodies_have_fixed_key_order() {
        let err = GatewayError::request("bad body");
        assert_eq!(
            String::from_utf8(openai_error_body(&err)).unwrap(),
            r#"{"error":{"message":"bad body","type":"invalid_request_error","param":null}}"#
        );
        assert_eq!(
            String::from_utf8(anthropic_error_body(&err)).unwrap(),
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad body"}}"#
        );
    }

    #[test]
    fn code_is_appended_after_param() {
        let mut err = GatewayError::request("bad body");
        err.code = Some("invalid_json");
        assert_eq!(
            String::from_utf8(openai_error_body(&err)).unwrap(),
            r#"{"error":{"message":"bad body","type":"invalid_request_error","param":null,"code":"invalid_json"}}"#
        );
    }

    #[test]
    fn final_status_follows_the_error_kind() {
        assert_eq!(GatewayError::request("x").http_status(), 400);
        assert_eq!(GatewayError::auth(401, "x").http_status(), 401);
        assert_eq!(GatewayError::auth(403, "x").http_status(), 403);
        assert_eq!(GatewayError::plan(403, "x").http_status(), 403);
        assert_eq!(GatewayError::rate_limit("x").http_status(), 429);
        assert_eq!(GatewayError::transient(None, "x").http_status(), 502);
    }
}
