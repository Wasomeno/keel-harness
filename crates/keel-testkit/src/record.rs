//! The recorder: a local proxy that captures live traffic into a cassette.
//!
//! It exists so a cassette can be produced mechanically from a real call once
//! someone has a key. It never runs in CI: recording against a non-loopback
//! origin needs `KEEL_CASSETTE_RECORD=1`, and `CI` overrides it.
//!
//! Requests are read by `content-length`, which keel's client always sends for
//! JSON bodies; a chunked upload would not be recorded correctly.
//!
//! Query credentials are redacted to `REDACTED`, and the player matches the
//! path exactly, so a scenario that authenticates in the query string replays
//! with the same placeholder. Header credentials do not affect matching.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::cassette::{
    CASSETTE_VERSION, Cassette, CassetteError, CassetteMode, CassetteRequest, CassetteResponse,
    Interaction, redactions,
};
use crate::faux::{RecordedRequest, Turn, read_request, write_head, write_turn};

const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "cookie",
    "set-cookie",
    "x-goog-api-key",
];

const SENSITIVE_QUERY: &[&str] = &["key", "api_key", "apikey", "access_token", "token"];

/// Whether this process may record. Reads `KEEL_CASSETTE_RECORD` and `CI`.
pub fn cassette_mode() -> Result<CassetteMode, CassetteError> {
    resolve_mode(record_requested(), in_ci())
}

/// The recording rule, with its inputs passed in so it can be tested.
///
/// A loopback upstream is always allowed: that is a fixture server or a
/// round-trip test, not provider traffic.
pub fn recording_mode(
    record_requested: bool,
    in_ci: bool,
    upstream_is_loopback: bool,
) -> Result<CassetteMode, CassetteError> {
    if upstream_is_loopback {
        return Ok(CassetteMode::Record);
    }
    resolve_mode(record_requested, in_ci)
}

fn resolve_mode(record_requested: bool, in_ci: bool) -> Result<CassetteMode, CassetteError> {
    match (record_requested, in_ci) {
        (true, true) => Err(CassetteError::CiMustReplay),
        (true, false) => Ok(CassetteMode::Record),
        (false, _) => Ok(CassetteMode::Replay),
    }
}

fn record_requested() -> bool {
    matches!(
        std::env::var("KEEL_CASSETTE_RECORD").as_deref(),
        Ok("1" | "true")
    )
}

fn in_ci() -> bool {
    std::env::var_os("CI").is_some()
}

/// Whether a URL points at this machine.
pub fn is_loopback(url: &str) -> bool {
    matches!(
        host_of(url).as_deref(),
        Some("127.0.0.1" | "localhost" | "::1")
    )
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    let authority = authority.rsplit('@').next()?;
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next()?,
        None => authority.split(':').next()?,
    };
    Some(host.to_ascii_lowercase())
}

/// A running recording proxy.
pub struct Recorder {
    url: String,
    state: Arc<RecorderState>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

struct RecorderState {
    name: String,
    upstream: String,
    client: reqwest::Client,
    interactions: Mutex<Vec<Interaction>>,
    redactions: Mutex<BTreeSet<String>>,
    provider: Mutex<String>,
    protocol: Mutex<String>,
    model: Mutex<String>,
    source: Mutex<String>,
}

impl Recorder {
    /// Binds a local port and forwards everything it receives to `upstream`.
    ///
    /// Fails with [`CassetteError::RecordingDisabled`] unless the upstream is
    /// loopback or `KEEL_CASSETTE_RECORD=1` is set, and with
    /// [`CassetteError::CiMustReplay`] when both that variable and `CI` are
    /// set.
    pub async fn start(
        name: impl Into<String>,
        upstream: impl Into<String>,
    ) -> Result<Self, CassetteError> {
        let upstream = upstream.into().trim_end_matches('/').to_owned();
        if !(upstream.starts_with("http://") || upstream.starts_with("https://"))
            || host_of(&upstream).is_none()
        {
            return Err(CassetteError::BadUpstream(upstream));
        }
        match recording_mode(record_requested(), in_ci(), is_loopback(&upstream))? {
            CassetteMode::Record => {}
            CassetteMode::Replay => return Err(CassetteError::RecordingDisabled),
        }

        let client = reqwest::Client::builder()
            .build()
            .map_err(|error| CassetteError::BadUpstream(format!("{upstream}: {error}")))?;
        let state = Arc::new(RecorderState {
            name: name.into(),
            upstream,
            client,
            interactions: Mutex::new(Vec::new()),
            redactions: Mutex::new(BTreeSet::new()),
            provider: Mutex::new(String::new()),
            protocol: Mutex::new(String::new()),
            model: Mutex::new(String::new()),
            source: Mutex::new("recorded through keel-testkit's recorder".to_owned()),
        });

        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(accept_loop(listener, Arc::clone(&state), receiver));

        Ok(Self {
            url: format!("http://{address}"),
            state,
            shutdown,
            task,
        })
    }

