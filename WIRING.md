# MCP resources — coordinator wiring

Branch `feat/mcp-resources` (do not push; ff-merge after CI). Nothing here
depends on `client.rs`, `mcp_manager.rs`, or the host runtime being edited
first — the new module is standalone and its tests run without a server.

## What landed on this branch

| File | Change |
|---|---|
| `crates/pi-mcp/src/resources.rs` | **New**, self-contained. All types, validation, pagination, payload builders, URI handling, tool schemas, and 15 unit tests. |
| `crates/pi-mcp/src/lib.rs` | Added `pub mod resources;`, a doc bullet, and re-exports of the 9 public data/trait types. **This is the only pre-existing file I touched** — it is `pi-mcp`'s own crate root, not a shared wiring file. If another slice also edited it, merge both lines; no other file was changed. |

## `resources.rs` public surface

Types: `Resource`, `ResourceTemplate`, `TextResourceContents`,
`BlobResourceContents`, `ResourceContents` (untagged, text-first),
`ListResourcesResult`, `ListResourceTemplatesResult`, `ReadResourceResult`,
`Page`, and the `ResourceTransport` trait.

Constants: `LIST_MCP_RESOURCES_TOOL`, `LIST_MCP_RESOURCE_TEMPLATES_TOOL`,
`READ_MCP_RESOURCE_TOOL`, `MAX_LIST_PAGES`, `LIST_RESOURCES_DESCRIPTION`,
`LIST_TEMPLATES_DESCRIPTION`, `READ_DESCRIPTION`.

Pure (no transport) helpers, all verbatim-faithful to upstream:

```rust
pub fn error_message(error: &McpError) -> String;          // errorMessage(), raw .message
pub fn is_resource(item: &Value) -> bool;                  // client.ts:113
pub fn is_resource_template(item: &Value) -> bool;         // client.ts:115
pub fn validate_list_page(method, key, value, is_item) -> Result<Page, McpError>; // client.ts:93
pub fn to_resource(item: &Value) -> Result<Resource, McpError>;                   // client.ts:118
pub fn to_resource_template(item: &Value) -> Result<ResourceTemplate, McpError>;  // client.ts:122
pub fn parse_list_resources_page(value: &Value) -> Result<ListResourcesResult, McpError>;
pub fn parse_list_resource_templates_page(value: &Value) -> Result<ListResourceTemplatesResult, McpError>;
pub fn parse_read_resource_result(value: &Value) -> Result<ReadResourceResult, McpError>;
pub fn list_all<F>(method, page: F) -> Result<Vec<Value>, McpError>; // F: FnMut(Option<&str>) -> Result<Page, McpError>
pub fn is_mcp_app_resource(uri, uri_template, mime_type) -> bool;   // resources.ts:48
pub fn is_mcp_app_value(item: &Value) -> bool;
pub fn listed(server: &str, item: &Value) -> Value;                 // resources.ts:54
pub fn string_argument(params, key) -> Result<Option<String>, McpError>;
pub fn find_server_error(name, servers) -> String;
pub fn list_payload<FPage, FAll>(params, key, &[String], page, all) -> Result<Value, McpError>;
pub fn format_size(bytes: f64) -> String;
pub fn extension_of(uri: &str) -> String;
pub fn is_text_mime_type(mime: Option<&str>) -> bool;
pub fn contents_to_model_blocks<F>(result, uri, saver: F) -> Vec<Value>; // F: Fn(&[u8], &str) -> Result<String,String>
pub fn read_structured_content(server, uri, result) -> Value;
pub fn list_parameters_schema() -> Value;
pub fn read_parameters_schema() -> Value;
pub fn list_output_schema() -> Value;
pub fn list_templates_output_schema() -> Value;
pub fn read_output_schema() -> Value;
```

Transport-backed (sync; `T: ResourceTransport`): `list_resources_page`,
`list_resources`, `list_resource_templates_page`, `list_resource_templates`,
`read_resource`. These are convenient for sync adapters; **do not use them
from inside a tokio runtime** — use the async `McpClient` methods below.

`ResourceTransport` has one method:
`fn request(&self, method: &str, params: Option<Value>) -> Result<Value, McpError>`.
Any `Fn(&str, Option<Value>) -> Result<Value, McpError>` implements it, which is
how the tests inject canned JSON-RPC replies.

## 1. `crates/pi-mcp/src/client.rs` — add async client methods

`McpClient::transport.request` is private and async, so the five wire calls
belong here. Paste inside `impl McpClient` (use `crate::resources::*`):

