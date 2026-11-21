//! TypeScript declarations for the script-visible API and JSON Schema
//! rendering.
//!
//! Ground truth: `packages/codemode/src/declarations.ts:1-351`. Every
//! exported helper of that file is ported: `renderDeclarations`,
//! `renderToolSignature`, `renderToolSample`, `renderToolOutputType`,
//! `schemaToType`, and `resolveSchemaRef`. Schemas are `serde_json::Value`
//! (upstream's `CodemodeJsonSchema`). Two deliberate micro-deviations:
//! malformed `%` escapes in a `$ref` resolve to `None` (upstream also names
//! nothing), and namespace declarations are emitted in first-seen order.

use std::collections::HashSet;
use std::fmt::Write as _;

use serde_json::Value as Json;

pub use crate::identifier::to_codemode_identifier;

/// Largest rendered input type, in characters, before it becomes `unknown`
/// (`declarations.ts:9-10`).
pub const DEFAULT_INPUT_SCHEMA_MAX_CHARS: usize = 16_000;

const INDENT: &str = "  ";
/// Local `$ref` expansions per rendered schema, so shared definitions cannot
/// blow up the output (`declarations.ts:12`).
const MAX_REF_EXPANSIONS: usize = 32;
const TAG_KEYWORDS: [&str; 11] = [
    "minimum",
    "exclusiveMinimum",
    "maximum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minItems",
    "maxItems",
];
const LOWER_BOUNDS: [&str; 4] = ["minimum", "exclusiveMinimum", "minLength", "minItems"];
const UPPER_BOUNDS: [&str; 4] = ["maximum", "exclusiveMaximum", "maxLength", "maxItems"];
const MAX_TAG_CHARS: usize = 80;

/// The subset of a tool a declaration needs (`declarations.ts:76-85`,
/// `declarations.ts:91-97`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderTool<'a> {
    /// Raw tool name (used to compute the script identifier).
    pub name: &'a str,
    /// Doc comment text.
    pub description: Option<&'a str>,
    /// Schema of the single argument.
    pub input_schema: Option<&'a Json>,
    /// Schema of the resolved value.
    pub output_schema: Option<&'a Json>,
    /// Globals only: an explicit TypeScript parameter list and return type.
    pub signature: Option<&'a str>,
}

impl<'a> RenderTool<'a> {
    /// Build a declaration from a tool-like value.
    #[must_use]
    pub fn new(name: &'a str) -> Self {
        Self {
            name,
            ..Self::default()
        }
    }

    /// Attach a description.
    #[must_use]
    pub fn description(mut self, description: &'a str) -> Self {
        self.description = Some(description);
        self
    }

    /// Attach an input schema.
    #[must_use]
    pub fn input_schema(mut self, schema: &'a Json) -> Self {
        self.input_schema = Some(schema);
        self
    }

    /// Attach an output schema.
    #[must_use]
    pub fn output_schema(mut self, schema: &'a Json) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// Attach an explicit signature (globals).
    #[must_use]
    pub fn signature(mut self, signature: &'a str) -> Self {
        self.signature = Some(signature);
        self
    }
}

/// Options of [`render_declarations`] (`declarations.ts:35-38`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderDeclarationsOptions<'a> {
    /// Tools become members of `declare const tools`.
    pub tools: &'a [RenderTool<'a>],
    /// Globals become `declare function` / `declare const namespace` blocks.
    pub globals: &'a [RenderTool<'a>],
}

/// Options shared by the single-tool renderers (`declarations.ts:76-79`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderToolOptions {
    /// Maximum rendered input type length. `None` uses
    /// [`DEFAULT_INPUT_SCHEMA_MAX_CHARS`].
    pub input_max_chars: Option<usize>,
}

