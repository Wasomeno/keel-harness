# MCP Streamable HTTP

## Problem

`keel-mcp` only spoke newline-delimited JSON-RPC over a stdio child process, so
a remote MCP server that exposes the Streamable HTTP transport could not be
used. Adding HTTP next to the stdio code risked two copies of the handshake,
pagination, result rendering and error text, and a second set of permission,
timeout and cancellation semantics.

## Design

### Configuration

`ServerConfig` keeps one shared shape (`enabled`, `parallel_safe`,
`startup_timeout_ms`) and accepts exactly one transport:

- `command` plus optional `args` and `env` selects stdio.
- `url` plus optional `headers` selects Streamable HTTP.

Both or neither skips that server with a warning; the rest of the file still
loads. `args`/`env` on a URL server and `headers` on a command server are
warnings, not fatal, because the server is otherwise usable. `headers` is a
table of strings; values expand `${VAR}` exactly once from the process
environment. A missing or empty variable, a non-token name, a byte outside
visible ASCII/tab (including CR/LF), or a reserved transport header
(`content-type`, `accept`, `content-length`, `host`, `mcp-session-id`,
`mcp-protocol-version`) skips that server. Only `http` and `https` URLs are
accepted; userinfo and fragments are rejected. `Debug` prints header names
only, and warnings name the header or variable, never a value.

Expansion happens during file parsing, so a programmatically built
`ServerConfig` is never re-expanded. The public config parse path and its tests
share the same lookup function; tests inject a map instead of mutating the
process environment.

### Transport seam

`McpClient` owns one transport:

```text
McpClient { server, protocol_version, Transport::{Stdio, Http} }
```

`initialize`, paginated `tools/list`, `tools/call`, result flattening, naming,
the startup deadline and the error enum live once. `Transport` only provides
`request`, `notify`, `set_protocol_version`, `shutdown`, `is_running` and error
decoration. The stdio module keeps the existing writer task, pending-id map,
line-bounded reader, stderr tail and kill/reap behavior; the HTTP module is
new. `Drop` keeps killing a stdio child and sends the HTTP `DELETE` best effort.

### Session handling

Every JSON-RPC request is one POST with `Content-Type: application/json` and
`Accept: application/json, text/event-stream`. The `initialize` POST carries no
session headers. Its response may include `Mcp-Session-Id`, stored for later
requests; the shared client then calls `set_protocol_version` with the validated
negotiated version (`2025-06-18` preferred, `2024-11-05` accepted) before
`notifications/initialized`. Every later request carries both
`MCP-Protocol-Version` and, when present, `Mcp-Session-Id`. A 404 on a request
that had a session is reported as an expired session that needs a restart. The
client never opens the optional GET stream, so it has no separate
server-to-client channel to resume.

### Response handling and bounds

A `200 application/json` is one JSON-RPC message. A `200 text/event-stream` is
parsed as SSE: `data:` fields accumulate into a frame, a blank line dispatches
it, other fields and comments are ignored, and a final unterminated line is
flushed at EOF. Each frame is one JSON-RPC message. Notifications are ignored;
a response for another id is ignored on a stream; a server-initiated request is
answered with `-32601` on a detached POST bounded by a two-second timeout. A
stream that ends before the matching response is a protocol error. `202` is
success for a notification; a request answered with `202` is a protocol error.

The JSON body, a single SSE line and an accumulated SSE frame are each capped
at 1 MiB before more is buffered. A JSON-RPC batch array is rejected because
MCP `2025-06-18` removed batching. A response with an unsupported content type
is a protocol error.

### Cancellation and shutdown

HTTP has no writer task: each call owns its POST, so `parallel_safe` calls
overlap without a shared lock. A `CancelGuard` snapshots the request context and
is disarmed on every completed exchange. If the caller drops the future, or the
deadline expires, the guard spawns a bounded, best-effort
`notifications/cancelled` POST and ignores its result. It never waits for that
task, so cancellation and shutdown return promptly.

