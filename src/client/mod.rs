//! Client protocol codecs: request JSON → [`NeutralRequest`], and
//! [`NeutralResponse`] / [`StreamEvent`] → the client's JSON or SSE
//! (spec `40`, `41`, `42`).
//!
//! One trait, three implementations, no branching on protocol elsewhere:
//! the entry layer picks the codec from the route.

pub mod anthropic;
pub mod chat;
pub mod responses;

use crate::error::GatewayError;
use crate::neutral::{NeutralRequest, NeutralResponse, StreamEvent};

/// Which client protocol a route speaks. Determines both the decode rules and
/// the error body shape (spec 30 §4, spec 70 §1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientProtocol {
    ChatCompletions,
    AnthropicMessages,
    Responses,
}

impl ClientProtocol {
    /// Content type for a non-streaming success response.
    pub fn content_type(self) -> &'static str {
        "application/json"
    }

    pub fn error_body(self, err: &GatewayError) -> Vec<u8> {
        match self {
            ClientProtocol::AnthropicMessages => crate::error::anthropic_error_body(err),
            ClientProtocol::ChatCompletions | ClientProtocol::Responses => {
                crate::error::openai_error_body(err)
            }
        }
    }
}

/// Decode a validated JSON object into the neutral request.
pub trait ClientDecoder: Send + Sync {
    fn decode(&self, body: &serde_json::Value) -> Result<NeutralRequest, GatewayError>;
}

/// Encode a reduced response into the exact bytes the client expects.
pub trait ClientEncoder: Send + Sync {
    fn encode_response(
        &self,
        response: &NeutralResponse,
        model: &str,
        now_unix: i64,
    ) -> Result<Vec<u8>, GatewayError>;

    /// One SSE frame (`event:`/`data:` lines plus the blank separator) for a
    /// neutral event, or `None` when the event has no client representation.
    fn encode_event(
        &self,
        event: &StreamEvent,
        state: &mut crate::proxy::stream::SseState,
    ) -> Result<Option<Vec<u8>>, GatewayError>;

    /// Terminal frames owed at the end of a stream (`[DONE]`, `message_stop`, …).
    fn finish(&self, state: &mut crate::proxy::stream::SseState) -> Result<Vec<u8>, GatewayError>;
}

/// Pick the decoder for a route (spec 70 §1.2).
pub fn decoder_for(protocol: ClientProtocol) -> Box<dyn ClientDecoder> {
    match protocol {
        ClientProtocol::ChatCompletions => Box::new(chat::ChatDecoder),
        ClientProtocol::AnthropicMessages => Box::new(anthropic::AnthropicDecoder),
        ClientProtocol::Responses => Box::new(responses::ResponsesDecoder),
    }
}

pub fn encoder_for(protocol: ClientProtocol) -> Box<dyn ClientEncoder> {
    match protocol {
        ClientProtocol::ChatCompletions => Box::new(chat::ChatEncoder),
        ClientProtocol::AnthropicMessages => Box::new(anthropic::AnthropicEncoder),
        ClientProtocol::Responses => Box::new(responses::ResponsesEncoder),
    }
}

/// Parse a client-supplied tool-call id or generate one per spec 45 §4.3.
/// Single authority for the `call_<16 hex>` shape used by all three protocols.
pub fn tool_call_id(existing: Option<&str>) -> String {
    match existing {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            use rand::RngCore;
            let mut bytes = [0u8; 8];
            rand::thread_rng().fill_bytes(&mut bytes);
            let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("call_{hex}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tool_call_ids_have_the_fixed_shape() {
        let id = tool_call_id(None);
        assert_eq!(id.len(), 21);
        assert!(id.starts_with("call_"));
        assert!(id[5..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_ne!(id, tool_call_id(None));
    }

    #[test]
    fn existing_ids_are_preserved_and_empty_ones_are_replaced() {
        assert_eq!(tool_call_id(Some("call_abc")), "call_abc");
        let generated = tool_call_id(Some(""));
        assert_eq!(generated.len(), 21);
        assert!(generated.starts_with("call_"));
    }
}
