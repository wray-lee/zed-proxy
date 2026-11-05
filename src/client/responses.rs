//! OpenAI Responses codec (spec `42-client-openai-responses.md`).
//!
//! Request decoding is spec 42 §1, the non-streaming `response` object §2 and
//! the SSE event sequence §3. JSON is built by hand because the key order is
//! normative and `serde_json::Map` would re-sort it.
//!
//! `SseState` has no Responses-specific fields and the struct is shared/frozen,
//! so this codec reuses the general ones (documented once here):
//! - `sequence`: last `sequence_number` emitted (0 = nothing emitted yet).
//! - `next_block_index`: next `output_index` to assign (always contiguous).
//! - `open_block`: an `ITEM_*` while an output item is open, `END_SENT` /
//!   `TERMINAL` once the stream is finished.
//! - `text`: payload of the open message or function-call item.
//! - `thinking`: payload of the open reasoning item.
//! - `tool_count`: number of tool items started.
//! - `tool_arguments[index]`: the completed output item JSON at that
//!   `output_index`; while a tool item is open it holds its
//!   `{item_id,name,call_id}` metadata.
//! - `usage` / `stop_reason`: last seen values, used by `response.completed`.

use serde_json::{Map, Value};

use crate::catalog::Catalog;
use crate::error::GatewayError;
use crate::neutral::{
    ContentBlock, ImageSource, Message, NeutralRequest, NeutralResponse, Reasoning, Role,
    StopReason, StreamEvent, SystemBlock, ToolChoice, ToolDef, ToolResult, ToolUse, Usage,
};
use crate::proxy::stream::{sse_event, SseState};

use super::{tool_call_id, ClientDecoder, ClientEncoder};

/// Fallback response id when the upstream reports none (spec 42 §2.1).
pub const RESPONSE_ID: &str = "resp_zed";

const EMPTY_SCHEMA: &str = r#"{"type":"object","properties":{}}"#;
/// Free-text tools are represented as a function taking one `input` string
/// (spec 42 §1.5).
const CUSTOM_SCHEMA: &str =
    r#"{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}"#;

const ITEM_MESSAGE: u64 = 1;
const ITEM_REASONING: u64 = 2;
const ITEM_TOOL: u64 = 3;
const END_SENT: u64 = u64::MAX;
const TERMINAL: u64 = u64::MAX - 1;

/// Client keys the OpenAI provider writes itself (spec 20 §2.2);
/// `parallel_tool_calls` stays in the passthrough block for the native shape
/// (spec 20 §7).
const PASSTHROUGH_EXCLUDED: [&str; 8] = [
    "model",
    "stream",
    "reasoning",
    "reasoning_effort",
    "max_output_tokens",
    "max_completion_tokens",
    "max_tokens",
    "tools",
];

fn catalog() -> &'static Catalog {
    static CATALOG: std::sync::OnceLock<Catalog> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| Catalog::builtin().expect("built-in catalog parses"))
}

fn warn(what: &str) {
    eprintln!("[zed-proxy] responses: {what}");
}

fn jstr(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn positive_int(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_i64).filter(|value| *value > 0)
}

// ---------------------------------------------------------------------------
// Request decoding (spec 42 §1)
// ---------------------------------------------------------------------------

fn decode_image_url(url: &str) -> Option<ImageSource> {
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

/// One content part of a message item (spec 42 §1.3).
fn decode_part(item: &Value) -> Option<ContentBlock> {
    let object = item.as_object()?;
    match object.get("type").and_then(Value::as_str) {
        Some("input_text") | Some("output_text") => {
            let text = object.get("text").and_then(Value::as_str)?;
            if text.is_empty() {
                None
            } else {
                Some(ContentBlock::Text(text.to_string()))
            }
        }
        Some("refusal") => {
            let text = object.get("refusal").and_then(Value::as_str)?;
            if text.is_empty() {
                None
            } else {
                Some(ContentBlock::Text(text.to_string()))
            }
        }
        Some("input_image") => {
            if let Some(url) = object.get("image_url").and_then(Value::as_str) {
                match decode_image_url(url) {
                    Some(source) => return Some(ContentBlock::Image(source)),
                    None => warn("dropped input_image with an unsupported url"),
                }
            } else if object.get("file_id").is_some() {
                warn("dropped input_image with file_id (not resolvable)");
            } else {
                warn("dropped input_image without image_url");
            }
            None
        }
        Some("input_file") | Some("input_audio") => {
            warn("dropped unsupported content part");
            None
        }
        other => {
            warn(&format!(
                "dropped content part {:?}",
                other.unwrap_or("without type")
            ));
            None
        }
    }
}

fn decode_content(value: Option<&Value>) -> Vec<ContentBlock> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::Text(text.clone())]
            }
        }
        Some(Value::Array(items)) => items.iter().filter_map(decode_part).collect(),
        Some(_) => {
            warn("ignored non-string, non-array content");
            Vec::new()
        }
    }
}

/// Text of a system/developer message content, joined with `\n\n`.
fn system_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                match object.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("output_text") | Some("text") => {
                        object.get("text").and_then(Value::as_str)
                    }
                    Some("refusal") => object.get("refusal").and_then(Value::as_str),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// Tool result text: a string, or the text blocks of an array (spec 42 §1.2).
fn tool_output_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                match object.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("output_text") | Some("text") => {
                        object.get("text").and_then(Value::as_str)
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        Some(_) => {
            warn("tool output is not a string or array; using an empty string");
            String::new()
        }
        None => String::new(),
    }
}

/// `arguments` text for a function call (spec 42 §1.2): non-object JSON becomes
/// `{}`, and the original text is logged for diagnosis.
fn decode_arguments(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(_)) => text.clone(),
            _ => {
                // The spec asks for the raw text; truncate so a malformed call
                // cannot flood the log.
                let preview: String = text.chars().take(200).collect();
                warn(&format!(
                    "function call arguments are not a JSON object; using {{}} (raw: {preview})"
                ));
                "{}".to_string()
            }
        },
        Some(Value::Object(_)) => serde_json::to_string(value.unwrap_or(&Value::Null))
            .unwrap_or_else(|_| "{}".to_string()),
        None => "{}".to_string(),
        Some(_) => {
            warn("function call arguments are neither string nor object; using {}");
            "{}".to_string()
        }
    }
}

/// A `function_call` / `custom_tool_call` input item (spec 42 §1.2).
fn decode_call(object: &Map<String, Value>, custom: bool) -> Option<ToolUse> {
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name.is_empty() {
        warn("dropped tool call without name");
        return None;
    }
    let id = tool_call_id(object.get("call_id").and_then(Value::as_str));
    let arguments = if custom {
        format!(
            "{{\"input\":{}}}",
            jstr(
                object
                    .get("input")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            )
        )
    } else {
        decode_arguments(object.get("arguments"))
    };
    Some(ToolUse {
        id,
        name: name.to_string(),
        arguments,
        signature: None,
        // Keep the free-form shape on the way back to the upstream.
        custom,
    })
}

/// A `function_call_output` / `custom_tool_call_output` input item.
fn decode_result(object: &Map<String, Value>, custom: bool) -> Option<ToolResult> {
    let id = object
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if id.is_empty() {
        warn("dropped tool output without call_id");
        return None;
    }
    Some(ToolResult {
        tool_use_id: id.to_string(),
        output: tool_output_text(object.get("output")),
        is_error: false,
        custom,
    })
}

fn flush_pending(pending: &mut Vec<ToolResult>, messages: &mut Vec<Message>) {
    if pending.is_empty() {
        return;
    }
    let parts = pending.drain(..).map(ContentBlock::ToolResult).collect();
    messages.push(Message {
        role: Role::User,
        parts,
    });
}

fn push_tool_use(messages: &mut Vec<Message>, call: ToolUse) {
    match messages.last_mut() {
        Some(last) if last.role == Role::Assistant => last.parts.push(ContentBlock::ToolUse(call)),
        _ => messages.push(Message {
            role: Role::Assistant,
            parts: vec![ContentBlock::ToolUse(call)],
        }),
    }
}

