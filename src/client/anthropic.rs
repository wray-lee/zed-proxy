//! Anthropic Messages codec (spec `41-client-anthropic-messages.md`).
//!
//! Request decoding is spec 41 §1, the non-streaming `message` object §2 and
//! the SSE event sequence §3. JSON is built by hand because the key order is
//! normative.
//!
//! Anthropic needs per-block bookkeeping that `SseState` has no dedicated
//! fields for, and the state struct is shared/frozen, so this codec reuses the
//! unused-for-Anthropic fields (documented once here):
//! - `sequence`: flags (`START_SENT` / `END_SENT` / `TERMINAL`).
//! - `open_block`: client block index currently open, if any.
//! - `next_block_index`: next client block index to assign.
//! - `tool_arguments`: one kind tag (`"thinking"` / `"text"` / `"tool"`) per
//!   opened client block, indexed by block index.
//! - `text`: arguments accumulated for the open tool block (to detect "no
//!   arguments at all" and synthesise `{}`).
//! - `tool_count`: number of `tool_use` blocks opened.

use serde_json::{Map, Value};

use crate::catalog::{Catalog, Provider};
use crate::error::GatewayError;
#[cfg(test)]
use crate::neutral::content::signed_tool_start_id;
use crate::neutral::content::{
    decode_tool_output, first_signature, split_signed_tool_start_id, tool_result_content_block,
    tool_result_text,
};
use crate::neutral::{
    BlockKind, ContentBlock, ImageSource, Message, NeutralRequest, NeutralResponse, Reasoning,
    Role, StopReason, StreamEvent, SystemBlock, ToolChoice, ToolDef, ToolResult, ToolUse, Usage,
};
use crate::proxy::stream::{sse_event, SseState};

use super::{tool_call_id, ClientDecoder, ClientEncoder};

/// Fixed non-streaming object id (spec 41 §2.1).
pub const MESSAGE_ID: &str = "msg_zed";

const EMPTY_SCHEMA: &str = r#"{"type":"object","properties":{}}"#;

const START_SENT: u64 = 1;
const END_SENT: u64 = 2;
const TERMINAL: u64 = 3;

fn catalog() -> &'static Catalog {
    static CATALOG: std::sync::OnceLock<Catalog> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| Catalog::builtin().expect("built-in catalog parses"))
}

fn warn(what: &str) {
    eprintln!("[zed-proxy] anthropic: {what}");
}

fn jstr(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn positive_int(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_i64).filter(|value| *value > 0)
}

/// Text blocks of a `content` value joined with `\n\n` (mirrors spec 41 §1.3).
fn content_text(value: Option<&Value>) -> String {
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

fn decode_image(source: Option<&Value>) -> Option<ImageSource> {
    let source = source?.as_object()?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media_type = source.get("media_type").and_then(Value::as_str)?;
            let data = source.get("data").and_then(Value::as_str)?;
            if media_type.is_empty() || data.is_empty() {
                return None;
            }
            Some(ImageSource::Data {
                media_type: media_type.to_string(),
                data: data.to_string(),
            })
        }
        Some("url") => source
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(|url| ImageSource::Url(url.to_string())),
        _ => None,
    }
}

fn decode_block(object: &Map<String, Value>, structured: bool) -> Vec<ContentBlock> {
    match object.get("type").and_then(Value::as_str) {
        Some("text") => object
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| vec![ContentBlock::Text(text.to_string())])
            .unwrap_or_default(),
        Some("image") => match decode_image(object.get("source")) {
            Some(source) => vec![ContentBlock::Image(source)],
            None => {
                warn("dropped image with unsupported source");
                Vec::new()
            }
        },
        Some("tool_use") => {
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name.is_empty() {
                warn("dropped tool_use without name");
                return Vec::new();
            }
            let arguments = match object.get("input") {
                Some(value @ Value::Object(_)) => {
                    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
                }
                _ => "{}".to_string(),
            };
            vec![ContentBlock::ToolUse(ToolUse {
                id: tool_call_id(object.get("id").and_then(Value::as_str)),
                name: name.to_string(),
                arguments,
                signature: first_signature(
                    object,
                    &[
                        &["thought_signature"],
                        &["thoughtSignature"],
                        &["extra_content", "google", "thought_signature"],
                    ],
                ),
                custom: false,
            })]
        }
        Some("tool_result") => decode_tool_result_block(object, structured),
        // History thinking blocks are dropped on purpose (spec 41 §1.3): the
        // upstream route never returns a signature we could replay, and a
        // signature-less block makes the whole request fail.
        Some("thinking") | Some("redacted_thinking") => Vec::new(),
        Some("compaction") => object
            .get("text")
            .and_then(Value::as_str)
            .map(|text| vec![ContentBlock::Compaction(text.to_string())])
            .unwrap_or_default(),
        other => {
            warn(&format!("dropped content block {:?}", other.unwrap_or("?")));
            Vec::new()
        }
    }
}

fn decode_tool_result_block(object: &Map<String, Value>, structured: bool) -> Vec<ContentBlock> {
    let id = object
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if id.is_empty() {
        warn("dropped tool_result without tool_use_id");
        return Vec::new();
    }
    let blocks = decode_tool_output(object.get("content"));
    let mut parts = vec![ContentBlock::ToolResult(ToolResult {
        tool_use_id: id.to_string(),
        output: tool_result_text(&blocks),
        is_error: object
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        custom: false,
    })];
    if structured
        && blocks.iter().any(|block| {
            matches!(
                block.get("type").and_then(Value::as_str),
                Some("image" | "json")
            )
        })
    {
        parts.push(tool_result_content_block(id, blocks));
    }
    parts
}

