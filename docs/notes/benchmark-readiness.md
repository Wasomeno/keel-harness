# M12 benchmark readiness: Terminal-Bench and SWE-bench

**Status:** wave 0 landed on `main` at `7c50d58`; PR #31 merged as `f4e7714`; PR #32 merged as `73f79ce`; wave 1 is complete at `5e461aa`: M12.6 merged as `ef4ca13` (PR [#34](https://github.com/Wasomeno/keel-harness/pull/34)), M12.4 as `911427a` (PR [#35](https://github.com/Wasomeno/keel-harness/pull/35)), M12.5 as `7d67a70` (PR [#37](https://github.com/Wasomeno/keel-harness/pull/37)) and M12.7 as `5e461aa` (PR [#38](https://github.com/Wasomeno/keel-harness/pull/38)). A live 10-task Terminal-Bench rerun at clean `5e461aa` measured M12.4 and M12.5 and found no measurable wave-1 gain. A separate ten-instance SWE-bench Lite run exercised M12.6 and M12.7 but is contaminated and is not a benchmark result. Main is `71c6b7d` (docs only).
**Date:** 2026-11-21
**Depends on:** M11 is on hold; this batch can proceed independently
**Related:** `engineering/benchmark-improvement-plan.md` (shared), `engineering/m12-wave1-handoff.md` (shared), `docs/notes/windows-ci.md`, `crates/keel-evals`, `docs/notes/m12-wave0-results.md`, `docs/notes/m12-wave1-results.md`

## 1. Goal

Make keel's benchmark scores trustworthy first, then improve the harness where the
traces show it matters.

The batch has two tracks:

1. **Measurement:** run Terminal-Bench 2.0 and SWE-bench with keel as the harness,
   fixed model and protocol, multiple trials, full traces and a failure taxonomy.
2. **Harness:** fix the failure modes those runs expose, in small PRs, with a
   measured before/after on a fixed subset.

The primary output is not a headline number. It is a repeatable scorecard with
pass@1, pass^k, confidence intervals, cost, wall-clock and trace-backed failure
tags, plus a harness that is better for reasons the traces justify.

## 2. Success criteria

A slice is done only when its own acceptance criteria pass **and** it has a
measured effect on the benchmark subset.

### Batch exit criteria

- Terminal-Bench 2.0: a 3-trial run on a fixed subset and a full 3-trial run at
  the end; results include pass@1, pass^3, 95% intervals, cost per task and
  wall-clock per task.
- SWE-bench: a 3-trial run on a fixed subset of SWE-bench Pro V2 (or a documented
  equivalent) and a full run if budget allows; results include patch-apply rate,
  empty-patch rate, test-run rate and pass@1.
- Every failed task has a failure tag from the taxonomy in section 5.
- The final report compares keel against at least one reference harness with the
  same model and tasks: Terminus 2 for Terminal-Bench, SWE-agent or mini-swe-agent
  for SWE-bench.
- No regression: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `cargo test --workspace`, Windows CI and the existing
  `keel-evals` replay remain green.
- No benchmark overfitting: a canary subset is never used for tuning, and no task
  answer, hidden test or benchmark-specific solution is hardcoded.

### Measurement contract

Every arm in an A/B comparison uses:

- the same provider, model, model version, temperature, max output tokens and
  reasoning effort;
- the same restricted network policy: the agent container has no route off the
  host except a relay to the model endpoint's exact origin and path prefix.
  A run made with `--allow-network` is debugging evidence, not an arm;
- the same benchmark subset, task order and per-task time limit;
- the same tool set and permission policy;
- at least 3 trials per task;
- the same cost accounting and trace schema.

The runner records: task id, trial, model, start/end time, turns, tool calls,
input/output/reasoning tokens, cost, stop reason, wall-clock, patch (if any),
patch size, patch-apply result, verifier result and failure tags.

## 3. Work breakdown

| Slice | Branch | Depends on | Deliverable | Size |
|---|---|---|---|---|
| M12.1 | `m12-harbor-adapter` | #20, #21 | Harbor `KeelAgent` and Linux artifact (landed — `7c50d58`) | M |
| M12.2 | `m12-swebench-runner` | #20, #21 | SWE-bench prediction runner and patch extraction (landed — `1c1d2d0`) | M |
| M12.3 | `m12-bench-traces` | M12.1, M12.2 | Trace schema, metrics, failure taxonomy and fixed subsets (landed — `ec4c871`) | M |
| M12.4 | `m12-terminal-session` | M12.3 | Persistent shell session and opt-in `terminal` tool (landed — `911427a`) | L |
| M12.5 | `m12-terminal-jobs` | M12.4 | Background jobs, streaming output, process groups (merged — `7d67a70`) | L |
| M12.6 | `m12-patch-finalize` | M12.2 | Patch finalization and empty-patch protection (landed — `ef4ca13`) | S |
| M12.7 | `m12-test-loop` | M12.2 | Test discovery, test loop and pre-final verify (landed — `5e461aa`) | M |
| M12.8 | `m12-context-memory` | M12.3 | Compaction working memory (in review) | M |
| M12.9 | `m12-nav-edit` | M12.3 | Search context, line-numbered read, `apply_patch` | M |
| M12.10 | `m12-budget-discipline` | M12.4 | Time/turn budgets, loop detection, premature-finish guard | M |
| M12.11 | `m12-scorecard` | all above | Full 3-trial runs and report | M |

Waves:

- **Wave 0:** M12.1, M12.2, M12.3. **Landed on 2026-11-21** at `7c50d58`;
  no harness behavior changes.
