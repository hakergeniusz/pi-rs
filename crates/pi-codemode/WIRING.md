# pi-codemode — host wiring and remaining deviations

This note is the companion to the module `//!` docs. It records what a host
must do to use the crate, what changed in the fidelity slice, and which
upstream mechanisms stay as typed stubs. Ground truth is pi 1.5.2 at
`packages/codemode/src/**` and
`packages/coding-agent/src/extensions/codemode/**`.

## No new dependencies

The crate still depends only on `rquickjs`, `serde`, `serde_json`, and
`thiserror` (all already in `Cargo.lock`). In particular:

- `pi-ai` is **not** a dependency. Its
  `constrained_sampling::{ConstrainedSampling, GrammarVariants}` already exists
  in the workspace, but adding a dependency would require a `Cargo.lock` edit
  that this slice cannot make. [`grammar.rs`](src/grammar.rs) therefore returns
  the exact upstream wire shape as `serde_json::Value`; the host converts it
  with `serde_json::from_value::<pi_ai::ConstrainedSampling>(...)` or constructs
  `pi_ai::ConstrainedSampling::openai_lark(pi_codemode::CODEMODE_SOURCE_GRAMMAR)`
  directly. `pi-coding-agent`'s `ToolDefinition.constrained_sampling` is set by
  that crate, not here — it is still `None` there and wiring it is a
  coordinator-side change.
- No `quickjs-wasi`, worker-thread, or WebAssembly crate is pulled in: the
  `rquickjs` engine is linked natively.

## Host wiring

### `execute()` (unchanged call shape)

`pi-coding-agent/src/codemode_host.rs` constructs `SandboxOptions` literally
and calls `Sandbox::execute(&code, ExecuteOptions { … })`. Neither struct
gained fields in this slice, so that call site still compiles. To enable
streaming, attach a sink with the consuming builder:

```rust
use pi_codemode::{Sandbox, SandboxOptions, UpdateEvent};

let sandbox = Sandbox::new(SandboxOptions {
    tools,
    globals,
    memory_limit_bytes: Some(256 * 1024 * 1024),
    ..SandboxOptions::default()
})?
.with_update_sink(std::sync::Arc::new(|event| match event {
    UpdateEvent::Output(item) => { /* append to the details snapshot */ }
    UpdateEvent::CallStarted { id, name, args, .. } => { /* nested row: running */ }
    UpdateEvent::CallFinished { id, name, status } => { /* nested row: final */ }
    UpdateEvent::Finished { ok } => { /* final publish */ }
}));
```

The sink runs synchronously on the thread executing the script, at the same
points upstream relays `output`/`call` messages and publishes `onUpdate`
(`execute.ts:496-520`, `protocol.ts:43-60`). Keep it non-blocking; the VM is
paused between JS microtasks, not concurrent with the sink.

### Deterministic runs

`Sandbox::start(code, CodemodeRunOptions)` returns a `CodemodeRun` immediately.
The VM runs on an OS thread; every call is forwarded to `on_call`, and the host
answers with `deliver(seq, now, id, outcome)`. `seq` numbers results from 1,
`now` is the virtual clock in ms since the epoch, and results are applied in
`seq` order even when delivered out of order. `wait()` joins the thread and
returns the final `CodemodeResult`; `abort()` unblocks waiting calls and ends
the run with an `aborted` error. `on_started` receives the engine fingerprint,
`on_idle` receives `(seq, issued)` after each applied result.

A minimal loop:

```rust
let (calls_tx, calls_rx) = std::sync::mpsc::channel();
let options = CodemodeRunOptions {
    started_at: 1_700_000_000_000.0,
    seed: 42,
    tools: declared_tools,
    on_call: Some(std::sync::Arc::new(move |call| {
        let _ = calls_tx.send(call.clone());
    })),
    ..CodemodeRunOptions::default()
};
let run = sandbox.start(code, options)?;
let mut seq = 1;
while let Ok(call) = calls_rx.recv() {
    let value = run_host_tool(&call)?;
    run.deliver(seq, started_at + seq as f64, call.id, CodemodeOutcome::Ok { value })?;
    seq += 1;
}
let result = run.wait();
```

