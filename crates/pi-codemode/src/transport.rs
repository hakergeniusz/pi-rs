//! Worker, remote, and wasm transports for the codemode VM.
//!
//! Ground truth:
//! `packages/codemode/src/runtime/host.ts:1-88` (the worker launcher: wasm
//! load, worker admission, `onWorkerStarted`, `postMessage`, termination, and
//! the `Failed to load QuickJS` / `Failed to start worker` /
//! `Worker exited with code …` messages),
//! `packages/codemode/src/runtime/worker.ts:1-39` (the worker entry: start a VM,
//! relay host messages through `deliver`, post a crash on failure),
//! `packages/codemode/src/runtime/remote.ts:52-277` (`CodemodeRemote`,
//! `serveCodemodeRemote`, `remoteLauncher`, and the reach/failure/early-return
//! messages), and `packages/codemode/src/wasm.ts:1-35` (`loadQuickJSWasm`).
//!
//! A real Cloudflare isolate, a Node/Bun worker thread, and a WebAssembly
//! runtime cannot be hosted without a new dependency or platform. What *is*
//! real here:
//!
//! - [`ThreadWorkerTransport`] runs one VM on an OS thread and relays tool
//!   calls and output over channels — the local analogue of `worker.ts`. The
//!   host-side executor stays on the calling thread, exactly as upstream's
//!   `CodemodeSandboxBase` runs tools on the host (`execution.ts:11-22`).
//! - [`RemoteCodemodeTransport`] with [`serve_remote`] runs the same protocol
//!   over an injected [`CodemodeRemote`] trait. An in-process object is a real
//!   remote in upstream too (`remote.ts:70-72`); [`InProcessRemote`] is that
//!   case.
//! - [`load_quickjs_wasm`] validates and retains the module bytes but never
//!   compiles them: `rquickjs` links QuickJS natively. Compiling wasm is a
//!   documented deviation (`WIRING.md`, deviation 1).
//!
//! All three report upstream's exact error strings from [`crate::admission`].
//! The `*_Stub` markers in that module remain for API compatibility; new hosts
//! should use the transports in this module.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use serde_json::json;

use crate::admission::{
    NoopWorkerAdmission, WorkerAdmission, WorkerExit, remote_failure, remote_reach_error,
    remote_returned_early, wasm_load_error, worker_exit_error, worker_start_error,
};
use crate::protocol::{CallTargetKind, HostToWorkerMessage, VmStartData, WorkerToHostMessage};
use crate::sandbox::{ToolError, ToolExecutor};
use crate::stream::{UpdateEvent, UpdateSink};
use crate::types::{
    AbortFlag, Call, CallStatus, CodemodeError, CodemodeErrorKind, CodemodeResult,
    CodemodeStoreWrites, OutputItem, ToolContext,
};
use crate::vm::{
    DEFAULT_INTERRUPT_BUDGET, VmOptions, parse_json_field, parse_script_error, parse_store_writes,
    run_vm,
};

/// Validated bytes of `quickjs-wasi/quickjs.wasm` (`wasm.ts:5-8`). The token is
/// inert: this port links QuickJS natively and never compiles the module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickJsWasm(Vec<u8>);

impl QuickJsWasm {
    /// The retained module bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

/// The local analogue of `loadQuickJSWasm` (`wasm.ts:21-35`).
///
/// Upstream reads and compiles the default wasm file; this port can only read
/// and validate a module a caller supplies. With no module the error is the
/// exact launcher message (`host.ts:25-27`). With a module the bytes are kept
/// but never compiled — see `WIRING.md` deviation 1.
pub fn load_quickjs_wasm(bytes: Option<&[u8]>) -> Result<QuickJsWasm, CodemodeError> {
    let Some(bytes) = bytes else {
        return Err(CodemodeError::sandbox(wasm_load_error(
            "no wasm module provided",
        )));
    };
    if !bytes.starts_with(b"\0asm") {
        return Err(CodemodeError::sandbox(wasm_load_error(
            "invalid WebAssembly module",
        )));
    }
    Ok(QuickJsWasm(bytes.to_vec()))
}

fn sandbox_result(error: CodemodeError) -> CodemodeResult {
    CodemodeResult::Err {
        error,
        output: Vec::new(),
        calls: Vec::new(),
        during: None,
    }
}

fn sandbox_message(message: impl Into<String>) -> CodemodeResult {
    sandbox_result(CodemodeError::sandbox(message))
}

/// `VmStartData.store` holds JSON text (`protocol.ts:20`); `run_vm` needs values.
fn parse_store_values(start: &VmStartData) -> BTreeMap<String, Json> {
    start
        .store
        .iter()
        .map(|(key, text)| {
            (
                key.clone(),
                serde_json::from_str(text).unwrap_or(Json::Null),
            )
        })
        .collect()
}

/// Encode store writes back into the prelude's `[key, json]` / `[key]` wire
/// shape (`protocol.ts:46-48`) for the remote terminal message.
fn encode_store_writes(writes: &CodemodeStoreWrites) -> String {
    let entries: Vec<Json> = writes
        .set
        .iter()
        .map(|(key, value)| {
            json!([
                key,
                serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
            ])
        })
        .chain(writes.delete.iter().map(|key| json!([key])))
        .collect();
    serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string())
}

