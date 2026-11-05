//! `google` provider: neutral request → Gemini `generateContent` payload, and
//! `candidates[].content.parts[]` → neutral events (spec `22-provider-google.md`).
//!
//! The tool-schema dialect (`google_schema`) is applied here and nowhere else.

use std::collections::HashSet;

use rand::RngCore;
use serde_json::Value;

use crate::catalog::ModelCapabilities;
use crate::catalog::Provider as ProviderKind;
use crate::error::{ErrorKind, GatewayError};
use crate::neutral::{
    ContentBlock, ImageSource, Message, NeutralRequest, NeutralResponse, Role, StopReason,
    StreamEvent, SystemBlock, ToolUse, Usage,
};

use super::google_schema::{clean_schema, has_parameters, write_schema};
use super::{ProviderEncoder, StreamDecoder};

/// Generation-config temperature is a fixed gateway value, not the client's
/// (spec 22 §4). It must be written as a float literal.
pub const FIXED_TEMPERATURE: &str = "1.0";

/// Ceiling used when the catalogue has no `max_output_tokens` (spec 22 §4.2).
pub const FALLBACK_MAX_OUTPUT_TOKENS: i64 = 1_048_576;
/// Client budgets below this are raised to it (spec 25 §3.2).
pub const MIN_OUTPUT_TOKENS: i64 = 32;

/// `maxOutputTokens` for one request: `min(max(client, 32), catalogue max)`,
/// or the catalogue max when the client sent none (spec 22 §4.2, D22-1 A).
pub fn max_output_tokens(client: Option<i64>, catalog_max: Option<i64>) -> i64 {
    let ceiling = catalog_max
        .filter(|value| *value > 0)
        .unwrap_or(FALLBACK_MAX_OUTPUT_TOKENS);
    match client.filter(|value| *value > 0) {
        Some(value) => value.max(MIN_OUTPUT_TOKENS).min(ceiling),
        None => ceiling,
    }
}

pub struct GoogleProvider;

impl ProviderEncoder for GoogleProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Google
    }

    fn encode_request(
        &self,
        request: &NeutralRequest,
        capabilities: &ModelCapabilities,
    ) -> Result<Vec<u8>, GatewayError> {
        let mut out = String::with_capacity(512);
        out.push_str("{\"model\":");
        push_json_str(&mut out, &model_path(&request.model));

        // One part per system source: the client decoders keep the top-level
        // prompt and appended system messages as separate blocks (spec 22 §3).
        let system: Vec<&SystemBlock> = request
            .system
            .iter()
            .filter(|block| !block.text.is_empty())
            .collect();
        if !system.is_empty() {
            out.push_str(",\"systemInstruction\":{\"parts\":[");
            for (position, block) in system.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                out.push_str("{\"text\":");
                push_json_str(&mut out, &block.text);
                out.push('}');
            }
            out.push_str("]}");
        }

        out.push_str(
            ",\"generationConfig\":{\"candidateCount\":1,\"stopSequences\":[],\"temperature\":",
        );
        out.push_str(FIXED_TEMPERATURE);
        let budget = max_output_tokens(request.max_output_tokens, capabilities.max_output_tokens);
        out.push_str(",\"maxOutputTokens\":");
        out.push_str(&budget.to_string());
        out.push_str(",\"thinkingConfig\":{");
        let level = if capabilities.supports_thinking {
            thinking_level(
                request.reasoning.effort.as_deref(),
                &capabilities.supported_effort_levels,
            )
        } else {
            None
        };
        if let Some(level) = level {
            out.push_str("\"thinkingLevel\":\"");
            out.push_str(&level);
            out.push_str("\",");
        }
        out.push_str("\"includeThoughts\":true}}");

        let declarations = tool_declarations(request, capabilities);
        if !declarations.is_empty() {
            out.push_str(",\"tools\":[{\"functionDeclarations\":[");
            for (position, declaration) in declarations.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                out.push_str(declaration);
            }
            out.push_str("]}]");
            if let Some(config) = tool_config(&request.tool_choice) {
                out.push_str(",\"toolConfig\":");
                out.push_str(&config);
            }
        }

        out.push_str(",\"contents\":[");
        for (position, item) in contents(request, capabilities).iter().enumerate() {
            if position > 0 {
                out.push(',');
            }
            out.push_str("{\"parts\":[");
            for (part_position, part) in item.parts.iter().enumerate() {
                if part_position > 0 {
                    out.push(',');
                }
                out.push_str(part);
            }
            out.push_str("],\"role\":\"");
            out.push_str(item.role);
            out.push_str("\"}");
        }
        out.push_str("]}");
        Ok(out.into_bytes())
    }

    fn decode_response(&self, body: &[u8]) -> Result<NeutralResponse, GatewayError> {
        let mut response = NeutralResponse::default();
        let mut tool_calls: Vec<ToolUse> = Vec::new();
        let mut parsed_any = false;
        let mut saw_candidate = false;
        let mut blocked: Option<String> = None;
        let mut finish: Option<String> = None;
        for line in String::from_utf8_lossy(body).split('\n') {
            let cleaned = super::clean_line(line);
            if cleaned.is_empty() || !cleaned.starts_with('{') {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(cleaned) else {
                continue;
            };
            if super::is_stream_ended(&value) {
                continue;
            }
            if let Some(err) = super::parse_status_failed(&value) {
                return Err(err);
            }
            parsed_any = true;
            if let Some(usage) = usage_from(&value) {
                response.usage = usage;
            }
            if let Some(reason) = prompt_block(&value) {
                blocked = Some(reason);
            }
            let Some(candidates) = value.get("candidates").and_then(Value::as_array) else {
                continue;
            };
            saw_candidate = true;
            let Some(candidate) = candidates.first() else {
                continue;
            };
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                finish = Some(reason.to_string());
            }
            let Some(parts) = candidate
                .get("content")
                .and_then(|content| content.get("parts"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if text.is_empty() {
                        continue;
                    }
                    if part
                        .get("thought")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        response.thinking.push_str(text);
                    } else {
                        response.text.push_str(text);
                    }
                    continue;
                }
                if let Some(call) = part.get("functionCall") {
                    if let Some(tool) = tool_from_call(call, part) {
                        tool_calls.push(tool);
                    }
                    continue;
                }
                if part.get("inlineData").is_some() {
                    eprintln!("[zed-proxy] google: dropping inline image in response");
                }
            }
        }
        if !parsed_any {
            return Err(GatewayError::transient(
                None,
                "upstream returned no recognizable events",
            ));
        }
        if let Some(reason) = finish.as_deref() {
            if is_fatal_finish(reason) {
                return Err(GatewayError::transient(
                    None,
                    format!("upstream finishReason {reason}"),
                ));
            }
        }
        // A prompt that never produced a candidate is a content filter, with the
        // zeroed usage the spec asks for (spec 22 §8.3).
        if let Some(reason) = blocked {
            if response.text.is_empty() && response.thinking.is_empty() && tool_calls.is_empty() {
                eprintln!("[zed-proxy] google: prompt blocked ({reason})");
                response.stop_reason = StopReason::ContentFilter;
                response.usage = Usage::default();
                return Ok(response);
            }
        }
        if !saw_candidate {
            return Err(GatewayError::transient(
                None,
                "upstream returned no candidates",
            ));
        }
        response.stop_reason = map_finish(finish.as_deref(), !tool_calls.is_empty());
        response.tool_calls = tool_calls;
        Ok(response)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(GoogleStreamDecoder::default())
    }
}

