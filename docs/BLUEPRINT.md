# keel blueprint

Read this before touching code. It says what keel is, what holds it together,
what is done, what is next, and how a worker ships a change.

Sources: `README.md`, `docs/architecture.md`, `docs/decisions.md`,
`docs/notes/*.md`, the crate manifests, `crates/keel-evals/`,
`.github/workflows/ci.yml`, and `git log` up to `ce5ecf8` (the M1 merge,
2026-11-19), plus the M1 review fixes in the follow-up PRs.
Where this file guesses rather than reads, it says **(inferred)**. Where it and
the README disagree, the code wins; fix the README in the same PR.

The D-xxx / O-xxx ids are summarized in [`docs/decisions.md`](decisions.md)
(`D-001`..`D-010` and `O-001`..`O-013`), and [`docs/architecture.md`](architecture.md)
maps `D-001`..`D-010` to code ("Where the decisions land"). If an id's meaning
or an open choice is unclear, ask; do not guess.

---

## 1. Purpose and scope

keel is the provider seam and session loop for an LLM agent harness, in Rust. It
owns the wire: it encodes one canonical request into each provider protocol
(OpenAI Chat Completions, OpenAI Responses, Anthropic Messages; Google later),
decodes the streams back into canonical events, repairs transcripts so they
replay safely across models, and runs a tool loop (model turn, tool calls, one
result per call, next turn) inside a sandboxed workspace. It does this with no
third-party unified client (D-001), and it logs sessions as JSONL with resume.
The `keel` CLI is the first host; the crates are meant to be embedded.

### Non-goals (now)

- Model fallback (O-004 remainder; README says out of scope for now).
- A third-party unified LLM client, or vendor-specific clients. Quirks are data
  (`Compat`), not code paths per vendor.
- Network calls in CI or in the default test suite. Live recording is opt-in.
- Writing into the user's repository. Session logs live under the data dir.
- `unsafe` code (`unsafe_code = "forbid"` in the workspace lints).
- A UI beyond the CLI **(inferred)**: nothing in the repo plans one.

---

## 2. Architecture

### Crate map

| crate | role | may depend on |
|---|---|---|
| `keel-llm-types` | canonical messages, parts, provenance, events, errors, usage, cache policy, `repair_transcript` | nothing of keel's |
| `keel-llm-protocol` | one module per wire protocol: body lowering and stream state machines | types |
| `keel-llm-route` | `Route = Protocol + Endpoint + Auth + Framing`, HTTP, SSE, retry classification | types, protocol |
| `keel-llm-catalog` | providers and per-model protocol assignments; `catalog/seed.toml` today | types |
| `keel-tools` | `Tool` trait, registry, permission gate, workspace sandbox, built-ins | types |
| `keel-testkit` | dev-only: scripted HTTP/SSE server, faux provider, cassette player and recorder | nothing of keel's |
| `keel-core` | route resolution, event assembly, `Session`, `Harness` loop, session log | all of the above |
| `keel-evals` | dev-only: Python fixture, 12 tasks, offline cassette replay, budget-capped live runner | core, catalog, route, tools, testkit, types |
| `keel-cli` | the `keel` binary | core, catalog, route, tools, types |

**Dependency rule (one-way).** Dependencies point down. "No provider type
reaches the session loop, and no HTTP concern reaches the types crate." A
protocol module cannot see the catalog, the session, or a provider name.
`keel-tools` does not know a model exists. `keel-testkit` and `keel-evals`
are dev-only.
Enforced by the `[dependencies]` in each crate's `Cargo.toml` and by review.
There is no automated test, by decision (section 6, decision 10). Adding an edge that
breaks this table needs Arndev's sign-off.

### Invariants

The README lists **nine** invariants (not six). Verbatim, with where each is held:

| # | invariant (README) | enforced in | pinned by (tests) |
|---|---|---|---|
| 1 | Provenance lives on the part, never reconstructed from a destination namespace. | `Source` on `Part` (`keel-llm-types/src/content.rs`); session stamps it from the target (`keel-core/src/session.rs`) | `repair.rs`: `keeps_signed_reasoning_from_another_model_of_the_same_protocol`, `drops_foreign_unsigned_reasoning` |
| 2 | Transcript repair runs before every request and is pure. | `repair_transcript` (`keel-llm-types/src/repair.rs`), called on the preview in `session.rs` before each request | `repair.rs`: `repair_is_deterministic_when_it_synthesizes_turns` and the other 12 |
| 3 | Tool arguments are raw text until one authoritative end event. | event contract (`ToolInputDelta` progress, `ToolInputEnd`/`ToolCall` authoritative) in decoders and `keel-core/src/assemble.rs`; tools parse their own args (`keel-tools/src/tool.rs`) | `recorded.rs`: `compatible_tool_calls_without_an_index_do_not_merge`, `groq_parallel_tool_calls_stay_separate` |
| 4 | Every stream has a terminal `Done`; errors carry partial content and usage. | protocol decoders, `EventStream` in `keel-llm-route/src/client.rs`, `Assembler` | `http.rs`: `a_stream_without_a_done_frame_still_terminates`; `faux_provider.rs`: `a_truncated_stream_is_closed_with_a_warning`; `session.rs`: `a_mid_stream_failure_keeps_partial_output` |
| 5 | Retry at the session boundary, with one narrow pre-stream exception. | `Client::stream_with_retry` (pre-stream), `RetryPolicy::decide` (`keel-llm-route/src/retry.rs`), `Harness` (`keel-core/src/run.rs`) | `faux_provider.rs`: `retries_a_429_before_the_stream_opens`; `run.rs`: `a_retryable_failure_replays_the_turn_without_duplicating_input`, `a_decode_failure_is_not_retried`, `retries_stop_at_max_attempts` |
| 6 | Per-protocol quirks are data (`Compat`), not vendor-specific clients. | `Compat` (`keel-llm-types/src/compat.rs`), per-model `compat` overrides in the catalog | golden requests (`keel-llm-protocol/tests/requests.rs`) |
| 7 | Capabilities and protocol are properties of the model entry. | `seed.toml` (`protocol`, `capabilities` per model); catalog maps capabilities into `Compat` (`keel-llm-catalog/src/lib.rs`) | catalog unit tests |
| 8 | Every tool call gets exactly one result, including denied, unknown, timed-out and cancelled calls. | `Harness` (`run.rs`, incl. `cancelled before execution`); repair step 7 synthesizes missing results | `run.rs`: `a_denied_tool_...`, `an_unknown_tool_...`, `a_timed_out_tool_...`, `cancellation_during_a_tool_leaves_a_complete_transcript` |
| 9 | Tool execution is deny-by-default for embedders; the CLI opts into asking or `--yes`. | `Harness::new` sets `PermissionGate::DenyAll` (`run.rs`); CLI uses `Ask` unless `--yes` | `run.rs`: `a_denied_tool_returns_an_error_result_without_running` |

