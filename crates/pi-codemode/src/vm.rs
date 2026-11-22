//! Drives one QuickJS VM through one codemode script.
//!
//! Ground truth: `packages/codemode/src/runtime/vm.ts:1-341` (VM lifecycle,
//! prelude evaluation, bridge callback, job draining, interrupt budget) and
//! `packages/codemode/src/runtime/inline.ts:39-113` (the calling-thread
//! launcher). Engine: `rquickjs` (QuickJS-NG), reused from `pi-ext-js` instead
//! of upstream's `quickjs-wasi` wasm module.
//!
//! The prelude owns the JS-side bridge and reports messages through the native
//! `bridge(kind, a, b, c)` callback. Unlike upstream's worker transport, this
//! driver never re-enters the VM from that callback: it queues messages in an
//! outbox, answers tool calls after the current JS stack has unwound, and
//! drains pending jobs between rounds. This is the same shape as upstream's
//! inline launcher (messages deferred to a microtask).
//!
//! Two additions serve the fidelity slices: a deterministic bootstrap
//! (`runtime/vm.ts:23-36,94-125,314-320`) installed before the prelude freezes
//! the intrinsics, and the [`crate::stream::UpdateSink`] seam for the host's
//! `onUpdate` snapshots (`coding-agent/…/execute.ts:496-520`).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rquickjs::{Context, Ctx, Function, Runtime, Value};
use serde_json::Value as Json;
use serde_json::json;

use crate::prelude::PRELUDE_SOURCE;
use crate::protocol::{
    CallTargetKind, GlobalStartData, HostToWorkerMessage, ToolStartData, WorkerToHostMessage,
};
use crate::sandbox::ToolExecutor;
use crate::stream::{StreamTarget, UpdateEvent};
use crate::types::{
    AbortFlag, Call, CallStatus, CodemodeError, CodemodeErrorKind, CodemodeResult,
    CodemodeStoreWrites, OutputItem, ToolContext,
};

/// Interrupt polls an execution may use before it fails with `timeout`
/// (`runtime/inline.ts:39`). Default: 100,000.
pub const DEFAULT_INTERRUPT_BUDGET: u64 = 100_000;
/// QuickJS stack guard for an inline execution (`runtime/inline.ts:41-42`).
pub const INLINE_MAX_STACK_SIZE: usize = 256 * 1024;
/// Safety valve for the message/job loop. Upstream's equivalent is the
/// interrupt budget; this bounds bridge round trips a hostile script can
/// trigger with `await` on many tools.
const MAX_LOOP_ITERATIONS: usize = 100_000;
/// Default overall deadline (`runtime/execution.ts:49`).
const DEFAULT_TIMEOUT_MS: u64 = 300_000;

/// Everything needed to start one VM (`VmStartData` plus host limits).
#[derive(Clone)]
pub struct VmOptions<'a> {
    /// Async-function body to evaluate.
    pub code: &'a str,
    /// Tools the script can call.
    pub tools: &'a [ToolStartData],
    /// Globals the script can call.
    pub globals: &'a [GlobalStartData],
    /// QuickJS heap limit.
    pub memory_limit_bytes: Option<u64>,
    /// `load()` snapshot.
    pub store: &'a BTreeMap<String, Json>,
    /// Overall deadline.
    pub timeout_ms: Option<u64>,
    /// Interrupt-budget override.
    pub interrupt_budget: u64,
    /// Caller's abort flag.
    pub signal: Option<AbortFlag>,
    /// Deterministic mode: virtual clock and seeded randomness.
    pub deterministic: Option<crate::protocol::DeterministicData>,
    /// Shared virtual clock (`Date.now()` reads it through a native function).
    pub clock: Option<Arc<Mutex<f64>>>,
    /// Progress callback called from the driver loop.
    pub update_sink: Option<crate::stream::UpdateSink>,
}