/// One `message` item, or the official shorthand that omits `type`.
fn decode_message_item(
    object: &Map<String, Value>,
    system: &mut Vec<SystemBlock>,
    messages: &mut Vec<Message>,
    pending: &mut Vec<ToolResult>,
) {
    match object.get("role").and_then(Value::as_str) {
        Some("user") => {
            // Results keep their own turn: pending outputs precede the text,
            // which preserves the client's item order on the way upstream
            // (spec 20 §4.5) and matches how Chat `role=tool` is encoded.
            flush_pending(pending, messages);
            messages.push(Message {
                role: Role::User,
                parts: decode_content(object.get("content")),
            });
        }
        Some("assistant") => {
            flush_pending(pending, messages);
            messages.push(Message {
                role: Role::Assistant,
                parts: decode_content(object.get("content")),
            });
        }
        Some("system") | Some("developer") => {
            flush_pending(pending, messages);
            let text = system_text(object.get("content"));
            if !text.is_empty() {
                system.push(SystemBlock {
                    text,
                    cache_control: None,
                });
            }
        }
        Some(other) => {
            flush_pending(pending, messages);
            warn(&format!("dropped message with role {other:?}"));
        }
        // A message item without a role is dropped, not forwarded (spec 42 §1.2).
        None => warn("dropped message item without role"),
    }
}

fn decode_tools(value: Option<&Value>) -> Vec<ToolDef> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut tools = Vec::new();
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match object.get("type").and_then(Value::as_str) {
            Some("function") => {
                if name.is_empty() {
                    warn("dropped tool declaration without name");
                    continue;
                }
                let parameters = object
                    .get("parameters")
                    .filter(|schema| schema.is_object())
                    .cloned()
                    .unwrap_or_else(|| serde_json::from_str(EMPTY_SCHEMA).expect("literal schema"));
                tools.push(ToolDef {
                    name: name.to_string(),
                    description: object
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    parameters,
                    strict: object.get("strict").and_then(Value::as_bool),
                    cache_control: None,
                    format: None,
                });
            }
            Some("custom") => {
                if name.is_empty() {
                    warn("dropped custom tool declaration without name");
                    continue;
                }
                tools.push(ToolDef {
                    name: name.to_string(),
                    description: object
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    parameters: serde_json::from_str(CUSTOM_SCHEMA).expect("literal schema"),
                    strict: None,
                    cache_control: None,
                    format: object.get("format").cloned(),
                });
            }
            Some("namespace") => warn("dropped namespace tool (not mapped in this round)"),
            Some(other) => warn(&format!("dropped tool declaration {other:?}")),
            None => warn("dropped tool declaration without type"),
        }
    }
    tools
}

fn decode_tool_choice(value: Option<&Value>, tools: &[ToolDef]) -> ToolChoice {
    let mut choice = match value {
        None => ToolChoice::Unspecified,
        Some(Value::String(mode)) => match mode.as_str() {
            "auto" => ToolChoice::Auto,
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => {
                warn("unknown tool_choice string; using auto");
                ToolChoice::Auto
            }
        },
        Some(Value::Object(object)) => {
            let named = match object.get("type").and_then(Value::as_str) {
                Some("function") | Some("custom") => {
                    object.get("name").and_then(Value::as_str).or_else(|| {
                        object
                            .get("function")
                            .and_then(|function| function.get("name"))
                            .and_then(Value::as_str)
                    })
                }
                _ => {
                    warn("unknown tool_choice object; using auto");
                    None
                }
            };
            match named {
                Some(name) if !name.is_empty() => ToolChoice::Named(name.to_string()),
                _ => ToolChoice::Auto,
            }
        }
        Some(_) => {
            warn("unknown tool_choice shape; using auto");
            ToolChoice::Auto
        }
    };
    if matches!(choice, ToolChoice::Named(_)) && tools.is_empty() {
        warn("named tool_choice without tools; using auto");
        choice = ToolChoice::Auto;
    }
    choice
}

fn decode_reasoning(body: &Value) -> Reasoning {
    let Some(object) = body.get("reasoning").and_then(Value::as_object) else {
        if body.get("reasoning").is_some() {
            warn("ignored non-object reasoning");
        }
        return Reasoning::default();
    };
    let effort = object
        .get("effort")
        .and_then(Value::as_str)
        .map(str::to_lowercase);
    let rest: Map<String, Value> = object
        .iter()
        .filter(|(key, _)| key.as_str() != "effort")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Reasoning {
        effort,
        extra: if rest.is_empty() {
            None
        } else {
            Some(Value::Object(rest))
        },
    }
}

pub struct ResponsesDecoder;

impl ClientDecoder for ResponsesDecoder {
    fn decode(&self, body: &serde_json::Value) -> Result<NeutralRequest, GatewayError> {
        let catalog = catalog();
        let model = catalog.normalize_model(body.get("model").and_then(Value::as_str));
        let provider = catalog.resolve_provider(&model)?;

        let mut system: Vec<SystemBlock> = Vec::new();
        // Responses carries the prompt as `instructions`, a single string; the
        // Anthropic provider may keep the string shorthand (spec 21 §4).
        let mut system_string = false;
        if let Some(text) = body.get("instructions").and_then(Value::as_str) {
            if !text.is_empty() {
                system.push(SystemBlock {
                    text: text.to_string(),
                    cache_control: None,
                });
                system_string = true;
            }
        }

        let mut messages: Vec<Message> = Vec::new();
        let mut pending_results: Vec<ToolResult> = Vec::new();
        let mut additional: Vec<ToolDef> = Vec::new();

        match body.get("input") {
            None | Some(Value::Null) => {}
            Some(Value::String(text)) => {
                let parts = if text.is_empty() {
                    Vec::new()
                } else {
                    vec![ContentBlock::Text(text.clone())]
                };
                messages.push(Message {
                    role: Role::User,
                    parts,
                });
            }
            Some(Value::Array(items)) => {
                let mut reasoning_dropped = false;
                for item in items {
                    let Some(object) = item.as_object() else {
                        warn("dropped non-object input item");
                        continue;
                    };
                    let kind = object.get("type").and_then(Value::as_str);
                    if kind.is_none() {
                        if object.get("role").is_some() {
                            decode_message_item(
                                object,
                                &mut system,
                                &mut messages,
                                &mut pending_results,
                            );
                        } else {
                            warn("dropped input item without type or role");
                        }
                        continue;
                    }
                    match kind {
                        Some("message") => decode_message_item(
                            object,
                            &mut system,
                            &mut messages,
                            &mut pending_results,
                        ),
                        Some("function_call") | Some("custom_tool_call") => {
                            flush_pending(&mut pending_results, &mut messages);
                            let custom = kind == Some("custom_tool_call");
                            if let Some(call) = decode_call(object, custom) {
                                push_tool_use(&mut messages, call);
                            }
                        }
                        Some("function_call_output") | Some("custom_tool_call_output") => {
                            let custom = kind == Some("custom_tool_call_output");
                            if let Some(result) = decode_result(object, custom) {
                                pending_results.push(result);
                            }
                        }
                        Some("reasoning") => {
                            if !reasoning_dropped {
                                warn("reasoning item dropped (not replayable)");
                                reasoning_dropped = true;
                            }
                        }
                        Some("additional_tools") => {
                            additional.extend(decode_tools(object.get("tools")));
                        }
                        other => warn(&format!(
                            "dropped input item {:?}",
                            other.unwrap_or("without type")
                        )),
                    }
                }
            }
            Some(_) => warn("ignored non-string, non-array input"),
        }
        flush_pending(&mut pending_results, &mut messages);

        let mut tools = decode_tools(body.get("tools"));
        for tool in additional {
            if tools.iter().any(|existing| existing.name == tool.name) {
                warn(&format!(
                    "additional tool {:?} duplicates a top-level tool; keeping the top-level one",
                    tool.name
                ));
            } else {
                tools.push(tool);
            }
        }
        let tool_choice = decode_tool_choice(body.get("tool_choice"), &tools);

        let tool_uses: Vec<ToolUse> = messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                ContentBlock::ToolUse(call) => Some(call.clone()),
                _ => None,
            })
            .collect();
        let unpaired_tool_results: Vec<ToolResult> = messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                ContentBlock::ToolResult(result)
                    if !tool_uses.iter().any(|call| call.id == result.tool_use_id) =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .collect();

        // `passthrough` keeps the client's top-level keys for the OpenAI
        // provider's native shape (spec 20 §1, §2.2).
        // shortcut: keys follow `serde_json::Map` order (alphabetical without
        // the `preserve_order` feature), not the client's key order.
        let mut passthrough: Vec<(String, Value)> = Vec::new();
        if let Some(object) = body.as_object() {
            for (key, value) in object {
                if PASSTHROUGH_EXCLUDED.contains(&key.as_str()) {
                    continue;
                }
                passthrough.push((key.clone(), value.clone()));
            }
        }
        for key in [
            "store",
            "previous_response_id",
            "conversation",
            "include",
            "metadata",
            "prompt",
        ] {
            if body.get(key).is_some() {
                warn(&format!("ignored {key} (the gateway keeps no state)"));
            }
        }

        let system_string = system_string && system.len() == 1;
        Ok(NeutralRequest {
            model,
            provider,
            system,
            system_string,
            messages,
            tools,
            tool_choice,
            parallel_tool_calls: Some(
                body.get("parallel_tool_calls")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            ),
            reasoning: decode_reasoning(body),
            max_output_tokens: positive_int(body.get("max_output_tokens")),
            temperature: body.get("temperature").and_then(Value::as_f64),
            top_p: body.get("top_p").and_then(Value::as_f64),
            stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
            passthrough,
            tool_uses,
            unpaired_tool_results,
            ..NeutralRequest::default()
        })
    }
}

