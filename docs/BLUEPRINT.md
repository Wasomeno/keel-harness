# keel blueprint

Read this before touching code. It says what keel is, what holds it together,
what is done, what is next, and how a worker ships a change.

Sources: `README.md`, `docs/architecture.md`, `docs/decisions.md`,
`docs/notes/*.md`, the crate manifests, `crates/keel-evals/`,
`.github/workflows/ci.yml`, and `git log` up to `911427a` (the PR #35 merge
after M12 wave 1, 2026-11-21), plus the follow-up review fixes.
Where this file guesses rather than reads, it says **(inferred)**. Where it and
the README disagree, the code wins; fix the README in the same PR.

The D-xxx / O-xxx ids are summarized in [`docs/decisions.md`](decisions.md)
(`D-001`..`D-012` and `O-001`..`O-013`), and [`docs/architecture.md`](architecture.md)
maps `D-001`..`D-012` to code ("Where the decisions land"). If an id's meaning
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
| `keel-llm-catalog` | providers and per-model protocol assignments; generated models.dev snapshot plus a checked-in overlay | types |
| `keel-catalog-gen` | dev-only: regenerates `catalog/models.dev.toml`; atomic last-known-good writes | catalog |
| `keel-tools` | `Tool` trait, registry, permission gate, workspace sandbox, built-ins | types |
| `keel-mcp` | MCP stdio and Streamable HTTP client, server config, `Tool` adapters | tools, types |
| `keel-mcp-testkit` | dev-only: scripted MCP stdio server, `FauxProvider` HTTP tests, and end-to-end MCP tests | core, mcp, protocol, route, testkit, tools, types |
| `keel-testkit` | dev-only: scripted HTTP/SSE server, faux provider, cassette player and recorder | nothing of keel's |
| `keel-core` | route resolution, event assembly, `Session`, `Harness` loop, session log | catalog, route, tools, types; dev-only edge to testkit |
| `keel-evals` | dev-only: Python fixture, 12 tasks, offline cassette replay, budget-capped live runner | core, catalog, route, tools, testkit, types |
| `keel-cli` | the `keel` binary | core, catalog, route, mcp, tools, types |

**Dependency rule (one-way).** Dependencies point down. "No provider type
reaches the session loop, and no HTTP concern reaches the types crate." A
protocol module cannot see the catalog, the session, or a provider name.
`keel-tools` does not know a model exists. `keel-mcp` depends on the tool seam
and is started by `keel-cli`, never imported by `keel-core`. `keel-testkit`,
`keel-mcp-testkit`, `keel-evals` and `keel-catalog-gen` are dev-only.
`keel-core`'s edge to
`keel-testkit` is a `[dev-dependencies]` edge only; the runtime graph has no
such dependency.
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
| 4 | Every stream has a terminal `Done`; errors carry partial content and usage. | protocol decoders, `EventStream` in `keel-llm-route/src/client.rs` (ends at the terminal event), `Assembler` | `http.rs`: `a_stream_without_a_done_frame_still_terminates`; `faux_provider.rs`: `a_truncated_stream_is_closed_with_a_warning`, `a_stalled_body_times_out_instead_of_hanging`, `a_complete_turn_ends_without_waiting_for_the_body_to_close`, `a_stalled_error_body_is_bounded`; `session.rs`: `a_mid_stream_failure_keeps_partial_output` |
| 5 | Retry at the session boundary, with one narrow pre-stream exception. | `Client::stream_with_retry` (pre-stream), `RetryPolicy::decide` (`keel-llm-route/src/retry.rs`), `EventStream` idle timeout (ends the stream, never retries), `Harness` (`keel-core/src/run.rs`) | `faux_provider.rs`: `retries_a_429_before_the_stream_opens`, `a_timeout_after_body_bytes_is_not_retried_by_the_transport`, `a_stalled_error_body_does_not_block_the_pre_stream_retry`; `run.rs`: `a_retryable_failure_replays_the_turn_without_duplicating_input`, `an_idle_timeout_after_partial_output_replays_at_the_turn_boundary`, `a_decode_failure_is_not_retried`, `retries_stop_at_max_attempts` |
| 6 | Per-protocol quirks are data (`Compat`), not vendor-specific clients. | `Compat` (`keel-llm-types/src/compat.rs`), per-model `compat` overrides in the catalog | golden requests (`keel-llm-protocol/tests/requests.rs`) |
| 7 | Capabilities and protocol are properties of the model entry. | `catalog/overlay.toml` (`protocol`, `capabilities` per model) merged with `catalog/models.dev.toml`; catalog maps capabilities into `Compat` (`keel-llm-catalog/src/lib.rs`) | catalog unit tests, `keel-llm-catalog/tests/providers.rs` |
| 8 | Every tool call gets exactly one result, including denied, unknown, timed-out and cancelled calls. | `Harness` (`run.rs`, incl. `cancelled before execution`); repair step 7 synthesizes missing results | `run.rs`: `a_denied_tool_...`, `an_unknown_tool_...`, `a_timed_out_tool_...`, `cancellation_during_a_tool_leaves_a_complete_transcript`; `keel-mcp-testkit/tests/harness.rs`: the MCP denied, unknown, timed-out and cancelled cases |
| 9 | Tool execution is deny-by-default for embedders; the CLI opts into asking or `--yes`. | `Harness::new` sets `PermissionGate::DenyAll` (`run.rs`); CLI uses `Ask` unless `--yes`; MCP tools are registered, not pre-approved | `run.rs`: `a_denied_tool_returns_an_error_result_without_running`; `keel-mcp-testkit/tests/harness.rs`: `a_denied_mcp_call_is_one_error_result_and_never_reaches_the_server` |

