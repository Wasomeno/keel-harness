# Benchmarks

This directory holds the benchmark adapters, fixed subsets and the trace
tooling that make every trial explainable and comparable. Everything here is
stdlib-only Python 3.11+ unless an adapter explicitly says otherwise. Harbor,
`swebench`, Docker and network access are never required by the unit tests.

| path | what it is |
| --- | --- |
| [`harbor/`](https://github.com/Wasomeno/keel-harness/tree/main/benchmarks/harbor) | Terminal-Bench 2.0 adapter that runs keel through Harbor. |
| [`swebench/`](https://github.com/Wasomeno/keel-harness/tree/main/benchmarks/swebench) | SWE-bench prediction runner and patch extraction. |
| [`traces/`](traces/README.md) | Trace schema, reporter, failure taxonomy and tagging helper. |
| [`subsets/`](subsets/README.md) | Fixed iteration, canary and adapter bring-up task lists. |
| `tests/` | Stdlib unit tests and synthetic trace fixtures. |

The Harbor and SWE-bench adapters are added by their own wave slices. Their
links point at the paths those slices occupy on `main`; until the wave merges,
read this trace toolkit on its own.

## Quick start

```bash
# Unit tests (no Docker, Harbor, swebench, or network)
python3 -m unittest discover -s benchmarks/tests -v
python3 -m compileall benchmarks

# Validate traces
python3 benchmarks/traces/tag_failure.py validate runs/*.jsonl

# Report one arm
python3 benchmarks/traces/report.py runs/*.jsonl
python3 benchmarks/traces/report.py runs/*.jsonl --json > report.json
python3 benchmarks/traces/report.py runs/*.jsonl --text --no-trace-index

# Tag a failed trial with an audit note
python3 benchmarks/traces/tag_failure.py add runs/run.jsonl \
  --task-id terminal-bench/fix-git --trial 0 \
  --tag localization --note "never opened the repository"
```

## Trace schema v1

One JSON object per trial, one object per line (JSONL). The schema version is
the string `"1.0"`. The canonical definition and every validation rule live in
[`traces/schema.py`](traces/schema.py); [`traces/README.md`](traces/README.md)
explains each field and the failure tags.

Required fields:

```text
schema_version, benchmark, task_id, trial, model, model_version, keel_target,
agent_version, binary_sha256, start_time, end_time, turns, tool_calls,
input_tokens, output_tokens, reasoning_tokens, cost_usd, stop_reason,
wall_clock_s, patch, patch_size, patch_apply_ok, verifier_result, failure_tags,
session_log_path, error
```

| field | type | meaning |
| --- | --- | --- |
| `schema_version` | string | Must be `"1.0"`. |
| `benchmark` | string | Dataset or suite, for example `terminal-bench-2.0`, `swebench-lite`, `swebench-pro-v2`. |
| `task_id` | string | Dataset task or instance ID, exactly as it appears in the subset file. |
| `trial` | integer >= 0 | Trial index within the task. `0..k-1` is conventional. |
| `model` | string | Provider/model slug used for the trial. |
| `model_version` | string or null | Exact provider version when exposed. |
| `keel_target` | string | Keel catalog target, for example `neuralwatt/deepseek-v4.1-flash`. |
| `agent_version` | string or null | Keel release, commit tag or adapter version. |
| `binary_sha256` | 64 lowercase hex or null | Checksum of the keel binary that ran. |
| `start_time`, `end_time` | ISO-8601 UTC strings | Trial wall-clock bounds. `Z` or `+00:00`; `end_time >= start_time`. |
| `turns` | integer >= 0 | Assistant turns. |
| `tool_calls` | integer >= 0 | Tool calls attempted. |
| `input_tokens`, `output_tokens`, `reasoning_tokens` | integer >= 0 | Provider usage counters. Use `0` when a counter is not exposed. |
| `cost_usd` | finite number >= 0 or null | Provider or computed trial cost. |
| `stop_reason` | non-empty string | Why the trial ended, for example `completed`, `max_turns`, `timeout`, `error`. |
| `wall_clock_s` | finite number >= 0 | Trial duration in seconds. |
| `patch` | string | Final diff. Empty or whitespace-only means an empty patch. |
| `patch_size` | integer >= 0 | UTF-8 byte length of `patch`; validated exactly. |
| `patch_apply_ok` | true, false, or null | Whether `git apply --check` succeeded; null means not evaluated. |
| `verifier_result` | `pass`, `fail`, `error`, `not_run` | Benchmark verifier outcome. |
| `failure_tags` | list of unique tags | Zero or more of the 16 taxonomy tags. |
| `session_log_path` | string or null | Keel session JSONL for the trial. |
| `error` | string or null | Harness or adapter error, separate from a normal verifier failure. |

Optional extension fields:

- `harness_health`: object with non-negative integer counters `tool_errors`,
  `context_overflows`, `provider_retries`, `loops`, `nudges`,
  `test_command_failures`. The reporter falls back to failure tags where a
  counter is unavailable.
- `failure_tag_notes`: audit entries written by `tag_failure.py`. Each entry has
  `action` (`add`, `replace`, `remove`), `tag`, an optional `previous` tag, a
  non-empty `note`, an ISO-8601 UTC `at`, and an optional `by`.

Example record (pretty-printed for readability; traces are one line each):

```json
{
  "schema_version": "1.0",
  "benchmark": "terminal-bench-2.0",
  "task_id": "terminal-bench/fix-git",
  "trial": 0,
  "model": "neuralwatt/deepseek-v4.1-flash",
  "model_version": "deepseek-v4.1-flash-1017",
  "keel_target": "neuralwatt/deepseek-v4.1-flash",
  "agent_version": "0.12.0",
  "binary_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "start_time": "2026-11-21T10:00:00Z",
  "end_time": "2026-11-21T10:00:30Z",
  "turns": 4,
  "tool_calls": 6,
  "input_tokens": 1000,
  "output_tokens": 200,
  "reasoning_tokens": 50,
  "cost_usd": 0.01,
  "stop_reason": "completed",
  "wall_clock_s": 30.0,
  "patch": "diff --git a/solution.py b/solution.py\n--- a/solution.py\n+++ b/solution.py\n@@ -1 +1 @@\n-old\n+new\n",
  "patch_size": 97,
  "patch_apply_ok": true,
  "verifier_result": "pass",
  "failure_tags": [],
  "session_log_path": "/keel-data/sessions/0000000001-aa00.jsonl",
  "error": null
}
```

## jq recipes

```bash
# Every failed trial with its tags and source log
jq -r 'select(.verifier_result != "pass")
  | [.task_id, .trial, .verifier_result,
     (.failure_tags | join(",")), .stop_reason, .session_log_path]
  | @tsv' runs/*.jsonl

# Failure-tag histogram
jq -r '.failure_tags[]' runs/*.jsonl | sort | uniq -c | sort -nr

# Per-task cost and trial count
jq -s 'group_by(.task_id)
  | map({task: .[0].task_id, trials: length,
         cost_usd: (map(.cost_usd // 0) | add)})
  | sort_by(.task)' runs/*.jsonl

# Harness counters, when the producer recorded them
jq -r 'select(.harness_health)
  | [.task_id, .trial, (.harness_health | tojson)] | @tsv' runs/*.jsonl

# Extract the final patch for one task
jq -r 'select(.task_id == "terminal-bench/fix-git" and .trial == 0) | .patch' \
  runs/*.jsonl

# A simple trace index: task, trial, file
jq -r '[.task_id, .trial, .verifier_result, input_filename] | @tsv' runs/*.jsonl

# Run-level counts without the reporter
jq -s '{trials: length,
        tasks: (map(.task_id) | unique | length),
        passes: (map(select(.verifier_result == "pass")) | length)}' runs/*.jsonl
```

## Reporter

`report.py` reads one or more shards of the same arm. An arm is one benchmark,
model, model version, keel target, agent version and binary checksum. Mixing
arms, or repeating a `(benchmark, task_id, trial)`, is rejected with the source
file and line instead of silently averaging incomparable trials.

Reported metrics:

- `pass@1`: mean of per-task success rates, so every task weighs equally even
  when trial counts differ.
- `pass@k` (default k = 3): fraction of tasks with at least k trials where all
  of the first k trials, ordered by `trial`, pass. Tasks with fewer trials are
  excluded and counted.
- 95% interval: percentile bootstrap over tasks, not trials. The default is
  2,000 resamples with seed 0, so the interval is deterministic. `--seed`
  changes it, `--bootstrap` changes the resample count.
- Cost total and cost per solved trial; wall-clock mean and p95; turns, tool
  calls and token totals and means.
- Patch size mean and p95, empty-patch rate, patch-apply failure rate over
  evaluated trials, verifier counts and test-command failures.
- Harness health: tool errors, context overflows, provider retries, loops,
  nudges, and test-command failures. A counter that no trace recorded is shown
  as `n/a`, not as zero.
- Failure-tag counts for all 16 tags, plus untagged failures, stop reasons and
  session-log coverage.

Markdown is the default. `--text` is for terminals and logs. `--json` emits the
same data for a scorecard or further processing. The per-task section and the
failed-trial index give a reader a direct path from a failed task to its trace
file, line, tags and session log.

## Subsets

The iteration subset is fixed for the batch, the canary is never used for
tuning, and the SWE-bench Lite list is only for adapter bring-up. Selection
rules, provenance and the SWE-bench Pro V2 replacement method are in
[`subsets/README.md`](subsets/README.md).

Full Terminal-Bench 2.0 runs happen only at the start and end of the batch.
A/B comparisons use the same subset, three trials, the same model and the same
limits. Report per-task outcomes and a paired bootstrap on the difference; do
not promote a change on aggregate movement alone.

## Ground rules

- No task answers, hidden tests, or benchmark-specific solutions in prompts or
  fixtures. Benchmarks are inputs, not things to tune against.
- Never commit API keys. Adapters read credentials from the environment and
  traces store only model, target and version metadata.
- Traces are the record of truth. If a task fails, tag it and link its session
  log; do not rewrite history by editing a verifier result.
- The canary is a guard against overfitting. Looking at canary failures to
  choose a change defeats its purpose.
