//! Wheel-event acceleration.
//!
//! Ground truth: `packages/tui/src/wheel-scroll.ts:1-82`.
//!
//! Terminals disagree about what a wheel notch is worth. macOS terminals
//! receive deltas the OS already accelerated and emit one event per line;
//! everywhere else one event is one notch. [`WheelScrollLines::Auto`] smooths
//! that out by scaling with event velocity.

/// Lines moved per wheel event. `wheel-scroll.ts:3`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WheelScrollLines {
    /// A fixed line count.
    Fixed(f64),
    /// Accelerate fast wheel spins, capped at [`MAX_AUTO_LINES`].
    Auto,
}

/// Events closer than this belong to one physical notch (Ghostty emits them
/// ~4 ms apart) or come from a high-resolution source. `wheel-scroll.ts:6-7`.
pub const BURST_GAP_MS: f64 = 5.0;
/// A pause longer than this ends a scroll gesture. `wheel-scroll.ts:9`.
pub const GESTURE_GAP_MS: f64 = 200.0;
/// Average event gap that maps to one line per event. `wheel-scroll.ts:11-12`.
pub const REFERENCE_GAP_MS: f64 = 100.0;
/// Ceiling on auto-accelerated lines per event. `wheel-scroll.ts:13`.
pub const MAX_AUTO_LINES: f64 = 6.0;

/// `terminalAcceleratesWheel`, `wheel-scroll.ts:21-28`.
///
/// True on macOS outside SSH: the client platform there is known and the OS
/// has already scaled the deltas. Over SSH the client is unknown, so pi
/// assumes notch-per-event like everywhere else.
pub fn terminal_accelerates_wheel(is_darwin: bool, ssh: SshEnv) -> bool {
    is_darwin && !ssh.any()
}

/// Which SSH variables were set. `wheel-scroll.ts:23-25`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SshEnv {
    pub connection: bool,
    pub client: bool,
    pub tty: bool,
}

impl SshEnv {
    /// Whether any SSH variable was set (macOS then keeps one line per event).
    pub fn any(&self) -> bool {
        self.connection || self.client || self.tty
    }

    /// Read `SSH_CONNECTION` / `SSH_CLIENT` / `SSH_TTY` from the live
    /// environment, as pi's `process.env` lookup does.
    pub fn from_process() -> Self {
        let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        Self {
            connection: set("SSH_CONNECTION"),
            client: set("SSH_CLIENT"),
            tty: set("SSH_TTY"),
        }
    }
}

/// `WheelScrollAccelerator`, `wheel-scroll.ts:34-80`.
#[derive(Debug, Clone)]
pub struct WheelScrollAccelerator {
    lines: WheelScrollLines,
    accelerate: bool,
    last_time: f64,
    last_direction: i64,
    average_gap: Option<f64>,
    carry: f64,
}

impl Default for WheelScrollAccelerator {
    /// pi's default constructor calls `terminalAcceleratesWheel()` against the
    /// live process (`wheel-scroll.ts:36`). Tests use
    /// [`WheelScrollAccelerator::for_platform`] so they never read the ambient
    /// environment.
    fn default() -> Self {
        Self::for_platform(
            WheelScrollLines::Auto,
            cfg!(target_os = "macos"),
            SshEnv::from_process(),
        )
    }
}

impl WheelScrollAccelerator {
    /// `constructor`, `wheel-scroll.ts:50-53`.
    pub fn new(lines: WheelScrollLines, accelerate: bool) -> Self {
        Self {
            lines,
            accelerate,
            ..Self::reset_state()
        }
    }

    /// Build one with pi's default acceleration for this platform.
    pub fn for_platform(lines: WheelScrollLines, is_darwin: bool, ssh: SshEnv) -> Self {
        Self::new(lines, !terminal_accelerates_wheel(is_darwin, ssh))
    }

    fn reset_state() -> Self {
        Self {
            lines: WheelScrollLines::Auto,
            accelerate: true,
            last_time: f64::NEG_INFINITY,
            last_direction: 0,
            average_gap: None,
            carry: 0.0,
        }
    }

    /// `setLines`, `wheel-scroll.ts:55-58`. Changing the count starts a new
    /// gesture so the carried fractional line is dropped.
    pub fn set_lines(&mut self, lines: WheelScrollLines) {
        self.lines = lines;
        self.reset();
    }

    /// The configured line mode, as set by [`WheelScrollAccelerator::set_lines`].
    pub fn lines(&self) -> WheelScrollLines {
        self.lines
    }