// ---------------------------------------------------------------------------
// Worker transport (host.ts:20-88, worker.ts:1-39)
// ---------------------------------------------------------------------------

/// One event on the worker's channel. Upstream sends only `WorkerToHostMessage`
/// values (`worker.ts:17-19`); this port adds a terminal variant carrying the
/// already-decoded [`CodemodeResult`] so the host does not repeat the bridge
/// decoding the VM driver already performed.
#[derive(Debug)]
enum WorkerEvent {
    /// A VM→host bridge message, relayed verbatim.
    Message(WorkerToHostMessage),
    /// The VM driver finished; carries the structured result.
    Finished(CodemodeResult),
}

/// Owned request for one worker execution (`protocol.ts:30-38` plus the host
/// limits in `inline.ts:44-55`).
#[derive(Clone)]
pub struct WorkerRequest {
    /// The script, its tools, globals, memory limit, and `load()` snapshot.
    pub start: VmStartData,
    /// Overall deadline; `None` uses the upstream default.
    pub timeout_ms: Option<u64>,
    /// Interrupt polls before a synchronous script times out.
    pub interrupt_budget: u64,
    /// Caller's abort flag, shared with the worker's executor.
    pub signal: AbortFlag,
    /// A validated wasm token. `None` fails with upstream's wasm-load error.
    pub wasm: Option<QuickJsWasm>,
    /// Receives output relayed from the worker thread.
    pub update_sink: Option<UpdateSink>,
}

impl WorkerRequest {
    /// Build a request for `code` with upstream's default limits.
    #[must_use]
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            start: VmStartData::new(code),
            timeout_ms: None,
            interrupt_budget: DEFAULT_INTERRUPT_BUDGET,
            signal: AbortFlag::new(),
            wasm: None,
            update_sink: None,
        }
    }

    /// Attach a validated wasm token, enabling the worker launch.
    #[must_use]
    pub fn with_wasm(mut self, wasm: QuickJsWasm) -> Self {
        self.wasm = Some(wasm);
        self
    }
}

/// The worker thread's tool bridge. Each call is posted to the host and then
/// blocks on the reply channel — the same shape as `worker.ts:24-32`, where the
/// VM waits for `deliver()` from the host.
struct RelayExecutor {
    messages: Mutex<Sender<WorkerEvent>>,
    replies: Receiver<HostToWorkerMessage>,
    signal: AbortFlag,
}

impl ToolExecutor for RelayExecutor {
    fn call(
        &self,
        id: u32,
        target: CallTargetKind,
        name: &str,
        args: Json,
        _context: &ToolContext,
    ) -> Result<Json, ToolError> {
        let payload = serde_json::to_string(&args).unwrap_or_else(|_| "null".to_string());
        {
            let messages = self
                .messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if messages
                .send(WorkerEvent::Message(WorkerToHostMessage::Call {
                    id,
                    target,
                    name: name.to_string(),
                    args: Some(payload),
                }))
                .is_err()
            {
                return Err(ToolError::new("Execution aborted"));
            }
        }
        loop {
            if self.signal.is_aborted() {
                return Err(ToolError::new("Execution aborted"));
            }
            match self.replies.recv_timeout(Duration::from_millis(20)) {
                Ok(reply) => {
                    if reply.ok {
                        return Ok(reply
                            .payload
                            .and_then(|text| serde_json::from_str(&text).ok())
                            .unwrap_or(Json::Null));
                    }
                    let mut error = ToolError::new(reply.payload.unwrap_or_default());
                    if let Some(properties) = reply.properties
                        && let Ok(map) =
                            serde_json::from_str::<serde_json::Map<String, Json>>(&properties)
                    {
                        error.properties = Some(map.into_iter().collect());
                    }
                    return Err(error);
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(ToolError::new("Execution aborted"));
                }
            }
        }
    }
}

/// Sends the worker-exit signal even when the thread unwinds, so
/// `onWorkerStarted` observes a real exit (`types.ts:156-162`).
struct ExitSignal(Sender<()>);

impl Drop for ExitSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Local analogue of upstream's `CodemodeSandbox` (`host.ts:77-88`). Each
/// execution gets its own thread and VM; a script spinning in JS cannot block
/// the calling thread.
pub struct ThreadWorkerTransport {
    admission: Arc<dyn WorkerAdmission>,
}

impl Default for ThreadWorkerTransport {
    fn default() -> Self {
        Self::new(Arc::new(NoopWorkerAdmission))
    }
}

impl ThreadWorkerTransport {
    /// Use `admission` around every worker launch (`types.ts:134-163`).
    #[must_use]
    pub fn new(admission: Arc<dyn WorkerAdmission>) -> Self {
        Self { admission }
    }