enum Terminal {
    Done {
        value: Json,
        writes: CodemodeStoreWrites,
    },
    Error(CodemodeError),
}

#[derive(Default)]
struct RunOutcome {
    output: Vec<OutputItem>,
    calls: Vec<Call>,
    terminal: Option<Terminal>,
}

fn sandbox_outcome(message: impl Into<String>) -> RunOutcome {
    RunOutcome {
        terminal: Some(Terminal::Error(CodemodeError::sandbox(message))),
        ..RunOutcome::default()
    }
}

/// Run one script in a fresh VM and return its result. Never panics for script
/// failures: they come back as `CodemodeResult::Err`.
#[must_use]
#[allow(clippy::too_many_lines)] // mirrors upstream's VM lifecycle; split when extending
pub fn run_vm(options: &VmOptions<'_>, executor: &dyn ToolExecutor) -> CodemodeResult {
    let signal = options.signal.clone().unwrap_or_default();
    if signal.is_aborted() {
        return CodemodeResult::Err {
            error: CodemodeError::new(CodemodeErrorKind::Aborted, "Execution aborted"),
            output: Vec::new(),
            calls: Vec::new(),
            during: None,
        };
    }

    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => return sandbox_result(format!("Failed to create QuickJS runtime: {error}")),
    };
    if let Some(limit) = options.memory_limit_bytes {
        runtime.set_memory_limit(limit as usize);
    }
    runtime.set_max_stack_size(INLINE_MAX_STACK_SIZE);

    let polls = Arc::new(AtomicU64::new(0));
    let interrupted = Arc::new(AtomicBool::new(false));
    // The budget applies to the script, not to evaluating the fixed prelude.
    let armed = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let budget = options.interrupt_budget;
    let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    {
        let polls = Arc::clone(&polls);
        let interrupted = Arc::clone(&interrupted);
        let armed = Arc::clone(&armed);
        let signal = signal.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            if signal.is_aborted() || started.elapsed().as_millis() >= u128::from(timeout_ms) {
                interrupted.store(true, Ordering::SeqCst);
                return true;
            }
            if armed.load(Ordering::Relaxed) && polls.fetch_add(1, Ordering::SeqCst) >= budget {
                interrupted.store(true, Ordering::SeqCst);
                return true;
            }
            false
        })));
    }
    let context = match Context::full(&runtime) {
        Ok(context) => context,
        Err(error) => return sandbox_result(format!("Failed to create QuickJS context: {error}")),
    };

    let tools_json = serde_json::to_string(
        &options
            .tools
            .iter()
            .map(|tool| json!({"name": tool.name, "jsName": tool.js_name, "description": tool.description}))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_string());
    let globals_json = serde_json::to_string(
        &options
            .globals
            .iter()
            .map(|global| json!({"name": global.name, "spread": global.spread}))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_string());
    let store_json = serde_json::to_string(options.store).unwrap_or_else(|_| "{}".to_string());
    // Prefix shares the first line with the script so reported line numbers
    // match the script as written (`runtime/vm.ts:323-327`).
    let script_source = format!("(async (tools, console) => {{{}\n}})", options.code);

    let outbox = Arc::new(Mutex::new(Vec::<WorkerToHostMessage>::new()));
    let setup = context.with(|ctx| -> RunOutcome {
        let bridge_outbox = Arc::clone(&outbox);
        let bridge = match Function::new(
            ctx.clone(),
            move |kind: String, a: Value, b: Value, c: Value| -> rquickjs::Result<()> {
                if let Some(message) = bridge_message(&kind, &a, &b, &c) {
                    bridge_outbox
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(message);
                }
                Ok(())
            },
        ) {
            Ok(bridge) => bridge,
            Err(error) => {
                return sandbox_outcome(format!("Failed to install the sandbox bridge: {error}"));
            }
        };
        // Deterministic mode must change the intrinsics *before* the prelude's
        // lockdown freezes them (`runtime/vm.ts:170-180` creates the VM with a
        // virtual clock and seeded PRNG before evaluating the prelude).
        if let Some(deterministic) = options.deterministic {
            let clock = options
                .clock
                .clone()
                .unwrap_or_else(|| Arc::new(Mutex::new(deterministic.started_at)));
            let clock_fn = match Function::new(ctx.clone(), move || -> String {
                let guard = clock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.to_string()
            }) {
                Ok(clock_fn) => clock_fn,
                Err(error) => {
                    return sandbox_outcome(format!(
                        "Failed to install the virtual clock: {error}"
                    ));
                }
            };
            if let Err(error) = ctx.globals().set("__codemodeNow", clock_fn) {
                return sandbox_outcome(format!("Failed to install the virtual clock: {error}"));
            }
            // mulberry32 (`vm.ts:83-92`), with one draw per call and a clock
            // that only advances when a result is delivered.
            let bootstrap = format!(
                r#"(function (seed) {{
	"use strict";
	let state = seed >>> 0;
	Math.random = function () {{
		state = (state + 0x6d2b79f5) >>> 0;
		let value = Math.imul(state ^ (state >>> 15), 1 | state);
		value = (value + Math.imul(value ^ (value >>> 7), 61 | value)) ^ value;
		return ((value ^ (value >>> 14)) >>> 0) / 4294967296;
	}};
	Date.now = function () {{ return Number(__codemodeNow()); }};
	try {{ delete globalThis.WeakRef; delete globalThis.FinalizationRegistry; }} catch (error) {{}}
}})({});"#,
                deterministic.seed
            );
            if let Err(error) = ctx.eval::<Value, _>(bootstrap) {
                return sandbox_outcome(format!(
                    "Failed to install deterministic intrinsics: {error}"
                ));
            }
        }
        let prelude = match ctx.eval::<Function, _>(PRELUDE_SOURCE) {
            Ok(prelude) => prelude,
            Err(error) => {
                return sandbox_outcome(format!("Failed to evaluate the sandbox prelude: {error}"));
            }
        };
        let api = match prelude.call::<_, Value>((bridge, tools_json, globals_json, store_json)) {
            Ok(api) => api,
            Err(error) => {
                return sandbox_outcome(format!(
                    "Failed to initialize the sandbox prelude: {error}"
                ));
            }
        };
        let Some(api) = api.into_object() else {
            return sandbox_outcome("Sandbox prelude did not return an object");
        };
        let (Ok(settle), Ok(run), Ok(stalled)) = (
            api.get::<_, Function>("settle"),
            api.get::<_, Function>("run"),
            api.get::<_, Function>("stalled"),
        ) else {
            return sandbox_outcome("Sandbox prelude is missing settle/run/stalled");
        };
        // rquickjs ties JS handles to the lifetime of `Ctx`, so they cannot
        // leave `Context::with`. Stash the two callbacks the driver needs on
        // the globals object (removed after the run). Upstream keeps them in a
        // closure; this is the one observable difference, documented in
        // WIRING.md. They are removed before `run_vm` returns.
        let globals = ctx.globals();
        for (name, function) in [
            ("__codemodeSettle", &settle),
            ("__codemodeStalled", &stalled),
        ] {
            if let Err(error) = globals.set(name, (*function).clone()) {
                return sandbox_outcome(format!("Failed to stash the sandbox bridge: {error}"));
            }
        }
        let script = match ctx.eval::<Value, _>(script_source) {
            Ok(script) => script,
            Err(error) => {
                return RunOutcome {
                    terminal: Some(Terminal::Error(script_exception(&ctx, &error))),
                    ..RunOutcome::default()
                };
            }
        };

        // `run` calls the async script and attaches its handlers; a sync throw
        // is caught by the prelude and posted as a `done` message.
        armed.store(true, Ordering::SeqCst);
        if let Err(error) = run.call::<_, Value>((script,)) {
            let failure = if interrupted.load(Ordering::SeqCst) {
                CodemodeError::new(
                    CodemodeErrorKind::Timeout,
                    format!("Execution interrupted: {error}"),
                )
            } else {
                script_exception(&ctx, &error)
            };
            return RunOutcome {
                terminal: Some(Terminal::Error(failure)),
                ..RunOutcome::default()
            };
        }
        RunOutcome::default()
    });

    let mut outcome = setup;
    if outcome.terminal.is_none() {
        outcome = drive(
            &context,
            &runtime,
            &outbox,
            executor,
            &signal,
            &interrupted,
            started + std::time::Duration::from_millis(timeout_ms),
            options.update_sink.as_ref(),
        );
    }
    context.with(|ctx| {
        let globals = ctx.globals();
        for name in ["__codemodeSettle", "__codemodeStalled"] {
            let _ = globals.remove(name);
        }
    });
    finish(outcome)
}