    /// `next`, `wheel-scroll.ts:60-78`. `direction` is `-1` for up and `+1`
    /// for down; `now_ms` is the host's monotonic clock in milliseconds.
    pub fn next(&mut self, direction: i64, now_ms: f64) -> i64 {
        if let WheelScrollLines::Fixed(lines) = self.lines {
            return if lines.is_finite() {
                (lines.floor() as i64).max(1)
            } else {
                1
            };
        }
        if !self.accelerate {
            return 1;
        }

        let gap = now_ms - self.last_time;
        let same_gesture = direction == self.last_direction && gap <= GESTURE_GAP_MS;
        self.last_time = now_ms;
        self.last_direction = direction;
        if !same_gesture {
            self.average_gap = None;
            self.carry = 0.0;
            return 1;
        }
        if gap < BURST_GAP_MS {
            return 1;
        }

        self.average_gap = Some(match self.average_gap {
            // Plain averaging matches upstream `wheel-scroll.ts`; `f64::midpoint`
            // is stable only since 1.87, past the crate's 1.85 MSRV.
            #[allow(clippy::manual_midpoint)]
            Some(average) => (average + gap) / 2.0,
            None => gap,
        });
        let average_gap = self.average_gap.unwrap_or(REFERENCE_GAP_MS);
        let lines = MAX_AUTO_LINES.min((REFERENCE_GAP_MS / average_gap).max(1.0)) + self.carry;
        let whole = lines.floor();
        self.carry = lines - whole;
        whole as i64
    }