    /// Run one script on a worker thread, answering its tool calls on this
    /// thread. Transport and script failures both come back as a
    /// [`CodemodeResult`] (`execution.ts:11-22`), never as a Rust error.
    #[must_use]
    #[allow(clippy::too_many_lines)] // mirrors host.ts:20-74; split when extending
    pub fn execute(&self, request: WorkerRequest, executor: &dyn ToolExecutor) -> CodemodeResult {
        // host.ts:22-27 — validate the wasm before reserving worker capacity.
        if let Err(error) = load_quickjs_wasm(request.wasm.as_ref().map(QuickJsWasm::bytes)) {
            return sandbox_result(error);
        }

        // host.ts:30 — reserve capacity; a denial becomes a sandbox error.
        let mut reservation = match self.admission.acquire() {
            Ok(reservation) => reservation,
            Err(message) => return sandbox_message(message),
        };

        let (messages_tx, messages_rx) = mpsc::channel::<WorkerEvent>();
        let (replies_tx, replies_rx) = mpsc::channel::<HostToWorkerMessage>();
        let (exit_tx, exit_rx) = mpsc::channel::<()>();

        // host.ts:67-72 — a throwing `onWorkerStarted` fails the execution and
        // requests termination; `catch_unwind` is the synchronous Rust analogue.
        if catch_unwind(AssertUnwindSafe(|| {
            self.admission.on_worker_started(WorkerExit::new(exit_rx));
        }))
        .is_err()
        {
            request.signal.abort();
            return sandbox_message("onWorkerStarted failed");
        }

        let host_signal = request.signal.clone();
        let host_sink = request.update_sink.clone();
        let relay = RelayExecutor {
            messages: Mutex::new(messages_tx.clone()),
            replies: replies_rx,
            signal: host_signal.clone(),
        };
        // Relay output to the host through the same channel (`worker.ts:17-19`).
        let sink_sender = Arc::new(Mutex::new(messages_tx.clone()));
        let relay_sink: UpdateSink = Arc::new(move |event| {
            if let UpdateEvent::Output(item) = event {
                let guard = sink_sender
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = guard.send(WorkerEvent::Message(WorkerToHostMessage::Output { item }));
            }
        });
        let update_sink = host_sink.clone().or(Some(relay_sink));
        let worker_signal = host_signal.clone();
        let worker = thread::Builder::new()
            .name("pi-codemode-worker".to_string())
            .spawn(move || {
                let _exit = ExitSignal(exit_tx);
                let store = parse_store_values(&request.start);
                let vm_options = VmOptions {
                    code: &request.start.code,
                    tools: &request.start.tools,
                    globals: &request.start.globals,
                    memory_limit_bytes: request.start.memory_limit_bytes,
                    store: &store,
                    timeout_ms: request.timeout_ms,
                    interrupt_budget: request.interrupt_budget,
                    signal: Some(worker_signal),
                    deterministic: request.start.deterministic,
                    clock: None,
                    update_sink,
                };
                let result = run_vm(&vm_options, &relay);
                let _ = messages_tx.send(WorkerEvent::Finished(result));
            });
        // host.ts:31-37 — a spawn failure is upstream's `Failed to start worker`.
        let worker: JoinHandle<()> = match worker {
            Ok(worker) => worker,
            Err(error) => return sandbox_message(worker_start_error(&error.to_string())),
        };

        // host.ts:39-58 — relay VM messages and answer tool calls on the host.
        let mut terminal = None;
        for event in messages_rx {
            match event {
                WorkerEvent::Finished(result) => {
                    terminal = Some(result);
                    break;
                }
                WorkerEvent::Message(message) => match message {
                    WorkerToHostMessage::Call {
                        id,
                        target,
                        name,
                        args,
                    } => {
                        let arguments = serde_json::from_str(args.as_deref().unwrap_or("null"))
                            .unwrap_or(Json::Null);
                        let context = ToolContext {
                            signal: host_signal.clone(),
                        };
                        let reply = match executor.call(id, target, &name, arguments, &context) {
                            Ok(value) => HostToWorkerMessage::new(
                                id,
                                true,
                                serde_json::to_string(&value).ok(),
                            ),
                            Err(error) => {
                                let mut reply =
                                    HostToWorkerMessage::new(id, false, Some(error.message));
                                reply.properties = error.properties.and_then(|properties| {
                                    serde_json::to_string(&Json::Object(
                                        properties.into_iter().collect(),
                                    ))
                                    .ok()
                                });
                                reply
                            }
                        };
                        let _ = replies_tx.send(reply);
                    }
                    WorkerToHostMessage::Output { item } => {
                        if let Some(sink) = &host_sink {
                            sink(UpdateEvent::Output(item));
                        }
                    }
                    WorkerToHostMessage::DoneOk { .. }
                    | WorkerToHostMessage::DoneError { .. }
                    | WorkerToHostMessage::Crash { .. }
                    | WorkerToHostMessage::Started { .. }
                    | WorkerToHostMessage::Idle { .. } => {}
                },
            }
        }
        drop(replies_tx);
        // host.ts:56-58 — a worker that ends before the script settled is a
        // sandbox failure; a panicking thread has exit code 1 in this port.
        let result = match terminal {
            Some(result) => {
                let _ = worker.join();
                result
            }
            None => sandbox_message(worker_exit_error(i32::from(worker.join().is_err()))),
        };
        reservation.release();
        result
    }
}

// ---------------------------------------------------------------------------
// Remote transport (remote.ts:52-277, cloudflare.ts:61-75)
// ---------------------------------------------------------------------------

