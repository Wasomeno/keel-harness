# keel

Provider seam and session loop for an LLM agent harness, written in Rust.

**Status:** first vertical slice. OpenAI Chat Completions and Anthropic Messages
work end to end: request lowering, SSE framing, streaming state machines,
transcript repair, per-model routing, and a CLI. OpenAI Responses and
Google/Antigravity are catalogued but not implemented.

## Layout

| crate | role | may depend on |
|---|---|---|
| `keel-llm-types` | canonical messages, parts, provenance, events, errors, usage, cache policy, `repair_transcript` | nothing of keel's |
| `keel-llm-protocol` | one module per wire protocol: body lowering and stream state machines | types |
| `keel-llm-route` | `Route = Protocol + Endpoint + Auth + Framing`, HTTP transport, SSE, retry classification | types, protocol |
| `keel-llm-catalog` | providers and per-model protocol assignments; seed TOML today | types |
| `keel-core` | route resolution, event assembly, session loop | all of the above |
| `keel-cli` | the `keel` binary | core, catalog, route, types |

The dependency rule is one-way. No provider type reaches the session loop, and
no HTTP concern reaches the types crate.

## Try it

```sh
cargo test --workspace

cargo run --bin keel -- providers
cargo run --bin keel -- models
cargo run --bin keel -- route anthropic/claude-sonnet-4-5

OPENAI_API_KEY=... cargo run --bin keel -- chat openai/gpt-4.1 --prompt "hello"
```

`keel chat` streams to stdout and prints a stop reason plus usage. It is also
interactive when `--prompt` is omitted.

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
- SSE framing that survives arbitrary chunk splits, CRLF, comments, multi-line
  data, and a missing final blank line.
- Route composition and a streaming HTTP client with a typed error taxonomy.
- Narrow pre-stream retry only: connect failures, 429 and 5xx before the first
  byte. Once a stream opens, this layer stops.
- Per-model catalog with seed entries for all seven providers in scope.
- Session loop with streaming callbacks, partial output on failure, and history
  that is only committed after the stream opens.

## Not implemented yet

| Gap | Where it belongs |
|---|---|
| OpenAI Responses protocol | O-002, O-007 |
| Google/Antigravity module, OAuth PKCE, project/model discovery, schema sanitizer | D-005, O-012, phase 3 |
| Persistent, account-keyed credential store and single-flight refresh | O-005 |
| Tool execution, permissions, sandboxing | O-010 |
| Session persistence and resume | O-009 |
| Cassette harness and faux provider | O-008; current tests use local mock servers |
| Generated catalog | O-006; `catalog/seed.toml` is provisional |
| Full cache policy (`Auto` / `None` / `Object`) | O-003; `Auto` places up to two Anthropic breakpoints |
| Session-boundary retry and model fallback | O-004 |

## Invariants

1. Provenance lives on the part, never reconstructed from a destination
   namespace.
2. Transcript repair runs before every request and is pure.
3. Tool arguments are raw text until one authoritative end event.
4. Every stream has a terminal `Done`; errors carry partial content and usage.
5. Retry at the session boundary, with one narrow pre-stream exception.
6. Per-protocol quirks are data (`Compat`), not vendor-specific clients.
7. Capabilities and protocol are properties of the model entry.

## Docs

- [`docs/architecture.md`](docs/architecture.md): layers, one request's path,
  decision mapping, and the next steps.

The decision log lives outside this repository in the keel shared space
(`research/decisions.md`).