    /// Point keel's route at this URL instead of the provider.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn provider(&self, provider: impl Into<String>) {
        *self.lock(&self.state.provider) = provider.into();
    }

    pub fn protocol(&self, protocol: impl Into<String>) {
        *self.lock(&self.state.protocol) = protocol.into();
    }

    pub fn model(&self, model: impl Into<String>) {
        *self.lock(&self.state.model) = model.into();
    }

    pub fn source(&self, source: impl Into<String>) {
        *self.lock(&self.state.source) = source.into();
    }

    pub fn interaction_count(&self) -> usize {
        self.lock(&self.state.interactions).len()
    }

    /// The exchange recorded so far. Safe to call while recording.
    pub fn cassette(&self) -> Cassette {
        Cassette {
            version: CASSETTE_VERSION,
            name: self.state.name.clone(),
            provider: self.lock(&self.state.provider).clone(),
            protocol: self.lock(&self.state.protocol).clone(),
            model: self.lock(&self.state.model).clone(),
            source: self.lock(&self.state.source).clone(),
            redactions: redactions(self.lock(&self.state.redactions).iter().cloned()),
            interactions: self.lock(&self.state.interactions).clone(),
        }
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), CassetteError> {
        self.cassette().save(path)
    }

    /// A poisoned lock means another request panicked; the data is still
    /// readable, which is what a test wants to see.
    fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.task.abort();
    }
}

async fn accept_loop(
    listener: TcpListener,
    state: Arc<RecorderState>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => {
                let Ok((socket, _)) = accepted else { break };
                let state = Arc::clone(&state);
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    serve(socket, state, shutdown).await;
                });
            }
        }
    }
}

async fn serve(
    mut socket: TcpStream,
    state: Arc<RecorderState>,
    mut shutdown: watch::Receiver<bool>,
) {
    let Ok(request) = read_request(&mut socket).await else {
        return;
    };
    proxy(&mut socket, &state, &request, &mut shutdown).await;
    let _ = socket.shutdown().await;
}

/// Forwards one exchange and captures it. The body is streamed to the client as
/// it arrives, so a recorded call behaves like a direct one; the cassette gets
/// the whole body once the upstream ends it.
async fn proxy(
    socket: &mut TcpStream,
    state: &RecorderState,
    request: &RecordedRequest,
    shutdown: &mut watch::Receiver<bool>,
) {
    let upstream = match send_upstream(state, request).await {
        Ok(response) => response,
        Err(error) => {
            let _ = write_turn(socket, Turn::text(502, error), shutdown).await;
            return;
        }
    };

    let status = upstream.status().as_u16();
    let (recorded_headers, response_redactions) = response_headers(upstream.headers());
    let mut headers = recorded_headers.clone();
    headers.push(("connection".to_owned(), "close".to_owned()));
    if write_head(socket, status, &headers).await.is_err() {
        return;
    }

    let mut body = Vec::new();
    let mut chunks = upstream.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let Ok(bytes) = chunk else {
            // The upstream ended badly. The client sees the truncation a direct
            // call would show, and the cassette records what did arrive.
            break;
        };
        body.extend_from_slice(&bytes);
        if socket.write_all(&bytes).await.is_err() || socket.flush().await.is_err() {
            // The client left early; the exchange was not fully observed, so it
            // is not written to the cassette.
            return;
        }
    }
    let _ = socket.flush().await;

    record(
        state,
        request,
        CassetteResponse::new(
            status,
            recorded_headers,
            String::from_utf8_lossy(&body).into_owned(),
        ),
        response_redactions,
    );
}

async fn send_upstream(
    state: &RecorderState,
    request: &RecordedRequest,
) -> Result<reqwest::Response, String> {
    let url = format!("{}{}", state.upstream, request.path);
    let method =
        reqwest::Method::from_bytes(request.method.as_bytes()).unwrap_or(reqwest::Method::POST);

    let mut outgoing = state.client.request(method, &url);
    for (name, value) in &request.headers {
        if matches!(
            name.as_str(),
            "host" | "connection" | "content-length" | "transfer-encoding"
        ) {
            continue;
        }
        outgoing = outgoing.header(name.as_str(), value.as_str());
    }

    outgoing
        .body(request.body.clone())
        .send()
        .await
        .map_err(|error| format!("recorder: upstream request failed: {error}"))
}

fn record(
    state: &RecorderState,
    request: &RecordedRequest,
    mut response: CassetteResponse,
    response_redactions: BTreeSet<String>,
) {
    let (redacted, mut applied) = redact_request(request);
    let (body, body_names) = redact_body(&response.body);
    response.body = body;
    applied.extend(body_names);
    applied.extend(response_redactions);
    state
        .redactions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .extend(applied);
    state
        .interactions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(Interaction::new(redacted, response));
}

