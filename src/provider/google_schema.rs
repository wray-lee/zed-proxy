//! Gemini JSON-Schema cleaning (spec `45-conversion-matrix.md` §4.1.1, expanded
//! in `22-provider-google.md` §6).
//!
//! Only the `google` provider calls this. The authoritative rules live in the
//! spec; this module is the single place they are applied.

use serde_json::{Map, Value};

/// Keys the Gemini schema subset accepts, in the order the request writer emits
/// them (spec 22 §6.3).
const ALLOWED_KEYS: [&str; 19] = [
    "type",
    "format",
    "title",
    "description",
    "nullable",
    "enum",
    "items",
    "properties",
    "required",
    "anyOf",
    "minItems",
    "maxItems",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "pattern",
    "minProperties",
    "maxProperties",
];

/// `format` values Gemini understands; anything else loses the key itself
/// (spec 22 §6.3).
const ALLOWED_FORMATS: [&str; 6] = ["enum", "date-time", "int32", "int64", "float", "double"];
/// Local `$ref` inlining stops below this depth (spec 22 §6.4 item 1).
const MAX_INLINE_DEPTH: usize = 8;
/// Total number of inlined `$ref` nodes per tool schema (spec 22 §6.4 item 1).
const MAX_INLINED_NODES: usize = 256;

struct Context<'a> {
    /// The unmodified `parameters` object; `#/$defs/…` resolves against it.
    root: &'a Value,
    /// `$ref` values on the current inlining path, for cycle detection.
    stack: Vec<String>,
    inlined: usize,
}

/// Recursively rewrite a client JSON Schema into the Gemini subset: drop
/// unknown keywords, convert `const`/`oneOf`/type arrays, inline local `$ref`s
/// with a depth cap and cycle guard, and keep `properties` keys untouched.
pub fn clean_schema(schema: &Value) -> Value {
    let mut context = Context {
        root: schema,
        stack: Vec::new(),
        inlined: 0,
    };
    clean_node(schema, &mut context, 0)
}

/// Whether the cleaned tool parameters are worth sending at all (spec 22 §6.1):
/// no `properties` and not an array/scalar schema means "omit `parameters`".
pub fn has_parameters(schema: &Value) -> bool {
    let has_properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| !properties.is_empty())
        .unwrap_or(false);
    let kind = schema.get("type").and_then(Value::as_str).unwrap_or("");
    has_properties || matches!(kind, "array" | "string" | "number" | "integer" | "boolean")
}

/// Serialize one cleaned schema with a stable key order (spec 22 §2: JSON is
/// compared byte for byte, so map iteration order cannot leak into the output).
pub(crate) fn write_schema(out: &mut String, schema: &Value) {
    let Some(map) = schema.as_object() else {
        out.push_str("{}");
        return;
    };
    out.push('{');
    let mut first = true;
    for key in ALLOWED_KEYS {
        let Some(value) = map.get(key) else {
            continue;
        };
        if !first {
            out.push(',');
        }
        first = false;
        out.push('"');
        out.push_str(key);
        out.push('"');
        out.push(':');
        match key {
            "properties" => write_properties(out, value),
            "items" => write_schema(out, value),
            "anyOf" => write_members(out, value),
            _ => out.push_str(&value.to_string()),
        }
    }
    out.push('}');
}

fn write_properties(out: &mut String, value: &Value) {
    out.push('{');
    let mut first = true;
    if let Some(properties) = value.as_object() {
        for (name, schema) in properties {
            if !first {
                out.push(',');
            }
            first = false;
            match serde_json::to_string(name) {
                Ok(encoded) => out.push_str(&encoded),
                Err(_) => out.push_str("\"\""),
            }
            out.push(':');
            write_schema(out, schema);
        }
    }
    out.push('}');
}

fn write_members(out: &mut String, value: &Value) {
    out.push('[');
    if let Some(members) = value.as_array() {
        for (position, member) in members.iter().enumerate() {
            if position > 0 {
                out.push(',');
            }
            write_schema(out, member);
        }
    }
    out.push(']');
}

fn degraded() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

