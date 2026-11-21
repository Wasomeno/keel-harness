# Decisions and open topics

In-repo summary of keel's architectural decisions (`D-001`..`D-012`) and open or
partially settled design topics (`O-001`..`O-013`). See
[`docs/architecture.md`](architecture.md) for how implemented decisions map onto
crates and types, and [`docs/BLUEPRINT.md`](BLUEPRINT.md) for milestone ordering
(`M0`..`M11`).

## Decisions

- **D-001 — Own the provider seam, not the long tail** *(Decided)*: keel
  implements its own canonical request/response/event types, `Protocol` state
  machines, `Route` composition, and transcript repair in-tree, with no
  third-party unified LLM client (`keel-llm-types`, `keel-llm-protocol`,
  `keel-llm-route`).
- **D-002 — Hybrid architecture, Rust-native** *(Decided)*: one-way crate graph
  from session loop down to canonical types; `Route = Protocol + Endpoint + Auth + Framing`;
  per-model protocol selection; generated-catalog-shaped metadata; scripted-wire
  and recorded-stream tests.
- **D-003 — Pi replay discipline** *(Decided)*: every model-produced content part
  carries provenance (`Source { protocol, provider, model }`), and one pure
  `repair_transcript(messages, target)` pass runs on a preview before every
  request without mutating raw history (`keel-llm-types/src/repair.rs`).
- **D-004 — Provider coverage and protocol modules** *(Decided)*: four wire
  protocol modules cover the providers in scope — `openai-chat`,
  `anthropic-messages`, and `openai-responses` (implemented), plus
  `google-generative` (M11). Protocol belongs to the model entry rather than the
  provider because gateways serve multiple protocols from one base URL.
- **D-005 — Antigravity is a provider; its protocol module is first-party**
  *(Decided)*: Antigravity is a catalog entry (endpoint plus account-keyed OAuth
  credentials), while the Google/Antigravity wire module, OAuth PKCE flow,
  project and per-account model discovery, quota-vs-rate-limit classification,
  and JSON Schema sanitizer are compiled into keel as first-party code (M11).
- **D-006 — Implementation language is Rust** *(Decided)*: single binary, traits
  and state machines over enums, `tokio` async runtime, and
  `unsafe_code = "forbid"` across the workspace.
- **D-007 — Tool execution is a library, permissions are a gate, the sandbox is
  the workspace** *(Decided)*: tools live in `keel-tools` (`Tool`,
  `ToolRegistry`, channel `PermissionGate` defaulting to `DenyAll`, lexically
  checked and canonicalized `Workspace` root, 64 KiB 60/40 head-and-tail
  truncation), and `Harness::run` guarantees one `ToolResult` per call across
  normal, denied, unknown, malformed, timed-out, and cancelled calls.
- **D-008 — Testing is scripted wire, golden requests, and recorded responses**
  *(Decided)*: `keel-testkit` serves scripted HTTP/SSE turns over a real socket,
  `keel-llm-protocol` pins encoded canonical requests (`UPDATE_GOLDEN=1`),
  decoders replay real provider SSE captures offline, and `keel-evals` replays
  twelve coding tasks from ordered multi-turn cassettes in default `cargo test`;
  live recording is opt-in, refused under `CI`, and budget-capped (M1).
- **D-009 — Responses replay is stateless, with reasoning carried in the
  signature** *(Decided)*: `openai-responses` sends `store: false` and
  `include: ["reasoning.encrypted_content"]`, stores
  `{"id", "encrypted_content"}` as JSON in the reasoning part signature for
  verbatim replay (warning once per stream when a gateway omits the blob),
  addresses tool calls and outputs by `call_id` emitted once at
  `output_item.done`, treats `response.completed` and `response.incomplete` as
  terminal without `[DONE]`, and drops unsupported sampling parameters with a
  warning.