    /// `reset`, `wheel-scroll.ts:80-85`.
    pub fn reset(&mut self) {
        self.last_time = f64::NEG_INFINITY;
        self.last_direction = 0;
        self.average_gap = None;
        self.carry = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto() -> WheelScrollAccelerator {
        WheelScrollAccelerator::new(WheelScrollLines::Auto, true)
    }

    fn scroll(accelerator: &mut WheelScrollAccelerator, times: &[f64]) -> Vec<i64> {
        times.iter().map(|now| accelerator.next(1, *now)).collect()
    }

    fn scroll_dir(
        accelerator: &mut WheelScrollAccelerator,
        times: &[f64],
        direction: i64,
    ) -> Vec<i64> {
        times
            .iter()
            .map(|now| accelerator.next(direction, *now))
            .collect()
    }

    /// "uses fixed line counts regardless of timing". `wheel-scroll.test.ts:11-17`.
    #[test]
    fn fixed_line_counts_ignore_timing() {
        let mut accelerator = WheelScrollAccelerator::new(WheelScrollLines::Fixed(3.0), true);
        assert_eq!(
            scroll(&mut accelerator, &[0.0, 10.0, 20.0, 1000.0]),
            vec![3, 3, 3, 3]
        );
        accelerator.set_lines(WheelScrollLines::Fixed(0.5));
        assert_eq!(accelerator.next(1, 2000.0), 1);
    }

    /// "keeps one line per event in auto mode when the terminal already
    /// accelerates". `wheel-scroll.test.ts:19-21`.
    #[test]
    fn accelerated_terminal_keeps_one_line_per_event() {
        let mut accelerator = WheelScrollAccelerator::new(WheelScrollLines::Auto, false);
        assert_eq!(
            scroll(&mut accelerator, &[0.0, 10.0, 20.0, 30.0]),
            vec![1, 1, 1, 1]
        );
    }

    /// "scales auto mode with wheel velocity". `wheel-scroll.test.ts:23-29`.
    /// The four sustained four-event sequences, exactly as pi asserts them.
    #[test]
    // Bindings are named for the gap they assert (150/50/20/10 ms), mirroring
    // `wheel-scroll.test.ts`.
    #[allow(clippy::similar_names)]
    fn scales_with_wheel_velocity() {
        let mut at_150 = auto();
        assert_eq!(
            scroll(&mut at_150, &[0.0, 150.0, 300.0, 450.0]),
            vec![1, 1, 1, 1]
        );

        let mut at_50 = auto();
        assert_eq!(
            scroll(&mut at_50, &[1000.0, 1050.0, 1100.0, 1150.0]),
            vec![1, 2, 2, 2]
        );

        let mut at_20 = auto();
        assert_eq!(
            scroll(&mut at_20, &[2000.0, 2020.0, 2040.0, 2060.0]),
            vec![1, 5, 5, 5]
        );

        let mut at_10 = auto();
        assert_eq!(
            scroll(&mut at_10, &[3000.0, 3010.0, 3020.0, 3030.0]),
            vec![1, 6, 6, 6]
        );
    }

    /// "does not accelerate bursts of events for a single notch".
    /// `wheel-scroll.test.ts:37-39`.
    #[test]
    fn bursts_of_one_notch_never_accelerate() {
        let mut accelerator = auto();
        assert_eq!(
            scroll(&mut accelerator, &[0.0, 3.0, 6.0, 9.0]),
            vec![1, 1, 1, 1]
        );
    }

    /// "resets acceleration on direction changes and pauses".
    /// `wheel-scroll.test.ts:41-46`. The flip only has teeth once the
    /// accelerator has actually reached a multi-line speed.
    #[test]
    fn direction_change_and_pause_reset_acceleration() {
        let mut accelerator = auto();
        assert_eq!(scroll(&mut accelerator, &[0.0, 20.0, 40.0]), vec![1, 5, 5]);
        assert_eq!(accelerator.next(-1, 60.0), 1, "direction flip resets");
        assert_eq!(scroll(&mut accelerator, &[500.0, 520.0]), vec![1, 5]);
    }

    /// "carries fractional lines between events". `wheel-scroll.test.ts:48-51`.
    #[test]
    fn fractional_lines_carry_between_events() {
        let mut accelerator = auto();
        assert_eq!(
            scroll(&mut accelerator, &[0.0, 40.0, 80.0, 120.0, 160.0]),
            vec![1, 2, 3, 2, 3]
        );
    }

    #[test]
    fn never_exceeds_the_maximum() {
        let mut accelerator = auto();
        accelerator.next(1, 0.0);
        let mut previous = 0;
        for step in 1..200 {
            let lines = accelerator.next(1, f64::from(step) * 6.0);
            assert!(
                lines <= MAX_AUTO_LINES as i64,
                "step {step} produced {lines}"
            );
            assert!(lines >= previous, "acceleration must be monotonic");
            previous = lines;
        }
        assert_eq!(previous, MAX_AUTO_LINES as i64);
    }

    #[test]
    fn carry_carries_the_fraction_across_events() {
        // A steady 60ms gap averages 1.67 lines per event. Without the carry
        // the sequence would truncate to 1 forever; with it the extra line
        // lands on a later notch instead of being lost.
        let mut accelerator = auto();
        accelerator.next(1, 0.0);
        let observed: Vec<i64> = (1..=4)
            .map(|step| accelerator.next(1, f64::from(step) * 60.0))
            .collect();
        assert_eq!(observed, vec![1, 2, 2, 1]);
        assert_eq!(observed.iter().sum::<i64>(), 6);
    }

    /// The burst guard keeps a physical notch worth one line even when the
    /// events arrive far faster than any real wheel.
    #[test]
    fn burst_guard_never_exceeds_one_line_per_notch() {
        let mut accelerator = auto();
        let observed = scroll_dir(
            &mut accelerator,
            &(1..=200).map(f64::from).collect::<Vec<_>>(),
            1,
        );
        assert!(observed.iter().all(|lines| *lines == 1), "{observed:?}");
    }

    /// `reset`, `wheel-scroll.ts:80-85`: the pause longer than
    /// `GESTURE_GAP_MS` starts a new gesture at one line.
    #[test]
    fn pause_longer_than_the_gesture_gap_restarts() {
        let mut accelerator = auto();
        assert_eq!(accelerator.next(1, 0.0), 1);
        assert_eq!(accelerator.next(1, 201.0), 1, "gap > GESTURE_GAP_MS");
    }

    #[test]
    fn set_lines_drops_the_carry() {
        let mut accelerator = auto();
        accelerator.next(1, 0.0);
        accelerator.next(1, 60.0);
        accelerator.set_lines(WheelScrollLines::Fixed(1.0));
        assert_eq!(accelerator.next(1, 120.0), 1);
    }

    #[test]
    fn macos_local_accelerates_ssh_does_not() {
        assert!(terminal_accelerates_wheel(true, SshEnv::default()));
        assert!(!terminal_accelerates_wheel(false, SshEnv::default()));
        assert!(!terminal_accelerates_wheel(
            true,
            SshEnv {
                connection: true,
                ..SshEnv::default()
            }
        ));
        assert!(!terminal_accelerates_wheel(
            true,
            SshEnv {
                client: true,
                ..SshEnv::default()
            }
        ));
        assert!(!terminal_accelerates_wheel(
            true,
            SshEnv {
                tty: true,
                ..SshEnv::default()
            }
        ));
        let mut accelerator =
            WheelScrollAccelerator::for_platform(WheelScrollLines::Auto, true, SshEnv::default());
        assert_eq!(accelerator.next(1, 0.0), 1);
    }
}
