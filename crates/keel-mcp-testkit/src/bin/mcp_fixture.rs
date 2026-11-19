//! A deterministic MCP stdio server used by keel's tests.
//!
//! Usage: `mcp_fixture [mode] [marker-path]`.
//! Mode `crash` exits before `initialize`; `crlf` and `blank` change output
//! framing. Every other mode serves the same tool set. When `marker-path` is
//! present, calls append `start name nanos` and `end name nanos` lines so tests
//! can prove a denied call never arrived and can observe batching.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

#[derive(Clone, Copy)]
struct OutputMode {
    crlf: bool,
    blank: bool,
}

#[derive(Default)]
struct State {
    responses: Mutex<HashMap<String, Value>>,
    changed: Condvar,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "normal".to_owned());
    let marker = args.next().map(PathBuf::from);
    let output_mode = OutputMode {
        crlf: mode == "crlf",
        blank: mode == "blank",
    };
    if mode == "crash" {
        eprintln!("fixture crashed intentionally");
        std::process::exit(2);
    }

    let state = Arc::new(State::default());
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let id = request.get("id").cloned();

        if method.is_empty() {
            if let Some(id) = id {
                state.responses.lock().unwrap().insert(id_key(&id), request);
                state.changed.notify_all();
            }
            continue;
        }
        if method.starts_with("notifications/") {
            continue;
        }

        let state = state.clone();
        let stdout = stdout.clone();
        let marker = marker.clone();
        let request_mode = mode.clone();
        std::thread::spawn(move || {
            handle_request(
                state,
                stdout,
                output_mode,
                marker,
                request_mode,
                method,
                id,
                request,
            );
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_request(
    state: Arc<State>,
    stdout: Arc<Mutex<std::io::Stdout>>,
    output_mode: OutputMode,
    marker: Option<PathBuf>,
    mode: String,
    method: String,
    id: Option<Value>,
    request: Value,
) {
    if mode == "binary" && method == "initialize" {
        write_raw(&stdout, output_mode, b"\xff\xfe");
        return;
    }
    if mode == "metadata" && method == "tools/list" {
        respond(
            &stdout,
            output_mode,
            id,
            json!({"tools": [
                {
                    "name": "bad_schema",
                    "description": "not an object schema",
                    "inputSchema": {"type": "string"}
                },
                tool("long_desc", &"d".repeat(3000), "value")
            ]}),
        );
        return;
    }
    match method.as_str() {
        "initialize" => respond(
            &stdout,
            output_mode,
            id,
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "keel-mcp-fixture", "version": "0.1.0"}
            }),
        ),
        "tools/list" => {
            let cursor = request
                .get("params")
                .and_then(|params| params.get("cursor"))
                .and_then(Value::as_str);
            if mode == "paged" && cursor.is_none() {
                respond(
                    &stdout,
                    output_mode,
                    id,
                    json!({
                        "tools": [tool("echo", "Echo a value", "value")],
                        "nextCursor": "page-two"
                    }),
                );
            } else if mode == "paged" {
                respond(
                    &stdout,
                    output_mode,
                    id,
                    json!({"tools": [tool("slow", "Sleep before answering", "value")]}),
                );
            } else {
                respond(
                    &stdout,
                    output_mode,
                    id,
                    json!({"tools": [
                        tool("echo", "Echo a value", "value"),
                        tool("progress", "Echo with a progress notification", "value"),
                        tool("slow", "Sleep before answering", "value"),
                        tool("large", "Return a large text result", "value"),
                        tool("fail", "Return an MCP tool error", "value"),
                        tool("malformed", "Return malformed JSON", "value"),
                        tool("oversized", "Return an oversized JSON line", "value"),
                        tool("out_of_order", "Answer with a bogus id first", "value"),
                        tool("string_id", "Echo the request id as a string", "value"),
                        tool("ping", "Send a server ping before answering", "value"),
                        tool("roots", "Send an unsupported server request", "value"),
                        tool("crash_mid", "Exit the server while handling the call", "value"),
                        tool("hang", "Never answer", "value")
                    ]}),
                );
            }
        }
        "tools/call" => {
            let name = request
                .get("params")
                .and_then(|params| params.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            note(&marker, "start", &name);
            match name.as_str() {
                "echo" => text_result(&stdout, output_mode, id, "hello from the fixture"),
                "progress" => {
                    if let Some(id) = &id {
                        write_line(
                            &stdout,
                            output_mode,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/progress",
                                "params": {
                                    "progressToken": format!("{id}"),
                                    "progress": 1,
                                    "total": 2
                                }
                            }),
                        );
                    }
                    text_result(&stdout, output_mode, id, "progress finished");
                }
                "slow" => {
                    let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
                    state.max_active.fetch_max(active, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(500));
                    let overlap = state.max_active.load(Ordering::SeqCst);
                    state.active.fetch_sub(1, Ordering::SeqCst);
                    text_result(
                        &stdout,
                        output_mode,
                        id,
                        &format!("slow result (overlap {overlap})"),
                    );
                }
                "hang" => {
                    std::thread::sleep(Duration::from_secs(600));
                }
                "large" => {
                    let text = format!("START{}END", "x".repeat(200_000));
                    text_result(&stdout, output_mode, id, &text);
                }
                "fail" => {
                    write_line(
                        &stdout,
                        output_mode,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "isError": true,
                                "content": [{"type": "text", "text": "fixture failure"}]
                            }
                        }),
                    );
                }
                "malformed" => {
                    write_raw(&stdout, output_mode, b"this is not json");
                }
                "oversized" => {
                    let text = "x".repeat(1_100_000);
                    text_result(&stdout, output_mode, id, &text);
                }
                "out_of_order" => {
                    let bogus = id
                        .as_ref()
                        .and_then(Value::as_u64)
                        .map(|id| id + 1000)
                        .unwrap_or(9999);
                    text_result(&stdout, output_mode, Some(json!(bogus)), "bogus");
                    text_result(&stdout, output_mode, id, "out of order result");
                }
                "string_id" => {
                    let string_id = id.as_ref().map(|id| json!(id.to_string()));
                    text_result(&stdout, output_mode, string_id, "string id result");
                }
                "ping" => {
                    write_line(
                        &stdout,
                        output_mode,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": "server-ping-1",
                            "method": "ping"
                        }),
                    );
                    let response = wait_response(&state, "server-ping-1", Duration::from_secs(2));
                    let text = match response {
                        Some(response) if response.get("result") == Some(&json!({})) => "pong",
                        _ => "no pong",
                    };
                    text_result(&stdout, output_mode, id, text);
                }
                "roots" => {
                    write_line(
                        &stdout,
                        output_mode,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": "server-roots-1",
                            "method": "roots/list"
                        }),
                    );
                    let response = wait_response(&state, "server-roots-1", Duration::from_secs(2));
                    let text = match response {
                        Some(response)
                            if response
                                .get("error")
                                .and_then(|error| error.get("code"))
                                .and_then(Value::as_i64)
                                == Some(-32601) =>
                        {
                            "method not found"
                        }
                        _ => "unexpected roots response",
                    };
                    text_result(&stdout, output_mode, id, text);
                }
                "crash_mid" => {
                    std::process::exit(3);
                }
                "" => rpc_error(&stdout, output_mode, id, -32602, "missing tool name"),
                other => rpc_error(
                    &stdout,
                    output_mode,
                    id,
                    -32602,
                    &format!("unknown fixture tool `{other}`"),
                ),
            }
            note(&marker, "end", &name);
        }
        other => rpc_error(
            &stdout,
            output_mode,
            id,
            -32601,
            &format!("unknown method `{other}`"),
        ),
    }
}

