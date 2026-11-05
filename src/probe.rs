//! Credential-free probe of one account: a single tiny real completion, no
//! failover, result written back to the account (spec `80-probe-and-webui-contract.md`
//! §1–§3).
//!
//! Implementation task owns this file. The fixed pieces are the request body,
//! the effort/budget selection rules and the success test.

use serde_json::Value;

use crate::accounts::AccountModelState;
use crate::catalog::{Catalog, EffortLevel, Provider};
use crate::error::GatewayError;

pub const DEFAULT_PROBE_MODEL: &str = "gpt-5.6-luna";
pub const PROBE_PROMPT: &str = "Reply only OK";
/// The probe body always advertises 32 and lets the provider raise it
/// (spec 80 §2.2).
pub const PROBE_CLIENT_BUDGET: i64 = 32;

/// Build the Chat-shaped probe body (spec 80 §2). `reasoning_effort` is omitted
/// entirely when `effort` is `None`.
pub fn probe_body(model: &str, effort: Option<&str>) -> Vec<u8> {
    let model = serde_json::to_string(model).unwrap_or_else(|_| "\"\"".to_string());
    let effort = match effort {
        Some(effort) => format!(
            ",\"reasoning_effort\":{}",
            serde_json::to_string(effort).unwrap_or_default()
        ),
        None => String::new(),
    };
    format!(
        "{{\"model\":{model},\"messages\":[{{\"role\":\"user\",\"content\":\"{PROBE_PROMPT}\"}}]{effort},\"max_tokens\":{PROBE_CLIENT_BUDGET},\"max_completion_tokens\":{PROBE_CLIENT_BUDGET},\"stream\":false}}"
    )
    .into_bytes()
}

/// Probe effort and its reported value (spec 80 §2.1): the client protocol
/// layer receives whatever this returns, and the report must match it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeEffort {
    /// Value put into `reasoning_effort`; `None` = omit the key.
    pub send: Option<String>,
    /// Value echoed in `probe.reasoning_effort`; `None` is serialized as `null`.
    pub report: Option<String>,
}

pub fn probe_effort(model: &str, provider: Provider, levels: &[EffortLevel]) -> ProbeEffort {
    if provider == Provider::Anthropic {
        return ProbeEffort {
            send: None,
            report: None,
        };
    }
    if model.starts_with("gpt-5.5") {
        return ProbeEffort {
            send: None,
            report: Some("xhigh".to_string()),
        };
    }
    let lowered: Vec<String> = levels.iter().map(|level| level.value.clone()).collect();
    if lowered.iter().any(|value| value == "none") {
        return ProbeEffort {
            send: Some("none".to_string()),
            report: Some("none".to_string()),
        };
    }
    if lowered.iter().any(|value| value == "low") {
        return ProbeEffort {
            send: Some("low".to_string()),
            report: Some("low".to_string()),
        };
    }
    if lowered.is_empty() {
        return ProbeEffort {
            send: None,
            report: None,
        };
    }
    let candidate = lowered
        .iter()
        .find(|value| *value != "xhigh" && *value != "max")
        .cloned()
        .unwrap_or_else(|| "xhigh".to_string());
    ProbeEffort {
        send: Some(candidate.clone()),
        report: Some(candidate),
    }
}

/// Reported budget must equal the effective one (spec 80 §2.2). The provider
/// encoder returns the effective value; `None` means "no ceiling is sent".
pub fn reported_budget(effective: Option<i64>) -> Option<i64> {
    effective
}

/// Probe success test (spec 80 §2.3): non-empty visible content, or any tool
/// call. Thinking-only responses fail on purpose.
pub fn probe_succeeded(response: &Value) -> bool {
    let Some(choices) = response.get("choices").and_then(Value::as_array) else {
        return false;
    };
    let Some(first) = choices.first() else {
        return false;
    };
    let Some(message) = first.get("message") else {
        return false;
    };
    let has_text = message
        .get("content")
        .and_then(Value::as_str)
        .map(|text| !text.trim().is_empty())
        .unwrap_or(false);
    let has_tools = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| !calls.is_empty())
        .unwrap_or(false);
    has_text || has_tools
}

