//! HTTP transport.
//!
//! The client owns the request lifecycle until a successful response stream is
//! handed back. After that it never retries: replay safety belongs to the
//! session loop. `EventStream` bounds the gap between body chunks with an idle
//! timeout; it does not impose a total request deadline.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use keel_llm_protocol::StreamDecoder;
use keel_llm_types::{ErrorKind, LlmError, LlmEvent, Request};

use crate::auth::{EnvResolver, SecretResolver};
use crate::retry::{RetryDecision, RetryPolicy};
use crate::route::{Framing, Route};
use crate::sse::SseDecoder;

/// Environment variable for the idle timeout between response body chunks.
///
/// Whole seconds; `0` disables the bound. The default is
/// [`DEFAULT_STREAM_IDLE_TIMEOUT`].
pub const STREAM_IDLE_TIMEOUT_ENV: &str = "KEEL_STREAM_IDLE_TIMEOUT_SECS";

/// Default idle timeout between response body chunks.
///
/// This is deliberately generous: a provider may buffer a long reasoning or
/// generation phase without sending body bytes, and killing a legitimate slow
/// turn is worse than waiting out a stall. It is not a total request deadline.
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Largest idle timeout that can be armed.
///
/// Longer values are clamped by [`Client::with_resolver`] and
/// [`Client::with_stream_idle_timeout`]. Tokio clamps sleeps to roughly 30
/// years; the same bound keeps re-arming the timer after a chunk from
/// overflowing on every supported platform.
pub const MAX_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 365 * 24 * 60 * 60);

/// A non-2xx error body is read for at most this long, or the idle timeout when
/// that is shorter. The response body is not a stream the caller can bound.
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    resolver: Arc<dyn SecretResolver>,
    stream_idle_timeout: Option<Duration>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

impl Client {
    pub fn new() -> Result<Self, LlmError> {
        Self::with_resolver(Arc::new(EnvResolver))
    }

    pub fn with_resolver(resolver: Arc<dyn SecretResolver>) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| {
                LlmError::new(
                    ErrorKind::Network,
                    format!("failed to build the http client: {error}"),
                )
            })?;
        Ok(Self {
            http,
            resolver,
            stream_idle_timeout: clamp_stream_idle_timeout(stream_idle_timeout_from_env()?),
        })
    }

    /// Overrides the idle timeout between response body chunks.
    ///
    /// `None` disables the streaming bound, which is what
    /// `KEEL_STREAM_IDLE_TIMEOUT_SECS=0` does. Values above
    /// [`MAX_STREAM_IDLE_TIMEOUT`] are clamped. Embedders and tests can use this
    /// without touching the process environment.
    pub fn with_stream_idle_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.stream_idle_timeout = clamp_stream_idle_timeout(timeout);
        self
    }

    /// One attempt. Any `Err` here is pre-stream and therefore safe to retry.
    pub async fn stream(&self, route: &Route, request: &Request) -> Result<EventStream, LlmError> {
        let encoded = route.protocol.encode(request)?;
        let auth = route.auth.apply(self.resolver.as_ref())?;

        let mut url = reqwest::Url::parse(&route.endpoint).map_err(|error| {
            LlmError::new(
                ErrorKind::InvalidRequest,
                format!("invalid endpoint {}: {error}", route.endpoint),
            )
        })?;
        if !auth.query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in &auth.query {
                pairs.append_pair(key, value);
            }
        }

        let body = serde_json::to_vec(&encoded.body).map_err(|error| {
            LlmError::new(
                ErrorKind::InvalidRequest,
                format!("failed to serialize the request body: {error}"),
            )
        })?;

        let mut builder = self.http.post(url).body(body);
        for (name, value) in encoded
            .headers
            .iter()
            .chain(route.headers.iter())
            .chain(auth.headers.iter())
        {
            builder = builder.header(name, value);
        }

        let response = builder.send().await.map_err(map_reqwest_error)?;
        let status = response.status();

        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let body_timeout = error_body_timeout(self.stream_idle_timeout);
            let body = match tokio::time::timeout(body_timeout, response.text()).await {
                Ok(Ok(body)) => body,
                Ok(Err(_)) => String::new(),
                Err(_) => {
                    let message = format!(
                        "the provider returned HTTP {} and its error body did not finish within {body_timeout:?}",
                        status.as_u16()
                    );
                    let mut error = LlmError::classify(status.as_u16(), message);
                    if let Some(after) = retry_after {
                        error = error.with_retry_after(after);
                    }
                    return Err(error);
                }
            };
            let message = extract_error_message(&body)
                .unwrap_or_else(|| body.chars().take(400).collect::<String>());
            let mut error = LlmError::classify(status.as_u16(), message);
            if let Some(after) = retry_after {
                error = error.with_retry_after(after);
            }
            return Err(error);
        }

        let mut warnings = encoded.warnings;
        warnings.extend(encoded.cache_warnings.iter().map(ToString::to_string));
        match route.framing {
            Framing::Sse => Ok(EventStream::new(
                response,
                route.protocol.decoder(),
                warnings,
                self.stream_idle_timeout,
            )),
        }
    }

    /// Narrow pre-stream retry. Once a stream is returned, this layer stops.
    pub async fn stream_with_retry(
        &self,
        route: &Route,
        request: &Request,
        policy: &RetryPolicy,
    ) -> Result<EventStream, LlmError> {
        let mut attempt = 1;
        loop {
            match self.stream(route, request).await {
                Ok(stream) => return Ok(stream),
                Err(error) => match policy.decide(&error, attempt) {
                    RetryDecision::Retry { after } => {
                        tokio::time::sleep(after).await;
                        attempt += 1;
                    }
                    RetryDecision::Fail => return Err(error),
                },
            }
        }
    }
}

