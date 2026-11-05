//! Tools and tool choice (spec 45 §1.1, §4.1, §4.2).

use serde_json::Value;

/// One tool declaration in the neutral shape. `parameters` is a JSON Schema; the
/// Google provider cleans it into the Gemini subset (spec 45 §4.1.1) and the
/// others forward it as-is.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// OpenAI `strict` flag; other providers ignore it.
    pub strict: Option<bool>,
    /// Anthropic prompt-caching marker, forwarded only by that provider.
    pub cache_control: Option<Value>,
    /// Responses `custom` tool format; only meaningful for OpenAI provider.
    pub format: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolChoice {
    /// The client sent no `tool_choice` at all; providers omit the key
    /// (spec 20 §6, 21 §11).
    #[default]
    Unspecified,
    /// The client asked for `auto` explicitly; providers write it.
    Auto,
    None,
    Required,
    /// Force one named tool.
    Named(String),
}

impl ToolChoice {
    pub fn name(&self) -> Option<&str> {
        match self {
            ToolChoice::Named(name) => Some(name),
            _ => None,
        }
    }
}
