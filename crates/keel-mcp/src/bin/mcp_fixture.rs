//! A deterministic MCP stdio server used by keel's tests.
//!
//! Usage: `mcp_fixture [mode] [marker-path]`.
//! Mode `crash` exits before `initialize`; every other mode serves the same
//! tool set. When `marker-path` is present, every `tools/call` appends the
//! remote tool name to it so tests can prove a denied call never arrived.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "normal".to_owned());
    let marker = args.next();
    if mode == "crash" {
        std::process::exit(2);
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
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
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let id = request.get("id").cloned();
        match method {
            "initialize" => {
                respond(
                    &mut stdout,
                    id,
                    json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "keel-mcp-fixture", "version": "0.1.0"}
                    }),
                );
            }
            "notifications/initialized" => {}
            "tools/list" => {
                let cursor = request
                    .get("params")
                    .and_then(|params| params.get("cursor"))
                    .and_then(Value::as_str);
                if mode == "paged" && cursor.is_none() {
                    respond(
                        &mut stdout,
                        id,
                        json!({
                            "tools": [tool("echo", "Echo a value", "value")],
                            "nextCursor": "page-two"
                        }),
                    );
                } else if mode == "paged" {
                    respond(
                        &mut stdout,
                        id,
                        json!({"tools": [tool("slow", "Sleep before answering", "value")]}),
                    );
                } else {
                    respond(
                        &mut stdout,
                        id,
                        json!({"tools": [
                            tool("echo", "Echo a value", "value"),
                            tool("progress", "Echo with a progress notification", "value"),
                            tool("slow", "Sleep before answering", "value"),
                            tool("large", "Return a large text result", "value"),
                            tool("fail", "Return an MCP tool error", "value"),
                            tool("malformed", "Return malformed JSON", "value"),
                            tool("oversized", "Return an oversized JSON line", "value"),
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
                    .unwrap_or("");
                if let Some(marker) = &marker
                    && let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(marker)
                {
                    let _ = writeln!(file, "{name}");
                }
                match name {
                    "echo" => text_result(&mut stdout, id, "hello from the fixture"),
                    "progress" => {
                        if let Some(id) = &id {
                            write_line(
                                &mut stdout,
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
                        text_result(&mut stdout, id, "progress finished");
                    }
                    "slow" => {
                        std::thread::sleep(Duration::from_millis(500));
                        text_result(&mut stdout, id, "slow result");
                    }
                    "hang" => {
                        std::thread::sleep(Duration::from_secs(600));
                    }
                    "large" => {
                        let text = format!("START{}END", "x".repeat(200_000));
                        text_result(&mut stdout, id, &text);
                    }
                    "fail" => {
                        write_line(
                            &mut stdout,
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
                        let _ = writeln!(stdout, "this is not json");
                        let _ = stdout.flush();
                    }
                    "oversized" => {
                        let text = "x".repeat(1_100_000);
                        text_result(&mut stdout, id, &text);
                    }
                    "" => rpc_error(&mut stdout, id, -32602, "missing tool name"),
                    other => rpc_error(
                        &mut stdout,
                        id,
                        -32602,
                        &format!("unknown fixture tool `{other}`"),
                    ),
                }
            }
            other if id.is_some() => {
                rpc_error(
                    &mut stdout,
                    id,
                    -32601,
                    &format!("unknown method `{other}`"),
                );
            }
            _ => {}
        }
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

fn text_result(stdout: &mut impl Write, id: Option<Value>, text: &str) {
    write_line(
        stdout,
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

fn respond(stdout: &mut impl Write, id: Option<Value>, result: Value) {
    write_line(
        stdout,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    );
}

fn rpc_error(stdout: &mut impl Write, id: Option<Value>, code: i64, message: &str) {
    write_line(
        stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        }),
    );
}

fn write_line(stdout: &mut impl Write, value: &Value) {
    let _ = serde_json::to_writer(&mut *stdout, value);
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}