fn clean_node(value: &Value, context: &mut Context, depth: usize) -> Value {
    let Some(source) = value.as_object() else {
        return Value::Object(Map::new());
    };
    let mut map = source.clone();
    if map.contains_key("$ref") {
        let reference = map.remove("$ref").unwrap_or(Value::Null);
        match inline(&reference, context, depth) {
            Some(mut target) => {
                // Sibling keys win over the referenced definition (spec 22 §6.4 item 1).
                for (key, value) in map {
                    target.insert(key, value);
                }
                map = target;
            }
            None => return degraded(),
        }
    }
    convert(&mut map, context, depth);
    Value::Object(map)
}

/// Resolve one local `$ref` and clean the target. `None` means "degrade".
fn inline(reference: &Value, context: &mut Context, depth: usize) -> Option<Map<String, Value>> {
    let reference = reference.as_str()?;
    let pointer = reference.strip_prefix('#')?;
    if !pointer.starts_with("/$defs/") && !pointer.starts_with("/definitions/") {
        return None; // external or unsupported reference
    }
    if depth >= MAX_INLINE_DEPTH || context.inlined >= MAX_INLINED_NODES {
        return None;
    }
    if context.stack.iter().any(|entry| entry == reference) {
        return None; // cycle
    }
    let target = context.root.pointer(pointer)?;
    context.inlined += 1;
    context.stack.push(reference.to_string());
    let cleaned = clean_node(target, context, depth + 1);
    context.stack.pop();
    match cleaned {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

fn convert(map: &mut Map<String, Value>, context: &mut Context, depth: usize) {
    // 2. const → single-value enum (an existing enum wins).
    if let Some(value) = map.remove("const") {
        map.entry("enum".to_string())
            .or_insert_with(|| Value::Array(vec![value]));
    }
    // 3. oneOf → anyOf; members append after any existing anyOf.
    if let Some(value) = map.remove("oneOf") {
        let mut members = match map.remove("anyOf") {
            Some(Value::Array(members)) => members,
            _ => Vec::new(),
        };
        match value {
            Value::Array(extra) => members.extend(extra),
            other => members.push(other),
        }
        map.insert("anyOf".to_string(), Value::Array(members));
    }
    normalize_type(map);
    normalize_enum(map);
    consistency(map);
    clean_required(map);
    clean_children(map, context, depth);
    ensure_items(map);
    // Gemini rejects any node without a `type` ("didn't specify the schema type
    // field"), including the typeless result of a dropped or const-only enum;
    // only an `anyOf` node may omit it.
    if !map.is_empty() && !map.contains_key("type") && !map.contains_key("anyOf") {
        let inferred = infer_type(map);
        map.insert("type".to_string(), Value::String(inferred));
    }
    map.retain(|key, _| ALLOWED_KEYS.contains(&key.as_str()));
}

/// Steps 4 and 5: type arrays, `"null"` handling, spelling normalization and
/// the "unknown type" inference.
fn normalize_type(map: &mut Map<String, Value>) {
    let mut nullable = map
        .get("nullable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut chosen: Option<String> = None;
    let mut had_type = false;
    match map.remove("type") {
        Some(Value::Array(names)) => {
            had_type = true;
            if names.iter().any(|name| name.as_str() == Some("null")) {
                nullable = true;
            }
            chosen = names
                .iter()
                .filter_map(Value::as_str)
                .find(|name| *name != "null")
                .map(str::to_string);
            if chosen.is_none() {
                chosen = Some("object".to_string());
            }
        }
        Some(Value::String(name)) => {
            had_type = true;
            if name == "null" {
                nullable = true;
            } else {
                chosen = Some(name);
            }
        }
        Some(_) => had_type = true,
        None => {}
    }
    if let Some(name) = chosen {
        let normalized = match map_type(&name) {
            Some(normalized) => normalized.to_string(),
            None => infer_type(map),
        };
        map.insert("type".to_string(), Value::String(normalized));
    } else if had_type && !map.contains_key("type") && !nullable {
        // `type: ["null"]` — no usable type is left.
        map.insert("type".to_string(), Value::String("object".to_string()));
    }
    if nullable {
        map.insert("nullable".to_string(), Value::Bool(true));
    }
}

fn map_type(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    Some(match name.as_str() {
        "string" => "string",
        "number" => "number",
        "integer" => "integer",
        "boolean" => "boolean",
        "array" => "array",
        "object" => "object",
        "float" | "double" => "number",
        "int" | "int32" | "int64" => "integer",
        "bool" => "boolean",
        "dict" | "map" => "object",
        "list" | "set" => "array",
        other if other.starts_with("uint") => "integer",
        _ => return None,
    })
}

fn infer_type(map: &Map<String, Value>) -> String {
    if map.contains_key("properties") || map.contains_key("required") {
        "object".to_string()
    } else if map.contains_key("items") {
        "array".to_string()
    } else {
        "string".to_string()
    }
}

/// Step 10 (enum side, spec 22 §6.4.1): Gemini enums are non-empty string
/// arrays. Empty strings are dropped; a non-string element, an empty result or
/// a non-array value deletes the whole key, keeping dropped scalar candidates
/// visible in `description`.
fn normalize_enum(map: &mut Map<String, Value>) {
    let Some(value) = map.remove("enum") else {
        return;
    };
    let Value::Array(items) = value else {
        drop_enum_format(map);
        return;
    };
    let mut kept: Vec<Value> = Vec::new();
    let mut unrepresentable = false;
    for item in &items {
        match item {
            Value::String(text) if text.is_empty() => {}
            Value::String(text) => {
                if !kept
                    .iter()
                    .any(|entry| entry.as_str() == Some(text.as_str()))
                {
                    kept.push(Value::String(text.clone()));
                }
            }
            _ => unrepresentable = true,
        }
    }
    if unrepresentable {
        // Numeric/boolean candidates (including `const` of those types) stay
        // visible to the model through `description` (spec 22 §6.4.1).
        if items
            .iter()
            .all(|item| matches!(item, Value::Number(_) | Value::Bool(_)))
        {
            append_allowed_values(map, &items);
        }
        drop_enum_format(map);
        return;
    }
    if kept.is_empty() {
        drop_enum_format(map);
        return;
    }
    map.insert("enum".to_string(), Value::Array(kept));
}

/// `format:"enum"` only carries meaning next to an `enum` (spec 22 §6.4.1).
fn drop_enum_format(map: &mut Map<String, Value>) {
    if map.get("format").and_then(Value::as_str) == Some("enum") {
        map.remove("format");
    }
}

/// Keep values that Gemini cannot express as enum candidates visible to the
/// model, appending after an existing description (spec 22 §6.4.1).
fn append_allowed_values(map: &mut Map<String, Value>, values: &[Value]) {
    let rendered: Vec<String> = values.iter().map(Value::to_string).collect();
    let note = format!("Allowed values: {}", rendered.join(", "));
    let description = match map.get("description").and_then(Value::as_str) {
        Some(existing) => format!("{existing}\n{note}"),
        None => note,
    };
    map.insert("description".to_string(), Value::String(description));
}

/// Step 10 (array side, spec 22 §6.4.2): every `type: array` node carries an
/// object `items` with a `type`. A missing or empty schema becomes the loose
/// object schema; keys the client did provide are kept.
fn ensure_items(map: &mut Map<String, Value>) {
    if map.get("type").and_then(Value::as_str) != Some("array") {
        return;
    }
    if !map.get("items").map(Value::is_object).unwrap_or(false) {
        map.insert("items".to_string(), degraded());
        return;
    }
    if let Some(items) = map.get_mut("items").and_then(Value::as_object_mut) {
        if items.get("type").and_then(Value::as_str).is_none() {
            items.insert("type".to_string(), Value::String("object".to_string()));
            items
                .entry("properties".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
        }
    }
}

/// Step 6: keep `type` and the structural keywords consistent.
fn consistency(map: &mut Map<String, Value>) {
    let has_properties = map.get("properties").map(Value::is_object).unwrap_or(false);
    let has_required = map.get("required").map(Value::is_array).unwrap_or(false);
    if (has_properties || has_required) && !map.contains_key("type") {
        map.insert("type".to_string(), Value::String("object".to_string()));
    }
    if map.contains_key("items") && !map.contains_key("type") {
        map.insert("type".to_string(), Value::String("array".to_string()));
    }
    let kind = map
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if kind != "array" {
        map.remove("items");
    }
    if kind != "object" {
        map.remove("properties");
        map.remove("required");
    }
}

/// Step 7: `required` may only name properties that exist.
fn clean_required(map: &mut Map<String, Value>) {
    let Some(Value::Array(items)) = map.get("required").cloned() else {
        if map.contains_key("required") {
            map.remove("required");
        }
        return;
    };
    let properties = map.get("properties").and_then(Value::as_object);
    let mut kept: Vec<Value> = Vec::new();
    for item in items {
        let Some(name) = item.as_str() else {
            continue;
        };
        let declared = properties
            .map(|properties| properties.contains_key(name))
            .unwrap_or(false);
        if declared && !kept.iter().any(|entry| entry.as_str() == Some(name)) {
            kept.push(Value::String(name.to_string()));
        }
    }
    if kept.is_empty() {
        map.remove("required");
    } else {
        map.insert("required".to_string(), Value::Array(kept));
    }
}

/// Steps 8 and 10: recurse into `properties` values, `items` and `anyOf`.
fn clean_children(map: &mut Map<String, Value>, context: &mut Context, depth: usize) {
    if let Some(Value::Object(properties)) = map.get("properties").cloned() {
        let mut cleaned = Map::new();
        for (name, schema) in properties {
            if schema.is_object() {
                cleaned.insert(name, clean_node(&schema, context, depth));
            }
        }
        map.insert("properties".to_string(), Value::Object(cleaned));
    } else if map.contains_key("properties") {
        map.remove("properties");
    }
    if let Some(value) = map.get("items").cloned() {
        if value.is_object() {
            map.insert("items".to_string(), clean_node(&value, context, depth));
        } else {
            map.remove("items");
        }
    }
    if let Some(Value::Array(members)) = map.get("anyOf").cloned() {
        let cleaned = members
            .iter()
            .map(|member| clean_node(member, context, depth))
            .collect();
        map.insert("anyOf".to_string(), Value::Array(cleaned));
    } else if map.contains_key("anyOf") {
        map.remove("anyOf");
    }
    match map.get("format").and_then(Value::as_str) {
        Some(format) if ALLOWED_FORMATS.contains(&format) => {}
        _ => {
            map.remove("format");
        }
    }
    if !matches!(map.get("nullable"), Some(Value::Bool(_))) {
        map.remove("nullable");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cleaned(schema: Value) -> Value {
        clean_schema(&schema)
    }

    fn rendered(schema: Value) -> String {
        let mut out = String::new();
        write_schema(&mut out, &cleaned(schema));
        out
    }

    #[test]
    fn parameter_presence_rule() {
        assert!(!has_parameters(
            &serde_json::json!({"type": "object", "properties": {}})
        ));
        assert!(has_parameters(&serde_json::json!({
            "type": "object",
            "properties": {"a": {"type": "string"}}
        })));
        assert!(has_parameters(&serde_json::json!({
            "type": "array",
            "items": {"type": "string"}
        })));
    }

    #[test]
    fn real_400_example_one_drops_schema_and_additional_properties() {
        let input = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        });
        assert_eq!(
            rendered(input),
            r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#
        );
    }

    #[test]
    fn real_400_example_two_drops_property_names_inside_the_second_property() {
        let input = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "vars": {
                    "type": "object",
                    "propertyNames": {"pattern": "^[A-Z_]+$"},
                    "additionalProperties": {"type": "string"}
                }
            },
            "required": ["path", "vars"]
        });
        assert_eq!(
            rendered(input),
            r#"{"type":"object","properties":{"path":{"type":"string"},"vars":{"type":"object"}},"required":["path","vars"]}"#
        );
    }

    #[test]
    fn conversion_table_from_the_spec() {
        assert_eq!(
            rendered(serde_json::json!({"const": "a"})),
            r#"{"type":"string","enum":["a"]}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"oneOf": [{"type": "string"}, {"type": "integer"}]})),
            r#"{"anyOf":[{"type":"string"},{"type":"integer"}]}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"type": ["string", "null"]})),
            r#"{"type":"string","nullable":true}"#
        );
        assert_eq!(
            rendered(
                serde_json::json!({"$ref": "#/$defs/X", "$defs": {"X": {"type": "object", "properties": {"a": {"type": "string"}}}}})
            ),
            r#"{"type":"object","properties":{"a":{"type":"string"}}}"#
        );
        assert_eq!(
            rendered(
                serde_json::json!({"$ref": "#/$defs/X", "$defs": {"X": {"$ref": "#/$defs/X"}}})
            ),
            r#"{"type":"object","properties":{}}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"format": "uri", "type": "string"})),
            r#"{"type":"string"}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"format": "date-time", "type": "string"})),
            r#"{"type":"string","format":"date-time"}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"properties": {"a": {"type": "string"}}})),
            r#"{"type":"object","properties":{"a":{"type":"string"}}}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"type": "object", "propertyNames": {"pattern": "^a$"}})),
            r#"{"type":"object"}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"type": "object", "additionalProperties": false})),
            r#"{"type":"object"}"#
        );
        assert_eq!(
            rendered(
                serde_json::json!({"type": "object", "properties": {"format": {"type": "string"}, "$schema": {"type": "boolean"}}})
            ),
            r#"{"type":"object","properties":{"format":{"type":"string"},"$schema":{"type":"boolean"}}}"#
        );
        // Non-string enums are not stringified (spec 22 §6.4.1); the candidates
        // survive in `description` instead.
        assert_eq!(
            rendered(serde_json::json!({"enum": [1, true]})),
            r#"{"type":"string","description":"Allowed values: 1, true"}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"enum": [1, true], "type": "integer"})),
            r#"{"type":"integer","description":"Allowed values: 1, true"}"#
        );
        // `required` alone implies an object (rule 6) before the name filter runs.
        assert_eq!(
            rendered(serde_json::json!({"required": ["missing"]})),
            r#"{"type":"object"}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"items": {"type": "string"}})),
            r#"{"type":"array","items":{"type":"string"}}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"allOf": [{"type": "string"}], "description": "x"})),
            r#"{"type":"string","description":"x"}"#
        );
    }

    #[test]
    fn empty_parameters_are_omitted_by_the_caller_rule() {
        let cleaned =
            cleaned(serde_json::json!({"type": "object", "properties": {}, "required": []}));
        assert_eq!(
            cleaned,
            serde_json::json!({"type": "object", "properties": {}})
        );
        assert!(!has_parameters(&cleaned));
    }

    #[test]
    fn type_aliases_and_unknown_types() {
        assert_eq!(
            rendered(serde_json::json!({"type": "float"})),
            r#"{"type":"number"}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"type": "uint32"})),
            r#"{"type":"integer"}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"type": "dict"})),
            r#"{"type":"object"}"#
        );
        // The alias resolves to `array` first, so §6.4.2 then supplies `items`.
        assert_eq!(
            rendered(serde_json::json!({"type": "list"})),
            r#"{"type":"array","items":{"type":"object","properties":{}}}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"type": "wat"})),
            r#"{"type":"string"}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"type": "wat", "items": {"type": "string"}})),
            r#"{"type":"array","items":{"type":"string"}}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"type": ["integer", "null"]})),
            r#"{"type":"integer","nullable":true}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"type": ["null"]})),
            r#"{"type":"object","nullable":true}"#
        );
    }

    #[test]
    fn string_enums_are_deduplicated_and_filtered() {
        assert_eq!(
            rendered(serde_json::json!({"type": "string", "enum": ["a", "a", "b", ""]})),
            r#"{"type":"string","enum":["a","b"]}"#
        );
    }

    #[test]
    fn empty_string_enum_entries_are_dropped() {
        assert_eq!(
            rendered(serde_json::json!({"type": "string", "enum": ["read", "write", ""]})),
            r#"{"type":"string","enum":["read","write"]}"#
        );
        // Only zero-length strings are dropped; whitespace is a real value.
        assert_eq!(
            rendered(serde_json::json!({"type": "string", "enum": ["", " " ]})),
            r#"{"type":"string","enum":[" "]}"#
        );
        for empty in [
            serde_json::json!({"type": "string", "enum": []}),
            serde_json::json!({"type": "string", "enum": [""]}),
            serde_json::json!({"type": "string", "enum": ["", ""]}),
        ] {
            assert_eq!(rendered(empty), r#"{"type":"string"}"#);
        }
    }

    #[test]
    fn non_string_enums_are_dropped_and_candidates_kept_in_description() {
        assert_eq!(
            rendered(serde_json::json!({"type": "integer", "enum": [1, 2, 3]})),
            r#"{"type":"integer","description":"Allowed values: 1, 2, 3"}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"type": "boolean", "enum": [true, false]})),
            r#"{"type":"boolean","description":"Allowed values: true, false"}"#
        );
        // An existing description is extended, never replaced.
        assert_eq!(
            rendered(serde_json::json!({"type": "integer", "description": "n", "enum": [1, 2]})),
            r#"{"type":"integer","description":"n\nAllowed values: 1, 2"}"#
        );
        // Mixed string/non-string and null elements drop the whole enum; `type` stays.
        assert_eq!(
            rendered(serde_json::json!({"type": "string", "enum": ["a", 1]})),
            r#"{"type":"string"}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"type": "string", "enum": [null]})),
            r#"{"type":"string"}"#
        );
        // A non-array enum and a dangling `format:"enum"` both disappear.
        assert_eq!(
            rendered(serde_json::json!({"type": "string", "enum": "a", "format": "enum"})),
            r#"{"type":"string"}"#
        );
    }

    #[test]
    fn const_values_follow_the_enum_rules() {
        assert_eq!(rendered(serde_json::json!({"const": ""})), r#"{}"#);
        assert_eq!(
            rendered(serde_json::json!({"const": "a"})),
            r#"{"type":"string","enum":["a"]}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"const": 1})),
            r#"{"type":"string","description":"Allowed values: 1"}"#
        );
        assert_eq!(
            rendered(serde_json::json!({"type": "string", "const": ""})),
            r#"{"type":"string"}"#
        );
    }

    #[test]
    fn arrays_get_a_loose_items_schema() {
        let loose = r#"{"type":"array","items":{"type":"object","properties":{}}}"#;
        assert_eq!(rendered(serde_json::json!({"type": "array"})), loose);
        assert_eq!(
            rendered(serde_json::json!({"type": "array", "items": {}})),
            loose
        );
        // Existing keys survive the patch-up.
        assert_eq!(
            rendered(serde_json::json!({"type": "array", "items": {"description": "note"}})),
            r#"{"type":"array","items":{"type":"string","description":"note"}}"#
        );
        for invalid in [
            serde_json::json!(true),
            serde_json::json!("x"),
            serde_json::json!([1]),
            serde_json::json!(null),
        ] {
            assert_eq!(
                rendered(serde_json::json!({"type": "array", "items": invalid})),
                loose
            );
        }
        // An explicit item type is client intent and stays untouched.
        assert_eq!(
            rendered(serde_json::json!({"type": "array", "items": {"type": "string"}})),
            r#"{"type":"array","items":{"type":"string"}}"#
        );
        // Nested bare arrays are patched all the way down.
        assert_eq!(
            rendered(serde_json::json!({"type": "array", "items": {"type": "array"}})),
            r#"{"type":"array","items":{"type":"array","items":{"type":"object","properties":{}}}}"#
        );
        // `items` on a non-array node is still dropped (spec 22 §6.4 item 6).
        assert_eq!(
            rendered(serde_json::json!({"type": "object", "items": {"type": "string"}})),
            r#"{"type":"object"}"#
        );
        // Items that lose their enum still receive the loose object shape.
        assert_eq!(
            rendered(serde_json::json!({"type": "array", "items": {"enum": [1, 2]}})),
            r#"{"type":"array","items":{"type":"string","description":"Allowed values: 1, 2"}}"#
        );
        // Exact fixture from spec 22 §6.4.2: a nested bare array under `properties`.
        assert_eq!(
            rendered(serde_json::json!({"properties": {"tags": {"type": "array"}}})),
            r#"{"type":"object","properties":{"tags":{"type":"array","items":{"type":"object","properties":{}}}}}"#
        );
    }

    #[test]
    fn enum_and_items_rules_reach_nested_and_inlined_nodes() {
        assert_eq!(
            rendered(serde_json::json!({
                "type": "object",
                "properties": {
                    "mode": {"type": "string", "enum": ["read", "write", ""]},
                    "count": {"type": "integer", "enum": [1, 2]},
                    "tags": {"type": "array"},
                    "flags": {"type": "array", "items": {}}
                }
            })),
            r#"{"type":"object","properties":{"mode":{"type":"string","enum":["read","write"]},"count":{"type":"integer","description":"Allowed values: 1, 2"},"tags":{"type":"array","items":{"type":"object","properties":{}}},"flags":{"type":"array","items":{"type":"object","properties":{}}}}}"#
        );
        assert_eq!(
            rendered(serde_json::json!({
                "type": "array",
                "items": {"type": "array", "items": {"type": "string", "enum": ["x", ""]}}
            })),
            r#"{"type":"array","items":{"type":"array","items":{"type":"string","enum":["x"]}}}"#
        );
        assert_eq!(
            rendered(serde_json::json!({
                "anyOf": [
                    {"type": "string", "enum": [""]},
                    {"type": "integer", "enum": [1, 2]},
                    {"type": "array"}
                ]
            })),
            r#"{"anyOf":[{"type":"string"},{"type":"integer","description":"Allowed values: 1, 2"},{"type":"array","items":{"type":"object","properties":{}}}]}"#
        );
        assert_eq!(
            rendered(serde_json::json!({
                "$ref": "#/$defs/X",
                "$defs": {
                    "X": {
                        "type": "object",
                        "properties": {
                            "kind": {"type": "string", "enum": ["a", ""]},
                            "rows": {"type": "array"}
                        }
                    }
                }
            })),
            r#"{"type":"object","properties":{"kind":{"type":"string","enum":["a"]},"rows":{"type":"array","items":{"type":"object","properties":{}}}}}"#
        );
    }

    #[test]
    fn cleaned_schemas_never_contain_illegal_enums_or_bare_arrays() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "mode": {"type": "string", "enum": ["read", "write", ""]},
                "empty": {"type": "string", "enum": []},
                "count": {"type": "integer", "enum": [1, 2, 3]},
                "flag": {"type": "boolean", "enum": [true, false]},
                "value": {"const": ""},
                "nested": {"type": "object", "properties": {"inner": {"type": "array"}}},
                "rows": {"type": "array", "items": {}},
                "choice": {"anyOf": [{"type": "array"}, {"type": "string", "enum": [""]}]},
                "linked": {"$ref": "#/$defs/L"}
            },
            "$defs": {"L": {"type": "array", "items": {"type": "integer", "enum": [1]}}}
        });
        let cleaned = clean_schema(&schema);
        assert_legal(&cleaned);
        let text = cleaned.to_string();
        for banned in [
            r#""enum":[]"#,
            r#""enum":[""]"#,
            r#"{"type":"array"}"#,
            "$ref",
            "$defs",
        ] {
            assert!(!text.contains(banned), "{banned} leaked: {text}");
        }
    }

    /// Output assertion from spec 22 §6.4.1/§6.4.2: every `enum` is a non-empty
    /// array of non-empty strings, and every `type: array` node has an object
    /// `items` with a `type`.
    fn assert_legal(schema: &Value) {
        let Some(map) = schema.as_object() else {
            panic!("schema node is not an object: {schema}");
        };
        if let Some(enumeration) = map.get("enum") {
            let items = enumeration.as_array().expect("enum is an array");
            assert!(!items.is_empty(), "empty enum in {schema}");
            for item in items {
                let text = item.as_str().expect("enum item is a string");
                assert!(!text.is_empty(), "empty enum entry in {schema}");
            }
        }
        if map.get("type").and_then(Value::as_str) == Some("array") {
            let items = map
                .get("items")
                .and_then(Value::as_object)
                .expect("array without object items");
            assert!(
                items.get("type").and_then(Value::as_str).is_some(),
                "array items without type: {schema}"
            );
        }
        if let Some(properties) = map.get("properties").and_then(Value::as_object) {
            for sub in properties.values() {
                assert_legal(sub);
            }
        }
        if let Some(items) = map.get("items") {
            assert_legal(items);
        }
        if let Some(members) = map.get("anyOf").and_then(Value::as_array) {
            for member in members {
                assert_legal(member);
            }
        }
    }

    #[test]
    fn required_is_deduplicated_and_filtered() {
        assert_eq!(
            rendered(serde_json::json!({
                "type": "object",
                "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
                "required": ["a", "a", "b", "zzz", 3]
            })),
            r#"{"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}},"required":["a","b"]}"#
        );
    }

    #[test]
    fn ref_depth_cap_degrades() {
        let mut names = Vec::new();
        for index in 0..12 {
            names.push(format!("D{index}"));
        }
        let mut defs = serde_json::Map::new();
        for (index, name) in names.iter().enumerate() {
            let next = names.get(index + 1);
            let mut node = serde_json::Map::new();
            node.insert("type".to_string(), Value::String("object".to_string()));
            if let Some(next) = next {
                node.insert(
                    "properties".to_string(),
                    serde_json::json!({"next": {"$ref": format!("#/$defs/{next}")}}),
                );
            }
            defs.insert(name.clone(), Value::Object(node));
        }
        let mut root = serde_json::Map::new();
        root.insert("$ref".to_string(), Value::String("#/$defs/D0".to_string()));
        root.insert("$defs".to_string(), Value::Object(defs));
        let cleaned = clean_schema(&Value::Object(root));
        let text = cleaned.to_string();
        assert!(!text.contains("$ref"), "{text}");
        assert!(text.contains(r#""properties":{}"#), "{text}");
    }

    #[test]
    fn unresolvable_and_external_refs_degrade() {
        for reference in [
            "#/$defs/Missing",
            "https://example.com/schema.json",
            "other.json#/defs/X",
            "#/properties/a",
        ] {
            let cleaned = cleaned(serde_json::json!({"$ref": reference}));
            assert_eq!(
                cleaned,
                serde_json::json!({"type": "object", "properties": {}})
            );
        }
    }

    #[test]
    fn ref_siblings_win_and_defs_disappear() {
        let cleaned = cleaned(serde_json::json!({
            "$ref": "#/$defs/X",
            "description": "sibling",
            "$defs": {"X": {"type": "object", "description": "def", "properties": {"a": {"type": "string"}}}}
        }));
        assert_eq!(
            cleaned,
            serde_json::json!({"type": "object", "description": "sibling", "properties": {"a": {"type": "string"}}})
        );
    }

    #[test]
    fn cleaned_schema_only_contains_whitelisted_keys() {
        let schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "title": "T",
            "description": "d",
            "properties": {
                "a": {"type": ["string", "null"], "format": "uri", "default": "x", "enum": ["q"]},
                "b": {"oneOf": [{"type": "integer"}, {"type": "boolean", "deprecated": true}]},
                "c": {"type": "array", "items": {"type": "object", "additionalProperties": false,
                      "properties": {"d": {"$ref": "#/$defs/D"}}}},
                "e": {"type": "object", "propertyNames": {"pattern": "^a$"}}
            },
            "required": ["a"],
            "additionalProperties": {"type": "string"},
            "$defs": {"D": {"type": "string", "x-custom": 1}}
        });
        let cleaned = clean_schema(&schema);
        assert_allowed(&cleaned);
        let text = cleaned.to_string();
        for banned in [
            "$schema",
            "additionalProperties",
            "propertyNames",
            "$defs",
            "oneOf",
            "default",
            "x-custom",
            "format",
        ] {
            assert!(!text.contains(banned), "{banned} leaked: {text}");
        }
    }

    fn assert_allowed(schema: &Value) {
        let Some(map) = schema.as_object() else {
            panic!("schema node is not an object: {schema}");
        };
        for (key, value) in map {
            assert!(
                ALLOWED_KEYS.contains(&key.as_str()),
                "unknown key {key} in {schema}"
            );
            match key.as_str() {
                "properties" => {
                    // `properties` is a name map: its keys are client-chosen
                    // parameter names and are never filtered (spec 22 §6.4).
                    for (_name, sub) in value.as_object().expect("properties is an object") {
                        assert_allowed(sub);
                    }
                }
                "items" => assert_allowed(value),
                "anyOf" => {
                    for member in value.as_array().expect("anyOf is an array") {
                        assert_allowed(member);
                    }
                }
                _ => {}
            }
        }
    }
}