Two more properties from `docs/architecture.md` that workers must keep: history
is committed only after the stream opens, and **the raw history is never
rewritten**: repair produces a per-request view, so failed turns stay
inspectable.

---

## 3. Current state

Core implementation through `f042b90` is on `main`. M0 aligned the
repository docs and moved the engineering notes under `docs/notes/`. M1 merged
as `ce5ecf8` (M1 work through `02a25c5`) and adds the behavioral eval suite
(243 passing tests, 12 replay tasks; two live tests ignored); the `m1-fixes` and
`m1-eval-safety` follow-ups address the review findings without a new live run.
M2 lands as `72dc3f4`: structured tool descriptions and actionable failure
text, with the eval cassettes refreshed from a live run and replay still 12/12.
M3 lands as `cd112cc`: the default prompt is embedded in `keel-core` and
`--system` appends to it, with the eval cassettes re-recorded live at 12/12 for
$0.039297.

| area | commit(s) | evidence |
|---|---|---|
| Workspace, canonical types, repair, Chat + Anthropic Messages, route, seed catalog, CLI | `dbf481b` | `keel-llm-types/tests/repair.rs` (13), `tests/anthropic_messages.rs`, `tests/openai_chat.rs` |
| Tools, sandbox, permission gate, `Harness` loop (D-007) | `53bd2aa` | `keel-tools/tests/builtin.rs` (13), `keel-core/tests/run.rs` |
| Faux provider, golden requests, recorded wire fixtures | `80cf61f` | `tests/requests.rs`, `fixtures/requests/*.json`, `recorded.rs`, `faux_provider.rs` |
| OpenAI Responses protocol | `fdf7af6` | `responses_text_turn_keeps_reasoning_replayable`, `responses_tool_call_is_addressed_by_call_id` |
| Cassette player, recorder, `scenarios.toml` manifest | `e9fa94a` | `a_changed_request_does_not_replay`, `every_cassette_is_covered_by_the_manifest` |
| Bounded JSON nesting (64) in all three codecs | `67aa022` | 15 unit tests in codec files; `docs/notes/codec-json-recursion.md` |
| NeuralWatt in catalog, gated live recorder, five live cassettes, body redaction, unsigned-reasoning warning | `102d2c4`, `cbcedc1`, `3f9056d`, `107aad4` | `the_live_neuralwatt_*_decodes`, `a_gateway_without_encrypted_reasoning_still_decodes`; `docs/notes/neuralwatt-live-captures.md` |
| JSONL session log and `--resume` | `3f8dee7` | `session.rs`: `a_sink_sees_every_message_that_enters_history`, `a_session_log_holds_the_transcript_of_a_turn`; `run.rs`: `a_session_log_holds_tool_results_in_order` |
| Turn retry at the session boundary | `f042b90` | `run.rs`: four retry tests listed above |
| Repo hygiene, blueprint, decision summaries, doc-drift alignment | M0 | `docs/BLUEPRINT.md`, `docs/decisions.md`, `docs/notes/`, `README.md`, `docs/architecture.md` |
| Behavioral eval suite: Python fixture, 12 tasks, ordered cassette replay, budget-capped live recorder | `ce5ecf8`, plus `m1-fixes` and `m1-eval-safety` | `keel-evals` 49 passing tests plus one ignored live test, including `replays_*` (12), `the_budget_gate_stops_before_the_first_request_when_the_cap_is_one_micro_short`, `the_suite_marks_the_next_task_not_run_after_a_real_budget_stop`, `a_failed_mid_stream_turn_is_reported_failed_and_does_not_replace`, `commit_cassette_persists_a_clean_replay`, `commit_cassette_rejects_a_replay_whose_check_fails`, `an_extra_cassette_exchange_fails_the_replay`, `a_grandchild_holding_stdout_does_not_block_a_finished_check`, `every_task_grades_with_its_own_check_script`, `every_recorded_shell_command_passes_the_guard`, `checks_bite`; `crates/keel-evals/reports/live.md` |
| Structured tool descriptions and actionable failure text | `72dc3f4` | `keel-tools/tests/builtin.rs` (30 tests, exact error text per failure path), `keel-evals/tests/tool_text.rs` (cassette tool-text drift gate), description-only golden diff, live 12/12 at $0.034032 |
| Default system prompt in `keel-core`, `--system` appends, stored-header resume | `cd112cc` | `prompt.rs` unit tests, `keel-core/tests/session.rs` captured-request tests, `keel-cli/tests/chat.rs` header and resume tests, system-text-only golden diff, live 12/12 at $0.039297 |

