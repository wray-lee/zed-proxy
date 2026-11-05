//! Neutral non-streaming response (spec 45 §1.2, 40/41/42 §2).

use super::content::{ContentBlock, ToolUse};
use super::stop::StopReason;
use super::usage::Usage;

/// The reduced result of one upstream call, before client-protocol encoding.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NeutralResponse {
    /// Upstream response id, when the provider reports one.
    pub id: Option<String>,
    /// Visible text, already concatenated in order.
    pub text: String,
    /// Reasoning text (upstream summary or raw thinking), already concatenated.
    pub thinking: String,
    pub tool_calls: Vec<ToolUse>,
    /// Other blocks in order (e.g. compaction); most providers return none.
    pub blocks: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}
