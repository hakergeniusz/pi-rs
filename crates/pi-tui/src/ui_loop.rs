//! Autonomous timers and a thread-safe render waker.
//!
//! Ground truth: the timer half of yannelli/pi-rust
//! `crates/pi-tui/src/ui_loop.rs:104-209,221-243` (`TimerId`, `set_timeout`,
//! `set_interval`, `clear_timer`, due-timer dispatch). Upstream runs timers on
//! a single-threaded UI loop and dispatches callbacks from `run_pending`; this
//! crate has no loop, so callbacks run on one dedicated timer thread and must
//! be `Send`. They request a frame through [`TuiRenderWaker`], the cross-thread
//! handle to a [`Tui`](crate::tui::Tui)'s pending render flag.
//!
//! The timer thread sleeps on a [`Condvar`] until the next deadline, so no
//! host polling is required for a timer to fire.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Cloneable, `Send + Sync` handle to a [`Tui`](crate::tui::Tui)'s pending
/// render flag. Timer callbacks use it for upstream's
/// `tui.requestRender(false)` without owning the TUI.
#[derive(Clone, Debug, Default)]
pub struct TuiRenderWaker(Arc<AtomicBool>);

impl TuiRenderWaker {
    /// Flags a throttled frame as pending. Safe to call from any thread.
    pub fn request_render(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether a frame is pending.
    pub fn render_requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Clears the flag, reporting whether a frame was pending. The TUI calls
    /// this as it sends a frame out.
    pub(crate) fn take(&self) -> bool {
        self.0.swap(false, Ordering::SeqCst)
    }
}

/// Identifier of a timer created with [`set_timeout`] or [`set_interval`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TimerId(u64);

type Callback = Arc<dyn Fn() + Send + Sync + 'static>;

struct Timer {
    deadline: Instant,
    interval: Option<Duration>,
    callback: Callback,
}

struct Registry {
    timers: HashMap<u64, Timer>,
    next_id: u64,
}

struct TimerState {
    registry: Mutex<Registry>,
    changed: Condvar,
}

static TIMER_STATE: OnceLock<Arc<TimerState>> = OnceLock::new();

fn state() -> &'static Arc<TimerState> {
    TIMER_STATE.get_or_init(|| {
        let state = Arc::new(TimerState {
            registry: Mutex::new(Registry {
                timers: HashMap::new(),
                next_id: 1,
            }),
            changed: Condvar::new(),
        });
        let thread_state = Arc::clone(&state);
        std::thread::Builder::new()
            .name("pi-tui-timer".to_owned())
            .spawn(move || run(&thread_state))
            .expect("spawn pi-tui timer thread");
        state
    })
}

fn run(state: &TimerState) {
    loop {
        let mut registry = state.registry.lock().expect("timer registry");
        let now = Instant::now();
        let mut due: Vec<(Instant, u64, Callback)> = registry
            .timers
            .iter()
            .filter(|(_, timer)| timer.deadline <= now)
            .map(|(&id, timer)| (timer.deadline, id, Arc::clone(&timer.callback)))
            .collect();
        if due.is_empty() {
            if let Some(deadline) = registry.timers.values().map(|timer| timer.deadline).min() {
                let (guard, _) = state
                    .changed
                    .wait_timeout(registry, deadline.saturating_duration_since(now))
                    .expect("timer registry");
                registry = guard;
            } else {
                registry = state.changed.wait(registry).expect("timer registry");
            }
            continue;
        }
        due.sort_unstable_by_key(|timer| (timer.0, timer.1));
        for (deadline, id, _) in &due {
            if let Some(timer) = registry.timers.get_mut(id)
                && timer.deadline == *deadline
            {
                match timer.interval {
                    Some(period) => {
                        // Skip missed beats instead of bursting to catch up,
                        // the way upstream advances from the current time.
                        timer.deadline = Instant::now().max(*deadline + period);
                    }
                    None => {
                        registry.timers.remove(id);
                    }
                }
            }
        }
        drop(registry);
        for (_, _, callback) in due {
            // A panicking callback must not take down every other timer.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback()));
        }
    }
}

fn add_timer(delay: Duration, interval: Option<Duration>, callback: Callback) -> TimerId {
    let state = state();
    let mut registry = state.registry.lock().expect("timer registry");
    let id = registry.next_id;
    registry.next_id += 1;
    // A zero period would spin the timer thread; browsers clamp it too.
    let period = interval.unwrap_or(delay).max(Duration::from_millis(1));
    let deadline = Instant::now() + period;
    registry.timers.insert(
        id,
        Timer {
            deadline,
            interval: interval.map(|_| period),
            callback,
        },
    );
    drop(registry);
    state.changed.notify_one();
    TimerId(id)
}

/// Runs `callback` once after `delay`, on the timer thread (upstream
/// `setTimeout`).
pub fn set_timeout(delay: Duration, callback: impl FnOnce() + Send + 'static) -> TimerId {
    let callback = Mutex::new(Some(callback));
    add_timer(
        delay,
        None,
        Arc::new(move || {
            if let Some(callback) = callback.lock().expect("timeout callback").take() {
                callback();
            }
        }),
    )
}

/// Runs `callback` every `period` until [`clear_timer`] (upstream
/// `setInterval`). A zero period is clamped to 1 ms.
pub fn set_interval(period: Duration, callback: impl FnMut() + Send + 'static) -> TimerId {
    let callback = Mutex::new(callback);
    add_timer(
        period,
        Some(period),
        Arc::new(move || {
            if let Ok(mut callback) = callback.lock() {
                callback();
            }
        }),
    )
}

/// Cancels a timer. Unknown or already-fired timers are ignored. A callback
/// already dispatched may still complete.
pub fn clear_timer(id: TimerId) {
    let Some(state) = TIMER_STATE.get() else {
        return;
    };
    let mut registry = state.registry.lock().expect("timer registry");
    if registry.timers.remove(&id.0).is_some() {
        state.changed.notify_one();
    }
}

/// Whether `id` is still scheduled.
pub fn timer_active(id: TimerId) -> bool {
    TIMER_STATE.get().is_some_and(|state| {
        state
            .registry
            .lock()
            .expect("timer registry")
            .timers
            .contains_key(&id.0)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn interval_fires_and_clear_stops_it() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_callback = Arc::clone(&hits);
        let id = set_interval(Duration::from_millis(10), move || {
            hits_for_callback.fetch_add(1, Ordering::SeqCst);
        });
        assert!(timer_active(id));
        std::thread::sleep(Duration::from_millis(120));
        assert!(hits.load(Ordering::SeqCst) >= 2, "the timer thread fired");
        clear_timer(id);
        assert!(!timer_active(id));
        std::thread::sleep(Duration::from_millis(50)); // let a tick in flight settle
        hits.store(0, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "a cleared timer stays quiet"
        );
    }

    #[test]
    fn timeout_fires_once_and_waker_requests_a_render() {
        let waker = TuiRenderWaker::default();
        let waker_for_callback = waker.clone();
        let id = set_timeout(Duration::from_millis(20), move || {
            waker_for_callback.request_render();
        });
        std::thread::sleep(Duration::from_millis(150));
        assert!(!timer_active(id), "a timeout removes itself");
        assert!(waker.render_requested(), "its callback requested a render");
    }

    #[test]
    fn waker_flag_can_be_taken() {
        let waker = TuiRenderWaker::default();
        assert!(!waker.take());
        waker.request_render();
        assert!(waker.take(), "the flag was pending");
        assert!(!waker.take(), "and is cleared once consumed");
    }
}