// ---------------------------------------------------------------------------
// Non-streaming response (spec 42 §2)
// ---------------------------------------------------------------------------

fn usage_json(usage: Usage) -> String {
    let usage = usage.with_computed_total();
    format!(
        "{{\"input_tokens\":{},\"input_tokens_details\":{{\"cached_tokens\":{}}},\"output_tokens\":{},\"output_tokens_details\":{{\"reasoning_tokens\":{}}},\"total_tokens\":{}}}",
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.output_tokens,
        usage.reasoning_tokens,
        usage.total_tokens
    )
}

fn reasoning_item(index: u64, text: &str) -> String {
    format!(
        "{{\"id\":\"rs_{index}\",\"type\":\"reasoning\",\"status\":\"completed\",\"summary\":[{{\"type\":\"summary_text\",\"text\":{}}}],\"content\":[]}}",
        jstr(text)
    )
}

fn message_item(index: u64, text: &str) -> String {
    format!(
        "{{\"id\":\"msg_{index}\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":{},\"annotations\":[]}}]}}",
        jstr(text)
    )
}

fn function_call_item(item_id: &str, call_id: &str, name: &str, arguments: &str) -> String {
    format!(
        "{{\"id\":{},\"type\":\"function_call\",\"status\":\"completed\",\"call_id\":{},\"name\":{},\"arguments\":{}}}",
        jstr(item_id),
        jstr(call_id),
        jstr(name),
        jstr(arguments)
    )
}

fn custom_tool_call_item(item_id: &str, call_id: &str, name: &str, input: &str) -> String {
    format!(
        "{{\"id\":{},\"type\":\"custom_tool_call\",\"status\":\"completed\",\"call_id\":{},\"name\":{},\"input\":{}}}",
        jstr(item_id),
        jstr(call_id),
        jstr(name),
        jstr(input)
    )
}

/// Free-form input of a custom call: the neutral `arguments` wraps it as
/// `{"input":"…"}` when it came from a client request (spec 42 §1.2) and holds
/// the raw text when it came from the upstream; accept both.
fn custom_input(arguments: &str) -> String {
    let parsed: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    match parsed.get("input").and_then(Value::as_str) {
        Some(input) => input.to_string(),
        None => arguments.to_string(),
    }
}

/// `output[]` in production order (spec 42 §2.2); empty when nothing was
/// produced.
fn output_items(response: &NeutralResponse) -> Vec<String> {
    let mut items = Vec::new();
    if !response.thinking.is_empty() {
        items.push(reasoning_item(0, &response.thinking));
    }
    if !response.text.is_empty() {
        let index = items.len() as u64;
        items.push(message_item(index, &response.text));
    }
    for call in &response.tool_calls {
        if call.custom {
            items.push(custom_tool_call_item(
                &format!("ctc_{}", call.id),
                &call.id,
                &call.name,
                &custom_input(&call.arguments),
            ));
        } else {
            items.push(function_call_item(
                &format!("fc_{}", call.id),
                &call.id,
                &call.name,
                &call.arguments,
            ));
        }
    }
    items
}

pub struct ResponsesEncoder;

impl ClientEncoder for ResponsesEncoder {
    fn encode_response(
        &self,
        response: &NeutralResponse,
        model: &str,
        now_unix: i64,
    ) -> Result<Vec<u8>, GatewayError> {
        let id = response
            .id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| RESPONSE_ID.to_string());
        Ok(format!(
            "{{\"id\":{},\"object\":\"response\",\"created_at\":{now_unix},\"status\":\"completed\",\"model\":{},\"output\":[{}],\"usage\":{},\"error\":null}}",
            jstr(&id),
            jstr(model),
            output_items(response).join(","),
            usage_json(response.usage)
        )
        .into_bytes())
    }

    fn encode_event(
        &self,
        event: &StreamEvent,
        state: &mut SseState,
    ) -> Result<Option<Vec<u8>>, GatewayError> {
        let mut out = Vec::new();
        match event {
            StreamEvent::Start { id } => {
                if state.response_id.is_none() {
                    state.response_id = id.clone().filter(|id| !id.is_empty());
                }
                ensure_started(state, &mut out);
            }
            StreamEvent::BlockStart { .. } | StreamEvent::BlockStop { .. } => {}
            StreamEvent::TextDelta { delta } => {
                ensure_started(state, &mut out);
                if state.open_block != Some(ITEM_MESSAGE) {
                    open_message(state, &mut out);
                }
                state.text.push_str(delta);
                let index = state.next_block_index.saturating_sub(1);
                let item_id = format!("msg_{index}");
                out.extend(event_frame(
                    state,
                    "response.output_text.delta",
                    format!(
                        "{{\"type\":\"response.output_text.delta\",\"item_id\":{},\"output_index\":{index},\"content_index\":0,\"delta\":{}}}",
                        jstr(&item_id),
                        jstr(delta)
                    ),
                ));
            }
            StreamEvent::ThinkingDelta { delta } => {
                ensure_started(state, &mut out);
                if state.open_block != Some(ITEM_REASONING) {
                    open_reasoning(state, &mut out);
                }
                state.thinking.push_str(delta);
                let index = state.next_block_index.saturating_sub(1);
                let item_id = format!("rs_{index}");
                out.extend(event_frame(
                    state,
                    "response.reasoning_summary_text.delta",
                    format!(
                        "{{\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":{},\"output_index\":{index},\"summary_index\":0,\"delta\":{}}}",
                        jstr(&item_id),
                        jstr(delta)
                    ),
                ));
            }
            StreamEvent::ToolStart {
                id, name, custom, ..
            } => {
                ensure_started(state, &mut out);
                open_tool(state, &mut out, id, name, *custom);
                state.tool_count += 1;
            }
            StreamEvent::ToolArgsDelta { delta, .. } => {
                if state.open_block == Some(ITEM_TOOL) {
                    state.text.push_str(delta);
                    let index = state.next_block_index.saturating_sub(1);
                    let item_id = tool_item_id(state, index);
                    let event = if tool_is_custom(state, index) {
                        "response.custom_tool_call_input.delta"
                    } else {
                        "response.function_call_arguments.delta"
                    };
                    out.extend(event_frame(
                        state,
                        event,
                        format!(
                            "{{\"type\":\"{event}\",\"item_id\":{},\"output_index\":{index},\"delta\":{}}}",
                            jstr(&item_id),
                            jstr(delta)
                        ),
                    ));
                }
            }
            StreamEvent::ToolEnd { .. } => {
                if state.open_block == Some(ITEM_TOOL) {
                    close_tool(state, &mut out);
                    state.open_block = None;
                }
            }
            StreamEvent::Usage(usage) => state.usage = *usage,
            StreamEvent::End { reason } => {
                state.stop_reason = *reason;
                ensure_started(state, &mut out);
                close_item(state, &mut out);
                emit_completed(state, &mut out);
            }
            StreamEvent::Error(err) => {
                ensure_started(state, &mut out);
                out.extend(failed_frame(state, err));
                state.open_block = Some(TERMINAL);
            }
        }
        Ok(if out.is_empty() { None } else { Some(out) })
    }

    fn finish(&self, state: &mut SseState) -> Result<Vec<u8>, GatewayError> {
        if matches!(state.open_block, Some(END_SENT) | Some(TERMINAL)) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        ensure_started(state, &mut out);
        close_item(state, &mut out);
        emit_completed(state, &mut out);
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// SSE helpers (spec 42 §3)
// ---------------------------------------------------------------------------

fn response_id(state: &SseState) -> String {
    state
        .response_id
        .clone()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| RESPONSE_ID.to_string())
}