/// Probe model validity: any registered slug plus the local alias (spec 80 §1).
pub fn is_known_probe_model(catalog: &Catalog, slug: &str) -> bool {
    catalog.get(slug).is_some() || slug == crate::catalog::GPT_ALIAS
}

/// Everything one probe request needs, resolved once per endpoint call
/// (spec 80 §1, §2). The report fields come from this same plan, so what the
/// response claims and what the request carried can never diverge.
#[derive(Debug)]
pub struct ProbePlan {
    /// Slug exactly as the client asked for it (echoed in `probe.model`).
    pub model: String,
    /// Slug after catalog normalization; drives provider, effort and budget.
    pub normalized: String,
    pub provider: Provider,
    pub effort: ProbeEffort,
    /// Effective output budget, or `None` when the provider sends no ceiling.
    pub max_output_tokens: Option<i64>,
}

/// Resolve the probe plan for one requested model (spec 80 §2.1–§2.2).
pub fn probe_plan(catalog: &Catalog, model: &str) -> Result<ProbePlan, GatewayError> {
    let normalized = catalog.normalize_model(Some(model));
    let capabilities = catalog.capabilities(&normalized)?;
    let provider = capabilities.provider;
    let effort = probe_effort(&normalized, provider, &capabilities.supported_effort_levels);
    let max_output_tokens = match provider {
        Provider::OpenAi => crate::provider::openai::output_budget(
            &normalized,
            crate::provider::openai::effective_effort(&normalized, effort.send.as_deref())
                .as_deref(),
            Some(PROBE_CLIENT_BUDGET),
            capabilities.max_output_tokens,
            capabilities.supported_effort_levels.is_empty(),
            true,
        ),
        Provider::Anthropic => Some(crate::provider::anthropic::max_tokens(
            Some(PROBE_CLIENT_BUDGET),
            capabilities.max_output_tokens,
        )),
        // The Google encoder always sends `maxOutputTokens`, so the report
        // must carry the same number (spec 25 §7.2, D22-1).
        Provider::Google => Some(crate::provider::google::max_output_tokens(
            Some(PROBE_CLIENT_BUDGET),
            capabilities.max_output_tokens,
        )),
    };
    Ok(ProbePlan {
        model: model.to_string(),
        normalized,
        provider,
        effort,
        max_output_tokens,
    })
}

/// Run one probe and write the outcome back to the account (spec 80 §2.3–§2.4).
/// Failover is deliberately absent: the named account is the one being judged.
/// The handler resolves the plan first so the report matches what it ran.
pub async fn run_planned_probe(
    services: &crate::proxy::Services,
    account: &str,
    plan: &ProbePlan,
    catalog: &Catalog,
) -> Result<Value, GatewayError> {
    let Some(snapshot) = services.accounts.snapshot(account) else {
        return Err(probe_error("account not found"));
    };
    let capabilities = catalog.capabilities(&plan.normalized)?;
    let provider = plan.provider;
    let body = probe_body(&plan.model, plan.effort.send.as_deref());
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return Err(GatewayError::internal("probe body is not valid JSON"));
    };
    let mut request =
        crate::client::decoder_for(crate::client::ClientProtocol::ChatCompletions).decode(&body)?;
    request.provider = provider;
    let encoder = crate::provider::encoder_for(provider);
    debug_assert_eq!(encoder.kind(), provider);
    let provider_request = encoder.encode_request(&request, &capabilities)?;

    let started = std::time::Instant::now();
    let outcome = probe_exchange(
        services,
        &snapshot,
        &plan.normalized,
        provider,
        &provider_request,
        encoder.as_ref(),
    )
    .await;
    let latency_ms = started.elapsed().as_millis() as u64;

    let now = crate::http::server::now_unix();
    match outcome {
        Ok(value) if probe_succeeded(&value) => {
            services.accounts.record_success(account, now);
            services
                .accounts
                .set_model_probe(account, AccountModelState::Healthy, now, latency_ms);
            Ok(value)
        }
        Ok(_) => {
            // A successful HTTP call with no visible output is the
            // budget-starved failure this probe exists to catch (spec 80 §2.3).
            let err = GatewayError::transient(Some(502), "probe returned no visible output");
            charge_probe_failure(services, account, &err, now, latency_ms);
            Err(err)
        }
        Err(err) => {
            charge_probe_failure(services, account, &err, now, latency_ms);
            Err(err)
        }
    }
}

