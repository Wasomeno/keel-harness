use std::sync::Arc;

use keel_core::{Session, SessionLog, SessionMeta, SessionSink};
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{Auth, Client, CredentialRef, EnvResolver, Framing, Route, SecretResolver};
use keel_llm_types::{Compat, Content, ErrorKind, Message, ProtocolId, RepairNote};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

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
    }
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

async fn read_request(socket: &mut TcpStream) -> String {
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
                    let headers = String::from_utf8_lossy(&captured[..end]).to_ascii_lowercase();
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

    String::from_utf8_lossy(&captured).into_owned()
}

async fn serve(responses: Vec<Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_task = captured.clone();

    tokio::spawn(async move {
        for response in responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let request = read_request(&mut socket).await;
            captured_task.lock().await.push(request);
            let _ = socket.write_all(&response).await;
            let _ = socket.flush().await;
        }
    });

    (format!("http://{address}"), captured)
}

fn request_body(request: &str) -> Value {
    let start = request.find("\r\n\r\n").expect("request has headers") + 4;
    serde_json::from_str(&request[start..]).expect("request body is json")
}

#[tokio::test]
async fn a_tool_loop_replays_a_synthesized_result() {
    let tool_turn = concat!(
        "data: {\"model\":\"gpt-4.1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call.1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let answer_turn = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    let (endpoint, captured) = serve(vec![sse(tool_turn), sse(answer_turn)]).await;
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let mut session = Session::new(route(endpoint), client);

    let first = session.step(Some("read the file")).await.unwrap();
    assert!(first.message.has_tool_calls());
    assert_eq!(first.message.usage.output_tokens, 0);

    let call_id = match &first.message.parts[0].content {
        Content::ToolCall { id, .. } => id.clone(),
        other => panic!("expected a tool call, got {other:?}"),
    };
    assert_eq!(call_id.as_str(), "call.1");
    let source = first.message.parts[0]
        .source
        .as_ref()
        .expect("provenance on model parts");
    assert_eq!(source.provider.as_str(), "test");
    assert_eq!(source.model.as_str(), "gpt-4.1");
    assert_eq!(source.protocol, ProtocolId::OpenAiChat);

    let second = session.step(Some("continue")).await.unwrap();
    assert_eq!(second.text(), "done");
    assert!(
        second
            .notes
            .iter()
            .any(|note| matches!(note, RepairNote::NormalizedToolId { .. }))
    );

    let requests = captured.lock().await;
    let body = request_body(&requests[1]);
    let messages = body["messages"].as_array().unwrap();
    let tool_message = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("a tool result was sent");
    assert_eq!(tool_message["tool_call_id"], "call_1");
    // Chat Completions has no `is_error` on tool messages, so the encoder
    // marks failure in the content itself.
    assert_eq!(tool_message["content"], "ERROR: No result provided");

    assert_eq!(session.history.len(), 4);
}

#[tokio::test]
async fn pre_stream_failures_leave_history_untouched() {
    struct NoSecrets;

    impl SecretResolver for NoSecrets {
        fn resolve(&self, _reference: &CredentialRef) -> Option<String> {
            None
        }
    }

    let client = Client::with_resolver(Arc::new(NoSecrets)).unwrap();
    let mut route = route("http://127.0.0.1:1/v1/chat/completions".to_owned());
    route.auth = Auth::bearer(CredentialRef::from_env("MISSING_KEY"));
    let mut session = Session::new(route, client);

    let error = session.step(Some("hello")).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Auth);
    assert!(session.history.is_empty());
}

#[tokio::test]
async fn connection_failures_do_not_commit_user_input() {
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let mut session = Session::new(
        route("http://127.0.0.1:1/v1/chat/completions".to_owned()),
        client,
    );
    session.retry.max_attempts = 1;

    let error = session.step(Some("hello")).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Network);
    assert!(session.history.is_empty());
}

#[tokio::test]
async fn a_mid_stream_failure_keeps_partial_output() {
    let body = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
        "data: {\"error\":{\"message\":\"upstream exploded\",\"code\":\"boom\"}}\n\n",
    );
    let (endpoint, _captured) = serve(vec![sse(body)]).await;
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let mut session = Session::new(route(endpoint), client);

    let outcome = session.step(Some("hello")).await.unwrap();
    assert_eq!(outcome.text(), "partial");
    assert!(outcome.failed());
    assert!(outcome.message.error.is_some());
    assert_eq!(session.history.len(), 2);
}

#[derive(Debug, Default)]
struct MemorySink {
    messages: std::sync::Mutex<Vec<Message>>,
}

impl SessionSink for MemorySink {
    fn message(&self, message: &Message) {
        self.messages.lock().expect("sink").push(message.clone());
    }
}

fn text_turn(text: &str) -> Vec<u8> {
    sse(&format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}},\
         \"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
    ))
}

#[tokio::test]
async fn a_sink_sees_every_message_that_enters_history() {
    let (endpoint, _captured) = serve(vec![text_turn("hello")]).await;
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let sink = Arc::new(MemorySink::default());
    let mut session = Session::new(route(endpoint), client).with_sink(sink.clone());

    session.step(Some("hi")).await.unwrap();

    let recorded = sink.messages.lock().expect("sink");
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    assert_eq!(recorded[0].text(), "hi");
    assert_eq!(recorded[1].text(), "hello");
    drop(recorded);
    assert_eq!(
        sink.messages.lock().expect("sink").len(),
        session.history.len()
    );
}

#[tokio::test]
async fn a_session_log_holds_the_transcript_of_a_turn() {
    let (endpoint, _captured) = serve(vec![text_turn("hello")]).await;
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let directory = tempfile::tempdir().expect("temp dir");
    let meta = SessionMeta::new(keel_core::store::new_id(), "test", "gpt-4.1", "openai-chat");
    let log = Arc::new(SessionLog::create(directory.path(), &meta).expect("create the log"));
    let mut session = Session::new(route(endpoint), client).with_sink(log.clone());

    session.step(Some("hi")).await.unwrap();

    assert_eq!(log.failure(), None);
    let (loaded, messages) = keel_core::store::read(log.path()).expect("read the log");
    assert_eq!(loaded, meta);
    assert_eq!(messages.len(), session.history.len());
    assert_eq!(messages[0].text(), "hi");
    assert_eq!(messages[1].text(), "hello");
}
