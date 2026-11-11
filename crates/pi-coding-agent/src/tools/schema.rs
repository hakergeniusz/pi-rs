//! Tool-argument validation and coercion.
//!
//! Ground truth: pi-mono `packages/ai/src/utils/validation.ts`
//! (`validateToolArguments`) and `packages/agent/src/agent-loop.ts`
//! (`prepareToolCallArguments`).
//!
//! The order matters and is pi's:
//!
//! 1. `prepareArguments` runs first and only the tool that declares one has it
//!    (`edit` accepts a JSON string, a single object, and the legacy
//!    top-level `oldText`/`newText`).
//! 2. `normalizeOptionalNulls` drops an explicit `null` on an optional
//!    property, which is how models say "not set".
//! 3. Type coercion turns `"5"` into `5`, `1` into `true`, `null` into `""`
//!    for a string, and recurses through objects and arrays.
//! 4. The coerced value is checked against the schema. A failure returns an
//!    error with pi's message envelope; the loop turns it into an error tool
//!    result.
//!
//! Only the JSON Schema keywords the builtin schemas actually use are
//! implemented: `type`, `properties`, `required`, `items`,
//! `additionalProperties`, `allOf`, `anyOf`, `oneOf`. Nothing else appears in
//! a tool schema.

use serde_json::{Map, Value};

/// A tool's `prepareArguments`, if it declares one.
pub type PrepareArguments = fn(Value) -> Value;

/// pi's `validateToolArguments`: clone, normalize, coerce, check.
pub fn validate_tool_arguments(
    tool_name: &str,
    schema: &Value,
    arguments: &Value,
    prepare: Option<PrepareArguments>,
) -> Result<Value, String> {
    let prepared = match prepare {
        Some(prepare) => prepare(arguments.clone()),
        None => arguments.clone(),
    };
    let mut value = prepared.clone();
    normalize_optional_nulls(&mut value, schema);
    coerce_with_json_schema(&mut value, schema);

    let errors = check(&value, schema);
    if errors.is_empty() {
        return Ok(value);
    }
    let listed = errors
        .iter()
        .map(|error| format!("  - {}: {}", error.path, error.message))
        .collect::<Vec<_>>()
        .join("\n");
    let listed = if listed.is_empty() {
        "Unknown validation error".to_string()
    } else {
        listed
    };
    let received =
        serde_json::to_string_pretty(&prepared).unwrap_or_else(|_| arguments.to_string());
    Err(format!(
        "Validation failed for tool \"{tool_name}\":\n{listed}\n\nReceived arguments:\n{received}"
    ))
}

/// One schema violation, already localized the way pi localizes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// Dotted path of the offending value (`root` for the value itself).
    pub path: String,
    /// typebox-style message.
    pub message: String,
}

/// pi's `check`: collect every violation of the keywords in play.
fn check(value: &Value, schema: &Value) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    check_into(value, schema, "", &mut errors);
    errors
}