```rust
/// One `resources/list` page (`client.ts:306-309`).
pub async fn resources_list_page(
    &self,
    cursor: Option<&str>,
    timeout: Duration,
) -> Result<ListResourcesResult, McpError> {
    let params = cursor.map(|cursor| serde_json::json!({ "cursor": cursor }));
    let value = self.transport.request("resources/list", params, timeout).await?;
    crate::resources::parse_list_resources_page(&value)
}

/// Every resource, following `nextCursor` (client.ts:301-303, 357-376).
pub async fn resources_list(&self, timeout: Duration) -> Result<Vec<Resource>, McpError> {
    let mut items = Vec::new();
    let mut cursors = std::collections::HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_LIST_PAGES {
        let page = self.resources_list_page(cursor.as_deref(), timeout).await?;
        items.extend(page.resources);
        let Some(next) = page.next_cursor else { return Ok(items); };
        if !cursors.insert(next.clone()) {
            return Err(McpError::Malformed(format!(
                "MCP resources/list returned duplicate cursor: {next}"
            )));
        }
        cursor = Some(next);
    }
    Err(McpError::Malformed(format!(
        "MCP resources/list exceeded {MAX_LIST_PAGES} pages"
    )))
}
```

Repeat for templates with method `"resources/templates/list"`, result type
`ListResourceTemplatesResult`, field `resource_templates`, and error messages
`"MCP resources/templates/list returned duplicate cursor: {next}"` /
`"MCP resources/templates/list exceeded {MAX_LIST_PAGES} pages"`. Both template
methods **must swallow "method not found"** like upstream's `withoutTemplates`
(`runtime.ts:126-138`): map `Err(McpError::Rpc { code: METHOD_NOT_FOUND, .. })`
to `Ok(...empty...)` (`METHOD_NOT_FOUND` is `-32601`, already in
`types::json_rpc_error_codes`).

```rust
/// `resources/read` (`client.ts:337-339`).
pub async fn resource_read(
    &self,
    uri: &str,
    timeout: Duration,
) -> Result<ReadResourceResult, McpError> {
    let value = self
        .transport
        .request("resources/read", Some(serde_json::json!({ "uri": uri })), timeout)
        .await?;
    crate::resources::parse_read_resource_result(&value)
}
```

No `client.rs` changes beyond these additions; `ActiveTransport::request`
already handles both stdio and HTTP.

## 2. `crates/pi-ext-js/src/mcp_manager.rs` — connections and tool results

### 2a. Record the resources capability on connect

`ServerCapabilities.resources: Option<Value>` already exists in `types.rs`. In
`connect_with_spawn` (`:377-430`), alongside `tools`, capture
`has_resources = client.capabilities.resources.is_some()` (`runtime.ts:393`).
Add it to both `ServerState::Connected` and `ConnectOutcome`, and expose
`McpManager::servers_with_resources() -> Vec<String>` so the extension can
build the `servers()` callback of `createMcpResourceToolDefinitions` — only
servers with resources are reachable (upstream `McpResourceServer[]`).

### 2b. New worker requests

Add to `enum Request` (`:91-108`) and handle them in `worker_loop` (`:258-313`):

```rust
ListResourcePage { server: String, cursor: Option<String>, reply: Sender<Result<Json,String>> },
ListResources     { server: String, reply: Sender<Result<Json,String>> },
ListTemplatePage  { server: String, cursor: Option<String>, reply: Sender<Result<Json,String>> },
ListTemplates     { server: String, reply: Sender<Result<Json,String>> },
ReadResource      { server: String, uri: String, reply: Sender<Result<Json,String>> },
```

Add `McpManager` methods mirroring `call_tool` (`:179-200`) — same
`{"result": …}` / `{"error": …}` reply convention (the JS wrapper at
`engine.rs:575-587` throws on `error`):

```rust
pub fn list_resources_page(&self, server: &str, cursor: Option<&str>) -> Json;
pub fn list_resources(&self, server: &str) -> Json;
pub fn list_resource_templates_page(&self, server: &str, cursor: Option<&str>) -> Json;
pub fn list_resource_templates(&self, server: &str) -> Json;
pub fn read_resource(&self, server: &str, uri: &str) -> Json;
```

Each worker handler calls the matching `McpClient` method via
`runtime.block_on(client.resources_*(&timeout))` (never `block_on` a second
time inside a running task). Use `self.reply_window(server, config_default_window())`
for the reply timeout.

Read-only reconnect semantics (upstream `withClient(..., true)`,
`runtime.ts:280-303`): reuse `call_tool_on`'s shape (`:456-537`) for lazy
reconnect on `ServerState::Failed`, one retry on `error_is_transient` (`:359`),
and one retry on `McpError::SessionExpired`. Unlike tool calls, retries are
safe because reads are idempotent.

### 2c. Assemble the three tool results

The three definitions are registered where the MCP tools are registered
(`runner.rs` registerTools area, `:878`), with names/descriptions/P schemas and
output schemas from `resources.rs` constants and `*_schema()` functions,
`readOnlyHint: true`, and the same exposure mapping as `toToolExposure`.

