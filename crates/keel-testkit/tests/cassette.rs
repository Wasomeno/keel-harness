//! The cassette player and recorder against a real socket.

use std::time::Duration;

use keel_testkit::cassette::json_difference;
use keel_testkit::{
    Cassette, CassetteError, CassetteMode, CassetteRequest, CassetteResponse, Chunk, FauxProvider,
    Interaction, Recorder, Turn, cassette_mode, is_loopback, recording_mode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A minimal HTTP/1.1 client: the testkit is the seam, so it does not use the
/// client it is testing.
async fn post(url: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
    let address = url.trim_start_matches("http://").to_owned();
    let mut socket = TcpStream::connect(&address).await.expect("connect");
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);

    socket.write_all(request.as_bytes()).await.expect("write");
    socket.flush().await.expect("flush");

    let mut response = Vec::new();
    socket.read_to_end(&mut response).await.expect("read");
    let response = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("a complete HTTP response");
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status code");
    (status, body.to_owned())
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn chat_cassette() -> Cassette {
    Cassette::builder("test/chat")
        .provider("test")
        .protocol("openai-chat")
        .model("gpt-4.1")
        .source("designed by this test")
        .interaction(Interaction::new(
            CassetteRequest::new(
                "POST",
                "/v1/chat/completions",
                vec![("content-type".to_owned(), "application/json".to_owned())],
                r#"{"model":"gpt-4.1","messages":[{"role":"user","content":"hi"}]}"#,
            ),
            CassetteResponse::sse("data: first\n\n"),
        ))
        .interaction(Interaction::new(
            CassetteRequest::new(
                "POST",
                "/v1/chat/completions",
                vec![],
                r#"{"model":"gpt-4.1","messages":[{"role":"user","content":"again"}]}"#,
            ),
            CassetteResponse::sse("data: second\n\n"),
        ))
        .build()
}

#[tokio::test]
async fn player_answers_the_recorded_request() {
    let server = chat_cassette().start().await;

    // Same JSON value, different formatting and key order.
    let (status, body) = post(
        server.url(),
        "/v1/chat/completions",
        &[("content-type", "application/json")],
        "{ \"messages\": [{\"content\": \"hi\", \"role\": \"user\"}], \"model\": \"gpt-4.1\" }",
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body, "data: first\n\n");
    assert_eq!(server.hits(), vec![0]);
    server.assert_no_misses();
}

#[tokio::test]
async fn player_serves_each_interaction_independently() {
    let server = chat_cassette().start().await;

    post(
        server.url(),
        "/v1/chat/completions",
        &[],
        r#"{"model":"gpt-4.1","messages":[{"role":"user","content":"again"}]}"#,
    )
    .await;
    let (_, body) = post(
        server.url(),
        "/v1/chat/completions",
        &[],
        r#"{"model":"gpt-4.1","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;

    // Order does not matter; the request does.
    assert_eq!(body, "data: first\n\n");
    assert_eq!(server.hits(), vec![1, 0]);
    server.assert_no_misses();
}

#[tokio::test]
async fn a_changed_request_is_a_miss_with_the_first_difference() {
    let server = chat_cassette().start().await;

    let (status, body) = post(
        server.url(),
        "/v1/chat/completions",
        &[],
        r#"{"model":"gpt-4.1","messages":[{"role":"user","content":"bye"}]}"#,
    )
    .await;

    assert_eq!(status, 500);
    assert!(body.contains("cassette miss"), "{body}");
    let misses = server.misses();
    assert_eq!(misses.len(), 1);
    let closest = misses[0].closest.as_deref().expect("a closest interaction");
    assert!(closest.contains("messages[0].content"), "{closest}");
    assert!(closest.contains("\"hi\""), "{closest}");
}

#[tokio::test]
async fn recorder_round_trips_into_a_replayable_cassette() {
    let origin = FauxProvider::start(vec![Turn::sse(vec![
        Chunk::text("data: {\"ok\":true}\n\n"),
        Chunk::text("data: [DONE]\n\n"),
    ])])
    .await;
    let recorder = Recorder::start("test/round-trip", origin.url())
        .await
        .expect("loopback recording is allowed");
    recorder.provider("test");
    recorder.protocol("openai-chat");
    recorder.model("gpt-4.1");

    let path = "/v1/chat/completions?key=sk-secret&alt=sse";
    let (status, body) = post(
        recorder.url(),
        path,
        &[
            ("authorization", "Bearer sk-live-123"),
            ("x-api-key", "sk-live-456"),
            ("content-type", "application/json"),
        ],
        r#"{"model":"gpt-4.1"}"#,
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body, "data: {\"ok\":true}\n\ndata: [DONE]\n\n");
    assert_eq!(recorder.interaction_count(), 1);

    // The upstream saw the real credentials; the cassette never does.
    let forwarded = origin.requests().await;
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0].path, path);
    assert_eq!(
        forwarded[0].header("authorization"),
        Some("Bearer sk-live-123")
    );

    let cassette = recorder.cassette();
    assert_eq!(cassette.name, "test/round-trip");
    assert_eq!(cassette.protocol, "openai-chat");
    assert_eq!(
        cassette
            .redactions
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["authorization", "key", "x-api-key"]
    );
    let interaction = &cassette.interactions[0];
    assert_eq!(
        interaction.request.path,
        "/v1/chat/completions?key=REDACTED&alt=sse"
    );
    assert_eq!(
        header(&interaction.request.headers, "authorization"),
        Some("REDACTED")
    );
    assert_eq!(
        header(&interaction.request.headers, "x-api-key"),
        Some("REDACTED")
    );
    assert_eq!(interaction.response.status, 200);

    // Saved, loaded, replayed: the response body survives byte for byte.
    let directory = tempfile::tempdir().expect("temp dir");
    let file = directory.path().join("round-trip.json");
    cassette.save(&file).expect("save");
    let loaded = Cassette::load(&file).expect("load");
    let replay = loaded.start().await;
    let (status, replayed) = post(
        replay.url(),
        "/v1/chat/completions?key=REDACTED&alt=sse",
        &[],
        r#"{"model":"gpt-4.1"}"#,
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(replayed, body);
    replay.assert_no_misses();
}

#[tokio::test]
async fn recorder_forwards_and_records_error_responses() {
    let origin = FauxProvider::start(vec![Turn::text(429, "slow down")]).await;
    let recorder = Recorder::start("test/errors", origin.url())
        .await
        .expect("loopback recording is allowed");

    let (status, body) = post(recorder.url(), "/v1/chat/completions", &[], "{}").await;

    assert_eq!(status, 429);
    assert_eq!(body, "slow down");
    let cassette = recorder.cassette();
    assert_eq!(cassette.interactions[0].response.status, 429);
    assert_eq!(cassette.interactions[0].response.body, "slow down");
}

#[test]
fn recording_is_replay_only_in_ci() {
    assert_eq!(
        recording_mode(false, false, false).expect("default mode"),
        CassetteMode::Replay
    );
    assert_eq!(
        recording_mode(false, true, false).expect("CI without opt-in"),
        CassetteMode::Replay
    );
    assert_eq!(
        recording_mode(true, false, false).expect("opted in"),
        CassetteMode::Record
    );
    assert!(matches!(
        recording_mode(true, true, false),
        Err(CassetteError::CiMustReplay)
    ));

    // A loopback origin is a fixture server, so it is always allowed.
    assert_eq!(
        recording_mode(false, true, true).expect("loopback"),
        CassetteMode::Record
    );
    assert_eq!(
        recording_mode(true, true, true).expect("loopback"),
        CassetteMode::Record
    );

    // This test process is not opted into recording.
    if std::env::var_os("KEEL_CASSETTE_RECORD").is_none() {
        assert_eq!(cassette_mode().expect("mode"), CassetteMode::Replay);
    }
}

#[test]
fn loopback_hosts_are_recognised() {
    assert!(is_loopback("http://127.0.0.1:8080"));
    assert!(is_loopback("https://localhost/v1"));
    assert!(is_loopback("http://[::1]:9000/v1"));
    assert!(is_loopback("http://user:pass@127.0.0.1:9000"));
    assert!(!is_loopback("https://api.openai.com/v1"));
    assert!(!is_loopback("https://127.0.0.1.example.com"));
    assert!(!is_loopback("not a url"));
}

#[tokio::test]
async fn recorder_refuses_a_remote_origin_without_opting_in() {
    if std::env::var_os("KEEL_CASSETTE_RECORD").is_some() {
        // A developer opted in; the refusal is covered by the policy test.
        return;
    }

    let error = Recorder::start("test/refused", "https://api.openai.com")
        .await
        .err()
        .expect("remote recording is refused");
    assert!(matches!(error, CassetteError::RecordingDisabled));
}

#[tokio::test]
async fn recorder_rejects_an_upstream_that_is_not_a_url() {
    let error = Recorder::start("test/bad", "api.openai.com")
        .await
        .err()
        .expect("a bare host is not a URL");
    assert!(matches!(error, CassetteError::BadUpstream(_)));
}

#[tokio::test]
async fn player_closes_the_connection_after_the_body() {
    // Guards the framing: the client sees a complete response and EOF, so a
    // test cannot hang on a keep-alive connection the server never closes.
    let server = chat_cassette().start().await;
    let (status, _) = tokio::time::timeout(
        Duration::from_secs(5),
        post(
            server.url(),
            "/v1/chat/completions",
            &[],
            r#"{"model":"gpt-4.1","messages":[{"role":"user","content":"hi"}]}"#,
        ),
    )
    .await
    .expect("no timeout");
    assert_eq!(status, 200);
}

/// Reads until `needle` appears, leaving the rest of the stream unread.
async fn read_until(socket: &mut TcpStream, needle: &str) -> String {
    let mut seen = Vec::new();
    let mut scratch = [0u8; 1024];
    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if String::from_utf8_lossy(&seen).contains(needle) {
                break;
            }
            match socket.read(&mut scratch).await {
                Ok(0) | Err(_) => break,
                Ok(read) => seen.extend_from_slice(&scratch[..read]),
            }
        }
        seen
    })
    .await
    .expect("the first chunk arrives while the upstream is still open");
    String::from_utf8_lossy(&collected).into_owned()
}