fn decode_content(value: Option<&Value>, structured: bool) -> Vec<ContentBlock> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::Text(text.clone())]
            }
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                item.as_object()
                    .map(|object| decode_block(object, structured))
            })
            .flatten()
            .collect(),
        Some(_) => {
            warn("ignored non-string, non-array content");
            Vec::new()
        }
    }
}

fn decode_system(value: Option<&Value>) -> Vec<SystemBlock> {
    match value {
        Some(Value::String(text)) => vec![SystemBlock {
            text: text.clone(),
            cache_control: None,
        }],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                if object.get("type").and_then(Value::as_str) != Some("text") {
                    warn("dropped non-text system block");
                    return None;
                }
                let text = object.get("text").and_then(Value::as_str)?.to_string();
                Some(SystemBlock {
                    text,
                    cache_control: object.get("cache_control").cloned(),
                })
            })
            .collect(),
        _ => Vec::new(),
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
        if object.get("type").is_some() {
            warn("dropped server-side tool declaration");
            continue;
        }
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if name.is_empty() {
            warn("dropped tool declaration without name");
            continue;
        }
        let parameters = object
            .get("input_schema")
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
            cache_control: object.get("cache_control").cloned(),
            format: None,
            custom: false,
        });
    }
    tools
}

fn decode_tool_choice(value: Option<&Value>) -> ToolChoice {
    let Some(value) = value else {
        return ToolChoice::Unspecified;
    };
    let Some(object) = value.as_object() else {
        return ToolChoice::Auto;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("auto") => ToolChoice::Auto,
        Some("any") => ToolChoice::Required,
        Some("none") => ToolChoice::None,
        Some("tool") => match object.get("name").and_then(Value::as_str) {
            Some(name) if !name.is_empty() => ToolChoice::Named(name.to_string()),
            _ => {
                warn("tool_choice tool without name; using auto");
                ToolChoice::Auto
            }
        },
        _ => {
            warn("unknown tool_choice; using auto");
            ToolChoice::Auto
        }
    }
}

fn decode_stop(value: Option<&Value>) -> Vec<String> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(Value::as_str)
        .filter(|text| !text.trim_matches([' ', '\t', '\r', '\n']).is_empty())
        .map(str::to_string)
        .collect()
}

pub struct AnthropicDecoder;

impl ClientDecoder for AnthropicDecoder {
    fn decode(&self, body: &serde_json::Value) -> Result<NeutralRequest, GatewayError> {
        let catalog = catalog();
        let model = catalog.normalize_model(body.get("model").and_then(Value::as_str));
        let provider = catalog.resolve_provider(&model)?;

        let mut system = decode_system(body.get("system"));
        // The string shorthand survives only when nothing is appended to it
        // (spec 21 §4); a message-level system turn forces the array form.
        let top_level_string = matches!(body.get("system"), Some(Value::String(_)));
        let top_level_blocks = system.len();
        let mut messages: Vec<Message> = Vec::new();
        let structured = provider == Provider::Google;
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
                match role {
                    // Claude Code appends system/developer turns; they become
                    // extra system blocks without cache_control (spec 41 §1.2).
                    "system" | "developer" => {
                        let text = content_text(object.get("content"));
                        if !text.is_empty() {
                            system.push(SystemBlock {
                                text,
                                cache_control: None,
                            });
                        }
                    }
                    "user" => messages.push(Message {
                        role: Role::User,
                        parts: decode_content(object.get("content"), structured),
                    }),
                    "assistant" => messages.push(Message {
                        role: Role::Assistant,
                        parts: decode_content(object.get("content"), structured),
                    }),
                    other => warn(&format!("dropped message with role {other:?}")),
                }
            }
        }

        let tools = decode_tools(body.get("tools"));
        let tool_choice = decode_tool_choice(body.get("tool_choice"));
        let disable_parallel = body
            .get("tool_choice")
            .and_then(Value::as_object)
            .and_then(|choice| choice.get("disable_parallel_tool_use"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut tool_uses: Vec<ToolUse> = Vec::new();
        for message in &messages {
            for part in &message.parts {
                if let ContentBlock::ToolUse(call) = part {
                    tool_uses.push(call.clone());
                }
            }
        }
        let unpaired_tool_results = messages
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

        if body.get("metadata").is_some() {
            warn("ignored metadata");
        }

        let system_string = top_level_string && system.len() == top_level_blocks;
        Ok(NeutralRequest {
            model,
            provider,
            system,
            system_string,
            messages,
            tools,
            tool_choice,
            parallel_tool_calls: Some(!disable_parallel),
            reasoning: Reasoning {
                effort: body
                    .get("output_config")
                    .and_then(Value::as_object)
                    .and_then(|config| config.get("effort"))
                    .and_then(Value::as_str)
                    .map(str::to_lowercase),
                extra: None,
            },
            thinking: body.get("thinking").cloned(),
            output_config: body.get("output_config").cloned(),
            context_management: body.get("context_management").cloned(),
            max_output_tokens: positive_int(body.get("max_tokens")),
            temperature: body.get("temperature").and_then(Value::as_f64),
            top_p: body.get("top_p").and_then(Value::as_f64),
            top_k: body.get("top_k").and_then(Value::as_i64),
            stop: decode_stop(body.get("stop_sequences")),
            stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
            tool_uses,
            unpaired_tool_results,
            ..NeutralRequest::default()
        })
    }
}
/// Anthropic `stop_reason` (spec 41 §2.3); tool calls win over every other
/// reason.
fn stop_reason(reason: StopReason, has_tools: bool) -> &'static str {
    if has_tools {
        return "tool_use";
    }
    match reason {
        StopReason::Length => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ContentFilter => "refusal",
        _ => "end_turn",
    }
}