/// The client's `exchange` callback (`remote.ts:67`). Owned so both the VM
/// bridge and the driver can invoke it; upstream's promise becomes a
/// synchronous result because the whole VM in this crate is synchronous.
pub type RemoteExchange =
    Arc<dyn Fn(&[RemoteMessage]) -> Result<RemoteReply, CodemodeError> + Send + Sync>;

/// One request across the RPC boundary (`remote.ts:52-56`).
#[derive(Debug, Clone)]
pub struct RemoteRequest {
    /// Script data, identical to a worker's start data.
    pub start: VmStartData,
    /// Requested interrupt budget; the server clamps it.
    pub interrupt_budget: Option<u64>,
}

impl RemoteRequest {
    /// Build a request for `code` with no budget override.
    #[must_use]
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            start: VmStartData::new(code),
            interrupt_budget: None,
        }
    }
}

/// A message from the remote to the client (`remote.ts:59`).
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteMessage {
    /// A VM bridge message.
    Vm(WorkerToHostMessage),
    /// The VM or transport failed outside the bridge.
    Failure(CodemodeError),
}

/// The client's answer to an exchange (`remote.ts:65`).
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteReply {
    /// The answer to the call in the batch.
    Result(HostToWorkerMessage),
    /// The client no longer wants the execution (deadline, abort, or close).
    Stop,
    /// The batch carried no call.
    None,
}

/// The remote end of one execution (`remote.ts:73-75`). Anything implementing
/// this trait works; over Workers RPC it would be a Durable Object stub, here
/// it is injected by the host.
pub trait CodemodeRemote: Send + Sync {
    /// Run one execution, relaying messages through `exchange`. Resolves when
    /// the script settled or was stopped; never rejects on script errors.
    fn execute(
        &self,
        request: RemoteRequest,
        exchange: &RemoteExchange,
    ) -> Result<(), CodemodeError>;
}

/// Server-side limits (`remote.ts:91-97`).
#[derive(Debug, Clone)]
pub struct RemoteServeOptions {
    /// Upper bound for the requested budget, and the value when none is named.
    pub max_interrupt_budget: u64,
    /// Upper bound for the VM memory limit.
    pub max_memory_limit_bytes: Option<u64>,
}

impl Default for RemoteServeOptions {
    fn default() -> Self {
        Self {
            max_interrupt_budget: DEFAULT_INTERRUPT_BUDGET,
            max_memory_limit_bytes: None,
        }
    }
}

/// Output is buffered before an exchange (`remote.ts:88-89`).
const FLUSH_ITEMS: usize = 1_000;
const FLUSH_CHARS: usize = 1 << 20;

#[derive(Default)]
struct RemoteBuffer {
    messages: Vec<RemoteMessage>,
    chars: usize,
}

impl RemoteBuffer {
    fn take(&mut self) -> Vec<RemoteMessage> {
        self.chars = 0;
        std::mem::take(&mut self.messages)
    }
}

/// The remote thread's tool bridge: every call and buffered output goes
/// through the client's `exchange` (`remote.ts:145-177,204-227`).
struct RemoteExecutor {
    buffer: Arc<Mutex<RemoteBuffer>>,
    exchange: RemoteExchange,
    stop: AbortFlag,
}

impl ToolExecutor for RemoteExecutor {
    fn call(
        &self,
        id: u32,
        target: CallTargetKind,
        name: &str,
        args: Json,
        _context: &ToolContext,
    ) -> Result<Json, ToolError> {
        let payload = serde_json::to_string(&args).unwrap_or_else(|_| "null".to_string());
        {
            let mut guard = self
                .buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .messages
                .push(RemoteMessage::Vm(WorkerToHostMessage::Call {
                    id,
                    target,
                    name: name.to_string(),
                    args: Some(payload),
                }));
        }
        let batch = self
            .buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match (self.exchange)(&batch) {
            Ok(RemoteReply::Result(reply)) => {
                if reply.ok {
                    Ok(reply
                        .payload
                        .and_then(|text| serde_json::from_str(&text).ok())
                        .unwrap_or(Json::Null))
                } else {
                    let mut error = ToolError::new(reply.payload.unwrap_or_default());
                    if let Some(properties) = reply.properties
                        && let Ok(map) =
                            serde_json::from_str::<serde_json::Map<String, Json>>(&properties)
                    {
                        error.properties = Some(map.into_iter().collect());
                    }
                    Err(error)
                }
            }
            Ok(RemoteReply::Stop) | Ok(RemoteReply::None) | Err(_) => {
                self.stop.abort();
                Err(ToolError::new("Execution aborted"))
            }
        }
    }
}

