# Wiring note: F2b MCP OAuth credential store

This branch adds exactly one module plus its export:

* `crates/pi-mcp/src/oauth/store.rs` — the file-backed `mcp-auth.json`
  credential store (port of
  `packages/coding-agent/src/extensions/mcp/oauth.ts:44-157` and the
  `FileAuthStorageBackend.withLock` half of `core/auth-storage.ts:36-116`).
* `crates/pi-mcp/src/oauth/mod.rs` — `pub mod store;` and
  `pub use store::{MCP_AUTH_FILE_NAME, McpOAuthCredentialStore,
  ServerOAuthStore, default_store_path, store_keys};`. No other file in the
  workspace was touched.

No `Cargo.toml`, `Cargo.lock`, `main.rs`, `lib.rs`, `mcp_manager.rs`,
`HANDOFF.md` or `AGENTS.md` change is needed for the module itself.

## What the coordinator must change to actually attach OAuth

The store is deliberately dormant: no existing call site can use it until a
host passes an auth provider to the HTTP transport. The minimum set of edits:

### 1. `crates/pi-mcp/src/client.rs`

`McpTransportConfig::Http` (`client.rs:38-41`) currently carries only `url`
and `headers`. Add

```rust
auth: Option<Arc<dyn crate::auth_provider::AuthProvider>>,
```

(defaults to `None` where the struct is built outside the manager) and forward
it in `ActiveTransport::build` (`client.rs:73-79`) into
`StreamableHttpOptions { auth, .. }`. `StreamableHttpOptions` already has
`pub auth: Option<Arc<dyn AuthProvider>>` (`http.rs:350-356`), so no change is
needed in `http.rs`.

### 2. `crates/pi-ext-js/src/mcp_manager.rs`

`spawn_config` (`mcp_manager.rs:542-567`) builds the HTTP config from a
`url`, resolving `headers`. For a URL server with no `Authorization` header:

1. Build one shared store (once per manager/host, not per connection):

   ```rust
   use pi_mcp::oauth::{McpOAuthCredentialStore, McpAuthProvider,
       McpAuthProviderOptions, McpOAuthSettings, McpClientMetadataDocuments};

   let store = McpOAuthCredentialStore::new();
   ```

   `McpOAuthCredentialStore::new()` resolves to
   `$XDG_CONFIG_HOME/pi-rs/mcp-auth.json` (or `~/.config/pi-rs/mcp-auth.json`,
   or `$PI_CODING_AGENT_DIR/mcp-auth.json`) — the pi-rs XDG agent dir, never
   `~/.pi`. `McpOAuthCredentialStore::with_path(path)` is available for a
   relocated agent dir (its refresh-lock dir is the file's parent).
2. Per server, get the store view and attach the provider:

   ```rust
   let server_store = Arc::new(store.for_server(name, url)?); // implements
       // McpOAuthStateStore + McpOAuthServerStore
   let provider = Arc::new(McpAuthProvider::new(McpAuthProviderOptions {
       server_url: url.to_string(),
       store: server_store,
       settings: Arc::new(move || McpOAuthSettings { /* config.oauth */ ..Default::default() }),
       client_name: pi_mcp::CLIENT_NAME.to_string(),   // "pi-rs" = APP_NAME
       client_metadata_documents: /* pi.dev CIMD impl, base url https://pi.dev/oauth */,
       on_challenge: Arc::new(move |challenge| /* remember it for /mcp sign-in */),
       redirect_url: None,
   }));
   ```

   Put `Some(provider)` in the new `McpTransportConfig::Http.auth` field.
   `McpAuthProvider` is already re-exported from
   `pi_mcp::oauth::auth_provider` — no new provider code is needed.
3. Skip the provider when the server has an explicit `Authorization` header
   (upstream `config.ts:24-26`: an `auth.provider` entry sends a `/login`
   token instead) — the same branch decides whether to attach a bearer
   provider from the resolved header.

`ServerOAuthStore` (returned by `for_server`) supplies `load`/`save` to
`McpOAuthProvider` and `with_refresh_lock` to `McpAuthProvider`; the lock
keeps two processes from rotating the same refresh token. A lock held for
the whole 25 s wait fails with `McpError::OAuth(OAuthError::RefreshLockBusy)`
— Display text exactly `Another process is refreshing the MCP server's
tokens` — which upstream treats as "go to the browser sign-in step".

### 3. `crates/pi-coding-agent`

Only needed for the `/mcp` sign-in UI and `pi mcp login` (separate slices):
hand the same `Arc<McpOAuthCredentialStore>` to the command layer so it can
call `for_server(...).save(...)`, `tokens(name, url)` and `remove(name, url)`
after a successful PKCE flow or sign-out. No constructor argument changes
are needed: `McpOAuthCredentialStore::new()` reads the same config root as
`pi-coding-agent::config::get_agent_dir()`.

## Semantics and error strings to preserve

| Operation | Upstream | Port |
|---|---|---|
| File | `<agent-dir>/mcp-auth.json`, `{}` on creation, `JSON.stringify(states, null, 2)\n` | identical bytes; `serde_json` has `preserve_order`, so insertion order is kept |
| Key | `mcp__<name with -→_>|<String(new URL(url))>` | `store_keys()` uses `Url::parse(...).to_string()`; invalid URL → `McpError::OAuth(OAuthError::Other(<parse error>))` |
| Legacy key | bare normalized URL, taken over by the first `load()`; `tokens()` never takes over | identical; `remove()` deletes whichever key holds the state |
| Servers sharing a URL | separate keys, no clobbering | identical (sequential saves merge under the file lock) |
| Missing/blank/non-object JSON | `{}`, no error | identical |
| Malformed JSON | V8 `SyntaxError` escapes | `Failed to read mcp-auth.json: <serde_json error>` (the `auth-storage.ts:224` wording, adapted) |
| Refresh lock | `McpRefreshLockBusyError` after 25 s | `OAuthError::RefreshLockBusy`, Display `Another process is refreshing the MCP server's tokens` |
| Credential lock busy | `proper-lockfile` `ELOCKED` | `mcp-auth.json is locked by another process` |

Every method that can fail returns `McpError`; the store never panics and
never reads a real config/auth directory in tests (all tests use unique
directories under the system temp dir with synthetic tokens).
