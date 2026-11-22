//! Worker admission seam and typed stubs for transports this port cannot host.
//!
//! Ground truth:
//! `packages/codemode/src/types.ts:134-163` (`CodemodeSandboxOptions.acquireWorker`
//! and `onWorkerStarted`), `packages/codemode/src/runtime/host.ts:20-74` (the
//! worker launcher: reserve capacity after loading wasm, release on exit,
//! terminate on stop, and the `Failed to load QuickJS` / `Failed to start
//! worker` / `Worker exited with code …` messages), and
//! `packages/codemode/src/runtime/remote.ts:229-247` (`Failed to reach the
//! remote sandbox`, `Remote sandbox failed: …`, `Remote sandbox returned before
//! the script settled`), plus `packages/codemode/src/wasm.ts:21-35`.
//!
//! The inline VM needs no workers, so [`NoopWorkerAdmission`] is the default.
//! Hosts that do run a worker pool can implement [`WorkerAdmission`] and call
//! it from their own launcher; the crate never starts a thread per execution.
//! [`WorkerTransportStub`], [`RemoteTransportStub`], and [`WasmLoaderStub`]
//! are data-only stubs: they carry upstream's error strings and a typed
//! `unsupported` error so downstream code can compile against the upstream
//! API surface without inventing a transport (see `WIRING.md`).

use std::sync::mpsc::Receiver;

use crate::types::CodemodeError;

/// A callback that releases one reserved worker slot. Upstream returns it from
/// `acquireWorker()` and calls it once, when worker construction fails or when
/// the worker actually exits (`types.ts:150-155`).
pub struct WorkerReservation {
    release: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl WorkerReservation {
    /// Wrap a release callback.
    #[must_use]
    pub fn new(release: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            release: Some(Box::new(release)),
        }
    }

    /// Release the slot now. Idempotent.
    pub fn release(&mut self) {
        if let Some(callback) = self.release.take() {
            callback();
        }
    }
}

impl Drop for WorkerReservation {
    fn drop(&mut self) {
        self.release();
    }
}

/// How the host observes a worker's actual exit (`onWorkerStarted`,
/// `types.ts:156-162`): upstream passes a promise that resolves on `exit`.
pub struct WorkerExit(Receiver<()>);

impl WorkerExit {
    /// Wrap a channel that receives one value when the worker exits.
    #[must_use]
    pub fn new(exited: Receiver<()>) -> Self {
        Self(exited)
    }

    /// Block until the worker exits.
    pub fn wait(self) {
        let _ = self.0.recv();
    }
}

/// Synchronous reservation of worker capacity before a VM starts.
pub trait WorkerAdmission: Send + Sync {
    /// Reserve one worker, or return the denial message. The returned release
    /// callback runs once when the worker is gone (or construction failed).
    fn acquire(&self) -> Result<WorkerReservation, String>;

    /// Called once for each worker the host starts. The default is a no-op for
    /// the inline VM, which starts no worker.
    fn on_worker_started(&self, _exited: WorkerExit) {}
}

/// Admission for the inline VM: nothing to reserve, nothing to observe.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopWorkerAdmission;

impl WorkerAdmission for NoopWorkerAdmission {
    fn acquire(&self) -> Result<WorkerReservation, String> {
        Ok(WorkerReservation::new(|| {}))
    }
}

/// Denial message a host returns when its pool is full (`acquireWorker`
/// throws; `host.ts:30` propagates it as a `sandbox` error).
#[must_use]
pub fn admission_denied(pool: &str) -> String {
    format!("No worker available in {pool}")
}

/// Failed wasm compilation (`host.ts:25-26`, `wasm.ts:21-35`).
#[must_use]
pub fn wasm_load_error(cause: &str) -> String {
    format!("Failed to load QuickJS: {cause}")
}

/// Failed worker construction (`host.ts:32-37`).
#[must_use]
pub fn worker_start_error(cause: &str) -> String {
    format!("Failed to start worker: {cause}")
}

/// Unexpected exit before the script settled (`host.ts:56-58`).
#[must_use]
pub fn worker_exit_error(code: i32) -> String {
    format!("Worker exited with code {code} before the script settled")
}

/// Failure to obtain a remote stub (`remote.ts:231-234`).
#[must_use]
pub fn remote_reach_error(cause: &str) -> String {
    format!("Failed to reach the remote sandbox: {cause}")
}

