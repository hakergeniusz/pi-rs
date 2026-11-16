//! pi's two message queues and every transition over them.
//!
//! Ground truth: pi-mono `packages/agent/src/agent.ts` - the
//! `PendingMessageQueue` class (`agent.ts:144-173`), the two queue fields on
//! `Agent` (`agent.ts:191-192`), and the methods that drive them
//! (`steer`/`followUp` `:302-313`, the clear family `:316-322`,
//! `hasQueuedMessages` `:325-327`, `peekQueuedMessages` `:330-332`, and the
//! two drains inside `continue()` `:381-395`).
//!
//! These are `Agent` fields in pi, not part of `Agent.state`, so they live
//! beside [`crate::state::AgentState`] rather than inside it. What the split
//! buys is that the queue semantics are stated once, here, instead of being
//! spelled out at every `Agent` call site.
//!
//! [`QueueHandle`] is the one place ownership differs from the TS: pi's
//! `steer`/`followUp` are plain methods on the agent object (`agent.ts:299-306`)
//! and JS has no borrows, so any caller can enqueue while `prompt` is running.
//! The port needs the same reachability from a `&Agent` (or from another
//! thread), which is what the handle is for; every rule it enforces is
//! `Queues`'.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::agent_message::AgentMessage;

use crate::types::{PendingMessageQueue, QueueMode, QueueSource};

/// Which queue [`Queues::drain_for_continue`] emptied, and with what.
#[derive(Debug, Clone)]
pub enum ContinueDrain {
    /// `steeringQueue.drain()` (`agent.ts:384`).
    Steering(Vec<AgentMessage>),
    /// `followUpQueue.drain()` (`agent.ts:390`).
    FollowUp(Vec<AgentMessage>),
}

/// The two queues, as one [`QueueSource`].
///
/// The order of the fields is the order pi declares them in
/// (`agent.ts:191-192`), and it is the order the loop polls them in.
#[derive(Debug)]
pub struct Queues {
    steering: PendingMessageQueue,
    follow_up: PendingMessageQueue,
}

impl Queues {
    /// Both queues start empty and in pi's default mode, `"one-at-a-time"`
    /// (`agent.ts:239-240`).
    pub fn new() -> Queues {
        Queues {
            steering: PendingMessageQueue::new(QueueMode::OneAtATime),
            follow_up: PendingMessageQueue::new(QueueMode::OneAtATime),
        }
    }

    /// `Agent.steer(messages)`: queued now, injected at the next steering poll.
    /// pi takes one message per call (`agent.ts:302-304`); a batch is the same
    /// enqueue in order.
    pub fn steer(&mut self, messages: impl IntoIterator<Item = AgentMessage>) {
        for message in messages {
            self.steering.enqueue(message);
        }
    }

    /// `Agent.followUp(messages)` (`agent.ts:307-309`): queued now, run only
    /// after the agent would otherwise stop.
    pub fn follow_up(&mut self, messages: impl IntoIterator<Item = AgentMessage>) {
        for message in messages {
            self.follow_up.enqueue(message);
        }
    }

    /// `Agent.clearSteeringQueue()` (`agent.ts:316-318`).
    pub fn clear_steering(&mut self) {
        self.steering.clear();
    }

    /// `Agent.clearFollowUpQueue()` (`agent.ts:311-313`).
    pub fn clear_follow_up(&mut self) {
        self.follow_up.clear();
    }

    /// `Agent.clearAllQueues()` (`agent.ts:319-322`): steering first, then
    /// follow-up. [`Queues::clear_for_reset`] is the one place pi uses the
    /// other order, and it matters only because both are unobservable.
    pub fn clear_all(&mut self) {
        self.clear_steering();
        self.clear_follow_up();
    }

    /// The clearing half of `Agent.reset()` (`agent.ts:364-365`): follow-up
    /// first, then steering - pi's order, opposite to `clearAllQueues`.
    pub fn clear_for_reset(&mut self) {
        self.clear_follow_up();
        self.clear_steering();
    }

