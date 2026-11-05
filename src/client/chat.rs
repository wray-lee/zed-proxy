//! OpenAI Chat Completions codec (spec `40-client-openai-chat.md`).
//!
//! Request decoding is spec 40 §1, the non-streaming object §2 and the SSE
//! frames §3. JSON is built by hand because the key order is normative and
//! `serde_json::Map` would re-sort it.
//!
//! Chat has no content-block indices, so two `SseState` fields are reused as
//! flags instead of adding fields to the shared struct: `next_block_index`
//! marks "role frame already sent" and `open_block` marks "end frame already
//! sent" / "terminal error".

use serde_json::{Map, Value};

use crate::catalog::Catalog;
use crate::error::GatewayError;
use crate::neutral::{
    ContentBlock, ImageSource, Message, NeutralRequest, NeutralResponse, Reasoning, Role,
    StopReason, StreamEvent, SystemBlock, ToolChoice, ToolDef, ToolResult, ToolUse, Usage,
};
use crate::proxy::stream::{sse_data, SseState, DONE_FRAME};

use super::{tool_call_id, ClientDecoder, ClientEncoder};

/// Fixed non-streaming object id (spec 40 §2.1).
pub const CHAT_COMPLETION_ID: &str = "chatcmpl-zed";

/// Empty JSON Schema used when `parameters` is missing or not an object.
const EMPTY_SCHEMA: &str = r#"{"type":"object","properties":{}}"#;

/// `SseState::next_block_index` doubles as the "role frame sent" flag here:
/// Chat has no content-block indices, so the field is otherwise unused.
const ROLE_SENT: u64 = 1;
/// `SseState::open_block` doubles as the terminal-state flag here.
const END_SENT: u64 = 1;
const TERMINAL: u64 = u64::MAX;

fn catalog() -> &'static Catalog {
    static CATALOG: std::sync::OnceLock<Catalog> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| Catalog::builtin().expect("built-in catalog parses"))
}

fn warn(what: &str) {
    eprintln!("[zed-proxy] chat: {what}");
}

fn jstr(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn positive_int(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_i64).filter(|value| *value > 0)
}

/// `data:<mime>;base64,<data>` or `http(s)://`; anything else is unsupported
/// (spec 40 §1.3). The `detail` hint has no neutral field and is dropped.
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

/// One `content` value: string, array, null or missing (spec 40 §1.3).
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
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            for item in items {
                let Some(object) = item.as_object() else {
                    warn("dropped non-object content part");
                    continue;
                };
                match object.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = object.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                parts.push(ContentBlock::Text(text.to_string()));
                            }
                        }
                    }
                    Some("image_url") => {
                        let url = match object.get("image_url") {
                            Some(Value::String(url)) => Some(url.as_str()),
                            Some(Value::Object(inner)) => inner.get("url").and_then(Value::as_str),
                            _ => None,
                        };
                        match url.and_then(decode_image_url) {
                            Some(source) => parts.push(ContentBlock::Image(source)),
                            None => warn("dropped unsupported image_url"),
                        }
                    }
                    other => warn(&format!("dropped content part {:?}", other.unwrap_or("?"))),
                }
            }
            parts
        }
        Some(_) => {
            warn("ignored non-string, non-array content");
            Vec::new()
        }
    }
}

/// Text of a system/developer `content`, joined with `\n\n` (spec 40 §1.2).
fn system_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                if object.get("type").and_then(Value::as_str) == Some("text") {
                    object.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// Normalized `arguments` text (spec 40 §1.4): objects are compacted, strings
/// that do not parse to an object become `{}`.
fn decode_arguments(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(_)) => text.clone(),
            _ => {
                warn("tool arguments are not a JSON object; using {}");
                "{}".to_string()
            }
        },
        Some(Value::Object(_)) => {
            serde_json::to_string(value.unwrap()).unwrap_or_else(|_| "{}".to_string())
        }
        None => "{}".to_string(),
        Some(_) => {
            warn("tool arguments are neither string nor object; using {}");
            "{}".to_string()
        }
    }
}

fn decode_tool_calls(value: Option<&Value>) -> Vec<ToolUse> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut calls = Vec::new();
    for item in items {
        let Some(object) = item.as_object() else {
            warn("dropped non-object tool call");
            continue;
        };
        if let Some(kind) = object.get("type").and_then(Value::as_str) {
            if kind != "function" {
                warn("dropped non-function tool call");
                continue;
            }
        }
        let Some(function) = object.get("function").and_then(Value::as_object) else {
            warn("dropped tool call without function object");
            continue;
        };
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if name.is_empty() {
            warn("dropped tool call without name");
            continue;
        }
        let id = object.get("id").and_then(Value::as_str);
        calls.push(ToolUse {
            id: tool_call_id(id),
            name: name.to_string(),
            arguments: decode_arguments(function.get("arguments")),
            signature: None,
            custom: false,
        });
    }
    calls
}

