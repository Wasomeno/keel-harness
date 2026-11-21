# M12 wave 0 results: Harbor, SWE-bench and traces

**Date:** 2026-11-21
**Status:** landed on `main` at `7c50d58`; wave 1 not started
**Scope:** M12.1 Harbor adapter, M12.2 SWE-bench runner, M12.3 traces/subsets
**Plan:** [`benchmark-readiness.md`](benchmark-readiness.md)

This note records what wave 0 actually delivered and the first live scores. It
is not a full benchmark result: the runs cover three valid Terminal-Bench tasks
and one SWE-bench instance, and the iteration subset was still running when
this note was written.

## What landed

| slice | PR | head | merge commit | deliverable |
| --- | --- | --- | --- | --- |
| M12.1 Harbor adapter | [#30](https://github.com/Wasomeno/keel-harness/pull/30) | `13d32de` | `7c50d58` | `benchmarks/harbor/keel_agent.py`, adapter tests, trace and trial artifacts |
| M12.2 SWE-bench runner | [#28](https://github.com/Wasomeno/keel-harness/pull/28) | `4663e65` | `1c1d2d0` | `benchmarks/swebench/run_keel.py`, predictions, patch extraction, harness command |
| M12.3 traces and subsets | [#29](https://github.com/Wasomeno/keel-harness/pull/29) | `e9593cb` | `ec4c871` | schema v1.0, reporter, tag helper, fixed subsets, results template, `benchmarks` CI job |

The merged tree is `7c50d58`. The wave did not change Rust harness behavior.

### M12.1 Harbor adapter

`benchmarks/harbor/keel_agent.py` implements Harbor's
`BaseInstalledAgent`. It uploads a Linux keel binary, installs or locates
`rg`, writes a secret-free runtime environment, validates the keel target,
runs one non-interactive `keel chat` turn in the task workspace, and leaves
the verifier to Harbor. It writes `keel-trace.jsonl` with exactly the 26
canonical fields, plus `keel-trial.json` for Harbor metadata and the captured
session/chat logs.

### M12.2 SWE-bench runner

`benchmarks/swebench/run_keel.py` loads instances, starts the official
SWE-bench image, runs keel in `/testbed`, extracts the patch with
`git add -N` and `git diff`, rejects binary/non-UTF-8/oversized patches, writes
official predictions JSONL and `run.json`, writes a canonical trace sidecar,
and invokes `swebench.harness.run_evaluation`. The apply check is advisory;
the official harness remains the grading authority.

### M12.3 traces and subsets

- Schema v1.0: JSONL, 26 required fields, exact UTF-8 `patch_size`, UTC
  timestamps, `pass|fail|error|not_run`, and the 16 failure tags.
- Reporter: pass@1 (per-task mean), trial pass rate, pass@k with contiguous
  `0..n-1` trials, task-level bootstrap interval, cost, wall clock, turns,
  tokens, patch metrics and harness health.
- `tag_failure.py`: list/validate/add/replace/remove with a required note and
  an atomic `failure_tag_notes` audit trail.
- Fixed subsets: the pinned 89-task Terminal-Bench 2.0 manifest, a 10-task
  iteration set, a 20-task deterministic canary, and a 10-instance SWE-bench
  Lite bring-up set. SWE-bench Pro V2 remains a placeholder.
- Results template at `docs/notes/benchmark-results-template.md`.
- A separate CI `benchmarks` job on Ubuntu with Python 3.12.

## Verification

```text
cargo test --workspace
624 passed, 0 failed, 2 ignored

python3 -m unittest discover -s benchmarks/tests -v
Ran 235 tests in 2.713s
OK

python3 -m compileall benchmarks
OK
```

`cargo fmt --all --check` and `cargo clippy --workspace --all-targets
-- -D warnings` pass. GitHub CI is green on Ubuntu, macOS and Windows plus the
`benchmarks` job. The Rust count was measured on macOS at `7c50d58`; Windows
runs the documented 26 fewer tests.

## Live Harbor smoke

The one live check ran at `13d32de` with Harbor 0.27.0, the
`hello-world/hello-world` task, the default `neuralwatt/deepseek-v4.1-flash`
target, and the x86_64 musl keel binary under Rosetta in an arm64 image.

| field | value |
| --- | --- |
| agent phase | exit 0, `stop_reason=completed` |
| failure tags | none |
| turns | 3 |
| tool calls | 2 |
| input tokens (cache-excluded trace) | 122 |
| output tokens | 172 |
| wall clock | 3.717 s |
| trace | 26 fields, strict-valid, `verifier_result="not_run"` |

The chat summary reported 5,626 input tokens with 5,504 cache reads; the
adapter subtracts cache reads, so the trace's 122 is the comparable input
count. Harbor's verifier then timed out after 120 seconds in `tests/test.sh`
while running its `apt-get`/`uv` setup (`VerifierTimeoutError`). That timeout
happened after the adapter finished. It is not a pass or fail, and the trace
correctly stays at `not_run`.

## Live benchmark results (wave 0)

These are the first live scores. They cover part of the 10-task Terminal-Bench
iteration subset and one SWE-bench Lite instance, not a full benchmark. The
Terminal-Bench runs used Harbor 0.27.0 with the x86_64 musl `keel` binary
(under Rosetta on arm64), the `neuralwatt/deepseek-v4.1-flash` target and
`max_turns=60`. The SWE-bench row came from the M12.2 runner.

| harness | task | result | notes |
| --- | --- | --- | --- |
| Terminal-Bench 2.0 | `fix-git` | 1.0 | 26 turns, 73.6 s wall clock |
| Terminal-Bench 2.0 | `regex-log` | 1.0 | |
| Terminal-Bench 2.0 | `nginx-request-logging` | 1.0 | |
| Terminal-Bench 2.0 | `extract-elf` | excluded | scored 0.0 but invalid: the agent read the task's own `test_outputs.py` and reference solution over the network |
| Terminal-Bench 2.0 | `custom-memory-heap-crash` | not scored | the prebuilt image pull stalled for more than six minutes |
| SWE-bench Lite | `astropy__astropy-12907` | 1 resolved, 0 unresolved, 0 empty (1/1) | provisional; the PR #32 rerun patch is clean: 4,712 bytes, 0 mode-only pairs |

The rest of the iteration subset was still running when this note was written,
so the three Terminal-Bench rows are not a final pass@1 for the subset.

### Caveats

- **Small subset.** Three scored Terminal-Bench tasks and one SWE-bench
  instance. The 89-task manifest, the 20-task canary and the 10-instance
  bring-up set have not run, so this is not a full score.
- **No contamination controls.** Containers had network access and nothing
  stopped the agent from reading task fixtures or a reference solution.
  `extract-elf` did exactly that; the other tasks were not audited.
- **`extract-elf` is invalid.** Its 0.0 is excluded. The agent fetched the
  task's own `test_outputs.py` and reference solution over the network, and
  the verifier then hung fetching `astral.sh/uv`.
- **`custom-memory-heap-crash` was skipped.** Its prebuilt image pull stalled
  for more than six minutes, so it has no score and is not counted as a loss.
- **SWE-bench is n=1 and the harness verdict is pending.** One instance only.
  PR [#32](https://github.com/Wasomeno/keel-harness/pull/32) fixed patch
  extraction with a temporary index. The rerun patch is 4,712 bytes with 0
  `old mode`/`new mode` pairs, down from 226,909 bytes with 1,861 pairs, and it
  contains only the `separable.py` fix plus test additions. The previous 1/1
  was graded on the noisy patch, so it stays provisional until the official
  harness grades the clean one.
- **Verifiers need the network.** Harbor's verifier setup fetches `apt`
  packages and `astral.sh/uv`, so a verifier timeout is an environment
  failure, not an agent failure.

## Residual limitations

- No native aarch64 Harbor binary is built. The smoke used x86_64 under
  Rosetta; container architecture and libc behavior on other images is
  untested.
- Harbor's `results.json` is not merged into the trace yet. The adapter trace
  is agent-phase only, so `verifier_result` is `not_run`; a raw `not_run` scores
  as a loss in the reporter until it is merged or tagged.
- Harbor's `BaseInstalledAgent` does not expose the trial index. The trace
  defaults `trial=0`; multi-trial runs must pass `--ak trial=N` or rewrite the
  field from Harbor's trial order.
- SWE-bench has one live instance, not a set. The official images are
  multi-gigabyte and the host still has no Linux artifact checked in. The
  earlier `astropy__astropy-12907` patch carried 1,861 mode-only pairs
  (226,909 bytes); PR #32's temporary index fixed extraction, and the rerun
  patch is 4,712 bytes with 0 mode-only pairs. The 1/1 stays provisional until
  the official harness grades the clean patch.
- Keel does not report a price, so Harbor and SWE-bench traces leave
  `cost_usd` null. Cost comparisons need a price source.
- Non-UTF-8 source patches are rejected rather than submitted, so those
  SWE-bench instances would become patch failures.
- There is no full Terminal-Bench or SWE-bench score yet. The live rows above
  are a partial iteration subset plus one SWE-bench instance; the 89-task
  manifest, the 20-task canary and the 10-instance bring-up set have not run.

## Next wave

Wave 1 is M12.4 (persistent terminal session) and M12.6 (patch finalization
and empty-patch protection) in parallel, then M12.5 (background jobs) and
M12.7 (test discovery and targeted loop). M12.8-M12.10 wait until baseline
traces show which harness changes matter, and M12.11 is the full scorecard.
M11 (Google/Antigravity) remains on hold pending O-012 and Google credentials;
it does not block M12.