- **D-010 — Cassettes are ordered, request-keyed, replay-only, and covered by
  a manifest** *(Decided)*: each cassette stores full HTTP interactions matched
  on method, path, and JSON body value; the player consumes them in recorded
  order, once each, and a miss is a non-retryable HTTP 422; captures run through
  `keel-testkit`'s loopback recorder with header, query, and body secret
  redaction (`KEEL_CASSETTE_RECORD=1`, refused under `CI`); `UPDATE_CASSETTE=1`
  regenerates encoder-produced request halves; `scenarios.toml` enforces full
  on-disk coverage.

- **D-011 — MCP tools are ordinary tools over stdio or Streamable HTTP**
  *(Decided — M7; HTTP added after M10)*:
  servers are declared per workspace in `.keel/mcp.toml`, but nothing starts
  until the user opts in with `chat --mcp`, `tools --mcp`, or `KEEL_MCP=1`.
  Without the opt-in keel reads the file only to print parse warnings and say
  valid servers are configured and not started; it lists no MCP tools and spawns
  nothing. `keel route` names configured servers without starting them. `enabled` defaults to true and is a
  secondary filter. A server command is arbitrary code with the user's full
  privileges, outside the workspace sandbox, equivalent to `bash`; `--yes` and
  the tool permission gate do not cover startup, so the opt-in is the trust
  decision.
  A server is exactly one transport: `command` (stdio) or `url` (Streamable
  HTTP), never both and never neither; an invalid entry is a per-server
  warning. `args` and `env` are stdio-only, `headers` is HTTP-only, and a wrong
  pair warns without dropping the server. HTTP header values expand `${VAR}`
  once from the process environment; a missing or empty variable, a non-token
  name, a forbidden byte, or a reserved transport header skips that server.
  Only `http`/`https` URLs without userinfo or fragments are accepted. Header
  values are redacted from `Debug` and every error.
  Servers run over newline-delimited JSON-RPC 2.0 on stdio. Keel offers MCP
  `2025-06-18` and accepts `2025-06-18` or `2024-11-05`, sends `initialize`,
  `notifications/initialized`, paginated `tools/list`, and `tools/call`, and
  caps one response line at 1 MiB. Blank lines and CRLF are tolerated. The
  reader routes numeric or numeric-string response ids, ignores notifications,
  answers `ping`, returns `-32601` for other server-initiated requests, and
  never routes a method-bearing message to a pending call. One writer task owns
  stdin, so a timeout or cancellation cannot split a line. The handshake and all
  list pages share one startup deadline; `startup_timeout_ms` is capped at 60
  seconds. A malformed, non-UTF-8 or oversized line closes the server, fails
  pending calls, and kills and reaps the child.
  Tools register as `server__tool`; dots and colons map to `_`, other invalid or
  over-64-character names and collisions are skipped with a warning, and
  built-ins win. Remote schemas must have `type: "object"` and stay under
  16 KiB; a missing `properties` is normalized to `{}` for the provider.
  Descriptions are cut at 2 KiB. `McpTool` implements `Tool`, so the permission
  gate, harness timeout, cancellation and 64 KiB truncation are unchanged.
  Calls are barriers by default; `parallel_safe = true` is the per-server
  opt-in. A failed start or crash is a startup warning with a bounded stderr
  tail and no tools, never a harness error; there is no restart. Cancelling or
  timing out a call drops the response future and sends
  `notifications/cancelled`; the request line is still delivered, so a timed-out
  call may already have executed on the server. Dropping the last client kills
  the child.
  Credentials should reach a stdio server through the `env` name allowlist;
  `args` is free-form, so keeping secrets out of it is guidance rather than
  enforcement. The allowlist is hygiene, not a security boundary: the command
  is arbitrary, so `env` plus `HOME` can still read anything the user can.
  `env_clear` also drops variables such as `SSL_CERT_FILE` and `XDG_*` unless
  listed. An HTTP server is authenticated only by its static `headers` table;
  there is no OAuth flow, keyring lookup or interactive prompt, and the server
  sees those values on every request.
  The Streamable HTTP transport POSTs one JSON-RPC message per request with
  `Content-Type: application/json` and `Accept: application/json,
  text/event-stream`, then reads either one JSON body or an SSE stream. After
  `initialize` it sends `MCP-Protocol-Version` with the negotiated version and
  `Mcp-Session-Id` when one was offered. Notifications are ignored; a
  server-initiated request gets a bounded `-32601` POST; `202` is success for
  notifications. A stream without the matching response, a batch array, and a
  body, line or SSE frame over 1 MiB are protocol errors. Non-2xx responses are
  typed: 401/403 report that the server rejected the request and name the URL
  and configured headers to check, 404 after a session names an expired session
  that needs a restart, and 429/5xx carry the status. Header values are scrubbed
  from server-supplied error text, including JSON-RPC `error.message` and
  `data`.
  HTTP calls are not retried. Cancellation drops the POST and sends
  `notifications/cancelled` best effort; shutdown sends `DELETE` with the
  session id and ignores its error. The optional GET server stream, SSE
  resumability and MCP OAuth are out of scope, as is keel acting as an MCP
  server.

