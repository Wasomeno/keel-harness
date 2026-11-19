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
`keel-llm-types`; it has no idea a model exists. `keel-testkit` is a dev-only
crate used by route and core tests, and is not part of the runtime graph.
`keel-evals` is dev-only too: it depends on core, catalog, route, tools,
testkit and types, drives `Harness` as a library, and nothing in the runtime
graph depends on it.

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
{"type":"meta","version":2,"id":"1763550000-a1b2","provider":"neuralwatt",...,"max_output_tokens":8192,"temperature":0.2}
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
stored text as-is. Version 2 additionally records `max_output_tokens` and
`temperature`, which resume applies unless a command-line flag overrides them;
version-1 headers load with those fields as `None`.

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
message's usage and uses the reported rule. At most one threshold compaction
runs per step.

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
`CompactionStats` and is not added to the step's assistant message, so the
trigger keeps reading the last main request's numbers; it is also surfaced as
`RunOutcome::compaction_usage`, which the eval budget and the CLI usage line
add to main usage. The eval gate charges it after the request, so a cap can be
exceeded by at most one summary request when a compaction lands on the
boundary.

The replacement view is, in order:

1. the system prompt (unchanged; it lives on the request, not in history),
2. the newest plain user message, always kept verbatim even when it alone
   exceeds the user budget, then older plain user messages newest-first until
   `min(20_000, context_window / 4)` estimated tokens are spent, restored to
   their original order,
3. one user message built by `summary_message`: `SUMMARY_PREFIX` from
   `crates/keel-core/src/prompts/compaction_prefix.md`, then the summary text.

Only `Role::User` messages with text or images and no tool calls or results
qualify. A prior handoff summary is excluded, so summaries do not accumulate.
Assistant turns, tool calls and tool results never cross the cut: a call and its
result leave together, and `repair_transcript` on the view synthesizes no
results. The view is not mutated further by repair; repair still runs on every
request as usual.

The view is then checked as the next request: system, view, pending input,
tools and the next output budget must fit the window. If they do not, compaction
fails with
`cannot compact: the replacement view needs about ... input tokens plus ...
output` instead of dropping the newest user message or sending an oversized
request. The stored view and the logged record are only written after that
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
tokens and summary usage. A step that threshold-compacts and then
overflow-compacts emits two. The CLI prints one stderr line per compaction:
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
pool. Writes truncate the opened file in place; atomic temp-file-plus-rename is
deferred because it would change mode and hard-link semantics.

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
| `write` | creates or overwrites a file | creates parent directories |
| `edit` | replaces one exact string | zero or multiple matches is an error unless `replace_all` |
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

`keel-evals` wraps these tools for replay without changing them: the temp
workspace path becomes `<workspace>`, `search` result lines are sorted, and the
eval shell rejects `find`, `ls`, `stat`, `du`, and `tree` because their output
depends on the machine. The model is told to use `glob` instead; the permission
gate is still `AllowAll` and every tool is registered.


## Where the decisions land

Summaries of `D-001`..`D-010` and `O-001`..`O-013` live in
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
| D-008 scripted wire, goldens, recorded streams | `keel-testkit` faux server, `keel-llm-protocol` golden requests (`UPDATE_GOLDEN=1`), offline recorded SSE fixtures, `keel-evals` replay/live suite |
| D-009 stateless Responses replay | `store: false`, `{"id", "encrypted_content"}` reasoning signatures, `call_id` addressing, tool calls emitted at `output_item.done` |
| D-010 ordered request-keyed cassettes and manifest | `CassetteServer`'s cursor (422 miss) + loopback `Recorder` with header/query/body redaction, `UPDATE_CASSETTE=1`, `scenarios.toml` coverage gate |

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

`catalog/seed.toml` is embedded at compile time and parsed once. It is seed data,
not the final catalog (O-006). Each model carries:

- `protocol` (the wire it speaks, not the provider's default)
- optional `path` override
- context and output limits, carried onto the `Route` as `context_window` and
  `max_output_tokens`; zero `context_window` disables compaction for a custom
  or uncatalogued route
- capabilities (tools, images, reasoning, cache)
- `compat` overrides (max-token field, temperature, stream options, ...)

Providers carry `base_url`, an optional `base_url_env`, auth shape, and notes.
Notes are documentation for humans and are never used for routing.

## Testing

Five layers. The default suite is offline; live recording is ignored and
opt-in:

1. **Scripted wire** (`keel-testkit`). A real socket serving a queue of turns.
   Chunks can be gated until the test releases them, a response can stall after
   its headers, and every request is recorded with its body. This is where
   cancellation, retry, chunk splitting and truncation are tested.
2. **Golden requests** (`keel-llm-protocol/tests/requests.rs`). One canonical
   request per protocol, encoded and compared byte-for-byte against a checked-in
   body. `UPDATE_GOLDEN=1` rewrites the files. A diff here means the request
   changed, which is the regression that matters most and the one a decode test
   cannot see.
3. **Recorded responses** (`keel-llm-route/tests/recorded.rs`). Raw SSE bodies
   captured from real providers, replayed through our framing and decoder.
   Provenance and the MIT license are in `fixtures/recorded/PROVENANCE.md`. The
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
   not covered.
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
   cassettes and report are the M3 run: 12/12, $0.039297. The shell guard is a
   token-based determinism aid, not a sandbox.

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
parent creation are pinned deterministically. Harness tests prove concurrency
with a barrier that two tools on one turn can only pass together, check that a
`safe, unsafe, safe` turn never overlaps, compare a parallel run's results with
the same tools marked unsafe, run ten safe calls across the eight-call chunk
boundary, compare cancellation before a batch with the sequential loop, and
check that cancellation inside a batch still leaves one result per call.

Plus the existing unit, repair, HTTP, session, tool and harness tests. Cassettes
are produced mechanically, never typed by hand: the recorder is a loopback proxy
that captures a live call, redacts credentials by header name, query name and
body pattern (bearer tokens, `sk-` keys, `AIza` keys, PEM blocks), and writes
the format the player reads. It captured five real exchanges from
NeuralWatt — chat text, a chat tool call, Anthropic Messages with a signed
thinking block, and two Responses turns — so those cassettes hold both halves as
the provider saw them, across all three protocols. The other cassettes are the
second mechanical path: the request half is the encoder's output and the
response half is a recorded fixture. Recording a non-loopback origin requires
`KEEL_CASSETTE_RECORD=1`, and `CI` refuses it outright.

The live captures corrected two assumptions. A chat tool turn arrives with
`delta.reasoning` in the same stream even when no reasoning effort, which the
decoder already maps alongside `reasoning_content`. And NeuralWatt answers
`include: ["reasoning.encrypted_content"]` with a summary and an item id but no
encrypted blob, so that turn decodes with no reasoning signature: the stateless
replay in D-009 depends on a provider that actually returns one.

## Next steps

Ordered per [`docs/BLUEPRINT.md`](BLUEPRINT.md):

1. MCP-backed tools registered in `ToolRegistry` (O-010 split, M7).
2. First-party live cassette captures from OpenAI and Anthropic endpoints (O-008
   remainder, M8).
3. Generated catalog with a runtime overlay and last-known-good fallback (O-006,
   M9).
4. Full cache policy (`Auto` / `None` / `Object`) across protocols (O-003, M10).
5. Google/Antigravity protocol module, OAuth PKCE, discovery, schema sanitizer,
   and Google captures (D-005, O-012, M11).
