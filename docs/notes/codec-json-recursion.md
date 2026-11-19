# Wire codecs: bounded JSON nesting

Scope: `crates/keel-llm-protocol/src/{openai_responses,openai_chat,anthropic_messages}.rs`
plus the shared helpers in `src/protocol.rs`. No wire-format changes; every
guard runs before existing encoding/parsing.

## Root cause

All three codecs trusted JSON nesting depth in both directions.

- **Encode.** Request bodies are assembled with `json!`, which converts
  interpolated `Value`s through serde's recursive `Serialize` path. A
  `ToolSpec.input_schema` nested 10,000 levels deep recursed once per level
  while lowering the body and aborted the process with a stack overflow.
  Anthropic additionally re-parsed each assistant tool call's `arguments`
  string into a `Value` before embedding it; Chat and Responses passed the
  string through (Chat did not parse it at all; both only used serde_json on
  it for validation/warnings). None of the codecs bounded nesting first.
- **Decode.** Each decoder passed the whole SSE payload to
  `serde_json::from_str` and mapped any failure to a generic
  `LlmError::decode("<codec>: invalid frame: {error}")`. serde_json's own
  128-container parser limit prevented a stack overflow, but the result was
  the raw serde string (`recursion limit exceeded at line 1 column ...`) with
  no documented limit the caller could see or test against.

## Fix: shared helpers, not duplicated logic

The depth logic lives once in `crates/keel-llm-protocol/src/protocol.rs` and
is used by all three codecs:

- `pub(crate) const MAX_JSON_DEPTH: usize = 64` (doc-commented).
- `parse_frame(codec, data)`: pre-scans each frame with `raw_json_depth`; over
  the limit it returns `LlmError::decode` naming the measured depth and the
  limit. If serde_json still rejects for recursion, its error text is mapped
  to a typed message naming `MAX_JSON_DEPTH` instead of the raw serde string.
  This is a backstop; the 64-level pre-scan fires first.
- `check_value_depth(codec, value, path)`: iterative, explicit-stack walk over
  a `serde_json::Value`; no recursion, so it cannot overflow while measuring.
  Returns `ErrorKind::InvalidRequest` naming the path and the limit.
- `check_json_text_depth(codec, text, path)` + private `raw_json_depth`: a
  byte scanner that counts `{`/`[` outside JSON strings (escape-aware), used
  before serde_json parses tool-argument text.
- `depth_error(codec, path)`: builds the shared `InvalidRequest` message.

All three codecs call them at the same points:

| Codec | schema guard (`check_value_depth`) | argument guard (`check_json_text_depth`) | frame guard (`parse_frame`) |
| --- | --- | --- | --- |
| OpenAI Responses | `tools[i].input_schema` before the tools `json!` | `messages[i].parts[j].arguments` in assistant tool calls | whole SSE frame |
| OpenAI Chat | `tools[i].input_schema` before the tools `json!` | `messages[i].parts[j].arguments` in assistant tool calls | whole SSE frame |
| Anthropic Messages | `tools[i].input_schema` before the tools `json!` | `messages[i].parts[j].arguments` before its `from_str`/`input` lowering | whole SSE frame |

Paths in errors are the canonical request paths, prefixed by the codec name
(`openai-responses: ...`, `openai-chat: ...`, `anthropic-messages: ...`).
Encode failures are `ErrorKind::InvalidRequest`; decode failures are
`ErrorKind::Decode` (the repository's typed decode category; there is no
`InvalidResponse` variant, and `keel-llm-types` was out of scope).

## Limit choice

64 containers.

- Real tool schemas are rarely deeper than about ten; 64 covers elaborate
  `anyOf`/`oneOf` trees with headroom.
- It is below serde_json's 128-container parser limit, so our typed error
  always fires before serde_json's generic one, in both directions.
- Recursive `Value` walks at 64 levels (clone, drop, serialization) are trivial
  on an 8 MiB default stack. The pre-fix failure at 10,000 levels is far past
  this.
- Anything at or below 64 encodes and decodes exactly as before. The guard is
  a pre-check, not a transformation, and the `enumerate()` changes only track
  indices for error paths.

## Tests added

All are `#[cfg(test)] mod tests` inside the codec files; 5 per codec, 15 total.