- **D-012 — Stream bodies have a per-chunk idle timeout, not a total deadline**
  *(Decided — post-M11 follow-up)*: `EventStream` owns a timer that is armed
  when the response status is known and re-armed by every body chunk. If no
  bytes arrive for
  `KEEL_STREAM_IDLE_TIMEOUT_SECS` (default 300; `0` disables the streaming
  bound), it yields one
  `LlmError` with `ErrorKind::Timeout` and the message `no data from the
  provider for Ns`, then ends the stream. A long generation that keeps sending
  text, reasoning or tool-argument deltas is unaffected; there is no
  total-request deadline. A terminal `Done` or `Error` also ends the stream and
  later body bytes are ignored: OpenAI Responses reaches that state at
  `response.completed`, while Chat and Anthropic wait for the body to close. A
  non-2xx error body is read under a 30 s total cap, or the idle bound when
  that is smaller, and is classified from the status, so a stalled error body
  cannot block the pre-stream retry. The streaming timer lives after
  `Client::stream_with_retry`; the transport retries only before a successful
  response stream opens. The harness's existing turn-boundary policy still
  decides whether to replay the failed turn, with repair keeping that attempt
  off the next request. The bound is deliberately generous because a provider
  may buffer a long reasoning phase without body bytes: a 120 s default was
  rejected as too close to an observed 1m56s turn with roughly 10.7k output
  tokens. Embedders can call `Client::with_stream_idle_timeout` (clamped at 30
  years) instead of setting the environment.

## Open topics

- **O-001 — Async runtime and concurrency model** *(Partially settled)*: `tokio`
  and `tokio_util::CancellationToken` are settled (D-007); stream backpressure
  policy remains open.
- **O-002 — Canonical event model details** *(Partially settled)*: progress
  deltas (`*Delta`) versus authoritative end events (`*End`, `ToolCall`), stable
  slot indices, and mandatory terminal `Done` are implemented; remaining
  conformance edge cases stay open, and Google capture conformance moves with
  M11 (O-008).
