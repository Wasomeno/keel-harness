# Decisions and open topics

In-repo summary of keel's architectural decisions (`D-001`..`D-010`) and open or
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

## Open topics

- **O-001 — Async runtime and concurrency model** *(Partially settled)*: `tokio`
  and `tokio_util::CancellationToken` are settled (D-007); stream backpressure
  policy remains open.
- **O-002 — Canonical event model details** *(Partially settled)*: progress
  deltas (`*Delta`) versus authoritative end events (`*End`, `ToolCall`), stable
  slot indices, and mandatory terminal `Done` are implemented; remaining
  conformance edge cases stay open alongside vendor captures (M8, M11).
- **O-003 — Cache policy shape** *(Open — M10)*: typed `Auto | None | Object`
  policy, per-protocol breakpoint caps, and warnings when breakpoints are
  dropped. `Auto` places up to two Anthropic breakpoints today; full policy
  choices need Arndev's sign-off before scoping M10.
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
  ACL and is not covered by CI (Linux and macOS only).
- **O-006 — Catalog generation and runtime overlay** *(Open — M9)*: build-time
  `models.dev` baseline, runtime discovery overlay with TTL, and offline
  last-known-good fallback (`catalog/seed.toml` is provisional).
- **O-007 — Tool schema projection and sanitizer** *(Open — M11)*: per-protocol
  JSON Schema restrictions, Responses `strict` mode, and table-driven schema
  sanitization for Google/Antigravity.
- **O-008 — Testing harness and vendor captures** *(Partially settled — M8,
  M11)*: faux server, golden requests, recorded fixtures, ordered cassette
  player and recorder, `scenarios.toml`, body-level secret redaction, five live
  NeuralWatt captures across Chat, Messages, and Responses, and one replay
  cassette per M1 task are in place. First-party captures from OpenAI and
  Anthropic (M8) and Google (M11) remain open.
- **O-009 — Session persistence and durability** *(Settled locally — M4;
  cross-machine sync out of scope)*: append-only per-project JSONL logs,
  `SessionSink`, `--resume last|<id>`, and `keel sessions` are implemented. M4
  added local context-window compaction: the trigger uses the last reported
  input plus output plus the next output budget against the catalog
  `context_window`, the summarizer is the session's own model with no tools, the
  replacement view keeps recent user messages verbatim plus one prefixed
  handoff summary, and a version-2 `compaction` record holds the summary and the
  view. Resume reads the latest record plus the messages after it; version-1
  logs still load. Cross-machine sync remains out of scope.
- **O-010 — Tool execution, sandboxing, and MCP** *(Partially settled — M6;
  MCP in M7)*: `ToolRegistry`, `PermissionGate`, lexical + canonicalized
  `Workspace` checks, cancellation, and truncation landed in D-007. M6 closed
  the file-tool TOCTOU gap on Unix with descriptor-relative opens (`openat` from
  an open root descriptor, `O_NOFOLLOW` at every component; writes create
  parents with `mkdirat`) and made the run loop execute consecutive
  `parallel_safe` calls concurrently in bounded, order-preserving batches, with
  unsafe calls as barriers. Symlink refusals name the path and point at
  `bash cat`; writes still truncate in place, so atomic replacement is deferred
  to preserve mode and hard-link semantics. `bash`, `glob` and `search` still
  receive canonicalized paths; `bash` is cwd-scoped, not OS-confined, and
  non-Unix keeps the canonicalizing fallback. MCP-backed tools are M7.
- **O-011 — Harness loop taxonomy** *(Open)*: native loop shapes beyond ReAct
  (generate-test-repair, plan-execute, search strategies).
- **O-012 — Antigravity provider enablement and consent** *(Open — M11)*:
  default enabled state, OAuth client identity (keel-owned vs user-provided),
  CLI/host labeling, account-rotation timing, and kill switch. Needs Arndev's
  sign-off before scoping M11.
- **O-013 — Distribution and update model** *(Open)*: release packaging, binary
  size, license bundle, and catalog updates without rebuilding the binary.