fn check_into(value: &Value, schema: &Value, path: &str, errors: &mut Vec<ValidationError>) {
    let Some(schema) = schema.as_object() else {
        return;
    };

    for nested in schema
        .get("allOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        check_into(value, nested, path, errors);
    }
    if let Some(union) = union_members(schema) {
        if !union.iter().any(|member| check(value, member).is_empty()) {
            errors.push(ValidationError {
                path: display_path(path),
                message: "Expected value to match at least one schema in anyOf/oneOf".to_string(),
            });
            return;
        }
    }

    let types = schema_types(schema);
    if !types.is_empty()
        && !types
            .iter()
            .any(|schema_type| matches_json_type(value, schema_type))
    {
        errors.push(ValidationError {
            path: display_path(path),
            message: expected_message(&types),
        });
        return;
    }

    if value.is_object() {
        let object = value.as_object().expect("checked above");
        let properties = schema.get("properties").and_then(Value::as_object);
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        for key in &required {
            if !object.contains_key(*key) {
                errors.push(ValidationError {
                    path: join_path(path, key),
                    message: format!("Expected required property '{key}'"),
                });
            }
        }
        let additional = schema.get("additionalProperties");
        for (key, entry) in object {
            match properties.and_then(|properties| properties.get(key)) {
                Some(property_schema) => {
                    check_into(entry, property_schema, &join_path(path, key), errors);
                }
                None => match additional {
                    Some(Value::Bool(false)) => errors.push(ValidationError {
                        path: join_path(path, key),
                        message: format!("Expected no additional properties but got '{key}'"),
                    }),
                    Some(Value::Object(extra)) => {
                        let extra = Value::Object(extra.clone());
                        check_into(entry, &extra, &join_path(path, key), errors);
                    }
                    _ => {}
                },
            }
        }
    }

    if value.is_array() {
        if let Some(items) = schema.get("items") {
            if let Some(entries) = value.as_array() {
                if items.is_array() {
                    for (index, entry) in entries.iter().enumerate() {
                        if let Some(item_schema) =
                            items.as_array().and_then(|items| items.get(index))
                        {
                            check_into(
                                entry,
                                item_schema,
                                &join_path(path, &index.to_string()),
                                errors,
                            );
                        }
                    }
                } else {
                    for (index, entry) in entries.iter().enumerate() {
                        check_into(entry, items, &join_path(path, &index.to_string()), errors);
                    }
                }
            }
        }
    }
}

fn union_members(schema: &Map<String, Value>) -> Option<Vec<Value>> {
    for keyword in ["anyOf", "oneOf"] {
        if let Some(members) = schema.get(keyword).and_then(Value::as_array) {
            return Some(members.clone());
        }
    }
    None
}

fn schema_types(schema: &Map<String, Value>) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(name)) => vec![name.as_str()],
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn expected_message(types: &[&str]) -> String {
    let label = match types {
        [single] => (*single).to_string(),
        many => many.join(" | "),
    };
    match label.as_str() {
        "integer" => "Expected integer".to_string(),
        other => format!("Expected {other}"),
    }
}

fn display_path(path: &str) -> String {
    if path.is_empty() {
        "root".to_string()
    } else {
        path.to_string()
    }
}

fn join_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// pi's `matchesJsonType`.
fn matches_json_type(value: &Value, schema_type: &str) -> bool {
    match schema_type {
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

/// pi's `normalizeOptionalNulls`: an explicit `null` on an optional,
/// non-nullable property means "absent".
fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    if let Some(entries) = value.as_array_mut() {
        match schema.get("items") {
            Some(items) if items.is_object() => {
                for entry in entries.iter_mut() {
                    normalize_optional_nulls(entry, items);
                }
            }
            Some(Value::Array(items)) => {
                for (index, entry) in entries.iter_mut().enumerate() {
                    if let Some(item_schema) = items.get(index) {
                        normalize_optional_nulls(entry, item_schema);
                    }
                }
            }
            _ => {}
        }
        return;
    }
    let Some(schema) = schema.as_object() else {
        return;
    };
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    for (key, property_schema) in properties {
        let Some(entry) = object.get_mut(key) else {
            continue;
        };
        let nullable =
            schema_types(property_schema.as_object().unwrap_or(&Map::new())).contains(&"null");
        if entry.is_null() && !required.contains(&key.as_str()) && !nullable {
            object.remove(key);
            continue;
        }
        normalize_optional_nulls(entry, property_schema);
    }
}

/// pi's `coerceWithJsonSchema`.
fn coerce_with_json_schema(value: &mut Value, schema: &Value) {
    let Some(schema) = schema.as_object() else {
        return;
    };

    if let Some(members) = schema.get("allOf").and_then(Value::as_array) {
        for nested in members {
            coerce_with_json_schema(value, nested);
        }
    }
    if let Some(members) = union_members(schema) {
        coerce_union(value, &members);
    }

    let types = schema_types(schema);
    let matches_union_member = types.len() > 1
        && types
            .iter()
            .any(|schema_type| matches_json_type(value, schema_type));
    if !types.is_empty() && !matches_union_member {
        for schema_type in &types {
            let candidate = coerce_primitive_by_type(value, schema_type);
            if candidate != *value {
                *value = candidate;
                break;
            }
        }
    }

    if types.contains(&"object") {
        coerce_object(value, schema);
    }
    if types.contains(&"array") {
        coerce_array(value, schema);
    }
}

fn coerce_union(value: &mut Value, members: &[Value]) {
    if members.iter().any(|member| check(value, member).is_empty()) {
        return;
    }
    for member in members {
        let mut candidate = value.clone();
        coerce_with_json_schema(&mut candidate, member);
        if check(&candidate, member).is_empty() {
            *value = candidate;
            return;
        }
    }
}

fn coerce_object(value: &mut Value, schema: &Map<String, Value>) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let properties = schema.get("properties").and_then(Value::as_object).cloned();
    let defined_keys: Vec<String> = properties
        .as_ref()
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();

    if let Some(properties) = properties {
        for (key, property_schema) in properties {
            if let Some(entry) = object.get_mut(&key) {
                coerce_with_json_schema(entry, &property_schema);
            }
        }
    }

    if let Some(Value::Object(additional)) = schema.get("additionalProperties") {
        let additional = Value::Object(additional.clone());
        for (key, entry) in object.iter_mut() {
            if defined_keys.contains(key) {
                continue;
            }
            coerce_with_json_schema(entry, &additional);
        }
    }
}