What exists for the agent's behavior, as opposed to the wire:

- Behavioral evals: twelve coding tasks over a Python fixture, offline replay
  from ordered multi-turn cassettes, and a live recorder under a $10 cap. The
  baseline is 12/12 passing (see `crates/keel-evals/reports/live.md`).
- Default system prompt: `keel-core/src/prompts/default_system.md`, embedded in
  `prompt.rs` and applied by `Session::new`. `with_system` and `--system` append
  after it; resume uses the header prompt as-is, and a legacy `system: None`
  header stays prompt-less (`keel-cli/src/main.rs`).
- Tool descriptions are structured (`keel-tools/src/builtin/*.rs`): each says
  when to use the tool, which tool to prefer (`search` vs `glob` vs `bash`),
  what it returns, and what a failure looks like. Failure results name the value
  that caused the error and the next step, and the text is pinned in
  `keel-tools/tests/builtin.rs`.
- File tools open descriptor-relatively on Unix (`openat` from the root
  descriptor, `O_NOFOLLOW` per component), so a symlink swapped after a lookup
  cannot escape the workspace. A symlink is refused with the requested path and
  a `bash cat` escape hatch. `bash`, `glob` and `search` still resolve a
  canonical path for their child process; `bash` is cwd-scoped, not OS-confined,
  and non-Unix keeps the canonicalizing fallback.
- Consecutive `parallel_safe` tool calls run concurrently in bounded,
  order-preserving batches (`keel-core/src/run.rs`); writes, edits and `bash`
  are barriers. Results and finish events stay in call order, and cancellation
  still leaves one result per call.
- `bash` confinement beyond the working directory, the non-Unix file-tool
  fallback, and atomic `write`/`edit` replacement remain gaps (O-010 later).
  Writes truncate in place, so a failure after truncation can leave a partial
  file; temp-file-plus-rename is deferred because it would change mode and
  hard-link semantics.

---

## 4. Roadmap

Order is settled (section 6): harness quality first (M1-M4), then the
architecture doc's "Next steps" minus Antigravity, with MCP split out, and
Antigravity last. Milestones with no dependency on each other can run in
parallel, but take them in number order when choosing.

| # | milestone | ids | depends on |
|---|---|---|---|
| M0 | Repo hygiene, doc drift, decision summaries *(done)* | D-001..D-010, O-001..O-013 | - |
| M1 | Behavioral eval suite (`keel-evals`) *(done)* | D-008, D-010 | M0 |
| M2 | Tool description and error-text quality *(done)* | - | M1 |
| M3 | Default system prompt *(done — `cd112cc`)* | - | M1 |
| M4 | Session compaction *(done — `f464835`)* | O-009, O-004 | M1, M3 |
| M5 | Credential store *(done — `bd00353`)* | O-005 | - |
| M6 | OS sandbox and parallel tool execution *(done — `c69a2d4`)* | O-010 | - |
| M7 | MCP tools | O-010 (split out) | M6 |
| M8 | First-party vendor captures (OpenAI, Anthropic) | O-008 | - |
| M9 | Generated catalog | O-006 | - |
| M10 | Full cache policy | O-003 | - |
| M11 | Google/Antigravity | D-005, O-012 | M5, and every milestone above |

### M0. Repo hygiene, doc drift, decision summaries *(done)*
- **Goal:** the docs a worker reads match the code, and the D-/O- ids are readable in-repo.
- **Scope:**
  - Move the root notes folder to `docs/notes/` (committing the two notes) and
    fix every reference.
  - Add `docs/decisions.md`: one concise summary per id for `D-001`..`D-010`
    and `O-001`..`O-013`, and add `D-008`..`D-010` to `docs/architecture.md`
    ("Where the decisions land").
  - Fix drift: README says seven providers (seed has eight); README config
    table lacks `NEURALWATT_API_KEY`; README lacks `UPDATE_CASSETTE=1`;
    README "Implemented" omits bounded JSON nesting, session log/resume, and
    turn retry; README says only the Chat decoder replays recorded streams
    (Responses does too); architecture "Testing" says three layers and lists
    four.
  - Update architecture "Next steps" to the order in this section.
- **Out of scope:** behavior changes; a dependency-direction test (decision 10: review rule only).
- **Accept:** no root notes directory remains; every notes path points under
  `docs/notes/`; `docs/decisions.md` lists every id the repo references with
  none invented; README agrees with `seed.toml`, `main.rs` usage text and test
  layout; CI green.
- **Touches:** `README.md`, `docs/BLUEPRINT.md`, `docs/architecture.md`, `docs/decisions.md`, `docs/notes/`.
- **Depends on:** nothing.

