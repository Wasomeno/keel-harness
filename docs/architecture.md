# Architecture

## Shape

```
                       keel-cli                  the `keel` binary
                          |
                       keel-core                 session, harness loop, assembly, routing
                  ______/    |    \______
        keel-llm-catalog  keel-tools  keel-llm-route
             catalog         tools     Route + HTTP + SSE
                                          |
                             keel-llm-protocol    one module per wire format
                                          |
                                keel-llm-types    canonical types, events, repair
```

Dependencies point down. A protocol module cannot see the catalog, the session,
or a provider's name. The session cannot see JSON. `keel-tools` depends only on
`keel-llm-types`; it has no idea a model exists. `keel-mcp` sits beside the tool
seam: it depends on `keel-tools` and `keel-llm-types`, and `keel-cli` starts its
servers. `keel-testkit` is a dev-only crate used by route and core tests, and is
not part of the runtime graph. `keel-mcp-testkit` is dev-only too: its
`mcp_fixture` binary and integration tests exercise `keel-mcp` and `Harness`
through a real stdio subprocess, and its HTTP tests drive `FauxProvider`.
`keel-evals` is dev-only as well: it depends on core,
catalog, route, tools, testkit and types, drives `Harness` as a library, and
nothing in the runtime graph depends on it. `keel-catalog-gen` is dev-only too:
it depends on the catalog and writes the committed models.dev snapshot; no build
or runtime code depends on it.

## One request's path