`shutdown` marks the transport closed, then sends `DELETE` with the session id
(and protocol version) and ignores the response or error. Dropping the last
client does the same from a detached task when the runtime is still alive; an
explicit `shutdown` is the reliable path. No MCP call is retried.

### Error mapping

| Condition | Error | Text |
|---|---|---|
| 401/403 | `McpError::HeadersRejected` | the server rejected the request; check the URL and configured headers |
| 404 after a session | `McpError::SessionExpired` | the session expired; restart keel |
| 429/5xx (and other non-2xx) | `McpError::Http` | names the status and a bounded body |
| connect/DNS/reset | `McpError::Io` | the HTTP request failed |
| deadline | `McpError::Timeout` | did not answer within the tool timeout |
| bad JSON, SSE, batch, no response | `McpError::Protocol` | names the offending shape |

HTTP errors never mention stderr: the stderr-tail decoration is applied only by
the stdio transport, after it has reaped the child. Header values are not part
of any error; the 401/403 text names the URL and any configured headers to
check. Server-supplied JSON-RPC `error.message` and `data` text is scrubbed too,
so a server that reflects a configured header cannot put it in the transcript.

## Guarantees

- `cargo fmt --all --check` and
  `cargo clippy --workspace --all-targets -- -D warnings` are clean.
- The stdio tests are unchanged and stay green; stdio error text, the startup
  deadline, stderr tails and parallel semantics are unchanged.
- Config warnings skip only the offending server. A `${VAR}` value is expanded
  once and never re-expanded.
- An HTTP call can be cancelled or time out without blocking the caller or
  corrupting the next call.
- A request's session and protocol headers are visible on the wire exactly
  where the protocol expects them, including `notifications/initialized`.

## Trade-offs and unverified items

- Static headers are the only authentication. There is no OAuth discovery,
  token refresh or interactive prompt, so a server that requires OAuth is not
  supported. Header values come from the process environment at load time; a
  rotated token needs a new process.
- The optional GET server-to-client stream, SSE resumability (`Last-Event-ID`),
  and JSON-RPC batching are out of scope. A server that relies on GET for
  server-initiated messages will not reach the client.
- Redirects are not followed. A 3xx is a typed HTTP error rather than a
  surprise origin for the session headers.
- A server-initiated request is always answered with `-32601`, including
  `ping`; the stdio path still answers `ping` with `{}`.
- Buffering is bounded per body, line and frame, not over a whole SSE stream;
  an endless stream of small notifications is bounded only by the call
  deadline. The tool timeout is the outer bound.
- The tests use `FauxProvider` over loopback HTTP. No live HTTPS MCP server,
  OAuth flow, resumable stream, GET stream or real remote server has been
  exercised in CI; TLS rests on the workspace `reqwest` rustls feature, which
  the rest of the workspace already uses.
- A `DELETE` on `Drop` is best effort and may not reach the server when the
  process exits or the runtime is shutting down.

## Tests

`keel-mcp/src/config.rs` covers URL parsing, header expansion once, missing and
empty variables, reserved/non-token/CRLF headers, bad schemes, userinfo,
fragments, and `Debug` redaction. `keel-mcp/src/http.rs` has SSE parser unit
tests. `keel-mcp-testkit/tests/http.rs` has 24 integration tests: the JSON path
with session/protocol/configured headers, the `2024-11-05` negotiation, the SSE
path with a notification and a server-initiated request (asserting the
`-32601` POST), a stream without the response, 401/403/404/429/500 mapping, a
configured header reflected in a JSON-RPC error or a tool-entry parse error, a
202 answer to a request, a 302 that is not followed, body/line/frame caps,
batch rejection, cancellation, timeout, `DELETE`, two overlapping parallel
calls, and a `Harness` batch proving parallel-safe HTTP tools overlap while
unsafe tools do not. A final
test registers an HTTP tool, proves `DenyAll` never reaches the server, then
proves `AllowAll` returns one result. `keel-cli/tests/tools.rs` binds a listener
and proves an HTTP config accepts no connection without `--mcp`.
