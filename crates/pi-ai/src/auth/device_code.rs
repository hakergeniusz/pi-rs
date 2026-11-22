//! RFC 8628 device-code polling (`packages/ai/src/auth/oauth/device-code.ts`).
//!
//! Ground truth: the cancel/timeout strings and interval constants
//! (`:1-9`), `abortableSleep` (`:26-44`), and `pollOAuthDeviceCodeFlow`
//! (`:46-98`) — including the 5-second default interval, the `slow_down`
//! increment, the server-provided interval override, and the distinct
//! slow-down timeout message.
//!
//! Deviations: `AbortSignal` is [`CancelToken`] and `setTimeout` is
//! `tokio::time::sleep`; the poll callback is a generic `FnMut` future.

use std::future::Future;
use std::time::Duration;

use super::{AuthError, CANCEL_MESSAGE, CancelToken, now_ms};

/// The minimum polling interval (`device-code.ts:5`).
const MINIMUM_INTERVAL_MS: u64 = 1000;
/// RFC 8628 section 3.2 default (`device-code.ts:7`).
const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
/// RFC 8628 section 3.5 `slow_down` increment (`device-code.ts:9`).
const SLOW_DOWN_INTERVAL_INCREMENT_MS: u64 = 5000;

/// `TIMEOUT_MESSAGE` (`device-code.ts:2`).
pub const TIMEOUT_MESSAGE: &str = "Device flow timed out";
/// `SLOW_DOWN_TIMEOUT_MESSAGE` (`device-code.ts:3-4`).
pub const SLOW_DOWN_TIMEOUT_MESSAGE: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";

/// `OAuthDeviceCodePollResult` (`device-code.ts:11-16`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceCodePollResult<T> {
    Pending,
    SlowDown { interval_seconds: Option<f64> },
    Failed(String),
    Complete(T),
}

/// `OAuthDeviceCodePollOptions` (`device-code.ts:18-24`).
pub struct DeviceCodePollOptions<'a, T, F> {
    pub interval_seconds: Option<f64>,
    pub expires_in_seconds: Option<u64>,
    pub wait_before_first_poll: bool,
    pub poll: F,
    pub cancel: &'a CancelToken,
    pub _marker: std::marker::PhantomData<T>,
}

impl<'a, T, F> DeviceCodePollOptions<'a, T, F> {
    #[must_use]
    pub fn new(cancel: &'a CancelToken, poll: F) -> Self {
        Self {
            interval_seconds: None,
            expires_in_seconds: None,
            wait_before_first_poll: false,
            poll,
            cancel,
            _marker: std::marker::PhantomData,
        }
    }
}

/// `abortableSleep` (`device-code.ts:26-44`).
///
/// # Errors
/// [`AuthError::Cancelled`] when `cancel` fires before the timer.
pub async fn abortable_sleep(
    milliseconds: u64,
    cancel: &CancelToken,
    cancel_message: &str,
) -> Result<(), AuthError> {
    if cancel.is_cancelled() {
        return Err(AuthError::Message(cancel_message.to_string()));
    }
    tokio::select! {
        () = tokio::time::sleep(Duration::from_millis(milliseconds)) => Ok(()),
        () = cancel.cancelled() => Err(AuthError::Message(cancel_message.to_string())),
    }
}

/// `pollOAuthDeviceCodeFlow` (`device-code.ts:46-98`).
///
/// # Errors
/// The poll's `Failed` message, cancellation, or the timeout message.
pub async fn poll_oauth_device_code_flow<T, F, Fut>(
    options: DeviceCodePollOptions<'_, T, F>,
) -> Result<T, AuthError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = DeviceCodePollResult<T>>,
{
    let deadline = options.expires_in_seconds.map_or(i64::MAX, |seconds| {
        now_ms().saturating_add(
            i64::try_from(seconds)
                .unwrap_or(i64::MAX)
                .saturating_mul(1000),
        )
    });
    let mut interval_ms = interval_from_seconds(
        options
            .interval_seconds
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS),
        None,
    );
    let mut slow_down_responses = 0_u32;
    let mut poll = options.poll;

    if options.wait_before_first_poll {
        let remaining = deadline.saturating_sub(now_ms());
        if remaining > 0 {
            abortable_sleep(
                interval_ms.min(u64::try_from(remaining).unwrap_or(u64::MAX)),
                options.cancel,
                CANCEL_MESSAGE,
            )
            .await?;
        }
    }

    loop {
        let now = now_ms();
        if now >= deadline {
            break;
        }
        if options.cancel.is_cancelled() {
            return Err(AuthError::Message(CANCEL_MESSAGE.to_string()));
        }

        match poll().await {
            DeviceCodePollResult::Complete(value) => return Ok(value),
            DeviceCodePollResult::Failed(message) => return Err(AuthError::Message(message)),
            DeviceCodePollResult::SlowDown { interval_seconds } => {
                slow_down_responses += 1;
                interval_ms =
                    interval_from_seconds(interval_seconds.unwrap_or(0.0), Some(interval_ms));
            }
            DeviceCodePollResult::Pending => {}
        }

        let remaining = deadline.saturating_sub(now_ms());
        if remaining <= 0 {
            break;
        }
        abortable_sleep(
            interval_ms.min(u64::try_from(remaining).unwrap_or(u64::MAX)),
            options.cancel,
            CANCEL_MESSAGE,
        )
        .await?;
    }

    if slow_down_responses > 0 {
        Err(AuthError::Message(SLOW_DOWN_TIMEOUT_MESSAGE.to_string()))
    } else {
        Err(AuthError::Message(TIMEOUT_MESSAGE.to_string()))
    }
}