- **Wave 1:** M12.6 (`ef4ca13`, PR #34), M12.4 (`911427a`, PR #35), M12.5
  (`7d67a70`, PR #37) and M12.7 (`5e461aa`, PR #38) are merged at `5e461aa`.
  A live 10-task Terminal-Bench rerun measured M12.4 and M12.5. The terminal
  tool was called zero times, jobs ran on two tasks, and the sweep shows no
  measurable wave-1 gain. A separate ten-instance SWE-bench Lite run exercised
  M12.6 and M12.7, but seven agents fetched reference material and the score is
  contaminated, not a benchmark result.
- **Wave 2:** M12.8, M12.9, M12.10, then M12.11. The wave-1 rerun supplies the
  live baseline on the merged harness, so wave 2 is no longer waiting on that
  measurement decision. M12.11's 3-trial scorecard is still the evidence needed
  for a rate claim.
- **Wave 3:** M12.11. Full runs, report and hand-off.

### Wave 0 results

- **M12.1 Harbor:** head `13d32de`, merged as `7c50d58` (PR #30). Adds the
  `BaseInstalledAgent` adapter and a Linux binary path. A live `hello-world`
  agent-phase smoke completed with exit 0, 3 turns, 2 tool calls, 122 input
  tokens, 172 output tokens and 3.717 s wall clock; the trace strict-validated
  with 26 fields and `verifier_result="not_run"`. Harbor's verifier timed out
  during its own `apt-get`/`uv` setup, so this is not a verifier result.
- **M12.2 SWE-bench:** head `4663e65`, merged as `1c1d2d0` (PR #28). Adds the
  prediction runner, patch extraction, predictions JSONL, `run.json`, trace
  sidecar and official-harness command. One live Lite instance ran (see
  below); `cost_usd` remains null.
- **M12.3 traces:** head `e9593cb`, merged as `ec4c871` (PR #29). Adds schema
  v1.0 (26 fields), the pass@1/pass@k/CI/cost reporter, the 16-tag taxonomy,
  the atomic tag editor, the results template, fixed subsets and the separate
  `benchmarks` CI job.
- **Live results:** all 10 Terminal-Bench iteration tasks ran through Harbor
  0.27.0 at `max_turns=60`. Six tasks passed (`fix-git`, `regex-log`,
  `nginx-request-logging`, `kv-store-grpc`, `fix-code-vulnerability` and
  `custom-memory-heap-crash`); `query-optimize` (900 s agent timeout) and
  `cancel-async-tasks` (`max_turns=60`) failed validly;
  `modernize-scientific-stack` and `extract-elf` are invalid. Eight valid
  trials give a 6/8 = 0.75 valid pass rate; the raw attempted rate is 6/10,
  not a clean benchmark pass@1 because two runs were invalid. All six passing
  logs and both valid failures were scanned for obvious web or reference
  access; none was found beyond package-manager URLs, and the custom-memory
  log has no URLs or fixture access. The per-task table and caveats are in
  `docs/notes/m12-wave0-results.md`. One SWE-bench Lite instance
  (`astropy__astropy-12907`) is confirmed at 1 resolved / 0 unresolved / 0
  empty / 0 errors on the clean 4,712-byte patch; that is one instance, not a
  Lite-wide score.
- **Evidence at the wave-0 merge:** 624 Rust tests passed, 0 failed, 2 ignored;
  the Python benchmark suite is 241 tests OK at `f4e7714`; CI was green on
  Ubuntu, macOS, Windows and the `benchmarks` job. The current `main`
  (`5e461aa`) suite is 679 passed, 0 failed, 2 ignored on macOS and Ubuntu (617
  on Windows in the CI job log), plus 278 Python tests.
- **Residuals:** no native aarch64 Harbor binary; Harbor traces stay
  `verifier_result="not_run"` until `results.json` is merged; live results are
  the completed 10-task iteration attempt (6/8 valid) plus one confirmed
  SWE-bench Lite instance (n=1), not a full score; the contamination check is a
  log scan, not a controlled test; Harbor's verifier setup needs the network
  (`apt`, `astral.sh/uv`) and `modernize-scientific-stack` failed on
  `releases.astral.sh` and `uvx`; `cost_usd` is null until a price source is
  added.
- **Full note:** `docs/notes/m12-wave0-results.md`.

### Wave 1 results

- **Live measurement:** a 10-task Terminal-Bench 2.0 rerun on the binary built
  from clean `5e461aa` (sha256 `f7ffb793…`) used the same tasks, model and
  flags as wave 0, one trial per task, and took 96 minutes. Nine tasks scored
  1.0 and `query-optimize` scored 0.0; `extract-elf` is invalid after fetching
  the task's tests and reference from GitHub. That gives a valid rate of 8/9
  (0.889) and 8/10 treating the invalid task as a failure. On the eight tasks
  valid in both waves the ratio moved
  6/8 to 7/8, but the only flip is `cancel-async-tasks`, which passed in 45
  turns using only `bash`, `read`, `write` and `edit` after failing at
  `max_turns=60`; that is model variance. `modernize-scientific-stack` became
  valid and passed because its verifier could reach `releases.astral.sh`, an
  environment change. **There is no measured gain attributable to wave 1.**
- **Tool use:** across 339 calls the run made 288 `bash`, 17 `jobs`, 14 `read`,
  12 `write`, 7 `edit`, 1 `glob` and 0 `terminal` calls. Total turns fell 291
  to 255 and calls 349 to 339, while agent wall time rose 49.3 to 62.6 minutes.
  The per-task table is in `docs/notes/m12-wave1-results.md`.
- **SWE-bench Lite exploratory run:** a separate ten-instance run at `a035307`
  exercised M12.6 and M12.7. The official harness resolved 9 of 10, but seven
  agents fetched upstream reference material or benchmark data; only astropy,
  django and pytest were clean (3 of 10, not a pass rate). M12.6 produced ten
  clean patches with zero mode pairs, no generated or final test files, sizes
  438 to 2,763 bytes and one attempt each. M12.7 recorded 84 test runs, but
  django recorded 2 of 24 commands and reported "0 failing" after a failed
  suite, and the 75-turn cap cut off verification on pylint and sympy. Not a
  benchmark result; see `docs/notes/m12-wave1-swebench.md`.
- **M12.6 patch finalization:** merged as `ef4ca13` (PR
  [#34](https://github.com/Wasomeno/keel-harness/pull/34)). Adds
  `keel chat --require-changes`, which exits 3 on a clean one-shot workspace
  without masking a failed turn, and SWE-bench patch finalization from one
  base-seeded temporary index with generated-path exclusion, test-edit
  detection, a whitespace gate and one resume-last retry. The exploratory Lite
  run exercised the finalization at ten instances and all ten patches were
  clean; its score is contaminated and the empty-patch retry was not exercised.
- **M12.4 terminal:** merged as `911427a` (PR
  [#35](https://github.com/Wasomeno/keel-harness/pull/35)). The slice adds an
  opt-in `terminal` tool, Unix-only, that keeps one interactive bash on a PTY.
  The default six-tool registry, the default chat request and the SWE-bench
  runner are unchanged. The Harbor adapter passes `--terminal`, but the model
  called `terminal` zero times in 339 calls, so wave 1 provides no evidence
  for this slice. Details and tests are in the M12.4 card below.
- **M12.5 background jobs:** merged as `7d67a70` (PR
  [#37](https://github.com/Wasomeno/keel-harness/pull/37)). The same
  `--terminal` opt-in registers a `jobs` tool with `start`,
  `poll`, `wait`, `kill` and `list`. Jobs run in their own process groups, keep
  a 64 KiB head and 1 MiB tail of output, report absolute offsets for chunked
  reads, cap their timeout by `KEEL_TASK_DEADLINE_MS` (default 10 minutes,
  hard cap 30 minutes), and are killed and reaped at session drop. The wave-1
  sweep called it 17 times on two tasks (`kv-store-grpc` and
  `query-optimize`), with no verifier gain; details are in the M12.5 card below.
- **M12.7 test loop:** merged as `5e461aa` (PR
  [#38](https://github.com/Wasomeno/keel-harness/pull/38)). The SWE-bench
  adapter discovers a likely test command from a bounded read of the repository
  and passes it to `--system` as a hint; the prompt asks for the failing test
  first. `keel chat --test-loop` tracks visible test runs, parses pytest,
  cargo and unittest failures, keeps the status through compaction, and injects
  one pre-final change/test/unverified message. The trace and reporter expose
  test-run evidence. The exploratory Lite run recorded 84 test runs and
  surfaced two defects: django undercounted commands and reported a failed
  suite as "0 failing", and the 75-turn cap cut off verification on pylint and
  sympy. The Harbor adapter still does not pass `--test-loop`, so
  Terminal-Bench cannot measure this slice until the adapter changes. Details
  in the M12.7 card below.
- **Closeout:** `main` at `5e461aa` has 679 Rust tests passed, 0 failed, 2
  ignored on macOS and Ubuntu (617 on Windows in the CI job log), plus 278
  Python tests OK; fmt and clippy are clean. The docs-only `a035307` merge does
  not change those counts. The wave-1 live result is one trial with a valid
  rate of 8/9 (8/10 treating the invalid task as a failure) and no measurable
  harness gain. The exploratory SWE-bench Lite run is contaminated, so M12.6
  and M12.7 still have no valid measurement, and M12.11's 3-trial scorecard is
  still the rate evidence.
- **Full note:** `docs/notes/m12-wave1-results.md` and
  `docs/notes/m12-wave1-swebench.md`.

Each slice is one PR. Do not stack slices unless a dependency forces it; if a
slice grows beyond its size, split it and update this table.

## 4. Task cards

### M12.1 Harbor adapter and Linux artifact

**Branch:** `m12-harbor-adapter`
**Depends on:** #20 (Windows CI), #21 (spinner) merged
**Status:** landed at `7c50d58` (head `13d32de`)

**Goal:** run keel as an installed agent in Terminal-Bench 2.0 through Harbor.

**Scope:**
- Add `benchmarks/harbor/keel_agent.py` with a `BaseInstalledAgent` subclass.
- Build and upload a Linux `keel` binary. Prefer `x86_64-unknown-linux-musl` for
  glibc independence; fall back to a glibc build only if the benchmark images
  require it.
- Install or bundle `rg`, or make `search` fall back to `grep -rn`.
- Run `keel chat <provider>/<model> --workspace <task-dir> --prompt <instruction>
  --yes --max-turns <N>` inside the task container.
- Use Harbor's model connection resolution for the API key and base URL; do not
  invent a parallel credential path.
- Set `KEEL_DATA_DIR` to a writable temp directory; capture the session JSONL.
- Let Harbor run the verifier; do not run the verifier from the adapter.

**Out of scope:** changing keel's default chat behavior; recording benchmark
cassettes; training or fine-tuning.

**Implementation notes:**
- `install()` uploads the binary and `chmod +x`; `configure()` writes runtime
  env and any model config; `run()` executes the one-shot prompt.
- `skip_install=true` must work when the binary is baked into the image.
- Record the agent version, binary checksum and model slug in the trial metadata.
- If the container has no network for package installation, upload a static
  binary and `rg`; do not assume `apt`.

**Tests:**
- Adapter unit test with a fake environment that asserts the command, env,
  workspace and upload paths.
- One `hello-world` Harbor task end to end.
- One Terminal-Bench task that needs a real shell command.

**Docs:** `benchmarks/README.md`, `docs/architecture.md` testing layer,
`docs/notes/benchmark-readiness.md` status.

**Accept:** one Harbor task completes through keel; the trace shows the model,
turns, tool calls, tokens, cost and verifier result; no host-side tool execution.

**Risk:** medium. Container architecture and libc are the main unknowns.

### M12.2 SWE-bench prediction runner and patch extraction

**Branch:** `m12-swebench-runner`
**Depends on:** #20, #21 merged
**Status:** landed at `1c1d2d0` (head `4663e65`)

**Goal:** turn a keel run in a SWE-bench container into a valid predictions file.

**Scope:**
- Add `benchmarks/swebench/run_keel.py`.
- Start each instance from the official SWE-bench image (or Harbor's SWE-bench
  dataset adapter), upload the keel binary, run at `/testbed`.
- Set the workspace to `/testbed`; keep the model endpoint reachable; disable
  MCP and any web tool.
- Produce the patch with `git add -N .` and `git diff <base_commit>`; include new
  files; exclude caches, `.venv`, `__pycache__`, build output and editor files.
- Write predictions JSONL with `instance_id`, `model_name_or_path`, `model_patch`.
- Invoke the official `swebench.harness.run_evaluation`; store its logs.
- Never modify tests; if the agent does, record it and let the official harness
  grade the pristine tests.

**Out of scope:** replacing the official SWE-bench harness; using SWE-bench
Verified for frontier claims; web search for solutions.

**Implementation notes:**
- Validate patch extraction against the gold patch first.
- Cap patch size; reject binary patches with a clear error.
- Keep the agent's session log and the final diff side by side.
- Record whether the patch applies with `git apply --check` before evaluation.

**Tests:**
- Unit tests for patch extraction: modified file, new file, deleted file,
  generated artifact excluded, empty patch, whitespace error.
- Gold-patch evaluation on one instance.
- One keel instance end to end on a small subset.

**Docs:** `benchmarks/README.md`, `docs/notes/benchmark-readiness.md`, a short
note on the predictions format.

**Accept:** one SWE-bench instance produces a patch, the patch applies, and the
official harness reports resolved/unresolved with logs.

**Risk:** medium. Docker images are large and slow; start with one instance.

### M12.3 Trace schema, metrics and failure taxonomy

**Branch:** `m12-bench-traces`
**Depends on:** M12.1, M12.2
**Status:** landed at `ec4c871` (head `e9593cb`)

**Goal:** make every trial explainable and comparable.

**Scope:**
- Define a JSONL trace schema in `benchmarks/README.md`.
- Add a small reporter that aggregates: pass@1, pass^k, 95% bootstrap interval,
  turns, tool calls, tokens, cost, wall-clock, patch size, empty patches,
  patch-apply failures and test-command failures.
- Add a failure tag enum and a `benchmarks/tag_failure.py` helper.
- Add a trace index and `jq` recipes for reading traces.
- Select the fixed iteration subset and the canary subset; write them down.

**Out of scope:** a web UI; LLM-as-judge grading; changing the benchmarks.

**Failure tags:** localization, wrong-file, missed-requirement,
integration-error, wrong-logic, syntax, regression, test-mismatch, test-broken,
timeout, context-overflow, provider-error, patch-failure, environment-error,
premature-finish, loop.

**Tests:**
- Reporter unit tests with synthetic traces.
- A fixture trace with each failure tag.
- Interval calculation tested against known values.

**Docs:** `benchmarks/README.md`, `docs/notes/benchmark-readiness.md`,
shared `engineering/benchmark-results-template.md`.

**Accept:** the baseline run produces a report with every metric above; a reader
can go from a failed task to its trace and tag.

**Risk:** low.

### M12.4 Persistent terminal session

**Branch:** `m12-terminal-session` (merged)
**Depends on:** M12.3
**Status:** merged as `911427a` (PR [#35](https://github.com/Wasomeno/keel-harness/pull/35));
measured in the wave-1 10-task Terminal-Bench rerun, where it was called zero
times, so no effect was observed.

**Goal:** keep shell state across tool calls so terminal tasks do not have to
re-establish `cd`, env and services every turn.

**Scope:**
- Add a persistent terminal abstraction (PTY or tmux-backed) behind a new
  `terminal` tool or an opt-in `bash` session mode.
- Preserve working directory and exported environment across calls.
- Default chat behavior stays one-shot `bash` until the new path is proven; the
  benchmark adapter opts in explicitly.
- Handle non-UTF8 output, signals, exit codes and interactive commands.
- Clean up the session on normal end, error, cancellation and panic.

**Out of scope:** Windows ConPTY parity in this slice; keep the one-shot path for
Windows and document it.

**Tests:**
- `cd` and exported variable persist across calls.
- A command with non-UTF8 output returns a usable lossy result.
- Ctrl-C interrupts the current command without killing the session.
- Session cleanup leaves no child process.
- Replay/regression tests for the default one-shot path.

**Docs:** `docs/architecture.md`, README tool list, `docs/notes/benchmark-readiness.md`.

**Accept:** a synthetic task can `cd`, export a variable and run a command in a
later call without repeating setup; the default path is unchanged.

**Risk:** high. PTY behavior differs across platforms; keep it Unix-only for the
benchmark and feature-gated.

**Result:** a new opt-in `terminal` tool (`--terminal` / `KEEL_TERMINAL=1`) owns
one interactive `bash --noprofile --norc -i` on a `portable-pty` pseudo-terminal.
`cd`, exported variables and services persist; `workdir` changes the persistent
directory and `reset` starts a fresh shell. Commands are framed with a quoted
here-doc and a unique `\x1eKEEL-<nonce>:<status>:<cwd>\x1f` marker, so an
interactive `read` does not consume the harness's own input; a timeout or
cancellation writes Ctrl-C, waits for a fresh marker and keeps the session when
it recovers. Output is bytes until the marker, so non-UTF-8 becomes lossy text;
stdout and stderr are combined because a terminal has one stream; 4 MiB is
retained before the harness truncation. Dropping the tool, or a session, sends
SIGTERM/SIGKILL to the shell's process group, reaps it and detaches the reader
and writer threads; job control is off so ordinary `&` children share that
group. `--terminal` is rejected on Windows and the one-shot `bash` path is
unchanged there. The default registry still has six tools; the Harbor adapter
passes `--terminal` explicitly, and normal chat and the SWE-bench runner do not.
Tests cover `cd`/`export` persistence, `workdir`, non-UTF-8 output, dropping a
call future and keeping the session, an interactive `read` timing out and
recovering, shell exit and restart, process-group cleanup with no surviving
shell or background child, the default one-shot `bash` behavior, the CLI
`tools` opt-in and a full `chat --terminal` turn.

**Live measurement:** the wave-1 10-task Terminal-Bench rerun made 339 tool
calls and none was `terminal`. `bash` remained the only shell path on this
subset, so the rerun says nothing about whether the persistent terminal helps.
The tool was registered and advertised, so this is model choice rather than a
wiring failure.

**Resolved by M12.5:** background jobs, streaming output and per-job process
groups live in a separate `jobs` tool. A command that deliberately starts a new
session (`setsid`) still escapes the terminal's single process-group cleanup;
ordinary `&` jobs do not, and jobs get their own groups.

### M12.5 Background jobs, streaming output and process groups

**Branch:** `m12-terminal-jobs`
**Depends on:** M12.4
**Status:** merged as `7d67a70` (PR
[#37](https://github.com/Wasomeno/keel-harness/pull/37)); measured in the
wave-1 10-task Terminal-Bench rerun, where it ran 17 times on two tasks.

**Goal:** support the commands that make Terminal-Bench hard: builds, training
runs, servers and long logs.

**Scope:**
- Start a command in the background and return a job id.
- Poll for new output, wait with a timeout, and kill the job.
- Keep a bounded output ring buffer; provide head and tail views.
- Kill the whole process group on timeout, but not detached services the task
  still needs.
- Tie the per-command timeout to the task's remaining wall-clock budget; keep a
  safe upper bound.
- Do not deadlock on a grandchild that keeps a pipe open.

**Out of scope:** full job control (`fg`, `bg`, shell job specs); Windows job
objects in this slice.

**Tests:**
- Start a server, poll until ready, run a request, then kill it.
- A command that writes 10 MiB is bounded and readable in chunks.
- A command that forks a grandchild does not hang the harness.
- Timeout kills the process group; an unrelated process survives.
- A background job survives the command that started it until the session ends.

**Docs:** terminal tool spec, architecture, README, notes.

**Accept:** a synthetic Terminal-Bench-style task (server plus test) completes
without a timeout and leaves no orphan process.

**Result:** `--terminal` / `KEEL_TERMINAL=1` now registers `jobs` beside
`terminal`; the default six-tool registry, normal chat and the SWE-bench runner
are unchanged. `start` runs `bash -lc` in its own process group and returns
`job: N` plus the effective timeout; `poll` returns state, absolute offsets and
the requested `new`, `head` or `tail` bytes; `wait` blocks up to `timeout_ms`;
`kill` signals the group; `list` shows every job. Output keeps a 64 KiB head
and the newest 1 MiB, and a `new` read past the retained tail reports skipped
bytes instead of failing. Per-command timeouts default to 10 minutes, are
capped at 30 minutes, and are further capped by `ToolLimits.deadline`; the CLI
sets that from `KEEL_TASK_DEADLINE_MS`, and an embedder can use
`Harness::with_deadline`. A monitor thread kills the group at the deadline even
without another poll. Job status comes from the direct child, not pipe EOF, so
a grandchild holding stdout does not hang `wait`. Drop drains the table, sends
SIGTERM then SIGKILL to every group and reaps the direct children; a `setsid`
service survives as the documented escape hatch. The deadline is wired only
through `KEEL_TASK_DEADLINE_MS` and `Harness::with_deadline`: the Harbor adapter
does not set it, and Harbor's `AgentContext` exposes no task timeout, so tying
jobs to a task's remaining budget is still a manual step.

Coverage is 13 Unix integration tests in `crates/keel-tools/tests/jobs.rs`
(server-plus-test task, 10 MiB bound and chunked reads, grandchild pipe,
timeout versus an unrelated job, `setsid` survival, monitor timeout without a
poll, jobs across foreground calls, drop cleanup, cancelled wait, run deadline,
wait bound, list/id reuse, default registry), four buffer unit tests, and one
`chat --terminal` harness test that runs `start`, `poll` and `kill`. `cargo
fmt`, clippy and the workspace suite were clean at the slice merge (663 passed,
2 ignored; the final wave-1 suite is 679).

**Live measurement:** the wave-1 10-task Terminal-Bench rerun called `jobs` 17
times on two tasks. `kv-store-grpc` backgrounded a `pip install` after the
foreground cap killed it and passed, though the run also hit the 900 s agent
timeout and the verifier still returned 7/7. `query-optimize` backgrounded its
slow query, polled for 444.7 s, hit the same timeout and still failed 2/6. The
tool made progress observable but changed no verifier result.

**Risk:** high. Process-group semantics and pipe ownership are easy to get wrong.

### M12.6 Patch finalization and empty-patch protection

**Branch:** `m12-patch-finalize`
**Depends on:** M12.2
**Status:** merged as `ef4ca13` (PR [#34](https://github.com/Wasomeno/keel-harness/pull/34));
exercised in the exploratory SWE-bench Lite run, where all ten patches were
clean; no valid score because the run was contaminated.

**Goal:** stop losing points to empty, incomplete or noisy patches.

**Result:** `keel chat --require-changes` checks `git status --short` after a
one-shot run and exits 3 with a stable retry hint when the workspace is clean.
A failed or cancelled turn stays a real error and never becomes that exit-3
no-op. The SWE-bench runner's `finalize_patch` staging pass keeps the PR #32 base-seeded
temporary index, and takes the patch, changed-file list and `git diff --check`
gate from that one staged state. Generated paths are excluded from all three.
Test-file edits are recorded in the trace and warned about in the prompt; the
patch is never rewritten. An empty patch or a whitespace-gate failure gets one
`--resume last` retry (`--max-agent-attempts`, default 2); a second whitespace
failure is not submitted and carries `patch-failure`. An empty patch after the
retry is submitted as an empty prediction with `premature-finish`. The trace
adds `agent_attempts`, `retry_reason`, `whitespace_ok` and `whitespace_errors`.
Coverage: empty-run exit 3, new-file staging, generated-artifact exclusion,
test-edit detection, whitespace detection, the retry paths, and the existing
patch-extraction tests. The merged wave-1 suite is 679 Rust tests passed, 0
failed, 2 ignored on macOS and Ubuntu (617 on Windows in the CI job log) and
278 Python tests OK. The exploratory SWE-bench Lite run at ten instances
applied all ten patches cleanly, with zero mode pairs, no generated or final
test files, sizes 438 to 2,763 bytes and one attempt each. Its score is
contaminated and the empty-patch retry path was not exercised, so M12.6 still
has no valid measurement.

**Scope:**
- Add a `--require-changes` CLI option for one-shot runs: after the run, if
  `git status --short` shows no changes in the workspace, exit non-zero with a
  message the adapter can use to retry once.
- Add a finalization helper in the SWE-bench runner: `git add -N .`,
  `git diff <base>`, exclude generated paths, check `git apply --check`.
- Detect test-file edits and record them; warn in the prompt and report.
- Add a `git diff --check` whitespace gate; if it fails, re-prompt or fail the
  trial with a clear tag.

**Out of scope:** automatically fixing a bad patch; silently dropping test edits;
changing the official harness.

**Tests:**
- Empty workspace exits non-zero under `--require-changes`.
- A new file is included after `git add -N`.
- Generated artifacts are excluded.
- A test-file edit is detected and reported.
- A whitespace error is detected.

**Docs:** CLI help, README, benchmarks README.

**Accept:** not measured validly. The exploratory SWE-bench Lite run had no
empty patches or patch-apply failures, but its score is contaminated and the
empty-patch retry path was not exercised. A clean SWE-bench subset with the
retry path is still needed.

**Risk:** low to medium.

### M12.7 Test discovery, targeted test loop and pre-final verify

**Branch:** `m12-test-loop` (merged)
**Depends on:** M12.2, M12.6
**Status:** merged as `5e461aa` (PR [#38](https://github.com/Wasomeno/keel-harness/pull/38)); exercised in the exploratory SWE-bench Lite run, where tests ran in all ten instances and 84 runs were recorded. The run was contaminated and exposed two defects, so no valid measurement. The Harbor adapter still passes `--terminal` but not `--test-loop`.

**Goal:** make the agent run the right tests early and verify before finishing.

**Scope:**
- Add a test-command discovery helper to the benchmark adapter: read README,
  Makefile, CI config and common conventions; pass the discovered command to the
  prompt as a hint, not as a hardcoded answer.
- Prompt the agent to run the failing test first, then the narrower module, then
  the full suite if time allows.
- Parse failing test names and a short failure excerpt; keep them through
  compaction.
- Add a pre-final verify step: before `EndTurn`, ask for what changed, what was
  run and what is still unverified. If visible tests were never run, inject a
  reminder once.

**Out of scope:** a universal test parser; running hidden tests; replacing the
repo's own test command.

**Result:**
- `benchmarks/swebench/test_discovery.py` reads a bounded set of repository
  files plus `git ls-files` through the container's existing `docker exec`
  path. Precedence is CI workflow `run:` step, Makefile `test`/`tests`/`check`
  target, README code span or fence, project config (`pytest`, `tox`,
  `package.json`, Cargo, Go), then layout convention. The result is a hint in
  `--system`; discovery failure or no match leaves it empty and never fails the
  trial. The hint and source are recorded in the trace.
- The system text asks for the failing test first, then the narrower module,
  then the full suite. `keel chat --test-loop` (new, opt-in) tracks `bash` and
  `terminal` test commands, parses pytest (`FAILED`/`ERROR`/`E`), cargo
  (`test ... FAILED` and `failures:`) and unittest (`FAIL:`/`ERROR:`) output,
  and keeps up to eight names plus a short excerpt in a `test_status` session
  record.
  Compaction pins the status in the summary request and appends it to the
  handoff message, so it survives repeated cuts.
- At the first `EndTurn`, the harness injects one user message asking what
  changed, what ran and what is still unverified. It asks for the narrowest test
  when none ran and for a fix or explanation when the last one failed. The
  message is per run; a second `EndTurn` ends normally, and the default chat
  path does not inject it.
- The SWE-bench trace gains `test_command_hint`, `test_command_source`,
  `test_runs`, `tests_run`, `test_last_status`, `failing_tests`,
  `test_excerpt` and `verify_prompts`. The reporter adds a visible test-run
  rate. M12.6 patch finalization, whitespace gating and retry fields are
  unchanged.

**Tests:** discovery from pytest/`pyproject.toml`, Makefile, GitHub Actions,
README and package/Cargo/Go config; command-recognition negatives; bundle and
container-helper parsing; pytest and cargo failure parsers; unittest names; the
pre-final reminder fires once and is opt-in; a tracked failure survives
compaction; a synthetic bug-fix task runs a failing unittest, edits the value,
runs it again successfully and ends after one verification prompt; CLI wiring;
trace and reporter fields.

**Accept:** not measured validly. The exploratory SWE-bench Lite run recorded
84 test runs, but its score is contaminated and it exposed two defects: django
recorded 2 of 24 test commands and reported "0 failing" after a failed suite,
and the 75-turn cap cut off verification on pylint and sympy. No clean subset
has compared the test-run rate or the `premature-finish` rate before and after
the test loop. The acceptance result is **not yet measured**, not a claim that
either rate improved. A clean SWE-bench subset run under restricted egress is
the evidence.

**Follow-up (`m12-swebench-egress`):** the django undercount came from the
recognizer, not the parser cadence: `python tests/runtests.py` was not a
recognised test command, so 23 runs became 2 and the one recorded failure was a
`cd: tests` shell error. The recognizer now accepts `runtests.py`,
`run_tests.py`, `runtest.py`, `test.py` and `python manage.py test`, and the
parser reads unittest `ERROR:` as well as `FAIL:`. A failed run with no parsed
names is reported as such instead of "0 failing test(s)", and a cap-exhausted
run records `stop_reason: max_turns` even when the last model stop was
`end_turn`. The turn budget itself is unchanged. The same branch makes the
restricted network the agent default and records `network_policy`,
`allowed_hosts` and `network_contaminated` in every trace.

**Risk:** medium.

### M12.8 Compaction working memory

**Branch:** `m12-context-memory`
**Depends on:** M12.3
**Status:** implemented, in review. Not merged.

**Goal:** long benchmark tasks do not lose the task, plan, changed files,
commands or failing tests when the transcript is compacted.

**Implemented:**
- `keel-core/src/working_memory.rs` splits the handoff in two. Deterministic
  facts are scanned from the history being dropped: successful `write`/`edit`
  paths, `bash` commands with the parsed `exit: N` status and first non-empty
  output line, and the harness-tracked `TestStatus`. Files and commands keep
  their newest 24 entries, deduplicated and newest last; a repeated path keeps
  its newest position, and a failed or missing `write`/`edit` result is dropped.
  The model never authors these facts.
- The compaction prompt asks for a fenced JSON block first (`goal`, `plan` with
  status, `decisions` with reason, `blockers`, `next_step`), then the prose
  handoff. `parse_summary` accepts a fenced block or a leading object, caps each
  field, and falls back to treating the whole response as the summary when the
  block is absent or malformed. Deterministic facts survive either way.
- A later compaction receives the previous memory and merges instead of
  re-summarizing. Files and commands combine with the new scan; a narrative
  field the response leaves empty inherits its previous value. A fact recorded
  before the first cut is still present after the second.
- The rendered `[keel working memory]` block is appended to the single summary
  message. It has a hard 2,500-token bound using the existing estimator. When
  over, the oldest commands are dropped first, then the oldest files; the goal,
  plan, decisions, blockers and next step are never dropped, and goal/blockers
  are never removed by the trimmer.
- A never-compacted session is unchanged: no scan, no extra summary message, no
  block, and the same request bytes as before.
- Log version 3 stores `working_memory` and `memory` counters on the compaction
  record. `read_with_state` / `SessionLog::open_with_state` restore it on
  resume; the first request after a resume carries the block (injection into the
  request view covers a legacy summary that lacks it). Version-1 and version-2
  logs still load.
- `RunEvent::Compacted`, `CompactionStats` and the CLI status line carry bounded
  `MemoryStats` counters: `memory_bytes`, `memory_tokens`, `files_remembered`
  and `commands_remembered` after trimming, and `compactions_so_far`.

**Out of scope:** a second model or a separate summarizer; changing the default
compaction threshold; a live benchmark sweep (M12.11 owns the measurement).
Keeping the latest raw tool output was dropped from the plan: the design here
records only harness-observed facts plus the model's structured narrative, so
the block stays bounded. Exact recent output still exists in the kept user
messages only to the extent it was a plain user turn.

**Tests:**
- `two_compactions_keep_a_fact_recorded_before_the_first`: two real compactions
  on a synthetic tool history; the file, command, goal, plan, decision and
  blocker survive both and the second summary request receives the previous
  memory.
- `successful_writes_and_edits_are_deduplicated_ordered_and_capped` and
  `bash_commands_keep_their_exit_status_and_first_output_line`: duplicate and
  `./`-normalized paths, a failed write and a missing result, newest-last order
  and the fixed 24-entry cap; command exit status and first line.
- `a_malformed_or_missing_block_falls_back_to_the_whole_response` and
  `a_malformed_structured_block_falls_back_without_losing_observed_facts`.
- `a_resumed_session_carries_its_working_memory_on_the_first_request`: a
  version-3 record is read back and the resumed request carries the block.
- `a_session_that_never_compacted_sends_no_working_memory`: the no-compaction
  request and history are unchanged.
- `the_size_limit_drops_oldest_commands_then_oldest_files`: trim order, with
  the goal and blockers retained.
- `the_rendered_block_is_stable`: literal snapshot of the rendered block.
- `a_compaction_record_round_trips_working_memory_and_its_counters`.
- Existing compaction, harness, store and CLI tests remain green. The branch
  suite is 700 Rust tests passed, 0 failed, 2 ignored on macOS; the Python
  benchmark suite is 297 tests OK. fmt and clippy are clean.

**Docs:** `docs/architecture.md` compaction section, this card.

**Accept:** not measured yet. A clean benchmark run has to show the failure
modes this slice targets. The evidence needed is a fixed-subset run with enough
long tasks to compact at least twice per trial, recording the compaction
records' memory counters and comparing against the wave-1 baseline:
context-overflow failures, re-discovery after a cut (re-reading a file or
re-running a test it already changed), and premature-finish after a lossy
summary. The counters should show memory bytes and file/command counts staying
bounded and not decaying round over round; a drop toward zero is a merge defect,
and a block pinned at the trim limit across cuts is a prompt-quality signal.
No live sweep was run for this slice.

**Risk:** medium. Model-authored fields can still be stale or wrong; the
deterministic facts and test status are the durable part.

### M12.9 Search context, line-numbered read and `apply_patch`

**Branch:** `m12-nav-edit`
**Depends on:** M12.3

**Goal:** make repo navigation and multi-hunk edits cheaper and more reliable.

**Scope:**
- `read`: add an optional `numbered` output and keep the current raw output as
  the default for exact edits.
- `search`: add `context` (before/after lines), a result cap and a `grep -rn`
  fallback when `rg` is missing.
- `apply_patch`: add a unified-diff tool for multi-hunk edits, with path safety,
  dry-run and a clear failure when the patch does not apply. Keep `edit` as the
  exact-match fallback.
- Update tool descriptions, golden requests and `keel-evals` tool-text gate.

**Out of scope:** fuzzy matching; a full symbol index; LSP integration.

**Tests:**
- Numbered read matches the file and preserves offsets.
- Search context includes the requested lines and still sorts deterministically.
- `grep` fallback works when `rg` is absent.
- `apply_patch` applies a multi-file patch, rejects an escaping path, rejects a
  stale patch and leaves the file untouched on failure.
- Tool-text and cassette replay remain green.

**Docs:** README tool table, architecture, tool descriptions.

**Accept:** on the SWE-bench subset, edit-failure and localization tags fall
against the baseline.

**Risk:** medium. `apply_patch` is a new attack surface; reuse the workspace
path checks and atomic write path.

### M12.10 Time/turn budgets, loop detection and premature-finish guard

**Branch:** `m12-budget-discipline`
**Depends on:** M12.4, M12.6

**Goal:** spend the benchmark's time and turns on progress, not loops.

**Scope:**
- Add a per-task wall-clock budget and a graceful stop reason; do not kill a
  command mid-write without flushing the session log.
- Add a turn budget separate from the existing default; benchmark adapters set
  it explicitly (SWE-bench hard tasks need more than 24).
- Detect repeated identical tool calls, repeated failing commands and long
  stretches without file changes; inject one strategy-change nudge.
- Detect a premature finish: no changes, no test run or failing visible tests;
  warn once and continue.
- Record every nudge in the trace.

**Out of scope:** changing the default turn count for normal chat; a model-based
critic.

**Tests:**
- Deadline stops a synthetic task with a typed stop reason.
- Loop detector fires on identical calls and does not fire on legitimate retries.
- Premature-finish warning fires once.
- Trace records the nudges.

**Docs:** CLI help, architecture, README, notes.

**Accept:** on both subsets, loop/timeout tags fall and median turns do not grow
without a pass@1 gain.

**Risk:** medium.

### M12.11 Full scorecard and report

**Branch:** `m12-scorecard`
**Depends on:** M12.1-M12.10

**Goal:** produce the batch's evidence.

**Scope:**
- Run Terminal-Bench 2.0 full, 3 trials, fixed model.
- Run the chosen SWE-bench subset, 3 trials; full run if budget and time allow.
- Run the reference harness on the same model and tasks for comparison.
- Report pass@1, pass^k, 95% intervals, cost/task, wall-clock/task, patch-apply
  rate, empty-patch rate and failure tags.
- Publish `docs/notes/benchmark-results.md` and a shared
  `engineering/benchmark-results-<date>.md`.
- Archive traces (compressed if large) and record checksums.

**Out of scope:** public leaderboard submission; claims that a keel score is a
model score.

**Accept:** the report is reproducible from the checked-in scripts and pinned
versions; the numbers include intervals and costs; the comparison is same-model,
same-task, same-protocol.

**Risk:** medium. Cost and wall-clock are the main constraints; use subsets and
parallelism deliberately.

## 5. Measurement protocol

### Trials and metrics

For each task and trial, record success. Report:

- **pass@1:** mean per-task success across trials.
- **pass^k:** fraction of tasks where all k trials succeed. Use k = 3.
- **95% interval:** bootstrap over tasks, not over trials, and say so.
- **cost:** USD per solved task and total per run.
- **wall-clock:** mean and p95 per task.
- **harness health:** tool errors, context overflows, provider retries, empty
  patches, patch-apply failures, test-command failures, loops and nudges.

### Subsets

- **Iteration subset:** 10 Terminal-Bench tasks and 10 SWE-bench tasks, chosen to
  cover the failure tags and kept fixed for the batch.
- **Canary subset:** at least 20% of each full benchmark, never used for tuning.
- **Full:** Terminal-Bench 2.0 (89 tasks) and the chosen SWE-bench split. Full
  runs only at the start and end of the batch.

### Comparing changes

- One harness change per branch.
- A/B on the same subset, 3 trials, same model and limits.
- Compare per-task outcomes, not only aggregates; report a paired bootstrap
  interval on the difference.
- A change is promoted only if the subset improves and the canary does not get
  worse. If the result is flat, say so and keep the change only if it removes a
  known failure mode.

## 6. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Model capability dominates and masks harness gains | Fix the model; compare against a reference harness with the same model; report both |
| Benchmark cost and wall-clock | Start with 10-task subsets; run full only at milestones; cache images; parallelize where safe |
| Container arch/libc mismatch | Prefer musl; test on the actual benchmark image; publish the binary checksum |
| `rg` missing in an image | Bundle `rg` or fall back to `grep -rn`; test both paths |
| PTY portability | Keep the new terminal Unix-only and opt-in; keep one-shot `bash` as the default until proven |
| Process leaks | Process-group cleanup tests; assert no orphan after every job test |
| Benchmark overfitting | Fixed canary; no task answers or hidden tests in the prompt; review traces |
| Adapter bugs look like model failures | Gold-patch validation; patch-apply checks; adapter unit tests |
| Long runs exceed the session/context | Deadline, turn budget, working memory and trace checkpoints |
| Docs drift | Each slice updates README, architecture, decisions when applicable, and this note |

## 7. Open decisions

1. **Model and budget.** Which provider/model is the fixed benchmark model, and
   what is the per-run cap? The plan assumes one strong tool-calling model; the
   model must be recorded with its version.
2. **Harbor integration.** Installed agent with an uploaded binary is the
   assumed path. Confirm that Harbor's task images allow the model endpoint and
   that a static binary is acceptable.
3. **Terminal backend.** Resolved for M12.4: one PTY-backed interactive bash
   per session via `portable-pty`, exposed as the opt-in Unix `terminal` tool.
   tmux was rejected because benchmark images are not guaranteed to ship it, and
   a session mode on `bash` would change the default tool schema and request.
   Windows ConPTY parity remains out of scope.
4. **`rg` packaging.** Bundle ripgrep (MIT) or fall back to `grep`. Bundling
   increases artifact size; fallback is smaller but changes output formatting.
5. **SWE-bench split.** Start with a fixed subset of SWE-bench Pro V2 public
   tasks; use SWE-bench Lite only for adapter bring-up, not for frontier claims.
6. **Results visibility.** Keep the first scorecard internal to the shared
   folder; decide later whether to publish.

## 8. Out of scope

- Training or fine-tuning a model.
- Changing benchmark tasks, tests or verifiers.
- Web search for solutions; modifying tests; hardcoding task answers.
- A public leaderboard submission or a claim that a keel score is a model score.
- Windows ConPTY parity in this batch; Windows CI must stay green, but the new
  terminal path is Unix-first.
- Multi-agent scaffolding until the single-agent foundation is measured.

## 9. Definition of done for the batch

- All slices merged through reviewed PRs, each with tests and docs.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace`, Windows CI and `keel-evals` replay green.
- The benchmark scorecard exists with 3 trials, intervals, cost and wall-clock.
- The failure taxonomy and traces are checked in or archived with checksums.
- The shared note summarizes what moved, what did not, and what to try next.
- The next batch is chosen from the remaining failure tags, not from intuition.

## 10. References

- Harbor custom agents: <https://docs.harborframework.com/agents/custom-agents.md>
- Harbor / Terminal-Bench 2.0: <https://github.com/harbor-framework/harbor>
- SWE-bench evaluation: <https://www.swebench.com/SWE-bench/guides/evaluation/>
- SWE-bench harness: <https://www.swebench.com/SWE-bench/reference/harness/>
- Anthropic eval guidance: <https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents>
- DeepSWE failure taxonomy: <https://arxiv.org/html/2607.07946v1>
- Terminal-Bench 2.0 methodology: <https://www.vals.ai/benchmarks/terminal-bench-2>
- Shared high-level plan: `engineering/benchmark-improvement-plan.md`