async fn probe_exchange(
    services: &crate::proxy::Services,
    snapshot: &crate::accounts::AccountSnapshot,
    model: &str,
    provider: Provider,
    provider_request: &[u8],
    encoder: &dyn crate::provider::ProviderEncoder,
) -> Result<Value, GatewayError> {
    let token =
        crate::proxy::account_token(services, snapshot, crate::http::server::now_unix()).await?;
    let envelope = crate::provider::build_envelope(provider, model, provider_request)?;
    let request = crate::proxy::completions_request(&envelope, &token, false);
    let response = services.upstream.execute(request).await?;
    if let Some(failure) =
        crate::upstream::client::classify_attempt(response.status, &response.body, false)
    {
        return Err(failure.error);
    }
    let reduced = encoder.decode_response(&response.body)?;
    let bytes = crate::client::encoder_for(crate::client::ClientProtocol::ChatCompletions)
        .encode_response(&reduced, model, crate::http::server::now_unix())?;
    serde_json::from_slice(&bytes)
        .map_err(|_| GatewayError::internal("cannot re-read the probe response"))
}

fn charge_probe_failure(
    services: &crate::proxy::Services,
    account: &str,
    err: &GatewayError,
    now: i64,
    latency_ms: u64,
) {
    // Probe failures use the same cooldown state machine as requests
    // (spec 80 §2.3, D80-3 option A).
    services
        .accounts
        .record_failure(account, err.kind, err.status.unwrap_or(0), now);
    services.accounts.set_model_probe(
        account,
        AccountModelState::from_gateway_error(err),
        now,
        latency_ms,
    );
}