/// Render TypeScript declarations for the script-visible API
/// (`declarations.ts:40-70`).
#[must_use]
pub fn render_declarations(options: &RenderDeclarationsOptions<'_>) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !options.tools.is_empty() {
        let members: Vec<String> = options
            .tools
            .iter()
            .map(|tool| {
                format!(
                    "{}{}{}",
                    doc_comment(tool.description, INDENT),
                    INDENT,
                    render_tool_signature(tool, &RenderToolOptions::default())
                )
            })
            .collect();
        sections.push(format!(
            "declare const tools: {{\n{}\n}};",
            members.join("\n")
        ));
    }

    // (namespace, rendered members), in first-seen order like TS's Map.
    let mut namespaces: Vec<(String, Vec<String>)> = Vec::new();
    for global in options.globals {
        if let Some(dot) = global.name.find('.') {
            let namespace = global.name[..dot].to_string();
            let member = render_global(&global.name[dot + 1..], global, INDENT);
            if let Some((_, members)) = namespaces.iter_mut().find(|(name, _)| name == &namespace) {
                members.push(member);
            } else {
                namespaces.push((namespace, vec![member]));
            }
        } else {
            let head = format!("declare function {}", global.name);
            sections.push(render_global(&head, global, ""));
        }
    }
    for (namespace, members) in namespaces {
        sections.push(format!(
            "declare const {namespace}: {{\n{}\n}};",
            members.join("\n")
        ));
    }
    sections.join("\n\n")
}

/// One tool as a member of the `tools` object (`declarations.ts:72-85`).
#[must_use]
pub fn render_tool_signature(tool: &RenderTool<'_>, options: &RenderToolOptions) -> String {
    let input = match tool.input_schema {
        None => "unknown".to_string(),
        Some(schema) => schema_to_type(
            schema,
            Some(
                options
                    .input_max_chars
                    .unwrap_or(DEFAULT_INPUT_SCHEMA_MAX_CHARS),
            ),
        ),
    };
    format!(
        "{}(args: {}): Promise<{}>;",
        to_codemode_identifier(tool.name),
        input,
        render_tool_output_type(tool.output_schema)
    )
}

/// A tool's description followed by its declaration (`declarations.ts:87-97`).
#[must_use]
pub fn render_tool_sample(tool: &RenderTool<'_>, options: &RenderToolOptions) -> String {
    let declaration = format!(
        "declare const tools: {{ {} }};",
        render_tool_signature(tool, options)
    );
    format!(
        "{}\n\ncodemode tool declaration:\n```ts\n{declaration}\n```",
        tool.description.unwrap_or("").trim()
    )
}

/// The type a tool call resolves to (`declarations.ts:99-102`).
#[must_use]
pub fn render_tool_output_type(schema: Option<&Json>) -> String {
    match schema {
        None => "unknown".to_string(),
        Some(schema) => schema_to_type(schema, None),
    }
}

fn render_global(head: &str, global: &RenderTool<'_>, indent: &str) -> String {
    if let Some(signature) = global.signature {
        return format!(
            "{}{}{head}{signature};",
            doc_comment(global.description, indent),
            indent
        );
    }
    let input = global.input_schema.map_or_else(
        || "unknown".to_string(),
        |schema| schema_to_type(schema, None),
    );
    let output = global.output_schema.map_or_else(
        || "unknown".to_string(),
        |schema| schema_to_type(schema, None),
    );
    format!(
        "{}{}{head}(args: {input}): Promise<{output}>;",
        doc_comment(global.description, indent),
        indent
    )
}

