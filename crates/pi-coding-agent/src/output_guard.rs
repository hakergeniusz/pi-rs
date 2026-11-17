//! stdout output guard: ordered raw stdout writes and the stdout takeover.
//!
//! 1:1 port of `core/output-guard.ts` (108 lines). Node's `process.stdout`
//! is an asynchronous stream: protocol writes queue on a promise tail
//! (`:11`), drain asynchronously, can backpressure, and can fail with
//! transient OS errors — so the guard serializes writes, waits for the
//! drain (`:95-103`), retries `ENOBUFS`/`EAGAIN`/`EWOULDBLOCK` after 10 ms
//! (`:36-40`), and patches `process.stdout.write` to divert stray writes
//! to stderr (`:54-63`). Rust stdout is a synchronous blocking
//! `LineWriter`, so the same surface lands on direct writes with the
//! mechanism deviations below; the upstream-visible contract — write
//! order, what is flushed when, restore semantics, silent `exit(1)` on a
//! failed write (`:90-92`) — is preserved.
//!
//! Deviations (mechanism only):
//!
//! * No promise tail (from `:11`, `:20-43`, `:85-93`): upstream chains
//!   every `writeRawStdout` chunk onto a promise that resolves when libuv
//!   hands the bytes to the OS; [`write_raw_stdout`] performs the
//!   equivalent blocking `write_all` in call order. Every mode call site
//!   writes a newline-terminated chunk, which the `LineWriter` hands to
//!   the fd within the call, so the drain upstream awaits has already
//!   happened by the time [`wait_for_raw_stdout_backpressure`] and
//!   [`flush_raw_stdout`] run — both are kept for call-site parity
//!   (`print-mode.ts:120-125`, `:180`).
//! * Retry set (from `:36-40`): upstream retries `ENOBUFS`, `EAGAIN`, and
//!   `EWOULDBLOCK`; std maps `EAGAIN`/`EWOULDBLOCK` to
//!   [`io::ErrorKind::WouldBlock`], which the port retries on the same
//!   10 ms delay. `ENOBUFS` has no std error kind (no libc dependency)
//!   and is not retried; it is unreachable on a blocking fd.
//! * Flush as an explicit drain (from `:105-108`): upstream's
//!   `flushRawStdout` writes an empty chunk to force one final stream
//!   cycle; an empty `write_all` is a true no-op in std, so
//!   [`flush_raw_stdout`] flushes the stdout handle instead — the same
//!   observable effect (all buffered bytes handed to the OS).
//! * No `process.stdout` patch (from `:45-70`): upstream replaces
//!   `process.stdout.write` with a stderr-forwarding shim;
//!   `std::io::stdout` cannot be rebound. [`take_over_stdout`] records
//!   the takeover marker for [`is_stdout_taken_over`] (upstream `:81-83`;
//!   upstream's consumer is `package-manager.ts:2876`) and
//!   [`restore_stdout`] clears it; the diversion effect is structural in
//!   the port — the extension host's console shim writes extension
//!   console output to stderr, and the print/JSON modes write stdout only
//!   through [`write_raw_stdout`].
//! * stderr interleaving (timing only): upstream's queued stdout writes
//!   can reach the fd after a synchronous `console.error` from the same
//!   run; the port's stdout writes complete first. Same streams, same
//!   bytes, different interleave at a shared terminal.
//!
//! Consumers: `modes/print-mode.ts` — `writeRawStdout` (`:117`, `:132`,
//! `:158`), `waitForRawStdoutBackpressure` (`:120-125`),
//! `flushRawStdout` (`:180`) — wired in `modes/print_mode.rs` and
//! `modes/json_event.rs`; `main.ts:705`/`:1059` (takeover lifecycle) —
//! wired in the binary's dispatch; `rpc-mode.ts` and
//! `package-manager.ts:2876` are the P5 remainder.