### M1. Behavioral eval suite *(done — `ce5ecf8`; review fixes on `m1-fixes` and `m1-eval-safety`)*
- **Goal:** a pass/fail baseline for "does an agent on keel finish real coding tasks".
- **Result:** 12/12 replay and live tasks pass; the final live run spent
  $0.027273 (two recording runs total $0.056289); the report is
  `crates/keel-evals/reports/live.md`. The review follow-ups fix the production
  budget test, extra-exchange failure, cassette replacement, check deadline and
  reader joins, workspace normalization, bytecode filtering, single-source
  checks, and the stop classification: only `EndTurn` or `MaxTurns` with a
  passing check can pass or replace a cassette; `Failed` and `Cancelled` cannot.
  No follow-up recorded live or spent anything.
- **Scope:** a new crate `crates/keel-evals` (name proposed), dev/test-only,
  added to the workspace and the crate table. It holds a small fixture repo,
  10-15 tasks (prompt, starting fixture state, one mechanical check script that
  exits 0), and a runner that executes each task in a temp copy of the fixture
  with all tools allowed. The runner drives `Harness` directly as a library, not
  the `keel chat` binary. Two modes:
  - **replay:** offline from recorded multi-turn cassettes (one cassette holds
    every exchange of one task run, in order), runs in `cargo test`.
  - **live:** `#[ignore]`d, opt-in env var, refused when `CI` is set; records
    with NeuralWatt `deepseek-v4.1-flash`, writes cassettes and a per-task
    report. Hard budget: **$10 per live run** (all tasks together). Before each
    model request the runner checks spend so far plus that request's
    worst-case cost; if it would pass $10 it stops, records the remaining
    tasks as `not run (budget)`, and writes the report.
  - **Determinism:** one cassette per task holds the whole session; the player
    consumes interactions in order, once each, and an unused exchange fails the
    replay; tool output replaces both spellings of the temp root with
    `<workspace>`, sorts `search`, and filters `__pycache__`/`.pyc` from
    `glob`/`search`; the eval shell rejects `find`/`ls`/`stat`/`du`/`tree`,
    whose output is machine-dependent, and the prompt points at `glob`. All
    tools stay registered and the permission gate is `AllowAll`. The guard is
    token-based, not a sandbox. A check command has a 30-second deadline and
    bounded output, so a hanging check fails the task; reader joins are bounded
    when a grandchild keeps a pipe open.
  - **Clean stop:** a task passes only when the model loop stops with `EndTurn`
    or `MaxTurns` and the check passes. `MaxTurns` is allowed because the
    format-table baseline deliberately ends on `max_turns` with a passing check;
    `Failed`, `Cancelled`, a missing stop, or any harness error fail the task.
  - **Live cassette safety:** the recorder writes to a temp file, the runner
    replays it from a fresh fixture, and only a clean pass moves it over the
    committed baseline. An error, failed check, `Failed`, `Cancelled`, or
    budget stop leaves the baseline untouched.
  - Cost source is a worker design choice. Per-token prices kept in
    `keel-evals` are the straightforward option because `SseDecoder` strips
    NeuralWatt's `: cost {...}` SSE comments before protocol decoders see them
    (`docs/notes/neuralwatt-live-captures.md`), and the catalog carries no
    prices. No cost source, no live run.
- **Out of scope:** changing prompts or tools to raise the score (M2, M3); LLM-as-judge grading.
- **Accept:**
  - `cargo test --workspace` runs every replay task with no network and passes.
  - Live mode refuses to run without its opt-in and never runs under `CI`.
  - Each check is proven to bite: a test runs it on the untouched fixture and asserts it fails.
  - Report per task: pass/fail, turns, tool calls, tokens, stop reason.
  - A changed request on replay is a cassette miss, as today; a multi-turn
    cassette replays its exchanges in order and a missing or extra exchange fails.
  - Budget test: with a fake cost source and a tiny cap, the runner stops before
    the request that would cross it and the report says which tasks did not run.
- **Touches:** `crates/keel-evals` (new), root `Cargo.toml`, `keel-testkit`
  (`CassetteServer` now advances a sequential cursor; a miss is HTTP 422),
  the `scenarios.toml` coverage rule, README crate table, and
  `.github/workflows/ci.yml` (installs `rg` on macOS).
- **Depends on:** M0 (crate table and docs settled). M2, M3, M4 depend on M1.

### M2. Tool description and error-text quality *(done — `72dc3f4`)*
- **Goal:** tool text tells the model when to use each tool and what to do after a failure.
- **Result:** all six built-in descriptions now use `Use when` / `Prefer` /
  `Returns` / `On failure` lines, and every failure path named below returns
  text that names the value and the next step. The golden request bodies
  changed only in the `read` description (three files, one line each). After
  `m1-eval-safety` merged, the 12 eval cassettes and `reports/live.md` were
  re-recorded live: 12/12 passed and the run spent $0.034032, against the M1
  baseline of 12/12 at $0.027273. Replay is 12/12. Timeouts stay a typed
  `ToolError::Timeout` (now carrying a message), and the hint suggests raising
  `timeout_ms` only when the request is below the harness cap.
  `crates/keel-evals/tests/tool_text.rs` still gates request-half drift, and its
  result migration covers the two historical result texts; a future
  result-text change needs its own migration or a live re-record. Workspace
  suite: 264 tests, 2 ignored.
