//! Message, system and content block types (spec 45 §1.1 `parts`).

use serde_json::{json, Map, Value};

/// Structured `tool_result` payload. `ToolResult` keeps its `output: String`
/// field because the frozen provider modules construct it with every field
/// listed; the ordered `text` / `image` / `json` blocks travel in an adjacent
/// `Opaque` block keyed by the same tool-call id (spec 22 §5.3.4).
const TOOL_RESULT_CONTENT: &str = "tool_result_content";

pub fn tool_result_content_block(tool_use_id: &str, blocks: Vec<Value>) -> ContentBlock {
    let mut item = Map::new();
    item.insert(
        TOOL_RESULT_CONTENT.to_string(),
        json!({"tool_use_id": tool_use_id, "blocks": blocks}),
    );
    ContentBlock::Opaque(Opaque {
        provider: "google".to_string(),
        item: Value::Object(item),
    })
}

/// Structured payload of an `Opaque` block made by
/// [`tool_result_content_block`]; other opaque shapes return `None`.
pub fn tool_result_content(item: &Value) -> Option<(&str, &[Value])> {
    let content = item.get(TOOL_RESULT_CONTENT)?;
    Some((
        content.get("tool_use_id")?.as_str()?,
        content.get("blocks")?.as_array()?,
    ))
}

pub fn tool_text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

pub fn tool_image_block(source: &ImageSource) -> Value {
    match source {
        ImageSource::Data { media_type, data } => {
            json!({"type": "image", "media_type": media_type, "data": data})
        }
        ImageSource::Url(url) => json!({"type": "image", "url": url}),
    }
}

pub fn tool_json_block(value: Value) -> Value {
    json!({"type": "json", "value": value})
}

/// Text-only fallback for providers that cannot express structured results
/// (spec 45 §4.7A): text blocks joined with `\n\n`, JSON blocks compacted,
/// images dropped.
pub fn tool_result_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => Some(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
            Some("json") => block.get("value").map(Value::to_string),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// First non-empty string among the dotted paths of a client's signature
/// carrier (spec 22 §5.3.3 priority table). Never fabricates a signature.
pub fn first_signature(object: &Map<String, Value>, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| {
        let (&first, rest) = path.split_first()?;
        let mut value = object.get(first)?;
        for key in rest {
            value = value.get(*key)?;
        }
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    })
}

/// `ToolStart` keeps its four fields because frozen provider modules construct
/// it with every field listed. Its `id` carries this reversible envelope when
/// a Gemini thought signature is present; client encoders split it back into
/// the protocol's call id and signature carrier.
const SIGNATURE_PREFIX: &str = "zed-sig-v1:";
const SIGNATURE_SUFFIX: &str = "\u{1f}end";

pub fn signed_tool_start_id(id: &str, signature: &str) -> String {
    // Byte-length prefix keeps delimiters and multibyte text in an opaque
    // signature unambiguous.
    format!(
        "{SIGNATURE_PREFIX}{}\u{1f}{signature}\u{1f}{id}{SIGNATURE_SUFFIX}",
        signature.len()
    )
}

pub fn split_signed_tool_start_id(id: &str) -> (&str, Option<&str>) {
    let Some(rest) = id
        .strip_prefix(SIGNATURE_PREFIX)
        .and_then(|value| value.strip_suffix(SIGNATURE_SUFFIX))
    else {
        return (id, None);
    };
    let Some((len, rest)) = rest.split_once('\u{1f}') else {
        return (id, None);
    };
    let Ok(len) = len.parse::<usize>() else {
        return (id, None);
    };
    let bytes = rest.as_bytes();
    let (Some(signature), Some(tail)) = (bytes.get(..len), bytes.get(len..)) else {
        return (id, None);
    };
    let (Ok(signature), Ok(tail)) = (std::str::from_utf8(signature), std::str::from_utf8(tail))
    else {
        return (id, None);
    };
    let Some(call_id) = tail.strip_prefix('\u{1f}') else {
        return (id, None);
    };
    (call_id, Some(signature))
}

/// Structured parts of a tool-result value (spec 40 §1.4A, 41 §1.3A,
/// 42 §1.2A). Three client wire shapes share one implementation; `json` is
/// the raw client value for objects/arrays that are not block arrays.
pub fn decode_tool_output(value: Option<&Value>) -> Vec<Value> {
    let Some(value) = value else {
        return vec![tool_text_block("")];
    };
    if let Some(text) = value.as_str() {
        return vec![tool_text_block(text)];
    }
    let Some(items) = value.as_array() else {
        return vec![tool_json_block(value.clone())];
    };
    // An array of bare JSON values (or objects without a `type`) is data, not
    // a client content-block array.
    let block_types = [
        "text",
        "input_text",
        "output_text",
        "json",
        "object",
        "image_url",
        "input_image",
        "image",
    ];
    if !items.iter().all(|item| {
        item.get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| block_types.contains(&kind))
    }) {
        return vec![tool_json_block(value.clone())];
    }
    let mut blocks = Vec::new();
    for object in items {
        match object.get("type").and_then(Value::as_str) {
            Some("text" | "input_text" | "output_text") => {
                if let Some(text) = object.get("text").and_then(Value::as_str) {
                    blocks.push(tool_text_block(text));
                }
            }
            Some("json" | "object") => {
                if let Some(value) = object.get("value") {
                    blocks.push(tool_json_block(value.clone()));
                }
            }
            Some("image_url") => {
                let url = object
                    .get("image_url")
                    .and_then(|image| image.get("url").or(Some(image)))
                    .and_then(Value::as_str);
                if let Some(source) = url.and_then(decode_tool_image_url) {
                    blocks.push(tool_image_block(&source));
                }
            }
            Some("input_image" | "image") => {
                let source = if object.get("type").and_then(Value::as_str) == Some("image") {
                    decode_anthropic_tool_image(object.get("source"))
                } else {
                    object
                        .get("image_url")
                        .and_then(Value::as_str)
                        .and_then(decode_tool_image_url)
                };
                if let Some(source) = source {
                    blocks.push(tool_image_block(&source));
                }
            }
            _ => {}
        }
    }
    blocks
}

fn decode_tool_image_url(url: &str) -> Option<ImageSource> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',')?;
        let media_type = meta.strip_suffix(";base64")?;
        if media_type.is_empty() || data.is_empty() {
            return None;
        }
        return Some(ImageSource::Data {
            media_type: media_type.to_string(),
            data: data.to_string(),
        });
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(ImageSource::Url(url.to_string()));
    }
    None
}

fn decode_anthropic_tool_image(source: Option<&Value>) -> Option<ImageSource> {
    match source?.get("type").and_then(Value::as_str) {
        Some("base64") => Some(ImageSource::Data {
            media_type: source?.get("media_type")?.as_str()?.to_string(),
            data: source?.get("data")?.as_str()?.to_string(),
        }),
        Some("url") => Some(ImageSource::Url(source?.get("url")?.as_str()?.to_string())),
        _ => None,
    }
}

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