    /// `Agent.hasQueuedMessages()` (`agent.ts:325-327`): true when either
    /// queue still holds a message.
    pub fn has_items(&self) -> bool {
        self.steering.has_items() || self.follow_up.has_items()
    }

    /// `Agent.peekQueuedMessages()` (`agent.ts:330-332`): the messages the
    /// next turn would inject, **without consuming them**.
    ///
    /// Steering wins whenever it has anything, and each queue previews only as
    /// far as its own mode reaches: `one-at-a-time` yields the oldest message,
    /// `all` yields everything queued.
    pub fn peek(&self) -> Vec<AgentMessage> {
        let steering = self.steering.peek();
        if !steering.is_empty() {
            return steering;
        }
        self.follow_up.peek()
    }

    /// The steering drain the loop polls through `getSteeringMessages`
    /// (`agent.ts:496-502`).
    pub fn drain_steering(&mut self) -> Vec<AgentMessage> {
        self.steering.drain()
    }

    /// The follow-up drain the loop polls through `getFollowUpMessages`
    /// (`agent.ts:503`).
    pub fn drain_follow_up(&mut self) -> Vec<AgentMessage> {
        self.follow_up.drain()
    }

    /// What `continue()` takes when the transcript ends on an assistant
    /// message (`agent.ts:381-395`): steering first, and only if that came back
    /// empty, follow-up.
    ///
    /// Which queue answered matters: pi re-enters through `prompt` either way,
    /// but only the steering branch sets `skipInitialSteeringPoll`
    /// (`agent.ts:385`, absent at `:391`), because only that branch has
    /// already drained the steering queue it is about to inject.
    pub fn drain_for_continue(&mut self) -> Option<ContinueDrain> {
        let queued_steering = self.steering.drain();
        if !queued_steering.is_empty() {
            return Some(ContinueDrain::Steering(queued_steering));
        }
        let queued_follow_ups = self.follow_up.drain();
        if !queued_follow_ups.is_empty() {
            return Some(ContinueDrain::FollowUp(queued_follow_ups));
        }
        None
    }

    /// Every queued steering message regardless of drain mode (see
    /// [`crate::types::PendingMessageQueue::all`]).
    pub fn steering_all(&self) -> Vec<AgentMessage> {
        self.steering.all()
    }

    /// Every queued follow-up message regardless of drain mode.
    pub fn follow_up_all(&self) -> Vec<AgentMessage> {
        self.follow_up.all()
    }

    /// `Agent.steeringMode` (`agent.ts:281-287`).
    pub fn steering_mode(&self) -> QueueMode {
        self.steering.mode
    }

    /// `Agent.followUpMode` (`agent.ts:290-296`).
    pub fn follow_up_mode(&self) -> QueueMode {
        self.follow_up.mode
    }

    /// `Agent.steeringMode = mode`.
    pub fn set_steering_mode(&mut self, mode: QueueMode) {
        self.steering.mode = mode;
    }

    /// `Agent.followUpMode = mode`.
    pub fn set_follow_up_mode(&mut self, mode: QueueMode) {
        self.follow_up.mode = mode;
    }
}

impl Default for Queues {
    fn default() -> Queues {
        Queues::new()
    }
}

/// pi's two queues behind interior mutability: the out-of-band handle to
/// `agent.steer(...)` / `agent.followUp(...)`.
///
/// Cloning it is how a caller gets the queues without the agent, so an enqueue
/// works while a `prompt` future holds `&mut Agent` - which is the whole point
/// (`agent.ts:299-306` are plain methods, reachable from anywhere, including
/// mid-run; pi's `--mode rpc` delivers exactly those two commands while a
/// prompt streams). The handle is `Send + Sync` and holds only the queues, so
/// the agent stays `!Send` where it owns the CLI's event sink.
///
/// pi never blocks on a queue: the loop polls at fixed points and the caller
/// only ever pushes, so there is no condvar here either - the mutex is taken
/// for the length of one drain or one enqueue and never across an await.
///
/// The poison case is a panic inside a listener or a tool while the lock was
/// held, which cannot leave the queues inconsistent (they are two `Vec`s), so
/// the guard is recovered rather than propagated.
#[derive(Debug, Clone)]
pub struct QueueHandle {
    queues: Arc<Mutex<Queues>>,
}

