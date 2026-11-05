//! `open_ai` provider: neutral request → OpenAI Responses payload, and
//! Responses events → neutral events (spec `20-provider-openai.md`).
//!
//! Implementation task owns this file. The interface below is fixed and must not
//! change; only the bodies are filled in.

use std::collections::HashSet;

use serde_json::Value;

use crate::catalog::ModelCapabilities;
use crate::catalog::Provider as ProviderKind;
use crate::error::GatewayError;
use crate::neutral::{
    ContentBlock, ImageSource, NeutralRequest, NeutralResponse, Role, StopReason, StreamEvent,
    SystemBlock, ToolChoice, ToolDef, ToolResult, ToolUse, Usage,
};

use super::{clean_line, is_stream_ended, parse_status_failed, ProviderEncoder, StreamDecoder};

/// `reasoning.effort` values the GPT-5.6 family accepts (spec 20 §8.3).
pub const GPT_56_EFFORTS: [&str; 5] = ["none", "low", "medium", "high", "xhigh"];

/// Client top-level keys the provider writes itself (spec 20 §2.2); `input` and
/// `tool_choice` are normalized in place inside the passthrough loop.
const NATIVE_DROPPED: [&str; 8] = [
    "model",
    "stream",
    "reasoning",
    "reasoning_effort",
    "max_output_tokens",
    "max_completion_tokens",
    "max_tokens",
    "tools",
];

const DEFAULT_TOOL_PARAMETERS: &str = "{\"type\":\"object\",\"properties\":{}}";
const DEFAULT_ARGUMENTS: &str = "{}";

pub struct OpenAiProvider;

impl ProviderEncoder for OpenAiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn encode_request(
        &self,
        request: &NeutralRequest,
        capabilities: &ModelCapabilities,
    ) -> Result<Vec<u8>, GatewayError> {
        let model = request.model.as_str();
        let client_effort = request
            .reasoning
            .effort
            .as_deref()
            .map(|effort| effort.trim().to_ascii_lowercase())
            .filter(|effort| !effort.is_empty());
        validate_effort(model, client_effort.as_deref())?;
        let effort = effective_effort(model, client_effort.as_deref());
        // Only the Responses client fills `passthrough`, so a non-empty list is
        // the native shape (spec 20 §2.2). Bytes are identical for empty lists.
        let native = !request.passthrough.is_empty();
        let named = request.tool_choice.name();
        let tool_items = if capabilities.supports_tools {
            tool_items(&request.tools, native, named)
        } else {
            Vec::new()
        };
        let input = input_items(request, native, capabilities.supports_images);
        let no_effort_levels = capabilities.supported_effort_levels.is_empty();
        let budget = output_budget(
            model,
            effort.as_deref(),
            request.max_output_tokens,
            capabilities.max_output_tokens,
            no_effort_levels,
            true,
        );

        let mut out = String::with_capacity(512);
        out.push_str("{\"model\":");
        out.push_str(&json_str(model));
        out.push_str(",\"stream\":true");
        let tools = if tool_items.is_empty() {
            None
        } else {
            Some(format!("[{}]", tool_items.join(",")))
        };
        // `tool_choice` only matters with tools: it is dropped when the model
        // takes none or the declaration list ended up empty. An absent client
        // key stays absent (spec 20 §6); explicit modes always survive.
        let choice = if !capabilities.supports_tools {
            None
        } else {
            match request.tool_choice {
                // A key the client never sent stays absent (spec 20 §6), even
                // when tools are declared.
                ToolChoice::Unspecified => None,
                ToolChoice::Auto if tools.is_none() => None,
                ref other => Some(tool_choice_json(other)),
            }
        };
        let mut client_sent_input = false;
        if native {
            // Native shape: the client's own top-level keys in client order;
            // `input` is normalized and `tool_choice` mapped in place
            // (spec 20 §2.2). Only the keys the provider writes itself are
            // skipped here.
            for (key, value) in &request.passthrough {
                if NATIVE_DROPPED.contains(&key.as_str()) {
                    continue;
                }
                let rendered = match key.as_str() {
                    "input" => {
                        client_sent_input = true;
                        format!("[{}]", input.join(","))
                    }
                    // A key the model cannot honour is dropped, not echoed
                    // (spec 20 §6, §16A.2).
                    "tool_choice" => match &choice {
                        Some(choice) => choice.clone(),
                        None => continue,
                    },
                    _ => json_value(value),
                };
                out.push(',');
                out.push_str(&json_str(key));
                out.push(':');
                out.push_str(&rendered);
            }
        }
        // Native shape omits `input` when the client sent none (§2.2 lists it
        // as optional) but keeps an empty list it did send; the synth shape
        // always writes it (§2.1).
        if !native || (!client_sent_input && !input.is_empty()) {
            out.push_str(",\"input\":[");
            out.push_str(&input.join(","));
            out.push(']');
        }
        if native {
            if let Some(tools) = &tools {
                out.push_str(",\"tools\":");
                out.push_str(tools);
            }
        } else {
            if let Some(tools) = &tools {
                out.push_str(",\"tools\":");
                out.push_str(tools);
            }
            if let Some(choice) = &choice {
                out.push_str(",\"tool_choice\":");
                out.push_str(choice);
            }
        }
        // Native mode carries the client's `parallel_tool_calls` inside the
        // passthrough block (spec 20 §7); the synthetic shape writes it here.
        if !native && request.parallel_tool_calls == Some(false) && tools.is_some() {
            out.push_str(",\"parallel_tool_calls\":false");
        } else if !native
            && request.parallel_tool_calls == Some(true)
            && !capabilities.supports_parallel_tool_calls
        {
            eprintln!(
                "[zed-proxy] open_ai: model {model} does not support parallel tool calls; omitting the key"
            );
        }
        let reasoning = reasoning_json(
            effort.as_deref(),
            request.reasoning.extra.as_ref(),
            capabilities.supports_thinking,
        );
        if let Some(reasoning) = reasoning {
            out.push_str(",\"reasoning\":");
            out.push_str(&reasoning);
        }
        if let Some(budget) = budget {
            out.push_str(",\"max_output_tokens\":");
            out.push_str(&budget.to_string());
        }
        out.push('}');
        Ok(out.into_bytes())
    }

    fn decode_response(&self, body: &[u8]) -> Result<NeutralResponse, GatewayError> {
        decode_response(body)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(ResponsesDecoder::default())
    }
}