Two more properties from `docs/architecture.md` that workers must keep: history
is committed only after the stream opens, and **the raw history is never
rewritten**: repair produces a per-request view, so failed turns stay
inspectable.

---

## 3. Current state

Current `main` is `042722e` (2026-11-21):
M0 through M10, M12 wave 0, and the wave-1 slices M12.4 and M12.6 are merged;
PR #31 merged as `f4e7714`, PR #32 as `73f79ce`, M12.6 as `ef4ca13` (PR #34)
and M12.4 as `911427a` (PR #35). M12.7 is implemented on `m12-test-loop` (PR
[#38](https://github.com/Wasomeno/keel-harness/pull/38)). On
`main` the Rust suite is 645 passed, 0 failed, 2 ignored on macOS and Ubuntu
(603 on Windows), and the Python benchmark suite is 255 tests OK. On this
branch the Rust suite is 661 passed, 0 failed, 2 ignored on macOS and the Python
benchmark suite is 278 tests OK. fmt and clippy are clean, and CI is green on
Ubuntu, macOS and Windows plus the `benchmarks` job. M11 is on hold pending
O-012 and Google keys. M12.5 (background jobs) is the remaining wave-1
follow-on.

M12 wave 0 landed as `7c50d58` from PRs [#30](https://github.com/Wasomeno/keel-harness/pull/30)
(`13d32de`, merged `7c50d58`), [#28](https://github.com/Wasomeno/keel-harness/pull/28)
(`4663e65`, merged `1c1d2d0`), and [#29](https://github.com/Wasomeno/keel-harness/pull/29)
(`e9593cb`, merged `ec4c871`). It adds the Harbor `BaseInstalledAgent` adapter,
the SWE-bench prediction runner, and the canonical trace schema, reporter,
failure taxonomy and fixed subsets. The Harbor `hello-world` smoke at
`13d32de` completed the agent phase (exit 0, 3 turns, 2 tool calls, 122 input
tokens, 172 output tokens, 3.717 s) with a strict-valid 26-field trace;
`verifier_result` is `not_run` because Harbor's verifier timed out during its
own package setup. The full 10-task Terminal-Bench iteration attempt then
produced six passes across all 10 attempted tasks: 8 valid trials (6 pass, 2
fail) for a 6/8 = 0.75 valid pass rate, with `extract-elf` and
`modernize-scientific-stack` excluded as invalid; the raw 6/10 attempted rate
is not a clean benchmark pass@1. `custom-memory-heap-crash` passed after its
stalled image pull was retried manually. One SWE-bench Lite instance is
confirmed at 1 resolved/0 unresolved/0 empty/0 errors on the clean 4,712-byte
patch after PR #32 (merged as `73f79ce`), which is one instance, not a
Lite-wide score. No native aarch64 binary exists, there is no full score, and
the adapter leaves `cost_usd` null. See `docs/notes/m12-wave0-results.md` for
the table and caveats.

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
M7 lands as `c305d5e`: opt-in MCP stdio tools, with a test-only fixture crate
and bounded startup/read/write paths. The MCP client now also speaks
Streamable HTTP (`mcp-http`): `url` servers, static headers, JSON and SSE
responses, session/protocol headers and typed HTTP errors, with the same
handshake, tool registry, permission gate, timeout and cancellation. Stdio
behavior is unchanged.
M9 adds the generated catalog: a committed models.dev snapshot, the checked-in
keel overlay, an optional runtime overlay, and the dev-only `keel-catalog-gen`
writer with atomic last-known-good behavior (499 passing tests, 2 ignored).
M8 is done by re-scope: first-party NeuralWatt captures cover all three
implemented protocols, and OpenAI- and Anthropic-endpoint-specific captures are
out of scope rather than waiting on keys (section 4, M8).

| area | commit(s) | evidence |
|---|---|---|
| Workspace, canonical types, repair, Chat + Anthropic Messages, route, seed catalog, CLI | `dbf481b` | `keel-llm-types/tests/repair.rs` (13), `tests/anthropic_messages.rs`, `tests/openai_chat.rs` |
| Tools, sandbox, permission gate, `Harness` loop (D-007) | `53bd2aa` | `keel-tools/tests/builtin.rs` (13), `keel-core/tests/run.rs` |
| Faux provider, golden requests, recorded wire fixtures | `80cf61f` | `tests/requests.rs`, `fixtures/requests/*.json`, `recorded.rs`, `faux_provider.rs` |
| OpenAI Responses protocol | `fdf7af6` | `responses_text_turn_keeps_reasoning_replayable`, `responses_tool_call_is_addressed_by_call_id` |
| Cassette player, recorder, `scenarios.toml` manifest | `e9fa94a` | `a_changed_request_does_not_replay`, `every_cassette_is_covered_by_the_manifest` |
| Bounded JSON nesting (64) in all three codecs | `67aa022` | 15 unit tests in codec files; `docs/notes/codec-json-recursion.md` |
| NeuralWatt in catalog, gated live recorder, five first-party live cassettes, body redaction, unsigned-reasoning warning (M8 re-scope) | `102d2c4`, `cbcedc1`, `3f9056d`, `107aad4` | `the_live_neuralwatt_*_decodes`, `a_gateway_without_encrypted_reasoning_still_decodes`; the twelve `replays_*` eval tests; `docs/notes/neuralwatt-live-captures.md` |
| JSONL session log and `--resume` | `3f8dee7` | `session.rs`: `a_sink_sees_every_message_that_enters_history`, `a_session_log_holds_the_transcript_of_a_turn`; `run.rs`: `a_session_log_holds_tool_results_in_order` |
| Turn retry at the session boundary | `f042b90` | `run.rs`: four retry tests listed above |
| Repo hygiene, blueprint, decision summaries, doc-drift alignment | M0 | `docs/BLUEPRINT.md`, `docs/decisions.md`, `docs/notes/`, `README.md`, `docs/architecture.md` |
| Behavioral eval suite: Python fixture, 12 tasks, ordered cassette replay, budget-capped live recorder | `ce5ecf8`, plus `m1-fixes` and `m1-eval-safety` | `keel-evals` 49 passing tests plus one ignored live test, including `replays_*` (12), `the_budget_gate_stops_before_the_first_request_when_the_cap_is_one_micro_short`, `the_suite_marks_the_next_task_not_run_after_a_real_budget_stop`, `a_failed_mid_stream_turn_is_reported_failed_and_does_not_replace`, `commit_cassette_persists_a_clean_replay`, `commit_cassette_rejects_a_replay_whose_check_fails`, `an_extra_cassette_exchange_fails_the_replay`, `a_grandchild_holding_stdout_does_not_block_a_finished_check`, `every_task_grades_with_its_own_check_script`, `every_recorded_shell_command_passes_the_guard`, `checks_bite`; `crates/keel-evals/reports/live.md` |
| Structured tool descriptions and actionable failure text | `72dc3f4` | `keel-tools/tests/builtin.rs` (30 tests, exact error text per failure path), `keel-evals/tests/tool_text.rs` (cassette tool-text drift gate), description-only golden diff, live 12/12 at $0.034032 |
| Default system prompt in `keel-core`, `--system` appends, stored-header resume | `cd112cc` | `prompt.rs` unit tests, `keel-core/tests/session.rs` captured-request tests, `keel-cli/tests/chat.rs` header and resume tests, system-text-only golden diff, live 12/12 at $0.039297 |
| MCP stdio tools, explicit opt-in, bounded protocol and lifecycle | `c305d5e` | `keel-mcp-testkit/tests/` (client + `Harness` cases), `keel-cli/tests/tools.rs` (no spawn without `--mcp`, spawn with it), `keel-mcp/src/config.rs` per-server parse and timeout tests |
| MCP Streamable HTTP transport: `url`/`headers` config, JSON and SSE responses, session and protocol headers, typed errors, cancellation and `DELETE` | `mcp-http` | `keel-mcp-testkit/tests/http.rs` (24 cases, including the denied/allowed `Harness` run), `keel-mcp/src/config.rs` URL/header tests, `keel-cli/tests/tools.rs` (HTTP config is not contacted without `--mcp`), `docs/notes/mcp-http.md` |
| Atomic `write`/`edit` replacement: temp file + fsync + `renameat`, mode preservation, read-only refusal | `atomic-writes` | `keel-tools/tests/builtin.rs`: `a_failed_write_leaves_the_old_file_intact`, `a_failed_edit_leaves_the_old_file_intact`, `writing_preserves_the_existing_file_mode`, `editing_preserves_the_file_mode`, `a_new_file_gets_the_process_umask_mode`, `a_successful_replace_leaves_no_temp_file`, `a_failed_replace_leaves_no_temp_file`, `a_read_only_file_is_not_replaced`, `write_refuses_a_fifo_without_blocking`, `write_refuses_a_socket_without_replacing_it`, `writing_drops_special_mode_bits`, `an_edit_that_fails_on_old_string_leaves_the_file_byte_identical`; `workspace.rs`: `a_symlink_swapped_in_before_the_rename_is_replaced_not_followed`; `docs/notes/atomic-writes.md` |
| Windows CI: same fmt/clippy/test matrix on Ubuntu, macOS and Windows; portable tool paths and error text, Git Bash resolution, LF/UTF-8 output | `windows-ci` | `.github/workflows/ci.yml` (three-OS matrix, cross-platform `rg` install); `keel-tools/tests/builtin.rs` portable read-only and directory cases; `docs/notes/windows-ci.md` (Windows runs 42 fewer tests: 44 Unix-gated minus two Windows-only; that note records the counts from its own PR, while the current branch suite is 661 passed on macOS; the Windows count is re-measured by CI) |
| Bounded provider stream stalls: per-chunk idle timeout with no total deadline, terminal-event stop, bounded non-2xx error bodies, configurable by env or embedder API | `m12-stream-idle-timeout` | `keel-llm-route/tests/faux_provider.rs`: `a_stalled_body_times_out_instead_of_hanging`, `chunks_spaced_below_the_idle_timeout_complete`, `a_timeout_after_body_bytes_is_not_retried_by_the_transport`, `a_disabled_idle_timeout_leaves_a_stall_alone`, `a_complete_turn_ends_without_waiting_for_the_body_to_close`, `a_stalled_error_body_is_bounded`, `a_stalled_error_body_does_not_block_the_pre_stream_retry`, `an_absurd_idle_timeout_is_clamped_instead_of_panicking`; `client.rs` unit tests for parsing, clamping and the default; `keel-core/tests/run.rs`: `an_idle_timeout_after_partial_output_replays_at_the_turn_boundary`; `keel-cli/tests/chat.rs`: `a_stalled_provider_stream_reports_a_timeout_instead_of_hanging` |
| Generated catalog, checked-in overlay, runtime overlay, last-known-good generator | `m9-generated-catalog` | `keel-llm-catalog/tests/providers.rs` pins all eight providers and 21 models including deprecated status; catalog unit tests cover generated-vs-overlay-vs-runtime precedence, additions, empty/unreadable overlays, duplicate-provider warnings, malformed/unknown-key fallback, unknown lookups and the snapshot/provenance date pair; `keel-catalog-gen` writer tests prove failure leaves the previous snapshot untouched and refuse dropped models without all four pinned facts; `keel-cli/tests/catalog.rs` pins the exact `keel models` and `keel providers` output and drives `models`/`route` from a runtime file |
| M12.1 Harbor adapter and Linux artifact | `13d32de`, merged as `7c50d58` (PR #30) | `benchmarks/harbor/keel_agent.py`; fake-environment unit tests, strict 26-field trace, and a live `hello-world` agent-phase smoke (3 turns, 2 tool calls, 122 input, 172 output, 3.717 s); verifier phase not merged (`not_run`) |
| M12.2 SWE-bench prediction runner and patch extraction | `4663e65`, merged as `1c1d2d0` (PR #28) | `benchmarks/swebench/run_keel.py`; 94 runner tests at the slice head, predictions JSONL, `run.json`, trace sidecar with six documented non-strict extensions, advisory `git apply --check`, and the official harness command; one confirmed live Lite instance (`astropy__astropy-12907`, harness 1 resolved/0 unresolved/0 empty/0 errors on the clean 4,712-byte patch with 0 mode-only pairs; PR #32 merged as `73f79ce`; one instance only) |
| M12.3 Trace schema, reporter, failure taxonomy and fixed subsets | `e9593cb`, merged as `ec4c871` (PR #29) | `benchmarks/traces/{schema,report,tag_failure}.py`; 88 trace/subsets tests at the slice head, 26 required fields, 16 tags, pass@1/pass@k/CI/cost metrics, atomic tag audit trail, fixed 89/10/20 Terminal-Bench and 10 SWE-bench Lite subsets, and the separate `benchmarks` CI job |
| M12.4 Persistent terminal session | merged as `911427a` (PR [#35](https://github.com/Wasomeno/keel-harness/pull/35)) | `keel-tools/src/builtin/terminal.rs` (Unix-only PTY shell), `--terminal`/`KEEL_TERMINAL=1`, Harbor adapter opt-in; `keel-tools/tests/terminal.rs` covers `cd`/`export` persistence, `workdir`, non-UTF-8, cancellation without losing the session, interactive timeout recovery, shell exit/restart, process-group cleanup and the unchanged one-shot `bash` path; `keel-cli/tests/{tools,chat}.rs` cover the opt-in listing and a full `chat --terminal` turn; `docs/notes/benchmark-readiness.md` records the slice |
| M12.6 Patch finalization and empty-patch protection | `00c596d`, `daae977`, merged as `ef4ca13` (PR #34) | `keel chat --require-changes` exits 3 on a clean one-shot workspace and never masks a failed turn (`keel-cli/tests/chat.rs`: `require_changes_fails_a_run_that_leaves_the_workspace_clean`, `require_changes_passes_when_the_run_writes_a_file`, `require_changes_does_not_mask_a_failed_turn`, `require_changes_needs_a_one_shot_prompt`); `benchmarks/swebench/run_keel.py` finalizes from one base-seeded temporary index, excludes generated paths, records test-file edits, gates whitespace and retries once (`benchmarks/tests/test_swebench_runner.py`: new-file staging, generated-artifact exclusion, test-edit detection, whitespace detection, empty workspace, shared temp index, no-change retry and whitespace retry); the merged wave-1 suite is 645 Rust tests and 255 Python tests |
| M12.7 Test discovery, targeted test loop and pre-final verify | branch `m12-test-loop`, PR [#38](https://github.com/Wasomeno/keel-harness/pull/38) | `benchmarks/swebench/test_discovery.py` reads README, Makefile, CI and project config and passes a hint; `keel chat --test-loop` tracks visible tests, parses pytest/cargo/unittest failures, pins the status through compaction and injects one pre-final message (`keel-core/src/test_loop.rs`, `keel-core/src/session.rs`, `keel-core/src/run.rs`, `keel-cli/tests/chat.rs`); traces and the reporter carry test-run evidence; tests include a synthetic failing-unittest fix (`keel-core/tests/test_loop.rs`) |

**Atomic write hardening.** `write` and `edit` no longer truncate the live
file. Each writes a temp entry beside the target, fsyncs it, renames it over
the target with `renameat`, then fsyncs the directory best-effort. A failure before the
rename leaves the old bytes untouched and a drop guard removes the temp entry.
The target is opened `O_WRONLY | O_NOFOLLOW | O_NONBLOCK` first, so a
read-only file is refused instead of becoming replaceable through a writable
directory; `fstat` must report a regular file, so a FIFO, socket or device is
refused rather than blocking the writer or being replaced. The existing
ordinary mode is copied with `fchmod` while setuid/setgid/sticky are dropped. The accepted cost is a new inode: hard
links keep the old contents, and ownership, ACLs and extended attributes are
not carried over. This closes the gap without adding a dependency; the
descriptor walk and the non-Unix fallback keep their existing error wording.

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
  and non-Unix keeps the canonicalizing fallback. Windows runs the same CI
  commands as Unix, so the fallback's paths and error text are tested even
  though its symlink and TOCTOU behavior still differs. Tool output uses `/`
  separators and LF on every platform, and Windows `bash` resolves Git Bash
  rather than the WSL launcher.
- Consecutive `parallel_safe` tool calls run concurrently in bounded,
  order-preserving batches (`keel-core/src/run.rs`); writes, edits and `bash`
  are barriers. Results and finish events stay in call order, and cancellation
  still leaves one result per call. MCP tools follow the same rule: a stdio
  server serializes writes through its writer task, while HTTP calls have no
  shared writer and overlap directly.
- `bash` confinement beyond the working directory and the non-Unix file-tool
  fallback remain gaps (O-010 later), even though Windows now runs in CI and
  tests the fallback's behavior on that platform. `write` and `edit` replace
  atomically through a temp file in the target's directory plus `renameat`,
  preserving an existing file's mode; a replaced file gets a new inode, so hard
  links keep the old contents.

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
| M4 | Session compaction *(done — `9419e0b`)* | O-009, O-004 | M1, M3 |
| M5 | Credential store *(done — `bd00353`)* | O-005 | - |
| M6 | OS sandbox and parallel tool execution *(done — `c69a2d4`)* | O-010 | - |
| M7 | MCP tools *(done — `c305d5e`)* | O-010 (split out) | M6 |
| M8 | First-party NeuralWatt captures (re-scoped from OpenAI/Anthropic) *(done — re-scoped, merge base `efbc507`)* | O-008 | - |
| M9 | Generated catalog *(done — `m9-generated-catalog`)* | O-006 | - |
| M10 | Full cache policy *(done — m10-cache-policy)* | O-003 | - |
| M11 | Google/Antigravity | D-005, O-012 | M5, and every milestone above |
| M12.1 | Harbor adapter and Linux artifact *(done — `7c50d58`)* | - | #20, #21 |
| M12.2 | SWE-bench prediction runner *(done — `1c1d2d0`)* | - | #20, #21 |
| M12.3 | Trace schema, reporter and fixed subsets *(done — `ec4c871`)* | - | M12.1, M12.2 |
| M12.4 | Persistent terminal session *(done — `911427a`)* | - | M12.3 |
| M12.6 | Patch finalization and empty-patch protection *(done — `ef4ca13`)* | - | M12.2 |
| M12.7 | Test discovery, targeted test loop and pre-final verify *(PR [#38](https://github.com/Wasomeno/keel-harness/pull/38))* | - | M12.2, M12.6 |

M12 wave 0 is complete at `7c50d58`, and the wave-1 slices are merged: M12.6
as `ef4ca13` (PR #34) and M12.4 as `911427a` (PR #35). M12.7 is implemented on
`m12-test-loop` (PR #38); M12.5 (background jobs) is the remaining wave-1
follow-on.
M12.6 adds `--require-changes` plus SWE-bench patch finalization; M12.4 adds
the opt-in Unix `terminal` tool and leaves the default six-tool chat path
unchanged; M12.7 adds test-command discovery as a prompt hint, an opt-in
`--test-loop` that tracks visible tests and asks once for a final
change/test/unverified summary, and test-run metrics for the trace reporter.
None of the three slices has been measured end to end on a live benchmark
subset yet.
The measurement contract, full slice list and the M11/O-012
blocker stay in `docs/notes/benchmark-readiness.md`. The wave-0 evidence is in
`docs/notes/m12-wave0-results.md`; this wave ran the completed 10-task
iteration attempt (6/8 valid) and one confirmed SWE-bench Lite instance (n=1),
not a full benchmark.

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
  none invented; README agreed with the then-current seed catalog, `main.rs`
  usage text and test layout; CI green.
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
  `.github/workflows/ci.yml` (installs `rg` when missing; this started with
  macOS and now covers all three runners).
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

### M4. Session compaction (O-009 remainder) *(done — `9419e0b`)*
- **Goal:** a session keeps going when it reaches the model's context window.
- **Result:** compaction runs in `Session::step_streaming`. The threshold check is
  exactly `last_reported_input + last_reported_output + next_output_budget >=
  context_window`, where the budget is `max_output_tokens` or the catalog output
  limit carried on the `Route`; with no reported usage (the first request, or a
  compacted resume whose view has no assistant usage) it uses the conservative
  estimator. The estimator preview includes the pending input, but the
  reported branch does not, so a very large new paste relies on the overflow
  path. One threshold compaction runs per step. A provider
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
  tools + next output budget) with the same inclusive boundary as the trigger;
  if it does not fit, compaction fails clearly, naming the output budget,
  pending input, summary/system/tools or newest user message as the cause,
  instead of dropping the request. Logs are version 2: one appended
  `{"type":"compaction",...}` record holds the reason, summary and full
  replacement view, earlier bytes are unchanged, version-1 logs load, and
  resume applies the latest record plus the messages after it. Version-2
  headers record `max_output_tokens` and `temperature` so a resumed compacted
  session sends the same request. The CLI prints one `[compact]` stderr line
  per compaction (a threshold compaction followed by an overflow compaction
  prints two), warns when a route has no catalog window, and `keel route` shows
  the window and output limit. A successful run reports summary usage as
  `RunOutcome::compaction_usage`; a summary whose compaction fails returns
  `Err` and leaves its usage on the session for `take_compaction_usage`. It is
  printed on the final usage line and charged by the eval budget, without
  inflating the trigger. A gated run can contain two compactions, so the cap
  can be exceeded by up to two summary requests. The
  token estimator is one token per three bytes plus fixed message, part, tool
  and image overheads (`keel-core/src/tokens.rs`). No live run was needed;
  cassettes and goldens are unchanged and the 12 M1 replay tasks pass. 391
  tests pass on the rebased branch (384 tests on main after M5 and M6, plus the
  7 M4 follow-up tests; 21 compaction integration tests plus estimator, view,
  store, harness, CLI, eval-budget and protocol-classifier coverage); clippy
  and fmt are clean.
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

### M7. MCP tools (split out of O-010) *(done — `c305d5e`)*
- **Goal:** tools from MCP servers appear as ordinary `Tool`s.
- **Scope:** an MCP-backed `Tool` source registered in `ToolRegistry`; calls go
  through the same permission gate, timeout, cancellation and truncation.
- **Out of scope:** keel acting as an MCP server.
- **Result:** `keel-mcp` is a crate between `keel-cli` and the tool seam, and
  `keel-mcp-testkit` is a dev-only crate with the scripted fixture and
  end-to-end tests. Nothing starts from a workspace file by accident: `chat
  --mcp`, `tools --mcp`, or `KEEL_MCP=1` is required, and without it the CLI
  reads the config only to print parse warnings and one "configured but not
  started" line for valid servers, and lists no MCP tools. `enabled` is a
  secondary filter. A server command runs with the
  user's full privileges, outside the workspace sandbox, equivalent to `bash`;
  `--yes` and the tool permission gate do not cover startup.
  Servers start in the workspace over newline-delimited JSON-RPC, offering MCP
  `2025-06-18` and accepting `2025-06-18` or `2024-11-05`. The client sends
  `initialize`, `notifications/initialized`, paginated `tools/list`, and
  `tools/call`; the handshake and every page share one startup deadline, and
  `startup_timeout_ms` is capped at 60 seconds. Blank lines and CRLF are
  tolerated; progress is ignored; `ping` is answered; other server-initiated
  requests get `-32601`; a method-bearing message never reaches a pending call;
  numeric-string ids are accepted. A 1 MiB line cap and bounded page/tool
  counts keep a server from streaming unbounded data. One writer task owns
  stdin, so a timeout cannot leave a partial line. A malformed, non-UTF-8 or
  oversized line closes the server, fails pending calls, and kills and reaps
  the child.
  Every remote tool registers as `server__tool`; `.` and `:` map to `_`, other
  invalid or over-64-character names and collisions are skipped with a warning,
  and built-ins win. Schemas must have `type: "object"` and stay under 16 KiB;
  a missing `properties` is normalized to `{}` for the provider, and
  descriptions are cut at 2 KiB. `McpTool` implements `Tool`, so the permission
  gate, `ToolLimits` timeout, cancellation and 64 KiB truncation are the same
  as built-ins, and MCP errors become tool results. A missing or crashing server
  is one startup warning, including a bounded stderr tail, with no tools; there
  is no restart. Calls are barriers unless a server sets
  `parallel_safe = true`. Timeout or cancellation removes the pending entry and sends
  `notifications/cancelled`; the request line is still delivered, so a timed-out
  call may already have executed on the server. Dropping the last client kills
  the child. The `env` allowlist is hygiene, not a security boundary; `env_clear`
  drops `SSL_CERT_FILE` and `XDG_*` unless listed, and `args` is free-form.
  `keel tools` shows MCP tools with a SOURCE column; `keel route --workspace`
  lists enabled server names without starting them. The suite is 443 passed and
  2 ignored.
- **Follow-up — Streamable HTTP (`mcp-http`):** a server may now set `url`
  instead of `command`. `headers` is an HTTP-only table whose values expand
  `${VAR}` once from the process environment; bad names, values or missing
  variables skip that server with a warning, and `Debug` and errors never show
  a value. One POST carries one JSON-RPC message with `Content-Type:
  application/json` and `Accept: application/json, text/event-stream`; the
  response is one JSON body or an SSE stream. After `initialize`,
  `MCP-Protocol-Version` and `Mcp-Session-Id` ride on every request. The client
  ignores notifications, answers a server-initiated request with a bounded
  `-32601` POST, treats `202` as notification success, rejects batches, and
  caps a body, line and frame at 1 MiB. Non-2xx maps to typed errors for
  401/403 (configured headers), 404 after a session (expired, restart), and
  429/5xx (status). Cancellation drops the POST and sends
  `notifications/cancelled`; shutdown sends `DELETE`; calls are never retried.
  There is no writer task, so `parallel_safe` HTTP calls overlap. The optional
  GET stream, SSE resumability and OAuth are out of scope. stdio behavior and
  its tests are unchanged. The workspace suite is 584 passed and 2 ignored.
- **Accept:** a scripted MCP server in tests; invariants 8 and 9 hold for MCP
  tools (denied, unknown, timed-out, cancelled each get one result); MCP
  errors are tool results, not harness errors. `keel tools` does not spawn a
  configured command without `--mcp`, and does start it with the flag. The
  `mcp_fixture` binary is a real subprocess in `keel-mcp-testkit/tests/`; denied
  calls leave a marker untouched. A zero-argument tool with `{"type": "object"}`
  registers with `properties: {}`. Oversized output, malformed output and a
  mid-call crash each reap the child; a timed-out call leaves the next call
  usable; an unsafe MCP call splits a batch.
- **Touches:** new `keel-mcp` and `keel-mcp-testkit` crates, `keel-cli` config,
  opt-in and display.
- **Depends on:** M6 (parallel execution semantics settled first).

### M8. First-party NeuralWatt captures (re-scoped) *(done — merge base `efbc507`)*
- **What M8 originally meant:** cassettes recorded from OpenAI's and Anthropic's
  own endpoints, "not borrowed bytes", each listed in `scenarios.toml` with
  named decode tests, `PROVENANCE.md` updated and redaction verified. It
  depended on vendor keys that never became available.
- **What was delivered:** first-party, live-recorded NeuralWatt coverage of all
  three implemented protocols. NeuralWatt's catalog entry serves OpenAI Chat
  Completions, OpenAI Responses and Anthropic Messages from one host, so each
  recording through keel's loopback recorder holds both halves of a real
  provider exchange, not a response copied from another project. All five were
  recorded on 2026-11-18 and are already on `main`; the acceptance evidence is:
  - `crates/keel-llm-route/tests/fixtures/cassettes/neuralwatt/chat-text.json`
    → `the_live_neuralwatt_capture_decodes` (chat text)
  - `.../chat-tool-call.json` → `the_live_neuralwatt_tool_call_decodes`
    (chat tool call)
  - `.../messages-text.json` → `the_live_neuralwatt_messages_turn_decodes`
    (Anthropic Messages with a signed thinking block)
  - `.../responses-text.json` → `the_live_neuralwatt_responses_turn_decodes`
    (Responses text)
  - `.../responses-reasoning.json` →
    `a_gateway_without_encrypted_reasoning_still_decodes` (Responses
    reasoning: the gateway omits the encrypted blob and the decoder warns once)

  All five are listed in
  `crates/keel-llm-route/tests/fixtures/cassettes/scenarios.toml` and replayed
  by `crates/keel-llm-route/tests/cassettes.rs` through the real client, route,
  framing and decoder. The twelve M1 eval cassettes under
  `crates/keel-evals/fixtures/cassettes/evals/` are also live NeuralWatt
  recordings (`deepseek-v4.1-flash`), consumed by `replays_truncate`,
  `replays_slugify`, `replays_parse_int`, `replays_mean`, `replays_clamp`,
  `replays_unique`, `replays_chunk`, `replays_format_table`,
  `replays_refactor_whitespace`, `replays_rename_format_row`,
  `replays_readme_slugify` and `replays_package_exports`. Recorder redaction is
  pinned by `secrets_inside_a_body_are_redacted`,
  `a_private_key_block_is_redacted` and
  `the_recorder_redacts_secrets_echoed_in_bodies` in
  `crates/keel-testkit/tests/cassette.rs`; the NeuralWatt files carry the
  redacted `authorization` header.
- **What was dropped:** OpenAI- and Anthropic-endpoint-specific captures are
  explicitly out of scope, not deferred. keel makes no claim to have recorded
  from either vendor's own endpoint. The `.sse` files under
  `crates/keel-llm-route/tests/fixtures/recorded/` are borrowed, MIT-licensed
  decoder conformance fixtures from opencode and stay separate from the
  first-party NeuralWatt captures. Google captures stay in M11 with the Google
  protocol.
- **Accept:** met by the evidence above: every first-party capture is listed in
  `scenarios.toml` with the named test that replays it, the response halves are
  the provider's own bytes, the request halves are keel's encoder output, and
  no cassette byte changed in the re-scope PR.
- **Touches:** `docs/`; the captures themselves landed earlier in
  `keel-llm-route/tests/fixtures/` and `keel-evals/fixtures/`.
- **Depends on:** nothing (the NeuralWatt key replaced the unavailable vendor
  keys).

### M9. Generated catalog (O-006) *(done — `m9-generated-catalog`)*
- **Goal:** catalog generated (seed comment: from models.dev at build time) with a runtime overlay and last-known-good fallback.
- **Result:** the hand-written `catalog/seed.toml` is gone. `catalog/models.dev.toml`
  is a committed snapshot of models.dev facts written by the dev-only
  `keel-catalog-gen` crate, and `catalog/overlay.toml` carries endpoints, auth,
  per-model protocol, path/compat overrides and pins. Both are embedded with
  `include_str!`; no build script and no build-time network. The generator is an
  explicit action (`cargo run -p keel-catalog-gen`, or `--from FILE`), writes
  through a temporary file and renames, and leaves the previous file untouched
  on any fetch, parse, schema or render failure. Precedence is runtime overlay >
  checked-in overlay > snapshot > defaults. The runtime overlay
  (`KEEL_CATALOG`, else the platform config path) merges by id and appends new
  providers/models; malformed or unknown-key input warns and falls back to the
  embedded catalog. `keel models` keeps its columns and order. models.dev is
  missing eleven of the 21 models (older entries, or providers it does not
  carry) and differs on four facts; those are pinned in the overlay and recorded
  in `PROVENANCE.md`. `keel providers` keeps its rows and only its trailing note
  changed. A dropped mapped model without all four pinned facts makes
  regeneration refuse to write, and the snapshot date must match `PROVENANCE.md`.
- **Accept:** builds offline from the last-known-good file; overlay test;
  `keel models` output stable. Covered by 499 passing workspace tests.
- **Touches:** `keel-llm-catalog`, `keel-catalog-gen` (new dev-only crate),
  `keel-cli`, docs.
- **Depends on:** nothing.

### M10. Full cache policy (O-003) *(done — `m10-cache-policy`)*
- **Goal:** `Auto` / `None` / `Object` cache policy across every implemented protocol.
- **Result:** `Request::cache` and `Session::with_cache` carry the policy; `Auto`
  is the default and keeps today's two Anthropic breakpoints plus a
  deterministic OpenAI `prompt_cache_key`. `Object` names canonical positions
  (`System`, `Tool`, `Message`, `Part`, `LastMessage`) and a `5m`/`1h` TTL;
  Anthropic keeps the first four breakpoints in the order given, which is
  caller priority (list durable tools/system anchors first), and every dropped
  position is a typed `CacheWarning` that the client emits as
  `LlmEvent::Warning` before the first frame. The stable Messages API version
  `2023-06-01` takes `ttl` with no beta header; the prompt-caching docs were
  checked 2026-11-19. A live Anthropic acceptance run for the one-hour TTL is
  not part of the suite. `None` strips every cache field, and
  `keel chat --cache none` or `KEEL_CACHE=none` opts out for strict
  OpenAI-compatible endpoints. The resolved policy is stored in the session
  header, so resume keeps the opt-out unless a new flag or env value overrides
  it for that run. Compaction summaries always use `None`. OpenAI cannot express
  explicit positions or TTL, so it sends the key and warns `Unsupported`. Nine
  golden bodies cover Auto/None/Object for Chat, Responses and Messages; unit
  tests cover cap-and-warn, missing/duplicate/non-cacheable positions, TTL
  serialization without a beta header, `None` stripping, key stability and
  warning delivery.
  Cassette, golden and eval request halves were regenerated from the encoder;
  no response half changed and no live call was made. Workspace suite: 537
  passed, 2 ignored.
- **Accept:** golden bodies per policy per protocol; `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`.
- **Touches:** `keel-llm-types`, `keel-llm-protocol`, `keel-llm-route` warning
  delivery, `keel-core` session builder, `keel-evals` preview and request-half
  gate, fixtures and docs.
- **Depends on:** O-003 (settled).

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
  `python3`. On Windows, install Git for Windows; the `bash` tool finds Git Bash
  automatically, or set `KEEL_BASH` to its `bash.exe`. `docs/notes/windows-ci.md`
  lists the tests that are Unix-only.
- Never commit keys. Use `KEEL_RECORD_KEY_FILE` for recording.

**Review checklist** (author checks before asking for review):
- Dependency rule: no new `[dependencies]` edge outside the crate table in
  section 2. This is a review rule, not a test (decision 10); call out any new edge.
- The nine invariants in section 2 still hold; say which tests cover any you touched.
- Golden and cassette diffs are explained line by line.

**Definition of done** (matches `ci.yml`, ubuntu, macOS and Windows):
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
| 2 | D-/O- summaries in-repo | Yes. `docs/decisions.md` summarizes `D-001`..`D-012` and `O-001`..`O-013` in-repo (M0). |
| 3 | Engineering notes | Moved under `docs/notes/` (M0). |
| 4 | Where evals live | New crate `keel-evals` (name proposed); the runner drives `Harness` directly; replay consumes ordered cassettes with eval-only output normalization; one check script per task with a deadline; `EndTurn`/`MaxTurns` with a passing check is a clean stop, `Failed`/`Cancelled` never are (M1, review fixes). |
| 5 | Live evals | NeuralWatt `deepseek-v4.1-flash`; $10 hard cap per live run; cassettes hold multi-turn sessions; a new cassette replaces the baseline only after a clean replay. 12/12 tasks pass; two recording runs cost $0.056289; the review fixes did not record again (M1). Compaction summary requests are charged after they are sent; a gated run can compact twice (threshold then overflow), so actual spend can exceed the cap by at most two summary requests. |
| 6 | Default system prompt | Lives in `keel-core`; `--system` appends; no per-model variants for now (M3). Resume uses the stored header prompt as-is; a legacy `system: None` header stays prompt-less, so `--system` there applies alone. |
| 7 | Compaction | **Decided and implemented (M4, `9419e0b`).** Summary by the session's main chat model; triggers at the model's catalog context window; follows OpenAI practice. The log record is the worker's version-2 design: one appended `compaction` record with the summary and the replacement view. |
| 8 | Antigravity placement | Last milestone (M11), after the credential store and everything else. |
| 9 | MCP | Landed in M7 as opt-in stdio tools registered through `ToolRegistry`; a server command needs `--mcp`/`KEEL_MCP=1` because it runs with full user privileges. The `mcp-http` follow-up adds Streamable HTTP `url` servers with static `headers` under the same opt-in; optional GET streams, SSE resumability, OAuth and serving MCP remain out of scope. |
| 10 | Dependency rule check | Review rule only; no automated test. |
| 11 | Workflow | PRs only, Arndev merges; branch `m<N>-<slug>`; keep the factory-droid `Co-authored-by` trailer. |
| 12 | Tool text quality | Structured `Use when` / `Prefer` / `Returns` / `On failure` descriptions and actionable failure results; no new tools or schema changes (M2). |
| 13 | Catalog data | Committed models.dev snapshot embedded at compile time, a checked-in overlay for endpoints/auth/protocol/pins, and an optional runtime overlay. Precedence runtime > overlay > snapshot > defaults. Regeneration is explicit through `keel-catalog-gen`, which stays atomic and refuses dropped models without all four pinned facts or a mismatched provenance date. `keel models` and `keel providers` keep their columns and order and are pinned by exact-output tests (M9). |
| 14 | M8 capture scope | **Re-scoped and done.** M8 meant first-party captures from OpenAI's and Anthropic's own endpoints; those need vendor keys we do not have. It now means first-party NeuralWatt captures covering all three implemented protocols: five live route cassettes plus the twelve live eval cassettes. OpenAI- and Anthropic-endpoint-specific captures are dropped from scope, not deferred; Google captures remain M11 (O-008). |

### Open

M8 is closed by the re-scope recorded above, and M10 is done. M11 waits on its
O-item sign-off.

### Deferred

Open design choices in `O-012` (Antigravity default enablement, OAuth client
identity, labeling, account rotation, kill switch) are summarized in
`docs/decisions.md` and only need Arndev's call before scoping M11.