impl QueueHandle {
    /// Both queues empty and in pi's default mode (`agent.ts:247-248`).
    pub fn new() -> QueueHandle {
        QueueHandle {
            queues: Arc::new(Mutex::new(Queues::new())),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Queues> {
        self.queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `Agent.steer(messages)` (`agent.ts:299-301`), reachable from outside the
    /// agent while a run is in flight. The next steering poll takes it, so the
    /// current run sees it before its next model call.
    ///
    /// Returns nothing, like pi's: the disposition `"handled" | "queued"` is
    /// `AgentSession`'s layer (`agent-session.ts:294`, `:2129-2157`), not the
    /// agent's, and `Agent.steer` has no result to report.
    pub fn steer(&self, messages: impl IntoIterator<Item = AgentMessage>) {
        self.lock().steer(messages);
    }

    /// `Agent.followUp(messages)` (`agent.ts:304-306`): queued now, drained
    /// only at the point the agent would otherwise stop.
    pub fn follow_up(&self, messages: impl IntoIterator<Item = AgentMessage>) {
        self.lock().follow_up(messages);
    }

    /// `Agent.clearSteeringQueue()` (`agent.ts:308-311`).
    pub fn clear_steering(&self) {
        self.lock().clear_steering();
    }

    /// `Agent.clearFollowUpQueue()` (`agent.ts:313-316`).
    pub fn clear_follow_up(&self) {
        self.lock().clear_follow_up();
    }

    /// `Agent.clearAllQueues()` (`agent.ts:318-322`).
    pub fn clear_all(&self) {
        self.lock().clear_all();
    }

    /// `Agent.hasQueuedMessages()` (`agent.ts:324-327`).
    pub fn has_items(&self) -> bool {
        self.lock().has_items()
    }

    /// `Agent.peekQueuedMessages()` (`agent.ts:329-332`).
    pub fn peek(&self) -> Vec<AgentMessage> {
        self.lock().peek()
    }

    /// The steering drain the loop polls through `getSteeringMessages`.
    pub fn drain_steering(&self) -> Vec<AgentMessage> {
        self.lock().drain_steering()
    }

    /// The follow-up drain the loop polls through `getFollowUpMessages`.
    pub fn drain_follow_up(&self) -> Vec<AgentMessage> {
        self.lock().drain_follow_up()
    }

    /// What `continue()` takes from an assistant message (`agent.ts:381-395`).
    pub fn drain_for_continue(&self) -> Option<ContinueDrain> {
        self.lock().drain_for_continue()
    }

    /// `Agent.steeringMode` (`agent.ts:280-287`).
    pub fn steering_mode(&self) -> QueueMode {
        self.lock().steering_mode()
    }

    /// `Agent.followUpMode` (`agent.ts:289-296`).
    pub fn follow_up_mode(&self) -> QueueMode {
        self.lock().follow_up_mode()
    }

    /// `Agent.steeringMode = mode`.
    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.lock().set_steering_mode(mode);
    }

    /// `Agent.followUpMode = mode`.
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.lock().set_follow_up_mode(mode);
    }
}

impl Default for QueueHandle {
    fn default() -> QueueHandle {
        QueueHandle::new()
    }
}

impl QueueSource for QueueHandle {
    fn drain_steering(&mut self) -> Vec<AgentMessage> {
        QueueHandle::drain_steering(self)
    }

    fn drain_follow_up(&mut self) -> Vec<AgentMessage> {
        QueueHandle::drain_follow_up(self)
    }
}

impl QueueSource for Queues {
    fn drain_steering(&mut self) -> Vec<AgentMessage> {
        Queues::drain_steering(self)
    }