/// Remote RPC rejection (`remote.ts:244-247`).
#[must_use]
pub fn remote_failure(cause: &str) -> String {
    format!("Remote sandbox failed: {cause}")
}

/// Remote returned without a terminal message (`remote.ts:239-242`).
#[must_use]
pub const fn remote_returned_early() -> &'static str {
    "Remote sandbox returned before the script settled"
}

/// The interrupt budget message of the inline launcher (`inline.ts:92-95`).
#[must_use]
pub fn interrupt_budget_error(budget: u64) -> String {
    format!("Execution exceeded its interrupt budget of {budget}")
}

/// Data-only marker for the Node/Bun worker-thread transport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkerTransportStub;

impl WorkerTransportStub {
    /// The error a caller receives if it asks this port for a worker VM.
    #[must_use]
    pub fn unsupported() -> CodemodeError {
        CodemodeError::sandbox(
            "CodemodeSandbox needs a worker thread (node:worker_threads or Bun); this port runs the inline VM",
        )
    }
}

/// Data-only marker for the Cloudflare/RPC remote transport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemoteTransportStub;

impl RemoteTransportStub {
    /// The error a caller receives if it asks this port for a remote VM.
    #[must_use]
    pub fn unsupported() -> CodemodeError {
        CodemodeError::sandbox(
            "RemoteCodemodeSandbox needs an RPC isolate (serveCodemodeRemote); this port runs the inline VM",
        )
    }
}

/// Data-only marker for `loadQuickJSWasm`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WasmLoaderStub;

impl WasmLoaderStub {
    /// The error a caller receives if it asks this port to compile a wasm
    /// module. `rquickjs` links the engine natively instead.
    #[must_use]
    pub fn unsupported() -> CodemodeError {
        CodemodeError::sandbox(
            "loadQuickJSWasm needs a WebAssembly host; this port links QuickJS natively",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CodemodeErrorKind;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn noop_admission_reserves_and_releases() {
        let admission = NoopWorkerAdmission;
        let mut reservation = admission.acquire().expect("reservation");
        reservation.release();
        reservation.release(); // idempotent
    }

    #[test]
    fn custom_admission_can_deny_and_count_releases() {
        struct Pool {
            releases: AtomicUsize,
        }
        impl WorkerAdmission for Pool {
            fn acquire(&self) -> Result<WorkerReservation, String> {
                Err(admission_denied("codemode-pool"))
            }
            fn on_worker_started(&self, _exited: WorkerExit) {
                // no-op: no worker is started in this port
            }
        }
        let pool = Pool {
            releases: AtomicUsize::new(0),
        };
        assert_eq!(
            pool.acquire().unwrap_err(),
            "No worker available in codemode-pool"
        );
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&counter);
        {
            let reservation = WorkerReservation::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            });
            assert_eq!(counter.load(Ordering::SeqCst), 0);
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1); // Drop releases once
        pool.on_worker_started(WorkerExit::new(std::sync::mpsc::channel().1));
        assert_eq!(pool.releases.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn worker_exit_observes_a_signal() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(()).expect("send");
        WorkerExit::new(rx).wait();
    }

    #[test]
    fn stub_errors_match_upstream_strings() {
        assert_eq!(wasm_load_error("ENOENT"), "Failed to load QuickJS: ENOENT");
        assert_eq!(
            worker_start_error("EAGAIN"),
            "Failed to start worker: EAGAIN"
        );
        assert_eq!(
            worker_exit_error(7),
            "Worker exited with code 7 before the script settled"
        );
        assert_eq!(
            remote_reach_error("MISSING_BINDING"),
            "Failed to reach the remote sandbox: MISSING_BINDING"
        );
        assert_eq!(remote_failure("OOM"), "Remote sandbox failed: OOM");
        assert_eq!(
            remote_returned_early(),
            "Remote sandbox returned before the script settled"
        );
        assert_eq!(
            interrupt_budget_error(100_000),
            "Execution exceeded its interrupt budget of 100000"
        );
    }

    #[test]
    fn transport_stubs_are_sandbox_errors() {
        for error in [
            WorkerTransportStub::unsupported(),
            RemoteTransportStub::unsupported(),
            WasmLoaderStub::unsupported(),
        ] {
            assert_eq!(error.kind, CodemodeErrorKind::Sandbox);
            assert!(!error.message.is_empty());
        }
    }
}
