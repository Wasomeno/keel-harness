# M12 benchmark readiness: Terminal-Bench and SWE-bench

**Status:** wave 0 landed on `main` at `7c50d58`; PR #31 merged as `f4e7714`; PR #32 merged as `73f79ce`; wave 1 is complete at `5e461aa`: M12.6 merged as `ef4ca13` (PR [#34](https://github.com/Wasomeno/keel-harness/pull/34)), M12.4 as `911427a` (PR [#35](https://github.com/Wasomeno/keel-harness/pull/35)), M12.5 as `7d67a70` (PR [#37](https://github.com/Wasomeno/keel-harness/pull/37)) and M12.7 as `5e461aa` (PR [#38](https://github.com/Wasomeno/keel-harness/pull/38)). A live 10-task Terminal-Bench rerun at clean `5e461aa` measured M12.4 and M12.5 and found no measurable wave-1 gain. A separate ten-instance SWE-bench Lite rerun under restricted egress measured M12.6 and M12.7 at 7/10 raw and 7/9 environment-valid; it has no control arm and is not a Lite-wide score. Wave 2 is merged: M12.8 `e1fc23f` (PR [#43](https://github.com/Wasomeno/keel-harness/pull/43)), M12.9 `f3a4136` (PR [#44](https://github.com/Wasomeno/keel-harness/pull/44)), the relay body-drain fix `472d9b0` (PR [#45](https://github.com/Wasomeno/keel-harness/pull/45)) and M12.10 `78e0efb` (PR [#46](https://github.com/Wasomeno/keel-harness/pull/46)). Main is `78e0efb` with 766 Rust tests passed, 0 failed, 2 ignored and 306 Python tests OK (three consecutive runs after the relay flake fix). M12.11, the three-trial scorecard, is in progress; no result or rate claim exists yet.
**Date:** 2026-11-22
**Depends on:** M11 is on hold; this batch can proceed independently
**Related:** `engineering/benchmark-improvement-plan.md` (shared), `engineering/m12-wave1-handoff.md` (shared), `docs/notes/windows-ci.md`, `crates/keel-evals`, `docs/notes/m12-wave0-results.md`, `docs/notes/m12-wave1-results.md`, `docs/notes/m12-wave1-swebench.md`

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
| M12.8 | `m12-context-memory` | M12.3 | Compaction working memory (merged — `e1fc23f`, PR [#43](https://github.com/Wasomeno/keel-harness/pull/43)) | M |
| M12.9 | `m12-nav-edit` | M12.3 | Search context, line-numbered read, `apply_patch` (merged — `f3a4136`, PR [#44](https://github.com/Wasomeno/keel-harness/pull/44)) | M |
| M12.10 | `m12-budget-discipline` | M12.4 | Time/turn budgets, loop detection, premature-finish guard (merged — `78e0efb`, PR [#46](https://github.com/Wasomeno/keel-harness/pull/46)) | M |
| M12.11 | `m12-scorecard` | all above | Full 3-trial runs and report (in progress; no result or rate claim yet) | M |
| M13.1 | `m13-bash-cwd` | M12.3 trace evidence | Persistent one-shot `bash` working directory, default on (in review) | S |
| M13.2 | `m13-remove-terminal` | M13.1 | Remove the opt-in `terminal` tool; give `jobs` its own `--jobs` / `KEEL_JOBS=1` opt-in (open — PR [#53](https://github.com/Wasomeno/keel-harness/pull/53)) | S |

Waves:

- **Wave 0:** M12.1, M12.2, M12.3. **Landed on 2026-11-21** at `7c50d58`;
  no harness behavior changes.
- **Wave 1:** M12.6 (`ef4ca13`, PR #34), M12.4 (`911427a`, PR #35), M12.5
  (`7d67a70`, PR #37) and M12.7 (`5e461aa`, PR #38) are merged at `5e461aa`.
  A live 10-task Terminal-Bench rerun measured M12.4 and M12.5. The terminal
  tool was called zero times, jobs ran on two tasks, and the sweep shows no
  measurable wave-1 gain. A separate ten-instance SWE-bench Lite rerun under
  restricted egress measured M12.6 and M12.7 at 7/10 raw and 7/9
  environment-valid. All ten runs recorded the allowlist policy and no
  non-provider host was reached; it is a one-trial subset, not a Lite-wide
  score.
- **Wave 2:** M12.8 (`e1fc23f`, PR #43), M12.9 (`f3a4136`, PR #44) and
  M12.10 (`78e0efb`, PR #46) are merged; the relay body-drain fix is
  `472d9b0` (PR #45). The wave-1 rerun supplies the live baseline on the merged
  harness. M12.11's 3-trial scorecard is in progress and no result or rate
  claim exists yet.
- **Wave 3:** M12.11, in progress. Full runs, report and hand-off.

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
- **SWE-bench Lite restricted-egress rerun:** the ten-instance subset ran at
  `e5a1707` with the agent container restricted to the model endpoint. It
  resolved 7 of 10 raw, or 7 of 9 (0.778) after excluding `requests` as
  environment-invalid; `requests` stays in the raw denominator. All ten runs
  recorded `network_policy="allowlist"`, allowed host `api.neuralwatt.com` and
  `network_contaminated=false`; 14 of 16 network-tool commands were blocked and
  no non-provider host was reached. The earlier open-egress run resolved 9 of 10
  only after seven agents fetched reference material, and only three of ten were
  clean. M12.6 produced nine non-empty patches, all clean, with zero mode pairs,
  no generated or test-file edits and sizes 449 to 4,296 bytes; the empty-patch
  retry was exercised on sphinx. M12.7 recorded 127 test runs, and django's count
  rose from 2 to 21. There is no control arm, so this is not a score claim; see
  `docs/notes/m12-wave1-swebench.md`.
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
  The default tool registry, the default chat request and the SWE-bench
  runner are unchanged. The Harbor adapter passed `--terminal` at the time, but
  the model called `terminal` zero times in 339 calls, so wave 1 provides no
  evidence for this slice; M13.2 removes the tool. Details and tests are in the
  M12.4 card below.
- **M12.5 background jobs:** merged as `7d67a70` (PR
  [#37](https://github.com/Wasomeno/keel-harness/pull/37)). The same
  `--terminal` opt-in registered a `jobs` tool (M13.2 later moved it to
  `--jobs` / `KEEL_JOBS=1`) with `start`,
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
  test-run evidence. The final restricted-egress Lite run recorded 127 test
  runs; django rose from 2 to 21 and produced a real `FAILED (errors=19)`
  excerpt, and `stop_reason` reports `max_turns` on every capped run. The
  earlier undercount and shell-error defects are fixed. The Harbor adapter still
  does not pass `--test-loop`, so Terminal-Bench cannot measure this slice until
  the adapter changes. Details in the M12.7 card below.
- **Closeout:** the wave-1 closeout at `e5a1707` had 686 Rust tests passed, 0
  failed, 2 ignored on macOS and Ubuntu (621 on Windows in the CI job log),
  plus 297 Python tests OK; fmt and clippy were clean. The wave-1 live result is
  one trial with a valid rate of 8/9 (8/10 treating the invalid task as a
  failure) and no measurable harness gain. The restricted-egress SWE-bench Lite
  rerun resolved 7/10 raw and 7/9 environment-valid; it has no control arm, so
  M12.6 and M12.7 function but no score improvement is claimed. M12.11's
  3-trial scorecard is in progress; no rate claim exists yet.
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

**Result:** at merge, a new opt-in `terminal` tool (`--terminal` /
`KEEL_TERMINAL=1`) owned one interactive `bash --noprofile --norc -i` on a
`portable-pty` pseudo-terminal.
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
group. `--terminal` was rejected on Windows and the one-shot `bash` path was
unchanged there. The default registry still had seven tools; the Harbor adapter
passed `--terminal` explicitly, and normal chat and the SWE-bench runner did
not. Tests covered `cd`/`export` persistence, `workdir`, non-UTF-8 output,
dropping a call future and keeping the session, an interactive `read` timing
out and recovering, shell exit and restart, process-group cleanup with no
surviving shell or background child, the default one-shot `bash` behavior, the
CLI `tools` opt-in and a full `chat --terminal` turn.

**Live measurement:** the wave-1 10-task Terminal-Bench rerun made 339 tool
calls and none was `terminal`. `bash` remained the only shell path on this
subset, so the rerun says nothing about whether the persistent terminal helps.
The tool was registered and advertised, so this is model choice rather than a
wiring failure.

**Removed by M13.2:** with `bash` now remembering its directory and `jobs`
covering long-running work, the 0-of-339 result was the evidence to delete the
tool rather than keep advertising a second shell. The module, its 15 tests,
`portable-pty`, `--terminal` / `KEEL_TERMINAL=1`, and every registry and doc
reference are gone. `jobs` now has its own `--jobs` / `KEEL_JOBS=1` opt-in.

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

**Result:** at merge, `--terminal` / `KEEL_TERMINAL=1` registered `jobs` beside
`terminal`; M13.2 moved the tool to `--jobs` / `KEEL_JOBS=1` and removed
`terminal`. The default tool registry, normal chat and the SWE-bench runner
remain unchanged. `start` runs `bash -lc` in its own process group and returns
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
`chat --jobs` harness test that runs `start`, `poll` and `kill`. `cargo
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
exercised in the final restricted-egress SWE-bench Lite rerun. Nine non-empty
patches were clean and the empty-patch retry ran end to end on sphinx; sympy
still submitted an empty patch. No control arm, so no score claim.

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
278 Python tests OK. The final restricted-egress SWE-bench Lite rerun produced
nine non-empty patches, all clean: zero mode pairs, no generated or final test
files, sizes 449 to 4,296 bytes. The empty-patch retry was exercised for the
first time. Sphinx spent 75 turns without a workspace change, the retry produced
the 4,296-byte patch that applied cleanly, and the target test still failed.
Sympy exhausted two 75-turn attempts and still submitted an empty patch. The
retry path works end to end; it also costs a second full turn budget. With no
control arm this is a functional result, not a score improvement.

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

**Accept:** partially met. The restricted-egress run exercised both retry
outcomes: sphinx's retry produced a clean 4,296-byte patch that applied, while
sympy's retry still submitted an empty patch. Patch-apply and hygiene checks
passed. There is no control arm, so the "reduced against the baseline" part of
the acceptance is still not measured.

**Risk:** low to medium.

### M12.7 Test discovery, targeted test loop and pre-final verify

**Branch:** `m12-test-loop` (merged)
**Depends on:** M12.2, M12.6
**Status:** merged as `5e461aa` (PR [#38](https://github.com/Wasomeno/keel-harness/pull/38)); exercised in the final restricted-egress SWE-bench Lite rerun, where tests ran in all ten instances and 127 runs were recorded. Django's count rose from 2 to 21, `stop_reason` reports `max_turns` on every capped run, and the old `cd: tests` error is gone. There is no control arm, so this is not a score claim. The Harbor adapter still passes `--jobs` but not `--test-loop`.

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
- The final restricted-egress run recorded 127 test runs across all ten
  instances. Django rose from 2 to 21 runs, the `cd: tests` shell error is gone,
  and the trace contains a real `FAILED (errors=19)` excerpt. `stop_reason`
  reports `max_turns` on matplotlib, pylint, sphinx and sympy. Discovery still
  does not reliably select the covering test.

**Tests:** discovery from pytest/`pyproject.toml`, Makefile, GitHub Actions,
README and package/Cargo/Go config; command-recognition negatives; bundle and
container-helper parsing; pytest and cargo failure parsers; unittest names; the
pre-final reminder fires once and is opt-in; a tracked failure survives
compaction; a synthetic bug-fix task runs a failing unittest, edits the value,
runs it again successfully and ends after one verification prompt; CLI wiring;
trace and reporter fields.

**Accept:** partially met. The final restricted-egress run recorded 127 test runs
and fixed django's test-count accounting, the `FAILED (errors=19)` excerpt and
the cap `stop_reason`. There is still no control arm, so the test-run-rate and
`premature-finish` comparison is not measured and no score improvement is
claimed. A control-arm subset run is the evidence for that part.

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
**Status:** merged at `e1fc23f`
([PR #43](https://github.com/Wasomeno/keel-harness/pull/43)).

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
- Existing compaction, harness, store and CLI tests remain green. At the M12.8
  merge the suite was 700 Rust tests passed, 0 failed, 2 ignored on macOS; the
  Python benchmark suite was 297 tests OK. fmt and clippy were clean.

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
**Status:** merged at `f3a4136`
([PR #44](https://github.com/Wasomeno/keel-harness/pull/44)); follow-up parser
diagnostics in [PR #49](https://github.com/Wasomeno/keel-harness/pull/49); not
measured.

**Goal:** make repo navigation and multi-hunk edits cheaper and more reliable.

**Implemented:**
- `read` numbers every returned line as a right-aligned 1-based number, `→`,
  then the file line (`    12→fn main() {`). The number is the real file line
  even when `offset` starts mid-file; `offset` stays 0-based, `limit` still
  counts file lines, and `[lines A-B of N]`, `[empty file]` and the 64 KiB
  truncation are unchanged.
- `edit` first tries the literal `old_string`. If that finds nothing and the
  block is a uniform, consecutive read gutter, it strips the gutter and tries
  once more; `new_string` loses its gutter only when it has the same shape. A
  literal match always wins, so file content that begins with digits or even a
  gutter-like line is not rewritten.
- `write` uses the same conservative shape: when every line of `content` is a
  uniform, consecutive read gutter, it strips the gutter and reports the strip
  in the result. Mixed or narrow non-gutter content is written verbatim.
- `search` adds `context` (0-20 lines, clamped) and `before`/`after` overrides.
  Matches remain `path:line:text`; context is `path-line-text`, non-adjacent
  groups are separated by `--`, and `max_results` counts matches rather than
  context lines. With no context argument the output is byte-identical to the
  previous format.
- `apply_patch` joins the default registry. It parses `---`/`+++`/`@@` unified
  diffs with several hunks and several files, ignores `a/`/`b/` prefixes,
  timestamps, `diff --git` and `index` lines, honors
  `\ No newline at end of file`, and can create a file from `--- /dev/null`.
  Context and removed lines match strictly on content and leading whitespace
  but tolerate trailing whitespace. Every hunk is applied to an in-memory copy
  before any write; a mismatch reports the file, hunk number, expected line and
  closest actual line. When every context and removed line carries a read
  gutter, the mismatch also says to remove the line-number prefix; the patch is
  never stripped or shifted automatically. Writes use the existing atomic
  workspace path, and a write failure rolls earlier files back best-effort.
  Deleting, renaming,
  binary patches, mode changes, dry-run and fuzzy matching are rejected or
  absent; the tool description and README say it is not full `git apply`.
- The eval tool-text gate now refreshes the recorded `read` results to the
  numbered form in addition to the tool array and prompt cache key, so offline
  cassette replay still matches the live tool output.

**Reliability (M12.11 trial 0, `78e0efb`):** `apply_patch` was called 13 times
across ten SWE-bench instances: 1 success, 12 failures. All 12 were model-side:
7 hunk headers declared the wrong counts in both directions (for example `-17
+13` for a body with 17 old and 12 new lines, and `-11 +14` for 11 old and 15
new lines); 3 errors were a wrong count reaching the next `@@` or a
Codex-style `*** End Patch`; 1 context start was one line late (line 318
instead of 317); 1 header was one line short and left a body line outside the
hunk. The parser is not loosened. Count errors now report the declared and
actual body counts and say to fix the header or use `edit`; `*** End Patch` is
named as a Codex-style marker that is not part of a unified diff. This is a
low-yield tool in this sample: if a second run shows the same ratio, removing
it from the default registry is the better trade than carrying a tool the model
cannot drive. It is not removed in this PR.

**Decisions differing from the original card:** numbering is the default
rather than an opt-in `numbered` flag. The card's raw-default design would have
left every ordinary read unaddressable and needed a prompt change to make the
model opt in; the edit tolerance removes the round-trip cost of the numbered
default. The planned `grep -rn` fallback was not added because the orchestrator's
slice spec asked for bounded context and kept the existing ripgrep error text;
a missing `rg` still returns the actionable install/`grep` message. `apply_patch`
has no `dry-run` flag: all hunks are validated before the first write, which is
the useful part of a dry run, and a preview mode would add a second output
contract without a caller.

**Out of scope:** fuzzy matching; a full symbol index; LSP integration; a live
benchmark sweep (M12.11 owns the measurement).

**Tests:**
- `read`: first page, partial range, last page, empty file, single line without
  a trailing newline, digit-leading content, and gutter-like content, with the
  original line numbers and footers preserved.
- `edit`: a block copied from `read` replaces correctly, and a literal
  gutter-like line still matches literally.
- `write`: a wholly numbered block lands as clean content and reports the
  strip; mixed content and a single narrow `12→x` line are written verbatim.
- `search`: no-context output is unchanged; context before/after, near the file
  start and end, `--` group separation, `before`/`after` overrides, the result
  cap, the 20-line context cap and output truncation.
- `apply_patch`: single hunk, several hunks in one file, several files, exact
  summary counts, a mismatch naming the hunk and closest line, a gutter-bearing
  hunk failing with the remove-the-prefix hint and no write, path traversal
  with no write, a failing second hunk leaving every file untouched,
  trailing-whitespace tolerance with strict content, new-file creation and the
  no-newline marker.
- `apply_patch` replays the real M12.11 sample: the one successful pytest
  patch, representative django and pylint count errors, the Codex terminator,
  the sphinx extra body line and the sphinx context mismatch, plus a correct
  header with a trailing newline.
- Registry: the default names are exactly `apply_patch`, `bash`, `edit`,
  `glob`, `read`, `search`, `write`, with `terminal` and `jobs` absent; the CLI
  `--jobs` path advertises `jobs` and never `terminal`.
- Golden requests refreshed for the new `read` description; eval cassettes
  refreshed for the tool text, cache key and numbered read results; all 13
  replay tests remain green.

**Docs:** README built-in tool table and format notes, `docs/architecture.md`
tools section and opt-in wording, this card.

**Accept:** not measured. On the SWE-bench subset, the evidence needed is a
drop in edit-failure and localization tags against the wave-1 baseline. No live
sweep was run for this slice.

**Risk:** medium. `apply_patch` is a new attack surface; it reuses the
workspace path checks and atomic write path, and validates every hunk before
writing. It is also low-yield at 1/13 in the first SWE-bench sample; the
next-run removal criterion is recorded above.

### M12.10 Time/turn budgets, loop detection and premature-finish guard

**Branch:** `m12-budget-discipline`
**Depends on:** M12.4, M12.6
**Status:** merged at `78e0efb`
([PR #46](https://github.com/Wasomeno/keel-harness/pull/46)); not measured.

**Goal:** spend the benchmark's time and turns on progress, not loops.

**Implemented:**
- Per-run and per-phase progress counters (`turns`, `mutations`,
  `workspace_changes`, `test_runs`, `test_improvements`, `new_information`) in
  `keel-core/src/budget.rs`. A successful `write`, `edit` or `apply_patch`
  counts a mutation; a bounded metadata scan of the workspace (12,000 entries,
  skipping VCS/dependency/cache directories) counts a change without requiring
  git or a watcher; a recognised visible test run counts a new failing set, a
  strict subset of the previous failing names, or a pass as an improvement; a
  distinct tool result counts new information (capped at 512 results).
- One `RunEvent::Progress` per completed turn and the same snapshot on
  `RunOutcome::progress`, including idle turns, identical-call streak,
  unshrunk-test-run streak, distinct results, soft-budget prompts and verify
  prompts.
- Three count-based stall rules, all checked after a tool turn, each stopping
  the run with its own label and a one-sentence diagnosis naming the signal,
  the last observation and the cheapest next step:
  - `stalled_repeated_call` after 3 identical calls (name + raw arguments);
  - `stalled_test_failures_unshrunk` after 3 failed visible test runs with no
    strict, non-empty subset of the previous failing set;
  - `stalled_no_progress` after 8 completed turns with no mutation, workspace
    change, test improvement or new tool result.
  The diagnosis is appended to history as a `[keel stall]` message so a
  `--resume` retry sees it.
- One soft-budget message strictly below 5 remaining turns, only when no
  mutation exists yet, never repeated, and never sent to a run that already
  changed a file. The hard turn cap still returns `max_turns`.
- A premature-finish guard on a model `EndTurn`, after the test-loop verify
  prompt: `premature_finish_no_changes` when no mutation exists, and
  `premature_finish_no_test` when a mutation exists, the adapter set
  `--expect-tests`, and no visible test ran. A genuine failure or cancellation
  keeps its own stop.
- A task deadline stop at the next turn boundary (`task_timeout`) and honest
  stop labels for every path: `end`, `max_turns`, `task_timeout`,
  `stalled_*`, `premature_finish_*`, `cancelled`, `failed`. A harness cut never
  reports the model's `end_turn`.
- CLI wiring: `--budget-discipline` selects the preset, `--expect-tests` says a
  test command was discovered, `KEEL_STALL_*` overrides each threshold, and
  stdout gains one `[budget]` counters line and one `[stop]` detail line.
  Default `keel chat` leaves every rule off.
- Adapters: the SWE-bench runner passes `--budget-discipline` on the first and
  retry attempts and `--expect-tests` only when its discovery found a test
  command; the Harbor agent passes `--budget-discipline`. The SWE-bench trace
  parses the two lines into `progress`, `stop_detail` and
  `harness_health.loops`/`nudges`; the trace schema accepts those optional
  fields and the reporter keeps the new labels in `stop_reasons`. Failure tags
  gained `loop` for a stall, `premature-finish` for either premature label and
  `timeout` for a task deadline.

**Decisions differing from the original card:**
- The card said a premature finish should "warn once and continue". The slice
  brief requires a distinct stop reason recorded in the trace, so it is a hard
  stop with two labels. `--require-changes` still runs the same `git status
  --short` check and still exits 3 when the workspace is clean.
- The card said "inject one strategy-change nudge" for loops. The brief requires
  a stall to stop the run with a distinct signal, so the diagnosis is a stop
  plus a history message; the soft-budget reserve is the one nudge.
- The card's "turn budget separate from the existing default" already existed
  as `--max-turns`; adapters keep their own values (75 SWE-bench, 60 Harbor).
  This slice did not change the interactive default of 24.
- Workspace change detection is a bounded metadata scan rather than a watcher:
  the harness has no watcher, and a scan works in non-git workspaces. Mutation
  counters remain the primary signal; the scan is capped, so a very large tree
  is only partially observed.

**Out of scope:** changing compaction, tools or retry logic; a model-based
critic; a live benchmark sweep (M12.11 owns measurement).

**Tests:**
- Each stall shape fires at its threshold and not one turn below; a converging
  run that mutates early and investigates for many turns is not stopped.
- The soft-budget message appears once at the reserve boundary and never after a
  mutation.
- A sympy-shaped run (no mutation, no test, long cap) stalls before the cap and
  names the signal; a sphinx-shaped first empty attempt does not block the
  retry.
- Premature finish fires for no mutation and for a mutation without a visible
  test when tests are expected, and does not relabel a genuine failure or
  cancellation; `--require-changes` still exits 3 for a clean workspace.
- The stop-reason table gives every path a distinct label and never gives a
  harness cut the model's `end`/`end_turn`.
- Phase counters, one progress event per turn, a bash-only workspace change, a
  task deadline at a turn boundary, and CLI `[budget]`/`[stop]` output.
- SWE-bench parsing of `[budget]`/`[stop]` lines and the new labels, Harbor
  summary parsing of the new labels, and schema validation for `progress` and
  `stop_detail`.

**Docs:** README chat and environment tables, `docs/architecture.md` agent loop
and budget-discipline section, trace README schema fields, this card, and the
shared `engineering/m12.10-budget-discipline.md` threshold table.

**Accept:** not measured. The evidence needed is a fixed-subset run showing
`loop` and `timeout` tags falling against the wave-1 baseline without median
turns growing absent a pass@1 gain. No live sweep was run for this slice.

**Risk:** medium. The thresholds are deterministic but heuristic; the repeated
call rule can also catch a legitimate identical retry, which is why each rule
has its own threshold and can be disabled.

### M12.11 Full scorecard and report

**Branch:** `m12-scorecard`
**Depends on:** M12.1-M12.10
**Status:** in progress; no result or rate claim exists yet.

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

### M13.1 Persistent one-shot `bash` working directory

**Branch:** `m13-bash-cwd`
**Depends on:** M12.3 trace evidence
**Status:** merged at `f29db3f`
(PR [#50](https://github.com/Wasomeno/keel-harness/pull/50)); no live benchmark
measurement of this slice yet.

**Goal:** stop the model paying tokens to re-establish a directory the shell
already changed.

**Evidence:** across the 339 calls in the wave-1 Terminal-Bench rerun, 288 were
`bash` and **197 of those 288 re-issued `cd`**. That did not cost turns: 192
calls began with `cd <dir> && <command>` in the same call, zero calls were a
bare `cd`, and the prefixes total 2,553 characters, roughly 640 tokens across
the run. 120 of the 126 literal-path calls were `cd /app`, already the starting
directory; exactly one call used `workdir`. The opt-in `terminal` tool, which
already persists a directory, was called **0 times**, so the default one-shot
path is the one that has to remember.

**Implemented:**
- On Unix, `bash` reads the session's last recorded directory before the
  command and writes the shell's final physical `pwd` after it. The first call,
  a session with no record, and every Windows call start in the workspace root.
- The record is a `bash_cwd` auxiliary record in the session log (log version
  4), so `--resume` starts the next bash call where the previous process
  stopped. A missing, non-directory or unenterable record falls back to the
  workspace root and adds one `note:` line; the call is not an error.
- The Unix trailer runs the caller's command in a login shell with an `EXIT`
  trap that saves `$?`, appends `pwd -P` to a private temporary file and
  re-exits with the saved status. stdout and stderr are untouched, and the
  exit code is exact. A command that `exec`s or kills its shell skips the trap;
  a timeout or cancellation leaves the previous record.
- The record is per session when the CLI attaches the open `SessionLog` to the
  harness, and per tool instance for a default registry or an embedder with no
  store. `BashTool` holds a `tokio::sync::Mutex` across the call, so concurrent
  calls serialize and cannot record a stale value.
- The tool description says the directory persists and that `read`, `write`,
  `edit`, `apply_patch`, `glob` and `search` stay rooted at the workspace. Those
  six tools are unchanged, and so is `jobs`; M13.2 later removed `terminal`.
- Windows keeps today's behavior: every call starts in the workspace root, no
  trailer and no build break. This is a deliberate fallback; the benchmark
  target is Linux.

**Tests:** 12 unit tests in `crates/keel-tools/src/builtin/bash.rs` cover the
first-call root, `cd` visible in the next call, a session store read by a fresh
tool, an explicit `workdir` becoming the record, an exact failing exit code,
stdout with no trailing newline, a removed record with the fallback note, a
command that deletes its own directory, `exec`/`kill -9 $$`, concurrent calls
leaving a coherent record and the Windows workspace-root path. `keel-core`
covers the record round trip, newest-wins, a log with no record and a version-3
log. CLI tests drive two bash calls in one turn and a `--resume` seeded with a
recorded directory. The workspace suite went from 766 to 781 tests: 781
passed, 0 failed, 2 ignored; `cargo fmt --check` and clippy are clean.

**Accept:** functional only. No live sweep was run for this slice; the evidence
needed is a wave-1-style 10-task run measuring the token savings, with pass@1
not regressing. The working-directory record is also a prerequisite for judging
whether `terminal` is worth advertising.

**Docs:** `docs/notes/bash-cwd.md`, README built-in tool table and security
notes, `docs/architecture.md` sessions and built-in tools sections.

**Risk:** low to medium. The trailer is shell syntax on a command the model
supplied; it is hidden from stdout/stderr and preserves the exit code, and the
fallback path is the old behavior.

### M13.2 Remove the opt-in `terminal` tool

**Branch:** `m13-remove-terminal`
**Depends on:** M13.1
**Status:** open against `main` at `5069d4f` as PR
[#53](https://github.com/Wasomeno/keel-harness/pull/53); no benchmark run.

**Goal:** delete the second shell the model never used, and give `jobs` its own
opt-in so it no longer depends on a removed tool.

**Evidence:** in the wave-1 10-task Terminal-Bench rerun, `terminal` was called
**0 times in 339 tool calls**; `jobs` was called 17 times on two tasks
(`kv-store-grpc`, `query-optimize`). The default `bash` now remembers its
working directory between calls and across `--resume` (M13.1), so the only
state `terminal` still uniquely provided was exported variables and an
interactive PTY, which this run never asked for. Long-running work already
belonged to `jobs`.

**Implemented:**
- Deleted `keel-tools/src/builtin/terminal.rs` and
  `keel-tools/tests/terminal.rs`, the `portable-pty` dependency, and the
  `--terminal` / `KEEL_TERMINAL=1` CLI plumbing. The registry, `keel tools`
  listing, help text, README, architecture, Harbor adapter and notes no longer
  advertise the tool.
- `jobs` is unchanged internally. It is registered only by `--jobs` /
  `KEEL_JOBS=1`, remains Unix-only and default-off, and the default registry
  still has exactly the seven structured tools.
- The Harbor adapter now passes `--jobs`; its command test asserts that flag.
- The test loop watches `bash` calls only; the removed tool name is no longer
  accepted as a test surface.
- `jobs` integration tests that needed a foreground call now use `BashTool`;
  process groups, bounded output, deadlines and drop cleanup are unchanged.

**Tests:** a pristine `origin/main` at `5069d4f` reports **789 passed, 0
failed, 2 ignored**; this branch reports **773 passed, 0 failed, 2 ignored**.
The -16 is exactly the 7 unit tests in the deleted module, the 8 integration
tests in `tests/terminal.rs`, and the CLI persistent-shell turn; one test
binary goes away. New coverage:
- the default registry test asserts exactly `apply_patch`, `bash`, `edit`,
  `glob`, `read`, `search`, `write` and no `terminal` or `jobs`;
- `keel tools` lists exactly those seven by default, adds only `jobs` with
  `--jobs`, adds it with `KEEL_JOBS=1`, and refuses `--jobs` on Windows;
- the jobs CLI harness test passes `--jobs`.

`cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D
warnings` are clean; the 306 Python benchmark tests pass.

**Docs:** README, `docs/architecture.md`, `docs/BLUEPRINT.md`, this card,
`docs/notes/bash-cwd.md`, `docs/notes/bash-cwd-measured.md`,
`docs/notes/m12-wave1-results.md`, `docs/notes/windows-ci.md`, and the Harbor
README.

**Risk:** low. Removal cannot break a default chat path that never registered
the tool; the only caller that opted in was the Harbor adapter, and it now
passes `--jobs`. `jobs` keeps its implementation and its 13 integration plus 4
unit tests.

### M13.3 `view_image`

**Branch:** `m13-view-image`
**Depends on:** M13.2
**Status:** open against `main` at `e9dafd0` as PR
[#59](https://github.com/Wasomeno/keel-harness/pull/59); no benchmark run.

**Goal:** let the model see a local raster image, including one it just
rendered, without changing the text-only tool-result shape that OpenAI Chat
requires.

**Evidence:** the transport already existed. `Content::Image` serializes as a
data URI on all three protocols when `compat.supports_images` is set, repair
downgrades it to `[image omitted: <mime>]` when the target cannot see images,
and `tokens.rs` charges a flat 2,000 tokens per image. No tool ever created an
image part, so the capability was unreachable. This slice wires the missing
producer and keeps the existing transport untouched.

**Implemented:**
- New `view_image` tool (`keel-tools/src/builtin/view_image.rs`): accepts PNG,
  JPEG, WEBP and GIF by magic bytes, not extension; refuses SVG with a
  rasterise hint; refuses anything else while naming the detected type; caps
  the file at 2 MiB and says the cap; reads through `Workspace::open_existing`
  so symlinks and escapes behave exactly like `read`; never fetches a URL;
  reports `viewed <path> (<mime>, <size>, <WxH>)` with dimensions read from the
  header and no decode or resize. Optional `detail` (`low`/`high`, default
  `high`).
- `ToolOutcome` gained `images: Vec<ImageAttachment>` with `Default`, so every
  existing tool compiles and behaves unchanged. `ImageAttachment` carries the
  MIME type, base64 payload, caption and detail hint.
- The run loop collects images per finished call, in call order, and appends
  one follow-up user message per image-producing call immediately after the
  tool-results message. That shape works on OpenAI Chat, OpenAI Responses and
  Anthropic Messages; an image inside an Anthropic `tool_result` would not
  work for OpenAI Chat.
- `Content::Image` gained an optional `detail`; OpenAI Chat sends it inside
  `image_url`, OpenAI Responses beside `image_url`, and Anthropic ignores it.
  A text-only target keeps the caption and gets the existing repair placeholder
  and `RepairNote::DowngradedImage`.
- `view_image` is in the default registry, moving the default toolset from
  seven tools to eight. All twelve eval cassettes were re-recorded live, not
  hand-edited.

**Tests:** clean `origin/main` (`e9dafd0`) reports **789 passed, 3 failed,
2 ignored** with `HOME` isolated. This branch reports **812 passed, 3 failed,
2 ignored**: the difference is exactly the 23 new tests. The same three macOS
Seatbelt sandbox tests fail on the clean baseline in this environment, so they
are pre-existing and unrelated to this slice. The 23 new tests cover each
accepted fixture type and its dimensions, the detail hint, SVG/text/PDF
rejection, the 2 MiB cap, a symlink, an escaping path, a missing file and a
directory; the follow-up user message, the text-only placeholder and two
images from one batch in call order; one serialization test per protocol; and a
real CLI `chat` turn whose scripted provider sees the follow-up message. The
twelve-task eval suite passes 12/12 after the re-record.

**Accept:** functional only. No benchmark run was made for this slice. The
open question for scoring is whether seeing rendered screenshots changes
Terminal-Bench or SWE-bench outcomes; that needs a wave-1-style control run.

**Docs:** README built-in tool table and transport paragraph,
`docs/architecture.md` tools section, `docs/notes/windows-ci.md`, this card,
and the shared note `engineering/m13-view-image.md`.

**Risk:** medium. Two costs are explicit: each image is charged a flat 2,000
tokens regardless of size, and the base64 payload stays in session history and
the JSONL log, so the log grows by roughly 4/3 of each viewed file. The 2 MiB
cap is the only size policy, and it bounds one view, not a session. The shape a
reviewer should check most closely is the follow-up user message: images are
never placed inside a tool result, so all three protocols share one path.

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
3. **Terminal backend.** M12.4 chose one PTY-backed interactive bash per
   session via `portable-pty`, exposed as the opt-in Unix `terminal` tool;
   M13.2 removed it after 0 calls in 339. The default `bash` now remembers its
   directory and `jobs` covers long-running work. Windows ConPTY parity remains
   out of scope.
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
