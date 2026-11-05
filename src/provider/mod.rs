//! Provider encoders: neutral request → `provider_request`, and upstream
//! response → neutral events (spec `20`, `21`, `22`).
//!
//! The three providers share nothing but the neutral types and the small
//! envelope/line helpers in this module, so adding one never touches the
//! others (spec 00 invariant I3).

pub mod anthropic;
pub mod google;
pub mod google_schema;
pub mod openai;

use rand::RngCore;

use crate::catalog::{ModelCapabilities, Provider as ProviderKind};
use crate::error::GatewayError;
use crate::neutral::{NeutralRequest, NeutralResponse, StreamEvent};

pub const INTENT_USER_PROMPT: &str = "user_prompt";

/// Stateless half of a provider: request encoding and non-streaming reduction.
pub trait ProviderEncoder: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// Encode the neutral request into the `provider_request` object, byte for
    /// byte (keys in the order fixed by the provider spec).
    fn encode_request(
        &self,
        request: &NeutralRequest,
        capabilities: &ModelCapabilities,
    ) -> Result<Vec<u8>, GatewayError>;

    /// Reduce a complete upstream body (line-separated events) into one result.
    fn decode_response(&self, body: &[u8]) -> Result<NeutralResponse, GatewayError>;

    /// Fresh streaming decoder for one response.
    fn stream_decoder(&self) -> Box<dyn StreamDecoder>;
}

/// Streaming half of a provider: consumes upstream lines and yields neutral
/// events. Implementations must ignore `data: ` prefixes, non-JSON lines and
/// the `stream_ended` marker (spec 16 §2, §3).
pub trait StreamDecoder: Send {
    /// Feed one upstream line (already stripped of `\n` / `\r`).
    fn push_line(&mut self, line: &str) -> Result<Vec<StreamEvent>, GatewayError>;

    /// Called at EOF; emits the terminal events the provider owes (usage/end).
    fn finish(&mut self) -> Result<Vec<StreamEvent>, GatewayError>;

    /// True once at least one model event was seen; an HTTP 200 stream that
    /// never sets this is the "empty stream" case (spec 16 §5).
    fn saw_model_event(&self) -> bool;

    /// Terminal failure reported by the upstream body, if any.
    fn failure(&self) -> Option<GatewayError>;
}

/// Look up the encoder for a provider kind.
pub fn encoder_for(kind: ProviderKind) -> Box<dyn ProviderEncoder> {
    match kind {
        ProviderKind::OpenAi => Box::new(openai::OpenAiProvider),
        ProviderKind::Anthropic => Box::new(anthropic::AnthropicProvider),
        ProviderKind::Google => Box::new(google::GoogleProvider),
    }
}

/// Placeholder decoder used until each provider's streaming parser lands. It
/// accepts every line (so buffered paths can drain a stream) and reports a
/// single terminal error instead of emitting model events.
pub struct StubStreamDecoder {
    kind: ProviderKind,
}

impl StubStreamDecoder {
    pub fn new(kind: ProviderKind) -> Self {
        Self { kind }
    }
}