- **Scope:** rewrite `spec()` descriptions in `keel-tools/src/builtin/*.rs` with
  when-to-use / when-not guidance (`search` vs `glob` vs `bash`); make every
  error result actionable (what failed, the value that caused it, the next
  step; e.g. `edit` with 0 or N matches says which and suggests `read` or
  `replace_all`; a path outside the workspace names the root).
- **Out of scope:** new tools; schema changes beyond description text.
- **Accept:** `builtin_specs_are_consistent` passes; new tests pin the error
  text of each failure path; golden bodies regenerated with `UPDATE_GOLDEN=1`
  and the diff is description text only; affected cassettes regenerated; M1
  score before and after in the PR.
- **Touches:** `keel-tools`, `keel-llm-protocol/tests/fixtures/requests/`, cassettes, `keel-evals` cassettes.
- **Depends on:** M1.

### M3. Default system prompt *(done — `cd112cc`)*
- **Goal:** keel ships a default agent prompt instead of relying on model defaults.
- **Result:** `keel-core/src/prompts/default_system.md` is embedded as
  `DEFAULT_SYSTEM_PROMPT` in `prompt.rs`. `Session::new` starts with it;
  `Session::with_system` and `--system TEXT` append after a horizontal rule
  instead of replacing it, and `effective_system_prompt` is the pure function
  behind every case. A new `keel chat` session header records the effective
  text; resume loads that stored text before any append, and a legacy
  `system: None` header stays prompt-less so `--system` there applies alone.
  The protocol goldens changed in the system text only. All 12 eval cassettes
  were re-recorded live: 12/12 passed at $0.039297, against the M2 baseline of
  12/12 at $0.034032; replay is 12/12. Workspace suite: 281 tests, 2 ignored.
- **Scope:** one prompt file in `keel-core`, embedded at compile time, used by
  `Session`/`Harness` so embedders get it too. `--system TEXT` **appends** to
  the default; it does not replace it. One prompt for all models. Recorded in
  the session header as today; resuming a session uses the stored header prompt
  as-is without prepending the default a second time.
- **Out of scope:** per-model variants (not for now); templating.
- **Accept:** a request with no `--system` carries the default (captured-request
  or golden test); with `--system X` the system text is the default followed
  by `X` (test); resume keeps the recorded system prompt without duplicating
  the default; M1 score before and after in the PR.
- **Touches:** `keel-core`, `keel-cli` (help text, append semantics), golden files, cassettes.
- **Depends on:** M1. Separate PR from M2 so each eval delta is attributable.