/// Effective effort for this provider (spec 20 §8.2): GPT-5.6 defaults to
/// `xhigh` and GPT-5.5 is forced to `xhigh`; everything else passes the client
/// value through and emits no `reasoning` object when absent.
pub fn effective_effort(model: &str, client_effort: Option<&str>) -> Option<String> {
    if model.starts_with("gpt-5.6") {
        Some(
            client_effort
                .map(str::to_string)
                .unwrap_or_else(|| "xhigh".to_string()),
        )
    } else if model.starts_with("gpt-5.5") {
        Some("xhigh".to_string())
    } else {
        client_effort.map(str::to_string)
    }
}

/// `max`/`minimal` are rejected for the GPT-5.6 family, never rewritten
/// (spec 20 §8.3). The caller has already trimmed and lowercased the value.
pub fn validate_effort(model: &str, client_effort: Option<&str>) -> Result<(), GatewayError> {
    if !model.starts_with("gpt-5.6") {
        return Ok(());
    }
    if let Some(effort) = client_effort {
        if !GPT_56_EFFORTS.contains(&effort) {
            return Err(GatewayError::request(format!(
                "unsupported reasoning effort {effort:?}; available: none, low, medium, high, xhigh"
            )));
        }
    }
    Ok(())
}

/// Output budget for this provider (spec 20 §9 / §15.3).
///
/// `clamp_to_catalog` is the D1/D20-2 switch: `true` applies
/// `min(budget, catalog max_output_tokens)`.
pub fn output_budget(
    model: &str,
    effort: Option<&str>,
    client_budget: Option<i64>,
    catalog_max: Option<i64>,
    no_effort_levels: bool,
    clamp_to_catalog: bool,
) -> Option<i64> {
    // Large-reasoning floor only applies to the GPT-5.5/5.6 families.
    let large_reasoning = matches!(effort, Some("xhigh") | Some("max"))
        && (model.starts_with("gpt-5.6") || model.starts_with("gpt-5.5"));
    let raw = match (large_reasoning, client_budget) {
        (true, Some(value)) => Some(value.max(32_768)),
        (true, None) => Some(32_768),
        (false, Some(value)) => Some(value.max(32)),
        (false, None) => None,
    };
    // Models with no effort levels burn the budget on invisible reasoning
    // (spec 20 §15.3 / D20-4). The floor is also the budget when the client
    // sends none.
    let raw = if no_effort_levels {
        let floor = match catalog_max {
            Some(max) => 1_024.min(max),
            None => 1_024,
        };
        Some(raw.map_or(floor, |value| value.max(floor)))
    } else {
        raw
    };
    match (clamp_to_catalog, catalog_max, raw) {
        (true, Some(max), Some(value)) => Some(value.min(max)),
        (_, _, value) => value,
    }
}

fn decode_response(body: &[u8]) -> Result<NeutralResponse, GatewayError> {
    let text = String::from_utf8_lossy(body);
    let mut decoder = ResponsesDecoder::default();
    for line in text.split('\n') {
        decoder.push_line(line)?;
    }
    decoder.finish()?;
    if !decoder.saw_model_event() {
        // "No recognisable event to reduce" is a transient failure, not an
        // empty successful reply (spec 30 §3.3).
        return Err(GatewayError::transient(
            None,
            "upstream response contained no recognised event",
        ));
    }
    decoder.into_response()
}

/// One compact JSON string literal, quote/backslash escaped (spec 20 §2.4).
fn json_str(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn json_value(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// System text: segments joined with `"\n\n"`, empty segments skipped
/// (spec 20 §3).
fn merged_system(system: &[SystemBlock]) -> Option<String> {
    let text = system
        .iter()
        .map(|block| block.text.as_str())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

/// Copy of a client-returned object with every `null` member removed, because
/// the upstream parser rejects members like `content:null` (spec 20 §4.4).
fn strip_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, item)| !item.is_null())
                .map(|(key, item)| (key.clone(), strip_nulls(item)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(strip_nulls).collect()),
        other => other.clone(),
    }
}

fn message_item(role: &str, parts: Vec<String>) -> String {
    format!(
        "{{\"type\":\"message\",\"role\":{},\"content\":[{}]}}",
        json_str(role),
        parts.join(",")
    )
}

fn image_part(source: &ImageSource, supports_images: bool) -> Option<String> {
    if !supports_images {
        return None;
    }
    let url = match source {
        ImageSource::Data { media_type, data } => Some(format!("data:{media_type};base64,{data}")),
        ImageSource::Url(url) if url.starts_with("http://") || url.starts_with("https://") => {
            Some(url.clone())
        }
        ImageSource::Url(_) => None,
    };
    url.map(|url| {
        format!(
            "{{\"type\":\"input_image\",\"image_url\":{}}}",
            json_str(&url)
        )
    })
}

fn function_call_item(tool: &ToolUse) -> Option<String> {
    if tool.id.is_empty() || tool.name.is_empty() {
        eprintln!(
            "[zed-proxy] open_ai: dropping tool call without id or name (name {:?}, id {:?})",
            tool.name, tool.id
        );
        return None;
    }
    let arguments = if tool.arguments.trim().is_empty() {
        DEFAULT_ARGUMENTS
    } else {
        tool.arguments.as_str()
    };
    Some(format!(
        "{{\"type\":\"function_call\",\"call_id\":{},\"name\":{},\"arguments\":{}}}",
        json_str(&tool.id),
        json_str(&tool.name),
        json_str(arguments)
    ))
}

fn function_call_output_item(result: &ToolResult) -> Option<String> {
    if result.tool_use_id.is_empty() {
        eprintln!("[zed-proxy] open_ai: dropping tool result without call_id");
        return None;
    }
    Some(format!(
        "{{\"type\":\"function_call_output\",\"call_id\":{},\"output\":{}}}",
        json_str(&result.tool_use_id),
        json_str(&result.output)
    ))
}

