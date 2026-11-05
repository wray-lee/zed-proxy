//! `anthropic` provider: neutral request → Anthropic Messages payload, and
//! Anthropic events → neutral events (spec `21-provider-anthropic.md`).
//!
//! Every object is written by hand so the key order follows spec 21 §2. Opaque
//! client objects (`thinking`, `output_config`, `cache_control`, tool schemas)
//! are `serde_json::Value` and therefore already key-sorted when the client
//! decoder parsed them (this crate keeps serde_json's default `BTreeMap`); see
//! the gap note in the task report — byte-identical member order for those
//! needs serde_json's `preserve_order` feature (a Cargo.toml change).

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::catalog::ModelCapabilities;
use crate::catalog::Provider as ProviderKind;
use crate::error::{classify_status, GatewayError};
use crate::neutral::{
    BlockKind, ContentBlock, ImageSource, Message, NeutralRequest, NeutralResponse, Role,
    StopReason, StreamEvent, ToolChoice, ToolUse, Usage,
};

use super::{clean_line, is_stream_ended, parse_status_failed, ProviderEncoder, StreamDecoder};

/// Fallback ceiling when the catalog has no `max_output_tokens` (spec 21 §3).
pub const FALLBACK_MAX_TOKENS: i64 = 128_000;
/// Default when the client omits `max_tokens` (spec 21 §3).
pub const DEFAULT_MAX_TOKENS: i64 = 8_192;
/// Default strategy the upstream only accepts next to a `compaction` block
/// (spec 21 §7).
pub const DEFAULT_CONTEXT_MANAGEMENT: &str = r#"{"edits":[{"type":"compact_20260112"}]}"#;
/// Empty JSON Schema substituted for a missing/non-object `input_schema`
/// (spec 21 §10.1).
pub const EMPTY_SCHEMA: &str = r#"{"type":"object","properties":{}}"#;
/// Prefix for a `tool_result` whose id matches no call in this request
/// (spec 45 §4.3 item 4).
pub const UNPAIRED_PREFIX: &str = "[unpaired tool result ";

pub struct AnthropicProvider;

impl ProviderEncoder for AnthropicProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    fn encode_request(
        &self,
        request: &NeutralRequest,
        capabilities: &ModelCapabilities,
    ) -> Result<Vec<u8>, GatewayError> {
        Ok(encode_request(request, capabilities).into_bytes())
    }

    fn decode_response(&self, body: &[u8]) -> Result<NeutralResponse, GatewayError> {
        decode_response(body)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(AnthropicStreamDecoder::new())
    }
}

/// `min(client, catalog)` with the 128000 fallback (spec 21 §3).
pub fn max_tokens(client_max: Option<i64>, catalog_max: Option<i64>) -> i64 {
    let requested = match client_max {
        Some(value) if value > 0 => value,
        _ => DEFAULT_MAX_TOKENS,
    };
    requested.min(catalog_max.unwrap_or(FALLBACK_MAX_TOKENS))
}

/// Claude 5th-generation models reject `temperature` (spec 21 §8, D21-3).
pub fn drops_temperature(model: &str) -> bool {
    model.starts_with("claude-sonnet-5") || model.starts_with("claude-haiku-5")
}

fn jstr(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

fn number(value: f64) -> Option<String> {
    if value.is_finite() {
        serde_json::to_string(&value).ok()
    } else {
        None
    }
}

fn log_dropped(rule: &str, what: &str) {
    eprintln!("[zed-proxy] anthropic dropped {what} ({rule})");
}

/// Encode the whole `provider_request` object (spec 21 §2 key order).
pub fn encode_request(request: &NeutralRequest, capabilities: &ModelCapabilities) -> String {
    let mut out = String::with_capacity(768);
    out.push('{');
    out.push_str(&format!("\"model\":{}", jstr(&request.model)));
    out.push_str(&format!(
        ",\"max_tokens\":{}",
        max_tokens(request.max_output_tokens, capabilities.max_output_tokens)
    ));

    if let Some(system) = encode_system(&request.system, request.system_string) {
        out.push_str(",\"system\":");
        out.push_str(&system);
    }

    // top_p / top_k are never forwarded (spec 21 §8).
    if !drops_temperature(&request.model) {
        if let Some(value) = request.temperature.and_then(number) {
            out.push_str(",\"temperature\":");
            out.push_str(&value);
        }
    }

    if let Some(stops) = encode_stop_sequences(&request.stop) {
        out.push_str(",\"stop_sequences\":");
        out.push_str(&stops);
    }

    if let Some(thinking) = encode_thinking(request, capabilities) {
        out.push_str(",\"thinking\":");
        out.push_str(&thinking);
    }

    if let Some(config) = encode_output_config(request) {
        out.push_str(",\"output_config\":");
        out.push_str(&config);
    }

    if let Some(context) = encode_context_management(request, capabilities) {
        out.push_str(",\"context_management\":");
        out.push_str(&context);
    }

    let tools = if capabilities.supports_tools {
        encode_tools(&request.tools)
    } else {
        if !request.tools.is_empty() {
            log_dropped("supports_tools=false", "tools");
        }
        None
    };
    let has_tools = tools.is_some();
    if let Some(tools) = tools {
        out.push_str(",\"tools\":");
        out.push_str(&tools);
    }

    if has_tools {
        if let Some(choice) = encode_tool_choice(&request.tool_choice, request.parallel_tool_calls)
        {
            out.push_str(",\"tool_choice\":");
            out.push_str(&choice);
        }
    }

    out.push_str(",\"messages\":");
    out.push_str(&encode_messages(request, capabilities));
    out.push('}');
    out
}

/// System blocks, in order, as the array form of `StringOrContents`
/// (spec 21 §4). The neutral layer already holds the merged result: client
/// decoders append Claude Code's trailing `system` message as extra blocks, so
/// this function only has to keep block boundaries and `cache_control`.
fn encode_system(system: &[crate::neutral::SystemBlock], string_shorthand: bool) -> Option<String> {
    if system.is_empty() {
        return None;
    }
    if string_shorthand && system.len() == 1 {
        return Some(jstr(&system[0].text));
    }
    let mut blocks: Vec<String> = Vec::with_capacity(system.len());
    for block in system {
        let mut item = format!("{{\"type\":\"text\",\"text\":{}", jstr(&block.text));
        if let Some(cache_control) = &block.cache_control {
            item.push_str(",\"cache_control\":");
            item.push_str(&compact(cache_control));
        }
        item.push('}');
        blocks.push(item);
    }
    Some(format!("[{}]", blocks.join(",")))
}

fn encode_stop_sequences(stops: &[String]) -> Option<String> {
    let kept: Vec<String> = stops
        .iter()
        .filter(|value| !value.trim().is_empty())
        .map(|value| jstr(value))
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(format!("[{}]", kept.join(",")))
    }
}