The loop terminates when the VM stops issuing calls; a host that abandons the
loop should call `run.abort()` before dropping the receiver, otherwise the VM
thread waits forever (upstream's `result` promise likewise never settles).

### `models.*`

[`models.rs`](src/models.rs) provides `model_globals(Arc<dyn ModelRuntime>)`
(five spread globals with upstream validation/error strings),
`model_global_declarations()`, and `EmptyModelRuntime`. A host with a real
registry implements `ModelRuntime` by delegating to its model catalog;
classifier/image calls and usage/call-row accounting stay host-side (the
trait is synchronous, so an async registry must be adapted by blocking, as
with `Tool`). Add the globals to `SandboxOptions::globals` only when model
access is enabled, matching upstream's `models` option (default on upstream,
off in the current `pi-coding-agent` host).

### Admission

[`admission.rs`](src/admission.rs) exposes `WorkerAdmission` with
`acquire() -> WorkerReservation` and `on_worker_started(WorkerExit)`. The
inline VM uses `NoopWorkerAdmission`. A host that runs its own worker pool can
implement the trait and call it around its launcher.

### Transports

[`transport.rs`](src/transport.rs) provides three local, synchronous
equivalents of upstream's launchers. They share the tool-executor seam, so a
host swaps them without changing its tools:

- `ThreadWorkerTransport::execute(WorkerRequest, &dyn ToolExecutor)` runs one
  VM on an OS thread and relays output and tool calls over channels. The VM
  thread owns the script; the calling thread runs the host tools and posts
  replies — the same split as `CodemodeSandbox` (`host.ts:20-88`,
  `worker.ts:21-33`). It reserves worker capacity through `WorkerAdmission`,
  observes the real thread exit through `onWorkerStarted(WorkerExit)`, and
  returns `Failed to load QuickJS` / `No worker available in …` /
  `Failed to start worker` / `Worker exited with code …` as sandbox errors.
  Supply a header-validated `QuickJsWasm` token via
  `QuickJsWasm::from_bytes` / `WorkerRequest::with_wasm`; without one the
  launch fails with upstream's exact wasm-load error. No WebAssembly
  compilation happens — the token only proves a module was supplied.
- `RemoteCodemodeTransport` implements the client side of
  `RemoteCodemodeSandbox` (`remote.ts:199-277`): it obtains a
  `CodemodeRemote` per execution from an injected factory, answers calls from
  an exchange callback, and turns a reach error, an RPC rejection, or an
  early return into the exact upstream sandbox error. `serve_remote` is the
  server side (`remote.ts:104-197`) and `InProcessRemote` composes the two in
  one process — upstream explicitly allows an in-process remote in tests.
- `load_quickjs_wasm` is the honest "compile the module" API: because there is
  no WebAssembly host in this port, it **fails** for every input with upstream's
  exact `Failed to load QuickJS: …` launcher wrapper instead of retaining bytes
  it can never compile (see deviation 1). `QuickJsWasm::from_bytes` is the
  separate, non-compiling token validator the native-engine transports use.
  `InProcessRemote` and `ThreadWorkerTransport` run the VM with the native
  `rquickjs` engine.

### Host seam (`pi-coding-agent/src/codemode_host.rs`)

Production code reaches the transports through `CodemodeHost`:

- `CodemodeTransport::Inline` (the default) runs `Sandbox::execute` on the
  calling thread — byte-for-byte the behavior before this slice, so nothing
  changes unless a host opts in.
- `CodemodeTransport::ThreadWorker(Arc<ThreadWorkerTransport>)` is selected with
  `CodemodeHost::with_transport(...)`. `execute_code` then builds a
  `WorkerRequest` from the same tools/globals/store/timeout that the inline path
  uses and hands it to `ThreadWorkerTransport::execute`, with the `Sandbox`
  itself acting as the `ToolExecutor`. The resulting `CodemodeResult` flows
  through the *same* formatting, store-write, and nested-call code as the
  inline path. The transport is local and offline (OS thread + channels, native
  engine); no socket or worker runtime is started. The seam is inert by
  default — the coordinator would have to opt in via a settings flag, which is
  deliberately not wired here.
- `RemoteCodemodeTransport` and `serve_remote` stay library APIs: upstream's
  Cloudflare/Workers RPC isolate has no host here, so no production call site
  is wired for them (deviation 1). A test asserts nested calls traverse
  `ThreadWorkerTransport` (via a recording `WorkerAdmission`) and still reach
  the host tool invoker.

