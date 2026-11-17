# Architecture

## Shape

```
                       keel-cli
                          |
                       keel-core            session loop, assembly, route resolution
                      /    |     \
        keel-llm-catalog  |  keel-llm-route    catalog data | Route + HTTP + SSE + auth
                          |     /
                 keel-llm-protocol             one module per wire format
                          |
                   keel-llm-types              canonical types, events, repair
```

Dependencies point down. A protocol module cannot see the catalog, the session,
or a provider's name. The session cannot see JSON.

## One request's path

1. `Session::step_streaming` appends the pending user message to a *preview* of
   history. History is not committed yet.
2. `repair_transcript(preview, target)` returns a replay-safe message list and a
   list of notes (what it dropped, normalized, or synthesized).
3. The canonical `Request` is built: system, repaired messages, tools, limits,
   cache policy.
4. `build_route` has already resolved provider + model + compat into a `Route`:
   endpoint, protocol module, auth reference, framing.
5. `Client::stream` lowers the request through the protocol, resolves the
   credential, sends it, and returns an `EventStream` after the response status
   is known.
6. `EventStream` de-frames SSE, feeds each payload to the protocol decoder, and
   yields canonical `LlmEvent`s, ending with `Done`.
7. `Assembler` turns events into parts, using end events as authoritative and
   keeping partial content if an error arrives.
8. The assistant message gets provenance from the target, is appended to
   history, and `StepOutcome` reports text, usage, notes and warnings.

## Where the decisions land

| Decision | Code |
|---|---|
| D-001 own the provider seam | `keel-llm-protocol` implements the wire; no third-party unified client |
| D-002 hybrid, Rust-native | `Route`, per-model protocol, generated-catalog-shaped data, mock-wire tests |
| D-003 Pi replay discipline | `Source` on every model part; `repair_transcript` is one pure pass |
| D-004 coverage | four `ProtocolId`s; Chat and Messages implemented; Responses and Google are typed "unsupported" |
| D-005 Antigravity is a provider | catalog entry + auth data; its protocol module is first-party but later |
| D-006 Rust | workspace, traits, enums, no `unsafe` |

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

Event contract:

- `TextDelta` / `ReasoningDelta` / `ToolInputDelta` are progress only.
- `TextEnd` / `ReasoningEnd` / `ToolInputEnd` / `ToolCall` are authoritative.
- `Usage` is cumulative; the assembler assigns, it does not add.
- `Done` is mandatory. `Error` may arrive before it.
- Event `index` values are stable slots; ordering of parts is by index.

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
per request through `SecretResolver`. `EnvResolver` is the only implementation
today; the trait is where O-005 plugs in.

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

`RetryPolicy` is narrow and pre-stream: connect failure, 429, 5xx, overloaded,
with `retry-after` capped by policy. `Client::stream_with_retry` retries only
while opening the stream. Once `EventStream` exists, no retry happens.

Replay safety is a session decision (O-004): a turn that produced output must
not be silently re-run.

## Catalog

`catalog/seed.toml` is embedded at compile time and parsed once. It is seed data,
not the final catalog (O-006). Each model carries:

- `protocol` (the wire it speaks, not the provider's default)
- optional `path` override
- context and output limits
- capabilities (tools, images, reasoning, cache)
- `compat` overrides (max-token field, temperature, stream options, ...)

Providers carry `base_url`, an optional `base_url_env`, auth shape, and notes.
Notes are documentation for humans and are never used for routing.

## Testing

- Unit tests for ids, usage, error classification, SSE, auth, retry.
- Protocol tests with recorded wire payloads for both protocols.
- Repair tests for every rule, one test each.
- HTTP tests against a real local socket: streaming, 429 classification,
  credential headers, termination without `[DONE]`.
- Session tests: a two-step tool loop that verifies a synthesized result is
  replayed with a normalized id, pre-stream failure leaving history untouched,
  and partial output surviving a mid-stream error.

No test reaches the network. Cassettes and a faux provider are O-008.

## Next steps

1. OpenAI Responses, then Google/Antigravity with OAuth and the schema
   sanitizer (phases 2 and 3).
2. Credential store behind `SecretResolver` (O-005).
3. Tool execution and permissions (O-010).
4. Cassette harness plus faux provider; make missing coverage fail CI (O-008).
5. Generated catalog with a runtime overlay and last-known-good fallback (O-006).
