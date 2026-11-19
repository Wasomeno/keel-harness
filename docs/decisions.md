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
  decoders replay real provider SSE captures offline, and live behavioral evals
  stay opt-in outside default `cargo test` (M1).
- **D-009 — Responses replay is stateless, with reasoning carried in the
  signature** *(Decided)*: `openai-responses` sends `store: false` and
  `include: ["reasoning.encrypted_content"]`, stores
  `{"id", "encrypted_content"}` as JSON in the reasoning part signature for
  verbatim replay (warning once per stream when a gateway omits the blob),
  addresses tool calls and outputs by `call_id` emitted once at
  `output_item.done`, treats `response.completed` and `response.incomplete` as
  terminal without `[DONE]`, and drops unsupported sampling parameters with a
  warning.
- **D-010 — Cassettes are request-keyed, replay-only, and covered by a manifest**
  *(Decided)*: each cassette stores full HTTP interactions matched on method,
  path, and JSON body value; captures run through `keel-testkit`'s loopback
  recorder with header, query, and body secret redaction
  (`KEEL_CASSETTE_RECORD=1`, refused under `CI`); `UPDATE_CASSETTE=1`
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
  duplicating user input) are implemented. Quota, auth, invalid-request,
  context-overflow, decode, and abort errors are not retried here (overflow
  compaction-and-retry is M4); model fallback is out of scope for now.
- **O-005 — Credential store** *(Open — M5)*: persistent, account-keyed store
  behind `SecretResolver` (`CredentialRef.store_key`) with single-flight refresh
  per account. Only `EnvResolver` exists today.
- **O-006 — Catalog generation and runtime overlay** *(Open — M9)*: build-time
  `models.dev` baseline, runtime discovery overlay with TTL, and offline
  last-known-good fallback (`catalog/seed.toml` is provisional).
- **O-007 — Tool schema projection and sanitizer** *(Open — M11)*: per-protocol
  JSON Schema restrictions, Responses `strict` mode, and table-driven schema
  sanitization for Google/Antigravity.
- **O-008 — Testing harness and vendor captures** *(Partially settled — M8,
  M11)*: faux server, golden requests, recorded fixtures, cassette player and
  recorder, `scenarios.toml`, body-level secret redaction, and five live
  NeuralWatt captures across Chat, Messages, and Responses are in place.
  First-party captures from OpenAI and Anthropic (M8) and Google (M11) remain
  open.
- **O-009 — Session persistence and durability** *(Partially settled — M4)*:
  append-only per-project JSONL logs, `SessionSink`, `--resume last|<id>`, and
  `keel sessions` are implemented. Local context-window compaction is M4;
  cross-machine sync is out of scope.
- **O-010 — Tool execution, sandboxing, and MCP** *(Partially settled — M6,
  M7)*: `ToolRegistry`, `PermissionGate`, lexical + canonicalized `Workspace`
  checks, cancellation, and truncation landed in D-007. Closing the workspace
  TOCTOU gap (`openat`/`O_NOFOLLOW` or an OS sandbox) and parallel execution of
  `parallel_safe` tools are M6; MCP-backed tools are M7.
- **O-011 — Harness loop taxonomy** *(Open)*: native loop shapes beyond ReAct
  (generate-test-repair, plan-execute, search strategies).
- **O-012 — Antigravity provider enablement and consent** *(Open — M11)*:
  default enabled state, OAuth client identity (keel-owned vs user-provided),
  CLI/host labeling, account-rotation timing, and kill switch. Needs Arndev's
  sign-off before scoping M11.
- **O-013 — Distribution and update model** *(Open)*: release packaging, binary
  size, license bundle, and catalog updates without rebuilding the binary.