impl StreamDecoder for StubStreamDecoder {
    fn push_line(&mut self, _line: &str) -> Result<Vec<StreamEvent>, GatewayError> {
        Ok(Vec::new())
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, GatewayError> {
        Err(GatewayError::not_implemented(&format!(
            "{} stream decoding",
            self.kind.as_str()
        )))
    }

    fn saw_model_event(&self) -> bool {
        false
    }

    fn failure(&self) -> Option<GatewayError> {
        None
    }
}

/// The `completions` envelope (spec 10 §3): compact JSON, fixed key order,
/// two independent random UUIDv4s.
pub fn build_envelope(
    provider: ProviderKind,
    model: &str,
    provider_request: &[u8],
) -> Result<Vec<u8>, GatewayError> {
    let thread_id = uuid_v4();
    let mut prompt_id = uuid_v4();
    let mut guard = 0;
    while prompt_id == thread_id && guard < 8 {
        prompt_id = uuid_v4();
        guard += 1;
    }
    let model = serde_json::to_string(model)
        .map_err(|err| GatewayError::internal(format!("cannot encode model name: {err}")))?;
    let mut envelope = Vec::with_capacity(provider_request.len() + 192);
    envelope.extend_from_slice(
        format!(
            "{{\"thread_id\":\"{thread_id}\",\"prompt_id\":\"{prompt_id}\",\"intent\":\"{INTENT_USER_PROMPT}\",\"provider\":\"{}\",\"model\":{model},\"provider_request\":",
            provider.as_str()
        )
        .as_bytes(),
    );
    envelope.extend_from_slice(provider_request);
    envelope.push(b'}');
    Ok(envelope)
}

/// 36-character lowercase UUIDv4 from the OS CSPRNG (spec 10 §3.1).
pub fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Strip the optional `data: ` prefix and surrounding whitespace from one
/// upstream line (spec 16 §2 item 4).
pub fn clean_line(line: &str) -> &str {
    let trimmed = line.trim_matches(['\r', ' ', '\t']);
    match trimmed.strip_prefix("data:") {
        Some(rest) => rest.trim_matches(['\r', ' ', '\t']),
        None => trimmed,
    }
}

/// `{"status":"stream_ended"}` — structural match only (spec 10 §4.5).
pub fn is_stream_ended(value: &serde_json::Value) -> bool {
    value.get("status").and_then(serde_json::Value::as_str) == Some("stream_ended")
}

/// `{"status":{"failed":{…}}}` — the HTTP-200 failure wrapper (spec 10 §4.6).
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
    let code = failed.get("code").and_then(serde_json::Value::as_str);
    let status = code
        .and_then(|code| code.strip_prefix("upstream_http_"))
        .and_then(|suffix| suffix.parse::<u16>().ok());
    let plan_hint = message.to_ascii_lowercase().contains("plan");
    let kind = match status {
        Some(status) => crate::error::classify_status(status, plan_hint),
        None => {
            if retry_after {
                crate::error::ErrorKind::RateLimit
            } else {
                crate::error::ErrorKind::Transient
            }
        }
    };
    let kind = if retry_after && kind != crate::error::ErrorKind::Request {
        crate::error::ErrorKind::RateLimit
    } else {
        kind
    };
    let mut err = GatewayError::new(kind, message);
    err.status = status;
    Some(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_shape_is_v4() {
        let first = uuid_v4();
        let second = uuid_v4();
        assert_eq!(first.len(), 36);
        assert_eq!(first.as_bytes()[14], b'4');
        assert!(matches!(first.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'));
        assert_eq!(&first[8..9], "-");
        assert_ne!(first, second, "two draws must differ");
    }

    #[test]
    fn envelope_key_order_matches_the_spec() {
        let envelope =
            build_envelope(ProviderKind::OpenAi, "gpt-5.6-sol", b"{\"model\":\"x\"}").unwrap();
        let text = String::from_utf8(envelope).unwrap();
        let prefix = "{\"thread_id\":\"";
        assert!(text.starts_with(prefix));
        let keys: Vec<&str> = [
            "\"prompt_id\":",
            "\"intent\":\"user_prompt\"",
            "\"provider\":\"open_ai\"",
            "\"model\":\"gpt-5.6-sol\"",
            "\"provider_request\":",
        ]
        .to_vec();
        let mut position = 0;
        for key in keys {
            let found = text[position..]
                .find(key)
                .unwrap_or_else(|| panic!("missing {key}"));
            position += found + key.len();
        }
        assert!(text.ends_with("}"));
    }

    #[test]
    fn stream_ended_requires_structural_match() {
        let ended: serde_json::Value =
            serde_json::from_str(r#"{"status":"stream_ended"}"#).unwrap();
        assert!(is_stream_ended(&ended));
        let text: serde_json::Value = serde_json::from_str(r#"{"delta":"stream_ended"}"#).unwrap();
        assert!(!is_stream_ended(&text));
    }

    #[test]
    fn status_failed_maps_to_request_level_errors() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"status":{"failed":{"code":"upstream_http_400","message":"bad thing","retry_after":null}}}"#,
        )
        .unwrap();
        let err = parse_status_failed(&value).unwrap();
        assert_eq!(err.kind, crate::error::ErrorKind::Request);
        assert_eq!(err.status, Some(400));
        assert_eq!(err.message, "bad thing");
        assert!(!err.kind.is_account_level());
    }

    #[test]
    fn status_failed_rate_limit_and_unknown() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"status":{"failed":{"code":"unknown","message":"x","retry_after":5}}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_status_failed(&value).unwrap().kind,
            crate::error::ErrorKind::RateLimit
        );
        let value: serde_json::Value = serde_json::from_str(
            r#"{"status":{"failed":{"code":"upstream_http_401","message":"x","retry_after":null}}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_status_failed(&value).unwrap().kind,
            crate::error::ErrorKind::Auth
        );
    }

    #[test]
    fn clean_line_strips_data_prefix() {
        assert_eq!(clean_line("  data: {\"a\":1}\r"), "{\"a\":1}");
        assert_eq!(clean_line("{\"a\":1}"), "{\"a\":1}");
    }
}