/// Client effort → `thinkingLevel` (spec 22 §4.1). `None` means "do not write
/// the key at all".
pub fn thinking_level(
    effort: Option<&str>,
    levels: &[crate::catalog::EffortLevel],
) -> Option<String> {
    let effort = effort?.to_ascii_lowercase();
    let level = match effort.as_str() {
        "none" => return None,
        "minimal" => {
            if levels.iter().any(|level| level.value == "minimal") {
                "MINIMAL"
            } else {
                "LOW"
            }
        }
        "low" => "LOW",
        "medium" => "MEDIUM",
        _ => "HIGH",
    };
    Some(level.to_string())
}

/// `"models/" + slug`, without doubling the prefix (spec 22 §2.1).
pub fn model_path(slug: &str) -> String {
    format!("models/{}", slug.strip_prefix("models/").unwrap_or(slug))
}

fn push_json_str(out: &mut String, value: &str) {
    match serde_json::to_string(value) {
        Ok(encoded) => out.push_str(&encoded),
        Err(_) => out.push_str("\"\""),
    }
}

/// One `functionDeclarations[]` entry per surviving tool, already serialized.
fn tool_declarations(request: &NeutralRequest, capabilities: &ModelCapabilities) -> Vec<String> {
    let mut declarations = Vec::new();
    if !capabilities.supports_tools {
        return declarations;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for tool in &request.tools {
        if tool.name.is_empty() || !seen.insert(tool.name.as_str()) {
            continue;
        }
        let cleaned = clean_schema(&tool.parameters);
        let mut declaration = String::from("{\"name\":");
        push_json_str(&mut declaration, &tool.name);
        declaration.push_str(",\"description\":");
        push_json_str(&mut declaration, &tool.description);
        if has_parameters(&cleaned) {
            declaration.push_str(",\"parameters\":");
            write_schema(&mut declaration, &cleaned);
        }
        declaration.push('}');
        declarations.push(declaration);
    }
    declarations
}

fn tool_config(choice: &crate::neutral::ToolChoice) -> Option<String> {
    use crate::neutral::ToolChoice;
    // `auto` is the upstream default and is not sent (spec 22 §7 "缺省不输出",
    // §10A.1); the explicit modes are.
    let (mode, allowed) = match choice {
        ToolChoice::Unspecified | ToolChoice::Auto => return None,
        ToolChoice::None => ("NONE", None),
        ToolChoice::Required => ("ANY", None),
        ToolChoice::Named(name) => ("ANY", Some(name)),
    };
    let mut config = String::from("{\"functionCallingConfig\":{\"mode\":\"");
    config.push_str(mode);
    config.push('"');
    if let Some(name) = allowed {
        config.push_str(",\"allowedFunctionNames\":[");
        push_json_str(&mut config, name);
        config.push(']');
    }
    config.push_str("}}");
    Some(config)
}

struct Item {
    role: &'static str,
    parts: Vec<String>,
}

/// `contents[]`: messages keep their order, consecutive turns with the same
/// role merge into one item, and messages without parts are skipped
/// (spec 45 §4.6, which overrides D22-3's "only merge tool results").
fn contents(request: &NeutralRequest, capabilities: &ModelCapabilities) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    for (index, message) in request.messages.iter().enumerate() {
        let parts = message_parts(request, index, message, capabilities);
        if parts.is_empty() {
            continue;
        }
        let role = match message.role {
            Role::Assistant => "model",
            Role::User => "user",
        };
        match items.last_mut() {
            Some(last) if last.role == role => last.parts.extend(parts),
            _ => items.push(Item { role, parts }),
        }
    }
    // Gemini wants alternating turns; a history that starts with a model turn
    // gets an empty user turn in front (spec 45 §4.6).
    if items.first().is_some_and(|item| item.role == "model") {
        items.insert(
            0,
            Item {
                role: "user",
                parts: vec!["{\"text\":\"\"}".to_string()],
            },
        );
    }
    items
}

fn message_parts(
    request: &NeutralRequest,
    index: usize,
    message: &Message,
    capabilities: &ModelCapabilities,
) -> Vec<String> {
    let mut visible: Vec<String> = Vec::new();
    let mut calls: Vec<String> = Vec::new();
    let mut signature: Option<String> = None;
    for block in &message.parts {
        match block {
            ContentBlock::Text(text) => {
                if !text.is_empty() {
                    push_text_part(&mut visible, text, false);
                }
            }
            // History thinking is not replayed: without a signature the upstream
            // rejects a thought part, and the neutral block carries none
            // (spec 45 §4.5, which overrides 22 §5.3's `thought:true` row).
            ContentBlock::Thinking(_) => {}
            ContentBlock::Image(source) => match source {
                _ if !capabilities.supports_images => {}
                ImageSource::Data { media_type, data } => {
                    let mut part = String::from("{\"inlineData\":{\"mimeType\":");
                    push_json_str(&mut part, media_type);
                    part.push_str(",\"data\":");
                    push_json_str(&mut part, data);
                    part.push_str("}}");
                    visible.push(part);
                }
                ImageSource::Url(url) => {
                    eprintln!("[zed-proxy] google: dropping remote image {url}");
                }
            },
            ContentBlock::ToolUse(tool) => {
                if !tool.name.is_empty() {
                    // One signature belongs to one call (parallel calls carry it on
                    // the first only, spec 22 §5.3.3).
                    calls.push(function_call_part(tool, signature.as_deref()));
                    signature = None;
                }
            }
            ContentBlock::ToolResult(result) => {
                match result_name(request, index, &result.tool_use_id) {
                    Some(name) => calls.push(function_response_part(&name, result)),
                    None => {
                        // Gemini rejects a response with no matching call, so the
                        // result stays visible text instead (spec 45 §4.3 item 4).
                        eprintln!(
                            "[zed-proxy] google: unpaired tool result {}",
                            result.tool_use_id
                        );
                        let text = format!(
                            "[unpaired tool result {}]\n{}",
                            result.tool_use_id, result.output
                        );
                        push_text_part(&mut visible, &text, false);
                    }
                }
            }
            ContentBlock::Compaction(_) => {}
            ContentBlock::Opaque(opaque) => {
                if opaque.provider == "google" {
                    signature = opaque_signature(&opaque.item);
                }
            }
        }
    }
    // Assistant: text first, then the calls it introduced (spec 22 §5.2 item 2).
    // User: text parts keep their place ahead of the result parts.
    visible.extend(calls);
    visible
}