/// Build the `input` items in spec 20 §4.5 order.
fn input_items(request: &NeutralRequest, native: bool, supports_images: bool) -> Vec<String> {
    let mut items = Vec::new();
    let instructions_in_passthrough = native
        && request
            .passthrough
            .iter()
            .any(|(key, _)| key == "instructions");
    if !instructions_in_passthrough {
        if let Some(text) = merged_system(&request.system) {
            items.push(message_item(
                "system",
                vec![format!(
                    "{{\"type\":\"input_text\",\"text\":{}}}",
                    json_str(&text)
                )],
            ));
        }
    }
    for message in &request.messages {
        let mut parts = Vec::new();
        let mut text = String::new();
        for block in &message.parts {
            if let ContentBlock::Text(segment) = block {
                if !segment.is_empty() {
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(segment);
                }
            }
        }
        let part_type = match message.role {
            Role::Assistant => "output_text",
            Role::User => "input_text",
        };
        if !text.is_empty() {
            parts.push(format!(
                "{{\"type\":\"{part_type}\",\"text\":{}}}",
                json_str(&text)
            ));
        }
        for block in &message.parts {
            match block {
                ContentBlock::Image(source) => match image_part(source, supports_images) {
                    Some(part) => parts.push(part),
                    None if supports_images => {
                        eprintln!("[zed-proxy] open_ai: dropping unsupported image URL scheme")
                    }
                    None => eprintln!(
                        "[zed-proxy] open_ai: images_dropped (model has no image support)"
                    ),
                },
                ContentBlock::Thinking(_) | ContentBlock::Compaction(_) => {}
                _ => {}
            }
        }
        if !parts.is_empty() {
            items.push(message_item(message.role.as_str(), parts));
        }
        for block in &message.parts {
            match block {
                ContentBlock::ToolUse(tool) => items.extend(function_call_item(tool)),
                ContentBlock::ToolResult(result) => items.extend(function_call_output_item(result)),
                ContentBlock::Opaque(opaque) => {
                    let ours = opaque.provider == "open_ai" || opaque.provider == "openai";
                    if native && ours {
                        items.push(json_value(&strip_nulls(&opaque.item)));
                    } else {
                        eprintln!(
                            "[zed-proxy] open_ai: dropping opaque item from provider {:?}",
                            opaque.provider
                        );
                    }
                }
                _ => {}
            }
        }
    }
    items
}

/// Flat `function` declarations (spec 20 §5): skip empty names, keep the first
/// duplicate, always write `description`, default the parameters object, and
/// narrow to the named choice in `tools` order.
fn tool_items(tools: &[ToolDef], native: bool, named: Option<&str>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for tool in tools {
        if tool.name.is_empty() || !seen.insert(tool.name.clone()) {
            if tool.name.is_empty() {
                eprintln!("[zed-proxy] open_ai: dropping tool declaration without a name");
            }
            continue;
        }
        if let Some(named) = named {
            if tool.name != named {
                continue;
            }
        }
        if let Some(format) = &tool.format {
            if native {
                items.push(format!(
                    "{{\"type\":\"custom\",\"name\":{},\"description\":{},\"format\":{}}}",
                    json_str(&tool.name),
                    json_str(&tool.description),
                    json_value(format)
                ));
            } else {
                items.push(function_tool(tool));
            }
            continue;
        }
        items.push(function_tool(tool));
    }
    items
}

fn function_tool(tool: &ToolDef) -> String {
    let parameters = if tool.parameters.is_object() {
        json_value(&tool.parameters)
    } else {
        DEFAULT_TOOL_PARAMETERS.to_string()
    };
    let mut item = format!(
        "{{\"type\":\"function\",\"name\":{},\"description\":{},\"parameters\":{parameters}",
        json_str(&tool.name),
        json_str(&tool.description)
    );
    if let Some(strict) = tool.strict {
        item.push_str(&format!(",\"strict\":{strict}"));
    }
    item.push('}');
    item
}

/// `tool_choice` (spec 20 §6). A named tool is expressed as `required` plus a
/// one-tool list (the filtering happens in [`tool_items`]); if the name is
/// unknown `tools` is omitted entirely and only `"required"` remains.
fn tool_choice_json(choice: &ToolChoice) -> String {
    match choice {
        ToolChoice::Unspecified | ToolChoice::Auto => "\"auto\"".to_string(),
        ToolChoice::None => "\"none\"".to_string(),
        ToolChoice::Required | ToolChoice::Named(_) => "\"required\"".to_string(),
    }
}

/// `reasoning` object (spec 20 §8.1): `effort` first, then the client's other
/// keys. `summary:"detailed"` is filled whenever the client chose no summary
/// policy and the effort is not `none` — the Zig baseline does the same for an
/// explicit `reasoning_effort` — and only when the model can produce
/// summaries (spec 25 §4.1, D25-3).
fn reasoning_json(
    effort: Option<&str>,
    extra: Option<&Value>,
    supports_thinking: bool,
) -> Option<String> {
    let effort = effort?;
    let mut out = format!("{{\"effort\":{}", json_str(effort));
    let mut has_summary = false;
    if let Some(Value::Object(map)) = extra {
        for (key, value) in map {
            if key == "effort" {
                continue;
            }
            if key == "summary" {
                has_summary = true;
            }
            out.push(',');
            out.push_str(&json_str(key));
            out.push(':');
            out.push_str(&json_value(value));
        }
    }
    if !has_summary && effort != "none" && supports_thinking {
        out.push_str(",\"summary\":\"detailed\"");
    }
    out.push('}');
    Some(out)
}

/// Streaming half: Responses events → neutral events (spec 20 §13), with the
/// same state machine the non-streaming path reduces through (spec 20 §12).
#[derive(Default)]
pub struct ResponsesDecoder {
    /// Output text concatenated from `response.output_text.delta`.
    text: String,
    /// Reasoning text concatenated from the two reasoning delta events.
    thinking: String,
    /// Fallback text from the completion envelope, used only when no delta.
    envelope_text: Option<String>,
    /// Fallback reasoning from the envelope's reasoning items.
    envelope_thinking: Option<String>,
    /// Tool calls discovered through `output_item.added`.
    tools: Vec<ToolUse>,
    /// Index of the open tool call, if any.
    open_tool: Option<usize>,
    usage: Usage,
    response_id: Option<String>,
    stop_reason: StopReason,
    /// Terminal events already emitted (completed / incomplete / failed).
    ended: bool,
    saw_event: bool,
    failure: Option<GatewayError>,
}

impl ResponsesDecoder {
    fn into_response(self) -> Result<NeutralResponse, GatewayError> {
        if let Some(err) = self.failure {
            return Err(err);
        }
        let text = if self.text.is_empty() {
            self.envelope_text.clone().unwrap_or_default()
        } else {
            self.text
        };
        let thinking = if self.thinking.is_empty() {
            self.envelope_thinking.clone().unwrap_or_default()
        } else {
            self.thinking
        };
        let stop_reason = if self.tools.is_empty() {
            match self.stop_reason {
                StopReason::ToolCalls => StopReason::Stop,
                other => other,
            }
        } else {
            StopReason::ToolCalls
        };
        Ok(NeutralResponse {
            id: self.response_id,
            text,
            thinking,
            tool_calls: self.tools,
            blocks: Vec::new(),
            stop_reason,
            usage: self.usage.with_computed_total(),
        })
    }