/// One event frame with `sequence_number` appended as the last key.
fn event_frame(state: &mut SseState, event: &str, mut data: String) -> Vec<u8> {
    // Drop the closing brace so `sequence_number` can be the last key.
    if data.ends_with('}') {
        data.pop();
    }
    state.sequence += 1;
    data.push_str(&format!(",\"sequence_number\":{}}}", state.sequence));
    sse_event(event, data.as_bytes())
}

/// `response.created` + `response.in_progress` precede every other event.
fn ensure_started(state: &mut SseState, out: &mut Vec<u8>) {
    if state.sequence > 0 {
        return;
    }
    let snapshot = format!(
        "{{\"id\":{},\"object\":\"response\",\"created_at\":{},\"status\":\"in_progress\",\"model\":{},\"output\":[]}}",
        jstr(&response_id(state)),
        state.created_unix,
        jstr(&state.model)
    );
    out.extend(event_frame(
        state,
        "response.created",
        format!("{{\"type\":\"response.created\",\"response\":{snapshot}}}"),
    ));
    out.extend(event_frame(
        state,
        "response.in_progress",
        format!("{{\"type\":\"response.in_progress\",\"response\":{snapshot}}}"),
    ));
}

fn open_message(state: &mut SseState, out: &mut Vec<u8>) {
    close_item(state, out);
    let index = state.next_block_index;
    state.next_block_index += 1;
    state.open_block = Some(ITEM_MESSAGE);
    state.text.clear();
    state.tool_arguments.push(String::new());
    let item_id = format!("msg_{index}");
    out.extend(event_frame(
        state,
        "response.output_item.added",
        format!(
            "{{\"type\":\"response.output_item.added\",\"output_index\":{index},\"item\":{{\"id\":{},\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}}}",
            jstr(&item_id)
        ),
    ));
    out.extend(event_frame(
        state,
        "response.content_part.added",
        format!(
            "{{\"type\":\"response.content_part.added\",\"item_id\":{},\"output_index\":{index},\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"text\":\"\",\"annotations\":[]}}}}",
            jstr(&item_id)
        ),
    ));
}

fn close_message(state: &mut SseState, out: &mut Vec<u8>) {
    let index = state.next_block_index.saturating_sub(1);
    let item_id = format!("msg_{index}");
    let text = std::mem::take(&mut state.text);
    out.extend(event_frame(
        state,
        "response.output_text.done",
        format!(
            "{{\"type\":\"response.output_text.done\",\"item_id\":{},\"output_index\":{index},\"content_index\":0,\"text\":{}}}",
            jstr(&item_id),
            jstr(&text)
        ),
    ));
    out.extend(event_frame(
        state,
        "response.content_part.done",
        format!(
            "{{\"type\":\"response.content_part.done\",\"item_id\":{},\"output_index\":{index},\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"text\":{},\"annotations\":[]}}}}",
            jstr(&item_id),
            jstr(&text)
        ),
    ));
    let item = message_item(index, &text);
    out.extend(event_frame(
        state,
        "response.output_item.done",
        format!(
            "{{\"type\":\"response.output_item.done\",\"output_index\":{index},\"item\":{item}}}"
        ),
    ));
    state.tool_arguments[index as usize] = item;
}

fn open_reasoning(state: &mut SseState, out: &mut Vec<u8>) {
    close_item(state, out);
    let index = state.next_block_index;
    state.next_block_index += 1;
    state.open_block = Some(ITEM_REASONING);
    state.thinking.clear();
    state.tool_arguments.push(String::new());
    let item_id = format!("rs_{index}");
    out.extend(event_frame(
        state,
        "response.output_item.added",
        format!(
            "{{\"type\":\"response.output_item.added\",\"output_index\":{index},\"item\":{{\"id\":{},\"type\":\"reasoning\",\"status\":\"in_progress\",\"summary\":[]}}}}",
            jstr(&item_id)
        ),
    ));
    out.extend(event_frame(
        state,
        "response.reasoning_summary_part.added",
        format!(
            "{{\"type\":\"response.reasoning_summary_part.added\",\"item_id\":{},\"output_index\":{index},\"summary_index\":0,\"part\":{{\"type\":\"summary_text\",\"text\":\"\"}}}}",
            jstr(&item_id)
        ),
    ));
}

fn close_reasoning(state: &mut SseState, out: &mut Vec<u8>) {
    let index = state.next_block_index.saturating_sub(1);
    let item_id = format!("rs_{index}");
    let text = std::mem::take(&mut state.thinking);
    out.extend(event_frame(
        state,
        "response.reasoning_summary_text.done",
        format!(
            "{{\"type\":\"response.reasoning_summary_text.done\",\"item_id\":{},\"output_index\":{index},\"summary_index\":0,\"text\":{}}}",
            jstr(&item_id),
            jstr(&text)
        ),
    ));
    out.extend(event_frame(
        state,
        "response.reasoning_summary_part.done",
        format!(
            "{{\"type\":\"response.reasoning_summary_part.done\",\"item_id\":{},\"output_index\":{index},\"summary_index\":0,\"part\":{{\"type\":\"summary_text\",\"text\":{}}}}}",
            jstr(&item_id),
            jstr(&text)
        ),
    ));
    let item = reasoning_item(index, &text);
    out.extend(event_frame(
        state,
        "response.output_item.done",
        format!(
            "{{\"type\":\"response.output_item.done\",\"output_index\":{index},\"item\":{item}}}"
        ),
    ));
    state.tool_arguments[index as usize] = item;
}

