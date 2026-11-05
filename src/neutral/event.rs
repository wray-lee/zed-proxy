//! Neutral stream events (spec 45 §1.2, names taken from that table verbatim).

use super::stop::StopReason;
use super::usage::Usage;

/// Which kind of content block the Anthropic-style block boundary refers to.
/// `block_start`/`block_stop` are produced only by the Anthropic provider
/// (spec 45 §1.2 note) so the Anthropic client encoder can mirror block
/// boundaries; other providers never emit them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Text,
    Thinking,
    ToolUse,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Upstream response started; `id` may be empty.
    Start {
        id: Option<String>,
    },
    BlockStart {
        index: u64,
        kind: BlockKind,
    },
    TextDelta {
        delta: String,
    },
    ThinkingDelta {
        delta: String,
    },
    ToolStart {
        index: u64,
        id: String,
        name: String,
        /// Upstream item was a Responses `custom_tool_call` (spec 42 §3.2
        /// item 5); arg deltas then carry the free-form input text.
        custom: bool,
    },
    ToolArgsDelta {
        index: u64,
        delta: String,
    },
    ToolEnd {
        index: u64,
    },
    BlockStop {
        index: u64,
    },
    Usage(Usage),
    /// Terminal event; no further events follow.
    End {
        reason: StopReason,
    },
    /// Failure after the response was committed; carries the client-visible
    /// classification (spec 30 §6).
    Error(crate::error::GatewayError),
}