/// Server side of the remote transport (`remote.ts:104-197`). Runs one VM on
/// this thread and forwards output, calls, and the terminal message through
/// `exchange`. It never rejects: a failed exchange stops the VM, and the
/// client decides how to report that.
///
/// # Errors
///
/// Returns `Err` only when no usable VM can be created at all (the launcher
/// failure upstream reports as a `failure` message). Script errors and
/// transport stops are delivered through `exchange`.
#[allow(clippy::too_many_lines)] // mirrors serveCodemodeRemote; split when extending
pub fn serve_remote(
    request: RemoteRequest,
    exchange: &RemoteExchange,
    options: RemoteServeOptions,
) -> Result<(), CodemodeError> {
    let budget = request
        .interrupt_budget
        .unwrap_or(options.max_interrupt_budget)
        .min(options.max_interrupt_budget);
    let memory_limit_bytes = match (
        request.start.memory_limit_bytes,
        options.max_memory_limit_bytes,
    ) {
        (None, max) => max,
        (Some(wanted), None) => Some(wanted),
        (Some(wanted), Some(max)) => Some(wanted.min(max)),
    };
    let store = parse_store_values(&request.start);
    let buffer = Arc::new(Mutex::new(RemoteBuffer::default()));
    let stop = AbortFlag::new();

    // Buffer output and flush it with the next call or terminal message
    // (`remote.ts:158-165`). The Arc is shared with the exchange callbacks.
    let sink_buffer = Arc::clone(&buffer);
    let sink_exchange = Arc::clone(exchange);
    let sink_stop = stop.clone();
    let sink: UpdateSink = Arc::new(move |event| {
        let UpdateEvent::Output(item) = event else {
            return;
        };
        let mut guard = sink_buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.chars += match &item {
            OutputItem::Text { text, .. } => text.len(),
            OutputItem::Image { data, .. } => data.len(),
        };
        guard
            .messages
            .push(RemoteMessage::Vm(WorkerToHostMessage::Output { item }));
        if guard.messages.len() >= FLUSH_ITEMS || guard.chars >= FLUSH_CHARS {
            let batch = guard.take();
            drop(guard);
            if !matches!(
                sink_exchange(&batch),
                Ok(RemoteReply::Result(_)) | Ok(RemoteReply::None)
            ) {
                sink_stop.abort();
            }
        }
    });

    let executor = RemoteExecutor {
        buffer: Arc::clone(&buffer),
        exchange: Arc::clone(exchange),
        stop: stop.clone(),
    };
    let vm_options = VmOptions {
        code: &request.start.code,
        tools: &request.start.tools,
        globals: &request.start.globals,
        memory_limit_bytes,
        store: &store,
        timeout_ms: None,
        interrupt_budget: budget,
        signal: Some(stop.clone()),
        deterministic: request.start.deterministic,
        clock: None,
        update_sink: Some(sink),
    };
    let result = run_vm(&vm_options, &executor);

    // remote.ts:135-144 — deliver the terminal message with any buffered
    // output. A script error travels as `done`; a sandbox error as `failure`.
    if stop.is_aborted() {
        return Ok(());
    }
    let terminal = match result {
        CodemodeResult::Ok {
            value,
            store_writes,
            during,
            ..
        } => RemoteMessage::Vm(WorkerToHostMessage::DoneOk {
            value: serde_json::to_string(&value).ok(),
            writes: encode_store_writes(&store_writes),
            during,
        }),
        CodemodeResult::Err { error, .. } if error.kind == CodemodeErrorKind::Sandbox => {
            RemoteMessage::Failure(error)
        }
        CodemodeResult::Err { error, during, .. } => {
            let encoded = json!({
                "name": error.name,
                "message": error.message,
                "stack": error.stack,
            });
            RemoteMessage::Vm(WorkerToHostMessage::DoneError {
                error: serde_json::to_string(&encoded).unwrap_or_else(|_| "{}".to_string()),
                during,
            })
        }
    };
    let mut guard = buffer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut batch = guard.take();
    drop(guard);
    batch.push(terminal);
    let _ = exchange(&batch);
    Ok(())
}

/// A remote that runs the VM in this process through [`serve_remote`].
/// Upstream explicitly allows "an in-process object in tests"
/// (`remote.ts:70-72`); this makes that object a first-class transport.
pub struct InProcessRemote {
    options: RemoteServeOptions,
}

impl InProcessRemote {
    /// Use explicit server limits.
    #[must_use]
    pub fn new(options: RemoteServeOptions) -> Self {
        Self { options }
    }
}

impl Default for InProcessRemote {
    fn default() -> Self {
        Self::new(RemoteServeOptions::default())
    }
}

impl CodemodeRemote for InProcessRemote {
    fn execute(
        &self,
        request: RemoteRequest,
        exchange: &RemoteExchange,
    ) -> Result<(), CodemodeError> {
        serve_remote(request, exchange, self.options.clone())
    }
}

/// Returns the remote used for one execution (`remote.ts:77-85`).
pub type RemoteFactory = Arc<dyn Fn() -> Result<Arc<dyn CodemodeRemote>, String> + Send + Sync>;

/// Local analogue of upstream's `RemoteCodemodeSandbox` (`remote.ts:266-277`):
/// tools and call records stay on the client while the VM runs behind an
/// injected remote.
pub struct RemoteCodemodeTransport {
    remote: RemoteFactory,
}

impl RemoteCodemodeTransport {
    /// Create a client that obtains its remote from `remote` per execution.
    #[must_use]
    pub fn new(remote: RemoteFactory) -> Self {
        Self { remote }
    }

    /// Run one execution with no external stop signal.
    #[must_use]
    pub fn execute(&self, request: RemoteRequest, executor: &dyn ToolExecutor) -> CodemodeResult {
        self.execute_with_stop(request, executor, AbortFlag::new())
    }

