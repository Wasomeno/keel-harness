//! Cassettes: recorded request/response pairs that replay offline.
//!
//! A cassette is the whole exchange, not just the response body: the request
//! keel sent, the status, the headers and the raw body. The player answers a
//! test's HTTP request by exact match, so replayed traffic exercises the real
//! client, route and decoder, and a request that changed is a hard failure
//! instead of a silently passing decode test.
//!
//! # The cursor rule
//!
//! [`CassetteServer`] consumes interactions in cassette order, once each. The
//! first request must match `interactions[0]`, the second `interactions[1]`,
//! and so on. A request that does not match the next interaction, or that
//! arrives after the last one, is a miss: the server answers HTTP 422 (a
//! non-retryable `InvalidRequest`), so a test cannot turn a cassette miss into
//! a retry backoff. At the end of a replay, [`CassetteServer::assert_all_consumed`]
//! fails if any recorded interaction was left over.
//!
//! A single-exchange cassette therefore serves exactly one request. A test
//! that needs the same exchange twice lists it twice; order is the transcript,
//! not a set of independent fixtures.
//!
//! Cassettes are recorded through a local [`Recorder`] proxy, which refuses to
//! record against a non-loopback origin unless `KEEL_CASSETTE_RECORD=1`, and
//! refuses even then when `CI` is set. CI replays.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::faux::{RecordedRequest, Responder, Server, Turn};

/// The cassette format version this build reads and writes.
pub const CASSETTE_VERSION: u32 = 1;

/// Whether the process is allowed to record or must replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CassetteMode {
    Replay,
    Record,
}

#[derive(Debug, thiserror::Error)]
pub enum CassetteError {
    #[error("cassette: {0}")]
    Io(#[from] std::io::Error),
    #[error("cassette is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cassette version {found} is not supported (this build writes {CASSETTE_VERSION})")]
    UnsupportedVersion { found: u32 },
    #[error(
        "recording against a real endpoint needs KEEL_CASSETTE_RECORD=1; \
         replay from a committed cassette instead"
    )]
    RecordingDisabled,
    #[error("CI must replay cassettes, not record them; unset KEEL_CASSETTE_RECORD in CI")]
    CiMustReplay,
    #[error("recorder upstream {0:?} is not an absolute http(s) URL")]
    BadUpstream(String),
}

/// One recorded HTTP exchange.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Interaction {
    pub request: CassetteRequest,
    pub response: CassetteResponse,
}

impl Interaction {
    pub fn new(request: CassetteRequest, response: CassetteResponse) -> Self {
        Self { request, response }
    }

    /// Whether `request` is the request this interaction recorded. JSON bodies
    /// are compared as values, so formatting and key order do not matter but
    /// every field does.
    pub fn matches(&self, request: &RecordedRequest) -> Result<(), String> {
        if !self.request.method.eq_ignore_ascii_case(&request.method) {
            return Err(format!(
                "method {} != {}",
                self.request.method, request.method
            ));
        }
        if self.request.path != request.path {
            return Err(format!("path {} != {}", self.request.path, request.path));
        }
        body_difference(&self.request.body, request.body_text())
    }
}

/// The request half of an interaction, with credentials redacted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CassetteRequest {
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl CassetteRequest {
    pub fn new(
        method: impl Into<String>,
        path: impl Into<String>,
        headers: Vec<(String, String)>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            headers,
            body: body.into(),
        }
    }

    /// The request as the client sent it. Bodies that are not UTF-8 are
    /// replaced, which only affects protocols keel does not speak today.
    pub fn from_recorded(request: &RecordedRequest) -> Self {
        Self {
            method: request.method.clone(),
            path: request.path.clone(),
            headers: request.headers.clone(),
            body: String::from_utf8_lossy(&request.body).into_owned(),
        }
    }
}

/// The response half of an interaction, raw and undecoded.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CassetteResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl CassetteResponse {
    pub fn new(status: u16, headers: Vec<(String, String)>, body: impl Into<String>) -> Self {
        Self {
            status,
            headers,
            body: body.into(),
        }
    }

    /// A streaming response, which is what every chat protocol sends.
    pub fn sse(body: impl Into<String>) -> Self {
        Self::new(
            200,
            vec![("content-type".to_owned(), "text/event-stream".to_owned())],
            body,
        )
    }

    fn to_turn(&self) -> Turn {
        Turn::raw(
            self.status,
            self.headers.clone(),
            self.body.clone().into_bytes(),
        )
    }
}

/// A recorded session: named interactions plus the provenance a reader needs
/// to judge them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cassette {
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub model: String,
    /// Where the bytes came from: a live capture, or a recording borrowed
    /// from another project, with its license.
    #[serde(default)]
    pub source: String,
    /// Header and query names whose values were replaced by `REDACTED`.
    #[serde(default)]
    pub redactions: Vec<String>,
    pub interactions: Vec<Interaction>,
}

