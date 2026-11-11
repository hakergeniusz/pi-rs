//! Serializes file mutations per file.
//!
//! Ground truth: pi-mono
//! `packages/coding-agent/src/core/tools/file-mutation-queue.ts`.
//!
//! `edit` and `write` mutate the same paths, so a model that issues both in one
//! turn must not interleave them. Operations on different files still run in
//! parallel; operations on the same file run in registration order.
//!
//! The queue key is the canonical (symlink-resolved) path
//! (`file-mutation-queue.ts:16-26`), so two spellings of the same file queue
//! together. A path that does not exist yet keys on its resolved form.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::Mutex as AsyncMutex;

type Queue = Arc<AsyncMutex<()>>;

fn queues() -> &'static Mutex<HashMap<PathBuf, Queue>> {
    static QUEUES: OnceLock<Mutex<HashMap<PathBuf, Queue>>> = OnceLock::new();
    QUEUES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// pi's `getMutationQueueKey` (`file-mutation-queue.ts:16-26`).
fn mutation_queue_key(file_path: &Path) -> PathBuf {
    match std::fs::canonicalize(file_path) {
        Ok(resolved) => resolved,
        // ENOENT / ENOTDIR: the file is being created, key on the resolved path
        // (`file-mutation-queue.ts:7-14`).
        Err(_) => lexical_resolve(file_path),
    }
}

fn lexical_resolve(path: &Path) -> PathBuf {
    let base = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    };
    let mut resolved = base;
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

/// pi's `withFileMutationQueue` (`file-mutation-queue.ts:32-61`).
///
/// pi chains the whole promise, including a throwing operation: the lock is
/// released on every path out of the body, which the async mutex guard gives
/// here too.
pub async fn with_file_mutation_queue<T, F, Fut>(file_path: &Path, operation: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let key = mutation_queue_key(file_path);
    let queue = {
        let mut map = queues().lock().expect("mutation queue map poisoned");
        map.entry(key)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    };
    let _guard = queue.lock().await;
    operation().await
}