/// Canonical events decoded from one response body.
///
/// The stream enforces the client's per-chunk idle timeout (300 s by default,
/// see [`DEFAULT_STREAM_IDLE_TIMEOUT`]) and ends once the decoder queues a
/// terminal event or one typed error. It never retries: a successful response
/// stream has already been handed to the caller.
pub struct EventStream {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    sse: SseDecoder,
    decoder: Box<dyn StreamDecoder>,
    queue: VecDeque<Result<LlmEvent, LlmError>>,
    finished: bool,
    saw_terminal: bool,
    idle_timeout: Option<Duration>,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl std::fmt::Debug for EventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventStream")
            .field("finished", &self.finished)
            .field("saw_terminal", &self.saw_terminal)
            .field("idle_timeout", &self.idle_timeout)
            .finish_non_exhaustive()
    }
}

impl EventStream {
    fn new(
        response: reqwest::Response,
        decoder: Box<dyn StreamDecoder>,
        warnings: Vec<String>,
        idle_timeout: Option<Duration>,
    ) -> Self {
        let mut queue = VecDeque::new();
        for warning in warnings {
            queue.push_back(Ok(LlmEvent::Warning(warning)));
        }
        Self {
            inner: Box::pin(response.bytes_stream()),
            sse: SseDecoder::new(),
            decoder,
            queue,
            finished: false,
            saw_terminal: false,
            idle_timeout,
            deadline: idle_timeout.map(|timeout| Box::pin(tokio::time::sleep(timeout))),
        }
    }

    /// Re-arms the idle deadline after a body chunk. No-op when the bound is
    /// disabled.
    fn reset_deadline(&mut self) {
        if let (Some(timeout), Some(deadline)) = (self.idle_timeout, self.deadline.as_mut()) {
            let now = tokio::time::Instant::now();
            let at = now
                .checked_add(timeout)
                .unwrap_or_else(|| now + MAX_STREAM_IDLE_TIMEOUT);
            deadline.as_mut().reset(at);
        }
    }

    fn push_events(&mut self, events: Vec<LlmEvent>) {
        for event in events {
            // A decoder must emit a terminal event last. Ignore anything a
            // provider sends after it instead of keeping the body open.
            if self.finished {
                continue;
            }
            let terminal = matches!(event, LlmEvent::Done { .. } | LlmEvent::Error(_));
            self.queue.push_back(Ok(event));
            if terminal {
                self.saw_terminal = true;
                self.finished = true;
            }
        }
    }