    fn handle_event(&mut self, event: &Value) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            // The client writes `response.created` before `response.completed`
            // arrives, so the upstream id has to be carried by `Start`
            // (spec 42 §2.1); `in_progress` is the fallback when an upstream
            // skips `created`.
            "response.created" | "response.in_progress" => {
                let id = event
                    .get("response")
                    .and_then(|response| response.get("id"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty());
                if let Some(id) = id {
                    if self.response_id.is_none() {
                        self.response_id = Some(id.to_string());
                        events.push(StreamEvent::Start {
                            id: Some(id.to_string()),
                        });
                    }
                }
            }
            "ping"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.done" => {}
            "response.output_item.added" => {
                self.saw_event = true;
                let item = event.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    // At most one tool block is open at a time (spec 45 §1.2);
                    // a well-formed stream closes it with `done` first.
                    events.extend(self.close_tool());
                    let id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .or_else(|| item.get("id").and_then(Value::as_str))
                        .unwrap_or_default()
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let index = self.tools.len() as u64;
                    self.tools.push(ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                        signature: None,
                    });
                    self.open_tool = Some(index as usize);
                    events.push(StreamEvent::ToolStart { index, id, name });
                }
            }
            "response.output_item.done" => {
                events.extend(self.close_tool());
            }
            "response.function_call_arguments.delta" => {
                let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
                if !delta.is_empty() {
                    if let Some(index) = self.open_tool {
                        self.tools[index].arguments.push_str(delta);
                        events.push(StreamEvent::ToolArgsDelta {
                            index: index as u64,
                            delta: delta.to_string(),
                        });
                    }
                }
            }
            "response.function_call_arguments.done" => {
                events.extend(self.close_tool());
            }
            "response.output_text.delta" => {
                let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
                if !delta.is_empty() {
                    self.saw_event = true;
                    self.text.push_str(delta);
                    events.push(StreamEvent::TextDelta {
                        delta: delta.to_string(),
                    });
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
                if !delta.is_empty() {
                    self.saw_event = true;
                    self.thinking.push_str(delta);
                    events.push(StreamEvent::ThinkingDelta {
                        delta: delta.to_string(),
                    });
                }
            }
            "response.completed" | "response.incomplete" => {
                self.saw_event = true;
                let response = event.get("response").cloned().unwrap_or(Value::Null);
                self.absorb_envelope(&response, event_type == "response.incomplete");
                events.push(StreamEvent::Usage(self.usage.with_computed_total()));
                events.push(StreamEvent::End {
                    reason: self.stop_reason,
                });
                self.ended = true;
            }
            "response.failed" => {
                self.saw_event = true;
                let err = failure_from_envelope(event);
                self.failure = Some(err.clone());
                events.push(StreamEvent::Error(err));
                self.ended = true;
            }
            "" => {
                if let Some(err) = parse_status_failed(event) {
                    self.failure = Some(err.clone());
                    events.push(StreamEvent::Error(err));
                    self.ended = true;
                }
            }
            _ => {
                self.saw_event = true;
            }
        }
        events
    }

    fn close_tool(&mut self) -> Option<StreamEvent> {
        let index = self.open_tool.take()?;
        if self.tools[index].arguments.is_empty() {
            self.tools[index].arguments = DEFAULT_ARGUMENTS.to_string();
        }
        Some(StreamEvent::ToolEnd {
            index: index as u64,
        })
    }

    /// Fold `response.completed.response` into the neutral result (spec 20 §12).
    fn absorb_envelope(&mut self, response: &Value, incomplete: bool) {
        if let Some(id) = response.get("id").and_then(Value::as_str) {
            self.response_id = Some(id.to_string());
        }
        let output = response
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut text = String::new();
        let mut thinking_parts: Vec<String> = Vec::new();
        let mut envelope_tools = Vec::new();
        for item in &output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    for part in item
                        .get("content")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                    {
                        if part.get("type").and_then(Value::as_str) == Some("output_text") {
                            if let Some(fragment) = part.get("text").and_then(Value::as_str) {
                                text.push_str(fragment);
                            }
                        }
                    }
                }
                Some("reasoning") => {
                    for summary in item
                        .get("summary")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                    {
                        if let Some(fragment) = summary.get("text").and_then(Value::as_str) {
                            thinking_parts.push(fragment.to_string());
                        }
                    }
                }
                Some("function_call") => {
                    let id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .or_else(|| item.get("id").and_then(Value::as_str))
                        .unwrap_or_default()
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let arguments = match item.get("arguments") {
                        Some(Value::String(text)) if !text.is_empty() => text.clone(),
                        Some(Value::String(_)) | None => DEFAULT_ARGUMENTS.to_string(),
                        Some(other) => json_value(other),
                    };
                    envelope_tools.push(ToolUse {
                        id,
                        name,
                        arguments,
                        signature: None,
                    });
                }
                _ => {}
            }
        }
        if !text.is_empty() {
            self.envelope_text = Some(text);
        }
        if !thinking_parts.is_empty() {
            self.envelope_thinking = Some(thinking_parts.join("\n\n"));
        }
        if self.tools.is_empty() && !envelope_tools.is_empty() {
            self.tools = envelope_tools;
        }
        self.usage = envelope_usage(response.get("usage"));
        let reason = if incomplete {
            match response
                .get("incomplete_details")
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
            {
                Some("content_filter") => StopReason::ContentFilter,
                _ => StopReason::Length,
            }
        } else if self.tools.is_empty() {
            StopReason::Stop
        } else {
            StopReason::ToolCalls
        };
        self.stop_reason = reason;
    }
}

fn envelope_usage(usage: Option<&Value>) -> Usage {
    let number = |value: Option<&Value>| value.and_then(Value::as_i64).unwrap_or(0);
    let Some(usage) = usage else {
        return Usage::default();
    };
    Usage {
        input_tokens: number(usage.get("input_tokens")),
        output_tokens: number(usage.get("output_tokens")),
        reasoning_tokens: number(
            usage
                .get("output_tokens_details")
                .and_then(|details| details.get("reasoning_tokens")),
        ),
        cached_input_tokens: number(
            usage
                .get("input_tokens_details")
                .and_then(|details| details.get("cached_tokens")),
        ),
        total_tokens: number(usage.get("total_tokens")),
    }
}