fn usage_json(usage: Usage) -> String {
    format!(
        "{{\"input_tokens\":{},\"output_tokens\":{}}}",
        usage.input_tokens, usage.output_tokens
    )
}

fn tool_input(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(value @ Value::Object(_)) => {
            serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
        }
        _ => "{}".to_string(),
    }
}

/// Non-streaming `content` array; always at least one block (spec 41 §2.2).
fn content_json(response: &NeutralResponse) -> String {
    let mut blocks: Vec<String> = Vec::new();
    if !response.thinking.is_empty() {
        blocks.push(format!(
            "{{\"type\":\"thinking\",\"thinking\":{}}}",
            jstr(&response.thinking)
        ));
    }
    if !response.text.is_empty() {
        blocks.push(format!(
            "{{\"type\":\"text\",\"text\":{}}}",
            jstr(&response.text)
        ));
    }
    for call in &response.tool_calls {
        let signature = call
            .signature
            .as_deref()
            .map(|value| format!(",\"thought_signature\":{}", jstr(value)))
            .unwrap_or_default();
        blocks.push(format!(
            "{{\"type\":\"tool_use\",\"id\":{}{signature},\"name\":{},\"input\":{}}}",
            jstr(&call.id),
            jstr(&call.name),
            tool_input(&call.arguments)
        ));
    }
    if blocks.is_empty() {
        blocks.push("{\"type\":\"text\",\"text\":\"\"}".to_string());
    }
    format!("[{}]", blocks.join(","))
}

pub struct AnthropicEncoder;