    /// Run one execution; `stop` makes the client answer every later exchange
    /// with `RemoteReply::Stop` (upstream's deadline/abort/close path).
    #[must_use]
    #[allow(clippy::too_many_lines)] // mirrors remoteLauncher's exchange; split when extending
    pub fn execute_with_stop(
        &self,
        request: RemoteRequest,
        executor: &dyn ToolExecutor,
        stop: AbortFlag,
    ) -> CodemodeResult {
        let output = Mutex::new(Vec::<OutputItem>::new());
        let calls = Mutex::new(Vec::<Call>::new());
        let terminal = Mutex::new(Option::<CodemodeResult>::None);

        let exchange: RemoteExchange = Arc::new(move |messages| {
            if stop.is_aborted()
                || terminal
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_some()
            {
                return Ok(RemoteReply::Stop);
            }
            let mut answer = None;
            for message in messages {
                match message {
                    RemoteMessage::Vm(WorkerToHostMessage::Output { item }) => {
                        output
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(item.clone());
                    }
                    RemoteMessage::Vm(WorkerToHostMessage::Call {
                        id,
                        target,
                        name,
                        args,
                    }) => {
                        let arguments = serde_json::from_str(args.as_deref().unwrap_or("null"))
                            .unwrap_or(Json::Null);
                        let context = ToolContext {
                            signal: stop.clone(),
                        };
                        let started = Instant::now();
                        let reply = match executor.call(*id, *target, name, arguments, &context) {
                            Ok(value) => HostToWorkerMessage::new(
                                *id,
                                true,
                                serde_json::to_string(&value).ok(),
                            ),
                            Err(error) => {
                                let mut reply =
                                    HostToWorkerMessage::new(*id, false, Some(error.message));
                                reply.properties = error.properties.and_then(|properties| {
                                    serde_json::to_string(&Json::Object(
                                        properties.into_iter().collect(),
                                    ))
                                    .ok()
                                });
                                reply
                            }
                        };
                        let status = if reply.ok {
                            CallStatus::Ok
                        } else {
                            CallStatus::Error
                        };
                        calls
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(Call {
                                name: name.clone(),
                                status,
                                duration_ms: started.elapsed().as_secs_f64() * 1000.0,
                            });
                        answer = Some(RemoteReply::Result(reply));
                    }
                    RemoteMessage::Vm(WorkerToHostMessage::DoneOk {
                        value,
                        writes,
                        during,
                    }) => {
                        let parsed =
                            parse_json_field(value.as_deref(), "return value").and_then(|value| {
                                parse_store_writes(writes).map(|writes| (value, writes))
                            });
                        let (value, store_writes) = match parsed {
                            Ok(parsed) => parsed,
                            Err(message) => {
                                *terminal
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(sandbox_message(message));
                                continue;
                            }
                        };
                        let items = std::mem::take(
                            &mut *output
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        );
                        let recorded = std::mem::take(
                            &mut *calls
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        );
                        *terminal
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(CodemodeResult::Ok {
                                value,
                                output: items,
                                calls: recorded,
                                store_writes,
                                during: *during,
                            });
                    }
                    RemoteMessage::Vm(WorkerToHostMessage::DoneError { error, during }) => {
                        let error = parse_script_error(error).unwrap_or_else(|message| {
                            CodemodeError::sandbox(format!("Sandbox bridge broken: {message}"))
                        });
                        let items = std::mem::take(
                            &mut *output
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        );
                        let recorded = std::mem::take(
                            &mut *calls
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        );
                        *terminal
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(CodemodeResult::Err {
                                error,
                                output: items,
                                calls: recorded,
                                during: *during,
                            });
                    }
                    RemoteMessage::Vm(WorkerToHostMessage::Crash { message }) => {
                        let items = std::mem::take(
                            &mut *output
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        );
                        let recorded = std::mem::take(
                            &mut *calls
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        );
                        *terminal
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(CodemodeResult::Err {
                                error: CodemodeError::sandbox(message),
                                output: items,
                                calls: recorded,
                                during: None,
                            });
                    }
                    RemoteMessage::Failure(error) => {
                        let items = std::mem::take(
                            &mut *output
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        );
                        let recorded = std::mem::take(
                            &mut *calls
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        );
                        *terminal
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(CodemodeResult::Err {
                                error: error.clone(),
                                output: items,
                                calls: recorded,
                                during: None,
                            });
                    }
                    RemoteMessage::Vm(WorkerToHostMessage::Started { .. })
                    | RemoteMessage::Vm(WorkerToHostMessage::Idle { .. }) => {}
                }
            }
            Ok(answer.unwrap_or(RemoteReply::None))
        });

        // remote.ts:229-234 — reaching the remote can fail before any RPC.
        let target = match (self.remote)() {
            Ok(target) => target,
            Err(cause) => return sandbox_message(remote_reach_error(&cause)),
        };
        if let Err(cause) = target.execute(request, &exchange)
            && terminal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
        {
            // remote.ts:244-247 — the RPC rejection becomes a sandbox error.
            // Upstream's `errorMessage` uses `error.message`, not the `name`.
            return sandbox_message(remote_failure(&cause.message));
        }
        terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            // remote.ts:239-242 — no terminal message reached the client.
            .unwrap_or_else(|| sandbox_message(remote_returned_early()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ToolStartData;
    use crate::sandbox::tool;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const WASM: &[u8] = b"\0asm\x01\0\0\0";

    fn wasm() -> QuickJsWasm {
        load_quickjs_wasm(Some(WASM)).expect("valid wasm header")
    }

    fn double_tool() -> Arc<dyn ToolExecutor> {
        let tool = tool("double", |args, _| {
            let number = args.get("n").and_then(Json::as_i64).unwrap_or(0);
            Ok(json!({ "value": number * 2 }))
        });
        // `tool()` is a host `Tool`; wrap it so the transports' executor seam
        // is exercised with the same call signature as `Sandbox`.
        struct Adapter(Arc<dyn crate::sandbox::Tool>);
        impl ToolExecutor for Adapter {
            fn call(
                &self,
                _id: u32,
                target: CallTargetKind,
                name: &str,
                args: Json,
                context: &ToolContext,
            ) -> Result<Json, ToolError> {
                if target != CallTargetKind::Tool {
                    return Err(ToolError::new("unexpected global"));
                }
                if name != "double" {
                    return Err(ToolError::new(format!("Unknown tool \"{name}\"")));
                }
                crate::sandbox::Tool::execute(self.0.as_ref(), args, context)
            }
        }
        Arc::new(Adapter(tool))
    }

    fn worker_request() -> WorkerRequest {
        let mut request = WorkerRequest::new(
            "const r = await tools.double({ n: 21 }); text(JSON.stringify(r)); return r.value;",
        )
        .with_wasm(wasm());
        request.start.tools = vec![ToolStartData {
            name: "double".to_string(),
            js_name: "double".to_string(),
            description: String::new(),
        }];
        request
    }

    #[test]
    fn wasm_loader_reports_upstream_error_without_a_module() {
        let error = load_quickjs_wasm(None).expect_err("no module must fail");
        assert_eq!(error.kind, CodemodeErrorKind::Sandbox);
        assert_eq!(
            error.message,
            "Failed to load QuickJS: no wasm module provided"
        );
    }

    #[test]
    fn wasm_loader_validates_the_magic_header() {
        let error = load_quickjs_wasm(Some(b"nope")).expect_err("invalid module");
        assert_eq!(
            error.message,
            "Failed to load QuickJS: invalid WebAssembly module"
        );
        let module = load_quickjs_wasm(Some(WASM)).expect("valid module");
        assert_eq!(module.bytes(), WASM);
    }

    #[test]
    fn worker_transport_relays_calls_and_returns_the_result() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&observed);
        let mut request = worker_request();
        request.update_sink = Some(Arc::new(move |event| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }));
        let transport = ThreadWorkerTransport::default();
        let result = transport.execute(request, double_tool().as_ref());
        assert!(result.is_ok(), "{result:?}");
        match result {
            CodemodeResult::Ok {
                value,
                output,
                calls,
                ..
            } => {
                assert_eq!(value, json!(42));
                assert_eq!(output, vec![OutputItem::text(r#"{"value":42}"#)]);
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "double");
                assert_eq!(calls[0].status, CallStatus::Ok);
            }
            CodemodeResult::Err { .. } => unreachable!(),
        }
        let events = observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, UpdateEvent::Output(OutputItem::Text { text, .. }) if text == r#"{"value":42}"#))
        );
    }

    #[test]
    fn worker_transport_reports_missing_wasm_with_upstream_error() {
        let mut request = worker_request();
        request.wasm = None;
        let result = ThreadWorkerTransport::default().execute(request, double_tool().as_ref());
        let error = result.error().expect("must fail");
        assert_eq!(error.kind, CodemodeErrorKind::Sandbox);
        assert_eq!(
            error.message,
            "Failed to load QuickJS: no wasm module provided"
        );
    }

    #[test]
    fn worker_transport_honors_admission_denial() {
        struct Deny;
        impl WorkerAdmission for Deny {
            fn acquire(&self) -> Result<crate::admission::WorkerReservation, String> {
                Err(crate::admission::admission_denied("codemode-pool"))
            }
        }
        let transport = ThreadWorkerTransport::new(Arc::new(Deny));
        let result = transport.execute(worker_request(), double_tool().as_ref());
        assert_eq!(
            result.error().map_or("", |error| error.message.as_str()),
            "No worker available in codemode-pool"
        );
    }

    #[test]
    fn worker_transport_observes_exit_and_releases_the_reservation() {
        struct Pool {
            released: Arc<AtomicUsize>,
            exited: Arc<Mutex<Option<WorkerExit>>>,
        }
        impl WorkerAdmission for Pool {
            fn acquire(&self) -> Result<crate::admission::WorkerReservation, String> {
                let released = Arc::clone(&self.released);
                Ok(crate::admission::WorkerReservation::new(move || {
                    released.fetch_add(1, Ordering::SeqCst);
                }))
            }
            fn on_worker_started(&self, exited: WorkerExit) {
                *self
                    .exited
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(exited);
            }
        }
        let exited = Arc::new(Mutex::new(None));
        let pool = Arc::new(Pool {
            released: Arc::new(AtomicUsize::new(0)),
            exited: Arc::clone(&exited),
        });
        let transport = ThreadWorkerTransport::new(pool.clone());
        let result = transport.execute(worker_request(), double_tool().as_ref());
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(pool.released.load(Ordering::SeqCst), 1);
        // `WorkerExit::wait` consumes the receiver, so take it out of the
        // mutex. The worker already exited, so this returns immediately.
        let exit = exited
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("onWorkerStarted was called");
        exit.wait();
    }

    fn remote_request() -> RemoteRequest {
        let mut request = RemoteRequest::new(
            "text('start'); const r = await tools.double({ n: 4 }); return r.value;",
        );
        request.start.tools = vec![ToolStartData {
            name: "double".to_string(),
            js_name: "double".to_string(),
            description: String::new(),
        }];
        request
    }

    fn in_process_client() -> RemoteCodemodeTransport {
        RemoteCodemodeTransport::new(Arc::new(|| Ok(Arc::new(InProcessRemote::default()))))
    }

    #[test]
    fn remote_transport_runs_a_script_through_an_in_process_remote() {
        let result = in_process_client().execute(remote_request(), double_tool().as_ref());
        assert!(result.is_ok(), "{result:?}");
        match result {
            CodemodeResult::Ok {
                value,
                output,
                calls,
                ..
            } => {
                assert_eq!(value, json!(8));
                assert_eq!(output, vec![OutputItem::text("start")]);
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "double");
                assert_eq!(calls[0].status, CallStatus::Ok);
            }
            CodemodeResult::Err { .. } => unreachable!(),
        }
    }