fn coerce_array(value: &mut Value, schema: &Map<String, Value>) {
    let Some(entries) = value.as_array_mut() else {
        return;
    };
    match schema.get("items") {
        Some(Value::Array(items)) => {
            for (index, entry) in entries.iter_mut().enumerate() {
                if let Some(item_schema) = items.get(index) {
                    coerce_with_json_schema(entry, item_schema);
                }
            }
        }
        Some(item_schema @ Value::Object(_)) => {
            for entry in entries.iter_mut() {
                coerce_with_json_schema(entry, item_schema);
            }
        }
        _ => {}
    }
}

/// pi's `coercePrimitiveByType`, in Rust's value model.
fn coerce_primitive_by_type(value: &Value, schema_type: &str) -> Value {
    match schema_type {
        "number" | "integer" => match value {
            Value::Null => Value::from(0),
            Value::String(text) => match text.trim().parse::<f64>() {
                // pi leaves a non-integral parse alone so validation reports it.
                Ok(number)
                    if number.is_finite()
                        && (schema_type != "integer" || number.fract() == 0.0) =>
                {
                    if schema_type == "integer" {
                        Value::from(number as i64)
                    } else {
                        serde_json::Number::from_f64(number)
                            .map_or_else(|| value.clone(), Value::Number)
                    }
                }
                _ => value.clone(),
            },
            Value::Bool(flag) => Value::from(i64::from(*flag)),
            other => other.clone(),
        },
        "boolean" => match value {
            Value::Null => Value::from(false),
            Value::String(text) => match text.as_str() {
                "true" => Value::from(true),
                "false" => Value::from(false),
                _ => value.clone(),
            },
            Value::Number(number) => match number.as_i64() {
                Some(1) => Value::from(true),
                Some(0) => Value::from(false),
                _ => value.clone(),
            },
            other => other.clone(),
        },
        "string" => match value {
            Value::Null => Value::from(""),
            Value::String(_) => value.clone(),
            Value::Bool(_) | Value::Number(_) => Value::from(value.to_string()),
            other => other.clone(),
        },
        "null" => match value {
            Value::String(text) if text.is_empty() => Value::Null,
            Value::Number(number) if number.as_i64() == Some(0) => Value::Null,
            Value::Bool(false) => Value::Null,
            other => other.clone(),
        },
        _ => value.clone(),
    }
}
