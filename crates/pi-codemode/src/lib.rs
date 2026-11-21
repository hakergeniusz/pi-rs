//! JavaScript-in-sandbox tool execution ("code mode").
//!
//! Rust port of `@earendil-works/pi-codemode` v1.5.2 (TypeScript ground
//! truth: `packages/codemode/src/index.ts:1-66` and
//! `packages/codemode/src/portable.ts:1-53`). The public surface mirrors the
//! upstream entry points:
//!
//! - [`to_codemode_identifier`] — `identifier.ts:1-12`;
//! - [`declarations`] — `declarations.ts:1-351` (schema → TypeScript types,
//!   tool/global declarations, `renderToolSample`);
//! - [`source`] — `source.ts:1-125` (`// @options:` parsing);
//! - [`types`] — `types.ts:1-254` (tools, results, errors, host calls);
//! - [`protocol`] — `runtime/protocol.ts:1-93` (bridge messages);
//! - [`prelude`] — `runtime/prelude-source.ts:1-524` (the JS bridge evaluated
//!   inside the VM);
//! - [`vm`] / [`sandbox`] — `runtime/vm.ts:1-341`, `runtime/inline.ts:1-126`,
//!   `runtime/execution.ts:1-573` (the engine and host bridge).
//!
//! # Engine choice (deviation)
//!
//! Upstream runs `quickjs-wasi` (QuickJS compiled to wasm) in a Node worker
//! thread, or inline via a wasm module. This port reuses the `rquickjs`
//! (QuickJS-NG) engine already embedded by `pi-ext-js` rather than adding a
//! second wasm runtime. Consequently:
//!
//! - the VM runs synchronously on the calling thread, like upstream's
//!   `InlineCodemodeSandbox` (`runtime/inline.ts:115-126`);
//! - the tool executor is a synchronous trait ([`Tool`]). A host whose tools
//!   are async must adapt them by blocking (see `WIRING.md`);
//! - `CodemodeSandbox` (worker threads), `RemoteCodemodeSandbox` /
//!   `serveCodemodeRemote` (RPC isolates), `CodemodeSandboxBase::start()`
//!   (deterministic replay), and wasm loading have no Rust equivalent here;
//!   their data types are ported so downstream code can compile against them,
//!   but the transports are intentionally omitted (documented in `WIRING.md`).
//!
//! `cargo`-level note: all crates share the workspace toolchain; this crate
//! uses no `unsafe` and stays within the repo's pedantic lint policy.

#![allow(clippy::doc_markdown)] // upstream TS identifiers appear verbatim in module docs

pub mod declarations;
pub mod identifier;
pub mod prelude;
pub mod protocol;
pub mod sandbox;
pub mod source;
pub mod types;
pub mod vm;

pub use declarations::{
    DEFAULT_INPUT_SCHEMA_MAX_CHARS, RenderDeclarationsOptions, RenderTool, RenderToolOptions,
    render_declarations, render_tool_output_type, render_tool_sample, render_tool_signature,
    resolve_schema_ref, schema_to_type,
};
pub use identifier::to_codemode_identifier;
pub use prelude::{
    MAX_OUTPUT_CHARS, MAX_OUTPUT_ITEMS, MAX_STORE_TOTAL_CHARS, MAX_STORE_VALUE_CHARS,
    PRELUDE_SOURCE,
};
pub use protocol::{
    CallTargetKind, DeterministicData, GlobalStartData, HostToWorkerMessage, ToolStartData,
    VmStartData, WorkerToHostMessage, is_host_to_worker_message, is_worker_to_host_message,
};
pub use sandbox::{
    FnTool, InlineCodemodeSandbox, Sandbox, SandboxOptions, Tool, ToolContext, ToolError,
    ToolExecutor, tool,
};
pub use source::{
    CODEMODE_OPTIONS_PREFIX, CODEMODE_SOURCE_GRAMMAR, CodemodeSourceError, CodemodeSourceOptions,
    ParsedCodemodeSource, parse_codemode_source,
};
pub use types::{
    AbortFlag, Call, CallStatus, CodemodeDeclaredGlobal, CodemodeDeclaredTool, CodemodeDuring,
    CodemodeError, CodemodeErrorKind, CodemodeHostCall, CodemodeJsonSchema, CodemodeOutcome,
    CodemodeResult, CodemodeRun, CodemodeRunOptions, CodemodeStoreWrites, ExecuteOptions,
    OutputItem,
};
pub use vm::{DEFAULT_INTERRUPT_BUDGET, VmOptions, run_vm};