use std::io::{self, Write};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// Retry delay for a transiently failing raw write
/// (`RAW_STDOUT_RETRY_DELAY_MS`, `output-guard.ts:9`).
const RAW_STDOUT_RETRY_DELAY_MS: u64 = 10;

/// The stdout takeover (`StdoutTakeoverState`, `output-guard.ts:1-7`).
/// Upstream captures the bound raw stdout/stderr writers and the original
/// `process.stdout.write` it is about to replace; the port cannot rebind
/// the standard handles, so the state carries only the takeover marker the
/// other functions key on (`:7`, `:45-83`).
struct StdoutTakeoverState {}

static STDOUT_TAKEOVER_STATE: Mutex<Option<StdoutTakeoverState>> = Mutex::new(None);

fn takeover_state() -> MutexGuard<'static, Option<StdoutTakeoverState>> {
    STDOUT_TAKEOVER_STATE
        .lock()
        .expect("stdout takeover state poisoned")
}

/// `takeOverStdout` (`output-guard.ts:45-70`): take stdout over for the
/// protocol modes, idempotently.
pub fn take_over_stdout() {
    let mut state = takeover_state();
    if state.is_some() {
        return; // :46-48
    }
    // :50-69 capture the raw writers and patch `process.stdout.write`; the
    // port records the marker only (module doc).
    *state = Some(StdoutTakeoverState {});
}

/// `restoreStdout` (`output-guard.ts:72-79`): give the original stdout
/// semantics back, idempotently.
pub fn restore_stdout() {
    let mut state = takeover_state();
    if state.is_none() {
        return; // :73-75
    }
    *state = None; // :77-78
}

/// `isStdoutTakenOver` (`output-guard.ts:81-83`).
pub fn is_stdout_taken_over() -> bool {
    takeover_state().is_some()
}

/// The retryable write errors (`output-guard.ts:36-40`): `EAGAIN` and
/// `EWOULDBLOCK` surface as `WouldBlock`; `ENOBUFS` is not representable
/// (module doc) and is not retried.
fn is_retryable_write_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
}

/// The retry loop (`writeRawStdoutChunk`, `output-guard.ts:21-42`): repeat
/// the operation while it fails retryably, delaying
/// [`RAW_STDOUT_RETRY_DELAY_MS`] between attempts.
fn retrying_write(mut write: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    loop {
        match write() {
            Ok(()) => return Ok(()),
            Err(error) if is_retryable_write_error(&error) => {
                std::thread::sleep(Duration::from_millis(RAW_STDOUT_RETRY_DELAY_MS));
            }
            Err(error) => return Err(error),
        }
    }
}

fn write_raw_stdout_chunk(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    retrying_write(|| stdout.write_all(text.as_bytes()))
}

/// `writeRawStdout` (`output-guard.ts:85-93`): one ordered raw stdout
/// write. Empty text is a no-op (`:86-88`); a non-retryable write error
/// terminates the process with exit code 1, silently (`:90-92`).
pub fn write_raw_stdout(text: &str) {
    if text.is_empty() {
        return;
    }
    if write_raw_stdout_chunk(text).is_err() {
        std::process::exit(1);
    }
}

/// `waitForRawStdoutBackpressure` (`output-guard.ts:95-103`): the promise
/// tail's drain point (`print-mode.ts:120-125`). The port's writes are
/// synchronous (module doc), so no tail is ever pending and this returns
/// immediately; it exists for call-site parity.
pub fn wait_for_raw_stdout_backpressure() {}