#[tokio::test]
async fn recorder_streams_before_the_upstream_finishes() {
    let (second, release) = Chunk::gated("data: two\n\n");
    let origin =
        FauxProvider::start(vec![Turn::sse(vec![Chunk::text("data: one\n\n"), second])]).await;
    let recorder = Recorder::start("test/streaming", origin.url())
        .await
        .expect("loopback recording is allowed");

    let address = recorder.url().trim_start_matches("http://").to_owned();
    let mut socket = TcpStream::connect(&address).await.expect("connect");
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {address}\r\nContent-Length: 2\r\n\
         Connection: close\r\n\r\n{{}}"
    );
    socket.write_all(request.as_bytes()).await.expect("write");

    let seen = read_until(&mut socket, "data: one\n\n").await;
    assert!(seen.contains("data: one\n\n"), "{seen}");
    assert!(
        !seen.contains("data: two"),
        "the gated chunk must not arrive early: {seen}"
    );

    release.send(()).expect("release the second chunk");
    let mut rest = Vec::new();
    socket.read_to_end(&mut rest).await.expect("read the rest");
    assert!(String::from_utf8_lossy(&rest).contains("data: two"));

    for _ in 0..200 {
        if recorder.interaction_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let cassette = recorder.cassette();
    assert_eq!(cassette.interactions.len(), 1);
    assert_eq!(
        cassette.interactions[0].response.body,
        "data: one\n\ndata: two\n\n"
    );
}

#[tokio::test]
async fn a_server_keeps_a_bounded_request_log() {
    // A recorder-style server can outlive many requests; its log must not grow
    // without limit.
    let provider = FauxProvider::start(Vec::new()).await;
    for _ in 0..260 {
        post(provider.url(), "/v1/chat/completions", &[], "{}").await;
    }
    assert_eq!(provider.request_count().await, 256);
}

#[test]
fn a_cassette_from_an_unknown_version_is_rejected() {
    let error = Cassette::parse(r#"{"version": 99, "name": "x", "interactions": []}"#)
        .expect_err("an unknown version is a hard error");
    assert!(matches!(
        error,
        CassetteError::UnsupportedVersion { found: 99 }
    ));
}

#[test]
fn the_difference_names_the_first_edited_field() {
    let expected = serde_json::json!({"a": 1, "b": [1, 2, {"c": "x"}]});
    let actual = serde_json::json!({"a": 1, "b": [1, 2, {"c": "y"}]});

    assert_eq!(
        json_difference(&expected, &actual).as_deref(),
        Some(".b[2].c: \"x\" != \"y\"")
    );
    assert_eq!(json_difference(&expected, &expected), None);
}