OpenAI Responses (`src/openai_responses.rs`):
1. `encode_rejects_deeply_nested_tool_schema` — 10,000-level `Value`.
2. `encode_rejects_deeply_nested_tool_arguments` — 10,000-level JSON string.
3. `encode_accepts_nesting_at_the_limit` — schema at exactly 64.
4. `decode_rejects_deeply_nested_frame` — 10,000-level frame.
5. `decode_accepts_nesting_at_the_limit` — frame depth exactly 64.

OpenAI Chat (`src/openai_chat.rs`): the same five names, adapted to Chat's
`function.parameters` body shape and Chat frames.

Anthropic Messages (`src/anthropic_messages.rs`): the same five names, adapted
to Anthropic's `input_schema` body shape and `message_start` frames.

Encode rejection tests assert `ErrorKind::InvalidRequest`, the canonical path,
and the limit in the message. Decode rejection tests assert
`ErrorKind::Decode`, the measured-depth/limit message, and no raw serde-only
message. The deep `Value` encode tests leak the request with
`std::mem::forget` after `encode` because dropping a `Value` this deep is
itself recursive; the leak is test-local and deliberate.

## Pre-fix reproduction

Run before the guards were wired in:

- Responses encode schema: `thread ... has overflowed its stack` /
  `fatal runtime error: stack overflow, aborting` / `signal: 6, SIGABRT`.
  Re-running with only the guard removed reproduced the same abort inside
  `encode`.
- Chat encode schema: same stack overflow and SIGABRT.
- Anthropic encode schema: same stack overflow and SIGABRT.
- Chat encode arguments: `encode` returned `Ok` (the deep string passed
  through untouched); the test failed at `expect_err`.
- Anthropic encode arguments: `encode` returned `Ok` with the warning
  `tool call_1 arguments were invalid JSON (recursion limit exceeded at line 1
  column 636); sent as an empty object`; the test failed at `expect_err`.
- Decode rejection, pre-guard:
  - `openai-chat: invalid frame: recursion limit exceeded at line 1 column 648`
  - `anthropic-messages: invalid frame: recursion limit exceeded at line 1
    column 653`
  Both failed the limit/nesting-depth assertions because the message was
  serde-only.
- Post-fix, the same decode tests report messages such as
  `openai-chat: frame JSON nesting depth 10001 exceeds the limit of 64`.

All guards were restored; no temporary markers remain.

## Validation

```
cargo test -p keel-llm-protocol
  lib (3 codec modules): 15 passed, 0 failed
  tests/anthropic:       10 passed, 0 failed
  tests/openai_chat:     10 passed, 0 failed
  tests/requests:         3 passed, 0 failed  (golden bodies unchanged)
  doc-tests:              0
  total:                 38 passed, 0 failed

cargo test -p keel-llm-route --test cassettes
  5 passed, 0 failed  (recorded Responses and Chat turns replay unchanged)

cargo clippy -p keel-llm-protocol --all-targets -- -D warnings
  Finished `dev` profile; no warnings

rustfmt --edition 2024 --check on protocol.rs and the three codec files
  clean
```

## Remaining risks

- **Error taxonomy gap.** The taxonomy in `keel-llm-types` has
  `ErrorKind::Decode` but no `InvalidResponse`; this change uses `Decode`
  rather than editing another crate. A dedicated variant would be a
  cross-crate API change.
- **String-matched serde backstop.** The `recursion limit exceeded` check in
  `parse_frame` depends on serde_json's Display text. It is only a backstop:
  the 64-level pre-scan fires first regardless.
- **Whole-frame budget.** A frame is limited as a whole; a 64-deep legitimate
  event payload would be rejected. No known event from these providers nests
  that deeply (nested tool arguments arrive as strings and are not counted by
  the frame scan).
- **Encode guard placement.** Tool schemas are checked only when
  `compat.supports_tools` is true. If tools are unsupported they are discarded,
  so no recursive walk occurs; wire output is unaffected. Tool-argument checks
  run whenever the assistant turn contains a tool call, before any compat
  filtering.
- **Breadth is unbounded.** The guard bounds depth only; very wide but shallow
  JSON still costs time and memory proportional to its size, as before.
- **Reasoning signatures.** Responses parses a reasoning `signature` string to
  inspect `encrypted_content`; a pathologically nested signature fails
  serde_json's recursion check and is then treated as an opaque string. It is
  not a recursive `Value` walk and is not covered by these guards.