### M4. Session compaction (O-009 remainder) *(done — `f464835`)*
- **Goal:** a session keeps going when it reaches the model's context window.
- **Result:** compaction runs in `Session::step_streaming`. The threshold check is
  exactly `last_reported_input + last_reported_output + next_output_budget >=
  context_window`, where the budget is `max_output_tokens` or the catalog output
  limit carried on the `Route`; with no reported usage (the first request, or a
  compacted resume whose view has no assistant usage) it uses the conservative
  estimator. One threshold compaction runs per step. A provider
  `ContextOverflow` compacts once and replays once, pre-stream or mid-stream;
  the typed error is also produced from realistic HTTP 400/413/422 wording and
  from in-stream OpenAI Chat, OpenAI Responses and Anthropic error frames
  ("prompt is too long", "input is too long", "reduce the length of the
  messages", `context_length_exceeded`). A second overflow is a normal failure
  and user input is never sent twice. The summarizer is the session's own route
  and model with `ToolChoice::None` and no tools; it receives the repaired
  committed history plus `crates/keel-core/src/prompts/compaction.md`. The
  summary request is checked against the window and fails with `cannot compact:
  ...` rather than being sent when fewer than 128 output tokens would remain; a
  summary stopped at `MaxTokens` fails with `cannot compact: the summary was
  truncated`. The replacement view always keeps the newest plain user message
  verbatim, even over the `min(20_000, window/4)` user budget, then adds older
  ones that fit, in order, then one user message starting with
  `crates/keel-core/src/prompts/compaction_prefix.md`; no assistant turn, tool
  call or tool result crosses the cut, and a prior handoff summary is dropped.
  The view is then checked as the next request (system + view + pending input +
  tools + next output budget); if it cannot fit, compaction fails clearly
  instead of dropping the request. Logs are version 2: one appended
  `{"type":"compaction",...}` record holds the reason, summary and full
  replacement view, earlier bytes are unchanged, version-1 logs load, and
  resume applies the latest record plus the messages after it. Version-2
  headers record `max_output_tokens` and `temperature` so a resumed compacted
  session sends the same request. The CLI prints one `[compact]` stderr line
  per compaction (a threshold compaction followed by an overflow compaction
  prints two), warns when a route has no catalog window, and `keel route` shows
  the window and output limit. Summary usage is reported as
  `RunOutcome::compaction_usage`, printed on the final usage line, and charged
  by the eval budget, without inflating the trigger. The token estimator is one
  token per three bytes plus fixed message, part, tool and image overheads
  (`keel-core/src/tokens.rs`). No live run was needed; cassettes and goldens are
  unchanged and the 12 M1 replay tasks pass. 330 tests pass (16 compaction
  integration tests plus estimator, view, store, harness, CLI, eval-budget and
  protocol-classifier coverage); clippy and fmt are clean.
- **Reference:** OpenAI, per Arndev. What was checked (2026-11-19):
  - Responses API ([compaction guide](https://developers.openai.com/api/docs/guides/compaction),
    [`/responses/compact` reference](https://developers.openai.com/api/reference/resources/responses/methods/compact)):
    server-side mode compacts when the rendered token count crosses a
    `compact_threshold`; the standalone endpoint takes the full window (which
    must still fit the model's context) and returns a new window: the user
    messages followed by one `compaction` item whose `encrypted_content` is
    opaque. The returned window is passed back as-is, not pruned.
  - Codex CLI ([`compact.rs`](https://github.com/openai/codex/blob/main/codex-rs/core/src/compact.rs),
    [`context_window.rs`](https://github.com/openai/codex/blob/main/codex-rs/core/src/session/context_window.rs),
    [prompt](https://github.com/openai/codex/blob/main/codex-rs/prompts/templates/compact/prompt.md),
    [summary prefix](https://github.com/openai/codex/blob/main/codex-rs/prompts/templates/compact/summary_prefix.md),
    [config sample](https://learn.chatgpt.com/docs/config-file/config-sample.md)):
    auto-compacts when active context tokens reach `model_auto_compact_token_limit`
    (model default if unset) or a percentage of the model's full context
    window, which is a hard cap. The same session model is sent the history
    plus a summarization prompt ("CONTEXT CHECKPOINT COMPACTION ... handoff
    summary for another LLM": progress and decisions, constraints and user
    preferences, next steps, critical data). The new history is: the most
    recent user messages kept verbatim up to 20,000 tokens
    (`COMPACT_USER_MESSAGE_MAX_TOKENS`), then the summary as a user message
    starting with a fixed `SUMMARY_PREFIX`, with the initial context (system
    and environment) re-injected. Assistant turns and tool calls/results are
    not kept verbatim **(inferred from `build_compacted_history`; a
    retained-tool-output path exists, details unverified)**. The rollout log is
    JSONL and gets a `Compacted` record carrying the summary and the
    replacement history; resume rebuilds from the latest one
    ([`rollout_reconstruction.rs`](https://github.com/openai/codex/blob/main/codex-rs/core/src/session/rollout_reconstruction.rs);
    exact field set unverified).
- **Design guidance** (follow Codex's local, provider-neutral path; the
  encrypted `compaction` item only exists on OpenAI Responses):
  - **Trigger:** the session's model `context_window` from the catalog. Compact
    before the next request would not fit: last reported input + output tokens
    plus the next request's `max_output_tokens` reach the window. The request
    that asks for the summary must itself fit (OpenAI's rule), so compact
    while it still does. Also compact and retry once on a provider
    `ContextOverflow` error (today not retried).
  - **Summarizer:** the session's own route and model, no tools offered, a
    fixed checkpoint prompt modeled on Codex's (keel's own wording, in
    `keel-core` next to the M3 system prompt).
  - **New view:** system prompt, then the most recent user messages verbatim
    within a token budget, then one summary message with a fixed prefix
    marking it as a handoff. No tool call or result crosses the cut: keep a
    call and its result together or drop both, then run repair as usual.
  - **Log record (worker's design choice, not for Arndev):** append one
    `{"type":"compaction", ...}` record holding the summary message and the
    replacement view (or the indices it keeps), like Codex's `Compacted`
    record. Earlier `message` records stay in the file. Resume = latest
    compaction record + the messages after it. Bump the log `version` and keep
    reading version-1 logs.
- **Out of scope:** cross-machine sync (also O-009); OpenAI server-side or
  `/responses/compact` compaction (could be a later per-protocol option **(inferred)**);
  a user-facing `/compact` command.
- **Accept:**
  - Trigger: with a model whose catalog `context_window` is small (test catalog
    entry), the session compacts before the request that would exceed it,
    and not earlier than the rule above says.
  - A scripted `ContextOverflow` from the provider leads to one compaction and
    one retry, then a normal failure if it overflows again.
  - The summary request goes to the session's route and model (captured request).
  - The post-compaction request holds: system prompt, kept user messages
    verbatim, one summary message with the prefix; no tool result without its
    call and no call without its result; `repair_transcript` reports no
    synthesized results for it.
  - The JSONL file only grows: every byte before the compaction record is
    unchanged. Resuming a compacted session sends the same request the live
    session would have sent. A version-1 log still loads.
  - An M1 long-horizon task still passes.
- **Touches:** `keel-core` (session, store, run), maybe a marker type in
  `keel-llm-types` **(inferred)**, `keel-cli` (one status line when compaction runs).
- **Depends on:** M1, M3 (shares the prompt location).

### M5. Credential store (O-005) *(done — `bd00353`)*
- **Goal:** persistent, account-keyed credentials with single-flight refresh.
- **Scope:** a `SecretResolver` implementation behind the existing trait; `CredentialRef.store_key` gets used.
- **Out of scope:** OAuth flows (M11).
- **Result:** `StoreResolver` in `keel-llm-route::credentials` resolves the
  environment first and the store second; `build_route` sets `store_key` to the
  provider id from the catalog entry. The JSON store lives at the platform
  config path (`KEEL_CREDENTIALS` overrides it), writes 0600 in a 0700 directory
  on Unix, refuses a symlink at the path, follows a symlinked config directory
  without chmodding it, refuses non-regular files, replaces the file atomically,
  and never puts a value in errors or `Debug`. A snapshot miss performs one
  reload per generation; concurrent callers share it, and empty stored values
  are treated as absent. `keel auth set|remove|list` reads secrets from stdin or
  a no-echo prompt and lists keys only, and `keel chat` resolves through the
  store. Required auth reports a broken store; optional bearer auth treats it
  as absent. There is no TTL: OAuth token refresh remains M11. The suite is 370
  passed and 2 ignored.
- **Accept:** concurrent requests trigger one refresh (testkit test); values
  never appear in errors or logs; `EnvResolver` behavior unchanged.
- **Touches:** `keel-llm-route`, `keel-core`, `keel-cli`.
- **Depends on:** nothing.

### M6. OS sandbox and parallel tool execution (O-010 remainder, minus MCP) *(done — `c69a2d4`)*
- **Goal:** close the workspace TOCTOU gap and run `parallel_safe` tools concurrently.
- **Scope:** descriptor-relative opens (`openat`, `O_NOFOLLOW`) or an OS
  sandbox; parallel execution that keeps one result per call, in call order.
- **Out of scope:** MCP (M7).
- **Result:** On Unix, `read`, `write` and `edit` open paths component by
  component from an open root descriptor with `openat`/`O_NOFOLLOW`; writes
  create parents with `mkdirat`. Symlinks are refused, not followed, including
  inside the root; the refusal names the offending component and points at a
  quoted `bash cat` hint, and edit/write map permission and directory failures
  to their own next steps. The
  canonicalizing resolvers remain for `bash`, `glob` and `search`, which hand
  paths to child processes; `bash` stays cwd-scoped rather than OS-confined,
  and non-Unix keeps the fallback. The run loop runs
  consecutive `parallel_safe` calls concurrently in chunks of at most eight,
  with unsafe calls as barriers. Preparation is sequential, results and
  `ToolFinished` events are in call order, and cancellation still leaves one
  result per call. The symlink-swap test is a smoke test rather than a
  regression test for the old resolver; deterministic tests cover internal
  symlink refusal. A safe/unsafe/safe turn and a ten-call batch pin the barrier
  and chunk boundary. The suite is 387 passed and 2 ignored.
- **Accept:** symlink-swap smoke test; parallel test asserting result order and
  cancellation match the sequential loop; invariant 8 tests unchanged.
- **Touches:** `keel-tools`, `keel-core/src/run.rs`.
- **Depends on:** nothing.

### M7. MCP tools (split out of O-010)
- **Goal:** tools from MCP servers appear as ordinary `Tool`s.
- **Scope:** an MCP-backed `Tool` source registered in `ToolRegistry`; calls go
  through the same permission gate, timeout, cancellation and truncation.
- **Out of scope:** keel acting as an MCP server **(inferred)**.
- **Accept:** a scripted MCP server in tests; invariants 8 and 9 hold for MCP
  tools (denied, unknown, timed-out, cancelled each get one result); MCP
  errors are tool results, not harness errors.
- **Touches:** `keel-tools` (or a new crate; decide in the PR and keep the
  dependency rule), `keel-cli` config.
- **Depends on:** M6 (parallel execution semantics settled first).

### M8. First-party vendor captures (O-008)
- **Goal:** cassettes recorded from OpenAI and Anthropic endpoints, not borrowed bytes.
- **Scope:** OpenAI and Anthropic. Google captures move to M11.
- **Accept:** each listed in `scenarios.toml` with named decode tests;
  `PROVENANCE.md` updated; redaction verified.
- **Touches:** `keel-llm-route/tests/fixtures/`.
- **Depends on:** keys from Arndev.

### M9. Generated catalog (O-006)
- **Goal:** catalog generated (seed comment: from models.dev at build time) with a runtime overlay and last-known-good fallback.
- **Accept:** builds offline from the last-known-good file; overlay test; `keel models` output stable.
- **Touches:** `keel-llm-catalog`, a build script **(inferred)**, `keel-cli`.
- **Depends on:** nothing.

### M10. Full cache policy (O-003)
- **Goal:** `Auto` / `None` / `Object` cache policy; today `Auto` places up to two Anthropic breakpoints.
- **Accept:** golden bodies per policy per protocol.
- **Touches:** `keel-llm-types`, `keel-llm-protocol`.
- **Depends on:** O-003 open design choices settled with Arndev before scoping (see `docs/decisions.md`).

### M11. Google/Antigravity (D-005, O-012, phase 3) — last
- **Goal:** the fourth `ProtocolId` works end to end; today it is a typed "unsupported".
- **Scope:** Google protocol module, OAuth PKCE (tokens kept in the M5 store),
  project/model discovery, schema sanitizer, Google captures (rest of O-008).
- **Accept:** golden request body for the new protocol; decoder tests against
  recorded streams; a cassette in `scenarios.toml`; sanitizer tests on real
  tool schemas; `keel route antigravity/<model>` resolves; OAuth refresh uses
  M5's single-flight refresh.
- **Touches:** `keel-llm-protocol`, `keel-llm-route`, `keel-llm-catalog`, `keel-cli`.
- **Depends on:** M5 and all other milestones (last by decision), plus O-012 open design choices settled with Arndev before scoping (see `docs/decisions.md`).

---

## 5. Worker rules

**Picking work.** Take the lowest-numbered milestone whose dependencies have
landed and that nobody else holds. If it is bigger than one PR, split it into
vertical slices and say so in the PR. If it touches a deferred item (section
6), ask before coding that part.

**Branches and PRs.** Branch name `m<N>-<slug>` (e.g. `m1-evals`). Open a PR.
Never push to `main`, never force-push `main`, never merge your own PR; Arndev merges.

**One vertical slice per PR.** Each PR is usable end to end (types through CLI
where relevant), with its tests and doc updates. No drive-by refactors.

**Tests.**
- Everything runs offline. No network in CI or in the default suite. Live
  recording and live evals stay behind an opt-in env var and `#[ignore]`; `CI` refuses them.
- Golden request bodies: regenerate with
  `UPDATE_GOLDEN=1 cargo test -p keel-llm-protocol --test requests`, read the
  diff, and explain every changed line in the PR.
- Cassette request halves: `UPDATE_CASSETTE=1 cargo test -p keel-llm-route --test cassettes`.
  Never hand-edit a cassette. Every cassette on disk must be listed in `scenarios.toml`.
- New behavior gets a named test that reads as a sentence
  (`a_denied_tool_returns_an_error_result_without_running` style).
- PRs that change prompts, tool text or the loop (M2-M4 and later) include the
  M1 replay score before and after.
- Local tool tests need `bash` and `rg` on `PATH`; the eval suite also needs
  `python3`.
- Never commit keys. Use `KEEL_RECORD_KEY_FILE` for recording.

**Review checklist** (author checks before asking for review):
- Dependency rule: no new `[dependencies]` edge outside the crate table in
  section 2. This is a review rule, not a test (decision 10); call out any new edge.
- The nine invariants in section 2 still hold; say which tests cover any you touched.
- Golden and cassette diffs are explained line by line.

**Definition of done** (matches `ci.yml`, ubuntu and macOS):
```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Plus: review checklist passes; README, `docs/architecture.md` and this blueprint updated.

**Commit style** (from `git log`):
- Subject: imperative, sentence case, no prefix, no trailing period, about 60
  chars (`Retry a failed turn at the session boundary`).
- Body: prose wrapped near 80 columns. Why first, then what changed, what
  stays open (cite O-/D- ids), what the tests cover.
- Last body line: `N tests, clippy and fmt clean.`
- Keep the trailer:
  `Co-authored-by: factory-droid[bot] <138933559+factory-droid[bot]@users.noreply.github.com>`

**When a milestone lands** (in the same PR):
- README: move the row from "Not implemented yet" to "Implemented", or narrow
  it to what remains (as the O-009 and O-004 rows do today). Update "Status"
  if the slice count changes. Add new crates to the layout table.
- `docs/architecture.md`: update the relevant section and "Next steps".
- `docs/decisions.md`: update the id's line if the milestone changes it.
- This file: mark the milestone done with the commit hash, move its evidence
  to section 3, and add any new open question to section 6.

---

## 6. Decisions

Settled by Arndev on 2026-11-19.

| # | question | decision |
|---|---|---|
| 1 | Harness quality vs architecture "Next steps" | Harness quality (M1-M4) goes first. |
| 2 | D-/O- summaries in-repo | Yes. `docs/decisions.md` summarizes `D-001`..`D-010` and `O-001`..`O-013` in-repo (M0). |
| 3 | Engineering notes | Moved under `docs/notes/` (M0). |
| 4 | Where evals live | New crate `keel-evals` (name proposed); the runner drives `Harness` directly; replay consumes ordered cassettes with eval-only output normalization; one check script per task with a deadline; `EndTurn`/`MaxTurns` with a passing check is a clean stop, `Failed`/`Cancelled` never are (M1, review fixes). |
| 5 | Live evals | NeuralWatt `deepseek-v4.1-flash`; $10 hard cap per live run; cassettes hold multi-turn sessions; a new cassette replaces the baseline only after a clean replay. 12/12 tasks pass; two recording runs cost $0.056289; the review fixes did not record again (M1). |
| 6 | Default system prompt | Lives in `keel-core`; `--system` appends; no per-model variants for now (M3). Resume uses the stored header prompt as-is; a legacy `system: None` header stays prompt-less, so `--system` there applies alone. |
| 7 | Compaction | **Decided and implemented (M4, `f464835`).** Summary by the session's main chat model; triggers at the model's catalog context window; follows OpenAI practice. The log record is the worker's version-2 design: one appended `compaction` record with the summary and the replacement view. |
| 8 | Antigravity placement | Last milestone (M11), after the credential store and everything else. |
| 9 | MCP | Its own milestone (M7), split out of O-010. |
| 10 | Dependency rule check | Review rule only; no automated test. |
| 11 | Workflow | PRs only, Arndev merges; branch `m<N>-<slug>`; keep the factory-droid `Co-authored-by` trailer. |
| 12 | Tool text quality | Structured `Use when` / `Prefer` / `Returns` / `On failure` descriptions and actionable failure results; no new tools or schema changes (M2). |

### Open

Nothing blocks M4–M9.

### Deferred

Open design choices in `O-003` (cache policy object shape and breakpoint caps)
and `O-012` (Antigravity default enablement, OAuth client identity, labeling,
account rotation, kill switch) are summarized in `docs/decisions.md` and only
need Arndev's call before scoping M10 and M11 respectively.