fn push_text_part(out: &mut Vec<String>, text: &str, thought: bool) {
    let mut part = String::from("{\"text\":");
    push_json_str(&mut part, text);
    if thought {
        part.push_str(",\"thought\":true");
    }
    part.push('}');
    out.push(part);
}

fn opaque_signature(item: &Value) -> Option<String> {
    match item {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Object(map) => map
            .get("signature")
            .or_else(|| map.get("thoughtSignature"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

/// Resolve a tool-result id to the function name: the nearest preceding
/// assistant turn wins, the whole request is the fallback (spec 22 §5.3.2).
fn result_name(request: &NeutralRequest, index: usize, id: &str) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    for message in request.messages[..index].iter().rev() {
        if message.role != Role::Assistant {
            continue;
        }
        for block in &message.parts {
            if let ContentBlock::ToolUse(tool) = block {
                if tool.id == id && !tool.name.is_empty() {
                    return Some(tool.name.clone());
                }
            }
        }
        // Only the nearest assistant turn is the "turn-local" lookup.
        break;
    }
    for message in &request.messages {
        for block in &message.parts {
            if let ContentBlock::ToolUse(tool) = block {
                if tool.id == id && !tool.name.is_empty() {
                    return Some(tool.name.clone());
                }
            }
        }
    }
    request
        .tool_uses
        .iter()
        .find(|tool| tool.id == id && !tool.name.is_empty())
        .map(|tool| tool.name.clone())
}

fn function_call_part(tool: &ToolUse, signature: Option<&str>) -> String {
    let arguments = serde_json::from_str::<Value>(&tool.arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Default::default()));
    let mut part = String::from("{\"functionCall\":{\"name\":");
    push_json_str(&mut part, &tool.name);
    part.push_str(",\"args\":");
    part.push_str(&arguments.to_string());
    part.push('}');
    let signature = signature.or(tool.signature.as_deref());
    if let Some(signature) = signature.filter(|value| !value.is_empty()) {
        part.push_str(",\"thoughtSignature\":");
        push_json_str(&mut part, signature);
    }
    part.push('}');
    part
}

fn function_response_part(name: &str, result: &crate::neutral::ToolResult) -> String {
    let mut part = String::from("{\"functionResponse\":{\"name\":");
    push_json_str(&mut part, name);
    part.push_str(",\"response\":{\"");
    part.push_str(if result.is_error { "error" } else { "result" });
    part.push_str("\":");
    push_json_str(&mut part, &result.output);
    part.push_str("}}}");
    part
}

fn usage_from(value: &Value) -> Option<Usage> {
    let map = value.get("usageMetadata")?.as_object()?;
    let input = map
        .get("promptTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = map
        .get("candidatesTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let reasoning = map
        .get("thoughtsTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cached = map
        .get("cachedContentTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .min(input)
        .max(0);
    let total = match map.get("totalTokenCount").and_then(Value::as_i64) {
        Some(total) if total > 0 => total,
        _ => input + output + reasoning,
    };
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: reasoning,
        cached_input_tokens: cached,
        total_tokens: total,
    })
}

fn prompt_block(value: &Value) -> Option<String> {
    value
        .get("promptFeedback")?
        .get("blockReason")?
        .as_str()
        .map(str::to_string)
}

fn is_fatal_finish(reason: &str) -> bool {
    matches!(reason, "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL")
}

fn map_finish(reason: Option<&str>, has_tools: bool) -> StopReason {
    match reason {
        Some("STOP") => {
            if has_tools {
                StopReason::ToolCalls
            } else {
                StopReason::Stop
            }
        }
        Some("MAX_TOKENS") => StopReason::Length,
        Some("SAFETY")
        | Some("RECITATION")
        | Some("BLOCKLIST")
        | Some("PROHIBITED_CONTENT")
        | Some("SPII") => StopReason::ContentFilter,
        Some("MALFORMED_FUNCTION_CALL") | Some("UNEXPECTED_TOOL_CALL") => StopReason::Unknown,
        _ => {
            if has_tools {
                StopReason::ToolCalls
            } else {
                StopReason::Stop
            }
        }
    }
}

fn tool_from_call(call: &Value, part: &Value) -> Option<ToolUse> {
    let name = call
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())?
        .to_string();
    let arguments = match call.get("args") {
        Some(Value::Object(_)) => call.get("args").cloned().unwrap_or_default(),
        _ => Value::Object(Default::default()),
    };
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(new_call_id);
    let signature = part
        .get("thoughtSignature")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Some(ToolUse {
        id,
        name,
        arguments: arguments.to_string(),
        signature,
        custom: false,
    })
}

/// Gemini does not give function calls an id; generate the neutral shape
/// (spec 45 §4.3). The client module owns the public helper, but providers may
/// not depend on client code.
fn new_call_id() -> String {
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("call_{hex}")
}

#[derive(Default)]
pub struct GoogleStreamDecoder {
    started: bool,
    saw_model_event: bool,
    finished: bool,
    failure: Option<GatewayError>,
    usage: Option<Usage>,
    finish: Option<String>,
    blocked: Option<String>,
    tool_index: u64,
    tool_calls: usize,
}

impl StreamDecoder for GoogleStreamDecoder {
    fn push_line(&mut self, line: &str) -> Result<Vec<StreamEvent>, GatewayError> {
        let cleaned = super::clean_line(line);
        if cleaned.is_empty() || !cleaned.starts_with('{') {
            return Ok(Vec::new());
        }
        let Ok(value) = serde_json::from_str::<Value>(cleaned) else {
            return Ok(Vec::new());
        };
        if super::is_stream_ended(&value) {
            return Ok(Vec::new());
        }
        if let Some(err) = super::parse_status_failed(&value) {
            self.failure = Some(err.clone());
            return Err(err);
        }
        let mut events = Vec::new();
        if let Some(usage) = usage_from(&value) {
            self.usage = Some(usage);
        }
        if let Some(reason) = prompt_block(&value) {
            // Emitted once, on the line that carries the block.
            if self.blocked.is_none() {
                let mut err = GatewayError::new(
                    ErrorKind::Request,
                    format!("upstream blocked the prompt ({reason})"),
                );
                err.code = Some("content_filter");
                events.push(StreamEvent::Error(err));
            }
            self.blocked = Some(reason);
        }
        // A line that parses but carries no candidate still counts as a model
        // event: the stream is not "empty" (spec 22 §9.1).
        self.saw_model_event = true;
        if !self.started {
            self.started = true;
            events.push(StreamEvent::Start { id: None });
        }
        let Some(candidate) = value
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
        else {
            return Ok(events);
        };
        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            self.finish = Some(reason.to_string());
        }
        let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        else {
            return Ok(events);
        };
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if text.is_empty() {
                    continue;
                }
                if part
                    .get("thought")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    events.push(StreamEvent::ThinkingDelta {
                        delta: text.to_string(),
                    });
                } else {
                    events.push(StreamEvent::TextDelta {
                        delta: text.to_string(),
                    });
                }
                continue;
            }
            if let Some(call) = part.get("functionCall") {
                let Some(tool) = tool_from_call(call, part) else {
                    continue;
                };
                let index = self.tool_index;
                self.tool_index += 1;
                self.tool_calls += 1;
                events.push(StreamEvent::ToolStart {
                    index,
                    id: tool.id,
                    name: tool.name,
                    custom: false,
                });
                events.push(StreamEvent::ToolArgsDelta {
                    index,
                    delta: tool.arguments,
                });
                events.push(StreamEvent::ToolEnd { index });
                continue;
            }
            if part.get("inlineData").is_some() {
                eprintln!("[zed-proxy] google: dropping inline image in stream");
            }
        }
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, GatewayError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        if let Some(err) = &self.failure {
            return Err(err.clone());
        }
        let mut events = Vec::new();
        if let Some(usage) = self.usage {
            events.push(StreamEvent::Usage(usage));
        }
        let reason = if self.blocked.is_some() {
            StopReason::ContentFilter
        } else {
            map_finish(self.finish.as_deref(), self.tool_calls > 0)
        };
        if let Some(reason) = self.finish.as_deref() {
            if is_fatal_finish(reason) {
                let err = GatewayError::transient(None, format!("upstream finishReason {reason}"));
                self.failure = Some(err.clone());
                return Err(err);
            }
        }
        events.push(StreamEvent::End { reason });
        Ok(events)
    }

    fn saw_model_event(&self) -> bool {
        self.saw_model_event
    }

    fn failure(&self) -> Option<GatewayError> {
        self.failure.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::EffortLevel;
    use crate::neutral::{SystemBlock, ToolChoice, ToolDef, ToolResult};
    use serde_json::json;

    fn level(value: &str) -> EffortLevel {
        EffortLevel {
            name: value.to_string(),
            value: value.to_string(),
            is_default: false,
            supports_disabling_thinking: false,
        }
    }

    fn caps() -> ModelCapabilities {
        ModelCapabilities {
            provider: ProviderKind::Google,
            max_output_tokens: Some(65_536),
            supports_tools: true,
            supports_images: true,
            supports_thinking: true,
            supports_disabling_thinking: false,
            supports_parallel_tool_calls: false,
            supports_server_side_compaction: false,
            supported_effort_levels: vec![level("low"), level("medium"), level("high")],
        }
    }

    fn encode(request: &NeutralRequest) -> String {
        String::from_utf8(GoogleProvider.encode_request(request, &caps()).unwrap()).unwrap()
    }

    fn text(value: &str) -> ContentBlock {
        ContentBlock::Text(value.to_string())
    }

    fn call(id: &str, name: &str, arguments: &str) -> ContentBlock {
        ContentBlock::ToolUse(ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
            signature: None,
            custom: false,
        })
    }

    fn result(id: &str, output: &str) -> ContentBlock {
        ContentBlock::ToolResult(ToolResult {
            tool_use_id: id.to_string(),
            output: output.to_string(),
            is_error: false,
            custom: false,
        })
    }

    fn message(role: Role, parts: Vec<ContentBlock>) -> Message {
        Message { role, parts }
    }

    fn tool(name: &str, description: &str, parameters: Value) -> ToolDef {
        ToolDef {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
            strict: None,
            cache_control: None,
            format: None,
        }
    }

    #[test]
    fn effort_ladder_and_none() {
        assert_eq!(thinking_level(None, &[]), None);
        assert_eq!(thinking_level(Some("none"), &[]), None);
        assert_eq!(thinking_level(Some("low"), &[]).as_deref(), Some("LOW"));
        assert_eq!(
            thinking_level(Some("medium"), &[]).as_deref(),
            Some("MEDIUM")
        );
        assert_eq!(thinking_level(Some("high"), &[]).as_deref(), Some("HIGH"));
        assert_eq!(thinking_level(Some("xhigh"), &[]).as_deref(), Some("HIGH"));
        assert_eq!(thinking_level(Some("max"), &[]).as_deref(), Some("HIGH"));
        assert_eq!(thinking_level(Some("bogus"), &[]).as_deref(), Some("HIGH"));
    }

    #[test]
    fn minimal_depends_on_the_catalog_levels() {
        assert_eq!(
            thinking_level(Some("minimal"), &[level("low")]).as_deref(),
            Some("LOW")
        );
        assert_eq!(
            thinking_level(Some("minimal"), &[level("minimal"), level("low")]).as_deref(),
            Some("MINIMAL")
        );
    }

    #[test]
    fn model_path_has_a_single_prefix() {
        assert_eq!(model_path("gemini-4-argon"), "models/gemini-4-argon");
        assert_eq!(model_path("models/gemini-4-argon"), "models/gemini-4-argon");
    }

    #[test]
    fn request_skeleton_matches_the_spec_byte_for_byte() {
        let request = NeutralRequest {
            model: "gemini-3-flash".to_string(),
            messages: vec![message(Role::User, vec![text("ping")])],
            ..Default::default()
        };
        assert_eq!(
            encode(&request),
            r#"{"model":"models/gemini-3-flash","generationConfig":{"candidateCount":1,"stopSequences":[],"temperature":1.0,"maxOutputTokens":65536,"thinkingConfig":{"includeThoughts":true}},"contents":[{"parts":[{"text":"ping"}],"role":"user"}]}"#
        );
        let body = encode(&request);
        for banned in [
            "\"stream\"",
            "thinkingBudget",
            "thinkingLevel",
            "topP",
            "topK",
            "safetySettings",
        ] {
            assert!(!body.contains(banned), "{banned} in {body}");
        }
    }

    #[test]
    fn max_output_tokens_clamps_to_the_catalog_and_defaults_to_it() {
        assert_eq!(max_output_tokens(Some(200_000), Some(65_536)), 65_536);
        assert_eq!(max_output_tokens(Some(8_192), Some(65_536)), 8_192);
        assert_eq!(max_output_tokens(Some(1), Some(65_536)), 32);
        assert_eq!(max_output_tokens(None, Some(65_536)), 65_536);
        assert_eq!(max_output_tokens(Some(200_000), None), 200_000);
        assert_eq!(max_output_tokens(None, None), FALLBACK_MAX_OUTPUT_TOKENS);
        assert_eq!(max_output_tokens(Some(1), None), MIN_OUTPUT_TOKENS);
        // Non-positive client values count as "not given".
        assert_eq!(max_output_tokens(Some(0), Some(65_536)), 65_536);
    }

    #[test]
    fn generation_config_carries_the_clamped_budget() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(Role::User, vec![text("hi")])],
            max_output_tokens: Some(200_000),
            ..Default::default()
        };
        let body = encode(&request);
        assert!(
            body.contains(r#""temperature":1.0,"maxOutputTokens":65536,"thinkingConfig""#),
            "{body}"
        );
        assert!(!body.contains("max_output_tokens"), "{body}");
    }

    #[test]
    fn thinking_without_level_still_sends_include_thoughts() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(Role::User, vec![text("hi")])],
            ..Default::default()
        };
        let body = encode(&request);
        assert!(body.contains(r#""thinkingConfig":{"includeThoughts":true}"#));
        assert!(!body.contains("thinkingBudget"));
        let request = NeutralRequest {
            reasoning: crate::neutral::Reasoning {
                effort: Some("none".to_string()),
                extra: None,
            },
            ..request
        };
        assert!(!encode(&request).contains("thinkingLevel"));
    }

    #[test]
    fn thinking_level_is_uppercase_and_never_minimal_without_the_catalog() {
        let base = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(Role::User, vec![text("hi")])],
            ..Default::default()
        };
        for (effort, expected) in [
            ("minimal", "LOW"),
            ("low", "LOW"),
            ("medium", "MEDIUM"),
            ("high", "HIGH"),
            ("xhigh", "HIGH"),
        ] {
            let request = NeutralRequest {
                reasoning: crate::neutral::Reasoning {
                    effort: Some(effort.to_string()),
                    extra: None,
                },
                ..base.clone()
            };
            let body = encode(&request);
            assert!(
                body.contains(&format!(r#""thinkingLevel":"{expected}""#)),
                "{effort} -> {body}"
            );
            assert!(!body.contains("MINIMAL"), "{body}");
            assert!(!body.contains("thinkingBudget"), "{body}");
        }
    }

    #[test]
    fn thinking_level_follows_the_catalog_minimal_level() {
        let mut capabilities = caps();
        capabilities.supported_effort_levels = vec![level("minimal"), level("low")];
        let request = NeutralRequest {
            model: "gemini-3-flash".to_string(),
            messages: vec![message(Role::User, vec![text("hi")])],
            reasoning: crate::neutral::Reasoning {
                effort: Some("minimal".to_string()),
                extra: None,
            },
            ..Default::default()
        };
        let body = String::from_utf8(
            GoogleProvider
                .encode_request(&request, &capabilities)
                .unwrap(),
        )
        .unwrap();
        assert!(body.contains(r#""thinkingLevel":"MINIMAL""#), "{body}");
    }

    #[test]
    fn supports_thinking_false_omits_the_level_but_keeps_include_thoughts() {
        let mut capabilities = caps();
        capabilities.supports_thinking = false;
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(Role::User, vec![text("hi")])],
            reasoning: crate::neutral::Reasoning {
                effort: Some("high".to_string()),
                extra: None,
            },
            ..Default::default()
        };
        let body = String::from_utf8(
            GoogleProvider
                .encode_request(&request, &capabilities)
                .unwrap(),
        )
        .unwrap();
        assert!(!body.contains("thinkingLevel"), "{body}");
        assert!(body.contains(r#""includeThoughts":true"#), "{body}");
    }

    #[test]
    fn system_sources_stay_separate_parts_and_empty_ones_are_skipped() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            system: vec![
                SystemBlock {
                    text: "top".to_string(),
                    cache_control: None,
                },
                SystemBlock {
                    text: String::new(),
                    cache_control: None,
                },
                SystemBlock {
                    text: "message level".to_string(),
                    cache_control: Some(json!({"type": "ephemeral"})),
                },
            ],
            messages: vec![message(Role::User, vec![text("hi")])],
            ..Default::default()
        };
        let body = encode(&request);
        assert!(
            body.contains(
                r#""systemInstruction":{"parts":[{"text":"top"},{"text":"message level"}]}"#
            ),
            "{body}"
        );
        assert!(!body.contains("cache_control"), "{body}");
    }

    #[test]
    fn no_system_means_no_system_instruction() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(Role::User, vec![text("hi")])],
            ..Default::default()
        };
        assert!(!encode(&request).contains("systemInstruction"));
    }

    #[test]
    fn contents_roles_and_the_full_tool_round_trip() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            system: vec![SystemBlock {
                text: "Be brief".to_string(),
                cache_control: None,
            }],
            messages: vec![
                message(Role::User, vec![text("ls")]),
                message(
                    Role::Assistant,
                    vec![call("call_0123456789abcdef", "Bash", r#"{"command":"ls"}"#)],
                ),
                message(Role::User, vec![result("call_0123456789abcdef", "a.txt")]),
            ],
            tools: vec![tool(
                "Bash",
                "Run a command",
                json!({"type": "object", "properties": {"command": {"type": "string"}, "env": {"type": "object"}}, "required": ["command", "env"]}),
            )],
            tool_choice: ToolChoice::Auto,
            reasoning: crate::neutral::Reasoning {
                effort: Some("medium".to_string()),
                extra: None,
            },
            ..Default::default()
        };
        assert_eq!(
            encode(&request),
            r#"{"model":"models/gemini-4-argon","systemInstruction":{"parts":[{"text":"Be brief"}]},"generationConfig":{"candidateCount":1,"stopSequences":[],"temperature":1.0,"maxOutputTokens":65536,"thinkingConfig":{"thinkingLevel":"MEDIUM","includeThoughts":true}},"tools":[{"functionDeclarations":[{"name":"Bash","description":"Run a command","parameters":{"type":"object","properties":{"command":{"type":"string"},"env":{"type":"object"}},"required":["command","env"]}}]}],"contents":[{"parts":[{"text":"ls"}],"role":"user"},{"parts":[{"functionCall":{"name":"Bash","args":{"command":"ls"}}}],"role":"model"},{"parts":[{"functionResponse":{"name":"Bash","response":{"result":"a.txt"}}}],"role":"user"}]}"#
        );
    }

    #[test]
    fn tool_results_merge_into_one_user_turn_and_keep_order() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![
                message(Role::User, vec![text("go")]),
                message(
                    Role::Assistant,
                    vec![
                        call("call_0000000000000001", "A", "{}"),
                        call("call_0000000000000002", "B", "{}"),
                    ],
                ),
                message(Role::User, vec![result("call_0000000000000001", "one")]),
                message(Role::User, vec![result("call_0000000000000002", "two")]),
            ],
            ..Default::default()
        };
        let body = encode(&request);
        assert!(
            body.contains(
                r##"{"parts":[{"functionResponse":{"name":"A","response":{"result":"one"}}},{"functionResponse":{"name":"B","response":{"result":"two"}}}],"role":"user"}"##
            ),
            "{body}"
        );
    }

    #[test]
    fn unpaired_tool_results_become_prefixed_text() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(
                Role::User,
                vec![result("call_deadbeefdeadbeef", "x")],
            )],
            ..Default::default()
        };
        let body = encode(&request);
        assert!(
            body.contains(r#"{"text":"[unpaired tool result call_deadbeefdeadbeef]\nx"}"#),
            "{body}"
        );
        assert!(!body.contains("functionResponse"), "{body}");
    }

    #[test]
    fn failed_tool_results_use_the_error_key() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![
                message(
                    Role::Assistant,
                    vec![call("call_0000000000000001", "Bash", "{}")],
                ),
                message(
                    Role::User,
                    vec![ContentBlock::ToolResult(ToolResult {
                        tool_use_id: "call_0000000000000001".to_string(),
                        output: "denied".to_string(),
                        is_error: true,
                        custom: false,
                    })],
                ),
            ],
            ..Default::default()
        };
        let body = encode(&request);
        assert!(
            body.contains(r#"{"functionResponse":{"name":"Bash","response":{"error":"denied"}}}"#),
            "{body}"
        );
    }

    #[test]
    fn assistant_text_comes_before_the_calls() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(
                Role::Assistant,
                vec![
                    call("call_0000000000000001", "Bash", "{}"),
                    text("thinking out loud"),
                ],
            )],
            ..Default::default()
        };
        let body = encode(&request);
        let text_at = body.find("thinking out loud").unwrap();
        let call_at = body.find("functionCall").unwrap();
        assert!(text_at < call_at, "{body}");
        assert!(body.contains(r#""role":"model""#), "{body}");
    }

    #[test]
    fn thought_signature_is_attached_next_to_the_call() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(
                Role::Assistant,
                vec![
                    ContentBlock::Opaque(crate::neutral::Opaque {
                        provider: "google".to_string(),
                        item: json!({"signature": "sig-abc"}),
                    }),
                    call("call_0000000000000001", "Bash", r#"{"command":"ls"}"#),
                ],
            )],
            ..Default::default()
        };
        let body = encode(&request);
        assert!(
            body.contains(
                r#"{"functionCall":{"name":"Bash","args":{"command":"ls"}},"thoughtSignature":"sig-abc"}"#
            ),
            "{body}"
        );
    }

    #[test]
    fn empty_tool_arguments_become_an_object() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(
                Role::Assistant,
                vec![call("call_0000000000000001", "Bash", "not json")],
            )],
            ..Default::default()
        };
        assert!(
            encode(&request).contains(r#"{"functionCall":{"name":"Bash","args":{}}}"#),
            "{}",
            encode(&request)
        );
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(
                Role::Assistant,
                vec![call("call_0000000000000001", "Bash", "[1,2]")],
            )],
            ..Default::default()
        };
        assert!(encode(&request).contains(r#""args":{}"#));
    }

    #[test]
    fn empty_and_duplicate_tool_names_are_dropped() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(Role::User, vec![text("hi")])],
            tools: vec![
                tool(
                    "",
                    "no name",
                    json!({"type": "object", "properties": {"a": {"type": "string"}}}),
                ),
                tool(
                    "A",
                    "first",
                    json!({"type": "object", "properties": {"a": {"type": "string"}}}),
                ),
                tool(
                    "A",
                    "second",
                    json!({"type": "object", "properties": {"b": {"type": "string"}}}),
                ),
            ],
            ..Default::default()
        };
        let body = encode(&request);
        assert_eq!(body.matches("\"name\":\"A\"").count(), 1, "{body}");
        assert!(body.contains("\"description\":\"first\""), "{body}");
        assert!(!body.contains("no name"), "{body}");
    }

    #[test]
    fn tool_choice_maps_to_tool_config() {
        let tool_def = tool(
            "A",
            "a",
            json!({"type": "object", "properties": {"x": {"type": "string"}}}),
        );
        let base = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(Role::User, vec![text("hi")])],
            tools: vec![tool_def.clone()],
            ..Default::default()
        };
        // `auto` is the upstream default and is not sent (spec 22 §7, §10A.1).
        assert!(!encode(&base).contains("toolConfig"));
        for (choice, fragment) in [
            (ToolChoice::None, r#""mode":"NONE""#),
            (ToolChoice::Required, r#""mode":"ANY""#),
        ] {
            let request = NeutralRequest {
                tool_choice: choice,
                ..base.clone()
            };
            assert!(encode(&request).contains(fragment), "{}", encode(&request));
        }
        let request = NeutralRequest {
            tool_choice: ToolChoice::Named("A".to_string()),
            ..base.clone()
        };
        assert!(encode(&request)
            .contains(r#""functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["A"]}"#));
        let request = NeutralRequest {
            tools: vec![],
            tool_choice: ToolChoice::Required,
            ..base
        };
        let body = encode(&request);
        assert!(!body.contains("tools"), "{body}");
        assert!(!body.contains("toolConfig"), "{body}");
    }

    #[test]
    fn supports_tools_false_drops_declarations() {
        let mut capabilities = caps();
        capabilities.supports_tools = false;
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(Role::User, vec![text("hi")])],
            tools: vec![tool(
                "A",
                "a",
                json!({"type": "object", "properties": {"x": {"type": "string"}}}),
            )],
            tool_choice: ToolChoice::Required,
            ..Default::default()
        };
        let body = String::from_utf8(
            GoogleProvider
                .encode_request(&request, &capabilities)
                .unwrap(),
        )
        .unwrap();
        assert!(!body.contains("functionDeclarations"), "{body}");
        assert!(!body.contains("toolConfig"), "{body}");
    }

    #[test]
    fn the_two_real_400_schemas_pass_through_the_provider() {
        let read_file = tool(
            "read_file",
            "Read a file",
            json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }),
        );
        let write_env = tool(
            "write_env",
            "Write env vars",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "vars": {"type": "object", "propertyNames": {"pattern": "^[A-Z_]+$"}, "additionalProperties": {"type": "string"}}
                },
                "required": ["path", "vars"]
            }),
        );
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(Role::User, vec![text("go")])],
            tools: vec![read_file, write_env],
            ..Default::default()
        };
        let body = encode(&request);
        for banned in ["$schema", "propertyNames", "additionalProperties"] {
            assert!(!body.contains(banned), "{banned} in {body}");
        }
        assert!(
            body.contains(r#"{"name":"read_file","description":"Read a file","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}"#),
            "{body}"
        );
        assert!(
            body.contains(r#"{"name":"write_env","description":"Write env vars","parameters":{"type":"object","properties":{"path":{"type":"string"},"vars":{"type":"object"}},"required":["path","vars"]}}"#),
            "{body}"
        );
    }

    #[test]
    fn a_property_named_like_a_keyword_survives() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(Role::User, vec![text("go")])],
            tools: vec![tool(
                "A",
                "a",
                json!({"type": "object", "properties": {"format": {"type": "string"}, "$schema": {"type": "boolean"}}}),
            )],
            ..Default::default()
        };
        let body = encode(&request);
        // Client property order is preserved (serde_json `preserve_order`).
        assert!(
            body.contains(
                r#""properties":{"format":{"type":"string"},"$schema":{"type":"boolean"}}"#
            ),
            "{body}"
        );
    }

    #[test]
    fn history_thinking_is_not_replayed() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![
                message(Role::User, vec![text("q")]),
                message(
                    Role::Assistant,
                    vec![
                        ContentBlock::Thinking("secret reasoning".to_string()),
                        text("answer"),
                    ],
                ),
            ],
            ..Default::default()
        };
        let body = encode(&request);
        assert!(!body.contains("secret reasoning"), "{body}");
        assert!(!body.contains("thought"), "{body}");
        assert!(body.contains(r#"{"text":"answer"}"#), "{body}");
    }

    #[test]
    fn a_blocked_prompt_with_text_still_reports_the_text() {
        let body = r#"{"promptFeedback":{"blockReason":"SAFETY"},"candidates":[{"content":{"parts":[{"text":"partial"}],"role":"model"},"finishReason":"SAFETY"}]}"#;
        let response = decode(body);
        assert_eq!(response.text, "partial");
        assert_eq!(response.stop_reason, StopReason::ContentFilter);
    }

    #[test]
    fn parameters_without_properties_are_omitted() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(Role::User, vec![text("go")])],
            tools: vec![tool("A", "a", json!({"type": "object", "properties": {}}))],
            ..Default::default()
        };
        let body = encode(&request);
        assert!(body.contains(r#"{"name":"A","description":"a"}"#), "{body}");
        assert!(!body.contains("\"parameters\""), "{body}");
    }

    #[test]
    fn empty_contents_are_still_an_array() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            ..Default::default()
        };
        assert!(encode(&request).ends_with(r#""contents":[]}"#));
    }

    #[test]
    fn a_history_starting_with_a_model_turn_gets_an_empty_user_turn() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(
                Role::Assistant,
                vec![call("call_0000000000000001", "A", "{}")],
            )],
            ..Default::default()
        };
        let body = encode(&request);
        assert!(
            body.contains(r#""contents":[{"parts":[{"text":""}],"role":"user"},{"parts":[{"functionCall":{"name":"A","args":{}}}],"role":"model"}]"#),
            "{body}"
        );
    }

    #[test]
    fn images_data_urls_become_inline_data_and_remote_ones_are_dropped() {
        let request = NeutralRequest {
            model: "gemini-4-argon".to_string(),
            messages: vec![message(
                Role::User,
                vec![
                    ContentBlock::Image(ImageSource::Data {
                        media_type: "image/png".to_string(),
                        data: "QUJD".to_string(),
                    }),
                    ContentBlock::Image(ImageSource::Url("https://x/y.png".to_string())),
                ],
            )],
            ..Default::default()
        };
        let body = encode(&request);
        assert!(
            body.contains(r#"{"inlineData":{"mimeType":"image/png","data":"QUJD"}}"#),
            "{body}"
        );
        assert!(!body.contains("https://x/y.png"), "{body}");
        let mut capabilities = caps();
        capabilities.supports_images = false;
        let body = String::from_utf8(
            GoogleProvider
                .encode_request(&request, &capabilities)
                .unwrap(),
        )
        .unwrap();
        assert!(body.ends_with(r#""contents":[]}"#), "{body}");
    }

    fn decode(body: &str) -> NeutralResponse {
        GoogleProvider.decode_response(body.as_bytes()).unwrap()
    }

    #[test]
    fn non_stream_response_splits_thoughts_from_text() {
        let body = concat!(
            r#"{"candidates":[{"content":{"parts":[{"text":"think-a","thought":true},{"text":"OK"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":2,"totalTokenCount":9},"modelVersion":"gemini-4-argon"}"#,
            "\n",
            r#"{"candidates":[{"content":{"parts":[{"text":""}]},"index":0}]}"#,
            "\n",
            r#"{"status":"stream_ended"}"#,
            "\n"
        );
        let response = decode(body);
        assert_eq!(response.thinking, "think-a");
        assert_eq!(response.text, "OK");
        assert_eq!(response.stop_reason, StopReason::Stop);
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 7,
                output_tokens: 2,
                reasoning_tokens: 0,
                cached_input_tokens: 0,
                total_tokens: 9
            }
        );
    }

    #[test]
    fn non_stream_finish_reasons_and_usage_details() {
        let body = concat!(
            r#"{"candidates":[{"content":{"parts":[{"text":"x"}]},"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"thoughtsTokenCount":3,"cachedContentTokenCount":4,"totalTokenCount":18}}"#,
            "\n"
        );
        let response = decode(body);
        assert_eq!(response.stop_reason, StopReason::Length);
        assert_eq!(response.usage.reasoning_tokens, 3);
        assert_eq!(response.usage.cached_input_tokens, 4);
        assert_eq!(response.usage.total_tokens, 18);

        let body = r#"{"candidates":[{"content":{"parts":[]},"finishReason":"SAFETY"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":0}}"#;
        assert_eq!(decode(body).stop_reason, StopReason::ContentFilter);

        let body = r#"{"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"thoughtsTokenCount":2}}"#;
        let response = decode(body);
        assert_eq!(response.usage.total_tokens, 6);
    }

    #[test]
    fn non_stream_tool_calls_are_parsed() {
        let body = concat!(
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"path":"a.txt"}},"thoughtSignature":"sig"}]},"finishReason":"STOP"}]}"#,
            "\n"
        );
        let response = decode(body);
        assert_eq!(response.tool_calls.len(), 1);
        let tool = &response.tool_calls[0];
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.arguments, r#"{"path":"a.txt"}"#);
        assert_eq!(tool.signature.as_deref(), Some("sig"));
        assert_eq!(tool.id.len(), 21);
        assert!(tool.id.starts_with("call_"));
        assert_eq!(response.stop_reason, StopReason::ToolCalls);
    }

    #[test]
    fn non_stream_uses_the_upstream_call_id_when_present() {
        let body = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"abc","name":"A","args":{}}}]},"finishReason":"STOP"}]}"#;
        assert_eq!(decode(body).tool_calls[0].id, "abc");
    }

    #[test]
    fn non_stream_prompt_block_is_a_content_filter() {
        let body =
            r#"{"promptFeedback":{"blockReason":"SAFETY"},"usageMetadata":{"promptTokenCount":9}}"#;
        let response = decode(body);
        assert_eq!(response.stop_reason, StopReason::ContentFilter);
        assert_eq!(response.usage, Usage::default());
    }

    #[test]
    fn non_stream_without_events_is_a_transient_error() {
        assert!(GoogleProvider.decode_response(b"").is_err());
        assert!(GoogleProvider
            .decode_response(br#"{"modelVersion":"gemini-4-argon"}"#)
            .is_err());
    }

    #[test]
    fn non_stream_malformed_function_call_is_an_error() {
        let body =
            r#"{"candidates":[{"content":{"parts":[]},"finishReason":"MALFORMED_FUNCTION_CALL"}]}"#;
        assert!(GoogleProvider.decode_response(body.as_bytes()).is_err());
    }

    #[test]
    fn stream_decoder_emits_text_then_tool_then_usage_then_end() {
        let mut decoder = GoogleStreamDecoder::default();
        let mut events = Vec::new();
        for line in [
            r#"{"candidates":[{"content":{"parts":[{"text":"checking"}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"Bash","args":{"command":"ls"}}}]},"index":0}]}"#,
            r#"{"candidates":[{"content":{"parts":[]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":6,"totalTokenCount":16}}"#,
            r#"{"status":"stream_ended"}"#,
        ] {
            events.extend(decoder.push_line(line).unwrap());
        }
        assert!(decoder.saw_model_event());
        events.extend(decoder.finish().unwrap());
        assert!(matches!(events.first(), Some(StreamEvent::Start { .. })));
        assert!(events.contains(&StreamEvent::TextDelta {
            delta: "checking".to_string()
        }));
        let tool_start = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::ToolStart {
                    index, id, name, ..
                } => Some((*index, id.clone(), name.clone())),
                _ => None,
            })
            .expect("tool start");
        assert_eq!(tool_start.0, 0);
        assert_eq!(tool_start.2, "Bash");
        assert_eq!(tool_start.1.len(), 21);
        assert!(tool_start.1.starts_with("call_"));
        assert!(events.contains(&StreamEvent::ToolArgsDelta {
            index: 0,
            delta: r#"{"command":"ls"}"#.to_string()
        }));
        assert!(events.contains(&StreamEvent::ToolEnd { index: 0 }));
        assert!(events.contains(&StreamEvent::Usage(Usage {
            input_tokens: 10,
            output_tokens: 6,
            reasoning_tokens: 0,
            cached_input_tokens: 0,
            total_tokens: 16
        })));
        assert_eq!(
            events.last(),
            Some(&StreamEvent::End {
                reason: StopReason::ToolCalls
            })
        );
    }

    #[test]
    fn stream_decoder_keeps_thinking_and_text_apart_and_uses_the_last_usage() {
        let mut decoder = GoogleStreamDecoder::default();
        let mut events = Vec::new();
        for line in [
            r#"{"candidates":[{"content":{"parts":[{"text":"hmm","thought":true}]}}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1}}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":" hi"}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[]},"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}}"#,
        ] {
            events.extend(decoder.push_line(line).unwrap());
        }
        events.extend(decoder.finish().unwrap());
        assert!(events.contains(&StreamEvent::ThinkingDelta {
            delta: "hmm".to_string()
        }));
        assert!(events.contains(&StreamEvent::TextDelta {
            delta: " hi".to_string()
        }));
        assert!(events.contains(&StreamEvent::Usage(Usage {
            input_tokens: 1,
            output_tokens: 2,
            reasoning_tokens: 0,
            cached_input_tokens: 0,
            total_tokens: 3
        })));
        assert_eq!(
            events.last(),
            Some(&StreamEvent::End {
                reason: StopReason::Length
            })
        );
    }

    #[test]
    fn stream_decoder_reports_upstream_failures() {
        let mut decoder = GoogleStreamDecoder::default();
        let error = decoder
            .push_line(r#"{"status":{"failed":{"code":"upstream_http_400","message":"bad thing","retry_after":null}}}"#)
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Request);
        assert_eq!(decoder.failure().map(|err| err.status), Some(Some(400)));
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn stream_decoder_ignores_noise_and_blank_lines() {
        let mut decoder = GoogleStreamDecoder::default();
        assert!(decoder.push_line("").unwrap().is_empty());
        assert!(decoder.push_line("ping").unwrap().is_empty());
        assert!(decoder
            .push_line(r#"{"status":"stream_ended"}"#)
            .unwrap()
            .is_empty());
        assert!(!decoder.saw_model_event());
        // A parsed line with no candidate is not an empty stream (spec 22 §9.1).
        assert!(!decoder
            .push_line(r#"{"usageMetadata":{"promptTokenCount":2}}"#)
            .unwrap()
            .is_empty());
        assert!(decoder.saw_model_event());
        let events = decoder.finish().unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::Usage(Usage {
                    input_tokens: 2,
                    total_tokens: 2,
                    ..Default::default()
                }),
                StreamEvent::End {
                    reason: StopReason::Stop
                }
            ]
        );
    }

    #[test]
    fn stream_prompt_block_is_a_content_filter_error() {
        let mut decoder = GoogleStreamDecoder::default();
        let events = decoder
            .push_line(r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#)
            .unwrap();
        assert!(matches!(
            events.iter().find(|event| matches!(event, StreamEvent::Error(_))),
            Some(StreamEvent::Error(err)) if err.code == Some("content_filter")
        ));
        let events = decoder.finish().unwrap();
        assert_eq!(
            events.last(),
            Some(&StreamEvent::End {
                reason: StopReason::ContentFilter
            })
        );
    }

    #[test]
    fn stream_args_are_a_single_delta_even_for_empty_objects() {
        let mut decoder = GoogleStreamDecoder::default();
        let events = decoder
            .push_line(r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"A"}}]}}]}"#)
            .unwrap();
        assert!(events.contains(&StreamEvent::ToolArgsDelta {
            index: 0,
            delta: "{}".to_string()
        }));
    }
}
