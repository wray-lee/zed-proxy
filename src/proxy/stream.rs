//! Streaming shell: SSE framing state and the client-facing stream type
//! (spec `16-stream-envelope.md`).
//!
//! The line reader, commit semantics and termination rules are specified in
//! `16`; this file holds the pieces the client encoders share.

use bytes::Bytes;
use futures_util::Stream;

use crate::error::GatewayError;
use crate::neutral::StreamEvent;

/// One upstream line may be at most 64 KiB (spec 16 §2).
pub const MAX_LINE_BYTES: usize = 65_536;
/// Streams time out after 300 seconds (spec 16 §3).
pub const STREAM_TIMEOUT_SECS: u64 = 300;
/// Empty-200-stream retries per account (spec 16 §5).
pub const EMPTY_STREAM_RETRIES: u32 = 2;
pub const EMPTY_STREAM_BACKOFF_MS: u64 = 250;

/// Per-response state shared by the client encoder's `encode_event` calls:
/// header already sent, block indices, tool indices, text accumulated for
/// "done" events, usage.
#[derive(Debug, Default)]
pub struct SseState {
    pub model: String,
    pub created_unix: i64,
    pub response_id: Option<String>,
    pub committed: bool,
    pub next_block_index: u64,
    pub open_block: Option<u64>,
    pub tool_count: u64,
    pub text: String,
    pub thinking: String,
    pub usage: crate::neutral::Usage,
    pub stop_reason: crate::neutral::StopReason,
    /// `sequence_number` counter for the Responses protocol (spec 42 §3.1).
    pub sequence: u64,
    /// Tool argument text accumulated per neutral tool index.
    pub tool_arguments: Vec<String>,
    /// Set when the client asked for `stream_options.include_usage`.
    pub include_usage: bool,
}

impl SseState {
    pub fn new(model: impl Into<String>, created_unix: i64) -> Self {
        Self {
            model: model.into(),
            created_unix,
            sequence: 0,
            ..Self::default()
        }
    }
}

/// The SSE body returned to the client. Yielding `Err` aborts the response;
/// the connection is closed either way (spec 16 §1).
pub type SseStream = std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, GatewayError>> + Send>>;

/// Frame one SSE event in the Anthropic/Responses style:
/// `event: <name>\ndata: <json>\n\n` (spec 16 §8).
pub fn sse_event(event: &str, data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(event.len() + data.len() + 16);
    frame.extend_from_slice(b"event: ");
    frame.extend_from_slice(event.as_bytes());
    frame.extend_from_slice(b"\ndata: ");
    frame.extend_from_slice(data);
    frame.extend_from_slice(b"\n\n");
    frame
}

/// Frame a Chat-style SSE event: `data: <json>\n\n` (spec 16 §8).
pub fn sse_data(data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(data.len() + 8);
    frame.extend_from_slice(b"data: ");
    frame.extend_from_slice(data);
    frame.extend_from_slice(b"\n\n");
    frame
}

/// The literal end-of-stream marker for Chat (spec 40 §3.1).
pub const DONE_FRAME: &[u8] = b"data: [DONE]\n\n";

/// Split accumulating bytes into lines, honouring the 64 KiB cap: an overlong
/// line is dropped whole rather than truncated into a half object
/// (spec 16 §2 item 2).
#[derive(Debug, Default)]
pub struct LineBuffer {
    buffer: Vec<u8>,
    discarding: bool,
}

impl LineBuffer {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for byte in chunk {
            if *byte == b'\n' {
                if !self.discarding {
                    let line = String::from_utf8_lossy(&self.buffer).into_owned();
                    lines.push(line);
                }
                self.buffer.clear();
                self.discarding = false;
                continue;
            }
            if self.discarding {
                continue;
            }
            if self.buffer.len() >= MAX_LINE_BYTES {
                self.buffer.clear();
                self.discarding = true;
                continue;
            }
            self.buffer.push(*byte);
        }
        lines
    }

    /// Trailing bytes at EOF (a final line without a newline).
    pub fn finish(&mut self) -> Option<String> {
        if self.discarding || self.buffer.is_empty() {
            self.buffer.clear();
            self.discarding = false;
            return None;
        }
        let line = String::from_utf8_lossy(&self.buffer).into_owned();
        self.buffer.clear();
        Some(line)
    }
}