fn probe_error(message: &str) -> GatewayError {
    let mut err = GatewayError::request(message);
    err.code = Some("unknown_account");
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(value: &str) -> EffortLevel {
        EffortLevel {
            name: value.to_string(),
            value: value.to_string(),
            is_default: false,
            supports_disabling_thinking: false,
        }
    }

    #[test]
    fn probe_body_is_byte_stable() {
        assert_eq!(
            String::from_utf8(probe_body("gpt-5.6-luna", Some("low"))).unwrap(),
            r#"{"model":"gpt-5.6-luna","messages":[{"role":"user","content":"Reply only OK"}],"reasoning_effort":"low","max_tokens":32,"max_completion_tokens":32,"stream":false}"#
        );
        assert_eq!(
            String::from_utf8(probe_body("claude-sonnet-5", None)).unwrap(),
            r#"{"model":"claude-sonnet-5","messages":[{"role":"user","content":"Reply only OK"}],"max_tokens":32,"max_completion_tokens":32,"stream":false}"#
        );
    }

    #[test]
    fn effort_selection_follows_the_ordered_rules() {
        let anthropic = probe_effort("claude-sonnet-5", Provider::Anthropic, &[level("low")]);
        assert_eq!(
            anthropic,
            ProbeEffort {
                send: None,
                report: None
            }
        );

        let gpt55 = probe_effort("gpt-5.5", Provider::OpenAi, &[level("low"), level("xhigh")]);
        assert_eq!(gpt55.send, None);
        assert_eq!(gpt55.report.as_deref(), Some("xhigh"));

        let luna = probe_effort(
            "gpt-5.6-luna",
            Provider::OpenAi,
            &[level("low"), level("medium"), level("high"), level("xhigh")],
        );
        assert_eq!(luna.send.as_deref(), Some("low"));

        let no_levels = probe_effort("gpt-5-nano", Provider::OpenAi, &[]);
        assert_eq!(
            no_levels,
            ProbeEffort {
                send: None,
                report: None
            }
        );

        let only_high = probe_effort(
            "gpt-6.1-sol",
            Provider::OpenAi,
            &[level("max"), level("xhigh")],
        );
        assert_eq!(only_high.send.as_deref(), Some("xhigh"));

        let gemini = probe_effort(
            "gemini-4-argon",
            Provider::Google,
            &[level("low"), level("medium"), level("high")],
        );
        assert_eq!(gemini.send.as_deref(), Some("low"));
    }

    #[test]
    fn success_requires_visible_text_or_tool_calls() {
        assert!(probe_succeeded(&serde_json::json!({
            "choices": [{"message": {"content": "OK"}}]
        })));
        assert!(!probe_succeeded(&serde_json::json!({
            "choices": [{"message": {"content": "   "}}]
        })));
        assert!(!probe_succeeded(&serde_json::json!({
            "choices": [{"message": {"content": "", "reasoning_content": "thinking"}}]
        })));
        assert!(probe_succeeded(&serde_json::json!({
            "choices": [{"message": {"content": "", "tool_calls": [{"id": "c"}]}}]
        })));
        assert!(!probe_succeeded(&serde_json::json!({"choices": []})));
        assert!(!probe_succeeded(&serde_json::json!({})));
    }

    #[test]
    fn probe_models_must_come_from_the_catalog_or_the_alias() {
        let catalog = Catalog::builtin().unwrap();
        for slug in [
            "gpt-5.6-luna",
            "claude-sonnet-5",
            "gemini-4-argon",
            "gpt-5.6",
        ] {
            assert!(is_known_probe_model(&catalog, slug), "{slug}");
        }
        for slug in ["gpt-9", "", "GPT-5.6-luna"] {
            assert!(!is_known_probe_model(&catalog, slug), "{slug}");
        }
    }

    #[test]
    fn probe_plan_reports_the_effective_effort_and_budget() {
        let catalog = Catalog::builtin().unwrap();
        // Catalog-driven effort: gpt-5.6-* lists low/medium/high/xhigh, no
        // `none`, so the default probe now sends `low` (D80-2 option A).
        let luna = probe_plan(&catalog, "gpt-5.6-luna").unwrap();
        assert_eq!(luna.effort.send.as_deref(), Some("low"));
        assert_eq!(luna.effort.report.as_deref(), Some("low"));
        assert_eq!(luna.max_output_tokens, Some(32));

        // GPT-5.5 is forced to xhigh by the provider, so the budget floor is
        // 32768 and the report says so.
        let gpt55 = probe_plan(&catalog, "gpt-5.5").unwrap();
        assert_eq!(gpt55.effort.send, None);
        assert_eq!(gpt55.effort.report.as_deref(), Some("xhigh"));
        assert_eq!(gpt55.max_output_tokens, Some(32_768));

        // Anthropic sends no effort and clamps to 32.
        let claude = probe_plan(&catalog, "claude-sonnet-5").unwrap();
        assert_eq!(
            claude.effort,
            ProbeEffort {
                send: None,
                report: None
            }
        );
        assert_eq!(claude.max_output_tokens, Some(32));

        // Gemini reports the same ceiling the encoder sends (spec 25 §7.2):
        // `min(max(32,32), 65536)` = 32 for gemini-4-argon.
        let gemini = probe_plan(&catalog, "gemini-4-argon").unwrap();
        assert_eq!(gemini.effort.send.as_deref(), Some("low"));
        assert_eq!(gemini.max_output_tokens, Some(32));

        // A level-less OpenAI model still gets the 1024 floor.
        let nano = probe_plan(&catalog, "gpt-5-nano").unwrap();
        assert_eq!(nano.effort.send, None);
        assert_eq!(nano.max_output_tokens, Some(1_024));
    }

    #[test]
    fn probe_plan_normalizes_the_model_but_echoes_the_requested_slug() {
        let catalog = Catalog::builtin().unwrap();
        let alias = probe_plan(&catalog, "gpt-5.6").unwrap();
        assert_eq!(alias.model, "gpt-5.6");
        assert_eq!(alias.normalized, crate::catalog::GPT_ALIAS_SOURCE);
        assert_eq!(alias.provider, Provider::OpenAi);
    }
}
