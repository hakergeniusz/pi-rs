//! Process-wide tokio runtime for driving async work from the sync CLI paths.
//!
//! The agent loop is awaited with a plain executor (`futures::executor`), but
//! the provider adapters spawn onto tokio (`tokio::spawn`) and need its
//! reactor (reqwest I/O, timers). `block_on` runs those futures on a lazily
//! built current-thread runtime instead, so provider tasks and timers work.

use std::future::Future;
use std::sync::OnceLock;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Drive `future` to completion on the shared runtime.
///
/// # Panics
/// If the runtime cannot be built (I/O or thread failure).
pub fn block_on<F: Future>(future: F) -> F::Output {
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime")
    });
    runtime.block_on(future)
}
