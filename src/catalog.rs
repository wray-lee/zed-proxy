//! Model catalog: the slug → capability registry built from Zed's `/models`
//! response or from the built-in catalog file (spec `15-catalog-and-models.md`).
//!
//! The built-in file is `src/models.json` (embedded at compile time) and is the
//! cold-start source of truth; a successful upstream refresh replaces it
//! atomically. Everything the rest of the crate needs about a model — provider
//! dispatch, capability gates, output budget ceilings and the `/v1/models`
//! payload — is read from this one registry (spec 15 §2.1).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Mutex, RwLock};

use serde_json::Value;

use crate::error::GatewayError;

pub const BUILTIN_CATALOG: &[u8] = include_bytes!("models.json");
/// `/v1/models` conversion result cache lifetime (spec 15 §4).
pub const MODELS_CACHE_TTL_S: i64 = 3600;
/// Minimum interval between two real `/models` pulls (spec 15 §5).
pub const MODELS_REFRESH_MIN_INTERVAL_S: i64 = 5;
/// Local alias that only exists in `/v1/models` output (spec 15 §2.3).
pub const GPT_ALIAS: &str = "gpt-5.6";
pub const GPT_ALIAS_SOURCE: &str = "gpt-5.6-sol";
/// Default slug when the client omits `model` (spec 00 §3.2 item 4).
pub const DEFAULT_MODEL: &str = "claude-sonnet-5";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Provider {
    /// Default keeps [`crate::neutral::NeutralRequest`] `Default`-constructible;
    /// decoders always overwrite it from the catalog.
    #[default]
    OpenAi,
    Anthropic,
    Google,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::OpenAi => "open_ai",
            Provider::Anthropic => "anthropic",
            Provider::Google => "google",
        }
    }

    pub fn from_catalog_value(value: &str) -> Option<Self> {
        match value {
            "open_ai" => Some(Provider::OpenAi),
            "anthropic" => Some(Provider::Anthropic),
            "google" => Some(Provider::Google),
            _ => None,
        }
    }
}

/// One `supported_effort_levels` entry, values lowercased (spec 15 §1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortLevel {
    pub name: String,
    pub value: String,
    pub is_default: bool,
    pub supports_disabling_thinking: bool,
}

/// Capability record for one slug (spec 15 §2.1).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelEntry {
    pub slug: String,
    pub provider: Option<Provider>,
    pub display_name: Option<String>,
    pub max_token_count: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub supports_tools: bool,
    pub supports_images: bool,
    pub supports_thinking: bool,
    pub supports_disabling_thinking: bool,
    pub supports_parallel_tool_calls: bool,
    pub supports_server_side_compaction: bool,
    pub supported_effort_levels: Vec<EffortLevel>,
}

impl ModelEntry {
    fn new(slug: &str) -> Self {
        // Missing capability fields default to "send it" for tools/images/
        // thinking and "do not send it" for the opt-in flags (spec 25 §1).
        Self {
            slug: slug.to_string(),
            provider: None,
            display_name: None,
            max_token_count: None,
            max_output_tokens: None,
            supports_tools: true,
            supports_images: true,
            supports_thinking: true,
            supports_disabling_thinking: false,
            supports_parallel_tool_calls: false,
            supports_server_side_compaction: false,
            supported_effort_levels: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn has_effort_level(&self, value: &str) -> bool {
        let wanted = value.to_ascii_lowercase();
        self.supported_effort_levels
            .iter()
            .any(|level| level.value == wanted)
    }
}

/// What an encoder may do with a model. Unknown slugs get the permissive
/// defaults of spec 25 §6 step 4.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCapabilities {
    pub provider: Provider,
    pub max_output_tokens: Option<i64>,
    pub supports_tools: bool,
    pub supports_images: bool,
    pub supports_thinking: bool,
    pub supports_disabling_thinking: bool,
    pub supports_parallel_tool_calls: bool,
    pub supports_server_side_compaction: bool,
    pub supported_effort_levels: Vec<EffortLevel>,
}

/// Immutable registry snapshot. Refreshes swap the whole `Arc` behind
/// [`CatalogHandle`] so readers never see a half-updated table (spec 15 §2.2).
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    entries: Vec<ModelEntry>,
    index: HashMap<String, usize>,
}