fn encode_thinking(request: &NeutralRequest, capabilities: &ModelCapabilities) -> Option<String> {
    let thinking = request.thinking.as_ref()?;
    if !thinking.is_object() {
        log_dropped("thinking not an object", "thinking");
        return None;
    }
    if !capabilities.supports_thinking {
        log_dropped("supports_thinking=false", "thinking");
        return None;
    }
    let Some(kind) = thinking.get("type").and_then(Value::as_str) else {
        log_dropped("thinking without a type", "thinking");
        return None;
    };
    if kind == "disabled" && !capabilities.supports_disabling_thinking {
        log_dropped("supports_disabling_thinking=false", "thinking");
        return None;
    }
    // `type` leads so the object keeps the spec's sample order; the other
    // members follow in the decoded (sorted) order.
    let mut out = format!("{{\"type\":{}", jstr(kind));
    if let Some(members) = thinking.as_object() {
        for (key, value) in members {
            if key == "type" {
                continue;
            }
            out.push(',');
            out.push_str(&jstr(key));
            out.push(':');
            out.push_str(&compact(value));
        }
    }
    out.push('}');
    Some(out)
}

fn encode_output_config(request: &NeutralRequest) -> Option<String> {
    let config = request.output_config.as_ref()?;
    if !config.is_object() {
        log_dropped("output_config not an object", "output_config");
        return None;
    }
    Some(compact(config))
}

fn has_compaction(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        message
            .parts
            .iter()
            .any(|part| matches!(part, ContentBlock::Compaction(_)))
    })
}

fn encode_context_management(
    request: &NeutralRequest,
    capabilities: &ModelCapabilities,
) -> Option<String> {
    if !capabilities.supports_server_side_compaction {
        if request.context_management.is_some() {
            log_dropped(
                "supports_server_side_compaction=false",
                "context_management",
            );
        }
        return None;
    }
    // An `edits` array that is missing/empty counts as "not given" (spec 21 §7).
    if let Some(context) = &request.context_management {
        let edits_ok = context
            .get("edits")
            .and_then(Value::as_array)
            .map(|edits| !edits.is_empty())
            .unwrap_or(false);
        if context.is_object() && edits_ok {
            return Some(compact(context));
        }
        log_dropped("context_management without edits", "context_management");
    }
    if has_compaction(&request.messages) {
        return Some(DEFAULT_CONTEXT_MANAGEMENT.to_string());
    }
    None
}

fn encode_tools(tools: &[crate::neutral::ToolDef]) -> Option<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut items: Vec<String> = Vec::new();
    for tool in tools {
        if tool.name.is_empty() {
            log_dropped("empty tool name", "tool");
            continue;
        }
        if !seen.insert(tool.name.as_str()) {
            log_dropped("duplicate tool name", "tool");
            continue;
        }
        let schema = if tool.parameters.is_object() {
            compact(&tool.parameters)
        } else {
            EMPTY_SCHEMA.to_string()
        };
        let mut item = format!(
            "{{\"name\":{},\"description\":{},\"input_schema\":{}",
            jstr(&tool.name),
            jstr(&tool.description),
            schema
        );
        // `strict` is not an Anthropic Messages field (spec 21 §10.2).
        if let Some(cache_control) = &tool.cache_control {
            item.push_str(",\"cache_control\":");
            item.push_str(&compact(cache_control));
        }
        item.push('}');
        items.push(item);
    }
    if items.is_empty() {
        None
    } else {
        Some(format!("[{}]", items.join(",")))
    }
}

fn encode_tool_choice(choice: &ToolChoice, parallel: Option<bool>) -> Option<String> {
    if matches!(choice, ToolChoice::Unspecified | ToolChoice::Auto) && parallel != Some(false) {
        // The default needs no key (spec 21 §11, sample 16A.1).
        return None;
    }
    let mut item = match choice {
        ToolChoice::Unspecified | ToolChoice::Auto => "{\"type\":\"auto\"".to_string(),
        ToolChoice::None => "{\"type\":\"none\"".to_string(),
        ToolChoice::Required => "{\"type\":\"any\"".to_string(),
        ToolChoice::Named(name) => format!("{{\"type\":\"tool\",\"name\":{}", jstr(name)),
    };
    if parallel == Some(false) {
        item.push_str(",\"disable_parallel_tool_use\":true");
    }
    item.push('}');
    Some(item)
}

fn known_tool_use_ids(request: &NeutralRequest) -> HashSet<&str> {
    let mut ids: HashSet<&str> = request
        .tool_uses
        .iter()
        .map(|call| call.id.as_str())
        .collect();
    for message in &request.messages {
        for part in &message.parts {
            if let ContentBlock::ToolUse(call) = part {
                ids.insert(call.id.as_str());
            }
        }
    }
    ids
}

fn encode_messages(request: &NeutralRequest, capabilities: &ModelCapabilities) -> String {
    let known = known_tool_use_ids(request);
    let mut messages: Vec<String> = Vec::new();
    for (position, message) in request.messages.iter().enumerate() {
        let content = encode_message_content(message, capabilities, &known, position);
        if content.is_empty() {
            continue;
        }
        messages.push(format!(
            "{{\"role\":\"{}\",\"content\":[{}]}}",
            message.role.as_str(),
            content.join(",")
        ));
    }
    format!("[{}]", messages.join(","))
}

