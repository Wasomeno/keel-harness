# M12 wave 1 results: terminal, jobs, patch finalization and test loop

**Date:** 2026-11-21
**Status:** wave 1 complete at `main` `5e461aa`: M12.4 merged as `911427a` (PR #35), M12.5 as `7d67a70` (PR #37), M12.6 as `ef4ca13` (PR #34) and M12.7 as `5e461aa` (PR #38)
**Scope:** M12.4 persistent terminal, M12.5 background jobs, M12.6 patch finalization, M12.7 test discovery, test loop and pre-final verify
**Plan:** [`benchmark-readiness.md`](benchmark-readiness.md)

This note records what wave 1 delivered. It contains no new live benchmark
results: none of the four slices has been measured end to end on a live
benchmark subset. The last live numbers are from wave 0 and predate all of them.

## What landed

| slice | PR | merge commit | deliverable |
| --- | --- | --- | --- |
| M12.4 persistent terminal | [#35](https://github.com/Wasomeno/keel-harness/pull/35) | `911427a` | Opt-in Unix `terminal` tool, one PTY-backed interactive bash, state preserved across calls |
| M12.5 background jobs | [#37](https://github.com/Wasomeno/keel-harness/pull/37) | `7d67a70` | Opt-in Unix `jobs` tool with `start`, `poll`, `wait`, `kill`, `list`, per-job process groups and bounded output |
| M12.6 patch finalization | [#34](https://github.com/Wasomeno/keel-harness/pull/34) | `ef4ca13` | `keel chat --require-changes`, one-index SWE-bench patch finalization, generated-path exclusion, test-edit detection, whitespace gate and one retry |
| M12.7 test loop | [#38](https://github.com/Wasomeno/keel-harness/pull/38) | `5e461aa` | SWE-bench test-command discovery as a hint, opt-in `keel chat --test-loop`, failure parsing, one pre-final verification message and test-run metrics |

The wave-1 merged tree is `5e461aa`. The documentation closeout is
`docs/notes/m12-wave1-results.md` (this note); the earlier wave-0 closeout is
`docs/notes/m12-wave0-results.md`.

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
job also runs `python3 -m compileall benchmarks`.

## Live measurement status

**Wave 1 has no end-to-end live benchmark measurement.** The last live numbers
are from wave 0 and predate every wave-1 slice:

- Terminal-Bench 2.0 iteration subset: 6 passes of 10 attempted, 6 of 8 valid
  after excluding `extract-elf` and `modernize-scientific-stack` as invalid
  (raw 6/10, not a clean pass@1).
- SWE-bench Lite: one instance, `astropy__astropy-12907`, 1 resolved,
  0 unresolved, 0 empty and 0 errors.

Neither result says anything about the wave-1 changes. No Terminal-Bench or
SWE-bench subset has been re-run on the merged wave-1 harness.

Specific gaps:

- **M12.7 acceptance is not measured live.** No SWE-bench subset run has
  compared the test-run rate or the `premature-finish` rate before and after
  the test loop. The unit and synthetic coverage is complete, but the
  acceptance result is not measured, not a claim that either rate improved.
- **M12.4, M12.5, M12.6 and M12.7 have no end-to-end live benchmark
  measurement at all.** There is no before/after for terminal state, background
  jobs, patch finalization or the test loop.
- **M12.5's task deadline is wired through env/config only.**
  `KEEL_TASK_DEADLINE_MS` (CLI) and `Harness::with_deadline` (embedder) set
  `ToolLimits.deadline`. The Harbor adapter does not set it, and Harbor's
  `AgentContext` exposes no task timeout, so tying background jobs to a task's
  remaining budget is still manual.

## Caveats

- **Unix only.** The persistent terminal and background jobs are Unix-only.
  Windows keeps the one-shot `bash` path and has no job objects or ConPTY
  parity; the CLI refuses `--terminal` there.
- **A `setsid` service can escape job cleanup.** Ordinary `&` children share
  the job's process group and are killed at the deadline or session drop; a
  process that starts a new session is deliberately left running.
- **Test discovery is a heuristic.** It reads a bounded set of files and can
  find no command. That leaves the hint empty and does not fail the trial; the
  model still has to choose and run tests.
- **The pre-final check is one message.** It asks once per run and cannot loop.
  It is an opt-in benchmark mode, not a guarantee that tests ran.
- **Trace grading is still agent-phase only.** Harbor's `results.json` is not
  merged into the trace, `verifier_result` stays `not_run`, and `cost_usd` is
  null because keel has no price source.

## Residual limitations

- No live benchmark evidence for wave 1. The only live scores remain the wave-0
  Terminal-Bench iteration subset (6/8 valid) and one SWE-bench Lite instance
  (n=1); the 89-task manifest, 20-task canary and full SWE-bench sets have not
  run.
- M12.6's retry path can spend a second agent attempt; its effect on patch-apply
  rate and cost has not been measured.
- M12.7's discovery and failure parsers cover pytest, cargo and unittest plus a
  list of known runners. Other repositories and test output shapes are
  unmeasured live.
- The Windows suite is smaller and has no terminal or job coverage; the CI log
  count is the evidence for that platform.
- The task deadline is not connected to Harbor's task budget. An adapter or
  harness caller must set it today.

## Next wave

Wave 2 is M12.8 compaction working memory, M12.9 navigation and edit, and
M12.10 budget discipline. After that, M12.11 is the full scorecard: 3 trials,
pass@1 and pass^k with intervals and cost, compared against Terminus 2 and
SWE-agent or mini-swe-agent.

Wave 2 is gated on a decision about re-measuring the fixed subset on the merged
wave-1 harness first. Without that baseline, a wave-2 change would have no
before/after and no trace-backed reason to keep it. M11 (Google/Antigravity)
remains on hold pending O-012 and Google credentials; it does not block M12.