impl Catalog {
    /// Parse the built-in catalog file. It is compiled in, so a failure is a
    /// build/repo defect and not a runtime condition.
    pub fn builtin() -> Result<Self, String> {
        let value: Value = serde_json::from_slice(BUILTIN_CATALOG)
            .map_err(|err| format!("built-in catalog is not valid JSON: {err}"))?;
        Self::from_builtin_value(&value).ok_or_else(|| "built-in catalog has no models".to_string())
    }

    /// Built-in catalog failures (already validated at build time) fall back to
    /// an empty registry rather than aborting a handler.
    #[cfg(test)]
    pub fn builtin_or_empty() -> Self {
        Self::builtin().unwrap_or_default()
    }

    /// Seeds from the built-in shape: `data[]` carries `id` + `owned_by`, and
    /// `models[]` carries the display name and reasoning levels.
    pub fn from_builtin_value(value: &Value) -> Option<Self> {
        let models = value.get("models")?.as_array()?;
        let data = value.get("data").and_then(Value::as_array);
        let mut entries: Vec<ModelEntry> = Vec::new();
        for item in models {
            let Some(slug) = item.get("slug").and_then(Value::as_str) else {
                continue;
            };
            if slug.is_empty() || slug == GPT_ALIAS {
                continue;
            }
            let mut entry = ModelEntry::new(slug);
            entry.display_name = item
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_string);
            let levels = item
                .get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .map(|levels| parse_builtin_levels(levels, &item.get("default_reasoning_level")))
                .unwrap_or_default();
            entry.supported_effort_levels = levels;
            entries.push(entry);
        }
        if let Some(data) = data {
            for item in data {
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(entry) = entries.iter_mut().find(|entry| entry.slug == id) else {
                    continue;
                };
                entry.provider = item
                    .get("owned_by")
                    .and_then(Value::as_str)
                    .and_then(Provider::from_catalog_value);
                entry.max_token_count = item.get("context_length").and_then(Value::as_i64);
                entry.max_output_tokens = item.get("max_output_tokens").and_then(Value::as_i64);
            }
        }
        let index = entries
            .iter()
            .enumerate()
            .map(|(position, entry)| (entry.slug.clone(), position))
            .collect();
        Some(Self { entries, index })
    }

    /// Build a registry from a Zed `/models` response (spec 15 §1, §2.2).
    /// Returns `None` when the payload has no `models` array, in which case the
    /// caller must keep the current registry untouched.
    pub fn from_zed_models(value: &Value) -> Option<Self> {
        let models = value.get("models")?.as_array()?;
        let mut entries: Vec<ModelEntry> = Vec::new();
        for item in models {
            let Some(object) = item.as_object() else {
                continue;
            };
            let Some(slug) = object.get("id").and_then(Value::as_str) else {
                continue;
            };
            if slug.is_empty() || slug == GPT_ALIAS {
                continue;
            }
            if object
                .get("is_disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let mut entry = ModelEntry::new(slug);
            entry.provider = object
                .get("provider")
                .and_then(Value::as_str)
                .and_then(Provider::from_catalog_value);
            entry.display_name = object
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_string);
            entry.max_token_count = object.get("max_token_count").and_then(Value::as_i64);
            entry.max_output_tokens = object.get("max_output_tokens").and_then(Value::as_i64);
            entry.supports_tools = bool_or(object, "supports_tools", true);
            entry.supports_images = bool_or(object, "supports_images", true);
            entry.supports_thinking = bool_or(object, "supports_thinking", true);
            entry.supports_disabling_thinking =
                bool_or(object, "supports_disabling_thinking", false);
            entry.supports_parallel_tool_calls =
                bool_or(object, "supports_parallel_tool_calls", false);
            entry.supports_server_side_compaction =
                bool_or(object, "supports_server_side_compaction", false);
            entry.supported_effort_levels = object
                .get("supported_effort_levels")
                .and_then(Value::as_array)
                .map(|levels| parse_catalog_levels(levels))
                .unwrap_or_default();
            entries.push(entry);
        }
        let index = entries
            .iter()
            .enumerate()
            .map(|(position, entry)| (entry.slug.clone(), position))
            .collect();
        Some(Self { entries, index })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, slug: &str) -> Option<&ModelEntry> {
        self.index
            .get(slug)
            .map(|position| &self.entries[*position])
    }

    #[cfg(test)]
    pub fn entries(&self) -> &[ModelEntry] {
        &self.entries
    }

    /// Model-name fallback table (spec 15 §7). The single implementation used by
    /// both the proxy routes and probe validation.
    pub fn normalize_model(&self, requested: Option<&str>) -> String {
        let Some(name) = requested else {
            return DEFAULT_MODEL.to_string();
        };
        if self.get(name).is_some() {
            return name.to_string();
        }
        if name.starts_with("gpt-5.6-sol") {
            "gpt-5.6-sol".to_string()
        } else if name.starts_with("gpt-5.6-terra") {
            "gpt-5.6-terra".to_string()
        } else if name.starts_with("gpt-5.6-luna") {
            "gpt-5.6-luna".to_string()
        } else if name.starts_with("gpt-5.6") {
            // The bare alias does not exist upstream; it 403s on plan.
            GPT_ALIAS_SOURCE.to_string()
        } else if name.starts_with("gpt-5.5") {
            "gpt-5.5".to_string()
        } else if name.starts_with("gpt-") {
            GPT_ALIAS_SOURCE.to_string()
        } else if name.starts_with("claude") {
            DEFAULT_MODEL.to_string()
        } else {
            name.to_string()
        }
    }

    /// Provider dispatch: catalog field first, name prefix only for slugs the
    /// catalog does not list (spec 15 §7.1).
    pub fn resolve_provider(&self, slug: &str) -> Result<Provider, GatewayError> {
        if let Some(entry) = self.get(slug) {
            return entry.provider.ok_or_else(|| unknown_provider(slug));
        }
        if slug.starts_with("claude") {
            Ok(Provider::Anthropic)
        } else if slug.starts_with("gpt-") {
            Ok(Provider::OpenAi)
        } else if slug.starts_with("gemini") {
            Ok(Provider::Google)
        } else {
            Ok(Provider::Anthropic)
        }
    }

    /// Whether `/v1/models` must synthesise the local `gpt-5.6` alias
    /// (spec 15 §6.1: Sol is listed and the bare name is not).
    pub fn has_alias(&self) -> bool {
        self.get(GPT_ALIAS).is_none() && self.get(GPT_ALIAS_SOURCE).is_some()
    }

    /// One `/v1/models` response: the OpenAI `data[]` list (alias first, then
    /// registry order) and the Codex `models[]` list (spec 15 §6.1, §6.2).
    /// Bytes are fixed: compact JSON, fixed key order, no spaces.
    pub fn models_payload(&self) -> Vec<u8> {
        let mut data = String::from("{\"object\":\"list\",\"data\":[");
        let mut codex = String::from("],\"models\":[");
        let mut data_count = 0usize;
        let mut priority = 0u32;

        if self.has_alias() {
            if let Some(source) = self.get(GPT_ALIAS_SOURCE) {
                push_data_entry(&mut data, &mut data_count, GPT_ALIAS, source);
                push_codex_entry(&mut codex, &mut priority, GPT_ALIAS, source);
            }
        }
        for entry in &self.entries {
            // `data[]` needs a named provider (spec 15 §6.2); `models[]` only
            // needs the slug.
            if entry.provider.is_some() {
                push_data_entry(&mut data, &mut data_count, &entry.slug, entry);
            }
            push_codex_entry(&mut codex, &mut priority, &entry.slug, entry);
        }
        data.push_str(&codex);
        data.push_str("]}");
        data.into_bytes()
    }

    /// Capabilities after normalization; unknown slugs get conservative
    /// defaults so the request is still forwarded (spec 25 §6).
    pub fn capabilities(&self, slug: &str) -> Result<ModelCapabilities, GatewayError> {
        let provider = self.resolve_provider(slug)?;
        let Some(entry) = self.get(slug) else {
            return Ok(ModelCapabilities {
                provider,
                max_output_tokens: None,
                supports_tools: true,
                supports_images: true,
                supports_thinking: true,
                supports_disabling_thinking: false,
                supports_parallel_tool_calls: false,
                supports_server_side_compaction: false,
                supported_effort_levels: Vec::new(),
            });
        };
        Ok(ModelCapabilities {
            provider,
            max_output_tokens: entry.max_output_tokens,
            supports_tools: entry.supports_tools,
            supports_images: entry.supports_images,
            supports_thinking: entry.supports_thinking,
            supports_disabling_thinking: entry.supports_disabling_thinking,
            supports_parallel_tool_calls: entry.supports_parallel_tool_calls,
            supports_server_side_compaction: entry.supports_server_side_compaction,
            supported_effort_levels: entry.supported_effort_levels.clone(),
        })
    }
}

