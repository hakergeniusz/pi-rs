//! Deterministic `start()` / `deliver()` runs of the inline VM.
//!
//! Ground truth:
//! `packages/codemode/src/runtime/execution.ts:519-566` (`start`: validation,
//! declared tools/globals, `CodemodeRun`), `:260-298` (`deliver`: finite clock,
//! once-only result numbers, one answer per pending call, JSON serialization),
//! `:367-376` (forwarding calls to the host), and
//! `packages/codemode/src/runtime/vm.ts:23-36,183-233,314-336` (virtual clock,
//! seeded PRNG, in-order delivery, `started`/`idle` messages, fingerprint).
//!
//! The inline engine is `rquickjs`, not `quickjs-wasi`, so WASI clock/random
//! shims do not apply. Before the upstream prelude freezes the intrinsics this
//! module installs a seeded `Math.random()`, a `Date.now()` backed by the
//! shared virtual clock, and removes `WeakRef`/`FinalizationRegistry`; the
//! remaining determinism (delivery order, a monotonic clock, one result number
//! per answer) is enforced here. The VM runs on an OS thread so `start()`
//! returns while the host answers calls through [`crate::CodemodeRun::deliver`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use serde_json::Value as Json;

use crate::identifier::to_codemode_identifier;
use crate::prelude::PRELUDE_SOURCE;
use crate::protocol::{
    CallTargetKind, DeterministicData, GlobalStartData, HostToWorkerMessage, ToolStartData,
};
use crate::sandbox::{Sandbox, SandboxDefaults, ToolError, ToolExecutor, check_global_names};
use crate::types::{
    AbortFlag, CallCallback, CodemodeError, CodemodeErrorKind, CodemodeHostCall, CodemodeOutcome,
    CodemodeResult, CodemodeRun, CodemodeRunOptions, StartedCallback, ToolContext,
};
use crate::vm::{INLINE_MAX_STACK_SIZE, VmOptions, run_vm};

/// Host-side state of one deterministic run (`vm.ts:182-188` plus the pending
/// table of `execution.ts:197-202`).
#[derive(Debug, Default)]
pub(crate) struct DeliveryState {
    /// Calls still waiting for an answer.
    pub(crate) pending: BTreeSet<u32>,
    /// Calls issued so far, tools and globals (`vm.ts:183`).
    pub(crate) issued: u32,
    /// Result numbers already used by `deliver()` (`execution.ts:197`).
    pub(crate) delivered: BTreeSet<u32>,
    /// Next result number the VM can apply (`vm.ts:185`).
    pub(crate) next_seq: u32,
    /// Results that arrived before their number (`vm.ts:188,214-223`).
    pub(crate) early: BTreeMap<u32, HostToWorkerMessage>,
    /// Whether the outcome has been decided.
    pub(crate) finished: bool,
}

impl DeliveryState {
    fn new() -> Self {
        Self {
            next_seq: 1,
            ..Self::default()
        }
    }
}

/// The VM thread's tool bridge: each call is forwarded to the host and then
/// blocks until `CodemodeRun::deliver()` answers it.
struct ChannelExecutor {
    state: Arc<Mutex<DeliveryState>>,
    replies: Receiver<HostToWorkerMessage>,
    on_call: Option<CallCallback>,
}

impl ToolExecutor for ChannelExecutor {
    fn call(
        &self,
        id: u32,
        target: CallTargetKind,
        name: &str,
        args: Json,
        _context: &ToolContext,
    ) -> Result<Json, ToolError> {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.finished {
                return Err(ToolError::new("Execution aborted"));
            }
            state.pending.insert(id);
            state.issued += 1;
        }
        if let Some(on_call) = &self.on_call {
            on_call(&CodemodeHostCall {
                id,
                target: if target == CallTargetKind::Tool {
                    crate::types::CallTarget::Tool
                } else {
                    crate::types::CallTarget::Global
                },
                name: name.to_string(),
                args,
            });
        }
        let reply = match self.replies.recv() {
            Ok(reply) => reply,
            Err(_) => return Err(ToolError::new("Execution aborted")),
        };
        if reply.ok {
            Ok(reply
                .payload
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or(Json::Null))
        } else {
            let mut error = ToolError::new(reply.payload.unwrap_or_default());
            if let Some(properties) = reply.properties
                && let Ok(map) = serde_json::from_str::<serde_json::Map<String, Json>>(&properties)
            {
                error.properties = Some(map);
            }
            Err(error)
        }
    }
}