fn doc_comment(description: Option<&str>, indent: &str) -> String {
    let Some(text) = description.map(str::trim).filter(|text| !text.is_empty()) else {
        return String::new();
    };
    let escaped = text.replace("*/", "*\\/");
    let lines: Vec<&str> = escaped
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if lines.len() == 1 {
        return format!("{indent}/** {} */\n", lines[0]);
    }
    let body = lines
        .iter()
        .map(|line| {
            if line.is_empty() {
                format!("{indent} *")
            } else {
                format!("{indent} * {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{indent}/**\n{body}\n{indent} */\n")
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let first_valid = first.is_ascii_alphabetic() || first == '_' || first == '$';
    first_valid && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

fn property_key(name: &str) -> String {
    if is_identifier(name) {
        name.to_string()
    } else {
        serde_json::to_string(name).unwrap_or_else(|_| "\"\"".to_string())
    }
}

fn union(types: Vec<String>) -> String {
    let mut unique: Vec<String> = Vec::new();
    for item in types {
        if !unique.contains(&item) {
            unique.push(item);
        }
    }
    if unique.iter().any(|item| item == "unknown") {
        return "unknown".to_string();
    }
    if unique.is_empty() {
        "never".to_string()
    } else {
        unique.join(" | ")
    }
}

fn json_stringify(value: &Json) -> Option<String> {
    serde_json::to_string(value).ok()
}

/// Convert a JSON Schema to a TypeScript type expression
/// (`declarations.ts:131-145`).
#[must_use]
pub fn schema_to_type(schema: &Json, max_chars: Option<usize>) -> String {
    let render = |tags: bool| {
        to_type(
            schema,
            &mut SchemaContext {
                root: schema,
                resolving: HashSet::new(),
                expansions: 0,
                tags,
            },
        )
    };
    let tagged = render(true);
    match max_chars {
        None => tagged,
        Some(limit) if tagged.chars().count() <= limit => tagged,
        Some(limit) => {
            let untagged = render(false);
            if untagged.chars().count() <= limit {
                untagged
            } else {
                "unknown".to_string()
            }
        }
    }
}

struct SchemaContext<'a> {
    root: &'a Json,
    resolving: HashSet<String>,
    expansions: usize,
    tags: bool,
}

/// The subschema a local `$ref` names (`declarations.ts:156-181`).
#[must_use]
pub fn resolve_schema_ref(reference: &str, root: &Json) -> Option<Json> {
    if reference != "#" && !reference.starts_with("#/") {
        return None;
    }
    let path = if reference == "#" {
        ""
    } else {
        &reference[2..]
    };
    let mut current = root.clone();
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        let decoded = percent_decode(segment)?;
        let key = decoded.replace("~1", "/").replace("~0", "~");
        current = current.as_object()?.get(&key)?.clone();
    }
    if current.is_boolean() || current.is_object() {
        Some(current)
    } else {
        None
    }
}

fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes.get(index + 1).and_then(|byte| hex_value(*byte))?;
            let low = bytes.get(index + 2).and_then(|byte| hex_value(*byte))?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn as_object(schema: &Json) -> Option<&serde_json::Map<String, Json>> {
    schema.as_object()
}

fn string_field(schema: &serde_json::Map<String, Json>, key: &str) -> Option<&str> {
    schema.get(key).and_then(Json::as_str)
}

fn to_type(schema: &Json, context: &mut SchemaContext<'_>) -> String {
    if let Some(value) = schema.as_bool() {
        return if value { "unknown" } else { "never" }.to_string();
    }
    let Some(object) = as_object(schema) else {
        return "unknown".to_string();
    };
    if let Some(reference) = string_field(object, "$ref") {
        if context.resolving.contains(reference) || context.expansions >= MAX_REF_EXPANSIONS {
            return "unknown".to_string();
        }
        let Some(target) = resolve_schema_ref(reference, context.root) else {
            return "unknown".to_string();
        };
        context.expansions += 1;
        context.resolving.insert(reference.to_string());
        let rendered = to_type(&target, context);
        context.resolving.remove(reference);
        return rendered;
    }
    if let Some(value) = object.get("const") {
        return json_stringify(value).unwrap_or_else(|| "unknown".to_string());
    }
    if let Some(values) = object.get("enum").and_then(Json::as_array) {
        return union(
            values
                .iter()
                .map(|value| json_stringify(value).unwrap_or_else(|| "unknown".to_string()))
                .collect(),
        );
    }
    if let Some(variants) = object
        .get("anyOf")
        .and_then(Json::as_array)
        .or_else(|| object.get("oneOf").and_then(Json::as_array))
    {
        return union(
            variants
                .iter()
                .map(|variant| to_type(variant, context))
                .collect(),
        );
    }
    if let Some(parts) = object.get("allOf").and_then(Json::as_array) {
        let parts: Vec<String> = parts
            .iter()
            .map(|part| to_type(part, context))
            .filter(|part| part != "unknown")
            .collect();
        if parts.is_empty() {
            return "unknown".to_string();
        }
        return parts
            .iter()
            .map(|part| {
                if part.contains(" | ") {
                    format!("({part})")
                } else {
                    part.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" & ");
    }

    let types = object.get("type");
    if let Some(types) = types.and_then(Json::as_array) {
        return union(
            types
                .iter()
                .map(|entry| {
                    let mut merged = object.clone();
                    merged.insert("type".to_string(), entry.clone());
                    to_type(&Json::Object(merged), context)
                })
                .collect(),
        );
    }
    match types.and_then(Json::as_str) {
        Some("string") => "string".to_string(),
        Some("number" | "integer") => "number".to_string(),
        Some("boolean") => "boolean".to_string(),
        Some("null") => "null".to_string(),
        Some("array") => array_type(object, context),
        Some("object") => object_type(object, context),
        None if object.contains_key("properties")
            || object.contains_key("additionalProperties")
            || object.contains_key("required") =>
        {
            object_type(object, context)
        }
        None if object.contains_key("items") || object.contains_key("prefixItems") => {
            array_type(object, context)
        }
        _ => "unknown".to_string(),
    }
}

fn array_type(schema: &serde_json::Map<String, Json>, context: &mut SchemaContext<'_>) -> String {
    if let Some(items) = schema.get("items")
        && !items.is_array()
    {
        return format!("Array<{}>", to_type(items, context));
    }
    let tuple = schema
        .get("prefixItems")
        .and_then(Json::as_array)
        .or_else(|| schema.get("items").and_then(Json::as_array));
    if let Some(tuple) = tuple
        && !tuple.is_empty()
    {
        let items: Vec<String> = tuple.iter().map(|item| to_type(item, context)).collect();
        return format!("[{}]", items.join(", "));
    }
    "unknown[]".to_string()
}

fn description_of(property: &Json) -> String {
    property
        .as_object()
        .and_then(|object| string_field(object, "description"))
        .unwrap_or("")
        .trim()
        .to_string()
}

fn number_text(value: &Json) -> String {
    match value {
        Json::Number(number) => number.to_string(),
        Json::String(text) => text.clone(),
        _ => String::new(),
    }
}

/// Constraints a property's TypeScript type cannot express, as JSDoc-style
/// tags (`declarations.ts:260-312`).
fn constraint_tags(schema: &Json) -> String {
    let Some(property) = schema.as_object() else {
        return String::new();
    };
    let variants = property
        .get("anyOf")
        .and_then(Json::as_array)
        .or_else(|| property.get("oneOf").and_then(Json::as_array));
    let non_null: Vec<&Json> = variants
        .map(|values| {
            values
                .iter()
                .filter(|value| {
                    value.as_object().and_then(|o| string_field(o, "type")) != Some("null")
                })
                .collect()
        })
        .unwrap_or_default();
    let branch = if non_null.len() == 1 {
        non_null[0].as_object()
    } else {
        None
    };
    let branches: Vec<&serde_json::Map<String, Json>> = match branch {
        Some(branch) => vec![property, branch],
        None => vec![property],
    };
    let schemas: Vec<serde_json::Map<String, Json>> = branches
        .into_iter()
        .map(|branch| {
            let mut merged = branch.clone();
            if merged.get("exclusiveMinimum").and_then(Json::as_bool) == Some(true) {
                if let Some(minimum) = merged.get("minimum").cloned() {
                    merged.insert("exclusiveMinimum".to_string(), minimum);
                }
                merged.remove("minimum");
            }
            if merged.get("exclusiveMaximum").and_then(Json::as_bool) == Some(true) {
                if let Some(maximum) = merged.get("maximum").cloned() {
                    merged.insert("exclusiveMaximum".to_string(), maximum);
                }
                merged.remove("maximum");
            }
            merged
        })
        .collect();

    let mut tags: Vec<String> = Vec::new();
    let is_integer = |value: Option<&Json>| {
        value.is_some_and(|value| {
            value.as_str() == Some("integer")
                || value
                    .as_array()
                    .is_some_and(|values| values.iter().any(|v| v.as_str() == Some("integer")))
        })
    };
    if schemas.iter().any(|schema| is_integer(schema.get("type"))) {
        tags.push("@integer".to_string());
    }
    for keyword in TAG_KEYWORDS {
        let values: Vec<&Json> = schemas
            .iter()
            .filter_map(|schema| schema.get(keyword))
            .collect();
        if LOWER_BOUNDS.contains(&keyword) || UPPER_BOUNDS.contains(&keyword) {
            let numbers: Vec<f64> = values.iter().filter_map(|value| value.as_f64()).collect();
            if numbers.is_empty() {
                continue;
            }
            let bound = if LOWER_BOUNDS.contains(&keyword) {
                numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            } else {
                numbers.iter().copied().fold(f64::INFINITY, f64::min)
            };
            tags.push(format!("@{keyword} {bound}"));
            continue;
        }
        let mut seen: Vec<String> = Vec::new();
        for value in values {
            if value.is_number() || value.is_string() {
                let text = number_text(value);
                if !seen.contains(&text) {
                    seen.push(text);
                }
            }
        }
        for text in seen {
            tags.push(format!("@{keyword} {text}"));
        }
    }
    if schemas
        .iter()
        .any(|schema| schema.get("uniqueItems").and_then(Json::as_bool) == Some(true))
    {
        tags.push("@uniqueItems".to_string());
    }
    if let Some(default) = property.get("default")
        && !default.is_null()
    {
        tags.push(format!(
            "@default {}",
            json_stringify(default).unwrap_or_default()
        ));
    }
    if property.get("deprecated").and_then(Json::as_bool) == Some(true) {
        tags.push("@deprecated".to_string());
    }
    tags.into_iter()
        .filter(|tag| tag.chars().count() <= MAX_TAG_CHARS)
        .map(|tag| escape_line_terminators(&tag))
        .collect::<Vec<_>>()
        .join(" ")
}

fn escape_line_terminators(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\r' | '\n' | '\u{2028}' | '\u{2029}' => {
                write!(escaped, "\\u{:04x}", character as u32)
                    .expect("writing to a String cannot fail");
            }
            _ => escaped.push(character),
        }
    }
    escaped
}

fn object_type(schema: &serde_json::Map<String, Json>, context: &mut SchemaContext<'_>) -> String {
    let properties = schema
        .get("properties")
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();
    let required: HashSet<&str> = schema
        .get("required")
        .and_then(Json::as_array)
        .map(|values| values.iter().filter_map(Json::as_str).collect())
        .unwrap_or_default();
    let mut names: Vec<&String> = properties.keys().collect();
    names.sort();

    let mut members: Vec<String> = names
        .iter()
        .map(|name| {
            let optional = if required.contains(name.as_str()) {
                ""
            } else {
                "?"
            };
            let rendered = properties
                .get(*name)
                .map(|property| to_type(property, context))
                .unwrap_or_else(|| "unknown".to_string());
            format!("{}{optional}: {rendered};", property_key(name))
        })
        .collect();
    let additional = schema.get("additionalProperties");
    match additional {
        Some(Json::Bool(false)) => {}
        Some(Json::Bool(true)) => members.push("[key: string]: unknown;".to_string()),
        Some(value) => {
            let rendered = to_type(value, context);
            members.push(format!("[key: string]: {rendered};"));
        }
        None if names.is_empty() => members.push("[key: string]: unknown;".to_string()),
        None => {}
    }
    if members.is_empty() {
        return "{}".to_string();
    }

    let comments: Vec<Vec<String>> = names
        .iter()
        .map(|name| {
            let property = properties.get(*name).map_or(Json::Null, Clone::clone);
            let mut lines: Vec<String> = description_of(&property)
                .split('\n')
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect();
            if context.tags {
                let tags = constraint_tags(&property);
                if !tags.is_empty() {
                    lines.push(tags);
                }
            }
            lines
        })
        .collect();
    if comments.iter().all(Vec::is_empty) {
        return format!("{{ {} }}", members.join(" "));
    }

    let mut lines = vec!["{".to_string()];
    for (index, comment) in comments.iter().enumerate() {
        for line in comment {
            lines.push(format!("{INDENT}// {line}"));
        }
        let indented = members[index].replace('\n', &format!("\n{INDENT}"));
        lines.push(format!("{INDENT}{indented}"));
    }
    for member in members.iter().skip(names.len()) {
        lines.push(format!("{INDENT}{member}"));
    }
    lines.push("}".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identifier_normalizes_tool_names() {
        assert_eq!(to_codemode_identifier("my-tool"), "my_tool");
        assert_eq!(to_codemode_identifier("read"), "read");
    }

    #[test]
    fn renders_a_tool_signature_from_schemas() {
        let tool = RenderTool::new("my-tool")
            .input_schema(&json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}))
            .output_schema(&json!({"type": "string"}));
        assert_eq!(
            render_tool_signature(&tool, &RenderToolOptions::default()),
            "my_tool(args: { path: string; }): Promise<string>;"
        );
    }

    #[test]
    fn missing_schemas_render_as_unknown() {
        let tool = RenderTool::new("read");
        assert_eq!(render_tool_output_type(None), "unknown");
        assert_eq!(
            render_tool_signature(&tool, &RenderToolOptions::default()),
            "read(args: unknown): Promise<unknown>;"
        );
    }

    #[test]
    fn renders_declarations_with_tools_and_namespaces() {
        let read = RenderTool::new("read").description("Read a file");
        let models =
            RenderTool::new("models.classify").signature("(prompt: string): Promise<string>");
        let options = RenderDeclarationsOptions {
            tools: &[read],
            globals: &[models],
        };
        assert_eq!(
            render_declarations(&options),
            "declare const tools: {\n  /** Read a file */\n  read(args: unknown): Promise<unknown>;\n};\n\ndeclare const models: {\n  classify(prompt: string): Promise<string>;\n};"
        );
    }

    #[test]
    fn doc_comments_escape_close_markers_and_multiline() {
        let tool = RenderTool::new("x").description("Uses */ inline\nsecond line");
        assert_eq!(
            render_tool_signature(&tool, &RenderToolOptions::default()),
            "/**\n * Uses *\\/ inline\n * second line\n */\nx(args: unknown): Promise<unknown>;"
        );
    }

    #[test]
    fn schema_to_type_handles_primitives_and_unions() {
        assert_eq!(schema_to_type(&json!(true), None), "unknown");
        assert_eq!(schema_to_type(&json!(false), None), "never");
        assert_eq!(schema_to_type(&json!({"type": "integer"}), None), "number");
        assert_eq!(
            schema_to_type(&json!({"type": ["string", "null"]}), None),
            "string | null"
        );
        assert_eq!(
            schema_to_type(&json!({"enum": ["a", "b", "a"]}), None),
            "\"a\" | \"b\""
        );
        assert_eq!(schema_to_type(&json!({"const": 3}), None), "3");
        assert_eq!(
            schema_to_type(&json!({"type": "array", "items": {"type": "number"}}), None),
            "Array<number>"
        );
        assert_eq!(
            schema_to_type(
                &json!({"type": "array", "prefixItems": [{"type": "string"}, {"type": "number"}]}),
                None
            ),
            "[string, number]"
        );
    }

    #[test]
    fn schema_to_type_resolves_local_refs_and_stops_on_cycles() {
        let schema = json!({
            "$defs": {"Item": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}},
            "$ref": "#/$defs/Item"
        });
        assert_eq!(schema_to_type(&schema, None), "{ id: string; }");
        let cyclic = json!({"$ref": "#", "type": "object"});
        assert_eq!(schema_to_type(&cyclic, None), "unknown");
        assert_eq!(resolve_schema_ref("#/missing", &json!({})), None);
        assert_eq!(resolve_schema_ref("https://example.com", &json!({})), None);
    }

    #[test]
    fn constraints_are_rendered_as_property_comments() {
        let schema = json!({
            "type": "object",
            "properties": {
                "count": {"type": "integer", "minimum": 1, "maximum": 10, "default": 2},
                "name": {"type": "string", "description": "The name"}
            },
            "required": ["count"]
        });
        assert_eq!(
            schema_to_type(&schema, None),
            "{\n  // @integer @minimum 1 @maximum 10 @default 2\n  count: number;\n  // The name\n  name?: string;\n}"
        );
    }

    #[test]
    fn max_chars_collapses_large_types_to_unknown() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "string", "description": "constraint @pattern ^[a-z]+$"}}});
        assert_eq!(schema_to_type(&schema, Some(5)), "unknown");
        assert_ne!(schema_to_type(&schema, Some(10_000)), "unknown");
    }

    #[test]
    fn render_sample_includes_the_declaration() {
        let declaration = RenderTool::new("read").description(" Read a file ");
        let sample = render_tool_sample(&declaration, &RenderToolOptions::default());
        assert!(sample.starts_with("Read a file\n\ncodemode tool declaration:\n```ts\n"));
        assert!(sample.contains("read(args: unknown): Promise<unknown>;"));
    }
}