impl Cassette {
    pub fn builder(name: impl Into<String>) -> CassetteBuilder {
        CassetteBuilder {
            cassette: Cassette {
                version: CASSETTE_VERSION,
                name: name.into(),
                provider: String::new(),
                protocol: String::new(),
                model: String::new(),
                source: String::new(),
                redactions: Vec::new(),
                interactions: Vec::new(),
            },
        }
    }

    pub fn parse(json: &str) -> Result<Self, CassetteError> {
        let cassette: Cassette = serde_json::from_str(json)?;
        cassette.validate()?;
        Ok(cassette)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, CassetteError> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

    pub fn to_json(&self) -> Result<String, CassetteError> {
        Ok(serde_json::to_string_pretty(self)? + "\n")
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), CassetteError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_json()?)?;
        Ok(())
    }

    fn validate(&self) -> Result<(), CassetteError> {
        if self.version != CASSETTE_VERSION {
            return Err(CassetteError::UnsupportedVersion {
                found: self.version,
            });
        }
        Ok(())
    }

    /// The first interaction that recorded `request`, if the cassette has one.
    ///
    /// This is a lookup helper for tests; [`CassetteServer`] does not use it.
    /// The player advances an ordered cursor, because a cassette is a session
    /// transcript rather than a set of independent fixtures.
    pub fn matching(&self, request: &RecordedRequest) -> Option<&Interaction> {
        self.interactions
            .iter()
            .find(|interaction| interaction.matches(request).is_ok())
    }

    /// Starts a server that answers from this cassette.
    pub async fn start(&self) -> CassetteServer {
        let state = Arc::new(CassetteState {
            cassette: self.clone(),
            cursor: Mutex::new(0),
            hits: Mutex::new(Vec::new()),
            misses: Mutex::new(Vec::new()),
        });
        CassetteServer {
            server: Server::start(state.clone()).await,
            state,
        }
    }
}

pub struct CassetteBuilder {
    cassette: Cassette,
}

impl CassetteBuilder {
    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.cassette.provider = provider.into();
        self
    }

    pub fn protocol(mut self, protocol: impl Into<String>) -> Self {
        self.cassette.protocol = protocol.into();
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.cassette.model = model.into();
        self
    }

    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.cassette.source = source.into();
        self
    }

    pub fn redactions(mut self, redactions: impl IntoIterator<Item = String>) -> Self {
        self.cassette.redactions = redactions.into_iter().collect();
        self
    }

    pub fn interaction(mut self, interaction: Interaction) -> Self {
        self.cassette.interactions.push(interaction);
        self
    }

    pub fn interactions(mut self, interactions: Vec<Interaction>) -> Self {
        self.cassette.interactions = interactions;
        self
    }

    pub fn build(self) -> Cassette {
        self.cassette
    }
}

/// A request that did not match the next recorded interaction, with what the
/// cassette expected for the failure message.
#[derive(Clone, Debug)]
pub struct Miss {
    pub request: RecordedRequest,
    /// The expected interaction and why it did not match, or why the cassette
    /// had nothing left. Kept as the old field name so existing assertions
    /// keep reading naturally.
    pub closest: Option<String>,
}

struct CassetteState {
    cassette: Cassette,
    /// Index of the next interaction to serve. Advanced only on a match, so a
    /// miss reports the same expected exchange until the test fixes the
    /// request instead of silently skipping it.
    cursor: Mutex<usize>,
    hits: Mutex<Vec<usize>>,
    misses: Mutex<Vec<Miss>>,
}

impl Responder for CassetteState {
    fn respond(&self, request: &RecordedRequest) -> Turn {
        let next = {
            let mut cursor = self
                .cursor
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let index = *cursor;
            match self.cassette.interactions.get(index) {
                Some(next) => match next.matches(request) {
                    Ok(()) => {
                        *cursor += 1;
                        Ok((index, next))
                    }
                    Err(reason) => Err(format!(
                        "interaction {index} ({}): {reason}",
                        next.request.path
                    )),
                },
                None => Err(format!(
                    "no interactions left ({} already consumed)",
                    self.cassette.interactions.len()
                )),
            }
        };

        match next {
            Ok((index, interaction)) => {
                self.hits
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(index);
                interaction.response.to_turn()
            }
            Err(expected) => self.miss(request, expected),
        }
    }
}

impl CassetteState {
    fn miss(&self, request: &RecordedRequest, expected: String) -> Turn {
        self.misses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Miss {
                request: request.clone(),
                closest: Some(expected.clone()),
            });