    #[test]
    fn remote_transport_reports_reach_failure_with_upstream_string() {
        let client = RemoteCodemodeTransport::new(Arc::new(|| Err("MISSING_BINDING".to_string())));
        let result = client.execute(remote_request(), double_tool().as_ref());
        assert_eq!(
            result.error().map_or("", |error| error.message.as_str()),
            "Failed to reach the remote sandbox: MISSING_BINDING"
        );
    }

    #[test]
    fn remote_transport_reports_remote_failure_with_upstream_string() {
        struct Broken;
        impl CodemodeRemote for Broken {
            fn execute(
                &self,
                _request: RemoteRequest,
                _exchange: &RemoteExchange,
            ) -> Result<(), CodemodeError> {
                Err(CodemodeError::sandbox("OOM"))
            }
        }
        let client = RemoteCodemodeTransport::new(Arc::new(|| Ok(Arc::new(Broken))));
        let result = client.execute(remote_request(), double_tool().as_ref());
        assert_eq!(
            result.error().map_or("", |error| error.message.as_str()),
            "Remote sandbox failed: OOM"
        );
    }

    #[test]
    fn remote_transport_reports_early_return_with_upstream_string() {
        struct Silent;
        impl CodemodeRemote for Silent {
            fn execute(
                &self,
                _request: RemoteRequest,
                _exchange: &RemoteExchange,
            ) -> Result<(), CodemodeError> {
                Ok(())
            }
        }
        let client = RemoteCodemodeTransport::new(Arc::new(|| Ok(Arc::new(Silent))));
        let result = client.execute(remote_request(), double_tool().as_ref());
        assert_eq!(
            result.error().map_or("", |error| error.message.as_str()),
            "Remote sandbox returned before the script settled"
        );
    }

