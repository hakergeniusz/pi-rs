//! Host-installed extension hooks for the loop's late event seams.
//!
//! Ground truth: `packages/coding-agent/src/core/agent-session.ts`
//! `_emitExtensionEvent`'s `message_end` arm (`:1399-1413`),
//! `_dispatchTurnEndBoundary` (`:907-943`) and `_runBeforeSettleBoundary`
//! (`:2086-2107`), which sit on top of pi-mono's `agent-loop.ts`.
//!
//! The coding-agent's extension runner lives in a higher crate than
//! `pi-agent`, so the loop cannot name it. These seams are a thread-local
//! registry the host installs for the duration of a run: print, JSON and RPC
//! each install the same hook object around their `run_agent_loop` call.
//! The registry is scoped (the guard restores the previous value on drop), so
//! parallel tests and nested loop calls do not observe each other's hooks.
//! Hosts without an extension runner (including `Agent` itself) install
//! nothing and the loop keeps its identity behaviour.

use std::cell::RefCell;
use std::sync::Arc;

use serde_json::Value;

use crate::agent_message::AgentMessage;

/// The two boundaries upstream dispatches through `emitBoundary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryKind {
    /// `turn_end` (`agent-session.ts:920-938`).
    TurnEnd,
    /// `agent_before_settle` (`agent-session.ts:2090-2097`).
    AgentBeforeSettle,
}

impl BoundaryKind {
    /// The extension event name on the wire.
    #[must_use]
    pub const fn event_name(self) -> &'static str {
        match self {
            Self::TurnEnd => "turn_end",
            Self::AgentBeforeSettle => "agent_before_settle",
        }
    }
}

/// One boundary dispatch request. `build_context` is the loop's rebuild
/// callback: it previews the session context for the draft `entries` a handler
/// has accumulated so far, exactly like upstream's
/// `_buildBoundaryContext(entries, boundary)` argument.
pub struct BoundaryRequest<'a> {
    pub kind: BoundaryKind,
    /// The base event (`type`, `message`, `toolResults`, `turnIndex`, ...).
    pub event: Value,
    /// Rebuild the context preview for a draft entry list.
    pub build_context: &'a dyn Fn(&Value) -> Value,
}

/// What the host's boundary dispatch decided.
#[derive(Debug, Clone, Default)]
pub struct BoundaryOutcome {
    /// Whether the extension voted to continue the run.
    pub continue_run: bool,
    /// False when a context rebuild failed: upstream discards every draft.
    pub valid: bool,
}

/// The host-side seams the loop calls for extension rewriting.
pub trait ExtensionHooks: Send + Sync {
    /// `emitMessageEnd`'s rewrite chain; `None` keeps the finalized message.
    fn rewrite_message_end(&self, _message: &AgentMessage) -> Option<AgentMessage> {
        None
    }

    /// `emitBoundary` for one boundary. The default is a no-vote.
    fn dispatch_boundary(&self, _request: BoundaryRequest<'_>) -> BoundaryOutcome {
        BoundaryOutcome::default()
    }
}

thread_local! {
    static HOOKS: RefCell<Option<Arc<dyn ExtensionHooks>>> = const { RefCell::new(None) };
}

/// Installs hooks for the current thread until the guard is dropped. Installing
/// nests: the previous hooks are restored, so a host that runs a nested loop
/// does not lose its outer hooks.
#[must_use = "the guard must be kept alive for the whole run"]
pub struct ExtensionHooksGuard(Option<Arc<dyn ExtensionHooks>>);

impl Drop for ExtensionHooksGuard {
    fn drop(&mut self) {
        HOOKS.with(|cell| {
            *cell.borrow_mut() = self.0.take();
        });
    }
}

/// Install `hooks` for this thread's current run.
pub fn install_extension_hooks(hooks: Arc<dyn ExtensionHooks>) -> ExtensionHooksGuard {
    ExtensionHooksGuard(HOOKS.with(|cell| cell.borrow_mut().replace(hooks)))
}

/// Apply the installed `message_end` rewrite, if any. A replacement with a
/// different role is rejected here as a second line of defence: upstream's
/// runner already reports and skips it (`runner.ts:1158-1195`).
pub(crate) fn rewrite_message_end(message: &mut AgentMessage) {
    HOOKS.with(|cell| {
        let borrowed = cell.borrow();
        let Some(hooks) = borrowed.as_ref() else {
            return;
        };
        if let Some(replacement) = hooks.rewrite_message_end(message)
            && replacement.role() == message.role()
        {
            *message = replacement;
        }
    });
}

/// Dispatch one boundary through the installed hooks.
pub(crate) fn dispatch_boundary(
    kind: BoundaryKind,
    event: Value,
    build_context: &dyn Fn(&Value) -> Value,
) -> BoundaryOutcome {
    HOOKS.with(|cell| {
        let borrowed = cell.borrow();
        let Some(hooks) = borrowed.as_ref() else {
            return BoundaryOutcome::default();
        };
        hooks.dispatch_boundary(BoundaryRequest {
            kind,
            event,
            build_context,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct CountingHooks(std::sync::atomic::AtomicU32);

    impl ExtensionHooks for CountingHooks {
        fn rewrite_message_end(&self, message: &AgentMessage) -> Option<AgentMessage> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(message.clone())
        }
    }

    #[test]
    fn guard_restores_the_previous_hooks() {
        let first = Arc::new(CountingHooks::default());
        let guard = install_extension_hooks(Arc::clone(&first) as Arc<dyn ExtensionHooks>);
        let mut message = crate::transcript::user_message("hi", 1);
        rewrite_message_end(&mut message);
        assert_eq!(first.0.load(std::sync::atomic::Ordering::SeqCst), 1);

        let second = Arc::new(CountingHooks::default());
        let nested = install_extension_hooks(Arc::clone(&second) as Arc<dyn ExtensionHooks>);
        rewrite_message_end(&mut message);
        assert_eq!(second.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(first.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(nested);
        rewrite_message_end(&mut message);
        assert_eq!(first.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        drop(guard);

        // Identity after the outermost guard is gone.
        rewrite_message_end(&mut message);
        assert_eq!(first.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