fn id_key(id: &Value) -> String {
    match id {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn wait_response(state: &State, id: &str, timeout: Duration) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    let mut responses = state.responses.lock().unwrap();
    loop {
        if let Some(response) = responses.remove(id) {
            return Some(response);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let (next, _) = state.changed.wait_timeout(responses, remaining).ok()?;
        responses = next;
    }
}

fn note(marker: &Option<PathBuf>, event: &str, name: &str) {
    let Some(marker) = marker else {
        return;
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(marker)
    {
        let _ = writeln!(file, "{event} {name} {nanos}");
    }
}

fn tool(name: &str, description: &str, property: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {property: {"type": "string"}},
            "required": [property]
        }
    })
}

fn text_result(stdout: &Mutex<std::io::Stdout>, mode: OutputMode, id: Option<Value>, text: &str) {
    write_line(
        stdout,
        mode,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "isError": false,
                "content": [{"type": "text", "text": text}]
            }
        }),
    );
}

fn respond(stdout: &Mutex<std::io::Stdout>, mode: OutputMode, id: Option<Value>, result: Value) {
    write_line(
        stdout,
        mode,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    );
}

fn rpc_error(
    stdout: &Mutex<std::io::Stdout>,
    mode: OutputMode,
    id: Option<Value>,
    code: i64,
    message: &str,
) {
    write_line(
        stdout,
        mode,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        }),
    );
}

fn write_line(stdout: &Mutex<std::io::Stdout>, mode: OutputMode, value: &Value) {
    let Ok(bytes) = serde_json::to_vec(value) else {
        return;
    };
    write_raw(stdout, mode, &bytes);
}

fn write_raw(stdout: &Mutex<std::io::Stdout>, mode: OutputMode, bytes: &[u8]) {
    let Ok(mut stdout) = stdout.lock() else {
        return;
    };
    if mode.blank {
        let _ = stdout.write_all(b"\n");
    }
    let _ = stdout.write_all(bytes);
    let _ = stdout.write_all(if mode.crlf { b"\r\n" } else { b"\n" });
    let _ = stdout.flush();
}
