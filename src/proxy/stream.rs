//! Streaming shell: SSE framing state and the client-facing stream type
//! (spec `16-stream-envelope.md`).
//!
//! The line reader, commit semantics and termination rules are specified in
//! `16`; this file holds the pieces the client encoders share.

use bytes::Bytes;
use futures_util::Stream;

use crate::error::GatewayError;

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
    pub next_block_index: u64,
    pub open_block: Option<u64>,
    pub tool_count: u64,
    pub text: String,
    pub thinking: String,
    pub usage: crate::neutral::Usage,
    pub stop_reason: crate::neutral::StopReason,
    /// `sequence_number` counter for the Responses protocol (spec 42 §3.1).
    pub sequence: u64,
    /// Client-protocol-specific slot metadata. Chat keeps argument text per
    /// tool index; Anthropic keeps a `"<neutral index>:<tag>"` per client block;
    /// Responses keeps an output item's metadata / completed JSON per
    /// `output_index`.
    pub tool_arguments: Vec<String>,
    /// Argument buffers keyed by neutral tool index (spec 45 §4.4A): every
    /// encoder appends only to `tool_args[event.index]`.
    pub tool_args: Vec<String>,
    /// Neutral tool index → client block index (Anthropic) / `output_index`
    /// (Responses); `NO_TOOL` means that slot is closed or was never opened.
    pub tool_blocks: Vec<u64>,
    /// Whether the tool at each neutral index uses free-form input.
    pub tool_custom: Vec<bool>,
    /// Client `output_index` of the open message/reasoning item (Responses).
    pub open_item_index: Option<u64>,
    /// Set when the client asked for `stream_options.include_usage`.
    pub include_usage: bool,
}

/// Sentinel for a tool slot that is closed or unknown (spec 45 §4.4A).
pub const NO_TOOL: u64 = u64::MAX;

impl SseState {
    pub fn new(model: impl Into<String>, created_unix: i64) -> Self {
        Self {
            model: model.into(),
            created_unix,
            sequence: 0,
            ..Self::default()
        }
    }

    /// Open the slot for neutral tool `index`, assigning it `client_index`.
    /// An absurd index is rejected instead of allocating a huge vector.
    pub fn ensure_tool(&mut self, index: u64, client_index: u64, custom: bool) -> bool {
        if index > 1024 || index == NO_TOOL || client_index == NO_TOOL {
            return false;
        }
        let slot = index as usize;
        while self.tool_args.len() <= slot {
            self.tool_args.push(String::new());
        }
        while self.tool_blocks.len() <= slot {
            self.tool_blocks.push(NO_TOOL);
        }
        while self.tool_custom.len() <= slot {
            self.tool_custom.push(false);
        }
        self.tool_args[slot].clear();
        self.tool_blocks[slot] = client_index;
        self.tool_custom[slot] = custom;
        true
    }

    /// The output slot assigned to an open tool, or `None` for an unknown /
    /// closed index. Out-of-range deltas must be dropped, not redirected.
    pub fn tool_slot(&self, index: u64) -> Option<u64> {
        let slot = usize::try_from(index).ok()?;
        let value = *self.tool_blocks.get(slot)?;
        (value != NO_TOOL).then_some(value)
    }

    pub fn close_tool(&mut self, index: u64) {
        if let Ok(slot) = usize::try_from(index) {
            if let Some(value) = self.tool_blocks.get_mut(slot) {
                *value = NO_TOOL;
            }
        }
    }

    pub fn open_tool_indices(&self) -> Vec<u64> {
        self.tool_blocks
            .iter()
            .enumerate()
            .filter(|(_, value)| **value != NO_TOOL)
            .map(|(index, _)| index as u64)
            .collect()
    }

    /// Grow `tool_arguments` (whose index meaning is client-protocol specific)
    /// to cover `index` and set its value.
    pub fn set_tool_argument(&mut self, index: u64, value: String) {
        if let Ok(slot) = usize::try_from(index) {
            while self.tool_arguments.len() <= slot {
                self.tool_arguments.push(String::new());
            }
            self.tool_arguments[slot] = value;
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