    #[test]
    fn remote_transport_forwards_a_stop_reply() {
        struct Stopping(Arc<Mutex<Vec<RemoteReply>>>);
        impl CodemodeRemote for Stopping {
            fn execute(
                &self,
                _request: RemoteRequest,
                exchange: &RemoteExchange,
            ) -> Result<(), CodemodeError> {
                let reply = exchange(&[RemoteMessage::Vm(WorkerToHostMessage::Call {
                    id: 1,
                    target: CallTargetKind::Tool,
                    name: "double".to_string(),
                    args: Some("{\"n\":3}".to_string()),
                })]);
                if let Ok(reply) = reply {
                    self.0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(reply);
                }
                Ok(())
            }
        }
        let replies = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&replies);
        let client = RemoteCodemodeTransport::new(Arc::new(move || {
            Ok(Arc::new(Stopping(Arc::clone(&seen))))
        }));
        let stop = AbortFlag::new();
        stop.abort();
        let result = client.execute_with_stop(remote_request(), double_tool().as_ref(), stop);
        let replies = replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(matches!(replies.as_slice(), [RemoteReply::Stop]));
        assert_eq!(
            result.error().map_or("", |error| error.message.as_str()),
            "Remote sandbox returned before the script settled"
        );
    }

    #[test]
    fn remote_serve_delivers_a_terminal_message() {
        let mut request = RemoteRequest::new("return 1;");
        request.interrupt_budget = Some(500_000);
        request.start.memory_limit_bytes = Some(512 * 1024 * 1024);
        // The server clamps both values to these maxima (`remote.ts:109-119`).
        let options = RemoteServeOptions {
            max_interrupt_budget: 100_000,
            max_memory_limit_bytes: Some(1024 * 1024 * 1024),
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_by_exchange = Arc::clone(&seen);
        let exchange: RemoteExchange = Arc::new(move |messages| {
            seen_by_exchange
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(messages);
            Ok(RemoteReply::None)
        });
        serve_remote(request, &exchange, options).expect("serve");
        let seen = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(seen.iter().any(|message| matches!(
            message,
            RemoteMessage::Vm(WorkerToHostMessage::DoneOk { .. })
        )));
    }
}