fn sandbox_result(message: impl Into<String>) -> CodemodeResult {
    CodemodeResult::Err {
        error: CodemodeError::sandbox(message),
        output: Vec::new(),
        calls: Vec::new(),
        during: None,
    }
}

fn finish(outcome: RunOutcome) -> CodemodeResult {
    match outcome.terminal {
        Some(Terminal::Done { value, writes }) => CodemodeResult::Ok {
            value,
            output: outcome.output,
            calls: outcome.calls,
            store_writes: writes,
            during: None,
        },
        Some(Terminal::Error(error)) => CodemodeResult::Err {
            error,
            output: outcome.output,
            calls: outcome.calls,
            during: None,
        },
        None => CodemodeResult::Err {
            error: CodemodeError::sandbox("The sandbox stopped without reporting a result"),
            output: outcome.output,
            calls: outcome.calls,
            during: None,
        },
    }
}

/// Translate the bridge's primitive arguments into the ported protocol
/// (`runtime/vm.ts:252-296`).
fn bridge_message(
    kind: &str,
    a: &Value<'_>,
    b: &Value<'_>,
    c: &Value<'_>,
) -> Option<WorkerToHostMessage> {
    match kind {
        "call" | "global" => Some(WorkerToHostMessage::Call {
            id: a
                .as_int()
                .or_else(|| a.as_number().map(|value| value as i32))
                .unwrap_or_default() as u32,
            target: if kind == "call" {
                CallTargetKind::Tool
            } else {
                CallTargetKind::Global
            },
            name: js_text(b).unwrap_or_default(),
            args: js_text(c),
        }),
        "output" => {
            let item = match js_text(a).as_deref() {
                Some("image") => OutputItem::image(
                    js_text(b).unwrap_or_default(),
                    js_text(c).unwrap_or_default(),
                ),
                Some("console") => OutputItem::console(js_text(b).unwrap_or_default()),
                _ => OutputItem::text(js_text(b).unwrap_or_default()),
            };
            Some(WorkerToHostMessage::Output { item })
        }
        "done" => {
            if a.as_bool().unwrap_or(false) {
                Some(WorkerToHostMessage::DoneOk {
                    value: js_text(b),
                    writes: js_text(c).unwrap_or_else(|| "[]".to_string()),
                    during: None,
                })
            } else {
                Some(WorkerToHostMessage::DoneError {
                    error: js_text(b).unwrap_or_default(),
                    during: None,
                })
            }
        }
        _ => None,
    }
}