fn interval_from_seconds(seconds: f64, previous_ms: Option<u64>) -> u64 {
    if seconds.is_finite() && seconds > 0.0 {
        // GitHub reports the new minimum in `interval`; trust it over the
        // client-tracked +5s value (`device-code.ts:78-86`).
        return MINIMUM_INTERVAL_MS.max((seconds * 1000.0) as u64);
    }
    previous_ms.map_or(MINIMUM_INTERVAL_MS, |previous| {
        MINIMUM_INTERVAL_MS.max(previous.saturating_add(SLOW_DOWN_INTERVAL_INCREMENT_MS))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[tokio::test]
    async fn pending_then_complete_polls_until_success() {
        let cancel = CancelToken::new();
        let calls = Cell::new(0_u32);
        let mut options = DeviceCodePollOptions::new(&cancel, || {
            let call = calls.get();
            calls.set(call + 1);
            std::future::ready(if call == 0 {
                DeviceCodePollResult::Pending
            } else {
                DeviceCodePollResult::Complete("token".to_string())
            })
        });
        options.interval_seconds = Some(0.0);
        options.expires_in_seconds = Some(30);
        assert_eq!(
            poll_oauth_device_code_flow(options)
                .await
                .expect("complete"),
            "token"
        );
        assert_eq!(calls.get(), 2);
    }

    #[tokio::test]
    async fn failure_message_is_forwarded() {
        let cancel = CancelToken::new();
        let options = DeviceCodePollOptions::new(&cancel, || {
            std::future::ready(DeviceCodePollResult::<()>::Failed(
                "Device flow failed: access_denied".to_string(),
            ))
        });
        assert_eq!(
            poll_oauth_device_code_flow(options)
                .await
                .unwrap_err()
                .to_string(),
            "Device flow failed: access_denied"
        );
    }

    #[tokio::test]
    async fn timeout_uses_the_upstream_message() {
        let cancel = CancelToken::new();
        let mut options = DeviceCodePollOptions::new(&cancel, || {
            std::future::ready(DeviceCodePollResult::<()>::Pending)
        });
        options.expires_in_seconds = Some(0);
        assert_eq!(
            poll_oauth_device_code_flow(options)
                .await
                .unwrap_err()
                .to_string(),
            TIMEOUT_MESSAGE
        );
    }

    #[tokio::test]
    async fn slow_down_timeout_uses_the_special_message() {
        let cancel = CancelToken::new();
        let mut options = DeviceCodePollOptions::new(&cancel, || {
            std::future::ready(DeviceCodePollResult::<()>::SlowDown {
                interval_seconds: None,
            })
        });
        options.expires_in_seconds = Some(1);
        assert_eq!(
            poll_oauth_device_code_flow(options)
                .await
                .unwrap_err()
                .to_string(),
            SLOW_DOWN_TIMEOUT_MESSAGE
        );
    }

    #[tokio::test]
    async fn cancel_rejects_a_pending_flow() {
        let cancel = CancelToken::new();
        let token = cancel.clone();
        let options = DeviceCodePollOptions::new(&cancel, move || {
            token.cancel();
            std::future::ready(DeviceCodePollResult::<()>::Pending)
        });
        assert_eq!(
            poll_oauth_device_code_flow(options)
                .await
                .unwrap_err()
                .to_string(),
            CANCEL_MESSAGE
        );
    }

    #[test]
    fn slow_down_uses_the_server_interval_when_given() {
        assert_eq!(interval_from_seconds(2.0, Some(1000)), 2000);
        assert_eq!(interval_from_seconds(0.0, Some(1000)), 6000);
        assert_eq!(interval_from_seconds(0.0, None), 1000);
    }
}
