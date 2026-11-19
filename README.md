# keel

Provider seam and session loop for an LLM agent harness, written in Rust.

**Status:** M0 through M6 are done. OpenAI Chat Completions, OpenAI
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
runs `parallel_safe` tools concurrently with results kept in call order.
Google/Antigravity is catalogued but not implemented.

## Layout

| crate | role | may depend on |
|---|---|---|
| `keel-llm-types` | canonical messages, parts, provenance, events, errors, usage, cache policy, `repair_transcript` | nothing of keel's |
| `keel-llm-protocol` | one module per wire protocol: body lowering and stream state machines | types |
| `keel-llm-route` | `Route = Protocol + Endpoint + Auth + Framing`, HTTP transport, SSE, retry classification | types, protocol |
| `keel-llm-catalog` | providers and per-model protocol assignments; seed TOML today | types |
| `keel-tools` | tool trait and registry, permission gate, workspace sandbox, built-in tools | types |
| `keel-testkit` | dev-only scripted HTTP/SSE server, cassette player and recorder | nothing of keel's |
| `keel-core` | route resolution, event assembly, session and agent loop, session log | all of the above |
| `keel-evals` | dev-only behavioral evals: Python fixture, 12 tasks, offline cassette replay, budget-capped live recording | core, catalog, route, tools, testkit, types |
| `keel-cli` | the `keel` binary | core, catalog, route, tools, types |

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
unless `--yes` is passed. `--max-turns` caps the loop (default 24). The command
is interactive when `--prompt` is omitted. On Unix the file tools are
descriptor-relative and refuse symlinks; `bash` only gets the workspace as its
working directory, so it is not OS-confined.

Every session starts with keel's default system prompt. `--system TEXT` appends
your text after it rather than replacing it, separated by a horizontal rule. The
prompt lives in `crates/keel-core/src/prompts/default_system.md` and is embedded
at compile time; it is the same for every model and protocol.

A turn that fails after output has arrived is replayed when the failure is
retryable (rate limit, overloaded, 5xx, network, timeout), up to
`--max-attempts` (default 3). The failed attempt stays in the log, repair keeps
it off the wire, and the retry sends no second copy of the user message.

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
`--max-tokens`) reach the model's catalog `context_window`. It also compacts
once and retries once when the provider reports a context-overflow error
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
messages that fit the user-message budget (newest first until the budget is
spent, then in their original order), and one user message starting with
`[keel handoff summary]`. No assistant turn, tool call or tool result is kept,
so a call and its result always leave together and `repair_transcript`
synthesizes nothing. The view is then checked as the next request against the
system prompt, tools and output budget; if it cannot fit, compaction fails
rather than dropping the request. One
`[compact] threshold|overflow: M -> N messages (K kept user), ~A -> ~B tokens`
line goes to stderr per compaction.

The JSONL log appends one record and never rewrites earlier bytes:

```json
{"type":"compaction","at":1763550009,"reason":"threshold","summary":{...},"messages":[...]}
```

`messages` is the replacement view, and `summary` is the same message. Resume
resolves the latest compaction record plus the messages after it, so a compacted
session continues from exactly the state the live session held. Log version 2
adds this record and the header config fields; version-1 logs still load. The
summary request's usage is reported separately as
`RunOutcome::compaction_usage`, printed on the final usage line when nonzero,
and charged by the eval budget; it is not folded into the main usage the
trigger reads. The token estimate used for the fit checks and for the no-usage
fallback is one token per three bytes plus fixed per-message and per-part
overheads and a flat 2,000-token image cost, documented in
`docs/architecture.md`.

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
per-user `%APPDATA%` ACL for the directory and file; CI covers Linux and macOS,
not Windows.

## Configuration

