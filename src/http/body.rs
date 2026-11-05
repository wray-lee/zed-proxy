//! Request/response body helpers: the 16 MiB request cap, the JSON parse
//! boundary and the two error-body shapes (spec 70 §1.2, §8; spec 30 §4).

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::Value;

/// Requests are rejected when `Content-Length` exceeds this (spec 70 §6).
pub const MAX_REQUEST_BODY: u64 = 16 * 1024 * 1024;

pub const INVALID_JSON_MESSAGE: &str = "invalid JSON request body";
/// Management endpoints use the simple body shape (spec 70 §8).
pub const INVALID_JSON_MANAGEMENT: &str = r#"{"error":"invalid json"}"#;
pub const NOT_OBJECT_MANAGEMENT: &str = r#"{"error":"request body must be an object"}"#;

/// The exact 413 body (spec 70 §7).
pub const BODY_TOO_LARGE: &str =
    r#"{"error":{"message":"request body exceeds 16 MiB","type":"invalid_request_error"}}"#;

pub const NOT_FOUND: &str = r#"{"error":"not found"}"#;

/// Content-Length as advertised by the client; missing or unparseable is 0
/// (spec 55 §3.1).
pub fn content_length(headers: &axum::http::HeaderMap) -> u64 {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

pub fn exceeds_limit(headers: &axum::http::HeaderMap) -> bool {
    content_length(headers) > MAX_REQUEST_BODY
}

/// Parse a request body as a JSON object. `None` covers both "not JSON" and
/// "valid JSON but not an object", which the entry layer treats identically
/// (spec 70 §1.2).
pub fn parse_json_object(bytes: &[u8]) -> Option<Value> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    value.is_object().then_some(value)
}

pub fn parse_json(bytes: &[u8]) -> Option<Value> {
    serde_json::from_slice(bytes).ok()
}

/// JSON response with the exact content type and length axum computes.
pub fn json_response(status: StatusCode, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

pub fn text_response(status: StatusCode, content_type: &'static str, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn missing_or_bad_content_length_counts_as_zero() {
        assert_eq!(content_length(&HeaderMap::new()), 0);
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("abc"));
        assert_eq!(content_length(&headers), 0);
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("42"));
        assert_eq!(content_length(&headers), 42);
    }

    #[test]
    fn the_limit_is_exactly_16_mib() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("16777216"));
        assert!(!exceeds_limit(&headers));
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("16777217"));
        assert!(exceeds_limit(&headers));
    }

    #[test]
    fn only_json_objects_pass_the_proxy_gate() {
        assert!(parse_json_object(br#"{"a":1}"#).is_some());
        for body in ["[]", "5", "\"x\"", "null", "{", ""] {
            assert!(parse_json_object(body.as_bytes()).is_none(), "{body}");
        }
    }
}
