# Recorded wire fixtures

These files are raw Server-Sent Event bodies captured from real provider
endpoints and copied from opencode's MIT-licensed recordings. They are decoder
conformance fixtures, not first-party keel captures, and they are not the M8
deliverable; the first-party captures are the NeuralWatt cassettes listed under
"Live captures" below. They were not written by hand. keel replays them through
its own SSE framing and protocol decoders in `tests/recorded.rs`, and one of
them is also the response half of a cassette in `tests/fixtures/cassettes/`.

## Source

- Repository: https://github.com/sst/opencode
- Commit: `a9a35b4b4ce35573d71207e246e770cfa26ff0e1`
- Path: `packages/ai/test/fixtures/recordings/<original>.json`, field
  `interactions[0].response.body`
- License: MIT, Copyright (c) 2025 opencode (full text below)

Only the response body is copied. Request bodies, prompts, headers and
credentials from the originals are not included.

## Files

| local | original | endpoint |
|---|---|---|
| `openai-chat/azure-text.sse` | `azure/chat-streams-text.json` | Azure OpenAI, Chat Completions |
| `openai-chat/groq-parallel-tool-calls.sse` | `groq-chat/continues-qwen-parallel-tool-calls.json` | Groq, Chat Completions |
| `openai-chat/gemini-parallel-tool-calls.sse` | `openai-compatible-chat/gemini-parallel-tool-signatures.json` | Gemini OpenAI-compatible endpoint |
| `openai-responses/gpt-5-5-reasoning.sse` | `openai-responses/openai-responses-gpt-5-5-reasoning.json` | OpenAI Responses |
| `openai-responses/gpt-5-5-tool-loop.sse` | `openai-responses/openai-responses-gpt-5-5-tool-loop.json` | OpenAI Responses |
| `openai-responses/gpt-5-5-image-tool-result.sse` | `openai-responses/openai-responses-gpt-5-5-image-tool-result.json` | OpenAI Responses |
| `openai-responses/gpt-5-5-reasoning-continuation.sse` | `openai-responses/openai-responses-gpt-5-5-reasoning-continuation.json` | OpenAI Responses |

`gpt-5-5-reasoning.sse` is the response half of
`tests/fixtures/cassettes/openai-responses/gpt-5-5-reasoning.json`; the request
half of that cassette was encoded by keel, not captured.

## Live captures

The files under `tests/fixtures/cassettes/neuralwatt/` are not copied from
opencode. The `record_live` test recorded each whole exchange through keel's
loopback recorder on 2026-11-18 against NeuralWatt's endpoints (`nw-small`), so
both halves are real: the body keel encoded and the SSE stream the provider
sent, comment lines and `energy` trailer included. The recorder redacted the
`authorization` header, so no key is in any file. These five captures are the
M8 acceptance evidence for all three implemented protocols. OpenAI- and
Anthropic-endpoint-specific captures are out of scope; these are NeuralWatt
recordings of those protocols, not captures from either vendor's own endpoint.
Google captures remain M11 (O-008).

| cassette | endpoint | what it holds |
|---|---|---|
| `chat-text.json` | `/v1/chat/completions` | text turn, reasoning tokens in usage |
| `chat-tool-call.json` | `/v1/chat/completions` | tool schema out, one `get_weather` call back |
| `messages-text.json` | `/v1/messages` | Anthropic shape, thinking block with a signature |
| `responses-text.json` | `/v1/responses` | event-sourced text, `: keepalive` comments, no `[DONE]` |
| `responses-reasoning.json` | `/v1/responses` | reasoning effort asked for; the gateway sent a summary and no encrypted blob |

## License

MIT License

Copyright (c) 2025 opencode

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