/// Turn one parsed upstream line into an event, dropping protocol metadata
/// (spec 16 §2, §3): blank lines, non-JSON lines and `stream_ended`.
pub fn parse_line(line: &str) -> Option<serde_json::Value> {
    let cleaned = crate::provider::clean_line(line);
    if cleaned.is_empty() || !cleaned.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(cleaned).ok()?;
    if crate::provider::is_stream_ended(&value) {
        return None;
    }
    Some(value)
}

/// Classify the end-of-stream situation for the retry decision
/// (spec 16 §5): HTTP 200, no model event, not yet committed.
pub fn is_empty_stream(status: u16, saw_model_event: bool, committed: bool) -> bool {
    status == 200 && !saw_model_event && !committed
}

/// Structural `{"status":"stream_ended"}` match on a raw upstream line; the
/// reader stops here without waiting for EOF (spec 16 §3). The provider
/// decoders drop the marker themselves, so the shell needs its own check to
/// know when to release the connection.
pub(crate) fn line_is_stream_ended(line: &str) -> bool {
    let cleaned = crate::provider::clean_line(line);
    if !cleaned.starts_with('{') {
        return false;
    }
    match serde_json::from_str::<serde_json::Value>(cleaned) {
        Ok(value) => crate::provider::is_stream_ended(&value),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_frames_have_the_documented_bytes() {
        assert_eq!(
            sse_event("message_stop", br#"{"type":"message_stop"}"#),
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec()
        );
        assert_eq!(sse_data(br#"{"a":1}"#), b"data: {\"a\":1}\n\n".to_vec());
        assert_eq!(DONE_FRAME, b"data: [DONE]\n\n");
    }

    #[test]
    fn line_buffer_splits_and_keeps_trailing_line() {
        let mut buffer = LineBuffer::default();
        assert_eq!(
            buffer.push(b"a\nb\r\n"),
            vec!["a".to_string(), "b\r".to_string()]
        );
        assert_eq!(buffer.finish(), None);
        let mut buffer = LineBuffer::default();
        assert!(buffer.push(b"tail").is_empty());
        assert_eq!(buffer.finish().as_deref(), Some("tail"));
    }

    #[test]
    fn overlong_lines_are_dropped_not_truncated() {
        let mut buffer = LineBuffer::default();
        let long = vec![b'x'; MAX_LINE_BYTES + 10];
        let mut lines = buffer.push(&long);
        lines.extend(buffer.push(b"\nnext\n"));
        assert_eq!(lines, vec!["next".to_string()]);
    }

    #[test]
    fn parse_line_skips_non_json_and_stream_ended() {
        assert!(parse_line("").is_none());
        assert!(parse_line("ping").is_none());
        assert!(parse_line(r#"{"status":"stream_ended"}"#).is_none());
        assert!(parse_line("data: {\"a\":1}").is_some());
        assert!(parse_line("{not json").is_none());
    }

    #[test]
    fn empty_stream_requires_http_200_without_events_or_commit() {
        assert!(is_empty_stream(200, false, false));
        assert!(!is_empty_stream(200, true, false));
        assert!(!is_empty_stream(200, false, true));
        assert!(!is_empty_stream(502, false, false));
    }

    #[test]
    fn stream_ended_marker_matches_structurally_only() {
        assert!(line_is_stream_ended(r#"{"status":"stream_ended"}"#));
        assert!(line_is_stream_ended(
            "  data: {\"status\":\"stream_ended\"}\r"
        ));
        assert!(!line_is_stream_ended(r#"{"delta":"stream_ended"}"#));
        assert!(!line_is_stream_ended(r#"{"status":"other"}"#));
        assert!(!line_is_stream_ended("ping"));
    }
}