fn tool_content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                if object.get("type").and_then(Value::as_str) == Some("text") {
                    object.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn decode_tool_message(object: &Map<String, Value>) -> Option<ToolResult> {
    let id = object
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if id.is_empty() {
        warn("dropped tool message without tool_call_id");
        return None;
    }
    Some(ToolResult {
        tool_use_id: id.to_string(),
        output: tool_content_text(object.get("content")),
        is_error: false,
    })
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
        if object.get("type").and_then(Value::as_str) != Some("function") {
            warn("dropped non-function tool declaration");
            continue;
        }
        let function = object.get("function").and_then(Value::as_object);
        let name = function
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if name.is_empty() {
            warn("dropped tool declaration without name");
            continue;
        }
        let description = function
            .and_then(|function| function.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let parameters = function
            .and_then(|function| function.get("parameters"))
            .filter(|parameters| parameters.is_object())
            .cloned()
            .unwrap_or_else(|| serde_json::from_str(EMPTY_SCHEMA).expect("literal schema"));
        let strict = function
            .and_then(|function| function.get("strict"))
            .and_then(Value::as_bool);
        tools.push(ToolDef {
            name: name.to_string(),
            description,
            parameters,
            strict,
            cache_control: None,
            format: None,
        });
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
            let named = if object.get("type").and_then(Value::as_str) == Some("function") {
                object
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .or_else(|| object.get("name").and_then(Value::as_str))
            } else {
                None
            };
            match named {
                Some(name) if !name.is_empty() => ToolChoice::Named(name.to_string()),
                _ => {
                    warn("unknown tool_choice object; using auto");
                    ToolChoice::Auto
                }
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
    let mut effort = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_lowercase);
    let mut extra = None;
    if let Some(object) = body.get("reasoning").and_then(Value::as_object) {
        if effort.is_none() {
            effort = object
                .get("effort")
                .and_then(Value::as_str)
                .map(str::to_lowercase);
        }
        let rest: Map<String, Value> = object
            .iter()
            .filter(|(key, _)| key.as_str() != "effort")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if !rest.is_empty() {
            extra = Some(Value::Object(rest));
        }
    }
    Reasoning { effort, extra }
}

fn decode_stop(value: Option<&Value>) -> Vec<String> {
    let items: Vec<&str> = match value {
        Some(Value::String(text)) => vec![text.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).take(4).collect(),
        _ => return Vec::new(),
    };
    items
        .into_iter()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
        .collect()
}

pub struct ChatDecoder;

impl ClientDecoder for ChatDecoder {
    fn decode(&self, body: &serde_json::Value) -> Result<NeutralRequest, GatewayError> {
        let catalog = catalog();
        let model = catalog.normalize_model(body.get("model").and_then(Value::as_str));
        let provider = catalog.resolve_provider(&model)?;

        let mut system_texts: Vec<String> = Vec::new();
        let mut messages: Vec<Message> = Vec::new();
        let mut pending_results: Vec<ToolResult> = Vec::new();
        let mut tool_uses: Vec<ToolUse> = Vec::new();

        if let Some(items) = body.get("messages").and_then(Value::as_array) {
            for item in items {
                let Some(object) = item.as_object() else {
                    warn("dropped non-object message");
                    continue;
                };
                let role = object
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if role != "tool" && !pending_results.is_empty() && role != "user" {
                    messages.push(Message {
                        role: Role::User,
                        parts: pending_results
                            .drain(..)
                            .map(ContentBlock::ToolResult)
                            .collect(),
                    });
                }
                match role {
                    "system" | "developer" => {
                        let text = system_text(object.get("content"));
                        if !text.is_empty() {
                            system_texts.push(text);
                        }
                    }
                    "user" => {
                        let mut parts: Vec<ContentBlock> = pending_results
                            .drain(..)
                            .map(ContentBlock::ToolResult)
                            .collect();
                        parts.extend(decode_content(object.get("content")));
                        messages.push(Message {
                            role: Role::User,
                            parts,
                        });
                    }
                    "assistant" => {
                        let mut parts = decode_content(object.get("content"));
                        let calls = decode_tool_calls(object.get("tool_calls"));
                        tool_uses.extend(calls.iter().cloned());
                        parts.extend(calls.into_iter().map(ContentBlock::ToolUse));
                        messages.push(Message {
                            role: Role::Assistant,
                            parts,
                        });
                    }
                    "tool" => {
                        if let Some(result) = decode_tool_message(object) {
                            pending_results.push(result);
                        }
                    }
                    other => warn(&format!("dropped message with role {other:?}")),
                }
            }
        }
        if !pending_results.is_empty() {
            messages.push(Message {
                role: Role::User,
                parts: pending_results
                    .drain(..)
                    .map(ContentBlock::ToolResult)
                    .collect(),
            });
        }

        let tools = decode_tools(body.get("tools"));
        let tool_choice = decode_tool_choice(body.get("tool_choice"), &tools);
        let known: Vec<&str> = tool_uses.iter().map(|call| call.id.as_str()).collect();
        let unpaired_tool_results: Vec<ToolResult> = messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                ContentBlock::ToolResult(result)
                    if !known.contains(&result.tool_use_id.as_str()) =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .collect();

        let mut request = NeutralRequest {
            model,
            provider,
            messages,
            tools,
            tool_choice,
            parallel_tool_calls: Some(
                body.get("parallel_tool_calls")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            ),
            reasoning: decode_reasoning(body),
            max_output_tokens: positive_int(body.get("max_completion_tokens"))
                .or_else(|| positive_int(body.get("max_tokens"))),
            temperature: body.get("temperature").and_then(Value::as_f64),
            top_p: body.get("top_p").and_then(Value::as_f64),
            stop: decode_stop(body.get("stop")),
            stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
            include_usage: body
                .get("stream_options")
                .and_then(Value::as_object)
                .and_then(|options| options.get("include_usage"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            tool_uses,
            unpaired_tool_results,
            ..NeutralRequest::default()
        };
        if !system_texts.is_empty() {
            request.system.push(SystemBlock {
                text: system_texts.join("\n\n"),
                cache_control: None,
            });
            // Chat has one string-shaped system prompt, and the baseline keeps
            // that shorthand on the Anthropic wire (spec 21 §4).
            request.system_string = true;
        }
        if body.get("n").and_then(Value::as_i64).is_some_and(|n| n > 1) {
            warn("chat: n > 1 is not supported; using 1");
        }
        if body.get("response_format").is_some() {
            warn("ignored response_format");
        }
        Ok(request)
    }
}
/// Chat finish reason (spec 40 §2.3): tool calls win over every other reason.
fn finish_reason(reason: StopReason, has_tools: bool) -> &'static str {
    if has_tools {
        return "tool_calls";
    }
    match reason {
        StopReason::Length => "length",
        StopReason::ContentFilter => "content_filter",
        _ => "stop",
    }
}

fn chat_usage(usage: Usage) -> String {
    let usage = usage.with_computed_total();
    format!(
        "{{\"prompt_tokens\":{},\"completion_tokens\":{},\"total_tokens\":{}}}",
        usage.input_tokens, usage.output_tokens, usage.total_tokens
    )
}

fn tool_calls_json(calls: &[ToolUse]) -> String {
    let mut out = String::from("[");
    for (position, call) in calls.iter().enumerate() {
        if position > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"id\":{},\"type\":\"function\",\"function\":{{\"name\":{},\"arguments\":{}}}}}",
            jstr(&call.id),
            jstr(&call.name),
            jstr(&call.arguments)
        ));
    }
    out.push(']');
    out
}

fn message_json(response: &NeutralResponse) -> String {
    let mut out = String::from("{\"role\":\"assistant\"");
    if !response.thinking.is_empty() {
        let thinking = jstr(&response.thinking);
        out.push_str(&format!(
            ",\"thinking\":{thinking},\"reasoning_content\":{thinking}"
        ));
    }
    let content_null = !response.tool_calls.is_empty() && response.text.is_empty();
    if content_null {
        out.push_str(",\"content\":null");
    } else {
        out.push_str(&format!(",\"content\":{}", jstr(&response.text)));
    }
    if !response.tool_calls.is_empty() {
        out.push_str(",\"tool_calls\":");
        out.push_str(&tool_calls_json(&response.tool_calls));
    }
    out.push('}');
    out
}

pub struct ChatEncoder;

impl ClientEncoder for ChatEncoder {
    fn encode_response(
        &self,
        response: &NeutralResponse,
        model: &str,
        now_unix: i64,
    ) -> Result<Vec<u8>, GatewayError> {
        let finish = finish_reason(response.stop_reason, !response.tool_calls.is_empty());
        Ok(format!(
            "{{\"id\":\"{CHAT_COMPLETION_ID}\",\"object\":\"chat.completion\",\"created\":{now_unix},\"model\":{},\"choices\":[{{\"index\":0,\"message\":{},\"finish_reason\":\"{finish}\"}}],\"usage\":{}}}",
            jstr(model),
            message_json(response),
            chat_usage(response.usage)
        )
        .into_bytes())
    }

    fn encode_event(
        &self,
        event: &StreamEvent,
        state: &mut SseState,
    ) -> Result<Option<Vec<u8>>, GatewayError> {
        let frame = match event {
            StreamEvent::Start { .. } => role_frame(state),
            StreamEvent::BlockStart { .. } | StreamEvent::BlockStop { .. } => Vec::new(),
            StreamEvent::TextDelta { delta } => {
                let mut out = role_frame(state);
                out.extend(chunk(
                    state,
                    &format!("{{\"content\":{}}}", jstr(delta)),
                    "null",
                ));
                out
            }
            StreamEvent::ThinkingDelta { delta } => {
                let mut out = role_frame(state);
                out.extend(chunk(
                    state,
                    &format!("{{\"reasoning_content\":{}}}", jstr(delta)),
                    "null",
                ));
                out
            }
            StreamEvent::ToolStart { id, name, .. } => {
                let mut out = role_frame(state);
                let index = state.tool_count;
                state.tool_count += 1;
                state.tool_arguments.push(String::new());
                out.extend(chunk(
                    state,
                    &format!(
                        "{{\"tool_calls\":[{{\"index\":{index},\"id\":{},\"type\":\"function\",\"function\":{{\"name\":{},\"arguments\":\"\"}}}}]}}",
                        jstr(id),
                        jstr(name)
                    ),
                    "null",
                ));
                out
            }
            StreamEvent::ToolArgsDelta { delta, .. } => {
                if let Some(args) = state.tool_arguments.last_mut() {
                    args.push_str(delta);
                }
                let index = state.tool_count.saturating_sub(1);
                chunk(
                    state,
                    &format!(
                        "{{\"tool_calls\":[{{\"index\":{index},\"function\":{{\"arguments\":{}}}}}]}}",
                        jstr(delta)
                    ),
                    "null",
                )
            }
            StreamEvent::ToolEnd { .. } => {
                // An empty argument list still has to parse as JSON on the
                // client side (spec 40 §3.4).
                let mut out = Vec::new();
                if state
                    .tool_arguments
                    .last()
                    .is_some_and(|args| args.is_empty())
                {
                    let index = state.tool_count.saturating_sub(1);
                    out.extend(chunk(
                        state,
                        &format!(
                            "{{\"tool_calls\":[{{\"index\":{index},\"function\":{{\"arguments\":\"{{}}\"}}}}]}}"
                        ),
                        "null",
                    ));
                }
                out
            }
            StreamEvent::Usage(usage) => {
                state.usage = *usage;
                Vec::new()
            }
            StreamEvent::End { reason } => {
                state.stop_reason = *reason;
                state.open_block = Some(END_SENT);
                chunk(
                    state,
                    "{}",
                    &format!("\"{}\"", finish_reason(*reason, state.tool_count > 0)),
                )
            }
            StreamEvent::Error(err) => {
                state.open_block = Some(TERMINAL);
                let body = format!(
                    "{{\"error\":{{\"message\":{},\"type\":\"{}\"}}}}",
                    jstr(&err.message),
                    err.openai_type()
                );
                sse_data(body.as_bytes())
            }
        };
        Ok(if frame.is_empty() { None } else { Some(frame) })
    }

    fn finish(&self, state: &mut SseState) -> Result<Vec<u8>, GatewayError> {
        let mut out = Vec::new();
        match state.open_block {
            Some(TERMINAL) => {}
            Some(END_SENT) => {}
            _ => {
                state.open_block = Some(END_SENT);
                out.extend(chunk(
                    state,
                    "{}",
                    &format!(
                        "\"{}\"",
                        finish_reason(state.stop_reason, state.tool_count > 0)
                    ),
                ));
            }
        }
        if state.include_usage && state.open_block != Some(TERMINAL) {
            out.extend(usage_chunk(state));
        }
        out.extend_from_slice(DONE_FRAME);
        Ok(out)
    }
}

fn role_frame(state: &mut SseState) -> Vec<u8> {
    if state.next_block_index == ROLE_SENT {
        return Vec::new();
    }
    state.next_block_index = ROLE_SENT;
    chunk(state, "{\"role\":\"assistant\"}", "null")
}

fn chunk(state: &SseState, delta: &str, finish: &str) -> Vec<u8> {
    let body = format!(
        "{{\"id\":\"{CHAT_COMPLETION_ID}\",\"object\":\"chat.completion.chunk\",\"created\":{},\"model\":{},\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":{finish}}}]}}",
        state.created_unix,
        jstr(&state.model)
    );
    sse_data(body.as_bytes())
}

