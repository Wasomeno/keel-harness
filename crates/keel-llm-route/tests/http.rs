use std::sync::Arc;

use futures_util::StreamExt;
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{Auth, Client, CredentialRef, EnvResolver, Framing, Route, SecretResolver};
use keel_llm_types::{Compat, LlmEvent, Message, ProtocolId, Request, StopReason};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

fn route(endpoint: String) -> Route {
    Route {
        provider: "test".into(),
        model: "gpt-4.1".into(),
        endpoint,
        protocol: Arc::new(OpenAiChat::new(Compat::for_protocol(
            ProtocolId::OpenAiChat,
        ))),
        auth: Auth::None,
        framing: Framing::Sse,
        headers: Vec::new(),
        context_window: 0,
        max_output_tokens: 0,
    }
}

fn request() -> Request {
    Request::new("gpt-4.1").messages(vec![Message::user("hi")])
}

fn http_response(status: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(body);
    response.into_bytes()
}

fn sse(body: &str) -> Vec<u8> {
    http_response("200 OK", &[("Content-Type", "text/event-stream")], body)
}

async fn serve_once(response: Vec<u8>) -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = oneshot::channel();

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };

        let mut captured = Vec::new();
        let mut buffer = [0u8; 8192];
        let mut header_end = None;
        loop {
            match socket.read(&mut buffer).await {
                Ok(0) => break,
                Ok(n) => {
                    captured.extend_from_slice(&buffer[..n]);
                    if header_end.is_none() {
                        header_end = captured
                            .windows(4)
                            .position(|window| window == b"\r\n\r\n")
                            .map(|position| position + 4);
                    }
                    if let Some(end) = header_end {
                        let headers =
                            String::from_utf8_lossy(&captured[..end]).to_ascii_lowercase();
                        let expected = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if captured.len() >= end + expected {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }

        let _ = sender.send(String::from_utf8_lossy(&captured).into_owned());
        let _ = socket.write_all(&response).await;
        let _ = socket.flush().await;
    });

    (format!("http://{address}"), receiver)
}

#[tokio::test]
async fn streams_text_over_http() {
    let body = concat!(
        "data: {\"model\":\"gpt-4.1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (endpoint, _captured) = serve_once(sse(body)).await;
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();

    let mut stream = client.stream(&route(endpoint), &request()).await.unwrap();
    let mut text = String::new();
    let mut stop = None;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            LlmEvent::TextDelta { delta, .. } => text.push_str(&delta),
            LlmEvent::Done { stop: reason } => stop = Some(reason),
            _ => {}
        }
    }

    assert_eq!(text, "hello");
    assert_eq!(stop, Some(StopReason::EndTurn));
}

#[tokio::test]
async fn classifies_http_errors_before_the_stream_opens() {
    let body = r#"{"error":{"message":"rate limited","type":"rate_limit_error"}}"#;
    let response = http_response(
        "429 Too Many Requests",
        &[("Content-Type", "application/json"), ("Retry-After", "2")],
        body,
    );
    let (endpoint, _captured) = serve_once(response).await;
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();

    let error = client
        .stream(&route(endpoint), &request())
        .await
        .unwrap_err();

    assert_eq!(error.kind, keel_llm_types::ErrorKind::RateLimit);
    assert_eq!(error.status, Some(429));
    assert_eq!(error.retry_after, Some(std::time::Duration::from_secs(2)));
    assert!(error.is_retryable());
    assert!(error.message.contains("rate limited"));
}

#[tokio::test]
async fn sends_resolved_credentials() {
    struct Fixed;

    impl SecretResolver for Fixed {
        fn resolve(&self, _reference: &CredentialRef) -> Option<String> {
            Some("sk-test".to_owned())
        }
    }

    let body = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (endpoint, captured) = serve_once(sse(body)).await;
    let client = Client::with_resolver(Arc::new(Fixed)).unwrap();
    let mut route = route(endpoint);
    route.auth = Auth::bearer(CredentialRef::from_env("TEST_KEY"));

    let mut stream = client.stream(&route, &request()).await.unwrap();
    while let Some(event) = stream.next().await {
        event.unwrap();
    }

    let request = captured.await.unwrap().to_ascii_lowercase();
    assert!(request.contains("authorization: bearer sk-test"));
}

#[tokio::test]
async fn a_stream_without_a_done_frame_still_terminates() {
    let body = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
    );
    let (endpoint, _captured) = serve_once(sse(body)).await;
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();

    let mut stream = client.stream(&route(endpoint), &request()).await.unwrap();
    let mut saw_done = false;
    while let Some(event) = stream.next().await {
        if matches!(
            event.unwrap(),
            LlmEvent::Done {
                stop: StopReason::MaxTokens
            }
        ) {
            saw_done = true;
        }
    }
    assert!(saw_done);
}
