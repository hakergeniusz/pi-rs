//! `AssistantMessageEventStream`: async channel-backed event stream.
//!
//! Mirrors TS `packages/ai/src/utils/event-stream.ts`. The TS version is an
//! async-iterable with in-band error termination; here providers push events
//! into a channel and consumers `next()` them. The stream ends when the
//! provider drops its sender — `done`/`error` events are still ordinary
//! events (the contract requires a final terminal event before closing).

use crate::types::AssistantMessageEvent;
use futures::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

/// Capacity of the event channel. Generous: `partial` snapshots are cheap
/// clones of a small message, and blocking providers on slow consumers would
/// stall background work in the agent loop.
const CHANNEL_CAPACITY: usize = 256;

/// Sender side used by provider implementations to emit events.
pub type EventSender = mpsc::Sender<AssistantMessageEvent>;

/// Stream of `AssistantMessageEvent`s, terminating with `done` or `error`.
///
/// Implements `Stream`; also exposes `next()` for simple async iteration.
pub struct AssistantMessageEventStream {
    rx: mpsc::Receiver<AssistantMessageEvent>,
}

impl AssistantMessageEventStream {
    /// Create a paired (stream, sender). The provider holds the sender.
    pub fn channel() -> (Self, EventSender) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        (Self { rx }, tx)
    }

    /// Wrap an existing receiver.
    pub fn from_receiver(rx: mpsc::Receiver<AssistantMessageEvent>) -> Self {
        Self { rx }
    }

    /// Next event, or `None` when the provider finished (after a terminal event).
    pub async fn next(&mut self) -> Option<AssistantMessageEvent> {
        self.rx.recv().await
    }

    /// Terminal check for an event: is this `done` or `error`?
    pub fn is_terminal(event: &AssistantMessageEvent) -> bool {
        matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        )
    }
}

impl Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantMessage, StopReason, Usage, now_ms};

    fn msg(stop: StopReason) -> AssistantMessage {
        AssistantMessage {
            content: vec![],
            api: "openai-completions".into(),
            provider: "test".into(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: vec![],
            usage: Usage::default(),
            stop_reason: stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now_ms(),
            duration_ms: None,
        }
    }

    #[tokio::test]
    async fn emits_then_closes() {
        let (mut stream, tx) = AssistantMessageEventStream::channel();
        tx.send(AssistantMessageEvent::Start {
            partial: msg(StopReason::Pending),
        })
        .await
        .unwrap();
        tx.send(AssistantMessageEvent::Done {
            reason: StopReason::Stop,
            message: msg(StopReason::Stop),
        })
        .await
        .unwrap();
        drop(tx);

        let first = stream.next().await.unwrap();
        assert!(matches!(first, AssistantMessageEvent::Start { .. }));
        let second = stream.next().await.unwrap();
        assert!(AssistantMessageEventStream::is_terminal(&second));
        assert!(stream.next().await.is_none(), "closed after terminal event");
    }
}
