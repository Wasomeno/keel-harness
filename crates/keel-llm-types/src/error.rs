//! A single typed error taxonomy shared by transports and protocols.
//!
//! Retry policy itself lives at the session boundary. This type only answers
//! "is this retryable at all, and after how long".

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Auth,
    Permission,
    NotFound,
    InvalidRequest,
    ContextOverflow,
    RateLimit,
    QuotaExceeded,
    Overloaded,
    ServerError,
    Network,
    Timeout,
    Aborted,
    Decode,
    Unsupported,
}

impl ErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Permission => "permission",
            Self::NotFound => "not_found",
            Self::InvalidRequest => "invalid_request",
            Self::ContextOverflow => "context_overflow",
            Self::RateLimit => "rate_limit",
            Self::QuotaExceeded => "quota_exceeded",
            Self::Overloaded => "overloaded",
            Self::ServerError => "server_error",
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::Aborted => "aborted",
            Self::Decode => "decode",
            Self::Unsupported => "unsupported",
        }
    }

    /// Whether a retry could plausibly succeed without changing the request.
    pub const fn default_retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimit | Self::Overloaded | Self::ServerError | Self::Network | Self::Timeout
        )
    }

    pub fn from_status(status: u16) -> Self {
        match status {
            401 => Self::Auth,
            403 => Self::Permission,
            404 => Self::NotFound,
            408 => Self::Timeout,
            413 | 422 => Self::InvalidRequest,
            429 => Self::RateLimit,
            529 => Self::Overloaded,
            500..=599 => Self::ServerError,
            _ => Self::InvalidRequest,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmError {
    pub kind: ErrorKind,
    pub message: String,
    pub status: Option<u16>,
    pub retryable: bool,
    pub retry_after: Option<Duration>,
    pub provider_code: Option<String>,
}

impl LlmError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            retryable: kind.default_retryable(),
            retry_after: None,
            provider_code: None,
        }
    }

    pub fn classify(status: u16, message: impl Into<String>) -> Self {
        let message = message.into();
        let lowered = message.to_ascii_lowercase();
        let kind = if status == 400 || status == 422 {
            if lowered.contains("context length")
                || lowered.contains("context window")
                || lowered.contains("too many tokens")
                || lowered.contains("exceeds the maximum")
            {
                ErrorKind::ContextOverflow
            } else {
                ErrorKind::InvalidRequest
            }
        } else if status == 429
            && (lowered.contains("quota")
                || lowered.contains("insufficient")
                || lowered.contains("billing"))
        {
            ErrorKind::QuotaExceeded
        } else {
            ErrorKind::from_status(status)
        };

        Self::new(kind, message).with_status(status)
    }

    pub fn decode(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Decode, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_provider_code(mut self, code: impl Into<String>) -> Self {
        self.provider_code = Some(code.into());
        self
    }

    pub fn with_retry_after(mut self, after: Duration) -> Self {
        self.retry_after = Some(after);
        self
    }

    pub fn is_retryable(&self) -> bool {
        self.retryable
    }
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)?;
        if let Some(status) = self.status {
            write!(f, " (http {status})")?;
        }
        if let Some(code) = &self.provider_code {
            write!(f, " [{code}]")?;
        }
        Ok(())
    }
}

impl std::error::Error for LlmError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_context_overflow() {
        let error = LlmError::classify(400, "This model's maximum context length is 8192 tokens");
        assert_eq!(error.kind, ErrorKind::ContextOverflow);
        assert!(!error.is_retryable());
    }

    #[test]
    fn classifies_quota_separately_from_rate_limit() {
        let error = LlmError::classify(429, "insufficient quota for this billing period");
        assert_eq!(error.kind, ErrorKind::QuotaExceeded);
        let error = LlmError::classify(429, "rate limit reached");
        assert_eq!(error.kind, ErrorKind::RateLimit);
        assert!(error.is_retryable());
    }

    #[test]
    fn classifies_overloaded() {
        assert_eq!(ErrorKind::from_status(529), ErrorKind::Overloaded);
        assert!(ErrorKind::Overloaded.default_retryable());
    }
}
