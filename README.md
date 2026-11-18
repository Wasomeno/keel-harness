# keel

Provider seam and session loop for an LLM agent harness, written in Rust.

**Status:** third vertical slice. OpenAI Chat Completions, OpenAI Responses and
Anthropic Messages work end to end, and a session runs a full tool loop against
a workspace: model turn, tool calls, tool results, next turn.
Google/Antigravity is catalogued but not implemented.

## Layout

| crate | role | may depend on |
|---|---|---|
| `keel-llm-types` | canonical messages, parts, provenance, events, errors, usage, cache policy, `repair_transcript` | nothing of keel's |
| `keel-llm-protocol` | one module per wire protocol: body lowering and stream state machines | types |
| `keel-llm-route` | `Route = Protocol + Endpoint + Auth + Framing`, HTTP transport, SSE, retry classification | types, protocol |
| `keel-llm-catalog` | providers and per-model protocol assignments; seed TOML today | types |
| `keel-tools` | tool trait and registry, permission gate, workspace sandbox, built-in tools | types |
| `keel-testkit` | dev-only scripted HTTP/SSE server for transport tests | nothing of keel's |
| `keel-core` | route resolution, event assembly, session and agent loop | all of the above |
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
is interactive when `--prompt` is omitted.

`keel repair` is a debugging tool: it applies the same pure repair pass the
session uses, and prints the repaired transcript plus every change it made.

```sh
cargo run --bin keel -- repair messages.json commandcode/claude-sonnet-4-5
```

## Configuration

| variable | meaning |
|---|---|
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENCODE_API_KEY`, `COMMANDCODE_API_KEY`, `XAI_API_KEY` | provider keys |
| `ANTHROPIC_AUTH_TOKEN` | second accepted Anthropic source |
| `KEEL_<PROVIDER>_BASE_URL` | overrides the catalog endpoint |
| `KEEL_CUSTOM_BASE_URL` | required for the `custom` provider |
| `KEEL_CUSTOM_API_KEY` | optional key for `custom` |
| `KEEL_ANTIGRAVITY_ACCESS_TOKEN` | temporary access token until the OAuth flow lands |

Missing required credentials fail before any request is sent and name the
environment variable, never a value.

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
- SSE framing that survives arbitrary chunk splits, CRLF, comments, multi-line
  data, and a missing final blank line.
- Route composition and a streaming HTTP client with a typed error taxonomy.
- Narrow pre-stream retry only: connect failures, 429 and 5xx before the first
  byte. Once a stream opens, this layer stops.
- Per-model catalog with seed entries for all seven providers in scope.
- Session loop with streaming callbacks, partial output on failure, and history
  that is only committed after the stream opens.
- Tool registry with `read`, `write`, `edit`, `bash`, `glob` and
  ripgrep-backed `search`.
- Channel permission gate: allow-all, deny-all, or ask the host with a request
  the UI can render. A missing or dropped host is a denial.
- Workspace sandbox: normalized and canonicalized paths under one root;
  symlinks that leave the root are rejected.
- `Harness` agent loop: model turn, tool calls, one result per call, next turn,
  with a turn budget and a cancellation token.
- Head-and-tail output truncation with an explicit marker.
- `keel-testkit`: a scripted HTTP/SSE server with gated chunks, stalls,
  retryable errors and request capture.
- Golden request bodies per protocol, and decoder conformance tests against real
  recorded OpenAI Chat and OpenAI Responses traffic.

## Not implemented yet

| Gap | Where it belongs |
|---|---|
| Google/Antigravity module, OAuth PKCE, project/model discovery, schema sanitizer | D-005, O-012, phase 3 |
| Persistent, account-keyed credential store and single-flight refresh | O-005 |
| OS-level sandboxing, parallel tool execution, MCP tools | O-010 |
| Session persistence and resume | O-009 |
| Live captures from the vendors' own endpoints | O-008; NeuralWatt covers all three protocols, OpenAI, Anthropic and Google still replay borrowed bytes |
| Generated catalog | O-006; `catalog/seed.toml` is provisional |
| Full cache policy (`Auto` / `None` / `Object`) | O-003; `Auto` places up to two Anthropic breakpoints |
| Session-boundary retry and model fallback | O-004 |

## Testing

Everything runs offline.

- `keel-testkit` serves a scripted list of turns over a real socket. A chunk can
  be gated until the test releases it, a response can stall after its headers,
  and every request body is recorded for assertions.
- Request bodies are pinned as golden files in
  `crates/keel-llm-protocol/tests/fixtures/requests/`. Regenerate with
  `UPDATE_GOLDEN=1 cargo test -p keel-llm-protocol --test requests` and review
  the diff before committing.
- The OpenAI Chat decoder is replayed against real recorded streams in
  `crates/keel-llm-route/tests/fixtures/recorded/`; provenance and license are in
  that directory.
- Cassettes in `crates/keel-llm-route/tests/fixtures/cassettes/` replay a whole
  exchange through the real client: the request keel encoded, the status, the
  headers and the raw body. Replaying a request that changed is a miss, not a
  passing decode test. `scenarios.toml` fails when a cassette on disk is not
  covered. `keel-testkit` also carries the recorder, a loopback proxy that
  writes a cassette from a live call; it refuses a real endpoint unless
  `KEEL_CASSETTE_RECORD=1` and never records in CI. Five cassettes are real
  exchanges recorded from NeuralWatt, covering all three protocols and both a
  tool call and a reasoning turn; the rest pair a keel-encoded request with
  response bytes from recorded fixtures.

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

- [`docs/architecture.md`](docs/architecture.md): layers, one request's path,
  decision mapping, and the next steps.

The decision log lives outside this repository in the keel shared space
(`research/decisions.md`).