| variable | meaning |
|---|---|
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENCODE_API_KEY`, `COMMANDCODE_API_KEY`, `XAI_API_KEY`, `NEURALWATT_API_KEY` | provider keys |
| `ANTHROPIC_AUTH_TOKEN` | second accepted Anthropic source |
| `KEEL_<PROVIDER>_BASE_URL` | overrides the catalog endpoint |
| `KEEL_CUSTOM_BASE_URL` | required for the `custom` provider |
| `KEEL_CUSTOM_API_KEY` | optional key for `custom` |
| `KEEL_ANTIGRAVITY_ACCESS_TOKEN` | temporary access token until the OAuth flow lands |
| `KEEL_CREDENTIALS` | full path to the credential store (default: the platform config path) |
| `KEEL_DATA_DIR` | where session logs live (default: the platform data directory) |

Missing required credentials fail before any request is sent and name the
environment variable and/or store key, never a value.

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
- Bounded JSON nesting depth (64 containers) on tool schemas, tool-call
  arguments, and incoming SSE frames across all three wire codecs.
- SSE framing that survives arbitrary chunk splits, CRLF, comments, multi-line
  data, and a missing final blank line.
- Route composition and a streaming HTTP client with a typed error taxonomy.
- Persistent, account-keyed credential store behind `SecretResolver`, resolved
  after environment variables. `keel auth set|remove|list`, atomic 0600 writes
  in a 0700 directory on Unix, and single-flight reload shared by concurrent
  resolves.
- Narrow pre-stream retry (`Client::stream_with_retry` for connect failures,
  429, overloaded, and 5xx before the first byte) plus turn-boundary replay in
  `Harness` (`RunEvent::Retrying`, `--max-attempts`) that keeps failed attempts
  in the log while `repair_transcript` excludes them from the next wire request.
- Per-model catalog with seed entries for all eight providers in scope
  (`openai`, `anthropic`, `opencode`, `commandcode`, `grok`, `antigravity`,
  `neuralwatt`, `custom`).
- Session loop with streaming callbacks, partial output on failure, history
  committed only after the stream opens, append-only per-project JSONL logs,
  `keel sessions`, and `keel chat --resume last|<id>`.
- Session compaction at the catalog context window and on a provider
  `ContextOverflow`: a no-tools checkpoint summary on the session's own route,
  a view of recent user messages plus that summary, one `[compact]` status line
  on stderr, and a version-2 append-only `compaction` log record that resume
  resolves. Assistant turns and tool traffic never cross the cut.
- Tool registry with `read`, `write`, `edit`, `bash`, `glob` and
  ripgrep-backed `search`. Every description says when to use the tool, which
  tool to prefer (`search` vs `glob` vs `bash`), what it returns, and what a
  failure looks like.
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
- Parallel tool execution: consecutive `parallel_safe` calls run concurrently,
  at most eight at a time, while a write, edit or `bash` call is a barrier.
  Results and `ToolFinished` events stay in call order; cancellation still
  leaves one result per call.
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
- Golden request bodies per protocol, decoder conformance tests against real
  recorded OpenAI Chat and OpenAI Responses streams, and manifest-checked
  cassettes including five live NeuralWatt captures across Chat, Messages, and
  Responses.

## Not implemented yet

| Gap | Where it belongs |
|---|---|
| OS-level confinement for `bash` (cwd-scoped today), and the non-Unix file-tool fallback | O-010, later |
| Atomic replacement for `write`/`edit`; in-place `O_TRUNC` can leave a partial file on ENOSPC | O-010, later; temp-file-plus-rename would change mode and hard-link semantics |
| MCP tools | O-010 (split out), M7 |
| Live captures from the vendors' own endpoints | O-008, M8/M11; NeuralWatt covers all three implemented protocols, OpenAI, Anthropic and Google still replay borrowed bytes |
| Generated catalog and runtime overlay | O-006, M9; `catalog/seed.toml` is provisional |
| Full cache policy (`Auto` / `None` / `Object`) | O-003, M10; `Auto` places up to two Anthropic breakpoints |
| Google/Antigravity module, OAuth PKCE, project/model discovery, schema sanitizer | D-005, O-012, M11 |
| Model fallback | O-004 remainder; turn retry landed, fallback is out of scope for now |

## Testing

Everything in the default suite runs offline.

- `keel-testkit` serves a scripted list of turns over a real socket. A chunk can
  be gated until the test releases it, a response can stall after its headers,
  and every request body is recorded for assertions.
- Request bodies are pinned as golden files in
  `crates/keel-llm-protocol/tests/fixtures/requests/`. Regenerate with
  `UPDATE_GOLDEN=1 cargo test -p keel-llm-protocol --test requests` and review
  the diff before committing.
- The OpenAI Chat and OpenAI Responses decoders are replayed against real
  recorded streams in `crates/keel-llm-route/tests/fixtures/recorded/`;
  provenance and license are in that directory.
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
  `KEEL_CASSETTE_RECORD=1` and never records in CI. Five cassettes are real
  exchanges recorded from NeuralWatt, covering all three protocols and both a
  tool call and a reasoning turn; the rest pair a keel-encoded request with
  response bytes from recorded fixtures.
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
  regression test for the old resolver. Internal-symlink refusal and parent
  creation under `open_for_write` are pinned deterministically.
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
- Compaction is covered by `crates/keel-core/tests/compaction.rs`: the trigger
  on both sides of the exact boundary, pre-stream and mid-stream overflow
  (one compaction and one replay; a second overflow fails), captured summary
  requests with no tools, the replacement-view invariants, log growth with a
  version-1 load, resume request equality, and a 20-step long-horizon session
  whose synthetic reported input is far past the window. The estimator has its
  own unit tests in `crates/keel-core/src/tokens.rs`.
- Behavioral evals (`crates/keel-evals`) replay one multi-turn cassette per task
  through the real client, route and decoder. Tool calls execute against a temp
  copy of the fixture, and each task ends with one mechanical check: a command
  that exits 0, never an LLM judge. The eval wrapper normalizes the temp
  workspace path, sorts `search` output, filters Python bytecode from
  `glob`/`search` results, and rejects `find`/`ls`/`stat`/`du`/`tree` in the
  shell because their output differs between machines; `glob` is the
  deterministic path list. The suite needs `python3`, and CI installs `rg` on
  macOS for the `search` task. A check command has a 30-second deadline and its
  captured output is bounded; a check that hangs fails instead of stalling the
  suite. A task passes only on a clean stop (`EndTurn` or `MaxTurns`) with a
  passing check; `Failed` and `Cancelled` fail and cannot replace a cassette.

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
  roadmap (`M0`–`M11`), and worker rules.
- [`docs/architecture.md`](docs/architecture.md): layers, one request's path,
  decision mapping, and the next steps.
- [`docs/decisions.md`](docs/decisions.md): summaries of `D-001`..`D-010` and
  `O-001`..`O-013`.
- [`docs/notes/`](docs/notes/): engineering notes (`codec-json-recursion.md`,
  `neuralwatt-live-captures.md`).
