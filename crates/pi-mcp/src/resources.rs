//! MCP resources: the protocol layer behind the `list_mcp_resources`,
//! `list_mcp_resource_templates`, and `read_mcp_resource` tools.
//!
//! Upstream ground truth (all paths under `packages/` of `earendil-works/pi`
//! v1.5.2):
//!
//! * `coding-agent/src/extensions/mcp/resources.ts:1-344` — the resource
//!   tools: MCP-App filtering (`:47-57`), JSON output schemas (`:59-166`),
//!   argument parsing (`:168-177`), the JSON result wrapper (`:179-193`),
//!   server lookup (`:204-212`), the list-one-page / all-pages helper
//!   (`:214-255`), and the three tool definitions (`:257-343`).
//! * `mcp/src/protocol/types.ts:93-131` — [`Resource`], [`ResourceTemplate`],
//!   [`ListResourcesResult`], [`ListResourceTemplatesResult`], and
//!   [`ReadResourceResult`].
//! * `mcp/src/protocol/content.ts:42-54` — [`TextResourceContents`] and
//!   [`BlobResourceContents`].
//! * `mcp/src/client.ts:89-142, 300-376` — `invalid`, `validateListPage`,
//!   the `isResource` / `isResourceTemplate` guards, `toResource`,
//!   `toResourceTemplate`, `pageCursor`, `validateReadResourceResult`, the
//!   four list methods, `readResource`, and `listAll` (`MAX_LIST_PAGES`).
//! * `coding-agent/src/extensions/mcp/tools.ts:159-212` — `extensionOf`,
//!   `isTextMimeType`, and `toModelContent`'s resource arms.
//! * `coding-agent/src/core/mcp-servers.ts:37-39` — the tool-name constants.
//! * `mcp/src/protocol/jsonrpc.ts:39, 45-50, 81-83` — the invalid-request
//!   code and `McpError`'s raw `message`.
//!
//! Subscriptions (`resources/subscribe` / `unsubscribe`) are not part of
//! upstream 1.5.2, so there is nothing to port here.
//!
//! Transport boundary: JSON-RPC calls go through [`ResourceTransport`]. The
//! crate's `McpClient` keeps its `request` method private and async, so the
//! coordinator bridges it in `client.rs`/`mcp_manager.rs` (see `WIRING.md`).
//! A closure `Fn(&str, Option<Value>) -> Result<Value, McpError>` implements
//! the trait, which keeps every flow below testable without a server.

use std::collections::HashSet;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use url::Url;

use crate::error::McpError;
use crate::types::json_rpc_error_codes::INVALID_REQUEST;

/// `LIST_MCP_RESOURCES_TOOL` (`core/mcp-servers.ts:37`).
pub const LIST_MCP_RESOURCES_TOOL: &str = "list_mcp_resources";
/// `LIST_MCP_RESOURCE_TEMPLATES_TOOL` (`core/mcp-servers.ts:38`).
pub const LIST_MCP_RESOURCE_TEMPLATES_TOOL: &str = "list_mcp_resource_templates";
/// `READ_MCP_RESOURCE_TOOL` (`core/mcp-servers.ts:39`).
pub const READ_MCP_RESOURCE_TOOL: &str = "read_mcp_resource";

/// `MAX_LIST_PAGES` (`mcp/src/client.ts:39`).
pub const MAX_LIST_PAGES: usize = 1_000;

/// `listResources` tool description (`resources.ts:260-261`).
pub const LIST_RESOURCES_DESCRIPTION: &str = "Lists resources provided by MCP servers. Resources allow servers to share data that provides context to language models, such as files, database schemas, or application-specific information. Prefer resources over web search when possible.";
/// `listMcpResourceTemplates` tool description (`resources.ts:284-285`).
pub const LIST_TEMPLATES_DESCRIPTION: &str = "Lists resource templates provided by MCP servers. Parameterized resource templates allow servers to share data that takes parameters and provides context to language models, such as files, database schemas, or application-specific information. Prefer resource templates over web search when possible.";
/// `readMcpResource` tool description (`resources.ts:308`).
pub const READ_DESCRIPTION: &str =
    "Read a specific resource from an MCP server given the server name and resource URI.";

/// A resource a server lists in `resources/list` (`protocol/types.ts:93-103`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Resource {
    pub uri: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// A family of resources addressed by an RFC 6570 URI template
/// (`protocol/types.ts:105-114`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResourceTemplate {
    #[serde(rename = "uriTemplate")]
    pub uri_template: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// Text contents of a resource (`protocol/content.ts:42-47`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextResourceContents {
    pub uri: String,
    #[serde(default, rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    pub text: String,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// Base64 blob contents of a resource (`protocol/content.ts:49-54`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlobResourceContents {
    pub uri: String,
    #[serde(default, rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    pub blob: String,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// One entry in a `resources/read` result. Text is tried first so a server
/// that (against spec) sends both `text` and `blob` is read as text, like
/// upstream's `"text" in resource` check (`content.ts:91`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ResourceContents {
    Text(TextResourceContents),
    Blob(BlobResourceContents),
}

/// `resources/list` result (`protocol/types.ts:116-120`). Upstream's page
/// helpers drop result-level `_meta`, so `meta` is always `None` here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ListResourcesResult {
    #[serde(default)]
    pub resources: Vec<Resource>,
    #[serde(
        default,
        rename = "nextCursor",
        skip_serializing_if = "Option::is_none"
    )]
    pub next_cursor: Option<String>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// `resources/templates/list` result (`protocol/types.ts:122-126`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ListResourceTemplatesResult {
    #[serde(default, rename = "resourceTemplates")]
    pub resource_templates: Vec<ResourceTemplate>,
    #[serde(
        default,
        rename = "nextCursor",
        skip_serializing_if = "Option::is_none"
    )]
    pub next_cursor: Option<String>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// `resources/read` result (`protocol/types.ts:128-131`). Unlike the list