/// `{"id":…,"object":"model","owned_by":…[,"context_length":…,
/// "max_output_tokens":…,"max_completion_tokens":…]}` (spec 15 §6.1). A limit
/// that is missing, null or non-positive omits the field instead of inventing
/// a default.
fn push_data_entry(out: &mut String, count: &mut usize, slug: &str, entry: &ModelEntry) {
    if *count > 0 {
        out.push(',');
    }
    *count += 1;
    let provider = entry
        .provider
        .map(Provider::as_str)
        .unwrap_or_else(|| "anthropic");
    let _ = write!(
        out,
        "{{\"id\":{},\"object\":\"model\",\"owned_by\":\"{provider}\"",
        json_string(slug)
    );
    if let Some(context) = positive(entry.max_token_count) {
        let _ = write!(out, ",\"context_length\":{context}");
    }
    if let Some(output) = positive(entry.max_output_tokens) {
        let _ = write!(
            out,
            ",\"max_output_tokens\":{output},\"max_completion_tokens\":{output}"
        );
    }
    out.push('}');
}

/// One Codex `models[]` item (spec 15 §6.2). Returns whether it was written.
fn push_codex_entry(out: &mut String, priority: &mut u32, slug: &str, entry: &ModelEntry) {
    if *priority > 0 {
        out.push(',');
    }
    *priority += 1;
    let display_name = entry.display_name.as_deref().unwrap_or(slug);
    let _ = write!(
        out,
        "{{\"slug\":{},\"display_name\":{},\"description\":{}",
        json_string(slug),
        json_string(display_name),
        json_string(&format!("Zed - {display_name}"))
    );
    if let Some(default) = entry
        .supported_effort_levels
        .iter()
        .find(|level| level.is_default)
    {
        let _ = write!(
            out,
            ",\"default_reasoning_level\":{}",
            json_string(&default.value)
        );
    }
    out.push_str(",\"supported_reasoning_levels\":[");
    let mut first = true;
    for level in &entry.supported_effort_levels {
        if !first {
            out.push(',');
        }
        first = false;
        let name = if level.name.is_empty() {
            &level.value
        } else {
            &level.name
        };
        let _ = write!(
            out,
            "{{\"effort\":{},\"description\":{}}}",
            json_string(&level.value),
            json_string(name)
        );
    }
    let _ = write!(
        out,
        "],\"shell_type\":\"shell_command\",\"visibility\":\"list\",\"supported_in_api\":true,\"priority\":{}}}",
        *priority
    );
}

