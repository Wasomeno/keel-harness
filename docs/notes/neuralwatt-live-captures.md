# NeuralWatt live captures

**Recorded:** 2026-11-18, through `record_live` and the loopback recorder
**Cassettes:** `crates/keel-llm-route/tests/fixtures/cassettes/neuralwatt/`
**Provider:** NeuralWatt, model `nw-small`, Bearer key, one host
(`https://api.neuralwatt.com`)

## What was recorded

| scenario | protocol | request | reply |
|---|---|---|---|
| `chat-text` | `openai-chat` | one user message, 64 max tokens | `pong`, 64 prompt / 35 completion, 30 reasoning |
| `chat-tool-call` | `openai-chat` | `get_weather` schema, 512 max tokens | one call `get_weather({"city": "Paris"})`, 322 / 58, 29 reasoning |
| `messages-text` | `anthropic-messages` | cache breakpoints on system and last block, `max_tokens` 64 | `pong`, 64 / 14, thinking block with a signature |
| `responses-text` | `openai-responses` | `store: false`, 64 max tokens | `pong`, 64 / 19, 14 reasoning |
| `responses-reasoning` | `openai-responses` | `store: false`, `include: [reasoning.encrypted_content]`, effort `low` | `pong`, 52 / 19, 14 reasoning, **no encrypted blob** |

Commands (the key came from a file so it never reached a shell history):

```
KEEL_CASSETTE_RECORD=1 KEEL_RECORD_KEY_FILE=~/.config/neuralwatt/key \
  KEEL_RECORD_SCENARIO=neuralwatt/chat-tool-call KEEL_RECORD_TOOL=get_weather \
  KEEL_RECORD_PROMPT="What is the weather in Paris? Use the tool." \
  cargo test -p keel-llm-route --test record_live -- --ignored --nocapture
```

The protocol is chosen with `KEEL_RECORD_PROTOCOL` (`openai-chat`,
`openai-responses`, `anthropic-messages`) and reasoning with
`KEEL_RECORD_REASONING`.

## Findings

1. **Responses reasoning is not replayable on this gateway.** keel sent
   `include: ["reasoning.encrypted_content"]` with `store: false` and got back a
   reasoning item with an id and a `summary`, but no `encrypted_content`. The
   decoder emits the reasoning text with no signature, so a replay of that turn
   has nothing to send back. D-009 describes the stateless replay correctly, but
   it only holds for providers that return the blob. A warning when `include`
   was asked for and no blob arrives would make this visible at runtime.
2. **Chat streams carry `delta.reasoning`, not only `reasoning_content`.** The
   tool turn streamed `{"delta": {"reasoning": "The"}}` before the call, with no
   reasoning effort requested. The chat decoder already reads both fields.
3. **Anthropic Messages works with the same Bearer key.** The gateway accepted
   `Authorization: Bearer` (it also documents `x-api-key`), the request's cache
   breakpoints survived, and the reply carried a thinking block with a
   `signature_delta` (`c37824da…`, opaque hex) that keel stores as the part
   signature.
4. **Every endpoint appends two SSE comment lines**, `: energy {...}` and
   `: cost {...}`, before the terminal event. Chat puts them before
   `data: [DONE]`; Anthropic puts them before `event: message_stop`; Responses
   folds energy and cost into the `response.completed` body instead, and also
   sends `: keepalive` comments during the turn. Comments are not events, so
   none of them reaches the decoder as a warning.
5. **Responses still has no `[DONE]`.** The stream ends with
   `event: response.completed`; the capture confirms the framing assumption
   behind D-009.
6. **Usage arrives twice on Anthropic.** `message_start` reports input tokens
   with `output_tokens: 0`, then `message_delta` reports both. The decoder's
   merge (max per field) lands on 64 / 14.

## Cost

Five calls in total, about $0.0003 at the prices the streams reported
($0.000056, $0.000064, $0.000074, $0.000041 and $0.000041).

## Since the captures

- The four OpenAI request halves were re-encoded after M10 to add the
  deterministic `prompt_cache_key` (and a matching `content-length`); their
  recorded responses are untouched. The Anthropic request half is unchanged.
- The Responses decoder now warns once per stream when a reasoning item arrives
  without `encrypted_content`, naming the turn as not statelessly replayable.
- Recorder redaction covers bodies too: bearer tokens, `sk-` keys, `AIza` keys
  and PEM private key blocks, on both the request and the response half.

## Still open

- Google capture conformance remains M11 (O-008). OpenAI- and
  Anthropic-endpoint-specific captures are out of scope, not deferred: M8 was
  re-scoped to these NeuralWatt recordings, which cover all three implemented
  protocols. The borrowed `.sse` files under `tests/fixtures/recorded/` are
  decoder conformance fixtures and are not first-party captures.
