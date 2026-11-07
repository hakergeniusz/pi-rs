//! JavaScript-in-sandbox tool execution ("code mode").
//!
//! Rust port of `@earendil-works/pi-codemode` (TS source:
//! `packages/codemode/src`).
//!
//! Scope (P5): a JS runtime exposing the agent's tools to generated code,
//! matching the TS behavior of running sandboxed scripts with a tool-calling
//! bridge. Engine choice (`deno_core` / `quickjs` / `boa`) is part of the P5 gate
//! together with the extension-host decision.

/// Sandbox build placeholder (real runtime lands in P5).
pub const SANDBOX_VERSION: &str = "0.0.0";