fn encode_message_content(
    message: &Message,
    capabilities: &ModelCapabilities,
    known: &HashSet<&str>,
    position: usize,
) -> Vec<String> {
    // Dropped blocks name the message index so a bad history can be located
    // (spec 21 §12.2).
    let drop_block = |what: &str, rule: &str| {
        eprintln!("[zed-proxy] anthropic dropped {what} in message #{position} ({rule})");
    };
    // Anthropic requires every `tool_result` at the front of a user message
    // (spec 45 §4.6).
    let mut results: Vec<String> = Vec::new();
    let mut others: Vec<String> = Vec::new();
    // Assistant text blocks merge into one block that keeps the first block's
    // position (spec 21 §12.4).
    let mut merged_text: Option<(usize, String)> = None;
    for part in &message.parts {
        match part {
            ContentBlock::Text(text) => {
                if message.role == Role::Assistant {
                    match merged_text.as_mut() {
                        Some((_, merged)) => {
                            merged.push_str("\n\n");
                            merged.push_str(text);
                        }
                        None => {
                            others.push(String::new());
                            merged_text = Some((others.len() - 1, text.clone()));
                        }
                    }
                } else {
                    others.push(format!("{{\"type\":\"text\",\"text\":{}}}", jstr(text)));
                }
            }
            ContentBlock::Image(source) => {
                if !capabilities.supports_images {
                    drop_block("image", "supports_images=false");
                    continue;
                }
                match encode_image(source) {
                    Some(block) => others.push(block),
                    None => drop_block("image", "unsupported source"),
                }
            }
            ContentBlock::ToolUse(call) => {
                if call.id.is_empty() || call.name.is_empty() {
                    drop_block("tool_use", "missing id or name");
                    continue;
                }
                others.push(format!(
                    "{{\"type\":\"tool_use\",\"id\":{},\"name\":{},\"input\":{}}}",
                    jstr(&call.id),
                    jstr(&call.name),
                    tool_input(&call.arguments)
                ));
            }
            ContentBlock::ToolResult(result) => {
                if result.tool_use_id.is_empty() {
                    drop_block("tool_result", "missing tool_use_id");
                    continue;
                }
                if !known.contains(result.tool_use_id.as_str()) {
                    // Never drop: the model would replay the tool (spec 45 §4.3).
                    results.push(format!(
                        "{{\"type\":\"text\",\"text\":{}}}",
                        jstr(&format!(
                            "{}{}]\n{}",
                            UNPAIRED_PREFIX, result.tool_use_id, result.output
                        ))
                    ));
                    continue;
                }
                results.push(format!(
                    "{{\"type\":\"tool_result\",\"tool_use_id\":{},\"content\":{},\"is_error\":{}}}",
                    jstr(&result.tool_use_id),
                    jstr(&result.output),
                    result.is_error
                ));
            }
            // Historical thinking blocks can never be replayed: the gateway
            // never receives a signature (spec 21 §12.2).
            ContentBlock::Thinking(_) => drop_block("thinking", "historical block"),
            ContentBlock::Compaction(text) => {
                if capabilities.supports_server_side_compaction {
                    others.push(format!(
                        "{{\"type\":\"compaction\",\"text\":{}}}",
                        jstr(text)
                    ));
                } else {
                    drop_block("compaction", "supports_server_side_compaction=false");
                }
            }
            ContentBlock::Opaque(opaque) => {
                drop_block(
                    &format!("opaque block from {}", opaque.provider),
                    "no meaning",
                );
            }
        }
    }
    if let Some((slot, text)) = merged_text {
        others[slot] = format!("{{\"type\":\"text\",\"text\":{}}}", jstr(&text));
    }
    results.extend(others);
    results
}

fn tool_input(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) if value.is_object() => compact(&value),
        Ok(_) => {
            log_dropped("tool arguments not an object", "tool arguments");
            "{}".to_string()
        }
        Err(_) => {
            log_dropped("tool arguments are not JSON", "tool arguments");
            "{}".to_string()
        }
    }
}

fn encode_image(source: &ImageSource) -> Option<String> {
    match source {
        ImageSource::Data { media_type, data } => Some(format!(
            "{{\"type\":\"image\",\"source\":{{\"type\":\"base64\",\"media_type\":{},\"data\":{}}}}}",
            jstr(media_type),
            jstr(data)
        )),
        ImageSource::Url(url) => Some(format!(
            "{{\"type\":\"image\",\"source\":{{\"type\":\"url\",\"url\":{}}}}}",
            jstr(url)
        )),
    }
}

// ---------------------------------------------------------------------------
// Response parsing (non-streaming reduction and the streaming decoder)
// ---------------------------------------------------------------------------

/// Upstream body / event family. The first recognizable event fixes it
/// (spec 21 §13); events of the other family are logged and dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Unknown,
    Anthropic,
    Responses,
}

/// Streaming decoder for one upstream response (spec 21 §15).
///
/// The non-streaming reduction reuses it: a complete body is fed line by line
/// and the emitted events are folded into a [`NeutralResponse`].
pub struct AnthropicStreamDecoder {
    shape: Shape,
    saw_model_event: bool,
    failure: Option<GatewayError>,
    ended: bool,
    blocks: HashMap<u64, BlockKind>,
    tool_index_by_block: HashMap<u64, u64>,
    tool_count: u64,
    open_tool: Option<u64>,
    stop_reason: StopReason,
    usage: Usage,
}

impl AnthropicStreamDecoder {
    pub fn new() -> Self {
        Self {
            shape: Shape::Unknown,
            saw_model_event: false,
            failure: None,
            ended: false,
            blocks: HashMap::new(),
            tool_index_by_block: HashMap::new(),
            tool_count: 0,
            open_tool: None,
            stop_reason: StopReason::default(),
            usage: Usage::default(),
        }
    }

    fn adopt(&mut self, shape: Shape) -> bool {
        if self.shape == Shape::Unknown {
            self.shape = shape;
            return true;
        }
        if self.shape == shape {
            return true;
        }
        log_dropped(
            "event shape differs from the first recognized event",
            "event",
        );
        false
    }
}

impl Default for AnthropicStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn stop_reason_of(value: Option<&Value>) -> StopReason {
    match value.and_then(Value::as_str) {
        Some("max_tokens") => StopReason::Length,
        Some("tool_use") => StopReason::ToolCalls,
        Some("refusal") => StopReason::ContentFilter,
        Some("stop_sequence") => StopReason::Stop,
        Some("end_turn") => StopReason::Stop,
        Some(_) => StopReason::Unknown,
        // `null` / missing / end_turn all mean a normal stop (spec 21 §13.2).
        None => StopReason::Stop,
    }
}

fn stop_reason_from_responses(value: Option<&str>, has_tools: bool) -> StopReason {
    match value {
        Some("max_output_tokens") => StopReason::Length,
        Some("content_filter") => StopReason::ContentFilter,
        Some(_) => {
            if has_tools {
                StopReason::ToolCalls
            } else {
                StopReason::Stop
            }
        }
        None => {
            if has_tools {
                StopReason::ToolCalls
            } else {
                StopReason::Stop
            }
        }
    }
}

/// Usage block of an Anthropic event (`message_start.message.usage` or
/// `message_delta.usage`).
fn merge_usage(target: &mut Usage, usage: &Value) {
    if let Some(value) = usage.get("input_tokens").and_then(Value::as_i64) {
        target.input_tokens = value;
    }
    if let Some(value) = usage.get("output_tokens").and_then(Value::as_i64) {
        target.output_tokens = value;
    }
    // Neutral has one cache counter; the read side is the cached input count.
    if let Some(value) = usage.get("cache_read_input_tokens").and_then(Value::as_i64) {
        target.cached_input_tokens = value;
    }
    if let Some(value) = usage.get("total_tokens").and_then(Value::as_i64) {
        target.total_tokens = value;
    }
}

fn event_error(value: &Value) -> GatewayError {
    let error = value.get("error").unwrap_or(value);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("upstream rejected request")
        .to_string();
    match error.get("type").and_then(Value::as_str) {
        Some("invalid_request_error") => GatewayError::request(message),
        Some("authentication_error") => GatewayError::auth(401, message),
        Some("permission_error") => GatewayError::plan(403, message),
        Some("rate_limit_error") => GatewayError::rate_limit(message),
        Some("overloaded_error") => GatewayError::transient(Some(529), message),
        Some(_) | None => GatewayError::transient(None, message),
    }
}