/// The tool item id for the open/closed tool at `index` (`fc_<call id>`).
/// While the tool is open, `tool_arguments[index]` still holds its metadata.
fn tool_item_id(state: &SseState, index: u64) -> String {
    let Some(meta) = state.tool_arguments.get(index as usize) else {
        return String::new();
    };
    let parsed: Value = serde_json::from_str(meta).unwrap_or(Value::Null);
    parsed
        .get("item_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// True when the open tool at `index` is a custom (free-form) call; the
/// upstream marks it in the metadata `open_tool` stored there (spec 42 §3.2).
fn tool_is_custom(state: &SseState, index: u64) -> bool {
    state
        .tool_arguments
        .get(index as usize)
        .and_then(|meta| serde_json::from_str::<Value>(meta).ok())
        .and_then(|parsed| parsed.get("custom").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn open_tool(state: &mut SseState, out: &mut Vec<u8>, id: &str, name: &str, custom: bool) {
    close_item(state, out);
    let index = state.next_block_index;
    state.next_block_index += 1;
    state.open_block = Some(ITEM_TOOL);
    state.text.clear();
    let call_id = tool_call_id(if id.is_empty() { None } else { Some(id) });
    let prefix = if custom { "ctc_" } else { "fc_" };
    let item_id = format!("{prefix}{call_id}");
    let meta = format!(
        "{{\"item_id\":{},\"name\":{},\"call_id\":{},\"custom\":{custom}}}",
        jstr(&item_id),
        jstr(name),
        jstr(&call_id)
    );
    state.tool_arguments.push(meta);
    let item = if custom {
        format!(
            "{{\"id\":{},\"type\":\"custom_tool_call\",\"status\":\"in_progress\",\"call_id\":{},\"name\":{},\"input\":\"\"}}",
            jstr(&item_id),
            jstr(&call_id),
            jstr(name)
        )
    } else {
        format!(
            "{{\"id\":{},\"type\":\"function_call\",\"status\":\"in_progress\",\"call_id\":{},\"name\":{},\"arguments\":\"\"}}",
            jstr(&item_id),
            jstr(&call_id),
            jstr(name)
        )
    };
    out.extend(event_frame(
        state,
        "response.output_item.added",
        format!(
            "{{\"type\":\"response.output_item.added\",\"output_index\":{index},\"item\":{item}}}"
        ),
    ));
}

fn close_tool(state: &mut SseState, out: &mut Vec<u8>) {
    let index = state.next_block_index.saturating_sub(1);
    let meta = match state.tool_arguments.get_mut(index as usize) {
        Some(slot) => std::mem::take(slot),
        None => String::new(),
    };
    let parsed: Value = serde_json::from_str(&meta).unwrap_or(Value::Null);
    let item_id = parsed
        .get("item_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let call_id = parsed
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let name = parsed
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let custom = parsed
        .get("custom")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut arguments = std::mem::take(&mut state.text);
    if arguments.is_empty() && !custom {
        // A tool called without arguments must still carry parseable JSON
        // (spec 45 §4.4).
        arguments = "{}".to_string();
        out.extend(event_frame(
            state,
            "response.function_call_arguments.delta",
            format!(
                "{{\"type\":\"response.function_call_arguments.delta\",\"item_id\":{},\"output_index\":{index},\"delta\":\"{{}}\"}}",
                jstr(&item_id)
            ),
        ));
    }
    if custom {
        // `state.text` already carries the upstream's free-form input verbatim.
        let done = "response.custom_tool_call_input.done";
        out.extend(event_frame(
            state,
            done,
            format!(
                "{{\"type\":\"{done}\",\"item_id\":{},\"output_index\":{index},\"input\":{}}}",
                jstr(&item_id),
                jstr(&arguments)
            ),
        ));
        let item = custom_tool_call_item(&item_id, &call_id, &name, &arguments);
        out.extend(event_frame(
            state,
            "response.output_item.done",
            format!(
                "{{\"type\":\"response.output_item.done\",\"output_index\":{index},\"item\":{item}}}"
            ),
        ));
        state.tool_arguments[index as usize] = item;
        return;
    }
    out.extend(event_frame(
        state,
        "response.function_call_arguments.done",
        format!(
            "{{\"type\":\"response.function_call_arguments.done\",\"item_id\":{},\"output_index\":{index},\"arguments\":{}}}",
            jstr(&item_id),
            jstr(&arguments)
        ),
    ));
    let item = function_call_item(&item_id, &call_id, &name, &arguments);
    out.extend(event_frame(
        state,
        "response.output_item.done",
        format!(
            "{{\"type\":\"response.output_item.done\",\"output_index\":{index},\"item\":{item}}}"
        ),
    ));
    state.tool_arguments[index as usize] = item;
}

fn close_item(state: &mut SseState, out: &mut Vec<u8>) {
    match state.open_block {
        Some(ITEM_MESSAGE) => {
            close_message(state, out);
            state.open_block = None;
        }
        Some(ITEM_REASONING) => {
            close_reasoning(state, out);
            state.open_block = None;
        }
        Some(ITEM_TOOL) => {
            close_tool(state, out);
            state.open_block = None;
        }
        _ => {}
    }
}

/// `response.completed`, or `response.incomplete` when the output budget ran
/// out (spec 42 §3.2 item 6).
fn emit_completed(state: &mut SseState, out: &mut Vec<u8>) {
    let output = format!("[{}]", state.tool_arguments.join(","));
    let (event, status, details) = if state.stop_reason == StopReason::Length {
        (
            "response.incomplete",
            "incomplete",
            ",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}",
        )
    } else {
        ("response.completed", "completed", "")
    };
    out.extend(event_frame(
        state,
        event,
        format!(
            "{{\"type\":\"{event}\",\"response\":{{\"id\":{},\"object\":\"response\",\"created_at\":{},\"status\":\"{status}\"{details},\"model\":{},\"output\":{output},\"usage\":{},\"error\":null}}}}",
            jstr(&response_id(state)),
            state.created_unix,
            jstr(&state.model),
            usage_json(state.usage)
        ),
    ));
    state.open_block = Some(END_SENT);
}

/// Key order and keys are fixed by spec 42 §3.5, which is the sole authority
/// for this shape (it carries no `sequence_number`).
fn failed_frame(state: &SseState, err: &GatewayError) -> Vec<u8> {
    let code = err.code.unwrap_or("upstream_error");
    let message = jstr(&err.message);
    let data = format!(
        "{{\"type\":\"response.failed\",\"response\":{{\"id\":{},\"object\":\"response\",\"created_at\":{},\"status\":\"failed\",\"model\":{},\"output\":[],\"error\":{{\"code\":\"{code}\",\"message\":{message}}},\"status_details\":{{\"error\":{{\"message\":{message}}}}}}}}}",
        jstr(&response_id(state)),
        state.created_unix,
        jstr(&state.model)
    );
    sse_event("response.failed", data.as_bytes())
}

// ---------------------------------------------------------------------------
// Tests (migrated behaviours from spec 42 §5)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Provider;

    fn decode(body: Value) -> NeutralRequest {
        ResponsesDecoder.decode(&body).expect("request decodes")
    }

    fn frames(bytes: &[u8]) -> Vec<(String, String)> {
        let text = String::from_utf8(bytes.to_vec()).expect("SSE is UTF-8");
        text.split("\n\n")
            .filter(|frame| !frame.is_empty())
            .map(|frame| {
                let (event, data) = frame.split_once('\n').expect("event and data lines");
                (
                    event
                        .strip_prefix("event: ")
                        .expect("event prefix")
                        .to_string(),
                    data.strip_prefix("data: ")
                        .expect("data prefix")
                        .to_string(),
                )
            })
            .collect()
    }

    fn run(events: &[StreamEvent], state: &mut SseState) -> Vec<(String, String)> {
        let encoder = ResponsesEncoder;
        let mut bytes = Vec::new();
        for event in events {
            if let Some(frame) = encoder.encode_event(event, state).expect("event encodes") {
                bytes.extend(frame);
            }
        }
        bytes.extend(encoder.finish(state).expect("finish encodes"));
        frames(&bytes)
    }

    #[test]
    fn string_input_becomes_one_user_message() {
        let request = decode(serde_json::json!({"model":"gpt-5.6-sol","input":"ping"}));
        assert_eq!(request.model, "gpt-5.6-sol");
        assert_eq!(request.provider, Provider::OpenAi);
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].role, Role::User);
        assert_eq!(
            request.messages[0].parts,
            vec![ContentBlock::Text("ping".to_string())]
        );
        assert!(request.system.is_empty());
    }

    #[test]
    fn shorthand_message_without_type_is_normalized() {
        let request = decode(serde_json::json!({
            "input": [{"role": "user", "content": "ping"}]
        }));
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].role, Role::User);
        assert_eq!(
            request.messages[0].parts,
            vec![ContentBlock::Text("ping".to_string())]
        );
    }

    #[test]
    fn instructions_lead_the_system_blocks_and_developer_joins_them() {
        let request = decode(serde_json::json!({
            "input": [
                {"type": "message", "role": "developer", "content": "dev rules"},
                {"type": "message", "role": "user", "content": "hi"}
            ],
            "instructions": "base rules"
        }));
        let texts: Vec<&str> = request
            .system
            .iter()
            .map(|block| block.text.as_str())
            .collect();
        assert_eq!(texts, vec!["base rules", "dev rules"]);
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].role, Role::User);
    }

    #[test]
    fn message_items_require_a_role() {
        let request = decode(serde_json::json!({
            "input": [{"type": "message", "content": "orphan"}]
        }));
        assert!(request.messages.is_empty());
    }

    #[test]
    fn additional_tools_merge_and_namespace_tools_are_dropped() {
        let request = decode(serde_json::json!({
            "input": [{
                "type": "additional_tools",
                "tools": [
                    {"type": "custom", "name": "apply_patch", "description": "patch", "format": {"type": "grammar"}},
                    {"type": "namespace", "name": "ns", "tools": []}
                ]
            }],
            "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object", "properties": {}}}]
        }));
        let names: Vec<&str> = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(names, vec!["lookup", "apply_patch"]);
        let custom = request
            .tools
            .iter()
            .find(|tool| tool.name == "apply_patch")
            .unwrap();
        assert_eq!(
            custom.parameters,
            serde_json::json!({
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"]
            })
        );
        assert_eq!(custom.format, Some(serde_json::json!({"type": "grammar"})));
        assert!(
            request.messages.is_empty(),
            "additional_tools is not a message"
        );
    }

    #[test]
    fn function_tools_get_an_empty_schema_when_parameters_is_missing() {
        let request = decode(serde_json::json!({
            "tools": [{"type": "function", "name": "lookup"}]
        }));
        assert_eq!(
            request.tools[0].parameters,
            serde_json::json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn official_tool_choice_object_is_named() {
        let request = decode(serde_json::json!({
            "tools": [{"type": "function", "name": "lookup"}],
            "tool_choice": {"type": "function", "name": "lookup"}
        }));
        assert_eq!(request.tool_choice, ToolChoice::Named("lookup".to_string()));

        let chat_shape = decode(serde_json::json!({
            "tools": [{"type": "function", "name": "lookup"}],
            "tool_choice": {"type": "function", "function": {"name": "lookup"}}
        }));
        assert_eq!(
            chat_shape.tool_choice,
            ToolChoice::Named("lookup".to_string())
        );
        assert_eq!(
            decode(serde_json::json!({"tool_choice": "required"})).tool_choice,
            ToolChoice::Required
        );
    }

    #[test]
    fn function_call_and_output_items_round_trip() {
        let request = decode(serde_json::json!({
            "input": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "checking"}]},
                {"type": "function_call", "call_id": "call_1", "name": "Read", "arguments": "{\"path\":\"a.txt\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ]
        }));
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].role, Role::Assistant);
        assert_eq!(
            request.messages[0].parts,
            vec![
                ContentBlock::Text("checking".to_string()),
                ContentBlock::ToolUse(ToolUse {
                    id: "call_1".to_string(),
                    name: "Read".to_string(),
                    arguments: "{\"path\":\"a.txt\"}".to_string(),
                    signature: None,
                    custom: false,
                }),
            ]
        );
        assert_eq!(request.messages[1].role, Role::User);
        assert_eq!(
            request.messages[1].parts,
            vec![ContentBlock::ToolResult(ToolResult {
                tool_use_id: "call_1".to_string(),
                output: "ok".to_string(),
                is_error: false,
                custom: false,
            })]
        );
        assert_eq!(request.tool_uses.len(), 1);
        assert!(request.unpaired_tool_results.is_empty());
    }

    #[test]
    fn unpaired_tool_output_is_kept_and_reported() {
        let request = decode(serde_json::json!({
            "input": [{"type": "function_call_output", "call_id": "call_9", "output": [{"type": "output_text", "text": "a"}, {"type": "output_text", "text": "b"}]}]
        }));
        assert_eq!(request.messages.len(), 1);
        assert_eq!(
            request.messages[0].parts,
            vec![ContentBlock::ToolResult(ToolResult {
                tool_use_id: "call_9".to_string(),
                output: "a\n\nb".to_string(),
                is_error: false,
                custom: false,
            })]
        );
        assert_eq!(request.unpaired_tool_results.len(), 1);
    }

    #[test]
    fn custom_tool_calls_wrap_their_input() {
        let request = decode(serde_json::json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "input": "*** Begin Patch"},
                {"type": "custom_tool_call_output", "call_id": "call_2", "output": "done"}
            ]
        }));
        assert_eq!(
            request.messages[0].parts,
            vec![ContentBlock::ToolUse(ToolUse {
                id: "call_2".to_string(),
                name: "apply_patch".to_string(),
                arguments: "{\"input\":\"*** Begin Patch\"}".to_string(),
                signature: None,
                custom: true,
            })]
        );
        assert_eq!(
            request.messages[1].parts,
            vec![ContentBlock::ToolResult(ToolResult {
                tool_use_id: "call_2".to_string(),
                output: "done".to_string(),
                is_error: false,
                custom: true,
            })]
        );
    }

    #[test]
    fn reasoning_items_are_dropped_and_the_config_is_kept() {
        let request = decode(serde_json::json!({
            "input": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "old"}], "encrypted_content": "x"},
                {"type": "message", "role": "user", "content": "hi"}
            ],
            "reasoning": {"effort": "HIGH", "summary": "detailed", "context": "auto"}
        }));
        assert_eq!(request.messages.len(), 1);
        assert_eq!(
            request.reasoning.effort.as_deref(),
            Some("high"),
            "effort is lowercased"
        );
        assert_eq!(
            request.reasoning.extra,
            Some(serde_json::json!({"summary": "detailed", "context": "auto"}))
        );
    }

    #[test]
    fn missing_reasoning_leaves_the_provider_default_in_place() {
        let request = decode(serde_json::json!({"input": "hi"}));
        assert_eq!(request.reasoning.effort, None);
        assert_eq!(request.reasoning.extra, None);
    }

    #[test]
    fn parity_and_budget_fields_are_decoded() {
        let request = decode(serde_json::json!({
            "model": "claude-sonnet-5",
            "input": "hi",
            "max_output_tokens": 2048,
            "temperature": 0.2,
            "top_p": 0.9,
            "parallel_tool_calls": false,
            "stream": true
        }));
        assert_eq!(request.provider, Provider::Anthropic);
        assert_eq!(request.max_output_tokens, Some(2048));
        assert_eq!(request.temperature, Some(0.2));
        assert_eq!(request.top_p, Some(0.9));
        assert_eq!(request.parallel_tool_calls, Some(false));
        assert!(request.stream);

        let ignored = decode(serde_json::json!({
            "input": "hi",
            "max_output_tokens": 0,
            "parallel_tool_calls": "yes"
        }));
        assert_eq!(ignored.max_output_tokens, None);
        assert_eq!(ignored.parallel_tool_calls, Some(true));
    }

    #[test]
    fn passthrough_keeps_client_keys_but_not_provider_owned_ones() {
        let request = decode(serde_json::json!({
            "model": "gpt-5.6-sol",
            "stream": false,
            "input": "ping",
            "store": false,
            "instructions": "be brief",
            "tools": [],
            "reasoning": {"effort": "low"},
            "max_output_tokens": 64
        }));
        let keys: Vec<&str> = request
            .passthrough
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        // Client key order is preserved (serde_json `preserve_order`).
        assert_eq!(keys, vec!["input", "store", "instructions"]);
    }

    #[test]
    fn non_streaming_response_matches_the_spec_sample() {
        let response = NeutralResponse {
            id: Some("resp_1".to_string()),
            text: "查到了。".to_string(),
            thinking: "先查目录".to_string(),
            tool_calls: vec![ToolUse {
                id: "call_1".to_string(),
                name: "Read".to_string(),
                arguments: "{\"path\":\"a.txt\"}".to_string(),
                signature: None,
                custom: false,
            }],
            blocks: Vec::new(),
            stop_reason: StopReason::ToolCalls,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 30,
                reasoning_tokens: 12,
                cached_input_tokens: 0,
                total_tokens: 130,
            },
        };
        let bytes = ResponsesEncoder
            .encode_response(&response, "gpt-5.6-sol", 1_762_300_000)
            .expect("response encodes");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"id":"resp_1","object":"response","created_at":1762300000,"status":"completed","model":"gpt-5.6-sol","output":[{"id":"rs_0","type":"reasoning","status":"completed","summary":[{"type":"summary_text","text":"先查目录"}],"content":[]},{"id":"msg_1","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"查到了。","annotations":[]}]},{"id":"fc_call_1","type":"function_call","status":"completed","call_id":"call_1","name":"Read","arguments":"{\"path\":\"a.txt\"}"}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0},"output_tokens":30,"output_tokens_details":{"reasoning_tokens":12},"total_tokens":130},"error":null}"#
        );
    }

    #[test]
    fn non_streaming_response_falls_back_to_the_fixed_id_and_empty_output() {
        let response = NeutralResponse::default();
        let bytes = ResponsesEncoder
            .encode_response(&response, "gpt-5.6-sol", 10)
            .expect("response encodes");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"id":"resp_zed","object":"response","created_at":10,"status":"completed","model":"gpt-5.6-sol","output":[],"usage":{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":0},"error":null}"#
        );
    }

    #[test]
    fn stream_text_and_tool_frames_have_the_documented_bytes() {
        let mut state = SseState::new("gpt-5.6-sol", 1_762_300_000);
        let got = run(
            &[
                StreamEvent::Start {
                    id: Some("resp_1".to_string()),
                },
                StreamEvent::TextDelta {
                    delta: "片".to_string(),
                },
                StreamEvent::TextDelta {
                    delta: "段".to_string(),
                },
                StreamEvent::ToolStart {
                    index: 0,
                    id: "call_1".to_string(),
                    name: "Read".to_string(),
                    custom: false,
                },
                StreamEvent::ToolArgsDelta {
                    index: 0,
                    delta: "{\"path\":".to_string(),
                },
                StreamEvent::ToolArgsDelta {
                    index: 0,
                    delta: "\"a.txt\"}".to_string(),
                },
                StreamEvent::ToolEnd { index: 0 },
                StreamEvent::Usage(Usage {
                    input_tokens: 7,
                    output_tokens: 5,
                    reasoning_tokens: 2,
                    cached_input_tokens: 1,
                    total_tokens: 12,
                }),
                StreamEvent::End {
                    reason: StopReason::ToolCalls,
                },
            ],
            &mut state,
        );
        let expected: Vec<(&str, &str)> = vec![
            (
                "response.created",
                r#"{"type":"response.created","response":{"id":"resp_1","object":"response","created_at":1762300000,"status":"in_progress","model":"gpt-5.6-sol","output":[]},"sequence_number":1}"#,
            ),
            (
                "response.in_progress",
                r#"{"type":"response.in_progress","response":{"id":"resp_1","object":"response","created_at":1762300000,"status":"in_progress","model":"gpt-5.6-sol","output":[]},"sequence_number":2}"#,
            ),
            (
                "response.output_item.added",
                r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"msg_0","type":"message","status":"in_progress","role":"assistant","content":[]},"sequence_number":3}"#,
            ),
            (
                "response.content_part.added",
                r#"{"type":"response.content_part.added","item_id":"msg_0","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]},"sequence_number":4}"#,
            ),
            (
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","item_id":"msg_0","output_index":0,"content_index":0,"delta":"片","sequence_number":5}"#,
            ),
            (
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","item_id":"msg_0","output_index":0,"content_index":0,"delta":"段","sequence_number":6}"#,
            ),
            (
                "response.output_text.done",
                r#"{"type":"response.output_text.done","item_id":"msg_0","output_index":0,"content_index":0,"text":"片段","sequence_number":7}"#,
            ),
            (
                "response.content_part.done",
                r#"{"type":"response.content_part.done","item_id":"msg_0","output_index":0,"content_index":0,"part":{"type":"output_text","text":"片段","annotations":[]},"sequence_number":8}"#,
            ),
            (
                "response.output_item.done",
                r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"msg_0","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"片段","annotations":[]}]},"sequence_number":9}"#,
            ),
            (
                "response.output_item.added",
                r#"{"type":"response.output_item.added","output_index":1,"item":{"id":"fc_call_1","type":"function_call","status":"in_progress","call_id":"call_1","name":"Read","arguments":""},"sequence_number":10}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_call_1","output_index":1,"delta":"{\"path\":","sequence_number":11}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_call_1","output_index":1,"delta":"\"a.txt\"}","sequence_number":12}"#,
            ),
            (
                "response.function_call_arguments.done",
                r#"{"type":"response.function_call_arguments.done","item_id":"fc_call_1","output_index":1,"arguments":"{\"path\":\"a.txt\"}","sequence_number":13}"#,
            ),
            (
                "response.output_item.done",
                r#"{"type":"response.output_item.done","output_index":1,"item":{"id":"fc_call_1","type":"function_call","status":"completed","call_id":"call_1","name":"Read","arguments":"{\"path\":\"a.txt\"}"},"sequence_number":14}"#,
            ),
            (
                "response.completed",
                r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":1762300000,"status":"completed","model":"gpt-5.6-sol","output":[{"id":"msg_0","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"片段","annotations":[]}]},{"id":"fc_call_1","type":"function_call","status":"completed","call_id":"call_1","name":"Read","arguments":"{\"path\":\"a.txt\"}"}],"usage":{"input_tokens":7,"input_tokens_details":{"cached_tokens":1},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":12},"error":null},"sequence_number":15}"#,
            ),
        ];
        assert_eq!(
            got,
            expected
                .into_iter()
                .map(|(event, data)| (event.to_string(), data.to_string()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn reasoning_item_precedes_text_and_is_snapshotted() {
        let mut state = SseState::new("gpt-5.6-sol", 1);
        let got = run(
            &[
                StreamEvent::ThinkingDelta {
                    delta: "想".to_string(),
                },
                StreamEvent::TextDelta {
                    delta: "答".to_string(),
                },
                StreamEvent::End {
                    reason: StopReason::Stop,
                },
            ],
            &mut state,
        );
        let names: Vec<&str> = got.iter().map(|(event, _)| event.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta",
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
                "response.output_item.done",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_eq!(
            got[2].1,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"rs_0","type":"reasoning","status":"in_progress","summary":[]},"sequence_number":3}"#
        );
        assert_eq!(
            got[7].1,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"rs_0","type":"reasoning","status":"completed","summary":[{"type":"summary_text","text":"想"}],"content":[]},"sequence_number":8}"#
        );
        assert!(
            got[14]
                .1
                .contains(r#""output":[{"id":"rs_0","type":"reasoning","status":"completed","summary":[{"type":"summary_text","text":"想"}],"content":[]},{"id":"msg_1","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"答","annotations":[]}]}]"#),
            "completed output[] is reasoning then message: {}",
            got[14].1
        );
    }

    #[test]
    fn start_id_reaches_the_created_and_completed_frames() {
        let mut state = SseState::new("gpt-5.6-sol", 1_762_300_000);
        let got = run(
            &[
                StreamEvent::Start {
                    id: Some("resp_fixture_1".to_string()),
                },
                StreamEvent::TextDelta {
                    delta: "x".to_string(),
                },
                StreamEvent::End {
                    reason: StopReason::Stop,
                },
            ],
            &mut state,
        );
        assert!(
            got[0].1.contains(r#""id":"resp_fixture_1""#),
            "{}",
            got[0].1
        );
        assert!(
            got[1].1.contains(r#""id":"resp_fixture_1""#),
            "{}",
            got[1].1
        );
        let (event, data) = got.last().unwrap();
        assert_eq!(event, "response.completed");
        assert!(data.contains(r#""id":"resp_fixture_1""#), "{data}");
        assert!(!data.contains(RESPONSE_ID), "{data}");
    }

    #[test]
    fn empty_reply_still_emits_created_in_progress_and_completed() {
        let mut state = SseState::new("gpt-5.6-sol", 1_762_300_000);
        let got = run(&[], &mut state);
        assert_eq!(
            got,
            vec![
                (
                    "response.created".to_string(),
                    r#"{"type":"response.created","response":{"id":"resp_zed","object":"response","created_at":1762300000,"status":"in_progress","model":"gpt-5.6-sol","output":[]},"sequence_number":1}"#.to_string()
                ),
                (
                    "response.in_progress".to_string(),
                    r#"{"type":"response.in_progress","response":{"id":"resp_zed","object":"response","created_at":1762300000,"status":"in_progress","model":"gpt-5.6-sol","output":[]},"sequence_number":2}"#.to_string()
                ),
                (
                    "response.completed".to_string(),
                    r#"{"type":"response.completed","response":{"id":"resp_zed","object":"response","created_at":1762300000,"status":"completed","model":"gpt-5.6-sol","output":[],"usage":{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":0},"error":null},"sequence_number":3}"#.to_string()
                ),
            ]
        );
    }

    #[test]
    fn after_commit_failure_emits_response_failed_and_closes() {
        let mut state = SseState::new("gpt-5.6-sol", 1_762_300_000);
        let encoder = ResponsesEncoder;
        let mut bytes = Vec::new();
        for event in [
            StreamEvent::Start {
                id: Some("resp_1".to_string()),
            },
            StreamEvent::TextDelta {
                delta: "半".to_string(),
            },
            StreamEvent::Error(GatewayError::transient(None, "boom")),
        ] {
            if let Some(frame) = encoder.encode_event(&event, &mut state).unwrap() {
                bytes.extend(frame);
            }
        }
        assert!(encoder.finish(&mut state).unwrap().is_empty());
        let got = frames(&bytes);
        assert_eq!(got.last().unwrap().0, "response.failed");
        assert_eq!(
            got.last().unwrap().1,
            r#"{"type":"response.failed","response":{"id":"resp_1","object":"response","created_at":1762300000,"status":"failed","model":"gpt-5.6-sol","output":[],"error":{"code":"upstream_error","message":"boom"},"status_details":{"error":{"message":"boom"}}}}"#
        );
    }

    #[test]
    fn custom_tool_calls_encode_as_custom_tool_call_items() {
        let response = NeutralResponse {
            tool_calls: vec![ToolUse {
                id: "call_1".to_string(),
                name: "apply_patch".to_string(),
                arguments: r#"{"input":"*** Begin Patch"}"#.to_string(),
                signature: None,
                custom: true,
            }],
            ..NeutralResponse::default()
        };
        let bytes = ResponsesEncoder
            .encode_response(&response, "gpt-5.6-luna", 1)
            .expect("response encodes");
        let body = String::from_utf8(bytes).unwrap();
        assert!(
            body.contains(r#"{"id":"ctc_call_1","type":"custom_tool_call","status":"completed","call_id":"call_1","name":"apply_patch","input":"*** Begin Patch"}"#),
            "{body}"
        );
        assert!(!body.contains("function_call"), "{body}");
        assert!(!body.contains("arguments"), "{body}");
    }

    #[test]
    fn custom_tool_stream_emits_custom_input_frames() {
        let mut state = SseState::new("gpt-5.6-luna", 1);
        let got = run(
            &[
                StreamEvent::ToolStart {
                    index: 0,
                    id: "call_1".to_string(),
                    name: "apply_patch".to_string(),
                    custom: true,
                },
                StreamEvent::ToolArgsDelta {
                    index: 0,
                    delta: "*** Begin".to_string(),
                },
                StreamEvent::ToolArgsDelta {
                    index: 0,
                    delta: " Patch".to_string(),
                },
                StreamEvent::ToolEnd { index: 0 },
                StreamEvent::End {
                    reason: StopReason::ToolCalls,
                },
            ],
            &mut state,
        );
        let names: Vec<&str> = got.iter().map(|(event, _)| event.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.custom_tool_call_input.delta",
                "response.custom_tool_call_input.delta",
                "response.custom_tool_call_input.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert!(
            got[2]
                .1
                .contains(r#"{"id":"ctc_call_1","type":"custom_tool_call","status":"in_progress","call_id":"call_1","name":"apply_patch","input":""}"#),
            "{}",
            got[2].1
        );
        assert!(
            got[5].1.contains(r#""input":"*** Begin Patch""#),
            "{}",
            got[5].1
        );
        assert!(
            got[6].1.contains(r#"{"id":"ctc_call_1","type":"custom_tool_call","status":"completed","call_id":"call_1","name":"apply_patch","input":"*** Begin Patch"}"#),
            "{}",
            got[6].1
        );
        assert!(
            got[7].1.contains(r#"{"id":"ctc_call_1","type":"custom_tool_call","status":"completed","call_id":"call_1","name":"apply_patch","input":"*** Begin Patch"}"#),
            "{}",
            got[7].1
        );
    }

    #[test]
    fn several_tool_items_get_contiguous_output_indices() {
        let mut state = SseState::new("gpt-5.6-sol", 1);
        let got = run(
            &[
                StreamEvent::ToolStart {
                    index: 0,
                    id: "call_1".to_string(),
                    name: "Read".to_string(),
                    custom: false,
                },
                StreamEvent::ToolArgsDelta {
                    index: 0,
                    delta: "{\"path\":\"a\"}".to_string(),
                },
                StreamEvent::ToolEnd { index: 0 },
                StreamEvent::ToolStart {
                    index: 1,
                    id: "call_2".to_string(),
                    name: "Write".to_string(),
                    custom: false,
                },
                StreamEvent::ToolEnd { index: 1 },
                StreamEvent::End {
                    reason: StopReason::ToolCalls,
                },
            ],
            &mut state,
        );
        let added: Vec<&str> = got
            .iter()
            .filter(|(event, _)| event == "response.output_item.added")
            .map(|(_, data)| data.as_str())
            .collect();
        assert_eq!(added.len(), 2);
        assert!(added[0].contains(r#""output_index":0"#), "{}", added[0]);
        assert!(added[1].contains(r#""output_index":1"#), "{}", added[1]);
        let last = &got.last().unwrap().1;
        assert!(last.contains(r#""id":"fc_call_1""#), "{last}");
        assert!(last.contains(r#""id":"fc_call_2""#), "{last}");
        assert!(
            last.contains(r#""arguments":"{}""#),
            "a tool without arguments gets {{}}: {last}"
        );
    }

    #[test]
    fn truncated_stream_reports_incomplete() {
        let mut state = SseState::new("gpt-5.6-sol", 1);
        let got = run(
            &[
                StreamEvent::TextDelta {
                    delta: "x".to_string(),
                },
                StreamEvent::End {
                    reason: StopReason::Length,
                },
            ],
            &mut state,
        );
        let (event, data) = got.last().unwrap();
        assert_eq!(event, "response.incomplete");
        assert!(
            data.contains(
                r#""status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}"#
            ),
            "{data}"
        );
    }
}
