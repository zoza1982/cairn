//! Retry-with-backoff for transient backend failures.
//!
//! Network backends (SSH, object stores, clusters) hit transient errors — dropped connections,
//! timeouts, throttling — that succeed on a retry. [`retry`] re-runs an operation while its
//! [`VfsError`] [`is_retryable`](VfsError::is_retryable) and attempts remain, sleeping with capped
//! exponential backoff between tries. The backoff schedule ([`backoff_delay`]) is a pure function so
//! it can be unit-tested without waiting, and non-retryable errors fail fast.

use crate::error::VfsError;
use std::future::Future;
use std::time::Duration;

/// How many times to try and how long to wait between tries.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts including the first (so `1` means no retry). `0` is treated as `1`.
    pub max_attempts: u32,
    /// Base delay; the wait before retry `n` (0-indexed) is `base * 2^n`, capped at `max_delay`.
    pub base_delay: Duration,
    /// Upper bound on any single backoff wait.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    /// 4 attempts with 100 ms / 200 ms / 400 ms backoff (≤ 700 ms total sleep), capped at 5 s —
    /// suitable for transient network blips; adjust for slower backends.
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(5),
        }
    }
}

impl RetryPolicy {
    /// A policy that never retries (one attempt).
    #[must_use]
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }
}

/// The capped exponential backoff delay before the retry following attempt `attempt` (0-indexed):
/// `base * 2^attempt`, clamped to `max`. Computed in `u128` nanoseconds so it always saturates to
/// `max` (a large `attempt` or `base` can never overflow or under-clamp).
///
// TODO: add per-call jitter (e.g. ±25% of the delay) to spread simultaneous retries across panes /
// transfer workers. Low priority for single-pane use; matters under high concurrency.
#[must_use]
pub fn backoff_delay(attempt: u32, base: Duration, max: Duration) -> Duration {
    let factor = 1u128.checked_shl(attempt).unwrap_or(u128::MAX);
    let nanos = base.as_nanos().saturating_mul(factor).min(max.as_nanos());
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// Run `op`, retrying while it fails with a retryable [`VfsError`] and attempts remain.
///
/// The first failure that is **not** retryable, or the last attempt's error, is returned. With
/// [`RetryPolicy::none`] this is a single call with no delay.
///
/// Must run on a Tokio runtime with the **time driver** enabled (the backoff uses
/// `tokio::time::sleep`); the app runtime enables it via `enable_all`.
///
/// # Errors
/// The final [`VfsError`] from `op`.
pub async fn retry<T, F, Fut>(policy: RetryPolicy, mut op: F) -> Result<T, VfsError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, VfsError>>,
{
    let max = policy.max_attempts.max(1);
    // All but the final attempt: on a retryable error, back off and try again.
    for attempt in 0..max.saturating_sub(1) {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if !e.is_retryable() => return Err(e),
            Err(_) => {
                let delay = backoff_delay(attempt, policy.base_delay, policy.max_delay);
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    // The final attempt: its result is returned unconditionally (no `unreachable!` needed).
    op().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn backoff_is_exponential_and_capped() {
        let base = Duration::from_millis(100);
        let max = Duration::from_secs(1);
        assert_eq!(backoff_delay(0, base, max), Duration::from_millis(100));
        assert_eq!(backoff_delay(1, base, max), Duration::from_millis(200));
        assert_eq!(backoff_delay(2, base, max), Duration::from_millis(400));
        // 100ms * 2^4 = 1600ms, capped at 1s.
        assert_eq!(backoff_delay(4, base, max), Duration::from_secs(1));
        // A huge exponent saturates to the cap, never panics.
        assert_eq!(backoff_delay(64, base, max), max);
        // Tiny base + large attempt still clamps exactly to `max` (no intermediate truncation).
        assert_eq!(
            backoff_delay(40, Duration::from_nanos(1), Duration::from_secs(10)),
            Duration::from_secs(10)
        );
        // A huge base saturates rather than overflowing.
        assert_eq!(backoff_delay(3, Duration::MAX, max), max);
    }

    fn flaky_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::ZERO, // no real waiting in tests
            max_delay: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let calls = Cell::new(0u32);
        let out: Result<u32, _> = retry(flaky_policy(), || {
            let n = calls.get() + 1;
            calls.set(n);
            async move {
                if n < 3 {
                    Err(VfsError::Timeout(Duration::from_secs(1))) // retryable
                } else {
                    Ok(n)
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), 3);
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn non_retryable_fails_fast() {
        let calls = Cell::new(0u32);
        let out: Result<(), _> = retry(flaky_policy(), || {
            calls.set(calls.get() + 1);
            async move { Err(VfsError::Unsupported(cairn_types::Caps::READ)) }
        })
        .await;
        assert!(matches!(out, Err(VfsError::Unsupported(_))));
        assert_eq!(calls.get(), 1); // not retried
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let calls = Cell::new(0u32);
        let out: Result<(), _> = retry(flaky_policy(), || {
            calls.set(calls.get() + 1);
            async move { Err(VfsError::Timeout(Duration::from_secs(1))) }
        })
        .await;
        assert!(matches!(out, Err(VfsError::Timeout(_))));
        assert_eq!(calls.get(), 4); // exactly max_attempts
    }

    #[tokio::test(start_paused = true)]
    async fn actually_sleeps_the_backoff_schedule() {
        // With virtual time, prove the backoff is awaited: two retryable failures then success
        // should consume base + 2*base = 300ms of (paused) time for a 100ms base.
        let policy = RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(5),
        };
        let calls = Cell::new(0u32);
        let start = tokio::time::Instant::now();
        let out: Result<u32, _> = retry(policy, || {
            let n = calls.get() + 1;
            calls.set(n);
            async move {
                if n < 3 {
                    Err(VfsError::Timeout(Duration::from_secs(1)))
                } else {
                    Ok(n)
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), 3);
        // 100ms (after attempt 0) + 200ms (after attempt 1).
        assert_eq!(start.elapsed(), Duration::from_millis(300));
    }

    #[tokio::test]
    async fn policy_none_does_not_retry() {
        let calls = Cell::new(0u32);
        let _: Result<(), _> = retry(RetryPolicy::none(), || {
            calls.set(calls.get() + 1);
            async move { Err(VfsError::Timeout(Duration::from_secs(1))) }
        })
        .await;
        assert_eq!(calls.get(), 1);
    }
}