    fn fail(&mut self, error: LlmError) {
        self.queue.push_back(Err(error));
        self.finished = true;
    }

    fn decode_frames(&mut self, frames: Vec<crate::sse::SseFrame>) {
        for frame in frames {
            if self.finished {
                break;
            }
            match self.decoder.frame(&frame.data) {
                Ok(events) => self.push_events(events),
                Err(error) => {
                    self.fail(error);
                    return;
                }
            }
        }
    }
}

impl Stream for EventStream {
    type Item = Result<LlmEvent, LlmError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if let Some(item) = this.queue.pop_front() {
                return Poll::Ready(Some(item));
            }
            if this.finished {
                return Poll::Ready(None);
            }

            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => {
                    let expired = match this.deadline.as_mut() {
                        Some(deadline) => deadline.as_mut().poll(cx).is_ready(),
                        None => false,
                    };
                    if expired {
                        let timeout = this
                            .idle_timeout
                            .expect("a deadline exists only with an idle timeout");
                        this.fail(idle_timeout_error(timeout));
                        continue;
                    }
                    return Poll::Pending;
                }
                Poll::Ready(Some(Ok(bytes))) => {
                    this.reset_deadline();
                    let frames = this.sse.push(&bytes);
                    this.decode_frames(frames);
                }
                Poll::Ready(Some(Err(error))) => {
                    this.fail(map_reqwest_error(error));
                }
                Poll::Ready(None) => {
                    let frames = this.sse.finish();
                    this.decode_frames(frames);
                    if this.finished {
                        continue;
                    }
                    match this.decoder.finish() {
                        Ok(events) => this.push_events(events),
                        Err(error) => this.fail(error),
                    }
                    if !this.saw_terminal {
                        this.queue.push_back(Err(LlmError::decode(
                            "stream ended without a terminal event",
                        )));
                    }
                    this.finished = true;
                }
            }
        }
    }
}

fn idle_timeout_error(timeout: Duration) -> LlmError {
    LlmError::new(
        ErrorKind::Timeout,
        format!("no data from the provider for {timeout:?}"),
    )
}

fn clamp_stream_idle_timeout(timeout: Option<Duration>) -> Option<Duration> {
    timeout.map(|timeout| timeout.min(MAX_STREAM_IDLE_TIMEOUT))
}

fn error_body_timeout(stream_idle_timeout: Option<Duration>) -> Duration {
    stream_idle_timeout.map_or(ERROR_BODY_TIMEOUT, |timeout| {
        timeout.min(ERROR_BODY_TIMEOUT)
    })
}

fn stream_idle_timeout_from_env() -> Result<Option<Duration>, LlmError> {
    match std::env::var(STREAM_IDLE_TIMEOUT_ENV) {
        Ok(value) => parse_stream_idle_timeout(&value)
            .map_err(|message| LlmError::new(ErrorKind::InvalidRequest, message)),
        Err(std::env::VarError::NotPresent) => Ok(Some(DEFAULT_STREAM_IDLE_TIMEOUT)),
        Err(std::env::VarError::NotUnicode(_)) => Err(LlmError::new(
            ErrorKind::InvalidRequest,
            format!("{STREAM_IDLE_TIMEOUT_ENV} is not valid UTF-8"),
        )),
    }
}

fn parse_stream_idle_timeout(value: &str) -> Result<Option<Duration>, String> {
    let seconds = value.trim().parse::<u32>().map_err(|_| {
        format!(
            "{STREAM_IDLE_TIMEOUT_ENV} must be a whole number of seconds (0 disables), got `{value}`"
        )
    })?;
    Ok((seconds > 0).then(|| Duration::from_secs(u64::from(seconds))))
}

