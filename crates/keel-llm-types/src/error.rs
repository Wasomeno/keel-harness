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
            // Providers use 413 for "request too large". In an LLM request the
            // usual cause is the context, so the session gets one compaction
            // and one replay; a genuinely oversized non-context payload fails
            // again normally after that retry.
            413 => Self::ContextOverflow,
            422 => Self::InvalidRequest,
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
        let context_overflow = status == 413
            || ((status == 400 || status == 422) && Self::is_context_overflow(&lowered, None));
        let kind = if context_overflow {
            ErrorKind::ContextOverflow
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

    /// Classify an error reported inside a stream, where there is no HTTP
    /// status. `default` applies when the message and provider code carry no
    /// known signal. The message classifier is the same one the HTTP path
    /// uses, so realistic overflow wording and codes reach the compaction
    /// path instead of being treated as a retryable server error.
    pub fn classify_frame(
        default: ErrorKind,
        message: impl Into<String>,
        code: Option<&str>,
    ) -> Self {
        let message = message.into();
        let lowered = message.to_ascii_lowercase();
        let kind = if Self::is_context_overflow(&lowered, code) {
            ErrorKind::ContextOverflow
        } else {
            default
        };
        let mut error = Self::new(kind, message);
        if let Some(code) = code {
            error = error.with_provider_code(code);
        }
        error
    }

    /// Overflow wording and provider codes seen across OpenAI, Anthropic,
    /// OpenRouter-style gateways and local servers. The message is already
    /// lowercase; the code is compared lowercase too.
    fn is_context_overflow(lowered: &str, code: Option<&str>) -> bool {
        const PHRASES: &[&str] = &[
            "context length",
            "context window",
            "maximum context",
            "too many tokens",
            "exceeds the maximum",
            "prompt is too long",
            "input is too long",
            "reduce the length of the message",
            "request too large",
            "request_too_large",
            "input length",
        ];
        const CODES: &[&str] = &[
            "context_length_exceeded",
            "context_window_exceeded",
            "model_context_window_exceeded",
            "context_overflow",
            "request_too_large",
            "prompt_too_long",
            "input_too_long",
            "too_many_tokens",
        ];
        PHRASES.iter().any(|phrase| lowered.contains(phrase))
            || code.is_some_and(|code| {
                let code = code.to_ascii_lowercase();
                CODES.iter().any(|known| code.contains(known))
            })
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
    fn classifies_real_provider_overflow_wording() {
        let cases = [
            (400, "prompt is too long: 208000 tokens > 200000 maximum"),
            (
                400,
                "This model's maximum context length is 128000 tokens. Please reduce the length of the messages.",
            ),
            (400, "input is too long for the requested model"),
            (422, "the request exceeds the maximum input length"),
        ];
        for (status, message) in cases {
            let error = LlmError::classify(status, message);
            assert_eq!(error.kind, ErrorKind::ContextOverflow, "{message}");
            assert!(!error.is_retryable(), "{message}");
        }
    }

    #[test]
    fn classifies_413_as_context_overflow() {
        assert_eq!(ErrorKind::from_status(413), ErrorKind::ContextOverflow);
        let error = LlmError::classify(413, "payload too large");
        assert_eq!(error.kind, ErrorKind::ContextOverflow);
    }

    #[test]
    fn classifies_an_in_stream_frame_by_message_or_code() {
        let by_code = LlmError::classify_frame(
            ErrorKind::ServerError,
            "the request failed",
            Some("context_length_exceeded"),
        );
        assert_eq!(by_code.kind, ErrorKind::ContextOverflow);
        assert_eq!(
            by_code.provider_code.as_deref(),
            Some("context_length_exceeded")
        );

        let by_message = LlmError::classify_frame(
            ErrorKind::ServerError,
            "prompt is too long: 208000 tokens > 200000 maximum",
            Some("invalid_request_error"),
        );
        assert_eq!(by_message.kind, ErrorKind::ContextOverflow);

        let ordinary = LlmError::classify_frame(
            ErrorKind::ServerError,
            "upstream connect error",
            Some("server_error"),
        );
        assert_eq!(ordinary.kind, ErrorKind::ServerError);
        assert!(ordinary.is_retryable());
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