/// FNV-1a over the prelude bytes (`vm.ts:71-81` hashes UTF-16 code units;
/// this prelude is ASCII so the values coincide).
fn fnv1a(text: &str) -> u32 {
    let mut hash = 0x811c9dc5_u32;
    for byte in text.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

/// The fingerprint delivered to `onStarted` (`vm.ts:314-320`). The engine and
/// deterministic-version parts are this port's equivalents, not upstream's
/// `quickjs-wasi` version string.
#[must_use]
pub fn fingerprint(memory_limit_bytes: Option<u64>, interrupt_budget: u64) -> String {
    let heap = memory_limit_bytes.map_or_else(|| "none".to_string(), |bytes| bytes.to_string());
    format!(
        "rquickjs-ng, prelude {:08x}, deterministic 1, stack {}, heap {heap}, budget {}",
        fnv1a(PRELUDE_SOURCE),
        INLINE_MAX_STACK_SIZE,
        interrupt_budget
    )
}

/// Owned data handed to the VM thread (`VmStartData` plus host limits).
struct OwnedRun {
    code: String,
    tools: Vec<ToolStartData>,
    globals: Vec<GlobalStartData>,
    store: BTreeMap<String, Json>,
    timeout_ms: Option<u64>,
    interrupt_budget: u64,
    memory_limit_bytes: Option<u64>,
    signal: AbortFlag,
    deterministic: DeterministicData,
    on_started: Option<StartedCallback>,
}

impl Sandbox {
    /// Run `code` deterministically with host-answered calls
    /// (`execution.ts:519-566`). Script failures never become Rust errors;
    /// only invalid options and a closed sandbox are rejected here.
    pub fn start(&self, code: &str, options: CodemodeRunOptions) -> Result<CodemodeRun, String> {
        if self.is_closed() {
            return Err("Sandbox is closed".to_string());
        }
        if !options.started_at.is_finite() || options.started_at < 0.0 {
            return Err(
                "startedAt must be a non-negative number of milliseconds since the epoch"
                    .to_string(),
            );
        }
        // `seed: u32` cannot express upstream's other invalid values.
        let tools = options.tools;
        if tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            != tools.len()
        {
            return Err("Tool names must be unique".to_string());
        }
        let script_names: BTreeSet<String> = tools
            .iter()
            .map(|tool| {
                tool.js_name
                    .clone()
                    .unwrap_or_else(|| to_codemode_identifier(&tool.name))
            })
            .collect();
        if script_names.len() != tools.len() {
            return Err("Tool script names must be unique".to_string());
        }
        check_global_names(options.globals.iter().map(|global| global.name.as_str()))?;

        let defaults: SandboxDefaults = self.defaults();
        let deterministic = DeterministicData {
            started_at: options.started_at,
            seed: options.seed,
        };
        let owned = OwnedRun {
            code: code.to_string(),
            tools: tools
                .iter()
                .map(|tool| ToolStartData {
                    name: tool.name.clone(),
                    js_name: tool
                        .js_name
                        .clone()
                        .unwrap_or_else(|| to_codemode_identifier(&tool.name)),
                    description: tool.description.clone().unwrap_or_default(),
                })
                .collect(),
            globals: options
                .globals
                .iter()
                .map(|global| GlobalStartData {
                    name: global.name.clone(),
                    spread: global.spread,
                })
                .collect(),
            store: options.store.unwrap_or_default(),
            timeout_ms: options.timeout_ms.map(|ms| ms as u64),
            interrupt_budget: defaults.interrupt_budget,
            memory_limit_bytes: defaults.memory_limit_bytes,
            signal: options.signal.unwrap_or_default(),
            deterministic,
            on_started: options.on_started.clone(),
        };

        let (replies, incoming) = channel::<HostToWorkerMessage>();
        let state = Arc::new(Mutex::new(DeliveryState::new()));
        let clock = Arc::new(Mutex::new(options.started_at));
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let signal = owned.signal.clone();
        let executor = ChannelExecutor {
            state: Arc::clone(&state),
            replies: incoming,
            on_call: options.on_call.clone(),
        };
        let worker_state = Arc::clone(&state);
        let worker_clock = Arc::clone(&clock);
        let worker_finished = Arc::clone(&finished);
        let worker_result = Arc::clone(&result);
        let worker: JoinHandle<()> = std::thread::spawn(move || {
            if let Some(on_started) = &owned.on_started {
                on_started(&fingerprint(
                    owned.memory_limit_bytes,
                    owned.interrupt_budget,
                ));
            }
            let vm_options = VmOptions {
                code: &owned.code,
                tools: &owned.tools,
                globals: &owned.globals,
                memory_limit_bytes: owned.memory_limit_bytes,
                store: &owned.store,
                timeout_ms: owned.timeout_ms.or(defaults.timeout_ms),
                interrupt_budget: owned.interrupt_budget,
                signal: Some(owned.signal.clone()),
                deterministic: Some(owned.deterministic),
                clock: Some(worker_clock),
                update_sink: defaults.update_sink.clone(),
            };
            let outcome = run_vm(&vm_options, &executor);
            if let Ok(mut guard) = worker_state.lock() {
                guard.finished = true;
            }
            worker_finished.store(true, Ordering::SeqCst);
            if let Ok(mut guard) = worker_result.lock() {
                *guard = Some(outcome);
            }
        });
        Ok(CodemodeRun {
            replies,
            state,
            clock,
            signal,
            finished,
            result,
            on_idle: options.on_idle.clone(),
            worker: Some(worker),
        })
    }
}

impl CodemodeRun {
    /// Answer call `id` as result `seq`, with the VM clock reading `now`
    /// (`execution.ts:260-298`, `vm.ts:203-227`).
    ///
    /// Results are applied strictly in `seq` order even if delivered out of
    /// order. A non-finite `now`, a repeated `seq`, or a call that is not
    /// waiting for an answer returns an error and leaves the call pending.
    pub fn deliver(
        &self,
        seq: u32,
        now: f64,
        id: u32,
        outcome: CodemodeOutcome,
    ) -> Result<(), String> {
        let mut idle = Vec::new();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.finished || self.finished.load(Ordering::SeqCst) {
                return Ok(());
            }
            if !now.is_finite() {
                return Err(format!(
                    "The clock value of result {seq} is not a finite number"
                ));
            }
            if seq == 0 || state.delivered.contains(&seq) {
                return Err(format!(
                    "Result number {seq} is not a positive integer used once"
                ));
            }
            if !state.pending.contains(&id) {
                return Err(format!("Call {id} is not waiting for a result"));
            }
            let (ok, payload, properties) = match outcome {
                CodemodeOutcome::Ok { value } => (true, serde_json::to_string(&value).ok(), None),
                CodemodeOutcome::Error {
                    message,
                    properties,
                } => {
                    let properties = match properties {
                        None => None,
                        Some(map) => {
                            let object = Json::Object(map.into_iter().collect());
                            Some(serde_json::to_string(&object).map_err(|_| {
                                format!("The properties of result {seq} are not an object")
                            })?)
                        }
                    };
                    (false, Some(message), properties)
                }
            };
            state.delivered.insert(seq);
            state.pending.remove(&id);
            state.early.insert(
                seq,
                HostToWorkerMessage {
                    id,
                    ok,
                    payload,
                    seq: Some(seq),
                    now: Some(now),
                    properties,
                },
            );
            while let Some(reply) = state.early.remove(&state.next_seq) {
                if let Some(reply_now) = reply.now {
                    let mut clock = self
                        .clock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if reply_now > *clock {
                        *clock = reply_now;
                    }
                }
                idle.push((state.next_seq, state.issued));
                let _ = self.replies.send(reply);
                state.next_seq += 1;
            }
        }
        for (seq, issued) in idle {
            if let Some(on_idle) = &self.on_idle {
                on_idle(seq, issued);
            }
        }
        Ok(())
    }

    /// Whether the outcome has been decided (`execution.ts:231-235`).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    /// Abort the run: the VM is interrupted at its next poll and pending tool
    /// calls are answered with an error so the VM thread can unwind.
    pub fn abort(&self) {
        self.signal.abort();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.finished = true;
        for id in std::mem::take(&mut state.pending) {
            let _ = self.replies.send(HostToWorkerMessage::new(
                id,
                false,
                Some("Execution aborted".to_string()),
            ));
        }
        self.finished.store(true, Ordering::SeqCst);
    }

    /// Join the VM thread and return its result. Do not call this from an
    /// `on_call` callback: that is the VM thread itself.
    #[must_use]
    pub fn wait(mut self) -> CodemodeResult {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| CodemodeResult::Err {
                error: CodemodeError::new(
                    CodemodeErrorKind::Sandbox,
                    "The sandbox stopped without reporting a result",
                ),
                output: Vec::new(),
                calls: Vec::new(),
                during: None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxOptions;
    use crate::types::{AbortFlag, CodemodeDeclaredTool, ExecuteOptions, OutputItem};
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    fn sandbox() -> Sandbox {
        Sandbox::new(SandboxOptions::default()).expect("sandbox")
    }

    fn declared(name: &str) -> CodemodeDeclaredTool {
        CodemodeDeclaredTool {
            name: name.to_string(),
            js_name: None,
            description: None,
        }
    }

    fn with_calls(code: &str) -> (CodemodeRun, Receiver<CodemodeHostCall>) {
        let (tx, rx) = channel();
        let options = CodemodeRunOptions {
            started_at: 1_000.0,
            seed: 7,
            tools: vec![declared("double")],
            on_call: Some(Arc::new(move |call: &CodemodeHostCall| {
                let _ = tx.send(call.clone());
            })),
            ..CodemodeRunOptions::default()
        };
        let run = sandbox().start(code, options).expect("start");
        (run, rx)
    }

    fn next_call(rx: &Receiver<CodemodeHostCall>) -> CodemodeHostCall {
        rx.recv_timeout(Duration::from_secs(5))
            .expect("call arrived")
    }

    /// A run with no VM thread, for testing the delivery state machine alone.
    fn detached_run() -> (CodemodeRun, Receiver<HostToWorkerMessage>) {
        let (tx, rx) = channel();
        let run = CodemodeRun {
            replies: tx,
            state: Arc::new(Mutex::new(DeliveryState::new())),
            clock: Arc::new(Mutex::new(1_000.0)),
            signal: AbortFlag::new(),
            finished: Arc::new(AtomicBool::new(false)),
            result: Arc::new(Mutex::new(None)),
            on_idle: None,
            worker: None,
        };
        (run, rx)
    }

    #[test]
    fn fingerprint_is_stable_and_names_the_engine() {
        let first = fingerprint(Some(1024), 100_000);
        assert_eq!(first, fingerprint(Some(1024), 100_000));
        assert!(first.contains("rquickjs-ng"));
        assert!(first.contains("deterministic 1"));
        assert!(first.contains("heap 1024"));
        assert_ne!(first, fingerprint(None, 100_000));
    }

    #[test]
    fn start_validates_options_and_declared_names() {
        let sandbox = sandbox();
        assert_eq!(
            sandbox
                .start("return 1;", CodemodeRunOptions::default())
                .unwrap_err(),
            "startedAt must be a non-negative number of milliseconds since the epoch"
        );
        assert_eq!(
            sandbox
                .start(
                    "return 1;",
                    CodemodeRunOptions {
                        started_at: f64::NAN,
                        ..CodemodeRunOptions::default()
                    }
                )
                .unwrap_err(),
            "startedAt must be a non-negative number of milliseconds since the epoch"
        );
        assert_eq!(
            sandbox
                .start(
                    "return 1;",
                    CodemodeRunOptions {
                        started_at: 1.0,
                        tools: vec![declared("a"), declared("a")],
                        ..CodemodeRunOptions::default()
                    }
                )
                .unwrap_err(),
            "Tool names must be unique"
        );
        assert_eq!(
            sandbox
                .start(
                    "return 1;",
                    CodemodeRunOptions {
                        started_at: 1.0,
                        tools: vec![
                            CodemodeDeclaredTool {
                                name: "a-b".to_string(),
                                js_name: Some("same".to_string()),
                                description: None,
                            },
                            CodemodeDeclaredTool {
                                name: "a_b".to_string(),
                                js_name: Some("same".to_string()),
                                description: None,
                            },
                        ],
                        ..CodemodeRunOptions::default()
                    }
                )
                .unwrap_err(),
            "Tool script names must be unique"
        );
        assert!(
            sandbox
                .start(
                    "return 1;",
                    CodemodeRunOptions {
                        started_at: 1.0,
                        globals: vec![crate::types::CodemodeDeclaredGlobal {
                            name: "console.log".to_string(),
                            spread: false,
                        }],
                        ..CodemodeRunOptions::default()
                    }
                )
                .is_err()
        );
        let closed = sandbox();
        closed.close();
        assert_eq!(
            closed
                .start("return 1;", CodemodeRunOptions::default())
                .unwrap_err(),
            "Sandbox is closed"
        );
    }

    #[test]
    fn run_delivers_a_call_and_applies_the_value() {
        let (run, rx) = with_calls("return await tools.double({ n: 21 });");
        let call = next_call(&rx);
        assert_eq!(call.id, 1);
        assert_eq!(call.name, "double");
        assert_eq!(call.args, json!({"n": 21}));
        run.deliver(
            1,
            1_005.0,
            call.id,
            CodemodeOutcome::Ok { value: json!(42) },
        )
        .expect("deliver");
        let result = run.wait();
        assert!(result.is_ok(), "{result:?}");
        match result {
            CodemodeResult::Ok { value, calls, .. } => {
                assert_eq!(value, json!(42));
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].status, crate::types::CallStatus::Ok);
            }
            CodemodeResult::Err { .. } => unreachable!(),
        }
    }

    #[test]
    fn results_are_applied_in_seq_order_with_a_monotonic_clock() {
        let (run, incoming) = detached_run();
        {
            let mut state = run.state.lock().expect("lock");
            state.pending.insert(10);
            state.pending.insert(11);
        }
        run.deliver(2, 1_020.0, 11, CodemodeOutcome::Ok { value: json!(2) })
            .expect("early result is buffered");
        assert!(incoming.try_recv().is_err(), "seq 2 must wait for seq 1");
        run.deliver(1, 1_010.0, 10, CodemodeOutcome::Ok { value: json!(1) })
            .expect("first result");
        let first = incoming.recv().expect("first reply");
        assert_eq!(first.seq, Some(1));
        assert_eq!(first.id, 10);
        let second = incoming.recv().expect("second reply");
        assert_eq!(second.seq, Some(2));
        assert_eq!(second.id, 11);
        assert_eq!(*run.clock.lock().expect("clock"), 1_020.0);
    }

    #[test]
    fn a_backward_clock_never_moves_the_virtual_clock_back() {
        let (run, incoming) = detached_run();
        run.state.lock().expect("lock").pending.insert(1);
        run.deliver(1, 500.0, 1, CodemodeOutcome::Ok { value: json!(1) })
            .expect("deliver");
        let _ = incoming.recv().expect("reply");
        assert_eq!(*run.clock.lock().expect("clock"), 1_000.0);
    }

    #[test]
    fn deliver_validates_clock_seq_and_pending_call() {
        let (run, rx) = with_calls("return await tools.double({});");
        let call = next_call(&rx);
        assert_eq!(
            run.deliver(
                1,
                f64::NAN,
                call.id,
                CodemodeOutcome::Ok { value: json!(1) }
            )
            .unwrap_err(),
            "The clock value of result 1 is not a finite number"
        );
        assert_eq!(
            run.deliver(0, 1.0, call.id, CodemodeOutcome::Ok { value: json!(1) })
                .unwrap_err(),
            "Result number 0 is not a positive integer used once"
        );
        assert_eq!(
            run.deliver(1, 1.0, 99, CodemodeOutcome::Ok { value: json!(1) })
                .unwrap_err(),
            "Call 99 is not waiting for a result"
        );
        run.deliver(1, 1.0, call.id, CodemodeOutcome::Ok { value: json!(1) })
            .expect("answer");
        assert_eq!(
            run.deliver(1, 1.0, call.id, CodemodeOutcome::Ok { value: json!(1) })
                .unwrap_err(),
            "Result number 1 is not a positive integer used once"
        );
        let _ = run.wait();
    }

    #[test]
    fn on_started_and_on_idle_report_progress() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let idle = Arc::new(Mutex::new(Vec::new()));
        let seen_started = Arc::clone(&started);
        let seen_idle = Arc::clone(&idle);
        let (tx, rx) = channel();
        let options = CodemodeRunOptions {
            started_at: 2_000.0,
            seed: 1,
            tools: vec![declared("double")],
            on_started: Some(Arc::new(move |fingerprint: &str| {
                seen_started
                    .lock()
                    .expect("lock")
                    .push(fingerprint.to_string());
            })),
            on_call: Some(Arc::new(move |call: &CodemodeHostCall| {
                let _ = tx.send(call.clone());
            })),
            on_idle: Some(Arc::new(move |seq, issued| {
                seen_idle.lock().expect("lock").push((seq, issued));
            })),
            ..CodemodeRunOptions::default()
        };
        let run = sandbox()
            .start("return await tools.double({});", options)
            .expect("start");
        let call = next_call(&rx);
        run.deliver(1, 2_000.0, call.id, CodemodeOutcome::Ok { value: json!(1) })
            .expect("deliver");
        assert!(run.wait().is_ok());
        assert_eq!(started.lock().expect("lock").len(), 1);
        assert!(started.lock().expect("lock")[0].contains("deterministic 1"));
        assert_eq!(*idle.lock().expect("lock"), vec![(1, 1)]);
    }

    #[test]
    fn abort_releases_a_waiting_call() {
        let (run, rx) = with_calls("return await tools.double({});");
        let _call = next_call(&rx);
        run.abort();
        assert!(run.is_finished());
        let result = run.wait();
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn result_error_properties_reach_the_script() {
        let (run, rx) = with_calls(
            "try { await tools.double({}); } catch (e) { return e.structuredOutput.code; }",
        );
        let call = next_call(&rx);
        let mut properties = BTreeMap::new();
        properties.insert("structuredOutput".to_string(), json!({"code": 7}));
        run.deliver(
            1,
            1_000.0,
            call.id,
            CodemodeOutcome::Error {
                message: "boom".to_string(),
                properties: Some(properties),
            },
        )
        .expect("deliver error");
        let result = run.wait();
        assert!(result.is_ok(), "{result:?}");
        match result {
            CodemodeResult::Ok { value, .. } => assert_eq!(value, json!(7)),
            CodemodeResult::Err { .. } => unreachable!(),
        }
        assert!(
            sandbox()
                .execute("return 1;", ExecuteOptions::default())
                .is_ok()
        );
    }

    #[test]
    fn seeded_random_and_virtual_clock_are_reproducible() {
        let code = "const a = Math.random(); const b = await tools.double({}); text(`${a}:${b}:${Date.now()}`); return a;";
        // `wait` consumes the run; use two independent runs and compare their
        // rendered output instead.
        let (first, rx1) = with_calls(code);
        let call1 = next_call(&rx1);
        first
            .deliver(
                1,
                1_500.0,
                call1.id,
                CodemodeOutcome::Ok { value: json!(2) },
            )
            .expect("deliver");
        let first_result = first.wait();
        let (second, rx2) = with_calls(code);
        let call2 = next_call(&rx2);
        second
            .deliver(
                1,
                1_500.0,
                call2.id,
                CodemodeOutcome::Ok { value: json!(2) },
            )
            .expect("deliver");
        let second_result = second.wait();
        assert_eq!(first_result.output(), second_result.output());
        assert!(first_result.is_ok(), "{first_result:?}");
        assert!(
            first_result.output().iter().any(
                |item| matches!(item, OutputItem::Text { text, .. } if text.contains(":2:1500"))
            ),
            "{:?}",
            first_result.output()
        );
    }
}
