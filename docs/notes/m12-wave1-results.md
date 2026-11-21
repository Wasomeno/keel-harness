# M12 wave 1 results: terminal, jobs, patch finalization and test loop

**Date:** 2026-11-21
**Status:** wave 1 complete at `5e461aa`; a live 10-task Terminal-Bench rerun measured M12.4 and M12.5; main later advanced to `a035307` (docs only); M12.6 and M12.7 remain unmeasured
**Scope:** M12.4 persistent terminal, M12.5 background jobs, M12.6 patch finalization, M12.7 test discovery, test loop and pre-final verify
**Plan:** [`benchmark-readiness.md`](benchmark-readiness.md)
**Run:** Harbor 0.27.0, `neuralwatt/deepseek-v4.1-flash`, `max_turns=60`, `--verifier-timeout-multiplier 3`, one trial per task

This note records what wave 1 delivered and the live rerun on the same 10
Terminal-Bench 2.0 tasks as wave 0. The rerun measured M12.4 and M12.5 only.
M12.6 and M12.7 still have no live benchmark measurement.

The finding to keep in front is that **wave 1 has no measurable effect on
Terminal-Bench**. The one same-set task that flipped is model variance, and the
persistent terminal tool was never called.

## What landed

| slice | PR | merge commit | deliverable | live measurement |
| --- | --- | --- | --- | --- |
| M12.4 persistent terminal | [#35](https://github.com/Wasomeno/keel-harness/pull/35) | `911427a` | Opt-in Unix `terminal` tool, one PTY-backed interactive bash, state preserved across calls | 10-task rerun: 0 calls |
| M12.5 background jobs | [#37](https://github.com/Wasomeno/keel-harness/pull/37) | `7d67a70` | Opt-in Unix `jobs` tool with `start`, `poll`, `wait`, `kill`, `list`, per-job process groups and bounded output | 10-task rerun: 17 calls on 2 tasks |
| M12.6 patch finalization | [#34](https://github.com/Wasomeno/keel-harness/pull/34) | `ef4ca13` | `keel chat --require-changes`, one-index SWE-bench patch finalization, generated-path exclusion, test-edit detection, whitespace gate and one retry | none; needs a SWE-bench subset |
| M12.7 test loop | [#38](https://github.com/Wasomeno/keel-harness/pull/38) | `5e461aa` | SWE-bench test-command discovery as a hint, opt-in `keel chat --test-loop`, failure parsing, one pre-final verification message and test-run metrics | none; the Harbor adapter does not pass `--test-loop` |

The wave-1 merged tree is `5e461aa`. Main later advanced to `a035307` with
the docs closeout only, no Rust or Cargo changes. The earlier wave-0 closeout is
`docs/notes/m12-wave0-results.md`; the detailed rerun report is
`engineering/m12.4-m12.5-live-rerun.md` in the shared space.

### M12.4 persistent terminal

`--terminal` / `KEEL_TERMINAL=1` registers an opt-in Unix `terminal` tool that
keeps one `bash --noprofile --norc -i` on a PTY. `cd`, exported variables and
services persist across calls; `workdir` changes the directory and `reset`
starts a fresh shell. A timeout or cancellation interrupts the current command
and keeps the session when it recovers. Dropping the tool or session sends
SIGTERM/SIGKILL to the shell's process group and reaps it. The default six-tool
registry, the default chat request and the SWE-bench runner are unchanged. The
Harbor adapter passes `--terminal`. Windows keeps the one-shot `bash` path.

### M12.5 background jobs

The same `--terminal` opt-in registers `jobs` beside `terminal`. `start` runs
`bash -lc` in its own process group and returns a job id and effective timeout;
`poll` returns state, absolute offsets and new, head or tail bytes; `wait`
blocks up to a bound; `kill` signals the group; `list` shows every job. Output
keeps a 64 KiB head and the newest 1 MiB, so a 10 MiB log is readable from
either end. Per-command timeouts default to 10 minutes, are capped at 30
minutes, and are further capped by `ToolLimits.deadline`. A monitor thread kills
the group at the deadline even without another poll, and session drop kills and
reaps every remaining group. A command that calls `setsid` leaves the group and
survives until the session ends; that is the documented escape hatch for a
service the task still needs.

### M12.6 patch finalization

`keel chat --require-changes` exits 3 with a stable retry hint when a one-shot
run leaves the workspace clean, and never converts a failed or cancelled turn
into that no-op. The SWE-bench runner's `finalize_patch` takes the patch,
changed-file list and `git diff --check` gate from one base-seeded temporary
index, excluding generated paths. Test-file edits are recorded and warned
about, not dropped. An empty patch or a whitespace failure gets one
`--resume last` retry; a second whitespace failure is not submitted and carries
`patch-failure`. The trace adds `agent_attempts`, `retry_reason`,
`whitespace_ok` and `whitespace_errors`.

### M12.7 test discovery, test loop and pre-final verify

`benchmarks/swebench/test_discovery.py` reads a bounded set of repository files
and `git ls-files` through the container's existing `docker exec` path.
Precedence is a CI workflow `run:` step, a Makefile test target, a README code
span or fence, project configuration, then layout convention. The result is a
hint in `--system`, never a hardcoded answer; a failed read or no match leaves
it empty and does not fail the trial. The hint and source are recorded in the
trace.

`keel chat --test-loop` (opt-in) tracks `bash` and `terminal` commands that
invoke known test runners, parses pytest, cargo and unittest failures into up
to eight names plus a short excerpt, and stores them in a `test_status` record.
Compaction pins the status in the summary request and appends it to the handoff
message. At the first `EndTurn`, the harness injects one user message asking
what changed, what ran and what is still unverified; a second `EndTurn` ends
normally and the default chat path does not inject it. The SWE-bench trace adds
`test_command_hint`, `test_command_source`, `test_runs`, `tests_run`,
`test_last_status`, `failing_tests`, `test_excerpt` and `verify_prompts`; the
reporter adds a visible test-run rate.

## Verification

```text
cargo fmt --all -- --check
clean

cargo clippy --workspace --all-targets -- -D warnings
clean

cargo test --workspace
679 passed, 0 failed, 2 ignored

python3 -m unittest discover -s benchmarks/tests
Ran 278 tests in 4.844s
OK
```

The Rust and Python counts were measured locally on macOS at `5e461aa`. The
Windows CI job log at the same commit sums to 617 passed, 0 failed, 2 ignored.
GitHub CI is green on Ubuntu, macOS and Windows plus the `benchmarks` job; that
job also runs `python3 -m compileall benchmarks`. The docs-only `a035307` merge
does not change these counts.

## Live measurement

The wave-1 rerun used the binary built from a clean `5e461aa` checkout,
sha256 `f7ffb7935e09152bf3fbd7b212ce22f71144a9573c36dde6c017710eab2fd39a`.
It ran the same 10-task Terminal-Bench 2.0 iteration subset, with the same
model and flags as wave 0 (`max_turns=60`,
`--verifier-timeout-multiplier 3`), one trial per task, no repeats and no
re-trials. The sweep took 96 minutes. Raw logs are in
`/tmp/keel-tb2-iteration-wave1`; the preserved wave-0 logs are in
`/tmp/keel-tb2-iteration-wave0`.

Agent minutes below are the keel phase from `keel-trial.json`; they exclude
Harbor environment setup and verifier time. The wave-0
`custom-memory-heap-crash` row is the scored manual rerun after its image pull
stalled.

| task | wave 0 result | wave 0 turns | wave 1 result | wave 1 turns | wave 1 calls | wave 1 agent min |
| --- | --- | ---: | --- | ---: | ---: | ---: |
| `fix-git` | 1.0 | 27 | 1.0 | 15 | 21 | 0.6 |
| `extract-elf` | invalid (0.0) | 38 | invalid (1.0) | 23 | 30 | 3.2 |
| `regex-log` | 1.0 | 22 | 1.0 | 10 | 9 | 1.4 |
| `nginx-request-logging` | 1.0 | 17 | 1.0 | 25 | 29 | 1.8 |
| `custom-memory-heap-crash` | 1.0 | 54 | 1.0 | 52 | 81 | 14.4 |
| `query-optimize` | 0.0 | 28 | 0.0 | 36 | 62 | 15.0 |
| `modernize-scientific-stack` | invalid (0.0) | 15 | 1.0 | 11 | 17 | 0.8 |
| `kv-store-grpc` | 1.0 | 14 | 1.0 | 24 | 26 | 15.0 |
| `fix-code-vulnerability` | 1.0 | 16 | 1.0 | 14 | 19 | 0.7 |
| `cancel-async-tasks` | 0.0 | 60 | 1.0 | 45 | 45 | 9.7 |

### Aggregates

- **Wave 1:** 9 of 10 scored 1.0 and `query-optimize` scored 0.0.
- **Valid rate:** after excluding invalid `extract-elf`, 9 valid tasks remain
  and 8 passed, a valid rate of 8/9 (0.889).
- **Conservative count:** treating the invalid `extract-elf` score as a failure
  gives 8 of 10 (0.800).
- **Wave 0:** 6 of 8 valid tasks passed (0.750), raw 6/10.
- **Same-set comparison:** on the eight tasks valid in both waves, the ratio
  moved from 6/8 to 7/8. The only result change is `cancel-async-tasks`
  (failure to pass). It used only `bash`, `read`, `write` and `edit`, finished
  in 45 turns after hitting `max_turns=60` in wave 0, and is model variance, not
  a wave-1 effect.
- `modernize-scientific-stack` was invalid in wave 0 because the verifier could
  not reach `releases.astral.sh`, and became valid and passed in wave 1. That is
  an environment change, not a harness improvement.
- `extract-elf` is invalid for the second wave: it scored 1.0, but the agent
  fetched the task's own `tests/test_outputs.py` and reference solution from
  `raw.githubusercontent.com/harbor-framework/terminal-bench-1`. Exclude it
  rather than counting it as a pass.

**Do not read the 9/10 as a pass@1 improvement.** The only same-set flip is
variance, and the dedicated wave-1 terminal path went unused.

### Tool use and the wave-1 features

Across all ten runs there were **339 tool calls**:

| tool | calls |
| --- | ---: |
| `bash` | 288 |
| `jobs` | 17 |
| `read` | 14 |
| `write` | 12 |
| `edit` | 7 |
| `glob` | 1 |
| `terminal` | 0 |

- **M12.4 persistent terminal was invoked zero times in 339 calls.** Both
  `terminal` and `jobs` are registered and advertised with `--terminal`; the
  model simply chose one-shot `bash` on this subset. This is model choice, not
  a wiring failure, but it means this sweep provides no evidence for M12.4.
- **M12.5 background jobs was used 17 times on 2 of 10 tasks.**
  `kv-store-grpc` backgrounded a `pip install` after the 120-second foreground
  cap killed it, then passed (7/7 verifier) even though it also hit the 900 s
  agent timeout. `query-optimize` backgrounded its slow query, polled for 444.7
  seconds, hit the same timeout and still failed 2/6. The tool gave observable
  progress, but not a solution.
- **Totals moved in mixed directions:** 291 to 255 turns and 349 to 339 calls,
  while agent wall time rose from 49.3 to 62.6 minutes. The time increase is
  concentrated on `custom-memory-heap-crash` (7.2 to 14.4 minutes, 81 calls) and
  `kv-store-grpc` (2.7 to 15.0 minutes); several fast tasks got faster.

### What this sweep does not measure

- **M12.6 and M12.7 have no live benchmark measurement.** The Harbor adapter
  passes `--terminal` but never `--test-loop`; only the SWE-bench runner sets
  `--test-loop`. The wave-1 Terminal-Bench traces show `patch=""` and
  `patch_size=0` even on tasks that changed files, so patch finalization and
  the targeted test loop were not exercised. Terminal-Bench cannot measure
  M12.7 until the adapter changes; M12.6 needs a SWE-bench subset.
- **M12.7 acceptance remains unmeasured.** No SWE-bench subset run has compared
  the test-run rate or the `premature-finish` rate before and after the test
  loop.
- **The task deadline is still manual.** `KEEL_TASK_DEADLINE_MS` (CLI) and
  `Harness::with_deadline` (embedder) set `ToolLimits.deadline`; the Harbor
  adapter does not set it, and Harbor's `AgentContext` exposes no task timeout.

## Caveats

- **One trial per task.** A single sample catches gross regressions, not a
  stable pass rate. Turn and wall-clock deltas include model nondeterminism.
- **The tools are opt-in and model-dependent.** `terminal` was never used and
  `jobs` ran on only two tasks, so this subset says little about persistent
  terminals and only a little about background jobs.
- **The valid-rate denominator changed.** `modernize-scientific-stack` became
  valid because its verifier could install `uv`; `extract-elf` remained
  invalid. Use the same-set 6/8 to 7/8 for behavior, not the 6/8 to 8/9 headline.
- **The raw 9/10 is contaminated.** `extract-elf` fetched task tests and a
  reference solution. Count it as a failure for the conservative 8/10, or
  exclude it per protocol for 8/9.
- **Wave 0's `custom-memory-heap-crash` pass came from a manual retry** after
  its 4.23 GB image pull stalled.
- **Unix only.** The persistent terminal and background jobs are Unix-only;
  Windows keeps the one-shot `bash` path and has no job objects or ConPTY
  parity. A `setsid` service can escape job cleanup by design.
- **Trace grading is still agent-phase only.** Harbor's `results.json` is not
  merged into the trace, `verifier_result` stays `not_run`, and `cost_usd` is
  null because keel has no price source.

## Residual limitations

- M12.6 and M12.7 remain unmeasured on any live subset. M12.6 needs a SWE-bench
  run; M12.7 cannot be measured on Terminal-Bench until the Harbor adapter
  passes `--test-loop`.
- The terminal path is unexercised and the jobs evidence is two tasks. A larger
  or different subset could show different tool choices.
- The task deadline is not connected to Harbor's task budget. An adapter or
  harness caller must set it today.
- No full Terminal-Bench or SWE-bench score has run. This is one trial of the
  10-task iteration subset; the 89-task manifest, the 20-task canary and the
  full SWE-bench sets remain open.

## Next wave

Wave 2 is M12.8 compaction working memory, M12.9 navigation and edit, and
M12.10 budget discipline. After that, M12.11 is the full scorecard: 3 trials,
pass@1 and pass^k with intervals and cost, compared against Terminus 2 and
SWE-agent or mini-swe-agent.

The wave-1 rerun now supplies the live baseline on the merged harness that
wave 2 was gated on. Because that baseline is a single trial, M12.11's
multi-trial scorecard is still the evidence that can support a rate claim. M11
(Google/Antigravity) remains on hold pending O-012 and Google credentials; it
does not block M12.
