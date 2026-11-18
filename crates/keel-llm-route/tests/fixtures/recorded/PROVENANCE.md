# Recorded wire fixtures

These files are raw Server-Sent Event bodies captured from real provider
endpoints. They were not written by hand. keel replays them through its own SSE
framing and protocol decoders in `tests/recorded.rs`.

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

The Responses recordings are kept for the phase-2 protocol module; they are not
decoded yet.

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