/// helpers, upstream returns the validated value as-is, keeping `_meta`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReadResourceResult {
    #[serde(default)]
    pub contents: Vec<ResourceContents>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// The single JSON-RPC call the resource layer needs. Implemented by the
/// coordinator over `McpClient::request`; closures implement it directly for
/// tests. See `WIRING.md`.
pub trait ResourceTransport {
    /// Send `method` with optional `params` and return the raw `result`.
    ///
    /// # Errors
    /// Transport and JSON-RPC errors, propagated verbatim.
    fn request(&self, method: &str, params: Option<Value>) -> Result<Value, McpError>;
}

impl<F> ResourceTransport for F
where
    F: Fn(&str, Option<Value>) -> Result<Value, McpError>,
{
    fn request(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        self(method, params)
    }
}

/// One validated page of a paginated list (`client.ts:93-109`'s return).
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub items: Vec<Value>,
    pub next_cursor: Option<String>,
}

/// `isObject` (`jsonrpc.ts:81-83`): an object, not an array or null.
fn is_object(value: &Value) -> bool {
    value.is_object()
}

/// `invalid` (`client.ts:89-91`): an `McpError` with the invalid-request code
/// and the exact upstream message.
fn invalid(message: impl Into<String>) -> McpError {
    McpError::Rpc {
        code: INVALID_REQUEST,
        message: message.into(),
    }
}

/// Upstream `errorMessage` (`resources.ts:175-177`, `jsonrpc.ts:85-87`):
/// an `Error`'s `.message`, not its rendered `Error: …` form. Every error
/// this module creates carries its verbatim upstream string in one of these
/// two variants.
pub fn error_message(error: &McpError) -> String {
    match error {
        McpError::Rpc { message, .. }
        | McpError::Malformed(message)
        | McpError::Handshake(message)
        | McpError::Network(message) => message.clone(),
        other => other.to_string(),
    }
}

/// `isResource` (`client.ts:113-114`).
pub fn is_resource(item: &Value) -> bool {
    item.as_object().is_some_and(|map| {
        map.get("uri").is_some_and(Value::is_string) && map.get("name").is_none_or(Value::is_string)
    })
}

/// `isResourceTemplate` (`client.ts:115-116`).
pub fn is_resource_template(item: &Value) -> bool {
    item.as_object().is_some_and(|map| {
        map.get("uriTemplate").is_some_and(Value::is_string)
            && map.get("name").is_none_or(Value::is_string)
    })
}

/// `validateListPage` (`client.ts:93-109`), including the `null`/`""` cursor
/// normalization and the exact three error strings.
pub fn validate_list_page(
    method: &str,
    key: &str,
    value: &Value,
    is_item: fn(&Value) -> bool,
) -> Result<Page, McpError> {
    let Some(object) = value.as_object() else {
        return Err(invalid(format!("Invalid MCP {method} result")));
    };
    let Some(items) = object.get(key).and_then(Value::as_array) else {
        return Err(invalid(format!("Invalid MCP {method} result")));
    };
    for item in items {
        if !is_object(item) || !is_item(item) {
            return Err(invalid(format!("Invalid entry in MCP {method} result")));
        }
    }
    let raw_cursor = object.get("nextCursor");
    if raw_cursor.is_some_and(|cursor| cursor.is_null() || cursor.as_str() == Some("")) {
        return Ok(Page {
            items: items.clone(),
            next_cursor: None,
        });
    }
    let next_cursor = match raw_cursor {
        None => None,
        Some(Value::String(cursor)) => Some(cursor.clone()),
        Some(_) => return Err(invalid(format!("Invalid MCP {method} cursor"))),
    };
    Ok(Page {
        items: items.clone(),
        next_cursor,
    })
}

/// `toResource` (`client.ts:118-120`): fill a missing `name` from `uri`.
pub fn to_resource(item: &Value) -> Result<Resource, McpError> {
    let mut item = item.clone();
    if let Some(map) = item.as_object_mut()
        && map.get("name").is_none()
        && let Some(uri) = map.get("uri").and_then(Value::as_str)
    {
        map.insert("name".to_string(), Value::String(uri.to_string()));
    }
    serde_json::from_value(item).map_err(|error| invalid(format!("Invalid resource: {error}")))
}

/// `toResourceTemplate` (`client.ts:122-124`): fill a missing `name` from
/// `uriTemplate`.
pub fn to_resource_template(item: &Value) -> Result<ResourceTemplate, McpError> {
    let mut item = item.clone();
    if let Some(map) = item.as_object_mut()
        && map.get("name").is_none()
        && let Some(template) = map.get("uriTemplate").and_then(Value::as_str)
    {
        map.insert("name".to_string(), Value::String(template.to_string()));
    }
    serde_json::from_value(item)
        .map_err(|error| invalid(format!("Invalid resource template: {error}")))
}

