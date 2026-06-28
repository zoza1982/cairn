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
    /// Total attempts including the first (so `1` means no retry).
    pub max_attempts: u32,
    /// Base delay; the wait before retry `n` (0-indexed) is `base * 2^n`, capped at `max_delay`.
    pub base_delay: Duration,
    /// Upper bound on any single backoff wait.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
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
/// `base * 2^attempt`, clamped to `max`. Saturating, so a large `attempt` cannot overflow.
#[must_use]
pub fn backoff_delay(attempt: u32, base: Duration, max: Duration) -> Duration {
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    base.checked_mul(u32::try_from(factor).unwrap_or(u32::MAX))
        .unwrap_or(max)
        .min(max)
}

/// Run `op`, retrying while it fails with a retryable [`VfsError`] and attempts remain.
///
/// The first failure that is **not** retryable, or the last attempt's error, is returned. With
/// [`RetryPolicy::none`] this is a single call with no delay.
///
/// # Errors
/// The final [`VfsError`] from `op`.
pub async fn retry<T, F, Fut>(policy: RetryPolicy, mut op: F) -> Result<T, VfsError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, VfsError>>,
{
    let attempts = policy.max_attempts.max(1);
    for attempt in 0..attempts {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let last = attempt + 1 >= attempts;
                if last || !e.is_retryable() {
                    return Err(e);
                }
                let delay = backoff_delay(attempt, policy.base_delay, policy.max_delay);
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    // Unreachable: the loop always returns on the final attempt.
    unreachable!("retry loop exhausted without returning")
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