fn usage_chunk(state: &SseState) -> Vec<u8> {
    let body = format!(
        "{{\"id\":\"{CHAT_COMPLETION_ID}\",\"object\":\"chat.completion.chunk\",\"created\":{},\"model\":{},\"choices\":[],\"usage\":{}}}",
        state.created_unix,
        jstr(&state.model),
        chat_usage(state.usage)
    );
    sse_data(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::neutral::BlockKind;

    fn decode(body: Value) -> NeutralRequest {
        ChatDecoder.decode(&body).unwrap()
    }

    fn frame_texts(frames: &[u8]) -> Vec<String> {
        String::from_utf8(frames.to_vec())
            .unwrap()
            .split("\n\n")
            .filter(|frame| !frame.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn data_of(frame: &str) -> &str {
        frame.strip_prefix("data: ").unwrap()
    }

    fn tool(id: &str, name: &str, arguments: &str) -> ToolUse {
        ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
            signature: None,
            custom: false,
        }
    }

    fn tool_result(id: &str, output: &str) -> ToolResult {
        ToolResult {
            tool_use_id: id.to_string(),
            output: output.to_string(),
            is_error: false,
        }
    }

    fn usage() -> Usage {
        Usage {
            input_tokens: 100,
            output_tokens: 20,
            reasoning_tokens: 0,
            cached_input_tokens: 0,
            total_tokens: 120,
        }
    }

    // --- request decoding (spec 40 §1) ---

    #[test]
    fn system_and_developer_messages_become_one_top_level_block() {
        let request = decode(serde_json::json!({
            "model": "claude-sonnet-5",
            "messages": [
                {"role": "system", "content": "base"},
                {"role": "developer", "content": "dev"},
                {"role": "user", "content": "hi"}
            ]
        }));
        assert_eq!(
            request.system,
            vec![SystemBlock {
                text: "base\n\ndev".to_string(),
                cache_control: None
            }]
        );
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].role, Role::User);
        assert_eq!(
            request.messages[0].parts,
            vec![ContentBlock::Text("hi".into())]
        );
    }

    #[test]
    fn unknown_roles_are_dropped_and_null_assistant_content_is_kept() {
        let request = decode(serde_json::json!({
            "messages": [
                {"role": "wizard", "content": "nope"},
                {"role": "assistant", "content": null}
            ]
        }));
        assert_eq!(request.messages.len(), 1);
        assert!(request.messages[0].parts.is_empty());
        assert_eq!(request.messages[0].role, Role::Assistant);
    }

    #[test]
    fn tool_messages_merge_into_a_user_message_ahead_of_text() {
        let request = decode(serde_json::json!({
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_a", "type": "function", "function": {"name": "Read", "arguments": "{\"path\":\"a\"}"}},
                    {"id": "call_b", "type": "function", "function": {"name": "Read", "arguments": "{\"path\":\"b\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_a", "content": "A"},
                {"role": "tool", "tool_call_id": "call_b", "content": [
                    {"type": "text", "text": "B1"},
                    {"type": "text", "text": "B2"}
                ]},
                {"role": "user", "content": "next"}
            ]
        }));
        // The tool results merge into the following user turn, ahead of its
        // text (spec 40 §1.2 rule 1).
        assert_eq!(request.messages.len(), 2);
        assert_eq!(
            request.messages[1].parts,
            vec![
                ContentBlock::ToolResult(tool_result("call_a", "A")),
                ContentBlock::ToolResult(tool_result("call_b", "B1\n\nB2")),
                ContentBlock::Text("next".into()),
            ]
        );
        assert_eq!(request.tool_uses.len(), 2);
        assert!(request.unpaired_tool_results.is_empty());
    }

    #[test]
    fn dangling_tool_messages_become_a_trailing_user_message() {
        let request = decode(serde_json::json!({
            "messages": [
                {"role": "tool", "tool_call_id": "call_x", "content": "x"}
            ]
        }));
        assert_eq!(request.messages.len(), 1);
        assert_eq!(
            request.messages[0].parts,
            vec![ContentBlock::ToolResult(tool_result("call_x", "x"))]
        );
        assert_eq!(
            request.unpaired_tool_results,
            vec![tool_result("call_x", "x")]
        );
    }

    #[test]
    fn image_url_and_text_parts_decode_in_order() {
        let request = decode(serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA", "detail": "high"}},
                {"type": "image_url", "image_url": {"url": "https://example.test/a.png"}},
                {"type": "image_url", "image_url": {"url": "file:///etc/passwd"}},
                {"type": "input_audio", "input_audio": {"data": "x"}}
            ]}]
        }));
        assert_eq!(
            request.messages[0].parts,
            vec![
                ContentBlock::Text("look".into()),
                ContentBlock::Image(ImageSource::Data {
                    media_type: "image/png".to_string(),
                    data: "AAAA".to_string()
                }),
                ContentBlock::Image(ImageSource::Url("https://example.test/a.png".to_string())),
            ]
        );
    }

    #[test]
    fn tool_calls_generate_missing_ids_and_normalise_arguments() {
        let request = decode(serde_json::json!({
            "messages": [{"role": "assistant", "content": "", "tool_calls": [
                {"type": "function", "function": {"name": "Read", "arguments": "{\"path\":\"a\"}"}},
                {"type": "function", "function": {"name": "", "arguments": "{}"}},
                {"type": "custom", "function": {"name": "X", "arguments": "{}"}},
                {"id": "", "type": "function", "function": {"name": "Bash", "arguments": {"cmd": "ls"}}},
                {"id": "call_keep", "type": "function", "function": {"name": "Glob", "arguments": "not json"}}
            ]}]
        }));
        let calls = request.tool_uses;
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].name, "Read");
        assert_eq!(calls[0].id.len(), 21);
        assert!(calls[0].id.starts_with("call_"));
        assert_eq!(calls[1].name, "Bash");
        assert_eq!(calls[1].arguments, "{\"cmd\":\"ls\"}");
        assert_eq!(calls[2].id, "call_keep");
        assert_eq!(calls[2].arguments, "{}");
    }

    #[test]
    fn tools_get_empty_schemas_and_tool_choice_narrows() {
        let request = decode(serde_json::json!({
            "tools": [
                {"type": "function", "function": {"name": "Read"}},
                {"type": "function", "function": {"name": "Bash", "description": 7, "parameters": {"type": "object"}, "strict": true}},
                {"type": "web_search"},
                {"type": "function", "function": {"name": ""}}
            ],
            "tool_choice": {"type": "function", "function": {"name": "Read"}},
            "parallel_tool_calls": false
        }));
        assert_eq!(request.tools.len(), 2);
        assert_eq!(
            request.tools[0].parameters,
            serde_json::json!({"type": "object", "properties": {}})
        );
        assert_eq!(request.tools[0].description, "");
        assert_eq!(request.tools[1].strict, Some(true));
        assert_eq!(request.tool_choice, ToolChoice::Named("Read".into()));
        assert_eq!(request.parallel_tool_calls, Some(false));
    }

    #[test]
    fn named_tool_choice_without_tools_falls_back_to_auto() {
        let request = decode(serde_json::json!({
            "tool_choice": {"type": "function", "name": "Read"}
        }));
        assert_eq!(request.tool_choice, ToolChoice::Auto);
        assert_eq!(request.parallel_tool_calls, Some(true));
    }

    #[test]
    fn reasoning_effort_reads_top_level_first_then_nested() {
        let request = decode(serde_json::json!({
            "reasoning_effort": "HIGH",
            "reasoning": {"effort": "low", "summary": "detailed", "context": "auto"}
        }));
        assert_eq!(request.reasoning.effort.as_deref(), Some("high"));
        assert_eq!(
            request.reasoning.extra,
            Some(serde_json::json!({"summary": "detailed", "context": "auto"}))
        );

        let request = decode(serde_json::json!({
            "reasoning": {"effort": "Medium"}
        }));
        assert_eq!(request.reasoning.effort.as_deref(), Some("medium"));
        assert_eq!(request.reasoning.extra, None);
    }

    #[test]
    fn budgets_prefer_max_completion_tokens_and_ignore_non_positive() {
        let request = decode(serde_json::json!({
            "max_tokens": 100,
            "max_completion_tokens": 200
        }));
        assert_eq!(request.max_output_tokens, Some(200));
        let request = decode(serde_json::json!({
            "max_tokens": 100,
            "max_completion_tokens": -5
        }));
        assert_eq!(request.max_output_tokens, Some(100));
        let request = decode(serde_json::json!({"max_tokens": "big"}));
        assert_eq!(request.max_output_tokens, None);
    }

    #[test]
    fn sampling_parameters_and_stop_decode() {
        let request = decode(serde_json::json!({
            "temperature": 0.5,
            "top_p": 0.25,
            "stop": ["END", "  ", "STOP", "A", "B", "C", "D"]
        }));
        assert_eq!(request.temperature, Some(0.5));
        assert_eq!(request.top_p, Some(0.25));
        // spec 40 §1.1: first four string elements, then blanks are dropped.
        assert_eq!(request.stop, vec!["END", "STOP", "A"]);

        let request = decode(serde_json::json!({"stop": "END"}));
        assert_eq!(request.stop, vec!["END"]);
    }

    #[test]
    fn stream_and_usage_flags_decode() {
        let request = decode(serde_json::json!({
            "stream": true,
            "stream_options": {"include_usage": true}
        }));
        assert!(request.stream);
        assert!(request.include_usage);

        let request = decode(serde_json::json!({
            "stream": "true",
            "stream_options": 1
        }));
        assert!(!request.stream);
        assert!(!request.include_usage);
    }

    // --- non-streaming object (spec 40 §2) ---

    #[test]
    fn non_streaming_object_matches_the_spec_bytes() {
        let response = NeutralResponse {
            text: String::new(),
            thinking: "需要读文件".to_string(),
            tool_calls: vec![tool("call_1", "Read", "{\"path\":\"a.txt\"}")],
            stop_reason: StopReason::ToolCalls,
            usage: usage(),
            ..NeutralResponse::default()
        };
        let body = ChatEncoder
            .encode_response(&response, "claude-sonnet-5", 1_762_300_000)
            .unwrap();
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"id":"chatcmpl-zed","object":"chat.completion","created":1762300000,"model":"claude-sonnet-5","choices":[{"index":0,"message":{"role":"assistant","thinking":"需要读文件","reasoning_content":"需要读文件","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"Read","arguments":"{\"path\":\"a.txt\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}}"#
        );
    }

    #[test]
    fn non_streaming_text_without_thinking_or_tools() {
        let response = NeutralResponse {
            text: "OK".to_string(),
            stop_reason: StopReason::Stop,
            usage: Usage {
                input_tokens: 12,
                output_tokens: 3,
                ..Usage::default()
            },
            ..NeutralResponse::default()
        };
        let body = ChatEncoder
            .encode_response(&response, "gpt-5.6-luna", 1_762_300_000)
            .unwrap();
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"id":"chatcmpl-zed","object":"chat.completion","created":1762300000,"model":"gpt-5.6-luna","choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}"#
        );
    }

    #[test]
    fn finish_reason_mapping_and_tool_precedence() {
        assert_eq!(finish_reason(StopReason::Length, false), "length");
        assert_eq!(
            finish_reason(StopReason::ContentFilter, false),
            "content_filter"
        );
        assert_eq!(finish_reason(StopReason::StopSequence, false), "stop");
        assert_eq!(finish_reason(StopReason::Unknown, false), "stop");
        assert_eq!(finish_reason(StopReason::Stop, true), "tool_calls");
    }

    #[test]
    fn empty_content_is_a_string_but_tool_only_replies_are_null() {
        let empty = NeutralResponse::default();
        let body = String::from_utf8(ChatEncoder.encode_response(&empty, "m", 0).unwrap()).unwrap();
        assert!(body.contains(r#""message":{"role":"assistant","content":""}"#));

        let tool_only = NeutralResponse {
            tool_calls: vec![tool("call_1", "Read", "{}")],
            ..NeutralResponse::default()
        };
        let body =
            String::from_utf8(ChatEncoder.encode_response(&tool_only, "m", 0).unwrap()).unwrap();
        assert!(body.contains(r#""content":null,"tool_calls":"#));
    }

    // --- SSE (spec 40 §3) ---

    #[test]
    fn sse_text_stream_is_byte_exact() {
        let mut state = SseState::new("gpt-5.6-luna", 1_762_300_000);
        let mut frames = Vec::new();
        for event in [
            StreamEvent::Start { id: None },
            StreamEvent::BlockStart {
                index: 0,
                kind: BlockKind::Text,
            },
            StreamEvent::TextDelta {
                delta: "O".to_string(),
            },
            StreamEvent::TextDelta {
                delta: "K".to_string(),
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::End {
                reason: StopReason::Stop,
            },
        ] {
            if let Some(frame) = ChatEncoder.encode_event(&event, &mut state).unwrap() {
                frames.extend(frame_texts(&frame));
            }
        }
        let tail = ChatEncoder.finish(&mut state).unwrap();
        frames.extend(frame_texts(&tail));
        let expected = [
            r#"data: {"id":"chatcmpl-zed","object":"chat.completion.chunk","created":1762300000,"model":"gpt-5.6-luna","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
            r#"data: {"id":"chatcmpl-zed","object":"chat.completion.chunk","created":1762300000,"model":"gpt-5.6-luna","choices":[{"index":0,"delta":{"content":"O"},"finish_reason":null}]}"#,
            r#"data: {"id":"chatcmpl-zed","object":"chat.completion.chunk","created":1762300000,"model":"gpt-5.6-luna","choices":[{"index":0,"delta":{"content":"K"},"finish_reason":null}]}"#,
            r#"data: {"id":"chatcmpl-zed","object":"chat.completion.chunk","created":1762300000,"model":"gpt-5.6-luna","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ];
        assert_eq!(frames, expected);
    }

    #[test]
    fn sse_tool_stream_uses_indices_and_synthesises_empty_arguments() {
        let mut state = SseState::new("m", 0);
        let mut frames = Vec::new();
        for event in [
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
                delta: "\"a\"}".to_string(),
            },
            StreamEvent::ToolEnd { index: 0 },
            StreamEvent::ToolStart {
                index: 1,
                id: "call_2".to_string(),
                name: "Bash".to_string(),
                custom: false,
            },
            StreamEvent::ToolEnd { index: 1 },
            StreamEvent::End {
                reason: StopReason::Stop,
            },
        ] {
            if let Some(frame) = ChatEncoder.encode_event(&event, &mut state).unwrap() {
                frames.extend(frame_texts(&frame));
            }
        }
        let tail = ChatEncoder.finish(&mut state).unwrap();
        frames.extend(frame_texts(&tail));
        let deltas: Vec<&str> = frames
            .iter()
            .filter(|frame| frame.starts_with("data: {"))
            .map(|frame| {
                let body = data_of(frame);
                let start = body.find("\"delta\":").unwrap() + "\"delta\":".len();
                &body[start..body.rfind(",\"finish_reason\"").unwrap()]
            })
            .collect();
        assert_eq!(
            deltas,
            vec![
                r#"{"role":"assistant"}"#,
                r#"{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Read","arguments":""}}]}"#,
                r#"{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}"#,
                r#"{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]}"#,
                r#"{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"Bash","arguments":""}}]}"#,
                r#"{"tool_calls":[{"index":1,"function":{"arguments":"{}"}}]}"#,
                "{}",
            ]
        );
        assert_eq!(frames.last().unwrap(), "data: [DONE]");
        let end: Value = serde_json::from_str(data_of(&frames[frames.len() - 2])).unwrap();
        assert_eq!(end["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn sse_usage_chunk_only_when_requested_and_after_the_end_chunk() {
        for include_usage in [false, true] {
            let mut state = SseState::new("m", 7);
            state.include_usage = include_usage;
            let mut frames = Vec::new();
            for event in [
                StreamEvent::Usage(usage()),
                StreamEvent::End {
                    reason: StopReason::Length,
                },
            ] {
                if let Some(frame) = ChatEncoder.encode_event(&event, &mut state).unwrap() {
                    frames.extend(frame_texts(&frame));
                }
            }
            let tail = ChatEncoder.finish(&mut state).unwrap();
            frames.extend(frame_texts(&tail));
            assert_eq!(frames.last().unwrap(), "data: [DONE]");
            let bodies = &frames[..frames.len() - 1];
            if include_usage {
                assert_eq!(bodies.len(), 2);
                assert_eq!(
                    bodies[1],
                    r#"data: {"id":"chatcmpl-zed","object":"chat.completion.chunk","created":7,"model":"m","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}}"#
                );
            } else {
                assert_eq!(bodies.len(), 1);
                assert!(bodies[0].contains(r#""finish_reason":"length""#));
            }
        }
    }

    #[test]
    fn sse_empty_stream_still_emits_role_end_and_done() {
        let mut state = SseState::new("m", 0);
        let frames = frame_texts(&ChatEncoder.finish(&mut state).unwrap());
        assert_eq!(frames.len(), 2);
        assert!(frames[0].contains(r#""delta":{},"finish_reason":"stop""#));
        assert_eq!(frames[1], "data: [DONE]");
    }

    #[test]
    fn sse_error_emits_an_error_frame_then_done_without_an_end_chunk() {
        let mut state = SseState::new("m", 0);
        let err = GatewayError::request("bad upstream");
        let frame = ChatEncoder
            .encode_event(&StreamEvent::Error(err), &mut state)
            .unwrap()
            .unwrap();
        assert_eq!(
            String::from_utf8(frame).unwrap(),
            "data: {\"error\":{\"message\":\"bad upstream\",\"type\":\"invalid_request_error\"}}\n\n"
        );
        let tail = String::from_utf8(ChatEncoder.finish(&mut state).unwrap()).unwrap();
        assert_eq!(tail, "data: [DONE]\n\n");
    }

    #[test]
    fn sse_reasoning_content_is_the_only_thinking_channel() {
        let mut state = SseState::new("m", 0);
        let frame = ChatEncoder
            .encode_event(
                &StreamEvent::ThinkingDelta {
                    delta: "hmm".to_string(),
                },
                &mut state,
            )
            .unwrap()
            .unwrap();
        let frames = frame_texts(&frame);
        assert!(frames[0].contains(r#""delta":{"role":"assistant"}"#));
        assert!(frames[1].contains(r#""delta":{"reasoning_content":"hmm"}"#));
        assert!(!frames[1].contains("\"thinking\""));
    }
}