/// `flushRawStdout` (`output-guard.ts:105-108`): drain pending writes and
/// force the stream through one final cycle. Errors propagate, like
/// upstream's awaited empty write (`print-mode.ts:180` can reject).
pub fn flush_raw_stdout() -> io::Result<()> {
    retrying_write(|| io::stdout().lock().flush())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufWriter;

    /// Serializes the takeover test: the state is process-global and the
    /// harness runs tests on parallel threads.
    static TAKEOVER_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// `writeRawStdoutChunk` (`output-guard.ts:20-43`) over an injectable
    /// sink, so tests observe the write without a terminal.
    fn write_raw_stdout_chunk_to<W: Write>(writer: &mut W, text: &str) -> io::Result<()> {
        retrying_write(|| writer.write_all(text.as_bytes()))
    }

    /// A writer that fails the first `remaining` writes with `WouldBlock`
    /// (the `EAGAIN`/`EWOULDBLOCK` arm, `output-guard.ts:37`), then
    /// records.
    struct WouldBlockThenWrite {
        remaining: usize,
        received: Vec<u8>,
    }

    impl Write for WouldBlockThenWrite {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.remaining > 0 {
                self.remaining -= 1;
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            self.received.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Any other error surfaces immediately (`output-guard.ts:38-39`); the
    /// caller exits the process (`:90-92`).
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The documented order: raw writes queue in call order and only the
    /// final flush (`flushRawStdout`, `output-guard.ts:105-108`) releases
    /// them, in that order.
    #[test]
    fn raw_writes_buffer_and_flush_in_call_order() {
        // A BufWriter stands in for the std stdout buffer.
        let mut buffered = BufWriter::new(Vec::new());
        write_raw_stdout_chunk_to(&mut buffered, "first\n").expect("first write");
        write_raw_stdout_chunk_to(&mut buffered, "second\n").expect("second write");
        assert_eq!(
            std::str::from_utf8(buffered.buffer()).expect("utf8"),
            "first\nsecond\n"
        );
        buffered.flush().expect("flush");
        assert_eq!(
            std::str::from_utf8(buffered.get_ref()).expect("utf8"),
            "first\nsecond\n"
        );
    }

    /// An empty `write_all` never reaches the writer: the std no-op behind
    /// the flush deviation (upstream's empty-chunk write, `:105-108`,
    /// forces a stream cycle).
    #[test]
    fn empty_chunk_performs_no_write() {
        let mut writer = WouldBlockThenWrite {
            remaining: 1,
            received: Vec::new(),
        };
        write_raw_stdout_chunk_to(&mut writer, "").expect("empty write");
        assert_eq!(writer.remaining, 1);
        assert_eq!(writer.received, Vec::<u8>::new());
    }

    /// A `WouldBlock` write retries until it succeeds
    /// (`output-guard.ts:36-41`).
    #[test]
    fn would_block_writes_retry_after_the_documented_delay() {
        let mut writer = WouldBlockThenWrite {
            remaining: 2,
            received: Vec::new(),
        };
        write_raw_stdout_chunk_to(&mut writer, "payload").expect("retries drain the blocks");
        assert_eq!(writer.remaining, 0);
        assert_eq!(
            std::str::from_utf8(&writer.received).expect("utf8"),
            "payload"
        );
    }

    /// Any other error surfaces immediately (`output-guard.ts:38-39`).
    #[test]
    fn non_retryable_write_errors_surface() {
        let mut writer = FailingWriter;
        let error = write_raw_stdout_chunk_to(&mut writer, "x").expect_err("broken pipe");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    /// The takeover lifecycle (`output-guard.ts:45-83`): idempotent
    /// takeover, restore clears the marker, restore without a takeover is
    /// a no-op.
    #[test]
    fn takeover_and_restore_round_trip() {
        let _serialized = TAKEOVER_TEST_LOCK.lock().expect("test lock");
        assert!(!is_stdout_taken_over());
        take_over_stdout();
        assert!(is_stdout_taken_over());
        // A second takeover is a no-op (`output-guard.ts:46-48`).
        take_over_stdout();
        assert!(is_stdout_taken_over());
        restore_stdout();
        assert!(!is_stdout_taken_over());
        // Restoring without a takeover is a no-op (`output-guard.ts:73-75`).
        restore_stdout();
        assert!(!is_stdout_taken_over());
    }
}
