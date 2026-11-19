//! HTTP transport.
//!
//! The client owns the request lifecycle up to the first body byte. After that
//! it hands back an `EventStream` and never retries: replay safety belongs to
//! the session loop.

use std::collections::VecDeque;
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

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    resolver: Arc<dyn SecretResolver>,
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
        Ok(Self { http, resolver })
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
            let body = response.text().await.unwrap_or_default();
            let message = extract_error_message(&body)
                .unwrap_or_else(|| body.chars().take(400).collect::<String>());
            let mut error = LlmError::classify(status.as_u16(), message);
            if let Some(after) = retry_after {
                error = error.with_retry_after(after);
            }
            return Err(error);
        }

        let warnings = encoded.warnings;
        match route.framing {
            Framing::Sse => Ok(EventStream::new(
                response,
                route.protocol.decoder(),
                warnings,
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

pub struct EventStream {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    sse: SseDecoder,
    decoder: Box<dyn StreamDecoder>,
    queue: VecDeque<Result<LlmEvent, LlmError>>,
    finished: bool,
    saw_terminal: bool,
}

impl std::fmt::Debug for EventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventStream")
            .field("finished", &self.finished)
            .field("saw_terminal", &self.saw_terminal)
            .finish_non_exhaustive()
    }
}

impl EventStream {
    fn new(
        response: reqwest::Response,
        decoder: Box<dyn StreamDecoder>,
        warnings: Vec<String>,
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
        }
    }

    fn push_events(&mut self, events: Vec<LlmEvent>) {
        for event in events {
            if matches!(event, LlmEvent::Done { .. } | LlmEvent::Error(_)) {
                self.saw_terminal = true;
            }
            self.queue.push_back(Ok(event));
        }
    }

    fn fail(&mut self, error: LlmError) {
        self.queue.push_back(Err(error));
        self.finished = true;
    }

    fn decode_frames(&mut self, frames: Vec<crate::sse::SseFrame>) {
        for frame in frames {
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
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(bytes))) => {
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

fn map_reqwest_error(error: reqwest::Error) -> LlmError {
    let kind = if error.is_timeout() {
        ErrorKind::Timeout
    } else {
        ErrorKind::Network
    };
    // `reqwest::Error`'s Display includes the full URL, query string and all.
    // Query auth puts a secret there, so keep it out of the message.
    let message = match error.url() {
        Some(url) => {
            let mut url = url.clone();
            url.set_query(None);
            url.set_fragment(None);
            if error.is_timeout() {
                format!("request to {url} timed out")
            } else if error.is_connect() {
                format!("request to {url} failed to connect")
            } else {
                format!("request to {url} failed")
            }
        }
        None => "the request failed before it reached the server".to_owned(),
    };
    LlmError::new(kind, message)
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