fn positive(value: Option<i64>) -> Option<i64> {
    value.filter(|value| *value > 0)
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn unknown_provider(slug: &str) -> GatewayError {
    let mut err = GatewayError::request("unknown provider");
    err.code = Some("unknown_provider");
    err.message = format!("unknown provider for model {slug:?}");
    err
}

fn bool_or(object: &serde_json::Map<String, Value>, key: &str, default: bool) -> bool {
    object.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn parse_catalog_levels(levels: &[Value]) -> Vec<EffortLevel> {
    levels
        .iter()
        .filter_map(|level| {
            let object = level.as_object()?;
            let value = object.get("value")?.as_str()?.to_ascii_lowercase();
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&value)
                .to_string();
            Some(EffortLevel {
                name,
                value,
                is_default: object
                    .get("is_default")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                supports_disabling_thinking: object
                    .get("supports_disabling_thinking")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

fn parse_builtin_levels(levels: &[Value], default: &Option<&Value>) -> Vec<EffortLevel> {
    let default_level = default
        .and_then(Value::as_str)
        .map(|value| value.to_ascii_lowercase());
    levels
        .iter()
        .filter_map(|level| {
            let object = level.as_object()?;
            let value = object.get("effort")?.as_str()?.to_ascii_lowercase();
            let name = object
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or(&value)
                .to_string();
            Some(EffortLevel {
                is_default: default_level.as_deref() == Some(value.as_str()),
                name,
                value,
                supports_disabling_thinking: false,
            })
        })
        .collect()
}

/// `/v1/models` conversion cache plus the forced-refresh state (spec 15 §4,
/// §5). One instance lives in [`CatalogHandle`].
#[derive(Debug, Default)]
pub struct ModelsCache {
    payload: Option<Vec<u8>>,
    built_at: i64,
    /// When the last real upstream pull started (0 = never); reserves the
    /// 5 second window whether it succeeded or not (spec 15 §5).
    last_pull: i64,
}

impl ModelsCache {
    /// Cached bytes while younger than one hour (spec 15 §4).
    pub fn get(&self, now: i64) -> Option<&[u8]> {
        match &self.payload {
            Some(payload) if now - self.built_at < MODELS_CACHE_TTL_S => Some(payload),
            _ => None,
        }
    }

    /// Successful conversion: replace the bytes and stamp the time. An empty
    /// list is never cached (spec 15 §4).
    pub fn store(&mut self, payload: Vec<u8>, empty: bool, now: i64) -> bool {
        if empty {
            return false;
        }
        self.payload = Some(payload);
        self.built_at = now;
        true
    }

    pub fn invalidate(&mut self) {
        self.payload = None;
        self.built_at = 0;
    }

    /// `0` when a pull is allowed now, otherwise the remaining seconds rounded
    /// up; a pull always reserves the window (spec 15 §5).
    pub fn reserve_pull(&mut self, now: i64) -> i64 {
        let elapsed = now - self.last_pull;
        if self.last_pull != 0 && elapsed < MODELS_REFRESH_MIN_INTERVAL_S {
            return MODELS_REFRESH_MIN_INTERVAL_S - elapsed.max(0);
        }
        self.last_pull = now;
        0
    }
}

/// Shared, atomically replaceable catalog. Readers take a snapshot clone; the
/// refresh path swaps the snapshot in one write (spec 15 §4, §2.2 item 5).
#[derive(Debug)]
pub struct CatalogHandle {
    current: RwLock<Catalog>,
    models: Mutex<ModelsCache>,
    /// Serialises `/models` pulls so concurrent requests never both hit the
    /// upstream (spec 15 §4).
    pull_lock: tokio::sync::Mutex<()>,
}

impl CatalogHandle {
    pub fn new(catalog: Catalog) -> Self {
        Self {
            current: RwLock::new(catalog),
            models: Mutex::new(ModelsCache::default()),
            pull_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn snapshot(&self) -> Catalog {
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn replace(&self, catalog: Catalog) {
        *self
            .current
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = catalog;
    }

    pub fn len(&self) -> usize {
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Serialises catalog pulls across handlers (spec 15 §4).
    pub async fn lock_pull(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.pull_lock.lock().await
    }

    /// `None` when another task is already pulling (spec 15 §4).
    pub fn try_lock_pull(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        self.pull_lock.try_lock().ok()
    }

    /// Cached `/v1/models` bytes while fresh (spec 15 §4).
    pub fn cached_models(&self, now: i64) -> Option<Vec<u8>> {
        self.models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(now)
            .map(<[u8]>::to_vec)
    }

    /// Rebuild and cache `/v1/models` from the current registry. Empty lists
    /// are not cached (spec 15 §4).
    pub fn rebuild_models(&self, now: i64) -> Vec<u8> {
        let payload = self.snapshot().models_payload();
        let empty = self.snapshot().is_empty();
        self.models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .store(payload.clone(), empty, now);
        payload
    }

    /// Cached bytes even when stale (the "keep the old page" fallback,
    /// spec 15 §4 failure rows).
    pub fn stale_models(&self) -> Option<Vec<u8>> {
        self.models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .payload
            .clone()
    }

    pub fn invalidate_models(&self) {
        self.models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .invalidate();
    }

    /// Reserve the 5 second refresh window; `0` means the pull may proceed
    /// (spec 15 §5).
    pub fn reserve_refresh(&self, now: i64) -> i64 {
        self.models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reserve_pull(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> Catalog {
        Catalog::builtin().expect("built-in catalog parses")
    }

    #[test]
    fn builtin_catalog_seeds_without_the_alias() {
        let catalog = builtin();
        assert!(!catalog.is_empty());
        assert!(catalog.get(GPT_ALIAS).is_none());
        assert!(catalog.get(GPT_ALIAS_SOURCE).is_some());
        assert_eq!(
            catalog.get("claude-sonnet-5").unwrap().provider,
            Some(Provider::Anthropic)
        );
        assert_eq!(
            catalog.get("gemini-4-argon").unwrap().provider,
            Some(Provider::Google)
        );
    }

    #[test]
    fn catalog_models_are_forwarded_unchanged() {
        let catalog = builtin();
        assert_eq!(
            catalog.normalize_model(Some("gpt-5.6-luna")),
            "gpt-5.6-luna"
        );
        assert_eq!(
            catalog.normalize_model(Some("claude-sonnet-5-5")),
            "claude-sonnet-5-5"
        );
    }

    #[test]
    fn unknown_names_follow_the_fallback_table() {
        let catalog = builtin();
        assert_eq!(catalog.normalize_model(None), DEFAULT_MODEL);
        assert_eq!(catalog.normalize_model(Some("gpt-5.6")), "gpt-5.6-sol");
        assert_eq!(
            catalog.normalize_model(Some("gpt-5.6-preview")),
            "gpt-5.6-sol"
        );
        assert_eq!(catalog.normalize_model(Some("gpt-5.5-turbo")), "gpt-5.5");
        assert_eq!(catalog.normalize_model(Some("gpt-7")), "gpt-5.6-sol");
        assert_eq!(catalog.normalize_model(Some("claude-x")), DEFAULT_MODEL);
        assert_eq!(catalog.normalize_model(Some("gemini-x")), "gemini-x");
        assert_eq!(catalog.normalize_model(Some("grok-9")), "grok-9");
    }

    #[test]
    fn provider_dispatch_prefers_the_catalog_field() {
        let catalog = builtin();
        assert_eq!(
            catalog.resolve_provider("claude-sonnet-5").unwrap(),
            Provider::Anthropic
        );
        assert_eq!(
            catalog.resolve_provider("gpt-5.6-sol").unwrap(),
            Provider::OpenAi
        );
        assert_eq!(
            catalog.resolve_provider("gemini-4-argon").unwrap(),
            Provider::Google
        );
        // Unknown names fall back to the prefix table.
        assert_eq!(
            catalog.resolve_provider("gemini-99").unwrap(),
            Provider::Google
        );
        assert_eq!(
            catalog.resolve_provider("grok-9").unwrap(),
            Provider::Anthropic
        );
    }

    #[test]
    fn refresh_ignores_a_payload_without_models() {
        let empty = serde_json::json!({"object": "list"});
        assert!(Catalog::from_zed_models(&empty).is_none());
        let not_array = serde_json::json!({"models": 5});
        assert!(Catalog::from_zed_models(&not_array).is_none());
    }

    #[test]
    fn refresh_picks_up_capability_fields_and_skips_disabled() {
        let payload = serde_json::json!({"models": [
            {
                "id": "m1", "provider": "open_ai", "display_name": "M1",
                "max_token_count": 400000, "max_output_tokens": 128000,
                "supports_tools": false, "supports_thinking": false,
                "supported_effort_levels": [{"name": "Low", "value": "LOW", "is_default": true}]
            },
            {"id": "m2", "provider": "anthropic", "is_disabled": true},
            {"id": "gpt-5.6", "provider": "open_ai"},
            {"id": "", "provider": "google"},
            {"provider": "google"}
        ]});
        let catalog = Catalog::from_zed_models(&payload).unwrap();
        assert_eq!(catalog.len(), 1);
        let entry = catalog.get("m1").unwrap();
        assert_eq!(entry.provider, Some(Provider::OpenAi));
        assert_eq!(entry.max_token_count, Some(400_000));
        assert_eq!(entry.max_output_tokens, Some(128_000));
        assert!(!entry.supports_tools);
        assert!(entry.has_effort_level("low"));
        assert!(entry.supported_effort_levels[0].is_default);
    }

    #[test]
    fn handle_replace_swaps_the_snapshot() {
        let handle = CatalogHandle::new(Catalog::default());
        assert!(handle.is_empty());
        handle.replace(builtin());
        assert!(!handle.is_empty());
    }

    #[test]
    fn builtin_catalog_has_twenty_three_models_with_limits() {
        let catalog = builtin();
        assert_eq!(catalog.len(), 23);
        let entry = catalog.get("claude-sonnet-5-5").unwrap();
        assert_eq!(entry.max_token_count, Some(1_000_000));
        assert_eq!(entry.max_output_tokens, Some(128_000));
        // Cold start must already know the budgets (spec 15 §3 item 3).
        assert!(catalog
            .entries()
            .iter()
            .all(|entry| entry.max_output_tokens.unwrap_or(0) > 0));
    }

    #[test]
    fn models_payload_has_the_alias_first_and_c3_fields() {
        let catalog = builtin();
        assert!(catalog.has_alias());
        let text = String::from_utf8(catalog.models_payload()).unwrap();
        // Compact JSON: no spaces outside string values (display names have
        // them), one object with both lists.
        assert!(!text.contains("\": "));
        assert!(!text.contains(", "));
        assert!(!text.contains('\n'));
        assert!(text.starts_with(
            "{\"object\":\"list\",\"data\":[{\"id\":\"gpt-5.6\",\"object\":\"model\",\"owned_by\":\"open_ai\","
        ));
        assert!(text.contains(
            "\"context_length\":400000,\"max_output_tokens\":128000,\"max_completion_tokens\":128000"
        ));
        assert!(text.contains("],\"models\":[{\"slug\":\"gpt-5.6\""));
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["data"].as_array().unwrap().len(), 24);
        assert_eq!(value["models"].as_array().unwrap().len(), 24);
        // Priorities are dense from 1.
        for (index, item) in value["models"].as_array().unwrap().iter().enumerate() {
            assert_eq!(item["priority"], (index + 1) as u64);
        }
    }

    #[test]
    fn missing_or_non_positive_limits_are_omitted() {
        let payload = serde_json::json!({"models": [
            {"id": "no-limits", "provider": "anthropic"},
            {"id": "zero", "provider": "google", "max_token_count": 0, "max_output_tokens": -5},
            {"id": "only-context", "provider": "open_ai", "max_token_count": 10}
        ]});
        let catalog = Catalog::from_zed_models(&payload).unwrap();
        let text = String::from_utf8(catalog.models_payload()).unwrap();
        assert!(
            text.contains("\"id\":\"no-limits\",\"object\":\"model\",\"owned_by\":\"anthropic\"}")
        );
        assert!(text.contains("\"id\":\"zero\",\"object\":\"model\",\"owned_by\":\"google\"}"));
        assert!(text.contains(
            "\"id\":\"only-context\",\"object\":\"model\",\"owned_by\":\"open_ai\",\"context_length\":10}"
        ));
        assert!(text.contains("\"models\":[{\"slug\":\"no-limits\""));
    }

    #[test]
    fn alias_is_not_synthesised_without_its_source() {
        let payload = serde_json::json!({"models": [{"id": "m1", "provider": "anthropic"}]});
        let catalog = Catalog::from_zed_models(&payload).unwrap();
        assert!(!catalog.has_alias());
        let text = String::from_utf8(catalog.models_payload()).unwrap();
        assert!(!text.contains("\"id\":\"gpt-5.6\""));
        assert!(text.starts_with("{\"object\":\"list\",\"data\":[{\"id\":\"m1\""));
    }

    #[test]
    fn catalog_entry_without_provider_is_in_models_but_not_data() {
        let payload = serde_json::json!({"models": [
            {"id": "m1", "display_name": "M1"},
            {"id": "m2", "provider": "anthropic"}
        ]});
        let catalog = Catalog::from_zed_models(&payload).unwrap();
        let value: Value = serde_json::from_slice(&catalog.models_payload()).unwrap();
        assert_eq!(value["data"].as_array().unwrap().len(), 1);
        assert_eq!(value["data"][0]["id"], "m2");
        assert_eq!(value["models"].as_array().unwrap().len(), 2);
        assert_eq!(value["models"][0]["priority"], 1);
        assert_eq!(value["models"][1]["priority"], 2);
        assert_eq!(value["models"][0]["description"], "Zed - M1");
    }

    #[test]
    fn models_cache_expires_after_an_hour_and_skips_empty_lists() {
        let mut cache = ModelsCache::default();
        assert!(cache.get(100).is_none());
        assert!(!cache.store(b"x".to_vec(), true, 100));
        assert!(cache.get(100).is_none());
        assert!(cache.store(b"payload".to_vec(), false, 100));
        assert_eq!(cache.get(100), Some(&b"payload"[..]));
        assert_eq!(
            cache.get(100 + MODELS_CACHE_TTL_S - 1),
            Some(&b"payload"[..])
        );
        assert!(cache.get(100 + MODELS_CACHE_TTL_S).is_none());
    }

    #[test]
    fn refresh_window_is_five_seconds_and_reserves_on_every_pull() {
        let mut cache = ModelsCache::default();
        assert_eq!(cache.reserve_pull(1_000), 0);
        assert_eq!(cache.reserve_pull(1_000), 5);
        assert_eq!(cache.reserve_pull(1_003), 2);
        assert_eq!(cache.reserve_pull(1_005), 0);
        assert_eq!(cache.reserve_pull(1_006), 4);
    }

    #[test]
    fn handle_caches_and_invalidates_payloads() {
        let handle = CatalogHandle::new(Catalog::default());
        assert!(handle.cached_models(10).is_none());
        handle.rebuild_models(10);
        // An empty registry must not be cached.
        assert!(handle.cached_models(10).is_none());
        handle.replace(builtin());
        let payload = handle.rebuild_models(20);
        assert_eq!(handle.cached_models(3_619).as_deref(), Some(&payload[..]));
        assert!(handle.cached_models(3_620).is_none());
        handle.invalidate_models();
        assert!(handle.cached_models(20).is_none());
    }
}