/// `validateReadResourceResult` (`client.ts:130-142`) plus the typed
/// conversion. The two error strings are verbatim.
pub fn parse_read_resource_result(value: &Value) -> Result<ReadResourceResult, McpError> {
    let Some(object) = value.as_object() else {
        return Err(invalid("Invalid MCP resources/read result"));
    };
    let Some(items) = object.get("contents").and_then(Value::as_array) else {
        return Err(invalid("Invalid MCP resources/read result"));
    };
    for item in items {
        let valid = item.as_object().is_some_and(|map| {
            map.get("uri").is_some_and(Value::is_string)
                && (map.get("text").is_some_and(Value::is_string)
                    || map.get("blob").is_some_and(Value::is_string))
        });
        if !valid {
            return Err(invalid("Invalid contents in MCP resources/read result"));
        }
    }
    let contents = items
        .iter()
        .map(|item| {
            serde_json::from_value::<ResourceContents>(item.clone()).map_err(|error| {
                invalid(format!(
                    "Invalid contents in MCP resources/read result: {error}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ReadResourceResult {
        contents,
        meta: object.get("_meta").cloned(),
    })
}

/// Parse one `resources/list` page result.
pub fn parse_list_resources_page(value: &Value) -> Result<ListResourcesResult, McpError> {
    let page = validate_list_page("resources/list", "resources", value, is_resource)?;
    Ok(ListResourcesResult {
        resources: page
            .items
            .iter()
            .map(to_resource)
            .collect::<Result<Vec<_>, _>>()?,
        next_cursor: page.next_cursor,
        meta: None,
    })
}

/// Parse one `resources/templates/list` page result.
pub fn parse_list_resource_templates_page(
    value: &Value,
) -> Result<ListResourceTemplatesResult, McpError> {
    let page = validate_list_page(
        "resources/templates/list",
        "resourceTemplates",
        value,
        is_resource_template,
    )?;
    Ok(ListResourceTemplatesResult {
        resource_templates: page
            .items
            .iter()
            .map(to_resource_template)
            .collect::<Result<Vec<_>, _>>()?,
        next_cursor: page.next_cursor,
        meta: None,
    })
}

/// `listAll` (`client.ts:357-376`): follow `nextCursor` through at most
/// [`MAX_LIST_PAGES`] pages, rejecting repeated cursors. The duplicate and
/// exceeded messages are plain `Error`s upstream; `error_message()` returns
/// them verbatim from the [`McpError::Malformed`] payload.
pub fn list_all<F>(method: &str, mut page: F) -> Result<Vec<Value>, McpError>
where
    F: FnMut(Option<&str>) -> Result<Page, McpError>,
{
    let mut items = Vec::new();
    let mut cursors = HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_LIST_PAGES {
        let result = page(cursor.as_deref())?;
        items.extend(result.items);
        let Some(next) = result.next_cursor else {
            return Ok(items);
        };
        if !cursors.insert(next.clone()) {
            return Err(McpError::Malformed(format!(
                "MCP {method} returned duplicate cursor: {next}"
            )));
        }
        cursor = Some(next);
    }
    Err(McpError::Malformed(format!(
        "MCP {method} exceeded {MAX_LIST_PAGES} pages"
    )))
}

fn page_request<T: ResourceTransport>(
    transport: &T,
    method: &str,
    key: &str,
    cursor: Option<&str>,
    is_item: fn(&Value) -> bool,
) -> Result<Page, McpError> {
    let params = cursor.map(|cursor| json!({ "cursor": cursor }));
    let value = transport.request(method, params)?;
    validate_list_page(method, key, &value, is_item)
}

/// `listResourcesPage` (`client.ts:306-309`): one page of resources.
pub fn list_resources_page<T: ResourceTransport>(
    transport: &T,
    cursor: Option<&str>,
) -> Result<ListResourcesResult, McpError> {
    let page = page_request(
        transport,
        "resources/list",
        "resources",
        cursor,
        is_resource,
    )?;
    Ok(ListResourcesResult {
        resources: page
            .items
            .iter()
            .map(to_resource)
            .collect::<Result<Vec<_>, _>>()?,
        next_cursor: page.next_cursor,
        meta: None,
    })
}

/// `listResources` (`client.ts:301-303`): every resource, all pages.
pub fn list_resources<T: ResourceTransport>(transport: &T) -> Result<Vec<Resource>, McpError> {
    let items = list_all("resources/list", |cursor| {
        page_request(
            transport,
            "resources/list",
            "resources",
            cursor,
            is_resource,
        )
    })?;
    items.iter().map(to_resource).collect()
}

/// `listResourceTemplatesPage` (`client.ts:323-335`): one template page.
pub fn list_resource_templates_page<T: ResourceTransport>(
    transport: &T,
    cursor: Option<&str>,
) -> Result<ListResourceTemplatesResult, McpError> {
    let page = page_request(
        transport,
        "resources/templates/list",
        "resourceTemplates",
        cursor,
        is_resource_template,
    )?;
    Ok(ListResourceTemplatesResult {
        resource_templates: page
            .items
            .iter()
            .map(to_resource_template)
            .collect::<Result<Vec<_>, _>>()?,
        next_cursor: page.next_cursor,
        meta: None,
    })
}

/// `listResourceTemplates` (`client.ts:312-320`): every template, all pages.
pub fn list_resource_templates<T: ResourceTransport>(
    transport: &T,
) -> Result<Vec<ResourceTemplate>, McpError> {
    let items = list_all("resources/templates/list", |cursor| {
        page_request(
            transport,
            "resources/templates/list",
            "resourceTemplates",
            cursor,
            is_resource_template,
        )
    })?;
    items.iter().map(to_resource_template).collect()
}

/// `readResource` (`client.ts:337-339`).
pub fn read_resource<T: ResourceTransport>(
    transport: &T,
    uri: &str,
) -> Result<ReadResourceResult, McpError> {
    let value = transport.request("resources/read", Some(json!({ "uri": uri })))?;
    parse_read_resource_result(&value)
}

/// The body of the `; profile="mcp-app"` regex (`resources.ts:50`):
/// `;\s*profile\s*=\s*"?mcp-app"?`, case-insensitive.
fn has_mcp_app_profile(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut search_from = 0;
    while let Some(offset) = lower[search_from..].find(';') {
        let mut index = search_from + offset + 1;
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if lower
            .get(index..)
            .is_some_and(|rest| rest.starts_with("profile"))
        {
            index += "profile".len();
            while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                index += 1;
            }
            if bytes.get(index) == Some(&b'=') {
                index += 1;
                while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                    index += 1;
                }
                if bytes.get(index) == Some(&b'"') {
                    index += 1;
                }
                if lower
                    .get(index..)
                    .is_some_and(|rest| rest.starts_with("mcp-app"))
                {
                    return true;
                }
            }
        }
        search_from = search_from + offset + 1;
    }
    false
}

/// `isMcpAppResource` (`resources.ts:48-51`): `ui://` URIs and
/// `profile=mcp-app` MIME types are host interfaces, left out of listings.
pub fn is_mcp_app_resource(
    uri: Option<&str>,
    uri_template: Option<&str>,
    mime_type: Option<&str>,
) -> bool {
    let identifier = uri.or(uri_template).unwrap_or_default();
    identifier.starts_with("ui://") || mime_type.is_some_and(has_mcp_app_profile)
}

/// `isMcpAppResource` applied to a raw listed item.
pub fn is_mcp_app_value(item: &Value) -> bool {
    let Some(map) = item.as_object() else {
        return false;
    };
    is_mcp_app_resource(
        map.get("uri").and_then(Value::as_str),
        map.get("uriTemplate").and_then(Value::as_str),
        map.get("mimeType").and_then(Value::as_str),
    )
}

/// `listed` (`resources.ts:54-57`): drop `_meta` and icons, tag with the
/// server. A `server` field already on the item wins, like `{ server, ...rest }`.
pub fn listed(server: &str, item: &Value) -> Value {
    let mut result = item.clone();
    if let Some(map) = result.as_object_mut() {
        map.remove("_meta");
        map.remove("icons");
        if !map.contains_key("server") {
            map.insert("server".to_string(), Value::String(server.to_string()));
        }
    }
    result
}

/// `stringArgument` (`resources.ts:168-173`): `undefined`/`null` and blank
/// strings become `None`; a non-string raises `"{key} must be a string"`.
pub fn string_argument(params: Option<&Value>, key: &str) -> Result<Option<String>, McpError> {
    let Some(value) = params.and_then(|params| params.get(key)) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(text) = value.as_str() else {
        return Err(McpError::Malformed(format!("{key} must be a string")));
    };
    let trimmed = text.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

/// `findServer`'s error text (`resources.ts:204-212`).
pub fn find_server_error<'a>(name: &str, servers: impl IntoIterator<Item = &'a str>) -> String {
    let available = servers.into_iter().collect::<Vec<_>>().join(", ");
    if available.is_empty() {
        format!("MCP server \"{name}\" has no resources")
    } else {
        format!("MCP server \"{name}\" has no resources. Servers with resources: {available}")
    }
}

/// The `list` helper of `resources.ts:214-255`: one page for a named server,
/// or every page of every server with per-server errors collected. `key` is
/// `"resources"` or `"resourceTemplates"`; `page` returns one server's page
/// and `all` returns all of one server's items.
pub fn list_payload<FPage, FAll>(
    params: Option<&Value>,
    key: &str,
    servers: &[String],
    page: FPage,
    all: FAll,
) -> Result<Value, McpError>
where
    FPage: Fn(&str, Option<&str>) -> Result<Page, McpError>,
    FAll: Fn(&str) -> Result<Vec<Value>, McpError>,
{
    let server_name = string_argument(params, "server")?;
    let cursor = string_argument(params, "cursor")?;
    if let Some(name) = &server_name {
        if !servers.contains(name) {
            return Err(McpError::Malformed(find_server_error(
                name,
                servers.iter().map(String::as_str),
            )));
        }
        let result = page(name, cursor.as_deref())?;
        let mut payload = Map::new();
        payload.insert("server".to_string(), Value::String(name.clone()));
        payload.insert(
            key.to_string(),
            Value::Array(
                result
                    .items
                    .iter()
                    .filter(|item| !is_mcp_app_value(item))
                    .map(|item| listed(name, item))
                    .collect(),
            ),
        );
        if let Some(next) = result.next_cursor {
            payload.insert("nextCursor".to_string(), Value::String(next));
        }
        return Ok(Value::Object(payload));
    }
    if cursor.is_some() {
        return Err(McpError::Malformed(
            "cursor can only be used when a server is specified".to_string(),
        ));
    }
    // `localeCompare` (resources.ts:242) has no std equivalent; a
    // case-folded byte compare is the closest reproducible approximation.
    let mut sorted = servers.to_vec();
    sorted.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    let mut items = Vec::new();
    let mut errors = Vec::new();
    for server in &sorted {
        match all(server) {
            Ok(values) => items.extend(
                values
                    .into_iter()
                    .filter(|item| !is_mcp_app_value(item))
                    .map(|item| listed(server, &item)),
            ),
            Err(error) => errors.push(json!({
                "server": server,
                "error": error_message(&error),
            })),
        }
    }
    let mut payload = Map::new();
    payload.insert(key.to_string(), Value::Array(items));
    if !errors.is_empty() {
        payload.insert("errors".to_string(), Value::Array(errors));
    }
    Ok(Value::Object(payload))
}

/// `formatSize` (`tools.ts:159-163`'s helper, `core/tools/truncate.ts:61-69`).
pub fn format_size(bytes: f64) -> String {
    if bytes < 1024.0 {
        format!("{bytes}B")
    } else if bytes < 1024.0 * 1024.0 {
        format!("{:.1}KB", bytes / 1024.0)
    } else {
        format!("{:.1}MB", bytes / (1024.0 * 1024.0))
    }
}

/// `extensionOf` (`tools.ts:160-163`): the URI's file extension, else `.bin`.
pub fn extension_of(uri: &str) -> String {
    let path = Url::parse(uri).map_or_else(|_| uri.to_string(), |parsed| parsed.path().to_string());
    path.rfind('.').map_or_else(
        || ".bin".to_string(),
        |index| {
            let extension = &path[index + 1..];
            if (1..=8).contains(&extension.len())
                && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
            {
                format!(".{extension}")
            } else {
                ".bin".to_string()
            }
        },
    )
}

/// `isTextMimeType` (`tools.ts:166-170`).
pub fn is_text_mime_type(mime_type: Option<&str>) -> bool {
    let Some(mime_type) = mime_type else {
        return false;
    };
    let base = mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    base.starts_with("text/")
        || base == "application/json"
        || base.ends_with("+json")
        || base.ends_with("+xml")
}

/// `Buffer.from(blob, "base64")` (`tools.ts:191`): a bad base64 string falls
/// back to its raw bytes.
fn base64_decode(blob: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(blob.trim())
        .unwrap_or_else(|_| blob.as_bytes().to_vec())
}

/// The blocks `resources.ts:320-329` hands to `toModelContent`, converted
/// the way `tools.ts:173-212` converts embedded resource blocks. `saver`
/// stands in for `saveToTempFile` (a temp file in the real extension), so
/// the flow runs without touching the filesystem.
pub fn contents_to_model_blocks<F>(result: &ReadResourceResult, uri: &str, saver: F) -> Vec<Value>
where
    F: Fn(&[u8], &str) -> Result<String, String>,
{
    let mut blocks = Vec::new();
    for contents in &result.contents {
        // Several contents (for example a directory) are labeled with URIs.
        if result.contents.len() > 1 {
            let label = match contents {
                ResourceContents::Text(text) => &text.uri,
                ResourceContents::Blob(blob) => &blob.uri,
            };
            blocks.push(json!({ "type": "text", "text": format!("{label}:") }));
        }
        match contents {
            ResourceContents::Text(text) => {
                blocks.push(json!({ "type": "text", "text": text.text }));
            }
            ResourceContents::Blob(blob) => {
                let data = base64_decode(&blob.blob);
                if let Some(mime) = blob.mime_type.as_deref()
                    && mime.starts_with("image/")
                {
                    blocks.push(json!({
                        "type": "image",
                        "data": blob.blob,
                        "mimeType": mime,
                    }));
                } else if is_text_mime_type(blob.mime_type.as_deref()) {
                    blocks.push(json!({
                        "type": "text",
                        "text": String::from_utf8_lossy(&data).into_owned(),
                    }));
                } else {
                    let kind = format!(
                        "{}, {}",
                        blob.mime_type.as_deref().unwrap_or("unknown type"),
                        format_size(data.len() as f64)
                    );
                    let extension = extension_of(&blob.uri);
                    let text = match saver(&data, &extension) {
                        Ok(path) => {
                            format!("[Binary resource {} ({kind}) saved to {path}]", blob.uri)
                        }
                        Err(reason) => format!(
                            "[Binary resource {} ({kind}) could not be saved: {reason}]",
                            blob.uri
                        ),
                    };
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({
            "type": "text",
            "text": format!("Resource {uri} is empty."),
        }));
    }
    blocks
}

/// The `structuredContent` of `read_mcp_resource` (`resources.ts:330-339`):
/// every contents entry without `_meta`.
pub fn read_structured_content(server: &str, uri: &str, result: &ReadResourceResult) -> Value {
    let contents = result
        .contents
        .iter()
        .map(|contents| {
            let mut value = serde_json::to_value(contents).unwrap_or(Value::Null);
            if let Some(map) = value.as_object_mut() {
                map.remove("_meta");
            }
            value
        })
        .collect::<Vec<_>>();
    json!({ "server": server, "uri": uri, "contents": contents })
}

/// `LIST_PARAMETERS` (`resources.ts:62-66`).
pub fn list_parameters_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": {
                "type": "string",
                "description": "MCP server name. Omit to list every server with resources."
            },
            "cursor": {
                "type": "string",
                "description": "Opaque cursor from a previous call with the same server; omit for the first page."
            }
        },
        "additionalProperties": false
    })
}

