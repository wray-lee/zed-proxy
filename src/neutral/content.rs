//! Message, system and content block types (spec 45 §1.1 `parts`).

use serde_json::Value;

/// One system prompt block. `cache_control` is forwarded verbatim to the
/// providers that support prompt caching (Anthropic only); Claude Code puts it
/// on individual system blocks, so the block boundary must be preserved.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemBlock {
    pub text: String,
    pub cache_control: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One conversation turn. Empty parts are allowed at the neutral layer; each
/// provider encoder decides whether to skip the message (spec 45 §4.8).
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImageSource {
    /// `data:<mime>;base64,<data>` was decoded into its two halves.
    Data { media_type: String, data: String },
    /// Remote `http(s)` URL; the gateway never downloads it.
    Url(String),
}

/// A tool call. `arguments` is JSON *text* (what every client protocol carries);
/// providers that want an object parse it at their boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub arguments: String,
    /// Opaque per-provider token that must be echoed back with the call
    /// (Gemini `thoughtSignature`, spec 22 §5.3.3).
    pub signature: Option<String>,
    /// The upstream reported this call as a Responses `custom_tool_call`
    /// (free-form tool). `arguments` then holds the raw free-form input text
    /// (not JSON); the Responses client encoder emits the `custom_tool_call`
    /// item shape from it.
    pub custom: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub output: String,
    pub is_error: bool,
    /// The client echoed a Responses `custom_tool_call_output`; only the native
    /// OpenAI shape may write that item type back (spec 42 §1.2, §4 R6).
    pub custom: bool,
}

/// A provider-specific item that has no cross-provider meaning and may only be
/// replayed to the provider that produced it (spec 20 §4.4).
#[derive(Debug, Clone, PartialEq)]
pub struct Opaque {
    pub provider: String,
    pub item: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text(String),
    Image(ImageSource),
    ToolUse(ToolUse),
    ToolResult(ToolResult),
    /// Reasoning / thinking text produced by the model. History thinking blocks
    /// are dropped on purpose (spec 41 §1.3), so no decoder builds this today;
    /// it stays part of the neutral part set (spec 45 §1.1).
    #[allow(dead_code)]
    Thinking(String),
    /// Server-side compaction summary (spec 21 §7).
    Compaction(String),
    /// Responses-native item replayed verbatim (spec 20 §4.4); only the OpenAI
    /// client decodes it, and only for a native passthrough request.
    #[allow(dead_code)]
    Opaque(Opaque),
}
