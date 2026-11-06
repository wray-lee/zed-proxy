//! Tools and tool choice (spec 45 §1.1, §4.1, §4.2).

use serde_json::{json, Value};

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
    /// The declaration was `{"type":"custom",…}`. Claude/Gemini receive a
    /// `{input:string}` function instead, and their calls are restored to
    /// custom by tool name (spec 42 §1.5A, 45 §4.1.2).
    pub custom: bool,
}

/// Names of tools declared as Responses `custom` tools in this request (spec
/// 42 §1.5A). The set is request-scoped and travels with the provider encoder.
pub fn custom_tool_names(tools: &[ToolDef]) -> Vec<String> {
    tools
        .iter()
        .filter(|tool| tool.custom)
        .map(|tool| tool.name.clone())
        .collect()
}

/// Schema used when a custom tool is downgraded for Claude / Gemini (spec 42
/// §1.5A). Its `format` (grammar/lark/regex) has no upstream equivalent.
pub fn custom_tool_schema() -> Value {
    json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"]})
}

/// Restore a Claude/Gemini custom call from its `{"input":…}` argument object
/// (spec 42 §1.5A). A hit with an `input` key becomes `(true, free-form text)`;
/// every other shape keeps the compact JSON arguments and clears `custom`.
/// An ordinary function tool (name not declared custom) is never rewritten.
pub fn restore_custom_call(name: &str, args: &Value, names: &[String]) -> (bool, String) {
    let compact = if args.is_object() {
        args.to_string()
    } else {
        "{}".to_string()
    };
    if !names.iter().any(|declared| declared == name) {
        return (false, compact);
    }
    match args.get("input") {
        Some(Value::String(text)) => (true, text.clone()),
        Some(other) => (true, other.to_string()),
        None => (false, compact),
    }
}

/// Free-form input of a custom call: the neutral `arguments` wraps it as
/// `{"input":"…"}` when it came from a client request (spec 42 §1.2) and holds
/// the raw text when it came from the upstream; accept both.
pub fn custom_input_text(arguments: &str) -> String {
    let parsed: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    match parsed.get("input").and_then(Value::as_str) {
        Some(input) => input.to_string(),
        None => arguments.to_string(),
    }
}

/// Rewrap free-form input as the single-property object the degraded Claude /
/// Gemini declaration expects when replaying history (spec 42 §1.5A).
pub fn wrap_custom_input(arguments: &str) -> String {
    json!({ "input": custom_input_text(arguments) }).to_string()
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
