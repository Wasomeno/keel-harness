# keel

Provider seam and session loop for an LLM agent harness, written in Rust.

**Status:** M0 through M10, M12 wave 0, and all of M12 wave 1 (M12.4-M12.7) are done; wave 2 (M12.8-M12.10) and the M12.11 scorecard are next. OpenAI Chat Completions, OpenAI
Responses and Anthropic Messages work end to end, and a session runs a full tool
loop against a workspace: model turn, tool calls, tool results, next turn. The
M1 behavioral eval suite replays twelve real coding tasks offline and can record
them live. M2 gives every built-in tool a description that says when to use it,
which tool to prefer, what it returns and how failures read, and makes every
failure result name the value and the next step. M3 ships keel's own default
system prompt; `--system TEXT` appends to it. M4 compacts a session before a
request would exceed the model's catalog context window, and once after a
provider context-overflow error. M5 adds a persistent, account-keyed credential
store with `keel auth` and single-flight reload. M6 opens file tools through
descriptor-relative paths with `O_NOFOLLOW`, closing the symlink swap window, and
runs `parallel_safe` tools concurrently with results kept in call order. M7 adds
opt-in MCP tools as ordinary `Tool`s, configured per workspace; the same client
now speaks Streamable HTTP as well as stdio. M8 was
re-scoped from first-party OpenAI/Anthropic-endpoint captures, which needed
vendor keys we do not have, to first-party NeuralWatt captures covering all
three implemented protocols; the vendor-endpoint-specific captures are dropped
from scope. M9 replaces the hand-written seed with a committed models.dev
snapshot, a checked-in keel overlay, and an optional runtime overlay; builds
stay offline. M10 adds a full cache policy (`Auto` / `None` / `Object`) with
explicit Anthropic breakpoints and TTL plus a deterministic OpenAI prompt cache
key; every dropped breakpoint is a typed warning. `keel chat --cache none` (or
`KEEL_CACHE=none`) opts out for strict OpenAI-compatible endpoints.
Google/Antigravity is catalogued but not implemented. M12.6 (merged as
`ef4ca13` in [PR #34](https://github.com/Wasomeno/keel-harness/pull/34)) adds
patch finalization for benchmark runs: `keel chat --require-changes` fails a one-shot
run that leaves the workspace clean, and the SWE-bench runner retries once,
gates added lines with `git diff --check`, reports test-file edits, and keeps the
base-seeded temporary-index diff. M12.4 (merged as `911427a` in
[PR #35](https://github.com/Wasomeno/keel-harness/pull/35)) adds an opt-in Unix
`terminal` tool that keeps `cd`, exported variables and services across calls.
M12.5 (merged as `7d67a70` in
[PR #37](https://github.com/Wasomeno/keel-harness/pull/37)) adds the matching
opt-in Unix `jobs` tool: start a command in its own process group, poll its
bounded output from absolute offsets, wait with a timeout, and kill it, with
every job cleaned up at session end. M12.7 (merged as `5e461aa` in
[PR #38](https://github.com/Wasomeno/keel-harness/pull/38)) adds test-command
discovery for the SWE-bench adapter, an opt-in `--test-loop` that tracks visible
test runs and injects one pre-final verification message, and test-run metrics
for the trace reporter.

## Layout

| crate | role | may depend on |
|---|---|---|
| `keel-llm-types` | canonical messages, parts, provenance, events, errors, usage, cache policy, `repair_transcript` | nothing of keel's |
| `keel-llm-protocol` | one module per wire protocol: body lowering and stream state machines | types |
| `keel-llm-route` | `Route = Protocol + Endpoint + Auth + Framing`, HTTP transport, SSE, retry classification | types, protocol |
| `keel-llm-catalog` | providers and per-model protocol assignments; generated models.dev snapshot plus a checked-in overlay | types |
| `keel-catalog-gen` | dev-only: regenerates the committed models.dev snapshot | catalog |
| `keel-tools` | tool trait and registry, permission gate, workspace sandbox, built-in tools | types |
| `keel-mcp` | MCP stdio and Streamable HTTP client, server config, and `Tool` adapters | tools, types |
| `keel-mcp-testkit` | dev-only scripted MCP servers (stdio fixture and `FauxProvider`) and end-to-end MCP tests | mcp, core, testkit |
| `keel-testkit` | dev-only scripted HTTP/SSE server, cassette player and recorder | nothing of keel's |
| `keel-core` | route resolution, event assembly, session and agent loop, session log | all of the above |
| `keel-evals` | dev-only behavioral evals: Python fixture, 12 tasks, offline cassette replay, budget-capped live recording | core, catalog, route, tools, testkit, types |
| `keel-cli` | the `keel` binary | core, catalog, route, mcp, tools, types |

The dependency rule is one-way. No provider type reaches the session loop, and
no HTTP concern reaches the types crate.

## Try it

```sh
cargo test --workspace

cargo run --bin keel -- providers
cargo run --bin keel -- models
cargo run --bin keel -- tools
cargo run --bin keel -- route anthropic/claude-sonnet-4-5

OPENAI_API_KEY=... cargo run --bin keel -- chat openai/gpt-4.1 \
  --workspace . --prompt "read Cargo.toml and summarize it"
```

`keel chat` streams to stdout and drives the tool loop. Tools run inside the
workspace (default: the current directory), and every call asks for permission
unless `--yes` is passed. `--max-turns` caps the loop (default 24).
`--require-changes` is an opt-in guard for one-shot `--prompt` runs: after the
turn, keel runs `git status --short` in the workspace. If it prints nothing, the
command exits with code 3 and the message
`--require-changes: no changes in the workspace; retry once with a prompt that
asks for the required edit`, so a benchmark adapter can tell an empty run apart
from a provider or environment error. Untracked files count; gitignored files do
not. Using the flag without `--prompt`, or in a non-git workspace, is an error,
not an empty run.

`--test-loop` is an opt-in benchmark mode for one-shot runs. It recognises
`bash` and `terminal` commands that invoke a test runner (pytest, unittest,
cargo test, make test, npm test, tox and similar), records the latest failing
test names and a short excerpt in the session log, and keeps that status across
compaction. At the first `EndTurn` it injects one message that asks what
changed, what ran and what is still unverified; if no visible test ran, the
message asks for the narrowest test, and if the last one failed it asks for a
fix or an explanation. The message is sent once per run, so it cannot loop, and
it is off unless the flag is passed. `keel chat` prints one `[tests]` line for
the recorded status and one `[verify]` line when the message is injected.

On a terminal it paints a small activity line on stderr after a short quiet spell
(for example `- waiting for model (3s)` or `- running bash (1s)`) and clears it
before any real output; the line never appears when stderr is redirected, so
scripts and tests see exactly the same bytes as before. When MCP is requested
(`--mcp` or `KEEL_MCP=1`) and `KEEL_MCP_DEBUG` is set, it also stays off,
because those debug lines write directly to stderr. A closed stdout stops the
run with a write failure instead of spending tokens to the end of the turn.
`--cache
auto|none` (default `auto`, or `KEEL_CACHE` when the flag is absent) controls
prompt caching; use `none` for a strict OpenAI-compatible endpoint that rejects
`prompt_cache_key`. The resolved policy is stored in the session header, so a
resume keeps it unless a new flag or `KEEL_CACHE` value overrides it. The
command is interactive when `--prompt` is omitted. On Unix the file tools are
descriptor-relative and refuse symlinks; on Windows they use a canonicalizing
fallback that follows symlinks. `bash` only gets the workspace as its working
directory, so it is not OS-confined. On Windows it prefers Git Bash (`KEEL_BASH`,
a standard Git for Windows install, or the `git.exe` on `PATH`); the last
resort is plain `bash`, which is the WSL launcher in `System32` when Git for
Windows is not installed.

Every session starts with keel's default system prompt. `--system TEXT` appends
your text after it rather than replacing it, separated by a horizontal rule. The
prompt lives in `crates/keel-core/src/prompts/default_system.md` and is embedded
at compile time; it is the same for every model and protocol.

A turn that fails after output has arrived is replayed when the failure is
retryable (rate limit, overloaded, 5xx, network, timeout), up to
`--max-attempts` (default 3). The failed attempt stays in the log, repair keeps
it off the wire, and the retry sends no second copy of the user message.

A provider that sends response headers and then stops sending body bytes is
bounded separately: a successful streaming response is aborted when the gap
between body chunks reaches 300 seconds by default
(`KEEL_STREAM_IDLE_TIMEOUT_SECS`; `0` disables it). Expiry surfaces a
`timeout: no data from the provider for 300s` error and ends the stream. This is
not a total request deadline, so a long generation that keeps sending text,
reasoning or tool-argument deltas keeps going. The stream also ends as soon as
the decoder queues a terminal event: OpenAI Responses emits `Done` at
`response.completed`, so an open body after a complete turn is not turned into a
timeout; Chat and Anthropic emit `Done` when the body closes, so an open body
after their end marker is still bounded by the idle timer. A non-2xx error body
is read under a 30 second cap, or the idle bound when that is smaller, so a
stalled 429/5xx body cannot block the pre-stream retry. The transport retries
only before a successful response stream opens; once that stream is handed to
the caller it never retries. The turn-boundary replay above still applies to a
reported failure.

`keel repair` is a debugging tool: it applies the same pure repair pass the
session uses, and prints the repaired transcript plus every change it made.

```sh
cargo run --bin keel -- repair messages.json commandcode/claude-sonnet-4-5
```

### Sessions

Every `keel chat` run is logged as JSONL under the keel data directory, in a
directory named after the project, so nothing is written into the repository.
The first line is the header (provider, model, protocol, workspace, system
prompt); every line after it is one canonical message. A new session records
the effective prompt exactly as it sends it, default included. On resume the
header keeps the creation-time prompt; `--system` applies to that run only and
is not written back.

```sh
cargo run --bin keel -- sessions                    # ids, model, message count
cargo run --bin keel -- sessions 1763550000-a1b2    # the transcript
cargo run --bin keel -- chat --resume last --prompt "keep going"
```

`--resume last` takes the newest session for the project; an id resumes that
one. A resumed session keeps the model it was recorded with, because repair
would have to drop foreign reasoning to switch; passing a different target warns
and keeps the stored one. Resume also keeps the prompt recorded in the header
and never prepends the default a second time; `--system` on resume appends to
that stored prompt. A session recorded before the default prompt existed has
`system: None` and keeps running without one, so an injected default cannot
change an existing transcript; on such a session `--system` applies alone.

A version-2 header also records `max_output_tokens` and `temperature`, and
resume applies them unless a command-line flag overrides them, so the resumed
request matches the live one.

### Compaction

A session compacts before a request when the last provider-reported input plus
output tokens plus that request's output budget (the catalog output limit, or
`--max-tokens`) reach the model's catalog `context_window`. The reported rule
does not include the pending input; when no usage has been reported yet, the
estimate branch does. A very large new paste therefore relies on the provider's
overflow path. It also compacts once and retries once when the provider reports
a context-overflow error
(including realistic `prompt is too long` / `context_length_exceeded` wording
and in-stream error frames); a second overflow is a normal failure. A route with
no catalog window (`context_window = 0`, such as `custom` or a hand-built route)
disables compaction, and `keel chat` prints one warning at startup.

The summarizer uses the session's own route and model and is offered no tools.
It receives the committed history plus a fixed checkpoint prompt and returns a
handoff summary. The request is checked against the window with a conservative
estimate before it is sent; if even a minimal summary cannot fit, keel fails
with `cannot compact: ...` instead of sending it. A summary cut off at the
output budget fails with `cannot compact: the summary was truncated` rather
than being accepted.

The replacement view is: the system prompt, the newest plain user message
(always kept verbatim, even if it alone exceeds the budget), older plain user
messages newest first while the kept total stays strictly under the budget
(then in their original order), and one user message starting with
`[keel handoff summary]`. No assistant turn, tool call or tool result is kept,
so a call and its result always leave together and `repair_transcript`
synthesizes nothing. The view is then checked as the next request against the
system prompt, tools, pending input and output budget, with the same inclusive
boundary as the trigger. If it does not fit, compaction fails and names the
actual cause (the output budget, pending input, summary/system/tools, or the
kept user message) rather than dropping the request. One
`[compact] threshold|overflow: M -> N messages (K kept user), ~A -> ~B tokens`
line goes to stderr per compaction.

The JSONL log appends one record and never rewrites earlier bytes:

```json
{"type":"compaction","at":1763550009,"reason":"threshold","summary":{...},"messages":[...]}
```

`messages` is the replacement view, and `summary` is the same message. Resume
resolves the latest compaction record plus the messages after it, so a compacted
session continues from exactly the state the live session held. Log version 2
adds this record and the header config fields; version-1 logs still load. A
successful run reports the summary request's usage separately as
`RunOutcome::compaction_usage`. A summary that is truncated, empty, or followed
by a view that does not fit leaves its usage on the session instead, because
that compaction returns `Err`; the caller drains it with
`take_compaction_usage`. The usage is printed on the final usage line when
nonzero and charged by the eval budget; it is not folded into the main usage
the trigger reads. If a chat run fails before that final line, a nonzero paid
summary usage is printed on stderr before the error. The token estimate used
for the fit checks and for the no-usage fallback is one token per three bytes
plus fixed per-message and per-part overheads and a flat 2,000-token image
cost, documented in `docs/architecture.md`.

## Credentials

Auth is a reference, not a value: `CredentialRef { env, store_key }` is resolved
per request. Resolution order is **environment first, then the store**. The
first non-empty environment variable in the reference wins; when none is set,
the store key is looked up. `EnvResolver` and `Client::new()` keep their
environment-only behavior, and existing callers keep working. `StoreResolver`
adds the store behind the same `SecretResolver` trait; `keel chat` uses it, and
embedders can pass it to `Client::with_resolver`.

`keel_core::build_route` sets every catalog route's store key to the provider id
(`openai`, `anthropic`, `neuralwatt`, ...). Storing `openai` therefore supplies
the fallback for `OPENAI_API_KEY`.

```sh
cargo run --bin keel -- auth set openai   # prompts without echo
cargo run --bin keel -- auth list         # keys only, never values
cargo run --bin keel -- auth remove openai
```

`auth set` reads the secret from stdin, or prompts with echo disabled when
stdin is a terminal. It never accepts the secret as an argument, so the value
does not land in shell history or process listings. For scripted input, redirect
stdin from a file or descriptor (for example `< /run/secrets/openai`); do not
build a pipeline whose helper puts the value in its own argv. `auth list` prints
sorted keys only.

The store is one JSON file:

```json
{
  "version": 1,
  "credentials": {
    "openai": "..."
  }
}
```

It lives at the platform config path:

| platform | path |
|---|---|
| macOS | `~/Library/Application Support/keel/credentials.json` |
| Linux | `$XDG_CONFIG_HOME/keel/credentials.json`, else `~/.config/keel/credentials.json` |
| Windows | `%APPDATA%\keel\credentials.json` |

`KEEL_CREDENTIALS` overrides the full file path. On Unix the file is written
0600, and a directory the store creates is 0700. An existing directory named by
`KEEL_CREDENTIALS` keeps its mode, because changing an arbitrary user-chosen
path (for example `/tmp`) would be harmful. A symlink at the credential path is
refused (`O_NOFOLLOW` on Unix), but a symlinked default config directory is
followed and never chmodded, so a dotfiles-managed or synced directory works.
Only a regular file is read: a directory, FIFO or other special file at the path
is refused. The reader does not check the mode of a file it did not write; 0600
and 0700 are guarantees made when keel writes. The file is replaced atomically:
write a private temporary file in the same directory, flush it, then rename it
over the target. Errors and `Debug` output name the key, variable or path, never
a value; the CLI prints keys and paths only.

The resolver keeps an in-memory snapshot. A resolve checks the environment,
then the snapshot; a hit does not touch the file. Empty or whitespace-only
stored values count as missing, the same as an empty environment variable. A
miss reloads the file once, and concurrent callers that observed the same
snapshot share that single reload and its result. A failed reload leaves the
previous snapshot intact and is reported as an auth error rather than as a
missing credential; optional bearer auth (`custom`) is the exception: it treats
a broken store as absent and sends an anonymous request. There is no time-based
expiry in this slice: OAuth token refresh belongs to M11. A cached key stays
cached for the process lifetime, and a key added to the file is picked up by the
next resolve that misses; `StoreResolver::reload()` forces a fresh read.

Two `keel auth set` processes have no cross-process lock: each write is atomic,
but the read-modify-write can lose one of two simultaneous updates
(last-writer-wins). That is acceptable for a single-user CLI. Windows uses the
per-user `%APPDATA%` ACL for the directory and file. CI runs on Windows and a
Windows-only auth test covers the `%APPDATA%` default path, but it does not
assert the resulting ACL; treat Windows ACL specifics as untested.

## Configuration

| variable | meaning |
|---|---|
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENCODE_API_KEY`, `COMMANDCODE_API_KEY`, `XAI_API_KEY`, `NEURALWATT_API_KEY` | provider keys |
| `ANTHROPIC_AUTH_TOKEN` | second accepted Anthropic source |
| `KEEL_<PROVIDER>_BASE_URL` | overrides the catalog endpoint |
| `KEEL_CUSTOM_BASE_URL` | required for the `custom` provider |
| `KEEL_CUSTOM_API_KEY` | optional key for `custom` |
| `KEEL_ANTIGRAVITY_ACCESS_TOKEN` | temporary access token until the OAuth flow lands |
| `KEEL_CATALOG` | full path to a runtime catalog overlay (default: `$XDG_CONFIG_HOME/keel/catalog.toml`, else `~/.config/keel/catalog.toml` or `%APPDATA%\keel\catalog.toml`) |
| `KEEL_CREDENTIALS` | full path to the credential store (default: the platform config path) |
| `KEEL_DATA_DIR` | where session logs live (default: the platform data directory) |
| `KEEL_CACHE` | `auto` (default) or `none`; `keel chat --cache` wins when both are set |
| `KEEL_STREAM_IDLE_TIMEOUT_SECS` | seconds allowed between response body chunks before the stream fails with a timeout (default `300`; `0` disables the stream bound; values above 30 years are clamped) |
| `KEEL_MCP` | `1` starts configured MCP servers, same as `--mcp` |
| `KEEL_TERMINAL` | `1` registers the persistent Unix `terminal` and `jobs` tools, same as `--terminal` |
| `KEEL_TASK_DEADLINE_MS` | wall-clock budget for a `keel chat` run; background jobs cap their per-command timeout by the time remaining |
| `KEEL_BASH` | Windows only: full path to the bash the `bash` tool runs |

Missing required credentials fail before any request is sent and name the
environment variable and/or store key, never a value.

### MCP servers

MCP servers are declared per workspace in `.keel/mcp.toml`. A server is exactly
one transport: a stdio command or a Streamable HTTP URL.

```toml
[servers.github]
command = "github-mcp"
args = ["--stdio"]
enabled = true
env = ["GITHUB_TOKEN"]
parallel_safe = false
startup_timeout_ms = 10000

[servers.remote]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${MCP_TOKEN}" }
parallel_safe = true
```

`args` and `env` apply only to stdio servers; `headers` applies only to URL
servers. Setting the wrong pair is a warning and the server still loads. Header
values expand `${VAR}` once from the process environment. A missing or empty
variable, a non-token header name, a CR/LF or other non-visible byte, or a
reserved transport header (`content-type`, `accept`, `content-length`, `host`,
`mcp-session-id`, `mcp-protocol-version`) skips that server with a warning.
Only `http` and `https` URLs are accepted; userinfo and fragments are rejected.
Header values never appear in `Debug`, warnings or errors. The process
environment is the only credential source: there is no MCP OAuth flow.

MCP is **off by default**. A workspace config is data until the user opts in
with `--mcp` on `keel chat` or `keel tools`, or with `KEEL_MCP=1` for scripts.
Without the opt-in keel prints any config warnings and, for valid servers, one
line saying they are configured but not started; it lists no MCP tools and
starts or contacts nothing. `keel route --workspace DIR` names the enabled
servers without starting them.

A stdio server command runs with the user's full privileges, outside the
workspace sandbox, equivalent to `bash`. An HTTP server is contacted only after
the same opt-in and receives exactly the headers in the file. `--yes` and the
tool permission gate do not protect startup: the opt-in is the trust decision.
Only enable a workspace whose `.keel/mcp.toml` you trust. `enabled` defaults to
true and is a secondary filter for keeping a server defined but stopped after
opt-in.

Both transports send `initialize` (offering MCP `2025-06-18`, accepting
`2025-06-18` or `2024-11-05`), then `notifications/initialized`, then paginated
`tools/list` (at most 32 pages and 512 tools), and `tools/call`. The handshake
and every page share one startup budget; `startup_timeout_ms` defaults to 10000
and is capped at 60000.

**stdio.** Each server is a newline-delimited JSON-RPC 2.0 process over
stdin/stdout, started in the workspace root. Blank lines and CRLF are tolerated,
progress notifications are ignored, `ping` is answered, and another
server-initiated request gets JSON-RPC `-32601`. A single line is capped at
1 MiB. A malformed, non-UTF-8 or oversized line closes that server for the
session and kills and reaps the child; later calls return the closed error.

**Streamable HTTP.** Each JSON-RPC request is one POST to the URL with
`Content-Type: application/json`, `Accept: application/json, text/event-stream`
and the configured headers. A `200 application/json` carries one message; a
`200 text/event-stream` is read as SSE `data:` frames, one JSON-RPC message per
frame. Notifications in the stream are ignored, and a server-initiated request
gets a bounded, best-effort `-32601` POST. After `initialize`, every request
carries `MCP-Protocol-Version` with the negotiated version and `Mcp-Session-Id`
when the server offered one; `initialize` carries neither. `202 Accepted` is
success for notifications. A stream that ends before the matching response, a
JSON-RPC batch, and a body, line or SSE frame over 1 MiB are protocol errors.
Non-2xx responses read a bounded body and map to typed errors: 401/403 say the
server rejected the request and to check the URL and configured headers, 404
after a session says the session expired and keel must be restarted, and 429/5xx
name the status. Calls are not retried.
Cancelling or timing out a call drops the in-flight request and sends
`notifications/cancelled` best effort; shutdown sends `DELETE` with the session
id and ignores its error. The optional GET server-to-client stream, SSE
resumability (`Last-Event-ID`) and MCP OAuth are not implemented. HTTP errors
never mention stderr, and configured header values are scrubbed from text the
server supplies, including JSON-RPC error messages and `data`.

MCP errors and `isError` tool results become ordinary tool results; they never
fail the harness.

Tools are registered as `server__tool`. Dots and colons in a remote tool name
become `_`; other invalid characters and names over 64 characters are skipped
with a warning. Built-ins and earlier MCP tools win a collision. Tool
descriptions are cut at 2 KiB. A tool is skipped if `inputSchema` is not an
object schema with `type: "object"` or is over 16 KiB; a missing `properties`
is normalized to `{}` for the provider. Only text content is flattened into the
result; images, audio, resources and links become visible
placeholders. The harness's 64 KiB truncation applies after the MCP result is
assembled. `keel tools` shows MCP tools with a `SOURCE` column and truncates
long descriptions on one line.

`env = [...]` is an allowlist plus a small baseline such as `PATH` and `HOME`.
It is hygiene, not a security boundary: the command is arbitrary, so `env` plus
`HOME` can still read anything the user can. `env_clear` also drops variables
such as `SSL_CERT_FILE` and `XDG_*`, which must be listed if the server needs
them. `args` is free-form; placing secrets there is unsafe, and the rule
against it is guidance, not enforcement. HTTP headers are sent on every request
and a server sees them in full, so scope them narrowly. A missing or crashing
server prints one warning on stderr, including a bounded stderr tail for stdio
servers, and contributes no tools. There is no restart policy: a server that
dies mid-run makes later calls error results. Dropping the last HTTP client
sends a best-effort `DELETE`; dropping the last stdio client kills the child.
MCP calls are barriers by default; set `parallel_safe = true` only for a server
that handles concurrent requests. Each call uses the harness timeout (120
seconds by default). Cancellation or timeout drops the in-flight request and
sends `notifications/cancelled`. The stdio writer task owns stdin, so a
cancelled call cannot split a JSON line; HTTP has no writer, so calls simply
overlap. A timed-out first call leaves the next call usable. The whole request
is still delivered, so a timed-out call may already have executed on the server.

### Persistent terminal and background jobs (Unix, opt-in)

`bash` starts a new shell for every call. Terminal tasks usually need the
opposite: a `cd`, an exported variable, an activated environment or a service
started in one call has to survive the next one. `--terminal` (or
`KEEL_TERMINAL=1`) registers two extra tools: `terminal`, backed by a single
interactive bash process on a pseudo-terminal, and `jobs`, a session-scoped
background job table. The default toolset stays the six built-ins; without the
flag nothing is spawned and `bash` remains one-shot.

- The shell starts in the workspace root. `cd` and `export` persist; an
  optional `workdir` argument changes the persistent directory, and
  `reset: true` starts a fresh shell before the command.
- stdout and stderr share the terminal, so a result is combined output, not
  two streams. The result starts with `exit: N` and the session's `cwd`.
- Each command is sent as a single here-doc so bash has consumed the frame
  before the command runs. That keeps a command that calls `read` from eating
  the harness's status line: it waits, hits the timeout, receives Ctrl-C, and
  the session recovers. A recovered session keeps its state; a command that
  cannot be interrupted restarts the session and says so.
- Output is bytes until a unique marker, so non-UTF-8 output becomes a lossy
  string rather than an error. Up to 4 MiB is retained, oldest bytes dropped
  first, before the harness's 64 KiB head-and-tail truncation.
- The shell is killed with its process group when the tool is dropped, on a
  normal end, an error, cancellation or a panic unwinding. Job control is off
  so ordinary children, including `&` jobs, share the shell's process group. A
  command that deliberately starts a new session can escape that group; use
  `jobs` for a server or long run that must outlive a foreground command.
- `jobs` has five actions: `start` returns `job: N`; `poll` reads output; `wait`
  blocks up to `timeout_ms`; `kill` signals the job's process group; `list`
  shows every job. A job runs `bash -lc <command>` in the workspace root or a
  `workdir`, in its own process group, so cleanup is one `killpg` and the
  harness's Ctrl-C does not reach it. Jobs do not inherit the persistent
  shell's `cd` or exports; put them in the command or pass `workdir`.
- Output is bounded: a 64 KiB head plus the newest 1 MiB. `poll` and `wait`
  return absolute byte offsets, a `next offset` for chunked reads, and the
  `head` or `tail` view on request. A read that asks for bytes older than the
  retained tail reports how many bytes it skipped instead of failing, so a
  10 MiB build log stays readable from either end without growing memory.
- The per-command timeout defaults to 10 minutes and is capped at 30 minutes or
  the run's remaining wall-clock budget, whichever is smaller. The monitor
  kills the process group even if the model never polls again. A child that
  calls `setsid` leaves the group and survives, which is the escape hatch for a
  service the task still needs.
- Dropping the tool, or the session, sends SIGTERM then SIGKILL to every job
  group and reaps the direct children. A grandchild that keeps a pipe open does
  not block `wait` or drop: status comes from the direct child, and reader
  threads are detached. The job table is capped at 64 entries.
- Unix only. Windows keeps the one-shot `bash` path; ConPTY parity and Windows
  job objects are out of scope for this slice. The persistent shell and jobs
  are policy-scoped like `bash`: they may `cd` outside the workspace, so only
  opt in for a trusted workspace.
- The Harbor Terminal-Bench adapter passes `--terminal` explicitly, so its
  model sees both `terminal` and `jobs`. The SWE-bench runner and normal chat
  runs keep the one-shot default until a benchmark comparison justifies
  changing them.

## Implemented

- Canonical `Request` / `Message` / `Part` types with provenance on every
  model-produced part.
- One pure `repair_transcript(messages, target)` pass: drop failed turns, drop
  unsigned foreign reasoning, normalize tool ids, synthesize missing tool
  results, drop orphan results, hoist system messages that split a call from its
  result, downgrade unsupported images.
- OpenAI Chat Completions: encode (system, tools, tool choice, images, tool
  results), decode text, reasoning, tool-argument deltas, usage, finish reason,
  in-stream errors.
- Anthropic Messages: encode (thinking + signatures, cache breakpoints, tool
  use/results, images), decode `message_start` / content blocks / deltas /
  `message_delta` / errors.
- OpenAI Responses: encode `instructions`, input items (messages, reasoning,
  function calls, function call outputs), tools and reasoning effort; decode the
  event-sourced stream (`response.created`, output item/text/argument events,
  `response.completed`, failures) with encrypted reasoning kept for replay.
- Cache policy (`Auto` / `None` / `Object`) on `Request` and `Session`:
  `Auto` keeps the Anthropic system-plus-last-message breakpoints and adds a
  deterministic OpenAI `prompt_cache_key`; `Object` sets explicit
  protocol-neutral positions and a `5m`/`1h` TTL, caps Anthropic at four
  breakpoints in caller priority order (list durable anchors first), and
  reports every dropped position as a typed warning that reaches the CLI as
  `LlmEvent::Warning`. The stable Messages API
  version `2023-06-01` takes `ttl` with no beta header; the prompt-caching docs
  were checked 2026-11-19. `--cache none`/`KEEL_CACHE` exists because a strict
  OpenAI-compatible endpoint may reject `prompt_cache_key`; compaction
  summaries always use no cache fields.
- Bounded JSON nesting depth (64 containers) on tool schemas, tool-call
  arguments, and incoming SSE frames across all three wire codecs.
- SSE framing that survives arbitrary chunk splits, CRLF, comments, multi-line
  data, and a missing final blank line.
- Route composition and a streaming HTTP client with a typed error taxonomy, a
  30 s connect timeout, and a per-chunk idle bound (300 s default,
  `KEEL_STREAM_IDLE_TIMEOUT_SECS`) that ends a stalled body and stops at a
  terminal event, without imposing a total request deadline; non-2xx error
  bodies are capped at 30 s so the pre-stream retry cannot hang.
- Persistent, account-keyed credential store behind `SecretResolver`, resolved
  after environment variables. `keel auth set|remove|list`, atomic 0600 writes
  in a 0700 directory on Unix, and single-flight reload shared by concurrent
  resolves.
- Narrow pre-stream retry (`Client::stream_with_retry` for connect failures,
  429, overloaded, and 5xx before the first byte) plus turn-boundary replay in
  `Harness` (`RunEvent::Retrying`, `--max-attempts`) that keeps failed attempts
  in the log while `repair_transcript` excludes them from the next wire request.
- Per-model catalog for all eight providers in scope (`openai`, `anthropic`,
  `opencode`, `commandcode`, `grok`, `antigravity`, `neuralwatt`, `custom`).
  Model facts come from a committed models.dev snapshot; endpoints, auth,
  per-model protocol and pins live in a checked-in overlay. The snapshot is
  embedded at compile time, so a build never fetches anything. `KEEL_CATALOG`
  adds an optional runtime overlay that can add providers/models or override
  fields without a rebuild; a malformed or unknown-key file warns and is
  ignored.
- Session loop with streaming callbacks, partial output on failure, history
  committed only after the stream opens, append-only per-project JSONL logs,
  `keel sessions`, and `keel chat --resume last|<id>`.
- Session compaction at the catalog context window and on a provider
  `ContextOverflow`: a no-tools checkpoint summary on the session's own route,
  a view of recent user messages plus that summary, one `[compact]` status line
  on stderr, and a version-2 append-only `compaction` log record that resume
  resolves. Assistant turns and tool traffic never cross the cut.
- Tool registry with `read`, `write`, `edit`, `bash`, `glob` and
  ripgrep-backed `search`. `write` and `edit` replace files atomically and
  document that hard links keep the old contents. Every description says when
  to use the tool, which tool to prefer (`search` vs `glob` vs `bash`), what it
  returns, and what a failure looks like.
- Opt-in persistent terminal (`terminal`, Unix only): one interactive bash on
  a pseudo-terminal keeps `cd`, exported variables and started services across
  calls. `--terminal`/`KEEL_TERMINAL=1` adds it; the default six-tool set and
  Windows stay one-shot. Ctrl-C interrupts a cancelled or timed-out command,
  non-UTF-8 output is lossy, and the process group is cleaned up on drop.
- Opt-in background jobs (`jobs`, Unix only, registered with `--terminal`):
  `start` a command in its own process group, `poll` bounded output from
  absolute offsets with head and tail views, `wait` with a timeout, `kill` the
  group, and `list` the session's jobs. Per-command timeouts are capped by the
  run deadline; session drop kills every remaining group and reaps it.
- Actionable tool failures: the result names the value that caused the error
  and the next step. Zero or multiple `edit` matches point at `read` or
  `replace_all`; an escaped path names the workspace root; a missing path points
  at `glob` or `write`; malformed globs and regexes name the pattern and the
  syntax to use; a non-zero `bash` exit keeps the code and output and adds a
  retry hint; a timeout names the command and the cap; write failures name the
  path or parent directory.
- Channel permission gate: allow-all, deny-all, or ask the host with a request
  the UI can render. A missing or dropped host is a denial.
- Workspace sandbox: normalized paths under one root; on Unix the file tools
  open component by component from an open root descriptor with `O_NOFOLLOW`,
  so a symlink swapped in after the check cannot escape. Symlinks are refused,
  not followed; the error names the offending path component and points at a
  quoted `bash cat` command when the target is trusted. `bash` is cwd-scoped
  rather than OS-confined; `glob` and `search` skip symlinks.
- Atomic file replacement: `write` and `edit` fill a temporary file in the
  target's directory, fsync it, rename it over the target and fsync the
  directory best-effort. A failed write leaves the old file intact, an existing file keeps
  its ordinary permission bits (setuid, setgid and sticky are dropped), and a
  replaced file gets a new inode, so hard links keep the old contents.
- Parallel tool execution: consecutive `parallel_safe` calls run concurrently,
  at most eight at a time, while a write, edit or `bash` call is a barrier.
  Results and `ToolFinished` events stay in call order; cancellation still
  leaves one result per call.
- MCP tools: after the explicit `--mcp`/`KEEL_MCP=1` opt-in, enabled stdio and
  Streamable HTTP servers from `.keel/mcp.toml` start with the workspace, list
  their tools as `server__tool`, and register in the same `ToolRegistry`. They
  use the same permission gate, timeout, cancellation and truncation as
  built-ins. MCP calls are barriers unless a server opts into `parallel_safe`;
  a missing or crashing server is a warning and contributes no tools.
- `Harness` agent loop: model turn, tool calls, one result per call, next turn,
  with a turn budget and a cancellation token.
- Default system prompt (`keel-core`): one embedded prompt for every model and
  protocol, applied by `Session::new` so embedders get it too. `Session::with_system`
  and the CLI's `--system` append after it; resume starts from the header prompt
  instead of adding the default again.
- Head-and-tail output truncation with an explicit marker.
- `keel-testkit`: scripted HTTP/SSE server (gated chunks, stalls, retryable
  errors, request capture), ordered cassette player, and loopback live recorder
  with header, query, and body secret redaction.
- Behavioral eval suite (`crates/keel-evals`): a Python fixture, twelve tasks
  with mechanical checks, offline multi-turn cassette replay in `cargo test`,
  and an `#[ignore]`d live mode that records NeuralWatt
  `deepseek-v4.1-flash` under a $10 cap and writes a per-task report.
- Golden request bodies per policy and protocol (`Auto`, `None`, `Object`),
  decoder conformance tests against borrowed, MIT-licensed OpenAI Chat and
  OpenAI Responses streams (decoder fixtures, separate from the first-party
  captures), and manifest-checked cassettes including five first-party live
  NeuralWatt captures across Chat, Messages, and Responses.

## Benchmarks

[`benchmarks/`](benchmarks/README.md) holds the measurement layer for
Terminal-Bench and SWE-bench. It is stdlib Python 3.11+; the unit tests need no
Docker, Harbor, `swebench`, or network access.

- **Harbor adapter.** [`benchmarks/harbor/keel_agent.py`](benchmarks/harbor/keel_agent.py)
  implements Harbor's `BaseInstalledAgent` and uploads a Linux `keel` binary
  into a Terminal-Bench task container. It runs one non-interactive `keel chat`
  turn with `--terminal`, writes a canonical trace, and leaves verification to
  Harbor. A live
  `hello-world` smoke at `13d32de` completed the agent phase with 3 turns, 2
  tool calls, 122 input tokens, 172 output tokens, and 3.717 s wall clock. The
  trace is agent-phase only (`verifier_result="not_run"`); Harbor's verifier
  timed out during its own `apt-get`/`uv` setup, so the smoke is not a scored
  result. The smoke used an x86_64 musl binary under Rosetta; a native aarch64
  artifact is still unbuilt. See [`benchmarks/harbor/README.md`](benchmarks/harbor/README.md).
- **SWE-bench runner.** [`benchmarks/swebench/run_keel.py`](benchmarks/swebench/run_keel.py)
  runs keel in an official SWE-bench instance image, extracts the patch, writes
  official predictions JSONL, and invokes `swebench.harness.run_evaluation`.
  Finalization stages the workspace in a base-seeded temporary index, excludes
  generated paths, gates added lines with `git diff --check`, records test-file
  edits, and runs `git apply --check` in a pristine worktree. A no-change exit
  or a whitespace-gate failure gets one resume-last retry; a patch that still
  fails the gate is not submitted and carries `patch-failure`. It is
  unit-tested, dry-run checked, and has one confirmed live Lite instance
  (below); `cost_usd` is null. See
  [`benchmarks/swebench/README.md`](benchmarks/swebench/README.md).
- **Trace toolkit.** [`benchmarks/traces/`](benchmarks/traces/README.md)
  defines schema v1.0 as JSONL with 26 required fields, a stdlib reporter
  (pass@1, pass@k, task-level bootstrap interval, cost, wall clock, turns,
  tokens, patch and harness health), the 16-tag failure taxonomy, and an
  atomic tag editor with an audit trail. `not_run` counts as a loss until a
  harness result is merged or the trial is tagged.
- **Fixed subsets.** [`benchmarks/subsets/`](benchmarks/subsets/README.md)
  pins the 89-task Terminal-Bench 2.0 manifest, a 10-task iteration set, a
  20-task deterministic canary, and a 10-instance SWE-bench Lite bring-up set.
  SWE-bench Pro V2 remains a documented placeholder. The lists are
  [`iteration-terminal-bench.txt`](benchmarks/subsets/iteration-terminal-bench.txt),
  [`canary-terminal-bench.txt`](benchmarks/subsets/canary-terminal-bench.txt),
  [`bringup-swebench-lite.txt`](benchmarks/subsets/bringup-swebench-lite.txt),
  and [`swebench-pro-v2.placeholder.md`](benchmarks/subsets/swebench-pro-v2.placeholder.md).

### Live benchmark results (wave 0 baseline)

The full 10-task Terminal-Bench iteration attempt ran with Harbor 0.27.0, the
x86_64 musl binary under Rosetta, `neuralwatt/deepseek-v4.1-flash` and
`max_turns=60`. This is not a clean 10-task pass@1 or a full benchmark. The four
wave-1 slices (`911427a`, `7d67a70`, `ef4ca13`, `5e461aa`) landed after these
runs.

A later 10-task rerun at clean `5e461aa` measures only M12.4 and M12.5. It
scored nine tasks at 1.0 and `query-optimize` at 0.0; `extract-elf` is invalid
again, leaving a valid rate of 8/9 (0.889), or 8/10 treating it as a failure.
On the eight tasks valid in both waves the ratio moved 6/8 to 7/8, but the only
flip is `cancel-async-tasks`, which is model variance. `terminal` was called
zero times in 339 calls and `jobs` 17 times on two tasks, so **wave 1 has no
measurable effect on Terminal-Bench**. M12.6 and M12.7 remain unmeasured. The
table below is the wave-0 baseline; the comparison is in
[`docs/notes/m12-wave1-results.md`](docs/notes/m12-wave1-results.md).

| Task | Result | Turns / calls | Wall | Notes |
| --- | --- | --- | --- | --- |
| `fix-git` | 1.0 | 27 / 34 | 1.0 min | pass |
| `regex-log` | 1.0 | 22 / 21 | 3.1 min | pass |
| `nginx-request-logging` | 1.0 | 17 / 27 | 1.0 min | pass |
| `kv-store-grpc` | 1.0 | 14 / 19 | 2.7 min | pass |
| `fix-code-vulnerability` | 1.0 | 16 / 20 | 0.7 min | pass |
| `custom-memory-heap-crash` | 1.0 | 54 / 53 | 7.2 min | pass; 434.7 s agent wall; `stop_reason=completed`; verifier passed (Harbor runtime 8m 38s); the image pull stalled and was retried manually before this scored run |
| `query-optimize` | 0.0 | 28 / 38 | — | valid failure: hit the 900 s agent timeout; never wrote `sol.sql`; the verifier ran and 2/6 tests passed |
| `cancel-async-tasks` | 0.0 | 60 / 60 | — | valid failure: hit `max_turns=60`; the verifier passed 4/6 tests, failing `test_tasks_run_concurrently` and `test_tasks_obey_max_concurrent` |
| `modernize-scientific-stack` | invalid | — | — | the verifier never ran tests; setup failed with `curl: (7) Failed to connect to releases.astral.sh port 443` and `uvx: command not found` |
| `extract-elf` | invalid | — | — | the agent read the task's own `test_outputs.py` and reference solution over the network; the verifier also hung on `astral.sh` |

Six passes across all 10 attempted tasks. Eight trials are valid (6 pass, 2
fail): a valid pass rate of 6/8 = 0.75. `extract-elf` and
`modernize-scientific-stack` are excluded as invalid; the raw attempted rate
is 6/10, but it is not a clean benchmark pass@1 because two runs were invalid.

**SWE-bench Lite**

| Task | Result | Notes |
| --- | --- | --- |
| `astropy__astropy-12907` | 1 resolved, 0 unresolved, 0 empty and 0 errors | clean 4,712-byte patch, 0 mode-only pairs; one instance only |

That is one instance, not a Lite-wide score.

**Caveats.**

- **Not a full score.** The 10-task iteration set is not the 89-task manifest,
  the 20-task canary or a full benchmark. The 0.75 rate is over valid trials;
  the raw 6/10 is not a clean pass@1 because two runs were invalid.
- **Contamination scan, not a controlled test.** The containers had network
  access. All six passing logs and both valid failures were scanned for
  obvious web or reference access; none was found beyond package-manager URLs.
  The custom-memory log has no URLs and no fixture or reference access (only
  the word "solution" in normal reasoning). `extract-elf` did read task
  fixtures and a reference solution, and is excluded.
- **Two invalid runs.** `modernize-scientific-stack` and `extract-elf` never
  produced a valid verifier result and are excluded.
- **One pass needed an environment retry.** The `custom-memory-heap-crash`
  prebuilt image pull stalled, was retried manually, and the scored 1.0 comes
  from a successfully started environment.
- **SWE-bench is n=1.** One Lite instance is not a Lite-wide score.
- **Verifiers need the network.** Harbor's verifier setup fetches `apt`
  packages and `astral.sh/uv`; `modernize-scientific-stack` failed setup with
  `curl: (7) Failed to connect to releases.astral.sh port 443` and
  `uvx: command not found`, so a verifier setup failure is an environment
  failure, not an agent failure.

The benchmark notes are [`docs/notes/m12-wave0-results.md`](docs/notes/m12-wave0-results.md),
[`docs/notes/m12-wave1-results.md`](docs/notes/m12-wave1-results.md),
[`docs/notes/benchmark-readiness.md`](docs/notes/benchmark-readiness.md), and
[`docs/notes/benchmark-results-template.md`](docs/notes/benchmark-results-template.md).

## Not implemented yet

| Gap | Where it belongs |
|---|---|
| OS-level confinement for `bash` (cwd-scoped today), and the non-Unix file-tool fallback | O-010, later |
| MCP optional GET server-to-client stream, SSE resumability, and OAuth authorization flows | out of scope; static headers only |
| MCP servers calling back into keel (keel as an MCP server) | out of scope |
| Live captures from Google endpoints | O-008 remainder, M11; all three implemented protocols replay first-party NeuralWatt captures (M8), and OpenAI- and Anthropic-endpoint-specific captures are explicitly out of scope |
| Google/Antigravity module, OAuth PKCE, project/model discovery, schema sanitizer | D-005, O-012, M11 |
| Windows ConPTY parity and Windows job objects for the persistent terminal and background jobs | out of scope for M12.4 and M12.5; Windows keeps the one-shot `bash` path |
| Shell job control (`fg`, `bg`, shell job specs) over background jobs | out of scope; `jobs` uses explicit ids and actions |
| Model fallback | O-004 remainder; turn retry landed, fallback is out of scope for now |

## Testing

Everything in the default suite runs offline.

- CI runs `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  -- -D warnings` and `cargo test --workspace` on Ubuntu, macOS and Windows
  with the same command list. Windows runs 62 fewer tests than macOS;
  the Unix-gated cases and the Windows behavior that remains untested are
  listed with the earlier counts in `docs/notes/windows-ci.md`. On `main` at
  `5e461aa` the suite is 679 passed, 0 failed, 2 ignored on macOS and Ubuntu,
  and 617 passed on Windows (from the Windows CI job log). GitHub CI is green
  on all three runners.
- The benchmark test suite is separate:
  `python3 -m unittest discover -s benchmarks/tests -v` -> 278 tests OK. It is
  stdlib-only, runs offline, and CI runs it on Ubuntu with Python 3.12 in the
  `benchmarks` job, together with `python3 -m compileall benchmarks`. That job
  is green as well.
- `keel-testkit` serves a scripted list of turns over a real socket. A chunk can
  be gated until the test releases it, a response can stall after its headers,
  and every request body is recorded for assertions.
- Request bodies are pinned as golden files in
  `crates/keel-llm-protocol/tests/fixtures/requests/`, one per policy and
  protocol. Regenerate with
  `UPDATE_GOLDEN=1 cargo test -p keel-llm-protocol --test requests` and review
  the diff before committing.
- The OpenAI Chat and OpenAI Responses decoders are replayed against real
  recorded streams in `crates/keel-llm-route/tests/fixtures/recorded/`;
  provenance and license are in that directory. These are borrowed,
  MIT-licensed decoder conformance fixtures from opencode, not first-party
  captures; the first-party captures are the NeuralWatt cassettes below.
- Cassettes in `crates/keel-llm-route/tests/fixtures/cassettes/` replay a whole
  exchange through the real client: the request keel encoded, the status, the
  headers and the raw body. Replaying a request that changed is a miss, not a
  passing decode test; regenerate the encoder-produced request half after an
  intentional wire change with
  `UPDATE_CASSETTE=1 cargo test -p keel-llm-route --test cassettes`.
  `scenarios.toml` fails when a cassette on disk is not covered. The player
  consumes interactions in recorded order, once each; a request that does not
  match the next one is an HTTP 422 miss (non-retryable), and an unconsumed
  interaction fails the replay. `keel-testkit` also carries the recorder, a
  loopback proxy that writes a cassette from a live call with header, query, and
  body secret redaction; it refuses a real endpoint unless
  `KEEL_CASSETTE_RECORD=1` and never records in CI. Five cassettes are
  first-party whole exchanges recorded from NeuralWatt, covering all three
  protocols and both a tool call and a reasoning turn; the rest pair a
  keel-encoded request with borrowed decoder-fixture response bytes. Only the
  NeuralWatt files are first-party captures (M8).
- Credentials are covered by store round-trip and version tests, Unix
  0600/0700 checks, symlinked file refusal and symlinked-parent follow, FIFO and
  directory refusal, empty-value fallback, optional-bearer anonymous behavior
  with a broken or newer store, an atomic-replacement test with concurrent
  readers, env precedence and fallback, missing-source error text,
  `Debug`/error/CLI output checks that no secret leaks, one coalesced reload
  under 16 concurrent resolves, two concurrent `Client` requests that share one
  store read, a real `Client` request whose `Authorization` header comes from
  the store, and transport-error query redaction. CLI tests run the binary for
  `auth set/list/remove`, validate a key before prompting, reject extra
  arguments, and run a `chat` turn authenticated only by a stored key as well
  as one where the environment wins.
- Workspace tests include a smoke test that swaps a directory for an outside
  symlink while `open_existing` runs; it checks that any successful read is the
  inside file, but the real race window is sub-microsecond, so it is not a
  regression test for the old resolver. Internal-symlink refusal (Unix only)
  and parent creation under `write_file` are pinned deterministically, as are a
  failed write and edit leaving the old bytes (Unix only), mode preservation
  (Unix only), the umask mode of a new file (Unix only), no `.keel-*` temp left
  after a successful replace on every platform (the forced-failure half is Unix
  only), a read-only file staying put (permission bits on Unix, the read-only
  attribute on Windows), a FIFO and a socket refused without blocking or
  replacement (Unix only), special mode bits dropped (Unix only),
  byte-identical output when `edit` does not match, and a symlink swapped in
  before the rename being replaced rather than followed (Unix only).
- The persistent terminal is covered on Unix by seven unit tests and eight
  integration tests: `cd`/`export` persistence, `workdir`, non-UTF-8 output, a
  dropped call interrupting a command while keeping the session, an
  interactive read timing out and recovering, shell exit and restart, and
  dropping the tool leaving no shell or background child. The CLI tests pin the
  opt-in `tools` listing and a full `chat --terminal` turn; the default-registry
  test proves `bash` calls still do not share state.
- Background jobs are covered on Unix by four buffer unit tests and thirteen
  integration tests in `crates/keel-tools/tests/jobs.rs`: a synthetic
  server-plus-test task polls until ready, runs a test command, kills the server
  and checks that the port is closed; a 10 MiB writer is bounded and read in
  chunks; a grandchild holding a pipe does not block `wait`; a timeout kills the
  process group while an unrelated job and a `setsid` service survive; a
  timeout is enforced with no further poll; a job keeps running across
  foreground calls and tool calls; drop leaves no orphan;
  a cancelled `wait` does not kill the job; and a run deadline caps the job
  timeout. A CLI test runs `jobs start`, `poll` and `kill` through a real
  `chat --terminal` turn, and the default registry has neither tool.
- Parallel execution is covered in `crates/keel-core/tests/run.rs`: two tools
  on one turn meet at a barrier (which a sequential loop cannot pass), a
  `safe, unsafe, safe` turn never overlaps, a parallel run's transcript equals
  the same tools marked unsafe, ten safe calls cross the eight-call chunk
  boundary in order, cancellation before a batch matches the sequential loop,
  and cancellation inside a batch still leaves exactly one result per call.
- Built-in tool failures are pinned line for line in
  `crates/keel-tools/tests/builtin.rs`, and the golden request bodies pin the
  advertised `read` description and keel's default system prompt. The eval
  cassettes' tool text has its own drift gate in
  `crates/keel-evals/tests/tool_text.rs`; refresh it with
  `UPDATE_EVAL_CASSETTE=1 cargo test -p keel-evals --test tool_text` after an
  intentional tool-text change. When the gate rewrites them, only the request
  halves change; the recorded responses stay as they were. The committed
  cassettes are the live M3 recording. The result-text migration covers the
  two historical texts M2 changed; a future result-text change needs its own
  migration or a live re-record, and a replay miss is the signal. A system
  prompt change is not covered by that gate, so it also needs a live re-record.
- MCP is covered by the `mcp_fixture` binary and `crates/keel-mcp-testkit/tests/`.
  The stdio client tests cover initialize, paginated `tools/list`, progress
  notifications, ping and an unsupported server request, CRLF and blank lines,
  string and out-of-order response ids, RPC and MCP tool errors, malformed and
  non-UTF-8 output, the 1 MiB line cap, a mid-call crash, child reaping after a
  bad line, a timeout followed by a usable call, metadata limits including a
  zero-argument schema, missing or crashing servers, and reserved-name filtering.
  The Streamable HTTP tests use `FauxProvider` for the JSON and SSE paths,
  session/protocol headers, configured headers, a server-initiated `-32601`
  answer, truncated streams, batch rejection, 202/401/403/404/429/5xx mapping,
  body/line/frame caps, timeout, cancellation, `DELETE` on shutdown, overlapping
  parallel-safe calls, unsafe barriers, config warnings, and an end-to-end
  denied-then-allowed tool through `Harness`. CLI tests prove `keel tools` does
  not spawn or contact config without `--mcp`, does start a stdio server with
  the flag, reports parse warnings and a broken server, and starts via
  `KEEL_MCP=1`.
- Compaction is covered by `crates/keel-core/tests/compaction.rs`: the trigger
  on both sides of the exact boundary, pre-stream and mid-stream overflow
  (one compaction and one replay; a second overflow fails), captured summary
  requests with no tools, the replacement-view invariants, exact-fit rejection
  at the inclusive boundary, cause-specific view-fit errors, no compaction log
  record for a failed attempt, failed-summary usage still reported, log growth
  with a version-1 load, resume request equality, and a 20-step long-horizon
  session whose synthetic reported input is far past the window. The estimator
  has its own unit tests in `crates/keel-core/src/tokens.rs`.
- The catalog is covered by `crates/keel-llm-catalog/tests/providers.rs`, which
  pins the eight providers and all 21 model ids, protocols, endpoint paths,
  windows, capabilities and deprecated status. Unit tests cover
  generated-vs-overlay-vs-runtime precedence, a runtime overlay that adds a
  provider and model, an empty overlay, a duplicate provider warning, an
  unreadable file, malformed input and unknown keys falling back with a warning,
  and the committed files matching what is embedded. `keel-cli` tests pin the
  exact `keel models` and `keel providers` output and drive a runtime overlay
  through both `models` and `route`. `keel-catalog-gen` tests use a fixed JSON
  fixture: a failed parse or schema error, a dropped model missing any of the
  four pinned facts, or a mismatched provenance date leaves the previous file
  byte-for-byte untouched; warnings are forwarded before a later error; and a
  successful run leaves no temporary file. The committed snapshot's
  `fetched_at` and the PROVENANCE `Fetched:` line are asserted equal, so the
  pair cannot drift silently.
- Behavioral evals (`crates/keel-evals`) replay one multi-turn cassette per task
  through the real client, route and decoder. Tool calls execute against a temp
  copy of the fixture, and each task ends with one mechanical check: a command
  that exits 0, never an LLM judge. The eval wrapper normalizes the temp
  workspace path, sorts `search` output, filters Python bytecode from
  `glob`/`search` results, and rejects `find`/`ls`/`stat`/`du`/`tree` in the
  shell because their output differs between machines; `glob` is the
  deterministic path list. The suite needs `python3`, and CI installs `rg` on
  every runner when it is missing (Chocolatey, Homebrew or apt) for the
  `search` task. A check command has a 30-second deadline and its captured
  output is bounded; a check that hangs fails instead of stalling the suite. A
  task passes only on a clean stop (`EndTurn` or `MaxTurns`) with a passing
  check; `Failed` and `Cancelled` fail and cannot replace a cassette.
  The committed twelve eval cassettes hold live NeuralWatt responses recorded
  through the same recorder, so they are first-party captures too (M8).

### The catalog

`crates/keel-llm-catalog/catalog/` holds the two checked-in layers:

- `models.dev.toml` is a snapshot of model facts generated from
  <https://models.dev>: ids, names, context and output limits, and
  tool/image/reasoning capabilities. It is keyed by models.dev ids; `models_dev_id`
  on an overlay provider or model maps a keel id onto one. Its provenance and
  the models.dev MIT license are in `catalog/PROVENANCE.md`.
- `overlay.toml` carries what models.dev does not know: provider endpoints and
  auth, the per-model protocol (and optional path), `compat` overrides, and pins
  for facts that must not move when the snapshot is refreshed.

The catalog crate embeds both files with `include_str!` and merges them at
startup. Precedence, highest first: a runtime overlay, the checked-in overlay,
the generated snapshot, struct defaults. A value in the checked-in overlay wins
over the snapshot; a model or provider the overlay omits a generated field for
takes the snapshot value. Four facts are pinned because changing them would move
behavior: `openai/gpt-5.5` context, `anthropic/claude-haiku-4-5` output,
`opencode/claude-sonnet-4-5` context, and `anthropic/claude-sonnet-4-5`
deprecated status (models.dev marks it deprecated; keel still routes it).

The runtime overlay is merged by provider and model id. It may override existing
fields or append new providers and models; existing order is preserved and new
entries append in file order. Every field except an entry's `id` is optional
when patching an existing entry, so one field can change without restating the
entry. An `auth` value is replaced as a whole, so adding one environment
variable means restating the full auth shape; `capabilities` and `compat` merge
field by field. There is no remove operation and no way to clear `base_url_env`,
`notes`, or a model's `path`. A new provider needs a `name`, a base URL (or
`base_url_env`) and at least one model; a new model needs a `name` and its
`protocol`. A provider listed twice in one patch applies in order and prints a
warning. A malformed file, unknown key, or merge that would produce an invalid
catalog prints one warning and is ignored, so a bad overlay never fails
`keel models`, `keel route` or `keel chat`.

`keel-evals` uses `Catalog::builtin()`, so a runtime overlay never affects
cassette replay.

Regeneration is an explicit developer action, never part of a build:

```sh
# From a local API dump (offline, and the only form allowed under CI):
cargo run -p keel-catalog-gen -- --from /tmp/models-dev.json

# Or fetch models.dev directly:
cargo run -p keel-catalog-gen
```

The generator reads `catalog/overlay.toml` to learn which providers and model
ids keel routes, writes a temporary file in the same directory, and renames it
over `models.dev.toml` only after the fetch, JSON parse, schema checks,
dropped-model fact checks and provenance check all succeed. A failure leaves the
previous last-known-good file untouched; the crate always builds from that
committed file. A mapped model that models.dev no longer lists is reported and
omitted from the snapshot. If its overlay entry does not pin `name`,
`context_window`, `max_output_tokens` and `capabilities`, regeneration refuses
to write, because an unpinned window would merge as zero and an unpinned name or
capabilities value would fail or silently change the route; an explicit `0`
counts as a pinned window. The snapshot records today's UTC date, and `PROVENANCE.md`
must record the same `Fetched:` date or the write is refused.

### Recording a live cassette

Recording is the only part of the suite that talks to a provider, and it is off
by default. The catalog carries NeuralWatt (`nw-small`, `qwen-3.8-27b`,
`deepseek-v4.1-flash`), which serves all three protocols from one host, so with
a key:

```
KEEL_CASSETTE_RECORD=1 NEURALWATT_API_KEY=sk-... \
  cargo test -p keel-llm-route --test record_live -- --ignored --nocapture
```

To keep the key out of a shell history, put it in a file and pass
`KEEL_RECORD_KEY_FILE=~/.config/neuralwatt/key`. The default records one chat
turn. Override `KEEL_RECORD_SCENARIO`, `KEEL_RECORD_UPSTREAM`,
`KEEL_RECORD_MODEL`, `KEEL_RECORD_PROVIDER` and `KEEL_RECORD_PROTOCOL`
(`openai-chat`, `openai-responses` or `anthropic-messages`).
`KEEL_RECORD_TOOL` sends one tool definition and fails the recording if the
model replies with prose instead of a call; `KEEL_RECORD_REASONING` (`minimal`,
`low`, `medium`, `high`) asks for reasoning effort. The test writes
`tests/fixtures/cassettes/<scenario>.json` and prints the manifest entry to add.
Nothing calls the network in the normal suite.

### Recording live evals

The M1 suite records one cassette per task and writes
`crates/keel-evals/reports/live.md` (and `live.json`). It is `#[ignore]`d, needs
its own opt-in in addition to the recorder's, and refuses to run under `CI`:

```
KEEL_EVALS_LIVE=1 KEEL_CASSETTE_RECORD=1 \
  cargo test -p keel-evals --test live -- --ignored --nocapture
```

The key comes from `NEURALWATT_API_KEY`, `KEEL_RECORD_KEY_FILE`, or
`~/.config/neuralwatt/key` (mode 0600; a key file that is group- or
world-readable is ignored); it is never printed. Before every model
request the runner checks spend so far plus that request's worst case against a
$10 hard cap, and stops before a request that would cross it, reporting the
remaining tasks as `not run (budget)`. Prices are the listed NeuralWatt
`deepseek-v4.1-flash` token rates in `crates/keel-evals/src/budget.rs`
($0.15 input, $0.015 cached input, $0.60 output per 1M tokens, read from
<https://neuralwatt.com/pricing> on 2026-11-19). The committed baseline is
12/12 passing, $0.039297 for the M3 recording run (M2 cost $0.034032 and the
two M1 runs cost $0.056289).

Known eval limits: replay returns one report per task, not a `RunReport`; the
live runner is the only writer of the aggregate report. The eval shell guard is
token-based (a determinism aid, not a sandbox), and a `search` result that the
tool already truncated keeps that head's order because the normalizer cannot
sort across a truncated set.

## Invariants

1. Provenance lives on the part, never reconstructed from a destination
   namespace.
2. Transcript repair runs before every request and is pure.
3. Tool arguments are raw text until one authoritative end event.
4. Every stream has a terminal `Done`; errors carry partial content and usage.
5. Retry at the session boundary, with one narrow pre-stream exception.
6. Per-protocol quirks are data (`Compat`), not vendor-specific clients.
7. Capabilities and protocol are properties of the model entry.
8. Every tool call gets exactly one result, including denied, unknown,
   timed-out and cancelled calls.
9. Tool execution is deny-by-default for embedders; the CLI opts into asking or
   `--yes`.

## Docs

- [`docs/BLUEPRINT.md`](docs/BLUEPRINT.md): invariants, current state, milestone
  roadmap (`M0`–`M12`), and worker rules.
- [`docs/architecture.md`](docs/architecture.md): layers, one request's path,
  decision mapping, and the next steps.
- [`docs/decisions.md`](docs/decisions.md): summaries of `D-001`..`D-012` and
  `O-001`..`O-013`.
- [`docs/notes/`](docs/notes/): engineering notes (`atomic-writes.md`,
  `codec-json-recursion.md`, `mcp-http.md`, `neuralwatt-live-captures.md`,
  `windows-ci.md`).