## Remaining deviations (deliberate)

1. **Worker threads / Cloudflare / wasm compilation.** The local equivalents
   in `transport.rs` are real thread/channel and injected-client transports,
   but a true Node/Bun `worker_threads` runtime and a Cloudflare Durable Object
   isolate are not hosted: they need a new platform (a worker runtime and an
   RPC isolate this crate cannot provide). A full WebAssembly runtime is
   likewise out of scope: it would require adding a `quickjs-wasi`-style wasm
   module plus a WebAssembly engine dependency and a WASI host to virtualize
   `clock_time_get`/`random_get` (the deterministic clock/RNG shims in
   `vm.rs`). `rquickjs` already links QuickJS natively, so adding a second
   engine would duplicate the VM and change the fingerprint/schema. Rather
   than fake a compiled module, `load_quickjs_wasm` is honest: for *every*
   input it fails with upstream's exact `Failed to load QuickJS: …` launcher
   wrapper (`host.ts:25-27`), because compilation cannot happen here. The
   native-engine transports use the separate `QuickJsWasm::from_bytes` token,
   which validates the `\0asm` header but never claims to compile. The
   `WorkerTransportStub`, `RemoteTransportStub`, `WasmLoaderStub`, and `*_error`
   helpers in `admission.rs` remain for API compatibility and carry upstream's
   exact error strings. The wasm `Stats`/`HMAC`/WHATWG URL shims are likewise
   unported.
2. **Deterministic engine shims.** Upstream virtualizes WASI `clock_time_get`,
   `random_get`, timezone, and removes `WeakRef`/`FinalizationRegistry` at VM
   creation (`vm.ts:103-125,170-180`). `rquickjs` has no WASI layer, so the
   same shape is installed in JS before the prelude freezes the intrinsics:
   seeded `mulberry32` for `Math.random`, a native `__codemodeNow()` behind
   `Date.now`, UTC-by-convention for `toLocaleString` (the host's locale still
   applies for formatters), and deletion of the weak-ref globals. The
   fingerprint names `rquickjs-ng` and `deterministic 1` instead of upstream's
   `quickjs-wasi`/`quickjs` versions. A replay across differing `rquickjs`
   versions is not guaranteed byte-identical.
3. **Async execution.** Upstream tools return promises and the host loop runs
   between calls; this port's `Tool` and `ModelRuntime` are synchronous. Async
   hosts block on their runtime (as `codemode_host.rs::execute_via_executor`
   already does). `onUpdate` is therefore a synchronous callback rather than
   an async `AgentToolResult` publish.
4. **Constrained sampling.** The hook returns the wire JSON; the typed pi-ai
   config and the coding-agent `ToolDefinition` field are wired by the host
   crate, not this one (see above).
5. **`during` / `CodemodeRunResult`.** Upstream reports `{ seq, issued }` on
   the terminal message of a deterministic run and aborts pending calls on
   finish. The host-side state machine tracks `issued` and `seq` for `on_idle`
   and validation, but `CodemodeResult::during` remains `None` for inline
   runs: the in-process driver settles synchronously and has no separate
   terminal message to attach it to. Replaying across a restart requires the
   host to persist the run's `seq`/`issued` itself.
6. **Dynamic `require` / `node_modules` resolution.** Upstream deliberately
   exposes no `require` (or timers, `fetch`, `process`, modules) inside the VM
   (`runtime/execution.ts:451-458`): the script sees only `tools`, globals,
   the output helpers, and `store`/`load`. QuickJS has no CommonJS loader, and
   `rquickjs::Context::full` provides ECMA built-ins only, so there is no
   `require` global to forward. Installing one would break both 1:1 fidelity
   and the sandbox boundary, so dynamic `require` is intentionally not
   ported. A test asserts `typeof require === "undefined"` in the VM. The
   `createRequire` call in upstream's `wasm.ts` is a host-side module resolver
   for the wasm file, not a script-visible API.

Everything else — declarations, source parsing, prelude/bridge marshalling,
tool records, store writes, error kinds, and the `execute()` result schema —
remains as ported before this slice.
