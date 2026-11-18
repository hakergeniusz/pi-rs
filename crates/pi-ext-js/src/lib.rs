//! The E1 headless extensions core: an embedded `QuickJS` host that loads
//! TypeScript/JavaScript extensions and dispatches their events with
//! upstream's exact semantics.
//!
//! Ports the runtime half of `@earendil-works/pi-coding-agent` 1.5.2's
//! extensions system to Rust, on the engine picked at the E0 gate:
//! `rquickjs` 0.14 (QuickJS-NG). What each module ports:
//!
//! - [`transpile`] — the jiti transform step (`oxc` strip), E1's
//!   replacement for Babel (`core/extensions/loader.ts:571-602`).
//! - [`resolver`] — the virtual-module table (`virtual-modules.ts`),
//!   relative-path resolution, and the directory entry rules.
//! - [`engine`] + [`glue`] — the embedded runtime, the `pi` API object
//!   with `createExtensionAPI`'s exact registration errors
//!   (`core/extensions/loader.ts:253-561`), the headless contexts, and
//!   the dispatch-session protocol.
//! - [`loader`] — discovery, the `pi.extensions` manifest
//!   (`core/pi-manifest.ts`), trust-gated loading, and errors-as-values.
//! - [`runner`] — the `ExtensionRunner` dispatch semantics, ported method
//!   by method from `core/extensions/runner.ts` (§2 of
//!   `docs/EXTENSIONS.md` tabulates them).
//! - [`mcp_registry`], [`mcp_manager`], [`exec`] — the MCP registration
//!   bridge and validation, the native server manager (the host half of
//!   the bundled `mcp` extension), and the `pi.exec` process primitive.
//!
//! Deliberately E1-scoped (§8 of `docs/EXTENSIONS.md`): no UI surface
//! beyond the headless no-op matrix, no `node:*` built-ins, no
//! `node_modules` resolution, and no provider registration — each lands
//! in E2–E4. Deviations as they appear are recorded in `HANDOFF.md`.

pub mod engine;
pub mod error;
pub mod events;
pub mod exec;
pub mod glue;
pub mod loader;
pub mod mcp_manager;
pub mod mcp_registry;
pub mod provider_registry;
pub mod resolver;
pub mod runner;
pub mod transcript;
pub mod transpile;

pub use engine::{ContextKind, HostActions, HostCore, JsHost};
pub use error::{ExtensionError, JsHostError, LoadError};
pub use loader::{ExtensionOrigin, LoadExtensionsResult, discover_and_load, discover_extensions};
pub use provider_registry::{RegisteredNativeProvider, RegisteredProvider, RegisteredVirtualModel};
pub use runner::{
    EntryRenderOutcome, ExtensionRunner, McpServerReport, ProviderStreamOutcome, ResolvedCommand,
    ResolvedMarkdownTransformer, ToolInfo, UiSink, UserBashDecision,
};
