//! The neutral request every client protocol decoder produces and every provider
//! encoder consumes (spec 45 §1.1).

use serde_json::Value;

use super::content::{Message, SystemBlock, ToolResult, ToolUse};
use super::tools::{ToolChoice, ToolDef};
use crate::catalog::Provider;

/// Reasoning controls. `effort` is the only cross-provider knob; `extra` carries
/// the remaining OpenAI `reasoning` members (`summary`, `context`, …) verbatim and
/// is consumed by that provider alone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reasoning {
    pub effort: Option<String>,
    pub extra: Option<Value>,
}

/// Everything a provider encoder needs. Fields that a given client protocol
/// cannot express stay `None` rather than being guessed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NeutralRequest {
    /// Normalized slug (spec 15 §7).
    pub model: String,
    pub provider: Provider,
    /// System prompt blocks in client order.
    pub system: Vec<SystemBlock>,
    /// The client supplied its system prompt as a single JSON string and no
    /// message-level system turn was appended, so the Anthropic provider may
    /// keep the string shorthand (spec 21 §4).
    pub system_string: bool,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub tool_choice: ToolChoice,
    /// `None` = the client did not say anything about parallelism.
    pub parallel_tool_calls: Option<bool>,
    pub reasoning: Reasoning,
    /// Anthropic `thinking` object, forwarded verbatim by that provider only.
    pub thinking: Option<Value>,
    /// Anthropic `output_config`, forwarded verbatim by that provider only.
    pub output_config: Option<Value>,
    /// Anthropic `context_management`, forwarded verbatim by that provider only.
    pub context_management: Option<Value>,
    /// Client output budget, before provider floors/clamps.
    pub max_output_tokens: Option<i64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<i64>,
    pub stop: Vec<String>,
    pub stream: bool,
    /// Chat `stream_options.include_usage`.
    pub include_usage: bool,
    /// Responses-only client keys forwarded verbatim by the OpenAI provider
    /// (spec 20 §1 `passthrough`), in client key order.
    pub passthrough: Vec<(String, Value)>,
    /// Tool calls carried by this request, used to resolve tool-result ids back
    /// to function names for providers that match by name (spec 45 §4.3).
    pub tool_uses: Vec<ToolUse>,
    /// Tool results whose id matches no call in this request. Providers encode
    /// them as text with a fixed prefix (spec 45 §4.3 item 4).
    pub unpaired_tool_results: Vec<ToolResult>,
}