fn map_reqwest_error(error: reqwest::Error) -> LlmError {
    let kind = if error.is_timeout() {
        ErrorKind::Timeout
    } else {
        ErrorKind::Network
    };
    let timed_out = error.is_timeout();
    let failed_to_connect = error.is_connect();
    // `reqwest::Error`'s Display includes the full URL, query string and all.
    // Query auth puts a secret there, so keep it out of the message.
    let url = error.url().map(|url| {
        let mut url = url.clone();
        url.set_query(None);
        url.set_fragment(None);
        url.to_string()
    });
    let error = error.without_url();
    let detail = error_detail(error.to_string(), &error);
    let message = match url {
        Some(url) if timed_out => format!("request to {url} timed out: {detail}"),
        Some(url) if failed_to_connect => format!("request to {url} failed to connect: {detail}"),
        Some(url) => format!("request to {url} failed: {detail}"),
        None => format!("the request failed before it reached the server: {detail}"),
    };
    LlmError::new(kind, message)
}

/// Append the bounded `std::error::Error::source` chain, skipping text that is
/// already present in the top-level message. The URL was removed first, so no
/// query credential can enter through the chain.
fn error_detail(mut detail: String, error: &(dyn std::error::Error + 'static)) -> String {
    let mut source = error.source();
    for _ in 0..3 {
        let Some(cause) = source else { break };
        let text = cause.to_string();
        if !text.is_empty() && !detail.contains(&text) {
            detail.push_str(": ");
            detail.push_str(&text);
        }
        source = cause.source();
    }
    detail
}

fn extract_error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .pointer("/error/message")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| {
            value
                .get("detail")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_stream_idle_timeout_is_generous_but_bounded() {
        assert_eq!(DEFAULT_STREAM_IDLE_TIMEOUT, Duration::from_secs(300));
    }

    #[test]
    fn clamps_an_absurd_stream_idle_timeout() {
        assert_eq!(
            clamp_stream_idle_timeout(Some(Duration::MAX)),
            Some(MAX_STREAM_IDLE_TIMEOUT)
        );
        assert_eq!(clamp_stream_idle_timeout(None), None);
        assert_eq!(
            clamp_stream_idle_timeout(Some(Duration::from_millis(100))),
            Some(Duration::from_millis(100))
        );
    }

    #[test]
    fn bounds_a_non_success_error_body() {
        assert_eq!(error_body_timeout(None), ERROR_BODY_TIMEOUT);
        assert_eq!(
            error_body_timeout(Some(Duration::from_millis(100))),
            Duration::from_millis(100)
        );
        assert_eq!(
            error_body_timeout(Some(DEFAULT_STREAM_IDLE_TIMEOUT)),
            ERROR_BODY_TIMEOUT
        );
    }

    #[test]
    fn parses_the_stream_idle_timeout_setting() {
        assert_eq!(
            parse_stream_idle_timeout("300").unwrap(),
            Some(Duration::from_secs(300))
        );
        assert_eq!(
            parse_stream_idle_timeout(" 5 ").unwrap(),
            Some(Duration::from_secs(5))
        );
        assert_eq!(parse_stream_idle_timeout("0").unwrap(), None);
    }

    #[test]
    fn rejects_a_malformed_stream_idle_timeout_setting() {
        for value in ["", "soon", "-1", "1.5", "4294967296"] {
            let error = parse_stream_idle_timeout(value).unwrap_err();
            assert!(
                error.contains(STREAM_IDLE_TIMEOUT_ENV),
                "the error names the setting: {error}"
            );
            assert!(error.contains("whole number"), "{error}");
        }
    }

    #[tokio::test]
    async fn transport_errors_redact_a_query_secret() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
        let port = listener.local_addr().expect("address").port();
        drop(listener);

        let error = reqwest::Client::new()
            .get(format!(
                "http://127.0.0.1:{port}/v1/models?key=sk-test-query-secret"
            ))
            .send()
            .await
            .expect_err("nothing listens on the closed port");
        let mapped = map_reqwest_error(error);
        let lower = mapped.message.to_lowercase();
        assert!(
            lower.contains("connect") || lower.contains("refused"),
            "the transport cause is appended: {}",
            mapped.message
        );

        assert_eq!(mapped.kind, ErrorKind::Network);
        assert!(
            !mapped.message.contains("sk-test-query-secret"),
            "{}",
            mapped.message
        );
        assert!(!mapped.message.contains("key="), "{}", mapped.message);
        assert!(
            mapped.message.contains(&format!("127.0.0.1:{port}")),
            "{}",
            mapped.message
        );
    }
}
