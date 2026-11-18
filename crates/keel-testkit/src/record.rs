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
    response: CassetteResponse,
    response_redactions: BTreeSet<String>,
) {
    let (redacted, mut applied) = redact_request(request);
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

    (
        CassetteRequest::new(request.method.clone(), path, headers, request.body_text()),
        applied,
    )
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