**List tools.** Call `resources::list_payload` with closures that `block_on`
one page / all pages for a server; then feed its JSON text through the existing
`limit_mcp_content` and wrap exactly like `jsonResult` (`resources.ts:179-193`):

```rust
// payload = resources::list_payload(params, "resources", &servers, page, all)?
let (content, full_output_path) = limit_mcp_content(vec![json!({
    "type": "text",
    "text": serde_json::to_string(&payload).unwrap_or_default(),
})]);
// { "content": content,
//   "details": { "server": server.unwrap_or(""), "tool": LIST_MCP_RESOURCES_TOOL, … },
//   "structuredContent": payload }
```

`list_payload` already filters MCP-App resources (`ui://` / `profile=mcp-app`),
strips `_meta`/icons via `listed`, tags each item with `server`, aggregates
per-server errors into `errors`, and applies the cursor/server guards. Pass
the real page/all closures from 2b through `resources::parse_*` or straight
from the `McpClient` results converted with `serde_json::to_value`.

**Read tool.** Validate with `string_argument` (`server`, `uri` required —
upstream errors `"server must be provided"` / `"uri must be provided"`),
look up the server with `find_server_error`, then:

```rust
let read = runtime.block_on(client.resource_read(uri, timeout))?; // via 2b
let blocks = resources::contents_to_model_blocks(&read, uri,
    |bytes, extension| write_output_file(extension, bytes));
let (content, full_output_path) = limit_mcp_content(if blocks.is_empty() {
    vec![json!({"type":"text","text": format!("Resource {uri} is empty.")})]
} else { blocks });
let structured = resources::read_structured_content(server, uri, &read);
// details: { "server": server, "tool": READ_MCP_RESOURCE_TOOL, …fullOutputPath }
```

`contents_to_model_blocks` already emits the per-URI labels for multi-content
results, passes text through, turns `image/*` blobs into image blocks, decodes
text MIME blobs, and saves other blobs to a temp file (or reports the failure).
`read_structured_content` strips `_meta` from every contents entry.

### 2d. Expose the three ops to extensions

Add host ops alongside `mcpCallTool` (`engine.rs:575-587`), e.g.
`mcpListResources`, `mcpListResourceTemplates`, `mcpReadResource`, each
calling the matching `McpManager` method from 2b and serializing the `Json`
reply to a string. The bundled extension JS then declares the three tools
using the constants/schemas above and calls these ops.

## Semantics / deviations to be aware of

1. **Sync transport boundary.** `ResourceTransport` is synchronous so tests
   need no runtime; the real bridge is the async `McpClient` methods in §1 and
   `runtime.block_on` in the manager. Do not build a sync adapter that calls
   `Handle::block_on` from inside the worker's runtime — that panics.
2. **Plain `Error` cases.** Upstream's duplicate-cursor and
   `exceeded 1000 pages` errors are plain `Error`s; here they are
   `McpError::Malformed(message)`. `error_message()` returns the raw message
   verbatim, which is what the listing `errors` array needs.
3. **Verbatim `invalid()` strings** use `McpError::Rpc { code: -32600, … }`.
   `error_message()` extracts `.message` like upstream's `errorMessage`.
4. **`localeCompare`** has no std equivalent; server-name sorting in
   `list_payload` uses a case-folded byte compare (documented in-code).
5. **Strict optional fields.** Typed structs decode `size` as `f64`; upstream
   spreads raw JSON and would pass a malformed `size` through. No upstream
   fixture relies on malformed optional fields.
6. **MIME regex ported by hand** (`has_mcp_app_profile`) because `pi-mcp` has
   no regex crate; semantics match `/;\s*profile\s*=\s*"?mcp-app"?/i`.
7. **`limitMcpContent` stays in `mcp_manager.rs`** (already ported there);
   `contents_to_model_blocks` stops at pre-limit blocks and takes the temp-file
   writer as a closure so the module never touches the filesystem.
8. **Subscriptions** (`resources/subscribe` / `unsubscribe`) do not exist in
   upstream 1.5.2 — intentionally not ported.
9. The port's pinned handshake version is `2025-06-18` (`lib.rs`); resources
   are stable in every supported version, so no extra negotiation is needed.

## Verification gate (coordinator)

```
podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh test -p pi-mcp
podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh check -p pi-mcp
podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh gate   # after wiring the other crates
podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh snapshot
```

15 tests in `resources.rs` cover text/blob round-trips, read/list validation
and verbatim errors, cursor normalization, `listAll` pagination/loops, mock
transport flows, MCP-App detection, `listed`, argument parsing, server lookup,
both list-payload modes, model-content conversion, `_meta` stripping, and the
five output schemas.