    fn drain_follow_up(&mut self) -> Vec<AgentMessage> {
        Queues::drain_follow_up(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(message: &str) -> AgentMessage {
        crate::transcript::user_message(message, 0)
    }

    /// `peek` reaches only as far as the mode does: `one-at-a-time` previews
    /// the oldest message, `all` previews everything. Neither consumes.
    #[test]
    fn peek_reaches_only_as_far_as_the_mode() {
        let mut queues = Queues::new();
        queues.steer([text("a"), text("b")]);

        assert_eq!(queues.peek().len(), 1, "one-at-a-time is the default");
        assert!(queues.has_items());

        queues.set_steering_mode(QueueMode::All);
        assert_eq!(queues.peek().len(), 2);
        assert!(queues.has_items(), "peeking consumes nothing");

        queues.set_steering_mode(QueueMode::OneAtATime);
        assert_eq!(
            queues.drain_steering().len(),
            1,
            "the drain follows the mode"
        );
        assert_eq!(queues.peek().len(), 1, "the drained message is gone");
    }

    /// `peekQueuedMessages` (`agent.ts:330-332`): steering wins whenever it has
    /// anything, and the follow-up queue is only previewed when it does not.
    #[test]
    fn the_peek_prefers_steering_over_follow_up() {
        let mut queues = Queues::new();
        queues.follow_up([text("follow")]);
        assert_eq!(queues.peek().len(), 1);

        queues.steer([text("steer")]);
        let peeked = queues.peek();
        assert_eq!(peeked.len(), 1);
        assert!(
            matches!(peeked.first(), Some(message) if message == &text("steer")),
            "with both queues loaded the peek is the steering message, not the follow-up behind it"
        );

        // Widen steering and the follow-up still does not surface: steering is
        // previewed as far as its own mode reaches, and only an empty steering
        // queue falls through (`agent.ts:330-332`).
        queues.set_steering_mode(QueueMode::All);
        queues.steer([text("steer-2")]);
        let peeked = queues.peek();
        assert_eq!(
            peeked.len(),
            2,
            "both steering messages, none from follow-up"
        );
        assert!(matches!(peeked.first(), Some(message) if message == &text("steer")));
        assert!(matches!(peeked.last(), Some(message) if message == &text("steer-2")));
    }

    /// `hasQueuedMessages` is true while *either* queue holds something.
    #[test]
    fn either_queue_keeps_the_agent_busy() {
        let mut queues = Queues::new();
        assert!(!queues.has_items());
        queues.steer([text("a")]);
        assert!(queues.has_items());
        queues.clear_steering();
        assert!(!queues.has_items());
        queues.follow_up([text("b")]);
        assert!(queues.has_items());
    }

    /// `continue()`'s precedence (`agent.ts:381-395`), and the one thing the
    /// split has to keep: only the steering branch has drained the steering
    /// queue, so only it may set `skipInitialSteeringPoll`.
    #[test]
    fn the_continue_drain_names_the_queue_it_emptied() {
        let mut queues = Queues::new();
        assert!(
            queues.drain_for_continue().is_none(),
            "nothing to continue from"
        );

        queues.follow_up([text("follow")]);
        assert!(matches!(
            queues.drain_for_continue(),
            Some(ContinueDrain::FollowUp(messages)) if messages == vec![text("follow")]
        ));

        queues.steer([text("steer")]);
        queues.follow_up([text("follow")]);
        assert!(
            matches!(
                queues.drain_for_continue(),
                Some(ContinueDrain::Steering(_))
            ),
            "steering is drained first, so it is the branch that already emptied a queue"
        );
    }

    /// Both clear families empty both queues; pi's reset clears them in the
    /// opposite order to `clearAllQueues`, which `clear_for_reset` keeps.
    #[test]
    fn both_clear_families_empty_both_queues() {
        let mut queues = Queues::new();
        queues.steer([text("a")]);
        queues.follow_up([text("b")]);
        queues.clear_all();
        assert!(!queues.has_items());

        queues.steer([text("a")]);
        queues.follow_up([text("b")]);
        queues.clear_for_reset();
        assert!(!queues.has_items());
    }
}
