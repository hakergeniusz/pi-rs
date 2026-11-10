//! Smoke test: the agent crate links and its crate-root types are reachable.

#[test]
fn crate_compiles() {
    let signal = pi_agent::types::AbortSignal::new();
    assert!(!signal.aborted(), "a fresh abort signal is not aborted");
}