/// `READ_PARAMETERS` (`resources.ts:67-77`).
pub fn read_parameters_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": {
                "type": "string",
                "description": "MCP server name exactly as configured. Must match the 'server' field returned by list_mcp_resources."
            },
            "uri": {
                "type": "string",
                "description": "Resource URI to read. Must be one of the URIs returned by list_mcp_resources."
            }
        },
        "required": ["server", "uri"],
        "additionalProperties": false
    })
}

fn listing_errors_schema() -> Value {
    json!({
        "type": "array",
        "description": "Servers that could not be listed",
        "items": {
            "type": "object",
            "properties": { "server": { "type": "string" }, "error": { "type": "string" } },
            "required": ["server", "error"]
        }
    })
}

/// `LIST_OUTPUT_SCHEMA` (`resources.ts:89-113`).
pub fn list_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": { "type": "string" },
            "resources": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "server": { "type": "string" },
                        "uri": { "type": "string" },
                        "name": { "type": "string" },
                        "title": { "type": "string" },
                        "description": { "type": "string" },
                        "mimeType": { "type": "string" },
                        "size": { "type": "number" }
                    },
                    "required": ["server", "uri", "name"]
                }
            },
            "nextCursor": { "type": "string" },
            "errors": listing_errors_schema()
        },
        "required": ["resources"]
    })
}