fn js_text(value: &Value<'_>) -> Option<String> {
    if value.is_undefined() || value.is_null() {
        return None;
    }
    rquickjs::String::from_value(value.clone())
        .ok()
        .and_then(|text| text.to_string().ok())
}

fn script_exception(ctx: &Ctx<'_>, error: &rquickjs::Error) -> CodemodeError {
    if matches!(error, rquickjs::Error::Exception) {
        let caught = ctx.catch();
        let object = caught.as_object();
        let field = |name: &str| object.and_then(|object| object.get::<_, String>(name).ok());
        CodemodeError {
            kind: CodemodeErrorKind::Script,
            name: field("name"),
            message: field("message").unwrap_or_else(|| error.to_string()),
            stack: field("stack"),
        }
    } else {
        CodemodeError::new(CodemodeErrorKind::Script, error.to_string())
    }
}

#[allow(clippy::too_many_lines)] // mirrors upstream's driver loop; split when extending
#[allow(clippy::too_many_arguments)] // VM handles, bridge state, and the update sink
fn drive(
    context: &Context,
    runtime: &Runtime,
    mailbox: &Arc<Mutex<Vec<WorkerToHostMessage>>>,
    executor: &dyn ToolExecutor,
    signal: &AbortFlag,
    interrupted: &Arc<AtomicBool>,
    deadline: Instant,
    update_sink: Option<&crate::stream::UpdateSink>,
) -> RunOutcome {
    let mut outcome = RunOutcome::default();
    for _ in 0..MAX_LOOP_ITERATIONS {
        // JS calls must happen inside `ctx.with`; `Runtime::execute_pending_job`
        // must happen outside it (both lock the same runtime mutex).
        let answered = context.with(|ctx| -> usize {
            let Ok(settle) = ctx.globals().get::<_, Function>("__codemodeSettle") else {
                outcome.terminal = Some(Terminal::Error(CodemodeError::sandbox(
                    "Sandbox bridge was removed unexpectedly",
                )));
                return 0;
            };
            let batch: Vec<WorkerToHostMessage> = {
                let mut guard = mailbox
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.drain(..).collect()
            };
            let mut answered = 0usize;
            for message in batch {
                if outcome.terminal.is_some() {
                    break;
                }
                match message {
                    WorkerToHostMessage::Output { item } => {
                        if let Some(sink) = update_sink {
                            sink(UpdateEvent::Output(item.clone()));
                        }
                        outcome.output.push(item);
                    }
                    WorkerToHostMessage::DoneOk { value, writes, .. } => {
                        match (
                            parse_json_field(value.as_deref(), "return value"),
                            parse_store_writes(&writes),
                        ) {
                            (Ok(value), Ok(writes)) => {
                                outcome.terminal = Some(Terminal::Done { value, writes });
                            }
                            (Err(error), _) | (_, Err(error)) => {
                                outcome.terminal = Some(Terminal::Error(bridge_error(error)));
                            }
                        }
                    }
                    WorkerToHostMessage::DoneError { error, .. } => {
                        let failure = if interrupted.load(Ordering::SeqCst) {
                            CodemodeError::new(
                                CodemodeErrorKind::Timeout,
                                format!("Execution interrupted: {error}"),
                            )
                        } else {
                            parse_script_error(&error).unwrap_or_else(bridge_error)
                        };
                        outcome.terminal = Some(Terminal::Error(failure));
                    }
                    WorkerToHostMessage::Crash { message } => {
                        let kind = if interrupted.load(Ordering::SeqCst) {
                            CodemodeErrorKind::Timeout
                        } else {
                            CodemodeErrorKind::Sandbox
                        };
                        outcome.terminal = Some(Terminal::Error(CodemodeError::new(kind, message)));
                    }
                    WorkerToHostMessage::Call {
                        id,
                        target,
                        name,
                        args,
                    } => {
                        answered += 1;
                        let started = Instant::now();
                        let arguments = match parse_json_field(args.as_deref(), "call arguments") {
                            Ok(arguments) => arguments,
                            Err(error) => {
                                outcome.terminal = Some(Terminal::Error(bridge_error(error)));
                                continue;
                            }
                        };
                        let tool_context = ToolContext {
                            signal: signal.clone(),
                        };
                        if let Some(sink) = update_sink {
                            sink(UpdateEvent::CallStarted {
                                id,
                                target: match target {
                                    CallTargetKind::Tool => StreamTarget::Tool,
                                    CallTargetKind::Global => StreamTarget::Global,
                                },
                                name: name.clone(),
                                args: arguments.clone(),
                            });
                        }
                        let reply = match executor.call(id, target, &name, arguments, &tool_context)
                        {
                            Ok(value) => HostToWorkerMessage::new(
                                id,
                                true,
                                serde_json::to_string(&value).ok(),
                            ),
                            Err(error) => {
                                let mut reply =
                                    HostToWorkerMessage::new(id, false, Some(error.message));
                                reply.properties = error.properties.and_then(|properties| {
                                    let map: serde_json::Map<String, Json> =
                                        properties.into_iter().collect();
                                    serde_json::to_string(&Json::Object(map)).ok()
                                });
                                reply
                            }
                        };
                        let status = if reply.ok {
                            CallStatus::Ok
                        } else {
                            CallStatus::Error
                        };
                        if let Some(sink) = update_sink {
                            sink(UpdateEvent::CallFinished {
                                id,
                                name: name.clone(),
                                status,
                            });
                        }
                        if target == CallTargetKind::Tool {
                            outcome.calls.push(Call {
                                name,
                                status,
                                duration_ms: started.elapsed().as_secs_f64() * 1000.0,
                            });
                        }
                        // Never re-enter from the bridge callback: `settle` is
                        // called only after the JS stack has unwound.
                        let _ = settle.call::<_, Value>((
                            id,
                            reply.ok,
                            reply.payload,
                            reply.properties,
                        ));
                    }
                    WorkerToHostMessage::Started { .. } | WorkerToHostMessage::Idle { .. } => {}
                }
            }
            answered
        });
        if outcome.terminal.is_some() {
            break;
        }

        let mut jobs = 0usize;
        let mut job_error = None;
        while jobs < MAX_LOOP_ITERATIONS {
            if jobs.is_multiple_of(50) && Instant::now() >= deadline {
                outcome.terminal = Some(Terminal::Error(CodemodeError::new(
                    CodemodeErrorKind::Timeout,
                    "Execution timed out",
                )));
                break;
            }
            match runtime.execute_pending_job() {
                Ok(true) => jobs += 1,
                Ok(false) => break,
                Err(error) => {
                    job_error = Some(error);
                    break;
                }
            }
        }
        if outcome.terminal.is_some() {
            break;
        }
        if let Some(error) = job_error {
            context.with(|ctx| {
                let _ = ctx.catch();
            });
            let kind = if interrupted.load(Ordering::SeqCst) {
                CodemodeErrorKind::Timeout
            } else {
                CodemodeErrorKind::Sandbox
            };
            outcome.terminal = Some(Terminal::Error(CodemodeError::new(kind, error.to_string())));
            break;
        }

        let is_stalled = context.with(|ctx| -> bool {
            let stalled: Function = match ctx.globals().get("__codemodeStalled") {
                Ok(stalled) => stalled,
                Err(_) => return false,
            };
            stalled.call::<_, bool>(()).unwrap_or(false)
        });
        if answered == 0 && jobs == 0 && !is_stalled {
            outcome.terminal = Some(Terminal::Error(CodemodeError::sandbox(
                "The script stopped making progress without settling",
            )));
            break;
        }
    }
    if let Some(sink) = update_sink {
        sink(UpdateEvent::Finished {
            ok: matches!(outcome.terminal, Some(Terminal::Done { .. })),
        });
    }
    outcome
}