impl ClientEncoder for AnthropicEncoder {
    fn encode_response(
        &self,
        response: &NeutralResponse,
        model: &str,
        _now_unix: i64,
    ) -> Result<Vec<u8>, GatewayError> {
        let reason = stop_reason(response.stop_reason, !response.tool_calls.is_empty());
        Ok(format!(
            "{{\"id\":\"{MESSAGE_ID}\",\"type\":\"message\",\"role\":\"assistant\",\"model\":{},\"content\":{},\"stop_reason\":\"{reason}\",\"stop_sequence\":null,\"usage\":{}}}",
            jstr(model),
            content_json(response),
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
            // `message_start` is not sent here: holding it until the first
            // content event lets a `usage` event that carries the input count
            // land in `message_start.usage.input_tokens` (spec 41 §3.2).
            StreamEvent::Start { id } => {
                state.response_id = id.clone();
            }
            StreamEvent::Usage(usage) => {
                state.usage = *usage;
                if state.sequence != START_SENT {
                    ensure_start(state, &mut out);
                }
            }
            StreamEvent::BlockStart { index, kind } => {
                if *kind == BlockKind::ToolUse {
                    // Tool blocks are opened by `ToolStart` (neutral contract,
                    // spec 45 §1.2).
                    return Ok(None);
                }
                ensure_start(state, &mut out);
                close_open_block(state, &mut out);
                open_block(state, &mut out, *index, *kind);
            }
            StreamEvent::TextDelta { delta } => {
                ensure_start(state, &mut out);
                ensure_block(state, &mut out, BlockKind::Text);
                let index = state.open_block.unwrap_or_default();
                out.extend(sse_frame(
                    "content_block_delta",
                    &format!(
                        "{{\"type\":\"content_block_delta\",\"index\":{index},\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}",
                        jstr(delta)
                    ),
                ));
            }
            StreamEvent::ThinkingDelta { delta } => {
                ensure_start(state, &mut out);
                ensure_block(state, &mut out, BlockKind::Thinking);
                let index = state.open_block.unwrap_or_default();
                out.extend(sse_frame(
                    "content_block_delta",
                    &format!(
                        "{{\"type\":\"content_block_delta\",\"index\":{index},\"delta\":{{\"type\":\"thinking_delta\",\"thinking\":{}}}}}",
                        jstr(delta)
                    ),
                ));
            }
            StreamEvent::ToolStart {
                index,
                id,
                name,
                custom,
            } => {
                let (id, signature) = split_signed_tool_start_id(id);
                let signature = signature
                    .map(|value| format!(",\"thought_signature\":{}", jstr(value)))
                    .unwrap_or_default();
                ensure_start(state, &mut out);
                if !open_block_is_tool(state) {
                    close_open_block(state, &mut out);
                }
                let client_index = state.next_block_index;
                state.next_block_index += 1;
                if !state.ensure_tool(*index, client_index, *custom) {
                    warn("dropped tool start with out-of-range index");
                    return Ok(None);
                }
                state.set_tool_argument(client_index, format!("{index}:tool"));
                state.open_block = Some(client_index);
                state.tool_count += 1;
                out.extend(sse_frame(
                    "content_block_start",
                    &format!(
                        "{{\"type\":\"content_block_start\",\"index\":{client_index},\"content_block\":{{\"type\":\"tool_use\",\"id\":{}{signature},\"name\":{},\"input\":{{}}}}}}",
                        jstr(id),
                        jstr(name)
                    ),
                ));
            }
            StreamEvent::ToolArgsDelta { index, delta } => {
                let Some(client_index) = state.tool_slot(*index) else {
                    warn("dropped tool arguments without an open tool index");
                    return Ok(None);
                };
                if let Some(args) = state.tool_args.get_mut(*index as usize) {
                    args.push_str(delta);
                }
                out.extend(sse_frame(
                    "content_block_delta",
                    &format!(
                        "{{\"type\":\"content_block_delta\",\"index\":{client_index},\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{}}}}}",
                        jstr(delta)
                    ),
                ));
            }
            StreamEvent::ToolEnd { index } => {
                let Some(client_index) = state.tool_slot(*index) else {
                    warn("dropped tool end without an open tool index");
                    return Ok(None);
                };
                if !state
                    .tool_custom
                    .get(*index as usize)
                    .copied()
                    .unwrap_or(false)
                    && state
                        .tool_args
                        .get(*index as usize)
                        .is_some_and(String::is_empty)
                {
                    out.extend(sse_frame(
                        "content_block_delta",
                        &format!(
                            "{{\"type\":\"content_block_delta\",\"index\":{client_index},\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{}}\"}}}}"
                        ),
                    ));
                }
                out.extend(sse_frame(
                    "content_block_stop",
                    &format!("{{\"type\":\"content_block_stop\",\"index\":{client_index}}}"),
                ));
                if state.open_block == Some(client_index) {
                    state.open_block = None;
                }
                state.close_tool(*index);
            }
            StreamEvent::BlockStop { index } => {
                if open_block_neutral_index(state) == Some(*index) && !open_block_is_tool(state) {
                    close_open_block(state, &mut out);
                }
            }
            StreamEvent::End { reason } => {
                state.stop_reason = *reason;
                ensure_start(state, &mut out);
                finish_sequence(state, &mut out);
            }
            StreamEvent::Error(err) => {
                state.sequence = TERMINAL;
                out.extend(sse_frame(
                    "error",
                    &String::from_utf8_lossy(&crate::error::anthropic_error_body(err)),
                ));
            }
        }
        Ok(if out.is_empty() { None } else { Some(out) })
    }

    fn finish(&self, state: &mut SseState) -> Result<Vec<u8>, GatewayError> {
        if state.sequence == TERMINAL {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        ensure_start(state, &mut out);
        if state.sequence != END_SENT {
            finish_sequence(state, &mut out);
        }
        Ok(out)
    }
}

fn sse_frame(event: &str, data: &str) -> Vec<u8> {
    sse_event(event, data.as_bytes())
}

/// `message_start` is required before any block (spec 41 §3.2 item 1).
fn ensure_start(state: &mut SseState, out: &mut Vec<u8>) {
    if state.sequence >= START_SENT {
        return;
    }
    state.sequence = START_SENT;
    out.extend(sse_frame(
        "message_start",
        &format!(
            "{{\"type\":\"message_start\",\"message\":{{\"id\":\"{MESSAGE_ID}\",\"type\":\"message\",\"role\":\"assistant\",\"model\":{},\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\"input_tokens\":{},\"output_tokens\":0}}}}}}",
            jstr(&state.model),
            state.usage.input_tokens
        ),
    ));
}

fn open_block(state: &mut SseState, out: &mut Vec<u8>, neutral_index: u64, kind: BlockKind) {
    let client_index = state.next_block_index;
    state.next_block_index += 1;
    state.open_block = Some(client_index);
    let tag = match kind {
        BlockKind::Thinking => "thinking",
        BlockKind::Text => "text",
        BlockKind::ToolUse => "tool",
    };
    state.tool_arguments.push(format!("{neutral_index}:{tag}"));
    // Tool blocks never come through here: `ToolStart` opens them itself.
    let content_block = match kind {
        BlockKind::Thinking => "{\"type\":\"thinking\",\"thinking\":\"\"}",
        BlockKind::Text | BlockKind::ToolUse => "{\"type\":\"text\",\"text\":\"\"}",
    };
    out.extend(sse_frame(
        "content_block_start",
        &format!(
            "{{\"type\":\"content_block_start\",\"index\":{client_index},\"content_block\":{content_block}}}"
        ),
    ));
}

/// Open a block of `kind` unless the open block already has that kind.
/// Interleaved upstream deltas close the current block and open a new one
/// (spec 41 §3.2); every new block gets the next index.
fn ensure_block(state: &mut SseState, out: &mut Vec<u8>, kind: BlockKind) {
    let wanted = match kind {
        BlockKind::Thinking => "thinking",
        BlockKind::Text => "text",
        BlockKind::ToolUse => "tool",
    };
    let already = state
        .open_block
        .and_then(|open| state.tool_arguments.get(open as usize))
        .is_some_and(|tag| tag.ends_with(wanted));
    if already {
        return;
    }
    close_open_block(state, out);
    let index = state.next_block_index;
    open_block(state, out, index, kind);
}

fn open_block_is_tool(state: &SseState) -> bool {
    state
        .open_block
        .and_then(|open| state.tool_arguments.get(open as usize))
        .is_some_and(|tag| tag.ends_with(":tool"))
}

fn open_block_neutral_index(state: &SseState) -> Option<u64> {
    state
        .open_block
        .and_then(|open| state.tool_arguments.get(open as usize))
        .and_then(|tag| tag.split(':').next())
        .and_then(|index| index.parse().ok())
}

fn close_open_block(state: &mut SseState, out: &mut Vec<u8>) {
    if let Some(index) = state.open_block.take() {
        out.extend(sse_frame(
            "content_block_stop",
            &format!("{{\"type\":\"content_block_stop\",\"index\":{index}}}"),
        ));
    }
}

/// Close the open block, guarantee at least one block, then emit
/// `message_delta` + `message_stop` (spec 41 §3.2 item 3, §3.4).
fn finish_sequence(state: &mut SseState, out: &mut Vec<u8>) {
    for index in state.open_tool_indices() {
        let client_index = state.tool_blocks[index as usize];
        out.extend(sse_frame(
            "content_block_stop",
            &format!("{{\"type\":\"content_block_stop\",\"index\":{client_index}}}"),
        ));
        state.close_tool(index);
    }
    if !open_block_is_tool(state) {
        close_open_block(state, out);
    }
    if state.next_block_index == 0 {
        open_block(state, out, u64::MAX, BlockKind::Text);
        close_open_block(state, out);
    }
    let reason = stop_reason(state.stop_reason, state.tool_count > 0);
    out.extend(sse_frame(
        "message_delta",
        &format!(
            "{{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{reason}\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":{}}}}}",
            state.usage.output_tokens
        ),
    ));
    out.extend(sse_frame("message_stop", "{\"type\":\"message_stop\"}"));
    state.sequence = END_SENT;
}