/// `LIST_TEMPLATES_OUTPUT_SCHEMA` (`resources.ts:114-137`).
pub fn list_templates_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": { "type": "string" },
            "resourceTemplates": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "server": { "type": "string" },
                        "uriTemplate": { "type": "string", "description": "RFC 6570 URI template" },
                        "name": { "type": "string" },
                        "title": { "type": "string" },
                        "description": { "type": "string" },
                        "mimeType": { "type": "string" }
                    },
                    "required": ["server", "uriTemplate", "name"]
                }
            },
            "nextCursor": { "type": "string" },
            "errors": listing_errors_schema()
        },
        "required": ["resourceTemplates"]
    })
}

/// `READ_OUTPUT_SCHEMA` (`resources.ts:138-166`).
pub fn read_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": { "type": "string" },
            "uri": { "type": "string" },
            "contents": {
                "type": "array",
                "items": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {
                                "uri": { "type": "string" },
                                "mimeType": { "type": "string" },
                                "text": { "type": "string" }
                            },
                            "required": ["uri", "text"]
                        },
                        {
                            "type": "object",
                            "properties": {
                                "uri": { "type": "string" },
                                "mimeType": { "type": "string" },
                                "blob": { "type": "string", "description": "base64" }
                            },
                            "required": ["uri", "blob"]
                        }
                    ]
                }
            }
        },
        "required": ["server", "uri", "contents"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn transport_for(calls: Vec<(&'static str, Option<Value>, Value)>) -> impl ResourceTransport {
        let state = RefCell::new(calls.into_iter());
        move |method: &str, params: Option<Value>| -> Result<Value, McpError> {
            let mut calls = state.borrow_mut();
            let (expected_method, expected_params, result) =
                calls.next().expect("unexpected request");
            assert_eq!(method, expected_method);
            assert_eq!(params, expected_params);
            Ok(result)
        }
    }

    #[test]
    fn content_kinds_round_trip_through_serde() {
        let text: ResourceContents = serde_json::from_value(json!({
            "uri": "file:///a.txt", "mimeType": "text/plain", "text": "hello"
        }))
        .expect("text contents");
        assert_eq!(
            text,
            ResourceContents::Text(TextResourceContents {
                uri: "file:///a.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: "hello".to_string(),
                meta: None,
            })
        );
        let blob: ResourceContents = serde_json::from_value(json!({
            "uri": "file:///a.png", "mimeType": "image/png", "blob": "AAAA"
        }))
        .expect("blob contents");
        assert!(matches!(blob, ResourceContents::Blob(_)));
        // Text wins when a non-conforming server sends both.
        let both: ResourceContents = serde_json::from_value(json!({
            "uri": "file:///a", "text": "t", "blob": "AAAA"
        }))
        .expect("both");
        assert!(matches!(both, ResourceContents::Text(_)));
        let encoded = serde_json::to_value(&blob).expect("encode");
        assert_eq!(encoded["blob"], "AAAA");
        assert_eq!(encoded["mimeType"], "image/png");
        assert_eq!(encoded.get("_meta"), None);
    }

    #[test]
    fn read_result_validation_and_conversion() {
        let result = parse_read_resource_result(&json!({
            "contents": [
                {"uri": "file:///a.txt", "text": "one"},
                {"uri": "file:///b.bin", "mimeType": "application/octet-stream", "blob": "AAAA"}
            ],
            "_meta": {"trace": 1}
        }))
        .expect("valid read");
        assert_eq!(result.contents.len(), 2);
        assert_eq!(result.meta, Some(json!({"trace": 1})));
        assert_eq!(
            error_message(&parse_read_resource_result(&json!({})).unwrap_err()),
            "Invalid MCP resources/read result"
        );
        assert_eq!(
            error_message(
                &parse_read_resource_result(&json!({"contents": [{"uri": "x"}]})).unwrap_err()
            ),
            "Invalid contents in MCP resources/read result"
        );
    }

    #[test]
    fn list_page_validation_normalizes_and_errors_verbatim() {
        let page = validate_list_page(
            "resources/list",
            "resources",
            &json!({
                "resources": [{"uri": "u", "name": "n"}], "nextCursor": null
            }),
            is_resource,
        )
        .expect("page");
        assert_eq!(page.next_cursor, None);
        let page = validate_list_page(
            "resources/list",
            "resources",
            &json!({
                "resources": [], "nextCursor": ""
            }),
            is_resource,
        )
        .expect("page");
        assert_eq!(page.next_cursor, None);
        let page = validate_list_page(
            "resources/list",
            "resources",
            &json!({
                "resources": [], "nextCursor": "next"
            }),
            is_resource,
        )
        .expect("page");
        assert_eq!(page.next_cursor.as_deref(), Some("next"));
        assert_eq!(
            error_message(
                &validate_list_page("resources/list", "resources", &json!({}), is_resource)
                    .unwrap_err()
            ),
            "Invalid MCP resources/list result"
        );
        assert_eq!(
            error_message(
                &validate_list_page(
                    "resources/list",
                    "resources",
                    &json!({
                        "resources": [{"name": "no uri"}]
                    }),
                    is_resource
                )
                .unwrap_err()
            ),
            "Invalid entry in MCP resources/list result"
        );
        assert_eq!(
            error_message(
                &validate_list_page(
                    "resources/list",
                    "resources",
                    &json!({
                        "resources": [], "nextCursor": 7
                    }),
                    is_resource
                )
                .unwrap_err()
            ),
            "Invalid MCP resources/list cursor"
        );
    }

    #[test]
    fn list_conversions_default_missing_names() {
        let result = parse_list_resources_page(&json!({
            "resources": [{"uri": "file:///a.txt", "mimeType": "text/plain", "size": 12}],
            "nextCursor": "p2"
        }))
        .expect("resources");
        assert_eq!(result.resources[0].name, "file:///a.txt");
        assert_eq!(result.resources[0].size, Some(12.0));
        assert_eq!(result.next_cursor.as_deref(), Some("p2"));
        assert_eq!(result.meta, None);
        let templates = parse_list_resource_templates_page(&json!({
            "resourceTemplates": [{"uriTemplate": "file:///{path}"}]
        }))
        .expect("templates");
        assert_eq!(templates.resource_templates[0].name, "file:///{path}");
    }

    #[test]
    fn list_all_follows_cursors_and_rejects_loops() {
        let pages = |cursor: Option<&str>| -> Result<Page, McpError> {
            match cursor {
                None => Ok(Page {
                    items: vec![json!("a")],
                    next_cursor: Some("c1".to_string()),
                }),
                Some("c1") => Ok(Page {
                    items: vec![json!("b")],
                    next_cursor: None,
                }),
                _ => panic!("unexpected cursor"),
            }
        };
        assert_eq!(
            list_all("resources/list", pages).expect("all"),
            vec![json!("a"), json!("b")]
        );
        let looping = |_: Option<&str>| -> Result<Page, McpError> {
            Ok(Page {
                items: Vec::new(),
                next_cursor: Some("same".to_string()),
            })
        };
        assert_eq!(
            error_message(&list_all("resources/list", looping).unwrap_err()),
            "MCP resources/list returned duplicate cursor: same"
        );
    }

    #[test]
    fn transport_backed_flows_issue_the_upstream_methods_and_params() {
        let page = transport_for(vec![(
            "resources/list",
            Some(json!({"cursor": "c"})),
            json!({"resources": [{"uri": "u", "name": "n"}]}),
        )]);
        let result = list_resources_page(&page, Some("c")).expect("page");
        assert_eq!(result.resources[0].uri, "u");
        assert_eq!(result.next_cursor, None);

        let all = transport_for(vec![
            (
                "resources/list",
                None,
                json!({"resources": [{"uri": "u1"}], "nextCursor": "c2"}),
            ),
            (
                "resources/list",
                Some(json!({"cursor": "c2"})),
                json!({"resources": [{"uri": "u2"}]}),
            ),
        ]);
        let resources = list_resources(&all).expect("all");
        assert_eq!(
            resources.iter().map(|r| r.uri.as_str()).collect::<Vec<_>>(),
            ["u1", "u2"]
        );

        let templates = transport_for(vec![(
            "resources/templates/list",
            None,
            json!({"resourceTemplates": [{"uriTemplate": "t://{id}"}]}),
        )]);
        let listed = list_resource_templates(&templates).expect("templates");
        assert_eq!(listed[0].uri_template, "t://{id}");

        let read = transport_for(vec![(
            "resources/read",
            Some(json!({"uri": "file:///a.txt"})),
            json!({"contents": [{"uri": "file:///a.txt", "text": "body"}]}),
        )]);
        let result = read_resource(&read, "file:///a.txt").expect("read");
        assert_eq!(result.contents.len(), 1);
    }

    #[test]
    fn mcp_app_detection_covers_uri_and_profile_variants() {
        assert!(is_mcp_app_resource(
            Some("ui://widget/index.html"),
            None,
            None
        ));
        assert!(is_mcp_app_resource(None, Some("ui://x/{id}"), None));
        assert!(is_mcp_app_resource(
            None,
            None,
            Some("text/html;profile=mcp-app")
        ));
        assert!(is_mcp_app_resource(
            None,
            None,
            Some("text/html; PROFILE = \"mcp-app\"")
        ));
        assert!(is_mcp_app_resource(
            None,
            None,
            Some("text/html; profile=mcp-app; charset=utf-8")
        ));
        assert!(!is_mcp_app_resource(
            Some("file:///a.txt"),
            None,
            Some("text/plain")
        ));
        assert!(!is_mcp_app_resource(
            None,
            None,
            Some("text/html; profile=other")
        ));
        assert!(is_mcp_app_value(&json!({"uri": "ui://x", "name": "x"})));
        assert!(!is_mcp_app_value(&json!({"uri": "file:///x", "name": "x"})));
    }

    #[test]
    fn listed_strips_meta_and_icons_and_tags_the_server() {
        let value = listed(
            "alpha",
            &json!({
                "uri": "u", "name": "n", "_meta": {"x": 1}, "icons": [{"src": "i"}]
            }),
        );
        assert_eq!(value["server"], "alpha");
        assert_eq!(value.get("_meta"), None);
        assert_eq!(value.get("icons"), None);
        assert_eq!(value["uri"], "u");
        let overridden = listed("alpha", &json!({"server": "beta", "uri": "u"}));
        assert_eq!(overridden["server"], "beta");
    }

    #[test]
    fn string_arguments_trim_and_reject_non_strings() {
        let params = json!({"server": "  alpha  ", "cursor": null, "uri": 5});
        assert_eq!(
            string_argument(Some(&params), "server")
                .expect("server")
                .as_deref(),
            Some("alpha")
        );
        assert_eq!(
            string_argument(Some(&params), "cursor").expect("cursor"),
            None
        );
        assert_eq!(
            string_argument(Some(&params), "missing").expect("missing"),
            None
        );
        assert_eq!(
            error_message(&string_argument(Some(&params), "uri").unwrap_err()),
            "uri must be a string"
        );
        assert_eq!(
            string_argument(Some(&json!({"server": "   "})), "server").expect("blank"),
            None
        );
    }

    #[test]
    fn find_server_error_is_verbatim() {
        assert_eq!(
            find_server_error("ghost", ["alpha", "beta"]),
            "MCP server \"ghost\" has no resources. Servers with resources: alpha, beta"
        );
        assert_eq!(
            find_server_error("ghost", std::iter::empty::<&str>()),
            "MCP server \"ghost\" has no resources"
        );
    }

    #[test]
    fn list_payload_handles_single_and_all_server_modes() {
        let servers = vec!["beta".to_string(), "alpha".to_string()];
        let page = |server: &str, cursor: Option<&str>| -> Result<Page, McpError> {
            assert_eq!(server, "beta");
            assert_eq!(cursor, Some("c"));
            Ok(Page {
                items: vec![json!({"uri": "u", "name": "n"})],
                next_cursor: Some("next".to_string()),
            })
        };
        let all = |server: &str| -> Result<Vec<Value>, McpError> {
            match server {
                "alpha" => Ok(vec![json!({"uri": "a", "name": "a"})]),
                _ => Err(McpError::Malformed("offline".to_string())),
            }
        };
        let one = list_payload(
            Some(&json!({"server": "beta", "cursor": "c"})),
            "resources",
            &servers,
            page,
            all,
        )
        .expect("one");
        assert_eq!(one["server"], "beta");
        assert_eq!(one["resources"][0]["server"], "beta");
        assert_eq!(one["nextCursor"], "next");

        let every = list_payload(None, "resources", &servers, page, all).expect("all");
        assert_eq!(every["resources"][0]["uri"], "a");
        assert_eq!(every["errors"][0]["server"], "beta");
        assert_eq!(every["errors"][0]["error"], "offline");
        assert_eq!(every.get("nextCursor"), None);

        assert_eq!(
            error_message(
                &list_payload(
                    Some(&json!({"cursor": "c"})),
                    "resources",
                    &servers,
                    page,
                    all
                )
                .unwrap_err()
            ),
            "cursor can only be used when a server is specified"
        );
    }

    #[test]
    fn model_content_converts_each_kind_and_the_empty_fallback() {
        let result = parse_read_resource_result(&json!({
            "contents": [
                {"uri": "file:///a.txt", "text": "plain"},
                {"uri": "file:///b.png", "mimeType": "image/png", "blob": "AAAA"},
                {"uri": "file:///c.json", "mimeType": "application/json", "blob": "eyJvayI6MX0="},
                {"uri": "file:///d.bin", "mimeType": "application/octet-stream", "blob": "AAAA"}
            ]
        }))
        .expect("read");
        let blocks = contents_to_model_blocks(&result, "file:///dir", |data, extension| {
            assert_eq!(data.len(), 3);
            assert_eq!(extension, ".bin");
            Ok("/tmp/saved.bin".to_string())
        });
        assert_eq!(blocks.len(), 8);
        assert_eq!(blocks[1]["text"], "plain");
        assert_eq!(blocks[3]["type"], "image");
        assert_eq!(blocks[5]["text"], "{\"ok\":1}");
        assert_eq!(
            blocks[7]["text"],
            "[Binary resource file:///d.bin (application/octet-stream, 3B) saved to /tmp/saved.bin]"
        );
        let empty = ReadResourceResult {
            contents: Vec::new(),
            meta: None,
        };
        let blocks = contents_to_model_blocks(&empty, "file:///empty", |_, _| unreachable!());
        assert_eq!(blocks[0]["text"], "Resource file:///empty is empty.");
    }

    #[test]
    fn structured_content_drops_each_meta() {
        let result = parse_read_resource_result(&json!({
            "contents": [{"uri": "u", "text": "t", "_meta": {"x": 1}}]
        }))
        .expect("read");
        let payload = read_structured_content("alpha", "u", &result);
        assert_eq!(payload["server"], "alpha");
        assert_eq!(payload["uri"], "u");
        assert_eq!(payload["contents"][0].get("_meta"), None);
        assert_eq!(payload["contents"][0]["text"], "t");
    }

    #[test]
    fn uri_extensions_sizes_and_mime_types() {
        assert_eq!(extension_of("file:///a.txt"), ".txt");
        assert_eq!(extension_of("urn:example:thing"), ".bin");
        assert_eq!(extension_of("a.TXT"), ".TXT");
        assert_eq!(format_size(512.0), "512B");
        assert_eq!(format_size(2048.0), "2.0KB");
        assert_eq!(format_size(2.0 * 1024.0 * 1024.0), "2.0MB");
        assert!(is_text_mime_type(Some("text/plain; charset=utf-8")));
        assert!(is_text_mime_type(Some("application/ld+json")));
        assert!(!is_text_mime_type(Some("image/png")));
        assert!(!is_text_mime_type(None));
    }

    #[test]
    fn schemas_expose_upstream_shapes() {
        assert_eq!(
            list_parameters_schema()["additionalProperties"],
            json!(false)
        );
        assert_eq!(list_output_schema()["required"][0], "resources");
        assert_eq!(
            list_templates_output_schema()["properties"]["resourceTemplates"]["type"],
            "array"
        );
        assert_eq!(read_parameters_schema()["required"][1], "uri");
        let schema = read_output_schema();
        let any_of = schema["properties"]["contents"]["items"]["anyOf"]
            .as_array()
            .expect("anyOf");
        assert_eq!(any_of.len(), 2);
    }
}