fn bridge_error(message: impl Into<String>) -> CodemodeError {
    CodemodeError::sandbox(format!(
        "Sandbox bridge broken: {}. The script may have modified built-ins such as a prototype's toJSON.",
        message.into()
    ))
}

fn parse_json_field(text: Option<&str>, what: &str) -> Result<Json, String> {
    match text {
        None => Ok(Json::Null),
        Some(text) => serde_json::from_str(text).map_err(|_| format!("{what} is not valid JSON")),
    }
}

fn parse_store_writes(text: &str) -> Result<CodemodeStoreWrites, String> {
    let value: Json =
        serde_json::from_str(text).map_err(|_| "store writes are not valid JSON".to_string())?;
    let entries = value
        .as_array()
        .ok_or_else(|| "store writes are not an array".to_string())?;
    let mut writes = CodemodeStoreWrites::default();
    for entry in entries {
        let array = entry
            .as_array()
            .ok_or_else(|| "store writes contain a malformed entry".to_string())?;
        let key = array
            .first()
            .and_then(Json::as_str)
            .ok_or_else(|| "store writes contain a malformed entry".to_string())?
            .to_string();
        match array.len() {
            1 => writes.delete.push(key),
            2 => {
                let json_text = array[1]
                    .as_str()
                    .ok_or_else(|| "store writes contain a malformed entry".to_string())?;
                let parsed: Json = serde_json::from_str(json_text)
                    .map_err(|_| format!("store value for {key:?} is not valid JSON"))?;
                writes.set.insert(key, parsed);
            }
            _ => return Err("store writes contain a malformed entry".to_string()),
        }
    }
    Ok(writes)
}