#[cfg(test)]
// The provider is a request-scoped const value; tests call it directly.
#[allow(clippy::borrow_interior_mutable_const)]
mod tests {
    use super::*;

    fn decode(body: Value) -> NeutralRequest {
        AnthropicDecoder.decode(&body).unwrap()
    }

    fn frames_of(bytes: &[u8]) -> Vec<String> {
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .split("\n\n")
            .filter(|frame| !frame.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn tool_use(id: &str, name: &str, arguments: &str) -> ToolUse {
        ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
            signature: None,
            custom: false,
        }
    }

    // --- request decoding (spec 41 §1) ---

    #[test]
    fn system_blocks_keep_cache_control_and_drop_non_text() {
        let request = decode(serde_json::json!({
            "system": [
                {"type": "text", "text": "base"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA"}},
                {"type": "text", "text": "style", "cache_control": {"type": "ephemeral"}}
            ]
        }));
        assert_eq!(
            request.system,
            vec![
                SystemBlock {
                    text: "base".to_string(),
                    cache_control: None
                },
                SystemBlock {
                    text: "style".to_string(),
                    cache_control: Some(serde_json::json!({"type": "ephemeral"}))
                },
            ]
        );
    }

    #[test]
    fn trailing_system_messages_append_without_cache_control() {
        let request = decode(serde_json::json!({
            "system": "top",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "system", "content": [{"type": "text", "text": "tail"}]}
            ]
        }));
        assert_eq!(
            request.system,
            vec![
                SystemBlock {
                    text: "top".to_string(),
                    cache_control: None
                },
                SystemBlock {
                    text: "tail".to_string(),
                    cache_control: None
                },
            ]
        );
        assert_eq!(request.messages.len(), 1);
    }

