//! The program status reporter (`modes/interactive/program-status-reporter.ts`,
//! the whole file, 1-111): reports interactive-mode state to the terminal
//! (OSC 7501) — `working` during agent runs and compaction, `blocked` while a
//! dialog waits for the user, then `done`, `error`, or `idle` once the run
//! settles. Messages are limited to the session name, dialog titles, and the
//! first line of errors; prompts and assistant output are never reported.
//!
//! The mode owns one reporter (`interactive-mode.ts:572-576`) and feeds it
//! every agent event from `handleEvent` (`interactive-mode.ts:3531-3536`);
//! dialogs and overlays set and clear `blocked` sources around their
//! lifetimes (`:2853`, `:3118`, `:3155`).
//!
//! Deviations: upstream holds a `getTerminal`/`getSessionName` closure pair;
//! the port's reporter is a plain struct whose calls take the session name
//! and the [`pi_tui::tui::Tui`] (the mode's terminal lives inside the `Tui`, and
//! borrows cannot be captured in fields), and the `blocked` map is a
//! `Vec` in insertion order — delete-then-push preserves upstream's
//! most-recent-wins rule without an order-preserving map crate. The
//! `message_end` case keys on the assistant message's `stopReason` exactly
//! like upstream (`program-status-reporter.ts:41-48`); the port's
//! [`AgentEvent`] carries the same event set the reporter consumes, so no
//! event mapping is needed.

use pi_agent::agent_message::AgentMessage;
use pi_agent::events::{AgentEvent, CompactionReason};
use pi_tui::program_status::{ProgramStatus, ProgramStatusKind, ProgramStatusState};
use pi_tui::tui::Tui;

/// `BlockedStatus`, `program-status-reporter.ts:5`: what a blocked dialog
/// waits for, reported as the program status `kind` and `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedStatus {
    pub kind: ProgramStatusKind,
    pub message: String,
}