/// `response.failed` (spec 10 §4.6, 30 §3.2): code `upstream_http_<n>` maps
/// through the shared status classifier.
fn responses_failed_error(value: &Value) -> GatewayError {
    let response = value.get("response").unwrap_or(value);
    let error = response
        .get("error")
        .or_else(|| response.get("status_details").and_then(|v| v.get("error")));
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| response.get("error").and_then(Value::as_str))
        .unwrap_or("upstream rejected request")
        .to_string();
    let status = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .and_then(|code| code.strip_prefix("upstream_http_"))
        .and_then(|suffix| suffix.parse::<u16>().ok());
    let plan_hint = message.to_ascii_lowercase().contains("plan");
    match status {
        Some(status) => {
            let mut err = GatewayError::new(classify_status(status, plan_hint), message);
            err.status = Some(status);
            err
        }
        None => GatewayError::transient(None, message),
    }
}

fn is_failed_wrapper(value: &Value) -> bool {
    value
        .get("status")
        .map(|status| status.is_object() && status.get("failed").is_some())
        .unwrap_or(false)
}

fn tool_ordinal(state: &mut AnthropicStreamDecoder, block_index: u64) -> u64 {
    if let Some(existing) = state.tool_index_by_block.get(&block_index) {
        return *existing;
    }
    let ordinal = state.tool_count;
    state.tool_count += 1;
    state.tool_index_by_block.insert(block_index, ordinal);
    ordinal
}