/// Replaces credential values, keeping the names so the file still says which
/// headers the request carried. Returns the request and the names redacted.
fn redact_request(request: &RecordedRequest) -> (CassetteRequest, BTreeSet<String>) {
    let mut applied = BTreeSet::new();

    let headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(name, value)| {
            if SENSITIVE_HEADERS.contains(&name.as_str()) {
                applied.insert(name.clone());
                (name.clone(), "REDACTED".to_owned())
            } else {
                (name.clone(), value.clone())
            }
        })
        .collect();

    let path = match request.path.split_once('?') {
        Some((path, query)) => {
            let redacted: Vec<String> = query
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((key, _))
                        if SENSITIVE_QUERY.contains(&key.to_ascii_lowercase().as_str()) =>
                    {
                        applied.insert(key.to_ascii_lowercase());
                        format!("{key}=REDACTED")
                    }
                    _ => pair.to_owned(),
                })
                .collect();
            format!("{path}?{}", redacted.join("&"))
        }
        None => request.path.clone(),
    };

    let (body, body_names) = redact_body(request.body_text());
    applied.extend(body_names);

    (
        CassetteRequest::new(request.method.clone(), path, headers, body),
        applied,
    )
}

/// Replaces credential-shaped substrings inside a body, so a cassette records
/// that a request or response carried a secret without carrying the secret.
///
/// Header and query redaction cannot see these: a key can sit in a JSON field,
/// a system prompt, or an echoed error body. The shapes are the ones the design
/// notes list — bearer tokens, `sk-` keys, Google `AIza` keys and PEM private
/// key blocks. Over-redaction is the accepted trade: a false positive costs one
/// confusing line in a cassette, a false negative costs a leaked key.
pub fn redact_body(body: &str) -> (String, BTreeSet<String>) {
    let mut applied = BTreeSet::new();
    let mut text = redact_private_keys(body, &mut applied);
    text = redact_token(
        &text,
        "Bearer ",
        8,
        "Bearer REDACTED",
        "bearer",
        &mut applied,
    );
    text = redact_token(&text, "sk-", 16, "sk-REDACTED", "sk-", &mut applied);
    // A Google API key is `AIza` plus 35 characters, but limits move; 30 is
    // already far past anything that occurs by accident.
    text = redact_token(&text, "AIza", 30, "AIzaREDACTED", "AIza", &mut applied);
    (text, applied)
}

fn redact_private_keys(body: &str, applied: &mut BTreeSet<String>) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "-----END ";

    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find(BEGIN) {
        let Some(end_offset) = rest[start..].find(END) else {
            break;
        };
        let after_end = start + end_offset + END.len();
        let Some(dashes_offset) = rest[after_end..].find("-----") else {
            break;
        };
        let mut end = after_end + dashes_offset + 5;
        if rest[end..].starts_with('\n') {
            end += 1;
        }
        out.push_str(&rest[..start]);
        out.push_str("REDACTED-PRIVATE-KEY");
        applied.insert("private-key".to_owned());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Redacts `<prefix><token>` when the token is at least `min_len` characters of
/// `[A-Za-z0-9_-]`. The prefix is kept, so the cassette still shows the shape.
fn redact_token(
    body: &str,
    prefix: &str,
    min_len: usize,
    replacement: &str,
    name: &str,
    applied: &mut BTreeSet<String>,
) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find(prefix) {
        let token_start = start + prefix.len();
        let token_len = rest[token_start..]
            .chars()
            .take_while(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
            .map(char::len_utf8)
            .sum::<usize>();
        if token_len >= min_len {
            out.push_str(&rest[..start]);
            out.push_str(replacement);
            applied.insert(name.to_owned());
            rest = &rest[token_start + token_len..];
        } else {
            out.push_str(&rest[..token_start]);
            rest = &rest[token_start..];
        }
    }
    out.push_str(rest);
    out
}

fn response_headers(
    headers: &reqwest::header::HeaderMap,
) -> (Vec<(String, String)>, BTreeSet<String>) {
    let mut applied = BTreeSet::new();
    let headers = headers
        .iter()
        .filter(|(name, _)| {
            !matches!(
                name.as_str(),
                "connection" | "transfer-encoding" | "content-length"
            )
        })
        .map(|(name, value)| {
            let value = value.to_str().unwrap_or_default().to_owned();
            let name = name.as_str().to_owned();
            if SENSITIVE_HEADERS.contains(&name.as_str()) {
                applied.insert(name.clone());
                (name, "REDACTED".to_owned())
            } else {
                (name, value)
            }
        })
        .collect();
    (headers, applied)
}