1. `Session::step_streaming` first checks the compaction trigger. If the next
   request would not fit, it summarizes and replaces history before anything is
   sent (see [Compaction](#compaction)).
2. It appends the pending user message to a *preview* of history. History is not
   committed yet.
3. `repair_transcript(preview, target)` returns a replay-safe message list and a
   list of notes (what it dropped, normalized, or synthesized).
4. The canonical `Request` is built: system (keel's default plus any appended
   host text), repaired messages, tools, limits, cache policy.
5. `build_route` has already resolved provider + model + compat into a `Route`:
   endpoint, protocol module, auth reference, framing, context and output limits.
6. `Client::stream` lowers the request through the protocol, resolves the
   credential (environment first, then the store when the resolver has one),
   sends it, and returns an `EventStream` after the response status is known.
7. `EventStream` de-frames SSE, feeds each payload to the protocol decoder, and
   yields canonical `LlmEvent`s, ending with `Done`.
8. `Assembler` turns events into parts, using end events as authoritative and
   keeping partial content if an error arrives.
9. The assistant message gets provenance from the target, is appended to
   history, and `StepOutcome` reports text, usage, notes and warnings.

## The agent loop

`Session` is one model turn. `Harness::run` is the loop above it:

```
input
  └→ Session::step_streaming            one model turn, streamed
       └→ any tool calls in that message?
            ├→ no:  RunStop::EndTurn
            └→ yes: for each call, in order
                 ├→ look up the tool, parse arguments
                 ├→ permission gate
                 ├→ Tool::execute (timeout + cancellation)
                 ├→ one ToolResult part, truncated
                 └→ next model turn with no new user input
```

The loop holds these properties:

- **One result per call.** Unknown tool, malformed arguments, denial, timeout
  and cancellation all produce a tool result with `is_error` set. A provider
  never sees a call without an answer.
- **Results are ordinary parts.** They are appended as one user message carrying
  `Content::ToolResult`, so the transcript the model sees, the transcript the UI
  renders, and the transcript repair reads are the same thing.
- **Cancellation leaves a complete transcript.** The in-flight call gets
  `cancelled`; calls that never started get `cancelled before execution`. The
  loop returns `RunStop::Cancelled` and the history stays replayable.
- **The turn budget is a stop, not an error.** `RunStop::MaxTurns` returns with
  history intact and `last` holding the model's final message.
- **Usage accumulates.** `RunOutcome::usage` sums every completed turn,
  including a turn that failed after producing output. A compaction summary
  request's usage is reported in `CompactionStats`, not added to the assistant
  message, so the trigger keeps reading the last main request's numbers.
- **A failed model turn is retried when the policy allows it.** A retryable
  failure replays the turn with no new input; once retries are exhausted the loop
  returns `RunStop::Failed` with the partial message in `last`.

## Sessions

A session's transcript is its log. `store::SessionLog` is a sink on `Session`:
every append to history goes through `Session::push_message`, which writes one
JSONL record before adding it to memory. A crash therefore loses at most the turn
in flight, and a half-written final line is dropped on read rather than treated
as corruption.

The file is a header followed by message records and, after a compaction, one
compaction record:

```
{"type":"meta","version":2,"id":"1763550000-a1b2","provider":"neuralwatt",...,"max_output_tokens":8192,"temperature":0.2,"cache":"none"}
{"type":"message","at":1763550009,"message":{"role":"user",...}}
{"type":"compaction","at":1763550010,"reason":"threshold","summary":{...},"messages":[...]}
```

Files live under the platform data directory (`KEEL_DATA_DIR` overrides it), one
directory per project, derived from the canonical project path: keel never writes
into the repository, and two projects cannot collide. Resume reads the messages
back into `history` and reopens the file for appending. It also keeps the
recorded route, because switching model mid-session makes repair drop the foreign
reasoning, so the stored target wins and a different command-line target only
warns. The header also records the effective system prompt; resume uses that
stored text as-is. Version 2 additionally records `max_output_tokens`,
`temperature` and the resolved cache policy, which resume applies unless a
command-line flag or `KEEL_CACHE` overrides them; version-1 headers and older
version-2 headers load with those fields as `None`. The cache field makes an
opt-out durable: `--cache none` followed by `--resume` does not silently
re-enable `prompt_cache_key`, and an explicit new flag or env value applies
only to that run without rewriting the header.

## Compaction

A session keeps going when the transcript reaches the model's context window
(O-009 remainder). `Session::step_streaming` checks the trigger before every
request:

```
last_reported_input + last_reported_output + next_output_budget >= context_window
```

`last_reported_*` comes from the newest message carrying provider usage;
`next_output_budget` is `Session::max_output_tokens` (the CLI's `--max-tokens`)
or, when that is unset, the catalog's output limit on the `Route`. The check is
inclusive: equality compacts. When no usage exists yet (the first request, or a
resumed compacted view that carries no assistant usage), the preview request is
measured with the estimator instead; an ordinary resume has the last assistant
message's usage and uses the reported rule. The estimator preview includes the
pending input, while the reported branch is exactly the formula above and does
not: a very large new paste therefore relies on the provider's overflow path
rather than the threshold check. At most one threshold compaction runs per
step.

The typed `ContextOverflow` comes from the HTTP classifier (400/422 context
wording, and 413 as request-too-large) and from in-stream error frames in all
three protocols, which run the same wording and provider-code classifier.
Realistic `prompt is too long`, `input is too long`, `reduce the length of the
messages` and `context_length_exceeded` reach the compaction path instead of
the retry path.

A provider `ContextOverflow` is handled the same way: pre-stream, the request
was never committed, so the user input stays pending; mid-stream, the user input
and the failed assistant turn are already in history, and the replacement view
drops the assistant turn. Either path compacts once and replays the request
once. A second overflow is a normal failure: pre-stream it returns `Err`, mid
stream it returns `StepOutcome::error` and the harness stops with
`RunStop::Failed`. User input is never sent twice.

The summarizer is the session's own route and model, offered no tools
(`ToolChoice::None`, empty tool list). It receives the repaired committed
history plus `COMPACTION_PROMPT` from
`crates/keel-core/src/prompts/compaction.md` as a final user message. For a
threshold or pre-stream compaction the pending input is not committed, so it is
not part of the summary request; on a mid-stream overflow it is already
committed and is included. Either way the pending input is sent exactly once by
the replay, which is what prevents duplication.
The request is checked before it is sent:
`input_estimate + summary_output <= context_window`, where `summary_output` is
`min(4096, context_window / 4, window - input_estimate)`. If that leaves fewer
than 128 tokens, compaction fails with
`cannot compact: the summary request needs about ...`; nothing is sent. A
summary stopped at `MaxTokens` fails with
`cannot compact: the summary was truncated at the output budget` rather than
being accepted. The summary request's reported usage is returned in
`CompactionStats` for a successful compaction and is added to a persistent
session side-usage accumulator as soon as the summary turn finishes. It is not
added to the step's assistant message, so the trigger keeps reading the last
main request's numbers. A summary that is then rejected (truncated, empty, or
followed by an unfit view) still leaves its usage on the session counter: the
compaction returns `Err`, so a caller drains it with
`Session::take_compaction_usage`. A successful run drains the counter into
`RunOutcome::compaction_usage`; the eval budget and the CLI usage line add it to
main usage. The eval gate charges it after the request. A gated run can contain
two compactions (threshold then overflow), so the cap can be exceeded by up to
two summary requests, not one.

The replacement view is, in order:

1. the system prompt (unchanged; it lives on the request, not in history),
2. the newest plain user message, always kept verbatim even when it alone
   exceeds the user budget, then older plain user messages newest-first while
   the kept total stays strictly below `min(20_000, context_window / 4)`
   estimated tokens (and below the room left by the system prompt, tools,
   pending input and output budget), restored to their original order,
3. one user message built by `summary_message`: `SUMMARY_PREFIX` from
   `crates/keel-core/src/prompts/compaction_prefix.md`, then the summary text.

Only `Role::User` messages with text or images and no tool calls or results
qualify. A prior handoff summary is excluded, so summaries do not accumulate.
Assistant turns, tool calls and tool results never cross the cut: a call and its
result leave together, and `repair_transcript` on the view synthesizes no
results. The view is not mutated further by repair; repair still runs on every
request as usual.

The view is then checked as the next request: system, view, pending input,
tools and the next output budget must be strictly below the window, matching
the trigger's inclusive boundary. If they do not fit, compaction fails with
`cannot compact: ...` instead of dropping the newest user message or sending an
oversized request. The message names the actual cause: the output budget alone,
the pending input, the summary/system/tools, or the unavoidable newest user
message. The stored view and the logged record are only written after that
check.

The log appends one `{"type":"compaction", ...}` record holding `reason`
(`threshold` or `overflow`), the summary message, and the full replacement view.
Earlier bytes are never rewritten. Resume applies the latest compaction record
and then appends the message records after it, which reconstructs the same
history the live session held; the next request from either path is byte-equal
for the same header config. The summary message in the record is also the last
element of `messages`, so a reader can use either. Log version 2 adds the record
and the header fields; a version-1 log still loads and can gain compaction
records if it is resumed and compacted (its header is not rewritten).

`Harness` drains the session's compactions after each step and emits one
`RunEvent::Compacted` per compaction with the reason, message counts, estimated
tokens and summary usage. On a successful run it also drains the side-usage
accumulator into `RunOutcome::compaction_usage`; if the run returns `Err`, the
accumulator stays on the session so a budgeting caller can drain it, including
usage from a summary whose compaction failed. A step that threshold-compacts
and then overflow-compacts emits two. The CLI prints one stderr line per
compaction:
`[compact] threshold|overflow: M -> N messages (K kept user), ~A -> ~B tokens`.
A route with no catalog window (`context_window = 0`, such as a custom entry or
a hand-built route) disables compaction; `keel chat` prints one warning at
startup rather than failing silently.

The estimate is deliberately conservative and tokenizer-free:
one token per three UTF-8 bytes (`ceil`), plus 4 tokens per message, 2 per part,
8 per tool spec, and a flat 2,000 per image (base64 is not text). It is used for
the summary-request fit check, the post-compaction view fit check, the
post-compaction trigger fallback, and nothing on the wire.
`crates/keel-core/src/tokens.rs` documents and tests it.

## System prompt

`keel-core/src/prompt.rs` embeds
`crates/keel-core/src/prompts/default_system.md` at compile time as
`DEFAULT_SYSTEM_PROMPT`. There is one prompt, the same for every model and
protocol; when-to-use guidance for each tool stays in the M2 tool descriptions.

`Session::new` starts with the default, so an embedder that never calls
`with_system` still sends it. `Session::with_system(TEXT)` and the CLI's
`--system TEXT` append rather than replace: the effective text is the current
prompt, `SYSTEM_PROMPT_SEPARATOR` (a horizontal rule), then `TEXT`. Assigning
`Session::system` directly replaces the prompt entirely.

When `keel chat` creates a session, the header records that effective text
exactly. `SessionLog` itself writes whatever `SessionMeta` the caller passes,
so a library embedder that writes a log must record `session.system` itself to
keep the same property. Resume sets `Session::system` from the header before
applying any `--system` append, so the default is never prepended twice.
A header written before M3 has `system: None`; resume keeps it prompt-less
rather than injecting the default into an existing transcript, and `--system`
on such a session applies alone. The header is append-only, so a resume with
`--system` appends for that run while the header keeps the prompt recorded at
creation.

## Tools

`keel-tools` is a library. It knows nothing about models, protocols or SSE.

**`Tool`** — `name`, `spec` (the schema the model sees), `summarize` (one line
for the UI), `parallel_safe`, `execute(context)`. Arguments arrive as raw JSON
text, exactly as the model produced them; the tool parses its own arguments, so
a malformed call is a tool error rather than a harness error.

**`ToolRegistry`** — a `BTreeMap` from name to tool. Encoding a request asks for
`specs()`; execution looks up by the name in the call. Ordering is stable, which
keeps prompts and tests stable.

**`Workspace`** — the sandbox. Tools never receive a host path; they resolve
relative paths through a root. `..` is rejected lexically before any filesystem
call. On Unix the file tools do not resolve a path and then open it: every
component is opened with `openat` from an open root directory descriptor, with
`O_NOFOLLOW` and `O_CLOEXEC`, and writes create missing parents with `mkdirat`
at each level. A symlink swapped in between the lexical check and the open
therefore cannot redirect the open outside the root; symlinks inside the root
are refused too, rather than followed. The refusal names the requested path and
points at `bash cat` as the trusted escape hatch. The root descriptor is opened
once with `O_DIRECTORY | O_NOFOLLOW` and shared by clones of the workspace.
`read`, `write` and `edit` run their blocking descriptor I/O on the blocking
pool. `write` and `edit` replace atomically: the new bytes go to a `.keel-*`
temp file created with `openat` in the target's directory
(`O_CREAT | O_EXCL | O_NOFOLLOW`), are fsynced, renamed over the target with
`renameat`, and the directory is fsynced best-effort. A failed write leaves the
old file intact. An existing target's permission bits are applied with `fchmod` right
after the temp is created, masked with `0o777`: the new inode does not carry
setuid, setgid or sticky bits, which an in-place write would have cleared. A
new file gets the process umask through the `0o666` create mode. The target is opened `O_WRONLY | O_NOFOLLOW | O_NONBLOCK` before
the temp is created, so a read-only file is refused rather than replaced just
because its directory is writable. `fstat` must report a regular file, so a
FIFO, socket or device is refused instead of hanging the blocking thread or
being replaced by a regular file. Because the temp entry is created beside the
target, replacement needs write permission on the directory; a writable file
in a read-only directory can no longer be rewritten. The cost is inode
replacement: a replaced file gets a new inode, so hard links keep the old
contents, and ownership, ACLs and extended attributes are not copied.

`bash`, `glob` and `search` still hand a canonical path to a child process, so
they use the canonicalizing resolvers: the lexical check catches `..`, then the
nearest existing ancestor is canonicalized and must start with the canonical
root. That closes the ordinary symlink escape but not a swap between that
canonicalization and the child's own open, which is why those tools are
policy-scoped. `bash` in particular is not OS-confined; its working directory is
inside the workspace, but the command can read and write anything the user can.
`glob` and the `ignore` walker behind `search` skip symlinks by default. On
non-Unix the canonicalizing fallback is used for every tool and the TOCTOU
window remains; Windows is not covered by CI.

Parallel execution is the run loop's, not the tool's. Consecutive calls whose
tools return `parallel_safe()` run as one batch, at most eight at a time, while
a non-safe call is a barrier. Permission checks still happen sequentially before
a batch starts, so an `Ask` host sees one prompt at a time. `ToolStarted` is
emitted in call order as each call is prepared; executions are polled with
`join_all`, and `ToolFinished` and the transcript results are emitted in call
order after the batch, regardless of completion order. Cancellation still
leaves one result per call: an in-flight call gets `cancelled`, a call that
never started gets `cancelled before execution`.

**`PermissionGate`** — `AllowAll`, `DenyAll`, or `Ask`. `Ask` sends a
`PermissionRequest` (tool, one-line summary, raw arguments, a responder) to the
host over an `mpsc` channel and waits. Cancellation, a dropped responder, or no
host at all is a denial. `Harness::new` defaults to `DenyAll`; an embedder must
opt in.

Built-ins:

| tool | does | notes |
|---|---|---|
| `read` | reads a text file with offset/limit | rejects binary content, notes the line range |
| `write` | creates or overwrites a file | atomic replace; creates parent directories; keeps an existing mode |
| `edit` | replaces one exact string | atomic replace; zero or multiple matches is an error unless `replace_all` |
| `bash` | `bash -lc <command>` in the workspace | timeout, cancellation, non-zero exit is an error result |
| `glob` | path patterns | ignore-aware walker, `literal_separator` |
| `search` | ripgrep `--json` | case sensitivity, glob filter, result cap |

Every built-in's `spec().description` has four labelled lines: `Use when`,
`Prefer`, `Returns`, and `On failure`. The `Prefer` line is where the model is
told to use `search` for file contents, `glob` for paths, and `bash` only for
work the structured tools cannot do. Names and JSON schema shapes are part of
the wire contract; the description text is not, so it can change without a
protocol change (the golden bodies still record the text they send).

Failure results are actionable rather than bare: each names the value that
caused the error and the next step. `edit` with zero matches says to `read` the
file and copy the exact text; multiple matches say to add context or set
`replace_all`. A path that escapes the sandbox names the workspace root, and a
missing path points at `glob` or `write`. A malformed glob or regex names the
pattern and the syntax to use. A non-zero `bash` exit keeps the exit code,
stdout and stderr, and adds a retry hint; a timeout names the command and the
cap and stays a typed `ToolError::Timeout` (now carrying a message), so
embedders can still distinguish it; a write failure names the file or parent
directory and says whether to check permissions, disk space, or the parent
path. Successful file operations (`read`, `write`, `edit`, and
`glob`/`search` with matches) keep their text. The `glob` and `search`
no-match lines are `Ok` results, not errors, and now carry check-the-pattern
guidance.

`bash` and `search` shell out, so they need `bash` and `rg` on `PATH`. Tool
output is truncated to 64 KiB by default, keeping a 60/40 head-and-tail split
with an explicit marker, so a failure at the end of a long log stays visible.

### MCP tools

`keel-mcp` is a tool source, not a second harness. It connects to configured
servers, performs the MCP handshake, fetches `tools/list`, and wraps each remote
tool in `McpTool`, which implements `Tool`. `Harness` therefore treats an MCP
call exactly like a built-in: the permission gate runs first, the same
`ToolLimits` timeout wraps it, cancellation drops the future, and
`truncate_output` applies to the final text.

Configuration is per workspace at `.keel/mcp.toml`. Starting it is an explicit
CLI opt-in: `chat --mcp`, `tools --mcp`, or `KEEL_MCP=1` for scripts. Without
it, `chat` and `tools` read the file only to print parse warnings and one
"configured but not started" line for valid servers, list no MCP tools, and
start no server, stdio or HTTP. The `enabled` flag is a secondary filter after
opt-in. `keel route --workspace DIR` names enabled servers without starting
them. `keel tools` shows the registry with a `SOURCE` column and truncates
descriptions.

A server is exactly one transport: `command` runs a stdio child, `url` names a
Streamable HTTP endpoint. A server with both or neither is skipped with a
warning. A stdio server command is arbitrary code with the user's full
privileges, outside the workspace sandbox, equivalent to `bash`. An HTTP server
is contacted only after the same opt-in and receives exactly the static headers
in the file. Neither `--yes` nor the tool permission gate applies to startup;
the opt-in is the trust decision. Config is parsed one server at a time, so one
bad entry is a warning and does not disable its neighbors. `args` and `env`
apply only to stdio; `headers` applies only to HTTP, and a wrong pair is a
warning that leaves the server usable. Header values expand `${VAR}` once from
the process environment; a missing or empty variable, a non-token name, a
CR/LF or other non-visible byte, or a reserved transport header skips that
server. Only `http` and `https` URLs are accepted, with no userinfo and no
fragment. Header values are redacted from `Debug`, warnings and errors.
`startup_timeout_ms` is clamped to 60 seconds, and the handshake and all
`tools/list` pages share one deadline.

`McpClient` is the shared seam: start/connect, `list_tools`, `call_tool` and
`shutdown` dispatch to the stdio or HTTP transport, while the handshake,
pagination, result rendering, timeout and error taxonomy live once.

**stdio transport.** The wire is newline-delimited JSON-RPC 2.0 over stdio.
Keel offers MCP `2025-06-18` and accepts `2025-06-18` or `2024-11-05`; it sends
`notifications/initialized` after a successful handshake. `tools/list` follows
`nextCursor` to a bounded page/tool count. A dedicated reader task skips blank
lines, strips CRLF, routes responses by numeric or numeric-string id, ignores
notifications, answers `ping`, and returns `-32601` for another server-initiated
request. It never routes a method-bearing message to a pending call. A method
without an id is a notification; a response with an unknown or unusable id is
logged distinctly under `KEEL_MCP_DEBUG`. A 1 MiB line cap is enforced before
allocating more. A malformed, non-UTF-8 or overlong line marks the server
closed, fails every pending call, and kills and reaps the child. A dead server
is not restarted.

One writer task owns stdin and writes each line whole. A cancelled or timed-out
call removes its pending entry, sends `notifications/cancelled`, and never
interrupts a partially written line, so the next call still behaves. The request
line is still delivered, so a timed-out call may already have executed on the
server. Dropping the last client aborts both tasks, sends `start_kill`, and
relies on `kill_on_drop` as a fallback. The stderr pipe is drained into a
bounded tail so a crash warning can say something and a chatty server cannot
block.

**Streamable HTTP transport.** One JSON-RPC request is one POST to the
configured URL with `Content-Type: application/json`, `Accept:
application/json, text/event-stream` and the configured headers. A `200` with
`application/json` is one message; a `200` with `text/event-stream` is read as
SSE `data:` frames, one JSON-RPC message per frame. Notifications are ignored;
a server-initiated request is answered with a bounded, best-effort `-32601`
POST. After `initialize`, every request carries `MCP-Protocol-Version` with the
negotiated version and `Mcp-Session-Id` when the server offered one; the
`initialize` POST carries neither. `202 Accepted` is success for notifications.
The response body, an SSE line and an SSE frame are each capped at 1 MiB, and a
JSON-RPC batch array is a protocol error because `2025-06-18` removed batches.
A stream that ends before the matching response is a protocol error.

Non-2xx responses read a bounded body and map to typed errors: 401/403 mean the
server rejected the request, and the message asks the user to check the URL and
any configured headers; 404 after a session means the session expired and keel
must restart; 429/5xx carry the status. No MCP call is retried. The HTTP client
follows no redirects, so the session and configured headers cannot move to
another origin. There is no writer task: `reqwest`'s client is cloneable and
each call owns its POST, so `parallel_safe` calls overlap naturally.
Cancellation or timeout drops the in-flight request and spawns a bounded
`notifications/cancelled`; shutdown sends `DELETE` with the session id and
ignores its error, as does dropping the last client. HTTP errors never mention
stderr, and configured header values are scrubbed from text the server
supplies, including JSON-RPC error messages and their `data`. The optional GET
server-to-client stream, SSE resumability (`Last-Event-ID`), JSON-RPC batching
and MCP OAuth are out of scope; authentication is the static header table,
expanded from the process environment.

Names are `server__tool`. Dots and colons in a remote tool name map to `_`;
other invalid or over-64-character names are skipped with a warning. If a
built-in or an earlier MCP tool already owns the qualified name, the later tool
is skipped with a warning. Remote metadata is bounded: a non-object
`inputSchema`, a schema without `type: "object"`, and a schema over 16 KiB skip
the tool; a missing `properties` is normalized to `{}` for the provider, and
descriptions are cut at 2 KiB with a warning.
Only text content is flattened; images, audio, resources and links become
placeholders, and `structuredContent` is used only when `content` is absent. A
JSON-RPC error, `isError: true`, a closed server, or a protocol error is a tool
result with `is_error` set, never a harness error.

Each call uses `ToolContext.limits.timeout` and honors `ToolContext.cancel`.
The stdio client keeps a request-id pending map; dropping a call removes its
entry and sends `notifications/cancelled` on a best-effort writer send, so the
harness can return one `cancelled` result without waiting for the server. The
reader keeps draining stdout, so a server that writes while a call is cancelled
cannot block on a full pipe. The HTTP path has no pending map or writer: a
cancelled call drops its POST future and sends the same notification on a
detached, bounded task. MCP tools are barriers by default; `parallel_safe =
true` opts a server into the bounded batch. Commands run with `env_clear` plus
an allowlist and a small baseline such as `PATH` and `HOME`. That is hygiene,
not a security boundary: the arbitrary command can still reach anything the
user can, and `env_clear` drops variables such as `SSL_CERT_FILE` and `XDG_*`
unless listed. `args` is free-form; never putting secrets there is guidance,
not enforcement. HTTP header values are the only credential source for a URL
server; there is no OAuth flow, keyring lookup or interactive prompt, and the
server sees every configured value on every request.

`keel-evals` wraps these tools for replay without changing them: the temp
workspace path becomes `<workspace>`, `search` result lines are sorted, and the
eval shell rejects `find`, `ls`, `stat`, `du`, and `tree` because their output
depends on the machine. The model is told to use `glob` instead; the permission
gate is still `AllowAll` and every tool is registered.


## Where the decisions land

Summaries of `D-001`..`D-011` and `O-001`..`O-013` live in
[`docs/decisions.md`](decisions.md).

| Decision | Code |
|---|---|
| D-001 own the provider seam | `keel-llm-protocol` implements the wire; no third-party unified client |
| D-002 hybrid, Rust-native | `Route`, per-model protocol, generated-catalog-shaped data, mock-wire tests |
| D-003 Pi replay discipline | `Source` on every model part; `repair_transcript` is one pure pass |
| D-004 coverage | four `ProtocolId`s; Chat, Messages and Responses implemented; Google is a typed "unsupported" |
| D-005 Antigravity is a provider | catalog entry + auth data; its protocol module is first-party in M11 |
| D-006 Rust | workspace, traits, enums, no `unsafe` |
| D-007 tool execution | `Tool` + `ToolRegistry`, `Harness` loop, channel permission gate, descriptor-relative workspace on Unix, ordered parallel-safe batches |
| D-008 scripted wire, goldens, recorded streams | `keel-testkit` faux server, `keel-llm-protocol` golden requests (`UPDATE_GOLDEN=1`), offline borrowed decoder fixtures, first-party NeuralWatt cassettes (M8), `keel-evals` replay/live suite |
| D-009 stateless Responses replay | `store: false`, `{"id", "encrypted_content"}` reasoning signatures, `call_id` addressing, tool calls emitted at `output_item.done` |
| D-010 ordered request-keyed cassettes and manifest | `CassetteServer`'s cursor (422 miss) + loopback `Recorder` with header/query/body redaction, `UPDATE_CASSETTE=1`, `scenarios.toml` coverage gate |
| D-011 MCP tools are ordinary tools | `keel-mcp` stdio and Streamable HTTP client + `McpTool` adapters; explicit `--mcp`/`KEEL_MCP=1` opt-in; `server__tool` naming; unsafe-by-default barriers; `keel-cli` starts opted-in `.keel/mcp.toml` servers and the harness applies the same gate, timeout, cancellation and truncation |

## Protocol module anatomy

A module implements two traits:

```rust
trait Protocol {
    fn id(&self) -> ProtocolId;
    fn compat(&self) -> Compat;
    fn encode(&self, request: &Request) -> Result<EncodedRequest, LlmError>;
    fn decoder(&self) -> Box<dyn StreamDecoder>;
}

trait StreamDecoder {
    fn frame(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError>;
    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError>;
}
```

`encode` owns body shape and the headers the protocol requires.
`StreamDecoder` owns the state machine. Neither knows where the bytes came from.

Responses is the odd one out: it is event-sourced rather than delta-shaped. Each
frame names an event and an output item, `response.completed` is the terminal
event (there is no `[DONE]`), and reasoning comes back as an opaque
`encrypted_content` blob with an item id. keel stores both in the part's
signature as JSON so a `store: false` transcript can be replayed without
server-side state. A tool call is addressed by `call_id`, not by item id, and is
emitted once at `output_item.done`, which is the event that carries the name and
the complete arguments.

Event contract:

- `TextDelta` / `ReasoningDelta` / `ToolInputDelta` are progress only.
- `TextEnd` / `ReasoningEnd` / `ToolInputEnd` / `ToolCall` are authoritative.
- `Usage` is cumulative; the assembler assigns, it does not add.
- `Done` is mandatory. `Error` may arrive before it.
- Event `index` values are stable slots; ordering of parts is by index.
- Tool calls are keyed by `index` when the provider sends one, and by call `id`
  otherwise. Gemini's OpenAI-compatible endpoint sends parallel calls one per
  chunk with no index; keying by index alone merged them into one malformed
  call. A turn that produced calls is normalized to `ToolUse` even when the
  provider labelled it `stop`.
- A stream that ends without a `finish_reason` still produces `Done`, preceded
  by a `Warning`; silence there would look like a clean completion of a
  truncated turn.

## Prompt caching

`Request::cache` is the only place a policy lives; `Session::with_cache` sets it
for every model turn, and the default `Auto` is what every caller got before M10
(plus the OpenAI key). Compaction summaries always use `None`: they are one-shot
requests over a different message list, so session positions need not fit and
their dropped-position warnings would not be surfaced on that path. `Route` does
not carry a policy: the protocol module decides how to express it. `keel chat`
chooses between `auto` and `none` with `--cache`, or `KEEL_CACHE` when the flag
is absent; strict OpenAI-compatible endpoints that reject `prompt_cache_key`
should opt out with `none`.

Breakpoint indices address the repaired request the encoder actually sends, not
the caller's un-repaired history: `repair_transcript` can remove failed turns
and merge blocks, so a caller deriving positions from its own log must run the
same repair first.

- `None` removes every cache field.
- `Auto` places the last system block and the last cacheable block of the final
  message for Anthropic, and a deterministic `prompt_cache_key` for OpenAI Chat
  and Responses.
- `Object` carries protocol-neutral `CacheBreakpoint`s (`System`, `Tool(i)`,
  `Message(i)`, `Part { message, part }`, `LastMessage`) and a `CacheTtl`
  (`5m`, serialized without a `ttl` field, or `1h`). Anthropic resolves each
  position against the emitted wire blocks, drops missing, non-cacheable
  (thinking/redacted) and duplicate targets, keeps the first four in the order
  given (caller priority) and warns about the rest; list durable tool/system
  anchors before a newest-turn breakpoint if those should survive the cap. A
  system-role message flattens to one block, so only part 0 addresses it; a
  later part is non-cacheable. OpenAI has no explicit breakpoint or TTL field,
  so it sends the key and warns `Unsupported` for what it cannot express; a
  five-minute TTL is OpenAI's default and is not warned.

The encoder sends the stable Messages API version `2023-06-01`. The
prompt-caching documentation checked 2026-11-19 shows `ttl` with no beta
header, so the encoder emits `ttl` without `anthropic-beta`; a test pins the
one-hour body and the absent header. A live Anthropic acceptance run for the
one-hour TTL is not part of the suite. The OpenAI key is a short FNV-1a hash of
the version, protocol, model, system text and tool names/descriptions/schemas;
it is stable across replays and identical prefixes, and excludes the provider
because protocol modules do not know it.

Warnings are typed on `EncodedRequest::cache_warnings` and rendered into the
existing warning stream by `Client::stream`, so a dropped breakpoint appears as
`LlmEvent::Warning` before the first provider frame and reaches
`StepOutcome::warnings` and the CLI. A pre-stream HTTP error opens no stream, so
there is no warning channel for it.

## Route anatomy

```rust
Route {
    provider, model,
    endpoint,            // base URL + protocol path
    protocol: Arc<dyn Protocol>,
    auth: Auth,          // none | bearer | optional bearer | header | query
    framing: Framing,    // Sse
    headers,             // static per route
}
```

Auth is a reference, not a value: `CredentialRef { env, store_key }` is resolved
per request through `SecretResolver`. Resolution order is environment first,
then the store. `EnvResolver` implements environment-only lookup and is still
what `Client::new()` uses; `StoreResolver` layers the persistent store behind
it, and `keel chat` passes that resolver to `Client::with_resolver`.

The store is JSON at the platform config path (`KEEL_CREDENTIALS` overrides it):
`{ "version": 1, "credentials": { "<key>": "<secret>" } }`. Keys are account
labels; `build_route` uses the provider id from the catalog. On Unix the file is
written 0600 and a directory the store creates is 0700; an existing override
parent is left alone, because changing an arbitrary user-chosen path would be
harmful. A symlink at the credential path is refused (the read opens with
`O_NOFOLLOW` after a metadata pre-check), while a symlinked config directory is
followed and never chmodded, so a dotfiles-managed or synced directory works.
The reader opens with `O_NONBLOCK` and requires a regular file, so a directory,
FIFO or other special file is refused instead of blocking. It does not check the
mode of a file it did not write; 0600/0700 are guarantees for writes. Writes go
to a private temporary file in the same directory, are fsynced, and are renamed
over the target, so a reader sees either the old file or the new one, never a
partial one. Temp creation retries on name collision left by an earlier process,
and the temp chmod is skipped when the create mode is already 0600 (some
filesystems reject `fchmod`).

`StoreResolver` keeps a snapshot. Resolve checks env, then the snapshot; a hit
does not read the file. Empty or whitespace-only values count as absent, like an
empty environment variable. A miss takes a refresh lock, reloads the file once,
and stores the new snapshot with a new generation. Concurrent callers that
observed the older generation use that reload's result instead of reading again.
A failed reload leaves the old snapshot in place and is surfaced as a typed auth
error; optional bearer auth is the exception and treats a broken store as
absent, keeping anonymous local servers working. There is no TTL: OAuth refresh
is M11, so a cached key lives until the process ends or `reload()` is called.
Errors and `Debug` include keys, variable names and paths, never values.

Concurrent `keel auth set` processes have no cross-process lock: each write is
atomic, but the read-modify-write can lose one of two simultaneous updates
(last-writer-wins). Windows uses the per-user `%APPDATA%` ACL; CI covers Linux
and macOS only.

## Transcript repair

`repair_transcript` is pure and returns notes, not mutations. Order matters:

1. Drop failed assistant turns (`error` set, or stop reason `Aborted`/`Error`).
2. Drop reasoning that is not portable to the target: unsigned reasoning from
   another origin, and redacted reasoning from another origin.
3. Downgrade images the target cannot see to `[image omitted: <mime>]`.
4. Drop messages with nothing replayable left.
5. Hoist system messages that landed between a tool call and its result, since
   they would break the call/result adjacency the wire requires.
6. Normalize tool call ids into the target's charset and length, rewriting the
   matching results through the same map.
7. Walk forward with a set of open calls: results that answer no open call are
   dropped, calls that never got a result get a synthesized error result placed
   first in the next user turn.

The raw history is never rewritten. Repair produces the view for one request, so
failed turns remain inspectable.

## Retry boundary

Retry happens at two layers, and neither is silent.

**Pre-stream.** `Client::stream_with_retry` retries a connect failure, a 429, a
5xx or `overloaded` while opening the stream, honouring `retry-after` up to the
policy's cap. Nothing has reached the caller yet, so replay is free.

**Turn boundary.** Once output has arrived, replay is a session decision.
`Harness` asks `RetryPolicy::decide` about the typed `StepOutcome::error`; a
retryable kind re-runs the turn with no new user input. The failed attempt stays
in history — repair drops it from the next request, which is what makes the
replay safe — and the loop emits `RunEvent::Retrying` before the backoff so the
caller can say so. A cancelled run does not wait the backoff out.

A failure that cannot succeed by repeating the same request is not retried:
invalid request, auth, quota, decode, abort. A context overflow is the special
case: the session compacts once and replays the request once (see
[Compaction](#compaction)); a second overflow is a normal failure. Model
fallback is not implemented (O-004 remainder).

## Catalog

The catalog has three layers, merged by provider and model id. Precedence,
highest first: a runtime overlay, the checked-in overlay, the generated
snapshot, and struct defaults.

- `catalog/models.dev.toml` is a committed snapshot of model facts generated
  from <https://models.dev>: ids, names, context and output limits, and
  tool/image/reasoning capabilities. It is keyed by models.dev ids, and an
  overlay `models_dev_id` maps a keel id onto one. It is written only by
  `keel-catalog-gen`; provenance and the MIT license are in
  `catalog/PROVENANCE.md`.
- `catalog/overlay.toml` is the checked-in keel overlay. It carries provider
  `base_url`, `base_url_env`, auth and notes, plus per-model `protocol`,
  optional `path`, `compat` overrides, and pins for facts that must not move
  when the snapshot is refreshed. A value here wins over the snapshot; a field
  omitted here comes from the snapshot. Models.dev does not carry a protocol,
  so every model names one here.
- The optional runtime overlay (`KEEL_CATALOG`, else
  `$XDG_CONFIG_HOME/keel/catalog.toml`, `~/.config/keel/catalog.toml` or
  `%APPDATA%\keel\catalog.toml`) is parsed as a patch. Every field except an
  entry's `id` is optional when patching an existing entry, so it can override
  one field without restating the entry. It can also append a provider or
  model; existing order is preserved and new entries append in file order. A new
  provider needs a name, a base URL (or `base_url_env`) and at least one model;
  a new model needs a name and `protocol`. `auth` is replaced as a whole, while
  `capabilities` and `compat` merge field by field. There is no remove operation
  and no way to clear `base_url_env`, `notes` or a model's `path`. A provider
  listed twice in one patch applies in order and prints a warning.

`Catalog::builtin` embeds the two committed files with `include_str!` and merges
them once. It never reads the environment or the filesystem, so a clean
checkout builds and tests offline and deterministically. `Catalog::load` adds
the runtime layer; the CLI uses it for `providers`, `models`, `route`, `chat` and
`repair`. `keel-evals` keeps using `Catalog::builtin()`, so a runtime overlay
never affects cassette replay. A malformed runtime file, an unknown key, a
missing required field on a new entry, or any validation issue after the merge
produces one warning and the embedded catalog is used instead, so a bad overlay
never fails a run.

The generator is an explicit developer action, never a build step:

```sh
cargo run -p keel-catalog-gen -- --from /tmp/models-dev.json
cargo run -p keel-catalog-gen
```

It reads `overlay.toml` to learn which providers and model ids keel routes,
renders a snapshot, and writes through a temporary file in the destination
directory before renaming it into place. Any fetch, JSON parse, schema,
pinned-fact or provenance failure leaves the previous file byte-for-byte
untouched: that committed file is the last-known-good snapshot every build
embeds. A mapped model that models.dev no longer lists is reported and omitted;
regeneration refuses to write unless its overlay entry pins `name`,
`context_window`, `max_output_tokens` and `capabilities` (an explicit `0` counts
as a pinned window), because an unpinned name, window or capabilities value
would fail or silently change the route. The snapshot records today's UTC date
and `PROVENANCE.md` must record the same `Fetched:` date. Skipped providers and
models are printed on stderr.

`keel models` and `keel providers` keep their columns and row order; their
footer and notes text now name the snapshot and overlay, and both are pinned by
exact-output tests.

Each final model carries `protocol` (the wire it speaks, not the provider's
default), an optional `path` override, context and output limits (carried onto
the `Route` as `context_window` and `max_output_tokens`; zero `context_window`
disables compaction for a custom or uncatalogued route), capabilities (tools,
images, reasoning, cache), and `compat` overrides. Providers carry `base_url`,
an optional `base_url_env`, auth shape, and notes. Notes are documentation for
humans and are never used for routing.

## Testing

Six layers. The default suite is offline; live recording is ignored and
opt-in:

1. **Scripted wire** (`keel-testkit`). A real socket serving a queue of turns.
   Chunks can be gated until the test releases them, a response can stall after
   its headers, and every request is recorded with its body. This is where
   cancellation, retry, chunk splitting and truncation are tested.
2. **Golden requests** (`keel-llm-protocol/tests/requests.rs`). One canonical
   request per policy and protocol (`Auto`, `None`, `Object`), encoded and
   compared byte-for-byte against a checked-in body. `UPDATE_GOLDEN=1` rewrites
   the files. A diff here means the request changed, which is the regression
   that matters most and the one a decode test cannot see.
3. **Recorded responses** (`keel-llm-route/tests/recorded.rs`). Raw SSE bodies
   captured from real providers and borrowed from opencode's MIT-licensed
   recordings, replayed through our framing and decoder. They are decoder
   conformance fixtures, not first-party keel captures; provenance and the MIT
   license are in `fixtures/recorded/PROVENANCE.md`. The
   Gemini recording is what exposed the missing-`index` merge; the Responses
   recordings drive its event-sourced decoder, including encrypted reasoning.
4. **Cassettes** (`keel-llm-route/tests/cassettes.rs`). A whole exchange
   replayed through `Client`, the route, the framing and the decoder: the
   request keel encoded, the status, the headers and the raw body. The request
   half comes from the encoder (`UPDATE_CASSETTE=1` rewrites it), so a changed
   request is a miss instead of a decode test that got weaker without anyone
   noticing. The player consumes interactions in recorded order, once each; a
   miss is HTTP 422 (non-retryable) and an unconsumed interaction fails the
   replay. `fixtures/cassettes/scenarios.toml` fails when a cassette on disk is
   not covered. Five entries are first-party whole exchanges recorded live from
   NeuralWatt (M8); the other two pair an encoder-produced request with a
   borrowed decoder fixture, so they are replay tests rather than captures.
5. **Behavioral evals** (`keel-evals`). Twelve coding tasks over a Python
   fixture. Replay loads one multi-turn cassette per task and drives `Harness`
   through the real client, route and decoder while executing tool calls against
   a temp copy; the task ends with one mechanical command that exits 0, never an
   LLM judge. A check has a 30-second deadline and bounded captured output: a
   hanging check fails the task, while chatty output is drained and truncated
   instead of stalling the suite. Live
   mode is `#[ignore]`d, opt-in, refused under `CI`, records through the
   loopback recorder with `deepseek-v4.1-flash`, and stops before a request
   would cross the $10 cap. A freshly recorded cassette replays from a temp file
   before it replaces the baseline. Cassettes are listed in
   `crates/keel-evals/fixtures/cassettes/scenarios.toml`; the committed baseline
   report is `crates/keel-evals/reports/live.md`. A pass requires a clean stop
   (`EndTurn` or `MaxTurns`) and a passing check; `Failed` and `Cancelled` fail,
   and never replace a committed cassette. Replay reports per task, not a
   `RunReport`; only live recording writes the aggregate report. The committed
   cassettes and report are the M3 run: 12/12, $0.039297. The twelve committed
   cassettes hold live NeuralWatt responses, so they are first-party captures
   too (M8). The shell guard is a
   token-based determinism aid, not a sandbox.
6. **MCP fixture and HTTP server** (`keel-mcp-testkit/tests/`). The
   `mcp_fixture` binary is a real stdio server, so the client and `Harness`
   tests exercise process startup, the handshake, pagination, progress, ping,
   unsupported server requests, CRLF and blank lines, string and out-of-order
   ids, malformed, non-UTF-8 and oversized lines, child reaping, mid-call
   crashes, a timeout followed by a usable call, cancellation, metadata limits
   including a zero-argument schema, and truncation without a network. The
   Streamable HTTP tests run against `FauxProvider` and cover the JSON and SSE
   response paths, session and protocol headers, configured headers, a
   server-initiated `-32601` answer, batch rejection, 1 MiB body/line/frame
   caps, 202/401/403/404/429/5xx mapping, a reflected header in a JSON-RPC
   error, a 202 answer to a request, no redirect following, cancellation,
   `DELETE` on shutdown, overlapping parallel-safe calls, and the unsafe
   barrier. A separate set
   asserts a missing or crashing server is a warning with no tools; a denied
   call uses a marker file (stdio) or a request count (HTTP) to prove it never
   reached the server. CLI tests prove config is not started or contacted
   without `--mcp`, is started with it, and reports parse warnings.

Credentials have their own tests in `keel-llm-route/tests/credentials.rs`, with
the CLI surface in `keel-cli/tests/auth.rs`: store round-trip and version checks,
0600/0700 modes on Unix, symlinked file refusal and symlinked-parent follow, FIFO
and directory refusal, empty-value fallback, optional-bearer behavior with a
broken or newer store, an atomic file replaced under a concurrent reader, env
precedence and fallback, missing-source text, `Debug`/error/CLI output checks
that no value leaks, one coalesced reload for sixteen concurrent resolves, two
concurrent `Client` requests that share one store read, a real `Client` request
authenticated only from the store, and query-string redaction in transport
errors with their causes. The CLI tests run the binary for `auth set/list/remove`,
check key validation before the prompt, reject extra arguments, and run `chat`
turns whose key comes only from the store and where the environment wins.

Workspace tests include a smoke test that swaps a directory for an outside
symlink while `open_existing` runs; it checks that any successful read is the
inside file, but the real race window is sub-microsecond, so it is not a
regression test for the old resolver. Internal-symlink refusal and `mkdirat`
parent creation are pinned deterministically. Atomic replacement is covered by
a failed write and a failed edit leaving the old bytes, mode preservation on
both paths, a new file's umask mode, no `.keel-*` temp left after success or
failure, a read-only file not being replaced, a FIFO and a Unix socket being
refused without blocking or replacement, special mode bits being dropped, an
`edit` that does not match leaving the file byte-identical, and a symlink
swapped in before the rename being replaced rather than followed. Harness tests prove concurrency
with a barrier that two tools on one turn can only pass together, check that a
`safe, unsafe, safe` turn never overlaps, compare a parallel run's results with
the same tools marked unsafe, run ten safe calls across the eight-call chunk
boundary, compare cancellation before a batch with the sequential loop, and
check that cancellation inside a batch still leaves one result per call.

Catalog tests pin the two committed files to what is embedded, the eight
providers and all 21 model ids, protocols, endpoint paths, windows,
capabilities and deprecated status (`keel-llm-catalog/tests/providers.rs`),
generated-vs-overlay-vs-runtime precedence, a runtime overlay that appends a
provider and model, an empty overlay, a duplicate provider warning, an
unreadable file, malformed input and unknown keys falling back with a warning,
and unknown lookups. The committed snapshot's `fetched_at` is asserted equal to
the `Fetched:` line in `PROVENANCE.md`. The generator tests use a fixed JSON
fixture to prove that a parse, schema, dropped model missing a pinned fact or
provenance-date failure leaves the previous snapshot untouched, warnings are
forwarded before a later error, a successful run leaves no temporary file,
aliases keep the source id, and a skipped model is reported. CLI tests pin the exact `keel models` and
`keel providers` output and exercise a runtime overlay through both `models` and
`route`.

Plus the existing unit, repair, HTTP, session, tool and harness tests. Cassettes
are produced mechanically, never typed by hand: the recorder is a loopback proxy
that captures a live call, redacts credentials by header name, query name and
body pattern (bearer tokens, `sk-` keys, `AIza` keys, PEM blocks), and writes
the format the player reads. It captured five real exchanges from
NeuralWatt — chat text, a chat tool call, Anthropic Messages with a signed
thinking block, and two Responses turns — so those cassettes hold both halves as
the provider saw them, across all three protocols. The other two cassettes are
the second mechanical path: the request half is the encoder's output and the
response half is a borrowed decoder fixture; they are conformance/replay tests,
not first-party captures. Recording a non-loopback origin requires
`KEEL_CASSETTE_RECORD=1`, and `CI` refuses it outright.

The live captures corrected two assumptions. A chat tool turn arrives with
`delta.reasoning` in the same stream even when no reasoning effort, which the
decoder already maps alongside `reasoning_content`. And NeuralWatt answers
`include: ["reasoning.encrypted_content"]` with a summary and an item id but no
encrypted blob, so that turn decodes with no reasoning signature: the stateless
replay in D-009 depends on a provider that actually returns one.

M8 was re-scoped to these NeuralWatt captures: they cover all three implemented
protocols, while OpenAI- and Anthropic-endpoint-specific captures are explicitly
out of scope (O-008).

## Next steps

Ordered per [`docs/BLUEPRINT.md`](BLUEPRINT.md):

1. Google/Antigravity protocol module, OAuth PKCE, discovery, schema sanitizer,
   and Google captures (D-005, O-012, M11).