fn parse_script_error(text: &str) -> Result<CodemodeError, String> {
    let parsed: Json =
        serde_json::from_str(text).map_err(|_| "script error is not valid JSON".to_string())?;
    let object = parsed
        .as_object()
        .ok_or_else(|| "script error is not an object".to_string())?;
    let message = object
        .get("message")
        .and_then(Json::as_str)
        .ok_or_else(|| "script error is malformed".to_string())?
        .to_string();
    let optional = |name: &str| -> Result<Option<String>, String> {
        match object.get(name) {
            None | Some(Json::Null) => Ok(None),
            Some(Json::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err("script error is malformed".to_string()),
        }
    };
    Ok(CodemodeError {
        kind: CodemodeErrorKind::Script,
        name: optional("name")?,
        message,
        stack: optional("stack")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_write_parsing_handles_sets_and_deletes() {
        let writes = parse_store_writes(r#"[["a","1"],["b","{\"x\":2}"],["c"]]"#).expect("parse");
        assert_eq!(writes.set.get("a"), Some(&Json::from(1)));
        assert_eq!(writes.set.get("b"), Some(&json!({"x": 2})));
        assert_eq!(writes.delete, vec!["c"]);
    }

    #[test]
    fn store_write_parsing_rejects_malformed_entries() {
        assert!(parse_store_writes("{}").is_err());
        assert!(parse_store_writes(r#"[["a","1","extra"]]"#).is_err());
        assert!(parse_store_writes(r#"[["a", 3]]"#).is_err());
    }

    #[test]
    fn script_error_parsing_preserves_name_and_stack() {
        let error = parse_script_error(
            r#"{"name":"TypeError","message":"boom","stack":"TypeError: boom\n at x"}"#,
        )
        .expect("parse");
        assert_eq!(error.kind, CodemodeErrorKind::Script);
        assert_eq!(error.name.as_deref(), Some("TypeError"));
        assert_eq!(error.message, "boom");
        assert!(error.stack.is_some_and(|stack| stack.contains("at x")));
        assert!(parse_script_error("\"oops\"").is_err());
        assert!(parse_script_error(r#"{"name":7,"message":"x"}"#).is_err());
    }

    #[test]
    fn bridge_message_ignores_unknown_kinds() {
        let runtime = Runtime::new().expect("runtime");
        let context = Context::full(&runtime).expect("context");
        context.with(|ctx| {
            let undefined = Value::new_undefined(ctx.clone());
            let null = Value::new_null(ctx.clone());
            assert!(bridge_message("unknown", &undefined, &undefined, &undefined).is_none());
            assert_eq!(js_text(&undefined), None);
            assert_eq!(js_text(&null), None);
        });
    }
}