    #[test]
    fn content_blocks_decode_images_tools_results_and_compaction() {
        let request = decode(serde_json::json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "look", "citations": []},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "QUJD"}},
                    {"type": "image", "source": {"type": "url", "url": "https://example.test/a.png"}},
                    {"type": "image", "source": {"type": "file", "file_id": "f"}},
                    {"type": "document", "source": {"type": "text"}}
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "redacted_thinking", "data": "x"},
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "a"}},
                    {"type": "tool_use", "name": "NoId"},
                    {"type": "tool_use", "id": "toolu_2"}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "A"},
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": [
                        {"type": "text", "text": "B"},
                        {"type": "image", "source": {"type": "url", "url": "https://x.test/b.png"}}
                    ], "is_error": true},
                    {"type": "compaction", "text": "summary"}
                ]}
            ]
        }));
        let user = &request.messages[0].parts;
        assert_eq!(user.len(), 3);
        assert_eq!(user[0], ContentBlock::Text("look".into()));
        assert_eq!(
            user[1],
            ContentBlock::Image(ImageSource::Data {
                media_type: "image/jpeg".to_string(),
                data: "QUJD".to_string()
            })
        );
        assert_eq!(
            user[2],
            ContentBlock::Image(ImageSource::Url("https://example.test/a.png".to_string()))
        );

        let assistant = &request.messages[1].parts;
        assert_eq!(assistant.len(), 2);
        assert_eq!(
            assistant[0],
            ContentBlock::ToolUse(tool_use("toolu_1", "Read", "{\"path\":\"a\"}"))
        );
        let generated = match &assistant[1] {
            ContentBlock::ToolUse(call) => call,
            other => panic!("expected tool_use, got {other:?}"),
        };
        assert_eq!(generated.id.len(), 21);
        assert!(generated.id.starts_with("call_"));
        assert_eq!(generated.arguments, "{}");

        let results = &request.messages[2].parts;
        assert_eq!(
            results,
            &vec![
                ContentBlock::ToolResult(ToolResult {
                    tool_use_id: "toolu_1".to_string(),
                    output: "A".to_string(),
                    is_error: false,
                    custom: false,
                }),
                ContentBlock::ToolResult(ToolResult {
                    tool_use_id: "toolu_2".to_string(),
                    output: "B".to_string(),
                    is_error: true,
                    custom: false,
                }),
                ContentBlock::Compaction("summary".to_string()),
            ]
        );
        assert_eq!(request.tool_uses.len(), 2);
    }

    #[test]
    fn tools_normalise_description_and_schema_and_drop_nameless() {
        let request = decode(serde_json::json!({
            "tools": [
                {"name": "Read"},
                {"name": "Bash", "description": null, "input_schema": 7},
                {"name": "", "description": "x"},
                {"type": "web_search_20250305", "name": "web_search"}
            ]
        }));
        assert_eq!(request.tools.len(), 2);
        assert_eq!(request.tools[0].description, "");
        assert_eq!(
            request.tools[0].parameters,
            serde_json::json!({"type": "object", "properties": {}})
        );
        assert_eq!(request.tools[1].description, "");
    }

    #[test]
    fn tool_choice_and_parallel_flag() {
        for (choice, expected) in [
            (serde_json::json!({"type": "auto"}), ToolChoice::Auto),
            (serde_json::json!({"type": "any"}), ToolChoice::Required),
            (serde_json::json!({"type": "none"}), ToolChoice::None),
            (
                serde_json::json!({"type": "tool", "name": "Read"}),
                ToolChoice::Named("Read".into()),
            ),
        ] {
            let request = decode(serde_json::json!({"tool_choice": choice}));
            assert_eq!(request.tool_choice, expected);
            assert_eq!(request.parallel_tool_calls, Some(true));
        }
        let request = decode(serde_json::json!({
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true}
        }));
        assert_eq!(request.parallel_tool_calls, Some(false));
    }

    #[test]
    fn budgets_sampling_thinking_and_stop_decode() {
        let request = decode(serde_json::json!({
            "max_tokens": 4096,
            "temperature": 0.2,
            "top_p": 0.9,
            "top_k": 20,
            "stop_sequences": ["END", " ", "\t\n", "STOP"],
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "output_config": {"effort": "HIGH"},
            "context_management": {"edits": []}
        }));
        assert_eq!(request.max_output_tokens, Some(4096));
        assert_eq!(request.temperature, Some(0.2));
        assert_eq!(request.top_p, Some(0.9));
        assert_eq!(request.top_k, Some(20));
        assert_eq!(request.stop, vec!["END", "STOP"]);
        assert_eq!(
            request.thinking,
            Some(serde_json::json!({"type": "enabled", "budget_tokens": 2048}))
        );
        assert_eq!(
            request.output_config,
            Some(serde_json::json!({"effort": "HIGH"}))
        );
        assert_eq!(
            request.context_management,
            Some(serde_json::json!({"edits": []}))
        );
        assert_eq!(request.reasoning.effort.as_deref(), Some("high"));
    }

    #[test]
    fn invalid_max_tokens_decodes_as_missing_for_the_provider_default() {
        for value in [
            serde_json::json!(0),
            serde_json::json!(-3),
            serde_json::json!("big"),
        ] {
            let request = decode(serde_json::json!({"max_tokens": value}));
            assert_eq!(request.max_output_tokens, None);
        }
        let request = decode(serde_json::json!({"max_tokens": 12.5}));
        assert_eq!(request.max_output_tokens, None);
    }

    #[test]
    fn empty_messages_stay_empty_instead_of_erroring() {
        let request = decode(serde_json::json!({"messages": []}));
        assert!(request.messages.is_empty());
        let request = decode(serde_json::json!({}));
        assert!(request.messages.is_empty());
        assert_eq!(request.model, "claude-sonnet-5");
    }

    // --- non-streaming object (spec 41 §2) ---

    #[test]
    fn message_object_matches_the_spec_bytes() {
        let response = NeutralResponse {
            thinking: "先读文件".to_string(),
            text: "读到了。".to_string(),
            tool_calls: vec![tool_use("toolu_1", "Read", "{\"path\":\"a.txt\"}")],
            stop_reason: StopReason::ToolCalls,
            usage: Usage {
                input_tokens: 120,
                output_tokens: 36,
                ..Usage::default()
            },
            ..NeutralResponse::default()
        };
        let body = AnthropicEncoder
            .encode_response(&response, "claude-sonnet-5", 1_762_300_000)
            .unwrap();
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"id":"msg_zed","type":"message","role":"assistant","model":"claude-sonnet-5","content":[{"type":"thinking","thinking":"先读文件"},{"type":"text","text":"读到了。"},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"a.txt"}}],"stop_reason":"tool_use","stop_sequence":null,"usage":{"input_tokens":120,"output_tokens":36}}"#
        );
    }

    #[test]
    fn empty_reply_has_an_empty_text_block_and_tool_only_has_no_text() {
        let body = String::from_utf8(
            AnthropicEncoder
                .encode_response(&NeutralResponse::default(), "m", 0)
                .unwrap(),
        )
        .unwrap();
        assert!(body.contains(r#""content":[{"type":"text","text":""}],"stop_reason":"end_turn""#));

        let tool_only = NeutralResponse {
            tool_calls: vec![tool_use("toolu_1", "Read", "not json")],
            ..NeutralResponse::default()
        };
        let body = String::from_utf8(
            AnthropicEncoder
                .encode_response(&tool_only, "m", 0)
                .unwrap(),
        )
        .unwrap();
        assert!(body.contains(
            r#""content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}]"#
        ));
    }

    #[test]
    fn stop_reason_mapping_and_tool_precedence() {
        assert_eq!(stop_reason(StopReason::Length, false), "max_tokens");
        assert_eq!(
            stop_reason(StopReason::StopSequence, false),
            "stop_sequence"
        );
        assert_eq!(stop_reason(StopReason::ContentFilter, false), "refusal");
        assert_eq!(stop_reason(StopReason::Stop, false), "end_turn");
        assert_eq!(stop_reason(StopReason::Unknown, false), "end_turn");
        assert_eq!(stop_reason(StopReason::Stop, true), "tool_use");
    }

    // --- SSE (spec 41 §3) ---

    fn collect(events: Vec<StreamEvent>) -> (String, SseState) {
        let mut state = SseState::new("claude-sonnet-5", 1_762_300_000);
        let mut out = String::new();
        for event in events {
            if let Some(frame) = AnthropicEncoder.encode_event(&event, &mut state).unwrap() {
                out.push_str(&String::from_utf8(frame).unwrap());
            }
        }
        out.push_str(&String::from_utf8(AnthropicEncoder.finish(&mut state).unwrap()).unwrap());
        (out, state)
    }

    #[test]
    fn sse_text_stream_is_byte_exact() {
        let (out, _) = collect(vec![
            StreamEvent::Start { id: None },
            StreamEvent::Usage(Usage {
                input_tokens: 120,
                output_tokens: 0,
                ..Usage::default()
            }),
            StreamEvent::BlockStart {
                index: 0,
                kind: BlockKind::Text,
            },
            StreamEvent::TextDelta {
                delta: "片".to_string(),
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::Usage(Usage {
                input_tokens: 120,
                output_tokens: 36,
                ..Usage::default()
            }),
            StreamEvent::End {
                reason: StopReason::Stop,
            },
        ]);
        assert_eq!(
            out,
            concat!(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_zed\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":120,\"output_tokens\":0}}}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"片\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":36}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            )
        );
    }

    #[test]
    fn sse_thinking_then_text_then_tool_blocks_do_not_interleave() {
        let (out, _) = collect(vec![
            StreamEvent::Start { id: None },
            StreamEvent::BlockStart {
                index: 0,
                kind: BlockKind::Thinking,
            },
            StreamEvent::ThinkingDelta {
                delta: "想".to_string(),
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::BlockStart {
                index: 1,
                kind: BlockKind::Text,
            },
            StreamEvent::TextDelta {
                delta: "说".to_string(),
            },
            StreamEvent::BlockStop { index: 1 },
            StreamEvent::ToolStart {
                index: 0,
                id: "toolu_9".to_string(),
                name: "Bash".to_string(),
                custom: false,
            },
            StreamEvent::ToolArgsDelta {
                index: 0,
                delta: "{\"cmd\":".to_string(),
            },
            StreamEvent::ToolArgsDelta {
                index: 0,
                delta: "\"ls\"}".to_string(),
            },
            StreamEvent::ToolEnd { index: 0 },
            StreamEvent::End {
                reason: StopReason::Stop,
            },
        ]);
        let frames = frames_of(out.as_bytes());
        let names: Vec<&str> = frames
            .iter()
            .map(|frame| {
                frame
                    .strip_prefix("event: ")
                    .unwrap()
                    .split('\n')
                    .next()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert!(
            frames[1].contains(r#""index":0,"content_block":{"type":"thinking","thinking":""}"#)
        );
        assert!(frames[4].contains(r#""index":1,"content_block":{"type":"text","text":""}"#));
        assert!(frames[7].contains(
            r#""index":2,"content_block":{"type":"tool_use","id":"toolu_9","name":"Bash","input":{}}"#
        ));
        assert!(frames[8].contains(r#""type":"input_json_delta","partial_json":"{\"cmd\":"#));
        assert!(frames[11].contains(r#""stop_reason":"tool_use""#));
    }

    #[test]
    fn sse_tool_without_arguments_gets_a_brace_pair_delta() {
        let (out, _) = collect(vec![
            StreamEvent::ToolStart {
                index: 0,
                id: "toolu_1".to_string(),
                name: "Read".to_string(),
                custom: false,
            },
            StreamEvent::ToolEnd { index: 0 },
            StreamEvent::End {
                reason: StopReason::ToolCalls,
            },
        ]);
        assert!(out.contains(
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#
        ));
    }

    #[test]
    fn sse_empty_reply_still_emits_a_text_block() {
        let (out, _) = collect(vec![StreamEvent::End {
            reason: StopReason::Stop,
        }]);
        let frames = frames_of(out.as_bytes());
        let names: Vec<&str> = frames
            .iter()
            .map(|frame| {
                frame
                    .strip_prefix("event: ")
                    .unwrap()
                    .split('\n')
                    .next()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert!(frames[1].contains(r#"{"type":"text","text":""}"#));
        // Unknown output tokens are reported as a real zero, not one.
        assert!(frames[3].contains(r#""usage":{"output_tokens":0}"#));
    }

    #[test]
    fn sse_finish_without_end_event_still_closes_the_sequence() {
        let mut state = SseState::new("m", 0);
        let mut out = Vec::new();
        for event in [
            // Usage before the first content event is what makes it into
            // `message_start.usage.input_tokens` (spec 41 §3.2 item 1).
            StreamEvent::Usage(Usage {
                input_tokens: 5,
                output_tokens: 6,
                ..Usage::default()
            }),
            StreamEvent::TextDelta {
                delta: "x".to_string(),
            },
        ] {
            if let Some(frame) = AnthropicEncoder.encode_event(&event, &mut state).unwrap() {
                out.extend(frame);
            }
        }
        out.extend(AnthropicEncoder.finish(&mut state).unwrap());
        let frames = frames_of(&out);
        assert_eq!(frames.len(), 6);
        assert!(frames[0].contains(r#""input_tokens":5"#));
        assert!(frames[5].starts_with("event: message_stop"));
        // Calling finish twice must not duplicate the terminator.
        assert!(AnthropicEncoder.finish(&mut state).unwrap().is_empty());
    }

    #[test]
    fn signature_and_image_tool_result_round_trip() {
        let signed = decode(serde_json::json!({
            "model": "gemini-4-argon",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}, "thought_signature": "sig-1"}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}}
                    ]}
                ]}
            ]
        }));
        let call = match &signed.messages[0].parts[0] {
            ContentBlock::ToolUse(call) => call,
            other => panic!("expected tool use, got {other:?}"),
        };
        assert_eq!(call.signature.as_deref(), Some("sig-1"));
        let mut response = NeutralResponse::default();
        response.tool_calls.push(call.clone());
        let json = String::from_utf8(AnthropicEncoder.encode_response(&response, "m", 0).unwrap())
            .unwrap();
        assert!(json.contains(r#""thought_signature":"sig-1""#), "{json}");
        let (out, _) = collect(vec![StreamEvent::ToolStart {
            index: 0,
            id: signed_tool_start_id("toolu_1", "sig-1"),
            name: "Read".to_string(),
            custom: false,
        }]);
        let start = out
            .lines()
            .find(|line| line.starts_with("data: {\"type\":\"content_block_start\""))
            .unwrap();
        assert!(
            start.contains(r#""id":"toolu_1","thought_signature":"sig-1""#),
            "{start}"
        );
        use crate::provider::ProviderEncoder;
        let encoded = String::from_utf8(
            crate::provider::google::GoogleProvider
                .encode_request(
                    &signed,
                    &crate::catalog::Catalog::builtin()
                        .unwrap()
                        .capabilities("gemini-4-argon")
                        .unwrap(),
                )
                .unwrap(),
        )
        .unwrap();
        assert!(
            encoded.contains(r#""thoughtSignature":"sig-1""#),
            "{encoded}"
        );
        assert!(
            encoded.contains(r#""displayName":"tool_image_0.png""#),
            "{encoded}"
        );
    }

    #[test]
    fn object_tool_result_is_a_json_value_in_google_request() {
        use crate::provider::google::GoogleProvider;
        use crate::provider::ProviderEncoder;

        let request = decode(serde_json::json!({
            "model": "gemini-4-argon",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1",
                     "content": [{"type": "json", "value": {"rows": [1, 2]}}]}
                ]}
            ]
        }));
        let body = String::from_utf8(
            GoogleProvider
                .encode_request(
                    &request,
                    &crate::catalog::Catalog::builtin()
                        .unwrap()
                        .capabilities("gemini-4-argon")
                        .unwrap(),
                )
                .unwrap(),
        )
        .unwrap();
        assert!(
            body.contains(
                r#"{"functionResponse":{"name":"Read","response":{"result":{"rows":[1,2]}}}}"#
            ),
            "{body}"
        );
    }

    #[test]
    fn sse_error_frame_skips_message_stop() {
        let (out, _) = collect(vec![StreamEvent::Error(GatewayError::request("boom"))]);
        assert_eq!(
            out,
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"boom\"}}\n\n"
        );
    }

    #[test]
    fn interleaved_tool_arguments_stay_on_their_own_block() {
        let mut state = SseState::new("m", 0);
        let mut out = String::new();
        for event in [
            StreamEvent::ToolStart {
                index: 0,
                id: "toolu_1".to_string(),
                name: "Read".to_string(),
                custom: false,
            },
            StreamEvent::ToolStart {
                index: 1,
                id: "toolu_2".to_string(),
                name: "Write".to_string(),
                custom: false,
            },
            StreamEvent::ToolArgsDelta {
                index: 0,
                delta: "{\"path\":".to_string(),
            },
            StreamEvent::ToolArgsDelta {
                index: 1,
                delta: "{\"text\":".to_string(),
            },
            StreamEvent::ToolArgsDelta {
                index: 0,
                delta: "\"a\"}".to_string(),
            },
            StreamEvent::ToolArgsDelta {
                index: 1,
                delta: "\"b\"}".to_string(),
            },
            StreamEvent::ToolEnd { index: 0 },
            StreamEvent::ToolEnd { index: 1 },
        ] {
            if let Some(frame) = AnthropicEncoder.encode_event(&event, &mut state).unwrap() {
                out.push_str(&String::from_utf8(frame).unwrap());
            }
        }
        let deltas: Vec<String> = frames_of(out.as_bytes())
            .iter()
            .filter(|frame| frame.contains("input_json_delta"))
            .map(|frame| frame.split_once("data: ").unwrap().1.to_string())
            .collect();
        assert_eq!(deltas.len(), 4, "{deltas:?}");
        for (data, (index, fragment)) in deltas.iter().zip([
            (0, r#""{\"path\":""#),
            (1, r#""{\"text\":""#),
            (0, r#""\"a\"}""#),
            (1, r#""\"b\"}""#),
        ]) {
            assert!(data.contains(&format!("\"index\":{index}")), "{data}");
            assert!(
                data.contains(&format!("\"partial_json\":{fragment}")),
                "{data}"
            );
        }
        assert_eq!(state.tool_args[0], "{\"path\":\"a\"}");
        assert_eq!(state.tool_args[1], "{\"text\":\"b\"}");
    }
}
