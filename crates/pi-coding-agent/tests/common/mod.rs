//! Shared helpers for the tool, prompt, and skills test binaries.

use std::path::PathBuf;
use std::sync::Arc;

use pi_agent::types::{AbortSignal, AgentToolResult};

/// A unique temporary directory per test, removed on drop (pi's
/// `tmpdir()/coding-agent-test-${Date.now()}` pattern).
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "pi-rs-{label}-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos()),
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("temp dir created");
        TempDir { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    #[allow(dead_code)]
    pub fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent dirs created");
        }
        std::fs::write(&path, content).expect("test file written");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The text of a tool result's text blocks, joined with newlines (pi's
/// `getTextOutput` helper in the test suite).
pub fn text_of(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The default abort signal for a tool call.
#[allow(dead_code)]
pub fn signal() -> Arc<AbortSignal> {
    Arc::new(AbortSignal::new())
}

/// The error string a failed tool call returns.
#[allow(dead_code)]
pub fn error_of(result: &Result<AgentToolResult, String>) -> String {
    match result {
        Ok(result) => text_of(result),
        Err(error) => error.clone(),
    }
}