/// `response.failed` → request-level error when the code names a 4xx status,
/// otherwise the transient default (spec 30 §2).
fn failure_from_envelope(event: &Value) -> GatewayError {
    let response = event.get("response").unwrap_or(event);
    let error = response.get("error").unwrap_or(&Value::Null);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| {
            response
                .get("status_details")
                .and_then(|details| details.get("error"))
                .and_then(|inner| inner.get("message"))
                .and_then(Value::as_str)
        })
        .unwrap_or("upstream response failed")
        .to_string();
    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    let status = code
        .strip_prefix("upstream_http_")
        .and_then(|suffix| suffix.parse::<u16>().ok());
    let mut err = match status {
        Some(status) => GatewayError::new(crate::error::classify_status(status, false), message),
        None => GatewayError::transient(None, message),
    };
    err.status = status.or(err.status);
    err
}

impl StreamDecoder for ResponsesDecoder {
    fn push_line(&mut self, line: &str) -> Result<Vec<StreamEvent>, GatewayError> {
        let cleaned = clean_line(line);
        if cleaned.is_empty() || !cleaned.starts_with('{') {
            return Ok(Vec::new());
        }
        let Ok(value) = serde_json::from_str::<Value>(cleaned) else {
            return Ok(Vec::new());
        };
        if is_stream_ended(&value) || self.ended {
            return Ok(Vec::new());
        }
        // Events arrive wrapped as `{"event":{…}}` or bare (spec 20 §12).
        let event = match value.get("event").filter(|event| event.is_object()) {
            Some(event) => event,
            None => &value,
        };
        Ok(self.handle_event(event))
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, GatewayError> {
        if self.ended {
            return Ok(Vec::new());
        }
        self.ended = true;
        if let Some(err) = &self.failure {
            return Ok(vec![StreamEvent::Error(err.clone())]);
        }
        // A stream that ends without a completion event still closes with the
        // terminal pair (spec 20 §12.4).
        let reason = if self.tools.is_empty() {
            StopReason::Stop
        } else {
            StopReason::ToolCalls
        };
        Ok(vec![
            StreamEvent::Usage(self.usage.with_computed_total()),
            StreamEvent::End { reason },
        ])
    }

    fn saw_model_event(&self) -> bool {
        self.saw_event
    }

    fn failure(&self) -> Option<GatewayError> {
        self.failure.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::EffortLevel;
    use crate::neutral::Message;

    fn level(value: &str) -> EffortLevel {
        EffortLevel {
            name: value.to_string(),
            value: value.to_string(),
            is_default: false,
            supports_disabling_thinking: false,
        }
    }

    /// Capabilities with effort levels, so the "no effort levels" 1024 floor
    /// does not fire (the registry never lists gpt-6/gpt-5.6 as level-less).
    fn caps_std() -> ModelCapabilities {
        caps(&["low", "medium", "high", "xhigh"])
    }

    fn caps(levels: &[&str]) -> ModelCapabilities {
        ModelCapabilities {
            provider: ProviderKind::OpenAi,
            max_output_tokens: None,
            supports_tools: true,
            supports_images: true,
            supports_thinking: true,
            supports_disabling_thinking: false,
            supports_parallel_tool_calls: true,
            supports_server_side_compaction: false,
            supported_effort_levels: levels.iter().map(|level| self::level(level)).collect(),
        }
    }

    fn request(model: &str) -> NeutralRequest {
        NeutralRequest {
            model: model.to_string(),
            ..NeutralRequest::default()
        }
    }

    fn encode(request: &NeutralRequest, capabilities: &ModelCapabilities) -> String {
        let bytes = OpenAiProvider
            .encode_request(request, capabilities)
            .unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn gpt_56_defaults_to_xhigh_and_55_is_forced() {
        assert_eq!(
            effective_effort("gpt-5.6-luna", None).as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            effective_effort("gpt-5.6-luna", Some("low")).as_deref(),
            Some("low")
        );
        assert_eq!(
            effective_effort("gpt-5.5", Some("low")).as_deref(),
            Some("xhigh")
        );
        assert_eq!(effective_effort("gpt-6-sol", None), None);
    }

    #[test]
    fn budget_floors_and_large_reasoning_ladder() {
        assert_eq!(
            output_budget("gpt-6-sol", None, Some(4), None, false, false),
            Some(32)
        );
        assert_eq!(
            output_budget("gpt-6-sol", None, None, None, false, false),
            None
        );
        assert_eq!(
            output_budget("gpt-5.6-sol", Some("xhigh"), None, None, false, false),
            Some(32_768)
        );
        assert_eq!(
            output_budget("gpt-5.6-sol", Some("low"), Some(2048), None, false, false),
            Some(2048)
        );
    }

    #[test]
    fn no_effort_models_get_the_1024_floor() {
        assert_eq!(
            output_budget("gpt-5-nano", None, Some(32), None, true, false),
            Some(1_024)
        );
        assert_eq!(
            output_budget("gpt-5-nano", None, Some(32), Some(512), true, false),
            Some(512)
        );
        assert_eq!(
            output_budget("gpt-5-nano", None, None, None, true, false),
            Some(1_024)
        );
    }

    #[test]
    fn catalog_clamp_applies_to_both_directions() {
        assert_eq!(
            output_budget("gpt-6-sol", None, Some(64), Some(10_000), false, true),
            Some(64)
        );
        assert_eq!(
            output_budget("gpt-6-sol", None, Some(200_000), Some(128_000), false, true),
            Some(128_000)
        );
    }

    #[test]
    fn gpt_56_rejects_max_and_minimal() {
        for effort in ["max", "minimal", "bogus"] {
            let mut req = request("gpt-5.6-sol");
            req.reasoning.effort = Some(effort.to_string());
            let err = OpenAiProvider
                .encode_request(&req, &caps(&["low", "medium", "high", "xhigh"]))
                .unwrap_err();
            assert_eq!(err.status, None);
            assert!(err.message.contains("none, low, medium, high, xhigh"));
        }
        for effort in ["none", "low", "medium", "high", "xhigh"] {
            let mut req = request("gpt-5.6-sol");
            req.reasoning.effort = Some(effort.to_string());
            let body = encode(&req, &caps(&["low", "medium", "high", "xhigh"]));
            assert!(body.contains(&format!("\"effort\":\"{effort}\"")));
        }
        // Other models never validate (D20-8).
        let mut req = request("gpt-5-nano");
        req.reasoning.effort = Some("max".to_string());
        assert!(OpenAiProvider.encode_request(&req, &caps(&[])).is_ok());
    }

    #[test]
    fn anthropic_route_does_not_validate_and_forces_xhigh() {
        let mut req = request("gpt-5.6-sol");
        req.reasoning.effort = None;
        let body = encode(&req, &caps(&["low", "high"]));
        assert!(body.contains("\"reasoning\":{\"effort\":\"xhigh\",\"summary\":\"detailed\"}"));
        assert!(body.contains("\"max_output_tokens\":32768"));
    }

    #[test]
    fn reasoning_extra_keys_keep_their_order_and_summary_is_filled() {
        let mut req = request("gpt-5.6-sol");
        req.reasoning.effort = Some("medium".to_string());
        req.reasoning.extra =
            Some(serde_json::json!({"context":"auto","summary":"concise","effort":"low"}));
        let body = encode(&req, &caps(&["medium"]));
        assert!(body.contains(
            "\"reasoning\":{\"effort\":\"medium\",\"context\":\"auto\",\"summary\":\"concise\"}"
        ));
        let mut req = request("gpt-5.6-sol");
        req.reasoning.effort = Some("none".to_string());
        let body = encode(&req, &caps(&["none"]));
        assert!(body.contains("\"reasoning\":{\"effort\":\"none\"}"));
    }

    #[test]
    fn spec_16a1_tool_history_matches_byte_for_byte() {
        let mut req = request("gpt-5.6-sol");
        req.system = vec![SystemBlock {
            text: "Be brief".to_string(),
            cache_control: None,
        }];
        req.messages = vec![
            Message {
                role: Role::User,
                parts: vec![ContentBlock::Text("hi".to_string())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![ContentBlock::ToolUse(ToolUse {
                    id: "call_1".to_string(),
                    name: "lookup".to_string(),
                    arguments: "{\"key\":\"v\"}".to_string(),
                    signature: None,
                })],
            },
            Message {
                role: Role::User,
                parts: vec![ContentBlock::ToolResult(ToolResult {
                    tool_use_id: "call_1".to_string(),
                    output: "done".to_string(),
                    is_error: false,
                })],
            },
        ];
        req.tools = vec![ToolDef {
            name: "lookup".to_string(),
            description: "Look up a key".to_string(),
            parameters: serde_json::json!({"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}),
            strict: None,
            cache_control: None,
            format: None,
        }];
        req.tool_choice = ToolChoice::Auto;
        req.reasoning.effort = Some("low".to_string());
        req.max_output_tokens = Some(2048);
        // A client that names an effort but no summary policy still gets
        // `"summary":"detailed"` (spec 20 §8.1, Zig baseline behavior), so the
        // exact spec sample is not what this request produces.
        let expected = r#"{"model":"gpt-5.6-sol","stream":true,"input":[{"type":"message","role":"system","content":[{"type":"input_text","text":"Be brief"}]},{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"key\":\"v\"}"},{"type":"function_call_output","call_id":"call_1","output":"done"}],"tools":[{"type":"function","name":"lookup","description":"Look up a key","parameters":{"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}}],"tool_choice":"auto","reasoning":{"effort":"low","summary":"detailed"},"max_output_tokens":2048}"#;
        let body = encode(&req, &caps(&["low"]));
        assert_eq!(body, expected);
    }

    #[test]
    fn spec_16a2_native_passthrough_matches_byte_for_byte() {
        let mut req = request("gpt-5.6-sol");
        req.passthrough = vec![
            ("store".to_string(), Value::Bool(false)),
            ("input".to_string(), Value::String("ping".to_string())),
        ];
        // The Responses decoder lowers `input` into messages; the passthrough
        // copy is dropped and the normalised items are written in its place.
        req.messages = vec![Message {
            role: Role::User,
            parts: vec![ContentBlock::Text("ping".to_string())],
        }];
        let expected = r#"{"model":"gpt-5.6-sol","stream":true,"store":false,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"ping"}]}],"reasoning":{"effort":"xhigh","summary":"detailed"},"max_output_tokens":32768}"#;
        assert_eq!(encode(&req, &caps(&["low"])), expected);
        // A client that sent no `input` at all gets none written back (the
        // normalised messages are absent too, as the Responses decoder only
        // produces them from `input`).
        req.passthrough.retain(|(key, _)| key != "input");
        req.messages.clear();
        let body = encode(&req, &caps(&["low"]));
        assert!(!body.contains("\"input\""));
        assert!(body.contains("\"store\":false"));
    }

    #[test]
    fn developer_role_and_empty_messages_follow_the_spec() {
        let mut req = request("gpt-6-sol");
        req.messages = vec![
            Message {
                role: Role::User,
                parts: vec![],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    ContentBlock::Text("a".to_string()),
                    ContentBlock::Text("b".to_string()),
                ],
            },
        ];
        let body = encode(&req, &caps_std());
        assert_eq!(
            body,
            r#"{"model":"gpt-6-sol","stream":true,"input":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"a\n\nb"}]}]}"#
        );
    }

    #[test]
    fn tool_calls_and_results_without_ids_are_dropped() {
        let mut req = request("gpt-6-sol");
        req.messages = vec![Message {
            role: Role::Assistant,
            parts: vec![
                ContentBlock::ToolUse(ToolUse {
                    id: String::new(),
                    name: "lookup".to_string(),
                    arguments: "{}".to_string(),
                    signature: None,
                }),
                ContentBlock::ToolUse(ToolUse {
                    id: "call_2".to_string(),
                    name: "lookup".to_string(),
                    arguments: String::new(),
                    signature: None,
                }),
            ],
        }];
        req.messages.push(Message {
            role: Role::User,
            parts: vec![ContentBlock::ToolResult(ToolResult {
                tool_use_id: String::new(),
                output: "x".to_string(),
                is_error: false,
            })],
        });
        let body = encode(&req, &caps_std());
        assert!(body.contains(
            r#"{"type":"function_call","call_id":"call_2","name":"lookup","arguments":"{}"}"#
        ));
        assert!(!body.contains("function_call_output"));
        assert_eq!(body.matches("function_call").count(), 1);
    }

    #[test]
    fn named_tool_choice_narrows_tools_and_empty_list_omits_tools() {
        let mut req = request("gpt-6-sol");
        req.tools = vec![
            ToolDef {
                name: "a".to_string(),
                description: String::new(),
                parameters: Value::Null,
                strict: None,
                cache_control: None,
                format: None,
            },
            ToolDef {
                name: "b".to_string(),
                description: "B".to_string(),
                parameters: serde_json::json!({"type":"object"}),
                strict: Some(true),
                cache_control: None,
                format: None,
            },
            ToolDef {
                name: "b".to_string(),
                description: "dup".to_string(),
                parameters: Value::Null,
                strict: None,
                cache_control: None,
                format: None,
            },
        ];
        req.tool_choice = ToolChoice::Named("b".to_string());
        let body = encode(&req, &caps_std());
        assert_eq!(
            body,
            r#"{"model":"gpt-6-sol","stream":true,"input":[],"tools":[{"type":"function","name":"b","description":"B","parameters":{"type":"object"},"strict":true}],"tool_choice":"required"}"#
        );
        req.tool_choice = ToolChoice::Named("missing".to_string());
        let body = encode(&req, &caps_std());
        assert!(!body.contains("\"tools\""));
        assert!(body.ends_with(",\"tool_choice\":\"required\"}"));
    }

    #[test]
    fn parallel_tool_calls_only_emits_false_with_tools() {
        let mut req = request("gpt-6-sol");
        req.tools = vec![ToolDef {
            name: "a".to_string(),
            description: String::new(),
            parameters: Value::Null,
            strict: None,
            cache_control: None,
            format: None,
        }];
        req.tool_choice = ToolChoice::Auto;
        req.parallel_tool_calls = Some(true);
        let body = encode(&req, &caps_std());
        assert!(!body.contains("parallel_tool_calls"));
        req.parallel_tool_calls = Some(false);
        let body = encode(&req, &caps_std());
        assert!(body.contains("],\"tool_choice\":\"auto\",\"parallel_tool_calls\":false"));
        // An absent client key stays absent even with tools declared.
        req.tool_choice = ToolChoice::Unspecified;
        let body = encode(&req, &caps_std());
        assert!(!body.contains("tool_choice"), "{body}");
        req.tools.clear();
        let body = encode(&req, &caps_std());
        assert!(!body.contains("parallel_tool_calls"));
    }

    #[test]
    fn images_are_forwarded_or_dropped_by_capability() {
        let mut req = request("gpt-6-sol");
        req.messages = vec![Message {
            role: Role::User,
            parts: vec![
                ContentBlock::Image(ImageSource::Data {
                    media_type: "image/png".to_string(),
                    data: "AAAA".to_string(),
                }),
                ContentBlock::Image(ImageSource::Url("https://example.com/a.png".to_string())),
                ContentBlock::Image(ImageSource::Url("file:///etc/passwd".to_string())),
            ],
        }];
        let body = encode(&req, &caps_std());
        assert!(body.contains(
            r#"[{"type":"input_image","image_url":"data:image/png;base64,AAAA"},{"type":"input_image","image_url":"https://example.com/a.png"}]"#
        ));
        let mut no_images = caps_std();
        no_images.supports_images = false;
        let body = encode(&req, &no_images);
        assert_eq!(body, r#"{"model":"gpt-6-sol","stream":true,"input":[]}"#);
    }

    #[test]
    fn opaque_items_strip_nulls_and_foreign_providers_are_dropped() {
        let mut req = request("gpt-6-sol");
        req.passthrough = vec![("store".to_string(), Value::Bool(true))];
        req.messages = vec![Message {
            role: Role::Assistant,
            parts: vec![
                ContentBlock::Opaque(crate::neutral::Opaque {
                    provider: "open_ai".to_string(),
                    item: serde_json::json!({"type":"reasoning","content":null,"id":"rs_1"}),
                }),
                ContentBlock::Opaque(crate::neutral::Opaque {
                    provider: "google".to_string(),
                    item: serde_json::json!({"type":"x"}),
                }),
            ],
        }];
        let body = encode(&req, &caps_std());
        // `content:null` is gone; the client's member order is preserved.
        assert!(body.contains(r#"{"type":"reasoning","id":"rs_1"}"#));
        assert!(!body.contains("content"));
        assert!(!body.contains("\"type\":\"x\""));
        assert!(
            !body.contains("tool_choice"),
            "no tools means no tool_choice"
        );
    }

    #[test]
    fn no_effort_levels_lifts_the_budget_to_1024() {
        let mut req = request("gpt-5-nano");
        req.max_output_tokens = Some(64);
        let body = encode(&req, &caps(&[]));
        assert!(body.contains("\"max_output_tokens\":1024"));
        assert!(!body.contains("reasoning"));
        let mut req = request("gpt-5.6-sol");
        req.max_output_tokens = Some(64);
        let body = encode(&req, &caps(&["low"]));
        assert!(body.contains("\"max_output_tokens\":32768"));
    }

    #[test]
    fn spec_16a3_non_streaming_reduction() {
        let body = br#"{"event":{"type":"response.output_item.added","item":{"type":"message","role":"assistant","content":[]}}}
{"event":{"type":"response.output_text.delta","delta":"OK"}}
{"event":{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":9,"output_tokens":2,"total_tokens":11},"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"OK"}]}]}}}
{"status":"stream_ended"}"#;
        let response = OpenAiProvider.decode_response(body).unwrap();
        assert_eq!(response.text, "OK");
        assert_eq!(response.id.as_deref(), Some("resp_1"));
        assert_eq!(response.stop_reason, StopReason::Stop);
        assert_eq!(response.usage.input_tokens, 9);
        assert_eq!(response.usage.output_tokens, 2);
        assert_eq!(response.usage.total_tokens, 11);
    }

    #[test]
    fn envelope_fallback_recovers_text_and_reasoning_without_duplication() {
        let body = br#"{"event":{"type":"response.completed","response":{"id":"r","status":"completed","usage":{"input_tokens":1,"output_tokens":2},"output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"why a"},{"type":"summary_text","text":"why b"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hel"},{"type":"output_text","text":"lo"}]}]}}}"#;
        let response = OpenAiProvider.decode_response(body).unwrap();
        assert_eq!(response.text, "hello");
        assert_eq!(response.thinking, "why a\n\nwhy b");
        assert_eq!(response.usage.total_tokens, 3);
        let body = br#"{"event":{"type":"response.output_text.delta","delta":"hi"}}
{"event":{"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"hi"}]}]}}}"#;
        let response = OpenAiProvider.decode_response(body).unwrap();
        assert_eq!(response.text, "hi");
    }

    #[test]
    fn response_created_carries_the_upstream_id_in_start() {
        let mut decoder = ResponsesDecoder::default();
        let events = decoder
            .push_line(
                r#"{"event":{"type":"response.created","response":{"id":"resp_fixture_1"}}}"#,
            )
            .unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::Start {
                id: Some("resp_fixture_1".to_string())
            }]
        );
        // The later `in_progress` must not repeat the start event.
        assert!(decoder
            .push_line(
                r#"{"event":{"type":"response.in_progress","response":{"id":"resp_fixture_1"}}}"#
            )
            .unwrap()
            .is_empty());

        // An upstream that skips `response.created` still supplies the id.
        let mut decoder = ResponsesDecoder::default();
        let events = decoder
            .push_line(r#"{"event":{"type":"response.in_progress","response":{"id":"resp_2"}}}"#)
            .unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::Start {
                id: Some("resp_2".to_string())
            }]
        );
    }

    #[test]
    fn spec_16a4_streaming_tool_call_events() {
        let lines = [
            r#"{"event":{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_9","name":"lookup","arguments":""}}}"#,
            r#"{"event":{"type":"response.function_call_arguments.delta","delta":"{\"key\":\"v\"}"}}"#,
            r#"{"event":{"type":"response.function_call_arguments.done","arguments":"{\"key\":\"v\"}"}}"#,
            r#"{"event":{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":12,"output_tokens":8,"total_tokens":20},"output":[{"type":"function_call","call_id":"call_9","name":"lookup","arguments":"{\"key\":\"v\"}"}]}}}"#,
            r#"{"status":"stream_ended"}"#,
        ];
        let mut decoder = ResponsesDecoder::default();
        let mut events = Vec::new();
        for line in lines {
            events.extend(decoder.push_line(line).unwrap());
        }
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolStart {
                    index: 0,
                    id: "call_9".to_string(),
                    name: "lookup".to_string()
                },
                StreamEvent::ToolArgsDelta {
                    index: 0,
                    delta: "{\"key\":\"v\"}".to_string()
                },
                StreamEvent::ToolEnd { index: 0 },
                StreamEvent::Usage(Usage {
                    input_tokens: 12,
                    output_tokens: 8,
                    reasoning_tokens: 0,
                    cached_input_tokens: 0,
                    total_tokens: 20
                }),
                StreamEvent::End {
                    reason: StopReason::ToolCalls
                },
            ]
        );
        assert!(decoder.finish().unwrap().is_empty());
        let response = decoder.into_response().unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].arguments, "{\"key\":\"v\"}");
    }

    #[test]
    fn incomplete_maps_to_length_and_content_filter() {
        let body = br#"{"event":{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":3,"output_tokens":5}}}}"#;
        let response = OpenAiProvider.decode_response(body).unwrap();
        assert_eq!(response.stop_reason, StopReason::Length);
        let body = br#"{"event":{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"content_filter"}}}}"#;
        let response = OpenAiProvider.decode_response(body).unwrap();
        assert_eq!(response.stop_reason, StopReason::ContentFilter);
    }

    #[test]
    fn stream_ended_without_completion_closes_with_stop() {
        let mut decoder = ResponsesDecoder::default();
        assert!(decoder.push_line("").unwrap().is_empty());
        assert!(decoder.push_line("ping").unwrap().is_empty());
        assert!(decoder.push_line("data: {not json").unwrap().is_empty());
        decoder
            .push_line(r#"{"event":{"type":"response.output_text.delta","delta":"x"}}"#)
            .unwrap();
        assert!(decoder.saw_model_event());
        assert_eq!(
            decoder.finish().unwrap(),
            vec![
                StreamEvent::Usage(Usage::default()),
                StreamEvent::End {
                    reason: StopReason::Stop
                }
            ]
        );
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn response_failed_reports_a_terminal_error() {
        let body = br#"{"event":{"type":"response.failed","response":{"status":"failed","error":{"code":"upstream_http_400","message":"bad schema"},"status_details":{"error":{"message":"bad schema"}}}}}"#;
        let err = OpenAiProvider.decode_response(body).unwrap_err();
        assert!(!err.kind.is_account_level());
        assert_eq!(err.status, Some(400));
        assert_eq!(err.message, "bad schema");
        let mut decoder = ResponsesDecoder::default();
        let events = decoder
            .push_line(r#"{"status":{"failed":{"code":"upstream_http_429","message":"slow down","retry_after":3}}}"#)
            .unwrap();
        assert!(matches!(events.as_slice(), [StreamEvent::Error(_)]));
        assert!(decoder.failure().unwrap().kind.is_account_level());
    }

    #[test]
    fn empty_bodies_are_a_transient_failure() {
        for body in [&b""[..], b"<html>oops</html>", b"\n\n"] {
            let err = OpenAiProvider.decode_response(body).unwrap_err();
            assert_eq!(err.kind, crate::error::ErrorKind::Transient);
            assert_eq!(err.http_status(), 502);
        }
    }

    #[test]
    fn unsupported_tools_drop_declarations_and_choice() {
        let mut req = request("gpt-6-sol");
        req.tools = vec![ToolDef {
            name: "a".to_string(),
            description: String::new(),
            parameters: Value::Null,
            strict: None,
            cache_control: None,
            format: None,
        }];
        req.tool_choice = ToolChoice::Required;
        let mut no_tools = caps_std();
        no_tools.supports_tools = false;
        let body = encode(&req, &no_tools);
        assert!(!body.contains("\"tools\""));
        assert!(!body.contains("tool_choice"));
    }

    #[test]
    fn tool_choice_modes_map_to_strings() {
        let tool = ToolDef {
            name: "a".to_string(),
            description: String::new(),
            parameters: Value::Null,
            strict: None,
            cache_control: None,
            format: None,
        };
        for (choice, expected) in [
            (ToolChoice::None, "\"tool_choice\":\"none\""),
            (ToolChoice::Required, "\"tool_choice\":\"required\""),
            (ToolChoice::Auto, "\"tool_choice\":\"auto\""),
        ] {
            let mut req = request("gpt-6-sol");
            req.tools = vec![tool.clone()];
            req.tool_choice = choice;
            let body = encode(&req, &caps_std());
            assert!(body.contains(expected), "{body}");
        }
    }

    #[test]
    fn custom_tools_keep_their_format_only_in_native_shape() {
        let custom = ToolDef {
            name: "shell".to_string(),
            description: "Run".to_string(),
            parameters: Value::Null,
            strict: None,
            cache_control: None,
            format: Some(serde_json::json!({"type":"text"})),
        };
        let mut req = request("gpt-5.3-codex");
        req.tools = vec![custom.clone()];
        let body = encode(&req, &caps_std());
        assert!(body.contains(
            r#"{"type":"function","name":"shell","description":"Run","parameters":{"type":"object","properties":{}}}"#
        ));
        req.passthrough = vec![("store".to_string(), Value::Bool(true))];
        let body = encode(&req, &caps_std());
        assert!(body.contains(
            r#"{"type":"custom","name":"shell","description":"Run","format":{"type":"text"}}"#
        ));
    }
}
