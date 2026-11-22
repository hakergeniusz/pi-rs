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
implement the trait and call it around its launcher; this crate never starts
one thread per execution on its own (the deterministic run's single replay
thread is the exception).

## Remaining deviations (deliberate)

1. **Worker threads / Cloudflare / wasm transports.** `CodemodeSandbox`
   (`node:worker_threads`), `RemoteCodemodeSandbox` / `serveCodemodeRemote`
   (Workers RPC / Durable Objects), `loadQuickJSWasm`, and the wasm `Stats`/
   `HMAC`/WHATWG URL shims have no Rust transport here. `WorkerTransportStub`,
   `RemoteTransportStub`, `WasmLoaderStub`, and the `*_error` helpers in
   `admission.rs` carry upstream's exact error strings so downstream code can
   compile against the upstream API surface. They are data-only: no fake
   transport is invented.
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

Everything else — declarations, source parsing, prelude/bridge marshalling,
tool records, store writes, error kinds, and the `execute()` result schema —
remains as ported before this slice.