fn response_id(value: &Value) -> Option<String> {
    value
        .get("message")
        .and_then(|message| message.get("id"))
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("id"))
        })
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Parse one already-cleaned line into events (spec 21 §13, §15).
fn parse_event(
    state: &mut AnthropicStreamDecoder,
    value: &Value,
    events: &mut Vec<StreamEvent>,
) -> Result<(), GatewayError> {
    if is_stream_ended(value) {
        state.ended = true;
        return Ok(());
    }
    if is_failed_wrapper(value) {
        let err = parse_status_failed(value)
            .unwrap_or_else(|| GatewayError::transient(None, "upstream rejected request"));
        state.failure = Some(err.clone());
        state.ended = true;
        events.push(StreamEvent::Error(err));
        return Ok(());
    }

    // Events may be wrapped in an `event` object (spec 10 §4.2).
    let value = match value.get("event") {
        Some(inner) if inner.is_object() => inner,
        _ => value,
    };
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return Ok(());
    };
    let responses_shape = kind.starts_with("response.");
    let shape = if responses_shape {
        Shape::Responses
    } else {
        Shape::Anthropic
    };
    if !state.adopt(shape) {
        return Ok(());
    }
    // A ping / other keep-alive still proves the stream carried data
    // (spec 16 §5).
    state.saw_model_event = true;

    match kind {
        "ping" => {}
        "message_start" => {
            events.push(StreamEvent::Start {
                id: response_id(value),
            });
            let message = value.get("message").unwrap_or(&Value::Null);
            if let Some(message_usage) = message.get("usage") {
                merge_usage(&mut state.usage, message_usage);
                events.push(StreamEvent::Usage(state.usage));
            }
        }
        "content_block_start" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            let block = value.get("content_block").unwrap_or(&Value::Null);
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    state.blocks.insert(index, BlockKind::Text);
                    events.push(StreamEvent::BlockStart {
                        index,
                        kind: BlockKind::Text,
                    });
                }
                Some("thinking") => {
                    state.blocks.insert(index, BlockKind::Thinking);
                    events.push(StreamEvent::BlockStart {
                        index,
                        kind: BlockKind::Thinking,
                    });
                }
                Some("tool_use") => {
                    state.blocks.insert(index, BlockKind::ToolUse);
                    let tool_index = tool_ordinal(state, index);
                    state.open_tool = Some(tool_index);
                    events.push(StreamEvent::ToolStart {
                        index: tool_index,
                        id: block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    });
                }
                Some(other) => log_dropped(&format!("block type {other}"), "content block"),
                None => log_dropped("block without type", "content block"),
            }
        }
        "content_block_delta" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            let delta = value.get("delta").unwrap_or(&Value::Null);
            let kind = delta.get("type").and_then(Value::as_str);
            let block = state.blocks.get(&index).copied();
            match (kind, block) {
                (Some("text_delta"), Some(BlockKind::Text)) => {
                    events.push(StreamEvent::TextDelta {
                        delta: delta
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    });
                }
                (Some("thinking_delta"), Some(BlockKind::Thinking)) => {
                    events.push(StreamEvent::ThinkingDelta {
                        delta: delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    });
                }
                // Never replayed: the gateway receives no signature
                // (spec 21 §12.2).
                (Some("signature_delta"), _) => {}
                (Some("input_json_delta"), Some(BlockKind::ToolUse)) => {
                    let tool_index = tool_ordinal(state, index);
                    events.push(StreamEvent::ToolArgsDelta {
                        index: tool_index,
                        delta: delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    });
                }
                _ => log_dropped("delta without its open block", "delta"),
            }
        }
        "content_block_stop" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            match state.blocks.remove(&index) {
                Some(BlockKind::ToolUse) => {
                    let tool_index = tool_ordinal(state, index);
                    state.open_tool = None;
                    events.push(StreamEvent::ToolEnd { index: tool_index });
                }
                Some(_) => {}
                None => log_dropped("block stop without start", "content block"),
            }
            events.push(StreamEvent::BlockStop { index });
        }
        "message_delta" => {
            if let Some(delta) = value.get("delta") {
                state.stop_reason = stop_reason_of(delta.get("stop_reason"));
            }
            if let Some(usage) = value.get("usage") {
                merge_usage(&mut state.usage, usage);
                events.push(StreamEvent::Usage(state.usage));
            }
        }
        "message_stop" => {
            state.ended = true;
            events.push(StreamEvent::End {
                reason: state.stop_reason,
            });
        }
        "error" => {
            let err = event_error(value);
            state.failure = Some(err.clone());
            state.ended = true;
            events.push(StreamEvent::Error(err));
        }
        "response.output_text.delta" => {
            events.push(StreamEvent::TextDelta {
                delta: value
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            events.push(StreamEvent::ThinkingDelta {
                delta: value
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        "response.output_item.added" => {
            let item = value.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let tool_index = state.tool_count;
                state.tool_count += 1;
                state.open_tool = Some(tool_index);
                events.push(StreamEvent::ToolStart {
                    index: tool_index,
                    id: item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                });
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(tool_index) = state.open_tool {
                events.push(StreamEvent::ToolArgsDelta {
                    index: tool_index,
                    delta: value
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                });
            } else {
                log_dropped("tool arguments without an open tool", "tool delta");
            }
        }
        "response.function_call_arguments.done" | "response.output_item.done" => {
            let is_tool_item = kind == "response.function_call_arguments.done"
                || value
                    .get("item")
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                    == Some("function_call");
            if is_tool_item {
                if let Some(tool_index) = state.open_tool.take() {
                    events.push(StreamEvent::ToolEnd { index: tool_index });
                }
            }
        }
        "response.completed" | "response.incomplete" => {
            let response = value.get("response").unwrap_or(value);
            if let Some(usage) = response.get("usage") {
                merge_usage(&mut state.usage, usage);
                events.push(StreamEvent::Usage(state.usage));
            }
            let has_tools = state.open_tool.is_some() || state.tool_count > 0;
            let detail = response
                .get("incomplete_details")
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str);
            state.stop_reason = stop_reason_from_responses(detail, has_tools);
            state.ended = true;
            events.push(StreamEvent::End {
                reason: state.stop_reason,
            });
        }
        "response.failed" => {
            let err = responses_failed_error(value);
            state.failure = Some(err.clone());
            state.ended = true;
            events.push(StreamEvent::Error(err));
        }
        _ => {}
    }
    Ok(())
}

impl StreamDecoder for AnthropicStreamDecoder {
    fn push_line(&mut self, line: &str) -> Result<Vec<StreamEvent>, GatewayError> {
        let mut events = Vec::new();
        if self.ended {
            return Ok(events);
        }
        let line = clean_line(line);
        if !line.starts_with('{') {
            return Ok(events);
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => return Ok(events),
        };
        parse_event(self, &value, &mut events)?;
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, GatewayError> {
        let mut events = Vec::new();
        if self.failure.is_none() && !self.ended {
            events.push(StreamEvent::End {
                reason: self.stop_reason,
            });
            self.ended = true;
        }
        Ok(events)
    }

    fn saw_model_event(&self) -> bool {
        self.saw_model_event
    }

    fn failure(&self) -> Option<GatewayError> {
        self.failure.clone()
    }
}

/// Fold the stream events of a complete body into the neutral response
/// (spec 21 §13).
pub fn decode_response(body: &[u8]) -> Result<NeutralResponse, GatewayError> {
    let text = String::from_utf8_lossy(body);
    let mut decoder = AnthropicStreamDecoder::new();
    let mut events: Vec<StreamEvent> = Vec::new();
    for line in text.split('\n') {
        events.extend(decoder.push_line(line)?);
    }
    events.extend(decoder.finish()?);
    if let Some(err) = decoder.failure() {
        return Err(err);
    }
    if !decoder.saw_model_event {
        return Err(GatewayError::transient(None, "upstream returned no events"));
    }

    let mut response = NeutralResponse::default();
    let mut tools: Vec<ToolUse> = Vec::new();
    for event in events {
        match event {
            StreamEvent::Start { id } => response.id = id,
            StreamEvent::TextDelta { delta } => response.text.push_str(&delta),
            StreamEvent::ThinkingDelta { delta } => response.thinking.push_str(&delta),
            StreamEvent::ToolStart { index, id, name } => {
                let slot = index as usize;
                while tools.len() <= slot {
                    tools.push(ToolUse {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                        signature: None,
                    });
                }
                tools[slot].id = id;
                tools[slot].name = name;
            }
            StreamEvent::ToolArgsDelta { index, delta } => {
                if let Some(tool) = tools.get_mut(index as usize) {
                    tool.arguments.push_str(&delta);
                }
            }
            StreamEvent::ToolEnd { index } => {
                if let Some(tool) = tools.get_mut(index as usize) {
                    if tool.arguments.is_empty() {
                        tool.arguments.push_str("{}");
                    }
                }
            }
            StreamEvent::Usage(usage) => {
                response.usage.input_tokens = usage.input_tokens;
                response.usage.output_tokens = usage.output_tokens;
                response.usage.cached_input_tokens = usage.cached_input_tokens;
                response.usage.reasoning_tokens = usage.reasoning_tokens;
                response.usage.total_tokens = usage.total_tokens;
            }
            StreamEvent::End { reason } => response.stop_reason = reason,
            StreamEvent::Error(err) => return Err(err),
            StreamEvent::BlockStart { .. } | StreamEvent::BlockStop { .. } => {}
        }
    }
    response.tool_calls = tools;
    if response.stop_reason == StopReason::Stop && !response.tool_calls.is_empty() {
        response.stop_reason = StopReason::ToolCalls;
    }
    response.usage = response.usage.with_computed_total();
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::EffortLevel;
    use crate::error::ErrorKind;
    use crate::neutral::{SystemBlock, ToolDef, ToolResult};
    use serde_json::json;

    fn caps() -> ModelCapabilities {
        ModelCapabilities {
            provider: ProviderKind::Anthropic,
            max_output_tokens: Some(128_000),
            supports_tools: true,
            supports_images: true,
            supports_thinking: true,
            supports_disabling_thinking: true,
            supports_parallel_tool_calls: true,
            supports_server_side_compaction: true,
            supported_effort_levels: Vec::<EffortLevel>::new(),
        }
    }

    fn request() -> NeutralRequest {
        NeutralRequest {
            model: "claude-sonnet-5".to_string(),
            provider: ProviderKind::Anthropic,
            ..NeutralRequest::default()
        }
    }

    fn encode(request: &NeutralRequest, caps: &ModelCapabilities) -> String {
        encode_request(request, caps)
    }

    fn text(value: &str) -> ContentBlock {
        ContentBlock::Text(value.to_string())
    }

    fn tool_use(id: &str, name: &str, arguments: &str) -> ContentBlock {
        ContentBlock::ToolUse(ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
            signature: None,
        })
    }

    fn message(role: Role, parts: Vec<ContentBlock>) -> Message {
        Message { role, parts }
    }

    #[test]
    fn max_tokens_defaults_and_clamps() {
        assert_eq!(max_tokens(None, None), 8_192);
        assert_eq!(max_tokens(Some(0), None), 8_192);
        assert_eq!(max_tokens(Some(-5), None), 8_192);
        assert_eq!(max_tokens(Some(1_000_000), None), 128_000);
        assert_eq!(max_tokens(Some(100_000), Some(64_000)), 64_000);
        assert_eq!(max_tokens(Some(1_000), Some(64_000)), 1_000);
    }

    #[test]
    fn fifth_generation_models_drop_temperature() {
        assert!(drops_temperature("claude-sonnet-5-5"));
        assert!(drops_temperature("claude-sonnet-5"));
        assert!(drops_temperature("claude-haiku-5-5"));
        assert!(!drops_temperature("claude-sonnet-4-6"));
        assert!(!drops_temperature("claude-haiku-4-5"));
    }

    /// Spec 21 §16A.1, byte for byte. The only intentional deviation is the
    /// member order of opaque client objects (serde_json without
    /// `preserve_order` sorts them); `thinking` is rebuilt with `type` first.
    #[test]
    fn sample_16a1_request_matches_the_spec_bytes() {
        let mut request = request();
        request.max_output_tokens = Some(1_000_000);
        request.system = vec![
            SystemBlock {
                text: "base rules".to_string(),
                cache_control: None,
            },
            SystemBlock {
                text: "style".to_string(),
                cache_control: Some(json!({"type": "ephemeral"})),
            },
        ];
        request.temperature = Some(0.7);
        request.stop = vec!["\n".to_string(), "END".to_string()];
        request.thinking = Some(json!({"type": "enabled", "budget_tokens": 2048}));
        request.output_config = Some(json!({"effort": "xhigh"}));
        request.tools = vec![ToolDef {
            name: "Bash".to_string(),
            description: String::new(),
            parameters: json!({"type": "object"}),
            strict: None,
            cache_control: None,
            format: None,
        }];
        request.messages = vec![
            message(Role::User, vec![text("ls")]),
            message(Role::Assistant, vec![tool_use("toolu_1", "Bash", "{}")]),
            message(
                Role::User,
                vec![ContentBlock::ToolResult(ToolResult {
                    tool_use_id: "toolu_1".to_string(),
                    output: String::new(),
                    is_error: false,
                })],
            ),
        ];
        let expected = concat!(
            r#"{"model":"claude-sonnet-5","max_tokens":128000,"#,
            r#""system":[{"type":"text","text":"base rules"},{"type":"text","text":"style","cache_control":{"type":"ephemeral"}}],"#,
            r#""stop_sequences":["END"],"#,
            r#""thinking":{"type":"enabled","budget_tokens":2048},"#,
            r#""output_config":{"effort":"xhigh"},"#,
            r#""tools":[{"name":"Bash","description":"","input_schema":{"type":"object"}}],"#,
            r#""messages":[{"role":"user","content":[{"type":"text","text":"ls"}]},"#,
            r#"{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}]},"#,
            r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"","is_error":false}]}]}"#
        );
        assert_eq!(encode(&request, &caps()), expected);
    }

    #[test]
    fn system_blocks_keep_boundaries_and_cache_control() {
        let mut request = request();
        request.system = vec![
            SystemBlock {
                text: "top".to_string(),
                cache_control: None,
            },
            SystemBlock {
                text: "claude code tail".to_string(),
                cache_control: None,
            },
        ];
        let body = encode(&request, &caps());
        assert!(body.contains(
            r#""system":[{"type":"text","text":"top"},{"type":"text","text":"claude code tail"}]"#
        ));
    }

    #[test]
    fn sampling_rules_and_stop_sequence_filtering() {
        let mut request = request();
        request.temperature = Some(0.7);
        request.top_p = Some(0.9);
        request.top_k = Some(5);
        request.stop = vec!["  ".to_string(), "END".to_string(), "STOP ".to_string()];
        let body = encode(&request, &caps());
        assert!(!body.contains("temperature"), "5th-gen Claude drops it");
        assert!(!body.contains("top_p"));
        assert!(!body.contains("top_k"));
        assert!(body.contains(r#""stop_sequences":["END","STOP "]"#));

        let mut old = request.clone();
        old.model = "claude-sonnet-4-6".to_string();
        assert!(encode(&old, &caps()).contains(r#""temperature":0.7"#));

        let mut blank = request.clone();
        blank.stop = vec![" ".to_string(), "\t".to_string()];
        assert!(!encode(&blank, &caps()).contains("stop_sequences"));
    }

    #[test]
    fn thinking_is_gated_by_catalog_flags() {
        let mut request = request();
        request.thinking = Some(json!({"type": "enabled", "budget_tokens": 1024}));
        assert!(encode(&request, &caps())
            .contains(r#""thinking":{"type":"enabled","budget_tokens":1024}"#));

        request.thinking = Some(json!({"type": "disabled"}));
        assert!(encode(&request, &caps()).contains(r#""thinking":{"type":"disabled"}"#));

        let mut no_disable = caps();
        no_disable.supports_disabling_thinking = false;
        assert!(!encode(&request, &no_disable).contains("thinking"));

        let mut no_thinking = caps();
        no_thinking.supports_thinking = false;
        request.thinking = Some(json!({"type": "enabled", "budget_tokens": 1024}));
        assert!(!encode(&request, &no_thinking).contains("thinking"));
    }

    #[test]
    fn output_config_is_forwarded_only_when_it_is_an_object() {
        let mut request = request();
        request.output_config = Some(json!({"effort": "xhigh"}));
        assert!(encode(&request, &caps()).contains(r#""output_config":{"effort":"xhigh"}"#));
        request.output_config = Some(json!("xhigh"));
        assert!(!encode(&request, &caps()).contains("output_config"));
    }

    #[test]
    fn compaction_history_synthesizes_context_management() {
        let mut request = request();
        request.messages = vec![message(
            Role::User,
            vec![ContentBlock::Compaction("summary".to_string()), text("hi")],
        )];
        let body = encode(&request, &caps());
        assert!(body.contains(r#""context_management":{"edits":[{"type":"compact_20260112"}]}"#));
        assert!(body.contains(r#"{"type":"compaction","text":"summary"}"#));

        let mut unsupported = caps();
        unsupported.supports_server_side_compaction = false;
        let body = encode(&request, &unsupported);
        assert!(!body.contains("context_management"));
        assert!(!body.contains("compaction"));

        let mut explicit = request.clone();
        explicit.context_management = Some(json!({"edits": [{"type": "clear_tool_uses"}]}));
        assert!(encode(&explicit, &caps())
            .contains(r#""context_management":{"edits":[{"type":"clear_tool_uses"}]}"#));

        explicit.context_management = Some(json!({"edits": []}));
        assert!(encode(&explicit, &caps())
            .contains(r#""context_management":{"edits":[{"type":"compact_20260112"}]}"#));
    }

    #[test]
    fn tool_declarations_fill_required_fields_and_drop_bad_names() {
        let mut request = request();
        request.max_output_tokens = Some(128_000);
        request.tools = vec![
            ToolDef {
                name: "Bash".to_string(),
                description: String::new(),
                parameters: Value::Null,
                strict: None,
                cache_control: None,
                format: None,
            },
            ToolDef {
                name: String::new(),
                description: "dropped".to_string(),
                parameters: json!({"type": "object"}),
                strict: None,
                cache_control: None,
                format: None,
            },
            ToolDef {
                name: "Bash".to_string(),
                description: "duplicate".to_string(),
                parameters: json!({"type": "object"}),
                strict: None,
                cache_control: None,
                format: None,
            },
        ];
        assert_eq!(
            encode(&request, &caps()),
            r#"{"model":"claude-sonnet-5","max_tokens":128000,"tools":[{"name":"Bash","description":"","input_schema":{"type":"object","properties":{}}}],"messages":[]}"#
        );

        request.tools.clear();
        assert!(!encode(&request, &caps()).contains("tools"));
    }

    #[test]
    fn tool_choice_maps_to_the_anthropic_variants() {
        let mut request = request();
        request.tools = vec![ToolDef {
            name: "Read".to_string(),
            description: "r".to_string(),
            parameters: json!({"type": "object"}),
            strict: None,
            cache_control: None,
            format: None,
        }];
        assert!(!encode(&request, &caps()).contains("tool_choice"));

        request.tool_choice = ToolChoice::Required;
        assert!(encode(&request, &caps()).contains(r#""tool_choice":{"type":"any"}"#));

        request.tool_choice = ToolChoice::None;
        assert!(encode(&request, &caps()).contains(r#""tool_choice":{"type":"none"}"#));

        request.tool_choice = ToolChoice::Named("Read".to_string());
        assert!(
            encode(&request, &caps()).contains(r#""tool_choice":{"type":"tool","name":"Read"}"#)
        );

        request.parallel_tool_calls = Some(false);
        assert!(encode(&request, &caps()).contains(
            r#""tool_choice":{"type":"tool","name":"Read","disable_parallel_tool_use":true}"#
        ));

        request.tools.clear();
        assert!(!encode(&request, &caps()).contains("tool_choice"));
    }

    #[test]
    fn chat_style_tool_history_becomes_anthropic_blocks() {
        let mut request = request();
        request.tool_uses = vec![ToolUse {
            id: "toolu_9".to_string(),
            name: "Read".to_string(),
            arguments: "{\"path\":\"a.txt\"}".to_string(),
            signature: None,
        }];
        request.messages = vec![
            message(Role::Assistant, vec![text("one"), text("two")]),
            message(
                Role::Assistant,
                vec![
                    tool_use("toolu_9", "Read", "{\"path\":\"a.txt\"}"),
                    tool_use("toolu_x", "", "{}"),
                ],
            ),
            message(
                Role::User,
                vec![
                    text("after"),
                    ContentBlock::ToolResult(ToolResult {
                        tool_use_id: "toolu_9".to_string(),
                        output: "done".to_string(),
                        is_error: true,
                    }),
                ],
            ),
        ];
        let body = encode(&request, &caps());
        assert!(body.contains(r#"{"type":"text","text":"one\n\ntwo"}"#));
        assert!(body.contains(
            r#"{"type":"tool_use","id":"toolu_9","name":"Read","input":{"path":"a.txt"}}"#
        ));
        assert!(!body.contains("toolu_x"), "id-less tool_use is dropped");
        // tool_result comes first even though the text block was first.
        assert!(body.contains(
            r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_9","content":"done","is_error":true},{"type":"text","text":"after"}]}"#
        ));

        let mut broken = request.clone();
        broken.messages = vec![message(
            Role::Assistant,
            vec![tool_use("toolu_1", "Read", "not json")],
        )];
        assert!(encode(&broken, &caps()).contains(r#""input":{}"#));
    }

    #[test]
    fn unpaired_tool_results_become_prefixed_text() {
        let mut request = request();
        request.messages = vec![message(
            Role::User,
            vec![ContentBlock::ToolResult(ToolResult {
                tool_use_id: "toolu_missing".to_string(),
                output: "42".to_string(),
                is_error: false,
            })],
        )];
        let body = encode(&request, &caps());
        assert!(!body.contains("tool_result"));
        assert!(body.contains("[unpaired tool result toolu_missing]"));
    }

    #[test]
    fn history_thinking_and_opaque_blocks_are_dropped() {
        let mut request = request();
        request.messages = vec![message(
            Role::Assistant,
            vec![
                ContentBlock::Thinking("why".to_string()),
                ContentBlock::Opaque(crate::neutral::Opaque {
                    provider: "google".to_string(),
                    item: json!({"x": 1}),
                }),
                text("visible"),
            ],
        )];
        let body = encode(&request, &caps());
        assert!(!body.contains("why"));
        assert!(!body.contains("\"x\""));
        assert!(
            body.contains(r#"{"role":"assistant","content":[{"type":"text","text":"visible"}]}"#)
        );
    }

    #[test]
    fn empty_messages_are_skipped() {
        let mut request = request();
        request.messages = vec![
            message(Role::User, vec![]),
            message(Role::User, vec![text("kept")]),
        ];
        assert!(encode(&request, &caps())
            .contains(r#""messages":[{"role":"user","content":[{"type":"text","text":"kept"}]}]"#));

        request.messages = vec![message(Role::User, vec![])];
        assert!(encode(&request, &caps()).ends_with(r#""messages":[]}"#));
    }

    #[test]
    fn images_are_forwarded_or_dropped_by_capability() {
        let mut request = request();
        request.messages = vec![message(
            Role::User,
            vec![
                ContentBlock::Image(ImageSource::Data {
                    media_type: "image/png".to_string(),
                    data: "QQ==".to_string(),
                }),
                ContentBlock::Image(ImageSource::Url("https://x/y.png".to_string())),
            ],
        )];
        let body = encode(&request, &caps());
        assert!(body.contains(
            r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"QQ=="}}"#
        ));
        assert!(
            body.contains(r#"{"type":"image","source":{"type":"url","url":"https://x/y.png"}}"#)
        );

        let mut no_images = caps();
        no_images.supports_images = false;
        assert!(!encode(&request, &no_images).contains("image"));
    }

    #[test]
    fn tools_are_dropped_when_the_catalog_disallows_them() {
        let mut request = request();
        request.tools = vec![ToolDef {
            name: "Read".to_string(),
            description: "r".to_string(),
            parameters: json!({"type": "object"}),
            strict: None,
            cache_control: None,
            format: None,
        }];
        request.tool_choice = ToolChoice::Named("Read".to_string());
        let mut no_tools = caps();
        no_tools.supports_tools = false;
        let body = encode(&request, &no_tools);
        assert!(!body.contains("tools"));
        assert!(!body.contains("tool_choice"));
    }

    #[test]
    fn streaming_sample_parses_like_the_spec() {
        let body = concat!(
            r#"{"type":"message_start","message":{"id":"msg_x","type":"message","role":"assistant","model":"claude-sonnet-5","content":[],"usage":{"input_tokens":21,"output_tokens":0}}}"#,
            "\n",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            "\n",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"thinking..."}}"#,
            "\n",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#,
            "\n",
            r#"{"type":"content_block_stop","index":0}"#,
            "\n",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            "\n",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"OK"}}"#,
            "\n",
            r#"{"type":"content_block_stop","index":1}"#,
            "\n",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            "\n",
            r#"{"type":"message_stop"}"#,
            "\n",
            r#"{"status":"stream_ended"}"#,
        );
        let response = decode_response(body.as_bytes()).unwrap();
        assert_eq!(response.text, "OK");
        assert_eq!(response.thinking, "thinking...");
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.stop_reason, StopReason::Stop);
        assert_eq!(response.usage.input_tokens, 21);
        assert_eq!(response.usage.output_tokens, 3);
        assert_eq!(response.usage.total_tokens, 24);
        assert_eq!(response.id.as_deref(), Some("msg_x"));
    }

    #[test]
    fn streaming_tool_sample_parses_arguments_and_stop_reason() {
        let body = concat!(
            r#"{"type":"message_start","message":{"id":"msg_x","role":"assistant","content":[],"usage":{"input_tokens":30,"output_tokens":0}}}"#,
            "\n",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_9","name":"Bash","input":{}}}"#,
            "\n",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\""}}"#,
            "\n",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":":\"ls\"}"}}"#,
            "\n",
            r#"{"type":"content_block_stop","index":0}"#,
            "\n",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}"#,
            "\n",
            r#"{"type":"message_stop"}"#,
            "\n",
            r#"{"status":"stream_ended"}"#,
        );
        let response = decode_response(body.as_bytes()).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "toolu_9");
        assert_eq!(response.tool_calls[0].name, "Bash");
        assert_eq!(response.tool_calls[0].arguments, "{\"command\":\"ls\"}");
        assert_eq!(response.stop_reason, StopReason::ToolCalls);
        assert_eq!(response.usage.output_tokens, 12);
    }

    #[test]
    fn stream_events_follow_the_spec_table() {
        let mut decoder = AnthropicStreamDecoder::new();
        let mut events = Vec::new();
        events.extend(
            decoder
                .push_line(r#"{"type":"message_start","message":{"id":"msg_x","usage":{"input_tokens":5}}}"#)
                .unwrap(),
        );
        events.extend(
            decoder
                .push_line(r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read"}}"#)
                .unwrap(),
        );
        events.extend(
            decoder
                .push_line(r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}"#)
                .unwrap(),
        );
        events.extend(
            decoder
                .push_line(r#"{"type":"content_block_stop","index":2}"#)
                .unwrap(),
        );
        events.extend(
            decoder
                .push_line(r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#)
                .unwrap(),
        );
        events.extend(decoder.push_line(r#"{"type":"message_stop"}"#).unwrap());
        assert!(matches!(events[0], StreamEvent::Start { .. }));
        assert!(matches!(events[1], StreamEvent::Usage(_)));
        assert!(matches!(events[2], StreamEvent::ToolStart { index: 0, .. }));
        assert!(matches!(
            events[3],
            StreamEvent::ToolArgsDelta { index: 0, .. }
        ));
        assert!(matches!(events[4], StreamEvent::ToolEnd { index: 0 }));
        assert!(matches!(events[5], StreamEvent::BlockStop { index: 2 }));
        assert!(matches!(
            events.last(),
            Some(StreamEvent::End {
                reason: StopReason::ToolCalls
            })
        ));
        assert!(decoder.saw_model_event());
        assert!(decoder.failure().is_none());
        assert!(decoder
            .push_line(
                r#"{"type":"content_block_start","index":9,"content_block":{"type":"text"}}"#
            )
            .unwrap()
            .is_empty());
    }

    #[test]
    fn deltas_without_the_matching_block_are_dropped() {
        let mut decoder = AnthropicStreamDecoder::new();
        let events = decoder
            .push_line(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}"#)
            .unwrap();
        assert!(events.is_empty());
        let events = decoder
            .push_line(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        let events = decoder
            .push_line(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}"#)
            .unwrap();
        assert!(
            events.is_empty(),
            "text delta cannot land in a thinking block"
        );
    }

    #[test]
    fn error_events_and_failure_wrappers_classify() {
        let mut decoder = AnthropicStreamDecoder::new();
        let events = decoder
            .push_line(
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
            )
            .unwrap();
        assert!(matches!(events[0], StreamEvent::Error(_)));
        assert_eq!(decoder.failure().unwrap().kind, ErrorKind::RateLimit);
        assert!(decoder.finish().unwrap().is_empty());

        let mut decoder = AnthropicStreamDecoder::new();
        let events = decoder
            .push_line(r#"{"status":{"failed":{"code":"upstream_http_400","message":"bad","retry_after":null}}}"#)
            .unwrap();
        assert!(matches!(events[0], StreamEvent::Error(_)));
        assert_eq!(decoder.failure().unwrap().kind, ErrorKind::Request);

        assert_eq!(
            decode_response(br#"{"status":{"failed":{"code":"upstream_http_429","message":"x"}}}"#)
                .unwrap_err()
                .kind,
            ErrorKind::RateLimit
        );
    }

    #[test]
    fn responses_shaped_upstream_is_accepted_and_not_mixed() {
        let body = concat!(
            r#"{"event":{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_9","name":"lookup"}}}"#,
            "\n",
            r#"{"event":{"type":"response.function_call_arguments.delta","delta":"{\"key\":\"v\"}"}}"#,
            "\n",
            r#"{"event":{"type":"response.function_call_arguments.done","arguments":"{\"key\":\"v\"}"}}"#,
            "\n",
            r#"{"event":{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":12,"output_tokens":8},"output":[]}}}"#,
            "\n",
            // A native event after the shape was fixed is dropped.
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
            "\n",
            r#"{"status":"stream_ended"}"#,
        );
        let response = decode_response(body.as_bytes()).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_9");
        assert_eq!(response.tool_calls[0].arguments, "{\"key\":\"v\"}");
        assert_eq!(response.stop_reason, StopReason::ToolCalls);
        assert_eq!(response.usage.input_tokens, 12);
        assert_eq!(response.usage.output_tokens, 8);
    }

    #[test]
    fn empty_and_garbage_bodies_are_transient_failures() {
        assert_eq!(decode_response(b"").unwrap_err().kind, ErrorKind::Transient);
        assert_eq!(
            decode_response(b"<html>nope</html>\n").unwrap_err().kind,
            ErrorKind::Transient
        );
        assert_eq!(
            decode_response(br#"{"status":"stream_ended"}"#)
                .unwrap_err()
                .kind,
            ErrorKind::Transient
        );
    }

    #[test]
    fn lone_message_without_stop_still_reduces() {
        let body = concat!(
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":2}}}"#,
            "\n",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
            "\n",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            "\n",
        );
        let response = decode_response(body.as_bytes()).unwrap();
        assert_eq!(response.text, "hi");
        assert_eq!(response.stop_reason, StopReason::Stop);
        assert_eq!(response.usage.output_tokens, 0);
    }
}
