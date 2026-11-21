# Benchmark results template

Use one copy per batch, run or A/B comparison. Fill every `TBD` before
publishing; an empty field is a missing measurement, not a zero. Keep traces and
the exact commands with the result.

## Status

- Status: `TBD` (draft / complete / blocked)
- Date: `TBD`
- Owner: `TBD`
- Report path: `docs/notes/benchmark-results-YYYY-MM-DD.md`
- Shared copy: `engineering/benchmark-results-YYYY-MM-DD.md`

## Arm

- Harness: `keel`
- Benchmark and split: `TBD` (for example Terminal-Bench 2.0 or SWE-bench Lite)
- Subset file and checksum: `TBD`
- Model and exact version: `TBD`
- Keel target: `TBD`
- Keel binary SHA-256: `TBD`
- Adapter version or commit: `TBD`
- Reference harness and version: `TBD` (same model and tasks)
- Trials per task and trial indices: `TBD` (must be `0..n-1` for pass@k)

## Commands

Record the commands that produced the traces, including the binary build,
adapter invocation and reporter command.

```bash
# TBD: build or download the exact binary
# TBD: run the benchmark
# TBD: merge the verifier results
python3 benchmarks/traces/tag_failure.py validate runs/*.jsonl
python3 benchmarks/traces/report.py runs/*.jsonl --json > report.json
```

Environment or image pins:

- Container image or digest: `TBD`
- Dataset revision or URL: `TBD`
- Network or provider caveats: `TBD`

## Headline metrics

Copy the values from `report.py --json`; do not re-derive them by hand.

| metric | value | notes |
| --- | --- | --- |
| pass@1 | `TBD` | task-weighted mean; `not_run` scores as a loss |
| trial pass rate | `TBD` | passing trials / all trials |
| pass@k | `TBD` | `TBD` tasks considered, `TBD` insufficient, `TBD` non-contiguous |
| pass@1 95% CI | `TBD` | task-level bootstrap, seed `TBD`, `TBD` resamples |
| unverified trials | `TBD` | `not_run`; merge `results.json` or tag |
| tasks / trials | `TBD` / `TBD` | |
| cost total | `TBD` | missing-cost trials: `TBD` |
| cost per solved trial | `TBD` | |
| cost per solved task | `TBD` | missing-cost solved tasks: `TBD` |
| wall-clock mean / p95 | `TBD` / `TBD` | seconds |
| patch-apply failure rate | `TBD` | `TBD` evaluated, `TBD` not evaluated |
| empty-patch rate | `TBD` | |
| overflows / loops / test-command failures | `TBD` | direct counters or tag fallback |
| tool errors / provider retries / nudges | `TBD` | direct counters only; `n/a` when absent |

## Per-task outcomes

| task | trials | passes | pass@1 | pass@k | failure tags | cost | wall mean |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `TBD` | `TBD` | `TBD` | `TBD` | `TBD` | `TBD` | `TBD` | `TBD` |

Copy the reporter's per-task table. Every non-passing task needs a tag from the
16-tag taxonomy and a one-line evidence note naming the concrete observation.

## Failure tags

| tag | count | explanation |
| --- | --- | --- |
| `localization` | `TBD` | |
| `wrong-file` | `TBD` | |
| `missed-requirement` | `TBD` | |
| `integration-error` | `TBD` | |
| `wrong-logic` | `TBD` | |
| `syntax` | `TBD` | |
| `regression` | `TBD` | |
| `test-mismatch` | `TBD` | |
| `test-broken` | `TBD` | |
| `timeout` | `TBD` | |
| `context-overflow` | `TBD` | |
| `provider-error` | `TBD` | |
| `patch-failure` | `TBD` | |
| `environment-error` | `TBD` | |
| `premature-finish` | `TBD` | |
| `loop` | `TBD` | |

Non-passing trials with no tag: `TBD`. If this is not zero, tag them or explain
why the trace cannot be tagged.

## Trace archive

- Trace files and line counts: `TBD`
- SHA-256 checksums: `TBD`
- Session-log location or archive: `TBD`
- Harness report (`results.json`) path: `TBD`
- Redactions or omitted logs: `TBD`

## Verification

- [ ] `python3 -m unittest discover -s benchmarks/tests -v`
- [ ] `python3 -m compileall benchmarks`
- [ ] Every trace passes `tag_failure.py validate`
- [ ] Every non-passing trial has a tag and evidence note
- [ ] `not_run` is merged or explicitly counted and explained
- [ ] pass@k trial indices are exactly `0..n-1`
- [ ] Cost and wall-clock missing counts are stated
- [ ] Reference harness uses the same model, tasks and protocol
- [ ] No canary task was used for tuning
- [ ] Numbers match the checked-in `report.json`

## Interpretation

- What changed against the comparison arm: `TBD`
- Paired or task-level evidence: `TBD`
- Noise, budget and deadline limits: `TBD`
- Residual risks and open questions: `TBD`
