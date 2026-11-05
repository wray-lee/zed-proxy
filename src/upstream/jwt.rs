//! LLM token (JWT) inspection (spec 10 §2.3, spec 70 §3.13).
//!
//! The gateway never verifies the signature: the token comes from the
//! configured upstream over TLS. Only the payload is needed, both for `exp`
//! cache expiry and for the raw `/zed/usage` response body.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::Value;

/// Raw payload bytes of a JWT, byte for byte as the token carries them
/// (spec 70 §3.13), or `None` when the token is malformed or the payload is
/// not JSON. Never includes the token itself or its signature.
pub fn jwt_payload_bytes(token: &str) -> Option<Vec<u8>> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload.as_bytes()).ok()?;
    serde_json::from_slice::<Value>(&decoded).ok()?;
    Some(decoded)
}

/// Decoded payload of a JWT as a `Value`, for expiry and typing.
pub fn jwt_payload(token: &str) -> Option<Value> {
    serde_json::from_slice(&jwt_payload_bytes(token)?).ok()
}

/// `exp` from the payload, as Unix seconds.
pub fn jwt_exp(token: &str) -> Option<i64> {
    jwt_payload(token)?.get("exp")?.as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsigned(claims: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims.as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn reads_exp_and_payload() {
        let token = unsigned(r#"{"sub":"42","exp":1762300000}"#);
        assert_eq!(jwt_exp(&token), Some(1_762_300_000));
        assert_eq!(jwt_payload(&token).unwrap()["sub"], "42");
    }

    #[test]
    fn payload_bytes_are_returned_verbatim() {
        let claims = "{\n  \"sub\": \"42\",  \"name\": \"alice\",\n  \"exp\": 1762300000\n}";
        let token = unsigned(claims);
        assert_eq!(jwt_payload_bytes(&token).unwrap(), claims.as_bytes());
    }

    #[test]
    fn malformed_tokens_are_rejected_without_panicking() {
        for token in ["", "abc", "a.b", "a.!!!.c"] {
            assert!(jwt_payload(token).is_none(), "{token:?}");
            assert!(jwt_payload_bytes(token).is_none(), "{token:?}");
        }
    }
}