- **O-003 — Cache policy shape** *(Settled — M10)*: `CachePolicy` is
  `Auto | None | Object(CacheObject)` on `Request::cache`; `Session` carries the
  same policy (default `Auto`) and `Session::with_cache` applies it to every
  model turn. The resolved policy is recorded in `SessionMeta::cache`; resume
  restores it, so `--cache none` survives a later `--resume` instead of
  returning to `Auto`. A flag or `KEEL_CACHE` value still overrides the stored
  policy for that run without rewriting the header. The compaction summary is
  a one-shot request over a different message list, so it always uses
  `CachePolicy::None` rather than carrying positions that no longer fit or
  dropping its warnings. `Route` is unchanged: the wire protocol, not the
  endpoint, decides how a policy is expressed. Indices address the repaired
  request after `repair_transcript`; a caller computing them from its own
  history must run the same repair first.
  - `None` strips every cache field: no `cache_control`, no
    `prompt_cache_key`, and no unsupported-feature warning. `keel chat` exposes
    it as `--cache none` or `KEEL_CACHE=none` (the flag wins, default is
    `auto`). Strict OpenAI-compatible endpoints that reject `prompt_cache_key`
    should use `none`; the catalog's cache capability is not consulted because
    its default of `false` would silently drop the key for ordinary models.
  - `Auto` keeps today's Anthropic placement (the last system block and the
    last cacheable block of the final message, two breakpoints) and adds a
    deterministic `prompt_cache_key` to OpenAI Chat and Responses. That key is
    the only deliberate wire change for callers that never set a policy.
  - `Object` addresses protocol-neutral positions: `System`, `Tool(i)`,
    `Message(i)`, `Part { message, part }`, `LastMessage`; `ttl` is
    `FiveMinutes` (serialized as `{"type":"ephemeral"}`, no `ttl` field) or
    `OneHour` (adds `"ttl":"1h"`). Anthropic places them exactly where asked,
    including tool definitions. `System` is the last system block, `Tool(i)` is
    the i-th advertised tool, `Message(i)` is the last cacheable emitted block
    of canonical message i (a system-role message resolves to its system block),
    and `Part { message, part }` is that canonical part before wire reordering
    (tool results are emitted before user text); a system-role message is
    flattened to one block, so only part 0 addresses it; a later part index is
    `NotCacheable`, not `NoSuchPosition`. A position that does not exist
    (`NoSuchPosition`), a thinking/redacted block (`NotCacheable`), or a
    duplicate target (`Duplicate`) is dropped and warned, never panicked. A
    request over Anthropic's four-breakpoint limit keeps the first four in the
    order the caller listed them, which is priority order, and warns with
    `CapReached { cap, dropped }`. A caller that needs durable tool/system
    anchors lists those before a newest-turn breakpoint; a caller that needs the
    newest turn most lists it first.
  - OpenAI has no explicit breakpoint or TTL field on Chat or Responses, so
    `Object` still sends the derived `prompt_cache_key` and warns
    `Unsupported` for the parts it cannot express. An empty breakpoint list
    warns nothing, a nonempty list warns `ExplicitBreakpoints`, and a one-hour
    TTL warns `Ttl`. The five-minute TTL is OpenAI's default lifetime and is not
    warned as unsupported. It does not send
    `prompt_cache_options`/`prompt_cache_breakpoint`: those are GPT-5.6+ only
    and need a model gate this layer does not have.
  - The encoder sends the stable Messages API version `2023-06-01`. The
    prompt-caching documentation checked 2026-11-19
    (<https://platform.claude.com/docs/en/build-with-claude/prompt-caching>)
    shows the `5m` and `1h` `ttl` values on `/v1/messages` without a beta
    header, so the encoder emits `ttl` without `anthropic-beta`. A live
    Anthropic acceptance run for the one-hour TTL is not part of the suite.
  - The OpenAI key is `keel-v1-<16 lowercase hex>`: FNV-1a 64 over the key
    version, protocol, model, system text, tool count, and each tool's name,
    description and input schema. Identical prefixes share a key, a changed
    model/system/tool set changes it, and replay is stable because nothing
    time- or request-id-dependent enters the hash. The provider is deliberately
    excluded: protocol modules are provider-blind, and OpenAI cache accounting
    is account-scoped.
  - Warnings are typed (`CacheWarning` with `protocol` and `kind`) on
    `EncodedRequest::cache_warnings`. `Client::stream` appends their `Display`
    text to the warning queue it already feeds to `EventStream`, so they arrive
    as `LlmEvent::Warning` before the first provider frame and flow through
    `Assembler`/`StepOutcome` to the CLI. A warning cannot be swallowed by a
    decoder or stream error. A pre-stream HTTP failure opens no stream, so its
    warnings are not delivered; the request never reached a provider cache.
- **O-004 — Retry boundary** *(Settled; model fallback out of scope)*: narrow
  pre-stream retry (`Client::stream_with_retry`) and turn-boundary replay in
  `Harness` (`RetryPolicy::decide`, keeping failed turns in history/logs while
  `repair_transcript` drops them from the replay wire request without
  duplicating user input) are implemented. Quota, auth, invalid-request, decode,
  and abort errors are not retried here. Context overflow compacts once and
  replays once (M4), and a second overflow is a normal failure; model fallback
  is out of scope for now.
- **O-005 — Credential store** *(Settled — M5; OAuth refresh M11)*: persistent,
  account-keyed store behind `SecretResolver`, reached through
  `CredentialRef.store_key`, which `build_route` sets from the provider id.
  Resolution order is environment first, then the store; `EnvResolver` and
  `Client::new()` stay environment-only. The file is JSON (`version: 1`, a
  key-to-secret map) at the platform config path, with `KEEL_CREDENTIALS` as a
  full-path override; Unix writes are 0600 in a 0700 directory the store
  creates, atomically replaced through a temp file and rename. A symlink at the
  credential path is refused, while a symlinked config directory is followed and
  never chmodded; the reader requires a regular file, so directories and special
  files are refused, and it does not verify the mode of a file it did not write.
  Empty or whitespace values are treated as absent. `StoreResolver` caches a
  snapshot; a miss performs one reload and concurrent callers that saw the same
  generation share it. A broken store is an auth error for required auth but is
  treated as absent for optional bearer auth. There is no TTL and no refresh
  timer: OAuth token refresh and multiple simultaneously selectable accounts per
  provider are M11 / later work. `keel auth set|remove|list` reads the secret
  from stdin or a no-echo prompt, never argv, and lists keys only; concurrent CLI
  writers are last-writer-wins with no cross-process lock. Errors and `Debug`
  name keys, variables and paths, never values. Windows inherits the `%APPDATA%`
  ACL and its store tests run in CI, including a Windows-only test for the
  `%APPDATA%` default path, but the ACL specifics remain untested.
- **O-006 — Catalog generation and runtime overlay** *(Settled — M9)*: the
  catalog is a committed models.dev snapshot plus a checked-in keel overlay,
  both embedded at compile time; a build never fetches the network. The
  snapshot (`catalog/models.dev.toml`) holds only facts models.dev states (ids,
  names, context/output limits, tool/image/reasoning capabilities, deprecated
  status), keyed by models.dev ids, and is written by the dev-only
  `keel-catalog-gen` crate. The overlay maps keel ids onto source ids with
  `models_dev_id`. The generator is an explicit action (`cargo run -p
  keel-catalog-gen`, or `--from FILE` for a local API dump, refused under `CI`
  without `--from`); it writes through a temporary file and renames into place,
  so a failed fetch, parse, schema, pinned-fact or provenance check leaves the
  previous last-known-good file byte-for-byte untouched. A mapped model that
  disappears from models.dev must pin `name`, `context_window`,
  `max_output_tokens` and `capabilities` in the overlay (an explicit `0` counts
  as a window) or regeneration refuses to write; the snapshot records today's
  UTC date and `PROVENANCE.md` must match it. The overlay carries endpoints,
  auth, per-model protocol, optional path, `compat`, and pins for facts that
  must not drift. Precedence is runtime overlay > checked-in overlay > generated
  snapshot > defaults; a value in the overlay wins, and a field it omits comes
  from the snapshot. The optional runtime overlay is a static user file
  (`KEEL_CATALOG`, else `$XDG_CONFIG_HOME/keel/catalog.toml`,
  `~/.config/keel/catalog.toml` or `%APPDATA%\keel\catalog.toml`), not
  automatic remote discovery: existing providers/models merge by id and new
  ones append after filling the fields the merge cannot infer (name, provider
  base URL, model protocol). `auth` is
  replaced as a whole, `capabilities` and `compat` merge per field, and there is
  no remove/clear operation for providers, models, `base_url_env`, `notes` or
  `path`. A provider listed twice in one patch applies in order and warns. A
  malformed file, unknown key, missing required field on a new entry, or any
  validation failure after the merge warns and falls back to the embedded
  catalog, never fails a run. `Catalog::builtin` is the deterministic embedded
  catalog; `Catalog::load` adds the runtime layer, and the CLI uses it.
  `keel-evals` keeps `Catalog::builtin()`, so a runtime overlay never affects
  cassette replay. `keel models` and `keel providers` keep their columns and
  row order; their footer/notes text now names the snapshot and overlay, and
  both outputs are pinned by exact-output tests. The snapshot is MIT-licensed;
  provenance is in `crates/keel-llm-catalog/catalog/PROVENANCE.md`.
- **O-007 — Tool schema projection and sanitizer** *(Open — M11)*: per-protocol
  JSON Schema restrictions, Responses `strict` mode, and table-driven schema
  sanitization for Google/Antigravity.
- **O-008 — Testing harness and vendor captures** *(Settled for the implemented
  protocols — M8 re-scoped; Google remains M11)*: faux server, golden requests,
  recorded fixtures, ordered cassette player and recorder, `scenarios.toml`,
  body-level secret redaction, five live NeuralWatt captures across Chat,
  Messages, and Responses, and one replay cassette per M1 task are in place.
  M8 originally meant first-party captures from OpenAI's and Anthropic's own
  endpoints; the keys were unavailable, so on 2026-11-19 it was re-scoped to
  first-party NeuralWatt captures covering all three implemented protocols.
  The five route cassettes and the twelve eval cassettes hold live NeuralWatt
  responses recorded through keel's recorder; the five route cassettes keep
  both recorded halves, and the eval responses are the recorded M3 bytes. The
  fixed `.sse` files under
  `crates/keel-llm-route/tests/fixtures/recorded/` are borrowed, MIT-licensed
  decoder conformance fixtures from opencode, not first-party captures, and
  do not count toward M8. OpenAI- and Anthropic-endpoint-specific captures are
  explicitly dropped from scope, not deferred. Google captures remain open with
  M11.
- **O-009 — Session persistence and durability** *(Settled locally — M4;
  cross-machine sync out of scope)*: append-only per-project JSONL logs,
  `SessionSink`, `--resume last|<id>`, and `keel sessions` are implemented. M4
  added local context-window compaction: the trigger uses the last reported
  input plus output plus the next output budget against the catalog
  `context_window`, the summarizer is the session's own model with no tools, the
  replacement view keeps the newest user message verbatim plus older user
  messages within a budget and one prefixed handoff summary; a view that cannot
  fit fails instead of being stored. The summary request's usage is accounted
  for even when compaction then fails. A version-2 `compaction` record holds the
  summary and the view. Resume reads the latest record plus the messages after
  it; version-1 logs still load. Cross-machine sync remains out of scope.
- **O-010 — Tool execution, sandboxing, and MCP** *(Settled — M7)*: `ToolRegistry`,
  `PermissionGate`, lexical + canonicalized `Workspace` checks, cancellation, and
  truncation landed in D-007. M6 closed the file-tool TOCTOU gap on Unix with
  descriptor-relative opens (`openat` from an open root descriptor, `O_NOFOLLOW`
  at every component; writes create parents with `mkdirat`) and made the run loop
  execute consecutive `parallel_safe` calls concurrently in bounded,
  order-preserving batches, with unsafe calls as barriers. Symlink refusals name
  the path and point at `bash cat`. `write` and `edit` now replace through a
  temp file in the target's directory plus `renameat` (the temp is fsynced,
  then the directory best-effort), preserve an existing file's ordinary mode
  bits, and refuse a read-only target; the directory must be writable, and a
  replaced file gets a new inode, so hard links keep the old contents. `bash`,
  `glob` and `search` still receive canonicalized paths; `bash` is cwd-scoped,
  not OS-confined, and non-Unix keeps the canonicalizing fallback. M7 adds
  opt-in MCP tools through D-011, over stdio and Streamable HTTP; a stdio
  server runs with full user privileges and an HTTP server receives its
  configured headers only after `--mcp`/`KEEL_MCP=1`. Serving MCP, the optional
  GET stream, SSE resumability and OAuth remain out of scope.
- **O-011 — Harness loop taxonomy** *(Open)*: native loop shapes beyond ReAct
  (generate-test-repair, plan-execute, search strategies).
- **O-012 — Antigravity provider enablement and consent** *(Open — M11)*:
  default enabled state, OAuth client identity (keel-owned vs user-provided),
  CLI/host labeling, account-rotation timing, and kill switch. Needs Arndev's
  sign-off before scoping M11.
- **O-013 — Distribution and update model** *(Open)*: release packaging, binary
  size, license bundle, and catalog updates without rebuilding the binary.