/// `firstLine`, `program-status-reporter.ts:7-9`: `text?.split(/\r?\n/, 1)[0]?.trim() || "Error"`
/// — the first CRLF/LF line, trimmed; `"Error"` when it is missing or blank.
fn first_line(text: Option<&str>) -> String {
    let Some(text) = text else {
        return "Error".to_string();
    };
    // `/\r?\n/`: a lone CR is not a line break here, matching the regex.
    let first = match text.find("\r\n").or_else(|| text.find('\n')) {
        Some(index) => &text[..index],
        None => text,
    };
    let trimmed = first.trim();
    if trimmed.is_empty() {
        "Error".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `ProgramStatusReporter`, `program-status-reporter.ts:17-111`.
#[derive(Debug)]
pub struct ProgramStatusReporter {
    run_active: bool,
    compacting: bool,
    /// Outcome of the current run, reported once it settles. `:22-23`.
    run_result: ProgramStatus,
    /// Status while no run is active. `:24-25`.
    resting_status: ProgramStatus,
    /// Open dialogs by source, in the order they opened. The most recent one
    /// is reported. `:26-27`.
    blocked: Vec<(String, BlockedStatus)>,
    last_report: Option<String>,
}

impl Default for ProgramStatusReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgramStatusReporter {
    /// `program-status-reporter.ts:30-33`: `runResult` starts `done`,
    /// `restingStatus` `idle`.
    pub fn new() -> Self {
        Self {
            run_active: false,
            compacting: false,
            run_result: ProgramStatus {
                state: ProgramStatusState::Done,
                ..ProgramStatus::default()
            },
            resting_status: ProgramStatus {
                state: ProgramStatusState::Idle,
                ..ProgramStatus::default()
            },
            blocked: Vec::new(),
            last_report: None,
        }
    }

    /// `handleEvent`, `program-status-reporter.ts:35-77`. Unhandled events
    /// return without reporting, exactly like upstream's `default` arm.
    pub fn handle_event(&mut self, event: &AgentEvent, session_name: Option<&str>, tui: &mut Tui) {
        match event {
            AgentEvent::AgentStart => {
                self.run_active = true;
                self.run_result = ProgramStatus {
                    state: ProgramStatusState::Done,
                    ..ProgramStatus::default()
                };
            }
            AgentEvent::MessageEnd { message } => {
                // The latest response decides the outcome, so a retried error
                // is replaced by its successful retry (`:41-48`).
                let AgentMessage::Assistant(assistant) = message else {
                    return;
                };
                self.run_result = if assistant.stop_reason == pi_ai::types::StopReason::Error {
                    ProgramStatus {
                        state: ProgramStatusState::Error,
                        message: Some(first_line(assistant.error_message.as_deref())),
                        ..ProgramStatus::default()
                    }
                } else {
                    ProgramStatus {
                        state: ProgramStatusState::Done,
                        ..ProgramStatus::default()
                    }
                };
            }
            AgentEvent::CompactionStart { .. } => {
                self.compacting = true;
            }
            AgentEvent::CompactionEnd {
                reason,
                aborted,
                error_message,
                ..
            } => {
                self.compacting = false;
                if self.run_active {
                    // A failed recovery compaction ends the run unless a later
                    // response succeeds (`:52-57`).
                    if *aborted {
                        self.run_result = ProgramStatus {
                            state: ProgramStatusState::Idle,
                            ..ProgramStatus::default()
                        };
                    } else if error_message.is_some() {
                        self.run_result = ProgramStatus {
                            state: ProgramStatusState::Error,
                            message: Some(first_line(error_message.as_deref())),
                            ..ProgramStatus::default()
                        };
                    }
                } else if *aborted {
                    self.resting_status = ProgramStatus {
                        state: ProgramStatusState::Idle,
                        ..ProgramStatus::default()
                    };
                } else if *reason == CompactionReason::Manual {
                    self.resting_status = if error_message.is_some() {
                        ProgramStatus {
                            state: ProgramStatusState::Error,
                            message: Some(first_line(error_message.as_deref())),
                            ..ProgramStatus::default()
                        }
                    } else {
                        ProgramStatus {
                            state: ProgramStatusState::Done,
                            ..ProgramStatus::default()
                        }
                    };
                }
            }
            AgentEvent::AgentSettled { aborted } => {
                self.run_active = false;
                self.resting_status = if *aborted {
                    ProgramStatus {
                        state: ProgramStatusState::Idle,
                        ..ProgramStatus::default()
                    }
                } else {
                    self.run_result.clone()
                };
            }
            AgentEvent::SessionInfoChanged { .. } => {
                // The session name is part of working and done reports; the
                // change itself reports through the next state change (`:70-72`).
            }
            _ => return,
        }
        self.report(session_name, tui);
    }

    /// Report `blocked` for a dialog until it is cleared with `None`.
    /// Reopening a source replaces it (`:79-84`).
    pub fn set_blocked(
        &mut self,
        source: &str,
        status: Option<BlockedStatus>,
        session_name: Option<&str>,
        tui: &mut Tui,
    ) {
        self.blocked.retain(|(existing, _)| existing != source);
        if let Some(status) = status {
            self.blocked.push((source.to_string(), status));
        }
        self.report(session_name, tui);
    }

    /// Forget the previous session's run, for example after switching
    /// sessions (`:86-93`).
    pub fn reset(&mut self, session_name: Option<&str>, tui: &mut Tui) {
        self.run_active = false;
        self.compacting = false;
        self.run_result = ProgramStatus {
            state: ProgramStatusState::Done,
            ..ProgramStatus::default()
        };
        self.resting_status = ProgramStatus {
            state: ProgramStatusState::Idle,
            ..ProgramStatus::default()
        };
        self.report(session_name, tui);
    }

    /// `report`, `program-status-reporter.ts:95-101`: dedupe by serialized
    /// status, then write through the terminal.
    pub fn report(&mut self, session_name: Option<&str>, tui: &mut Tui) {
        let mut status = self.current_status(session_name);
        status.app = Some(crate::config::APP_NAME.to_string());
        let key = dedupe_key(&status);
        if self.last_report.as_deref() == Some(key.as_str()) {
            return;
        }
        self.last_report = Some(key);
        tui.set_program_status(&status);
    }

    /// `currentStatus`, `program-status-reporter.ts:103-110`: blocked
    /// last-wins, then compacting, then the run or resting status. `working`
    /// and `done` carry the session name.
    fn current_status(&self, session_name: Option<&str>) -> ProgramStatus {
        if let Some((_, blocked)) = self.blocked.last() {
            return ProgramStatus {
                state: ProgramStatusState::Blocked,
                kind: Some(blocked.kind),
                message: Some(blocked.message.clone()),
                ..ProgramStatus::default()
            };
        }
        if self.compacting {
            return ProgramStatus {
                state: ProgramStatusState::Working,
                message: Some("Compacting context".to_string()),
                ..ProgramStatus::default()
            };
        }
        let status = if self.run_active {
            ProgramStatus {
                state: ProgramStatusState::Working,
                ..ProgramStatus::default()
            }
        } else {
            self.resting_status.clone()
        };
        if status.state == ProgramStatusState::Working || status.state == ProgramStatusState::Done {
            return ProgramStatus {
                message: session_name.map(str::to_string),
                ..status
            };
        }
        status
    }
}

/// The dedupe key: a canonical serialization of the reported status, standing
/// in for upstream's `JSON.stringify(status)` (`program-status-reporter.ts:97`).
fn dedupe_key(status: &ProgramStatus) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}",
        status.state, status.app, status.kind, status.message
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::terminal::{CaptureTerminal, TerminalSize};

    /// A `Tui` over a capture terminal whose OSC 7501 negotiation has been
    /// answered, so `set_program_status` writes like a supporting terminal.
    fn tui() -> (Tui, CaptureTerminal) {
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 100,
            rows: 40,
        });
        let handle = terminal.clone();
        let mut tui = Tui::new(Box::new(terminal));
        // `start` sends the setup query; the negotiation reply, as the
        // terminal would send it, flips support on (`terminal.ts:252-260`).
        tui.start().expect("tui start");
        tui.handle_input("\x1b]7501;?\x1b\\");
        (tui, handle)
    }

    /// One `AssistantMessage` with the fields the reporter reads (the same
    /// serde shape the scripted-stream fixtures use).
    fn test_assistant(
        stop_reason: pi_ai::types::StopReason,
        error_message: Option<&str>,
    ) -> pi_ai::types::AssistantMessage {
        serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": [],
            "api": "test",
            "provider": "test",
            "model": "test-model",
            "usage": {
                "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                "totalTokens": 0,
                "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0,
                          "cacheWrite": 0.0, "total": 0.0 }
            },
            "stopReason": stop_reason.as_str(),
            "timestamp": 0,
            "errorMessage": error_message,
        }))
        .expect("assistant message")
    }

    /// The last write, which every reporting call leaves behind.
    fn last_report_of(terminal: &CaptureTerminal) -> String {
        terminal
            .capture()
            .last_frame()
            .expect("a report was written")
    }

    /// `handleEvent`'s run lifecycle: working during the run, done with the
    /// session name at settle (`program-status-reporter.ts:37-48/:66-69/:107-109`).
    #[test]
    fn run_lifecycle_reports_working_then_done() {
        let (mut tui, terminal) = tui();
        let mut reporter = ProgramStatusReporter::new();
        reporter.handle_event(&AgentEvent::AgentStart, Some("fix bug"), &mut tui);
        let report = last_report_of(&terminal);
        assert!(report.contains("state=working"), "{report}");
        // base64("fix bug")
        assert!(report.contains("msg=Zml4IGJ1Zw=="), "{report}");
        reporter.handle_event(
            &AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(test_assistant(
                    pi_ai::types::StopReason::Stop,
                    None,
                )),
            },
            Some("fix bug"),
            &mut tui,
        );
        // message_end keeps the run active: the report is the same working
        // status, so the dedupe swallows it and the last write stands.
        assert!(
            last_report_of(&terminal).contains("state=working"),
            "still working mid-run"
        );
        reporter.handle_event(
            &AgentEvent::AgentSettled { aborted: false },
            Some("fix bug"),
            &mut tui,
        );
        let report = last_report_of(&terminal);
        assert!(report.contains("state=done"), "{report}");
        assert!(report.contains("msg=Zml4IGJ1Zw=="), "{report}");
    }

    /// An erroring response decides the run outcome
    /// (`program-status-reporter.ts:44-47`), and the first line of the error
    /// is the message.
    #[test]
    fn an_error_response_reports_the_first_line() {
        let (mut tui, terminal) = tui();
        let mut reporter = ProgramStatusReporter::new();
        reporter.handle_event(&AgentEvent::AgentStart, None, &mut tui);
        reporter.handle_event(
            &AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(test_assistant(
                    pi_ai::types::StopReason::Error,
                    Some("provider down\nsecond line"),
                )),
            },
            None,
            &mut tui,
        );
        reporter.handle_event(&AgentEvent::AgentSettled { aborted: false }, None, &mut tui);
        let report = last_report_of(&terminal);
        assert!(report.contains("state=error"), "{report}");
        // base64("provider down") — the first line only.
        assert!(report.contains("msg=cHJvdmlkZXIgZG93bg=="), "{report}");
    }

    /// Blocked dialogs override everything; reopening a source replaces it
    /// and clearing it drops back to the run state (`:79-84`, `:103-105`).
    #[test]
    fn blocked_sources_are_last_wins_and_cleared_by_source() {
        let (mut tui, terminal) = tui();
        let mut reporter = ProgramStatusReporter::new();
        reporter.handle_event(&AgentEvent::AgentStart, Some("run"), &mut tui);
        reporter.set_blocked(
            "extension-dialog",
            Some(BlockedStatus {
                kind: ProgramStatusKind::Question,
                message: "Pick one".to_string(),
            }),
            Some("run"),
            &mut tui,
        );
        let report = last_report_of(&terminal);
        assert!(
            report.contains("state=blocked:app=pi:kind=question"),
            "{report}"
        );
        // A second source becomes the reported one.
        reporter.set_blocked(
            "login",
            Some(BlockedStatus {
                kind: ProgramStatusKind::Auth,
                message: "Log in".to_string(),
            }),
            Some("run"),
            &mut tui,
        );
        let report = last_report_of(&terminal);
        assert!(report.contains("kind=auth"), "{report}");
        // Reopening the first source replaces its entry and moves it last.
        reporter.set_blocked(
            "extension-dialog",
            Some(BlockedStatus {
                kind: ProgramStatusKind::Question,
                message: "Pick two".to_string(),
            }),
            Some("run"),
            &mut tui,
        );
        let report = last_report_of(&terminal);
        assert!(report.contains("kind=question"), "{report}");
        // Clearing it falls back to the other open source.
        reporter.set_blocked("extension-dialog", None, Some("run"), &mut tui);
        let report = last_report_of(&terminal);
        assert!(report.contains("kind=auth"), "{report}");
        // Clearing the last source falls back to the run state.
        reporter.set_blocked("login", None, Some("run"), &mut tui);
        let report = last_report_of(&terminal);
        assert!(report.contains("state=working"), "{report}");
    }

    /// Identical reports do not write twice and unhandled events write
    /// nothing (`:73-76`, `:95-101`).
    #[test]
    fn identical_reports_dedupe() {
        let (mut tui, terminal) = tui();
        let mut reporter = ProgramStatusReporter::new();
        reporter.handle_event(&AgentEvent::AgentStart, Some("s"), &mut tui);
        let before = terminal
            .capture()
            .last_frame()
            .expect("agent_start wrote the working status");
        reporter.handle_event(&AgentEvent::TurnStart, Some("s"), &mut tui);
        assert_eq!(
            terminal.capture().last_frame(),
            Some(before.clone()),
            "an unhandled event (turn_start) writes nothing"
        );
        reporter.reset(Some("s"), &mut tui);
        let after = terminal
            .capture()
            .last_frame()
            .expect("reset writes the idle status");
        assert!(after != before, "reset changed the report");
        reporter.reset(Some("s"), &mut tui);
        assert_eq!(
            terminal.capture().last_frame(),
            Some(after),
            "an identical report writes nothing"
        );
    }

    /// Compaction reports working while it runs and done/idle at the end
    /// (`:49-65`).
    #[test]
    fn compaction_reports_working_then_the_outcome() {
        let (mut tui, terminal) = tui();
        let mut reporter = ProgramStatusReporter::new();
        reporter.handle_event(
            &AgentEvent::CompactionStart {
                reason: CompactionReason::Manual,
            },
            Some("s"),
            &mut tui,
        );
        let report = last_report_of(&terminal);
        assert!(
            report.contains("state=working") && report.contains("msg=Q29tcGFjdGluZyBjb250ZXh0"),
            "{report}"
        );
        reporter.handle_event(
            &AgentEvent::CompactionEnd {
                reason: CompactionReason::Manual,
                result: None,
                aborted: false,
                will_retry: false,
                error_message: None,
            },
            Some("s"),
            &mut tui,
        );
        let report = last_report_of(&terminal);
        assert!(report.contains("state=done"), "{report}");
        // An aborted manual compaction rests idle.
        reporter.handle_event(
            &AgentEvent::CompactionStart {
                reason: CompactionReason::Manual,
            },
            Some("s"),
            &mut tui,
        );
        reporter.handle_event(
            &AgentEvent::CompactionEnd {
                reason: CompactionReason::Manual,
                result: None,
                aborted: true,
                will_retry: false,
                error_message: None,
            },
            Some("s"),
            &mut tui,
        );
        let report = last_report_of(&terminal);
        assert!(report.contains("state=idle"), "{report}");
    }
}
