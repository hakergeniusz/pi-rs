//! Tokio runtime for driving async work from the sync CLI paths.
//!
//! The agent loop is awaited with a plain executor (`futures::executor`), but
//! the provider adapters spawn onto tokio (`tokio::spawn`) and need its
//! reactor (reqwest I/O, timers). `block_on` runs those futures on a lazily
//! built current-thread runtime instead, so provider tasks and timers work.
//!
//! The runtime is **thread-local**: a process-wide current-thread runtime
//! cannot be entered from several threads at once, which made independent
//! tests (and any concurrently driven host paths) flake under `-D warnings`
//! test runs. Each calling thread gets its own reactor; the process-wide
//! `futures::executor` agent loop still drives the provider tasks through
//! whichever thread owns this runtime for the duration of a `block_on` call.

use std::cell::RefCell;
use std::future::Future;

thread_local! {
    static RUNTIME: RefCell<tokio::runtime::Runtime> = RefCell::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime"),
    );
}

/// Drive `future` to completion on this thread's runtime.
///
/// # Panics
/// If the runtime cannot be built, or if called while another `block_on` is
/// already running on this thread (tokio current-thread runtimes are not
/// reentrant) — the same constraint as before.
pub fn block_on<F: Future>(future: F) -> F::Output {
    RUNTIME.with(|runtime| runtime.borrow_mut().block_on(future))
}
