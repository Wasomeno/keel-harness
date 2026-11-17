//! Narrow pre-stream retry policy.
//!
//! Replay safety is a session concern: once bytes have reached the caller, the
//! adapter must not retry. This policy exists only so a connect blip or a 429
//! before the first byte does not surface as a failed step.

use std::time::Duration;

use keel_llm_types::LlmError;

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryDecision {
    Retry { after: Duration },
    Fail,
}

impl RetryPolicy {
    /// `attempt` is 1-based: the attempt that just failed.
    pub fn decide(&self, error: &LlmError, attempt: u32) -> RetryDecision {
        if attempt >= self.max_attempts || !error.is_retryable() {
            return RetryDecision::Fail;
        }
        let backoff = self.base_delay * 2u32.pow(attempt.saturating_sub(1).min(6));
        let after = error.retry_after.unwrap_or(backoff).min(self.max_delay);
        RetryDecision::Retry { after }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_llm_types::{ErrorKind, LlmError};

    #[test]
    fn retries_a_rate_limit_with_server_hint() {
        let error = LlmError::new(ErrorKind::RateLimit, "slow down")
            .with_retry_after(Duration::from_secs(2));
        assert_eq!(
            RetryPolicy::default().decide(&error, 1),
            RetryDecision::Retry {
                after: Duration::from_secs(2)
            }
        );
    }

    #[test]
    fn caps_hints_at_the_policy_maximum() {
        let error = LlmError::new(ErrorKind::RateLimit, "slow down")
            .with_retry_after(Duration::from_secs(600));
        assert_eq!(
            RetryPolicy::default().decide(&error, 1),
            RetryDecision::Retry {
                after: Duration::from_secs(10)
            }
        );
    }

    #[test]
    fn does_not_retry_auth_or_invalid_requests() {
        for kind in [
            ErrorKind::Auth,
            ErrorKind::InvalidRequest,
            ErrorKind::ContextOverflow,
        ] {
            let error = LlmError::new(kind, "no");
            assert_eq!(
                RetryPolicy::default().decide(&error, 1),
                RetryDecision::Fail
            );
        }
    }

    #[test]
    fn stops_after_max_attempts() {
        let error = LlmError::new(ErrorKind::ServerError, "boom");
        assert_eq!(
            RetryPolicy::default().decide(&error, 3),
            RetryDecision::Fail
        );
    }
}