        let message = format!("cassette miss: expected {expected}");
        let body = serde_json::json!({
            "error": {
                "message": message,
                "cassette": self.cassette.name,
                "request": {
                    "method": request.method,
                    "path": request.path,
                },
                "expected": expected,
            }
        });
        // 422 is a non-retryable InvalidRequest in keel's taxonomy. A 5xx here
        // would turn a stale cassette into retry backoff, which hides the
        // mismatch; the cursor rule makes a miss fail immediately.
        Turn::raw(
            422,
            vec![("content-type".to_owned(), "application/json".to_owned())],
            body.to_string().into_bytes(),
        )
    }
}

/// A running server that answers requests from a cassette in recorded order.
///
/// See the module documentation for the cursor rule. A replay is complete only
/// when [`assert_all_consumed`](Self::assert_all_consumed) passes and there are
/// no misses.
pub struct CassetteServer {
    server: Server,
    state: Arc<CassetteState>,
}

impl CassetteServer {
    /// Base URL, without a trailing slash and without a protocol path.
    pub fn url(&self) -> &str {
        self.server.url()
    }

    pub async fn requests(&self) -> Vec<RecordedRequest> {
        self.server.requests().await
    }

    pub async fn wait_for_requests(&self, count: usize) {
        self.server.wait_for_requests(count).await;
    }

    /// Indices into [`Cassette::interactions`], in the order they were served.
    pub fn hits(&self) -> Vec<usize> {
        self.state
            .hits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn misses(&self) -> Vec<Miss> {
        self.state
            .misses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Recorded interactions not yet served.
    pub fn remaining(&self) -> usize {
        let consumed = *self
            .state
            .cursor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.state
            .cassette
            .interactions
            .len()
            .saturating_sub(consumed)
    }

    /// Panics with the request and the expected interaction when any request
    /// did not match the cassette cursor.
    pub fn assert_no_misses(&self) {
        let misses = self.misses();
        if misses.is_empty() {
            return;
        }
        let mut report = format!(
            "{} request(s) did not match cassette {:?}",
            misses.len(),
            self.state.cassette.name
        );
        for miss in misses {
            report.push_str(&format!(
                "\n  {} {}: {}",
                miss.request.method,
                miss.request.path,
                miss.closest.unwrap_or_else(|| "no interactions".to_owned())
            ));
        }
        panic!("{report}");
    }

    /// Panics when the replay did not consume every recorded interaction, which
    /// is what a session that stopped early looks like.
    pub fn assert_all_consumed(&self) {
        let remaining = self.remaining();
        if remaining == 0 {
            return;
        }
        panic!(
            "cassette {:?} has {remaining} unconsumed interaction(s) of {}",
            self.state.cassette.name,
            self.state.cassette.interactions.len()
        );
    }
}

fn body_difference(expected: &str, actual: &str) -> Result<(), String> {
    match (
        serde_json::from_str::<Value>(expected),
        serde_json::from_str::<Value>(actual),
    ) {
        (Ok(expected), Ok(actual)) => match json_difference(&expected, &actual) {
            None => Ok(()),
            Some(difference) => Err(format!("body differs at {difference}")),
        },
        _ if expected == actual => Ok(()),
        _ => Err(format!(
            "body differs ({} vs {} bytes)",
            expected.len(),
            actual.len()
        )),
    }
}

/// First structural difference between two JSON values, as a path relative to
/// the compared root. Short on purpose: it has to fit in a test failure line.
pub fn json_difference(expected: &Value, actual: &Value) -> Option<String> {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            for (key, value) in expected {
                match actual.get(key) {
                    Some(other) => {
                        if let Some(difference) = json_difference(value, other) {
                            return Some(format!(".{key}{difference}"));
                        }
                    }
                    None => return Some(format!(".{key}: missing, expected {}", summary(value))),
                }
            }
            for (key, value) in actual {
                if !expected.contains_key(key) {
                    return Some(format!(".{key}: unexpected {}", summary(value)));
                }
            }
            None
        }
        (Value::Array(expected), Value::Array(actual)) => {
            if expected.len() != actual.len() {
                return Some(format!(
                    ": {} items != {} items",
                    expected.len(),
                    actual.len()
                ));
            }
            for (index, (expected, actual)) in expected.iter().zip(actual).enumerate() {
                if let Some(difference) = json_difference(expected, actual) {
                    return Some(format!("[{index}]{difference}"));
                }
            }
            None
        }
        (expected, actual) if expected == actual => None,
        (expected, actual) => Some(format!(": {} != {}", summary(expected), summary(actual))),
    }
}

fn summary(value: &Value) -> String {
    let rendered = value.to_string();
    let mut truncated: String = rendered.chars().take(60).collect();
    if truncated.len() < rendered.len() {
        truncated.push('…');
    }
    truncated
}

pub(crate) fn redactions(names: impl IntoIterator<Item = String>) -> Vec<String> {
    names
        .into_iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
