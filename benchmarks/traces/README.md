# Traces, metrics and failure tags

A benchmark number without a trace is not actionable. This directory defines
the record every trial produces, the reporter that aggregates records, and the
helper that tags a failure with an audit note.

| file | purpose |
| --- | --- |
| [`schema.py`](schema.py) | Canonical schema v1.0, validation, JSONL reader. |
| [`report.py`](report.py) | pass@1, pass@k, bootstrap interval and harness metrics. |
| [`tag_failure.py`](tag_failure.py) | List, validate, add, replace and remove failure tags. |

The unit tests and synthetic fixtures are in
[`../tests/test_traces_*.py`](../tests/) and
[`../tests/fixtures/traces/`](../tests/fixtures/traces/).

## File format

One JSON object per trial, one object per line (JSONL), UTF-8, newline
terminated. Blank lines are ignored by the reader. A file may contain one trial
or a whole run. Use a stable layout such as:

```text
traces/terminal-bench-2.0/2026-11-21-m12-baseline/
  run-000.jsonl
  run-001.jsonl
  report.md
```

A trace file is not a transcript. Keep messages and tool output in the keel
session JSONL referenced by `session_log_path`; the trace is the compact,
comparable record.

## Schema version 1.0

`schema_version` is the string `"1.0"`. The required fields are:

```text
schema_version, benchmark, task_id, trial, model, model_version, keel_target,
agent_version, binary_sha256, start_time, end_time, turns, tool_calls,
input_tokens, output_tokens, reasoning_tokens, cost_usd, stop_reason,
wall_clock_s, patch, patch_size, patch_apply_ok, verifier_result, failure_tags,
session_log_path, error
```

| field | allowed value | meaning |
| --- | --- | --- |
| `schema_version` | `"1.0"` | Version of this schema. |
| `benchmark` | non-empty string | Dataset or suite name, for example `terminal-bench-2.0`, `swebench-lite`, `swebench-pro-v2`. |
| `task_id` | non-empty string | Task or instance ID from the subset file, exactly as written. |
| `trial` | integer >= 0 | Trial index. Use `0..k-1` for k trials; the reporter sorts by this value. |
| `model` | non-empty string | Provider/model slug. Record the exact slug, not a display name. |
| `model_version` | non-empty string or null | Provider-reported version, when the provider exposes one. |
| `keel_target` | non-empty string | Keel catalog target used to route the model. |
| `agent_version` | non-empty string or null | Keel version, release tag or adapter version. |
| `binary_sha256` | 64 lowercase hex or null | SHA-256 of the keel binary that ran. |
| `start_time` | ISO-8601 UTC string | Trial start. `Z` and `+00:00` are accepted; naive and non-zero offsets are rejected. |
| `end_time` | ISO-8601 UTC string | Trial end, greater than or equal to `start_time`. |
| `turns` | integer >= 0 | Assistant turns. |
| `tool_calls` | integer >= 0 | Tool calls attempted. |
| `input_tokens` | integer >= 0 | Prompt tokens. |
| `output_tokens` | integer >= 0 | Completion tokens. |
| `reasoning_tokens` | integer >= 0 | Reasoning tokens where reported; `0` otherwise. |
| `cost_usd` | finite number >= 0 or null | Trial cost. Null means unknown, not free. |
| `stop_reason` | non-empty string | Why the trial ended, for example `completed`, `max_turns`, `timeout`, `error`. |
| `wall_clock_s` | finite number >= 0 | Trial duration in seconds. |
| `patch` | string | Final diff. Empty or whitespace-only means an empty patch. |
| `patch_size` | integer >= 0 | UTF-8 byte length of `patch`. Validation requires an exact match. |
| `patch_apply_ok` | true, false, or null | Whether `git apply --check` passed. Null means not evaluated. |
| `verifier_result` | `pass`, `fail`, `error`, `not_run` | Benchmark verifier outcome. `error` is a harness or verifier problem, not a normal failing test. |
| `failure_tags` | list of unique tags | Zero or more tags from the fixed 16-tag taxonomy. |
| `session_log_path` | non-empty string or null | Path to the keel session JSONL. |
| `error` | non-empty string or null | Adapter or harness error, separate from a verifier `fail`. |

Optional fields:

| field | shape | meaning |
| --- | --- | --- |
| `harness_health` | object of non-negative integers | Counters named `tool_errors`, `context_overflows`, `provider_retries`, `loops`, `nudges`, `test_command_failures`. Fill what the adapter can observe; omit the rest. |
| `failure_tag_notes` | list of objects | Audit entries written by `tag_failure.py`: required `action`, `tag`, `note` and `at`, an optional `previous`, and an optional `by`. |

Unknown top-level fields are ignored by default so adapters can attach
forward-compatible metadata. `schema.validate_trace(record, strict=True)`
rejects them when a typo check is wanted, and also rejects unknown keys inside
`failure_tag_notes` entries.

### Validation rules

`schema.validate_trace` raises `TraceValidationError` with every problem in the
record. `schema.read_trace_file` adds the physical line number and stops at the
first malformed record. In addition to required fields and types:

- Timestamps must be ISO-8601 with a UTC offset and `end_time >= start_time`.
- `verifier_result` must be one of the four allowed values.
- Every failure tag must be one of the 16 exact tags; duplicates are rejected.
- `binary_sha256` must be 64 lowercase hexadecimal characters when present.
- `patch_size` must equal the UTF-8 byte length of `patch`.
- `patch_apply_ok` is a boolean or null; counters are non-negative integers.
- `harness_health` accepts only the six documented counters.
- `failure_tag_notes` entries must have a valid action, tag, non-empty note and
  required UTC `at`. `previous` is absent for `add`, required and different from
  `tag` for `replace`, and required and equal to `tag` for `remove`.
- Every string and object key must be valid UTF-8 text. A lone surrogate from a
  JSON escape such as `"\ud800"` is rejected as text, not allowed to crash the
  validator.

A file with no records is rejected. A JSON array, string or number on a line is
rejected as "expected a JSON object".

## Failure taxonomy

The 16 tags are fixed. `tag_failure.py list` prints the same list and
descriptions.

| tag | meaning |
| --- | --- |
| `localization` | The agent never found the code or artifact that had to change. |
| `wrong-file` | Work landed in the wrong file, package, or directory. |
| `missed-requirement` | The task instructions contained a requirement the patch ignored. |
| `integration-error` | Individually plausible changes do not work together or across components. |
| `wrong-logic` | The patch implements the wrong algorithm, condition, or data flow. |
| `syntax` | The delivered code or configuration does not parse or compile. |
| `regression` | Previously working behavior broke. |
| `test-mismatch` | The tests expect a different API, output, or behavior than the patch provides. |
| `test-broken` | The test command itself could not run or was left broken. |
| `timeout` | The trial hit a wall-clock, turn, or process deadline. |
| `context-overflow` | The context window filled or compaction lost required state. |
| `provider-error` | The model provider failed, throttled, or returned an unusable response. |
| `patch-failure` | The patch was empty, malformed, or did not apply. |
| `environment-error` | The container, network, dependency install, or tool bundle failed. |
| `premature-finish` | The agent stopped before completing or verifying the task. |
| `loop` | The agent repeated actions without making progress. |

A tag explains a failure, it does not replace evidence. When tagging, name the
concrete observation in the note, for example "never read
`src/parser.py`; edited `README.md` instead". Do not tag a verifier result as a
guess.

## Reporter

```bash
python3 benchmarks/traces/report.py runs/*.jsonl
python3 benchmarks/traces/report.py runs/*.jsonl --json > report.json
python3 benchmarks/traces/report.py runs/*.jsonl --text --no-trace-index
python3 benchmarks/traces/report.py runs/*.jsonl --k 3 --bootstrap 5000 --seed 11
```

One report covers one arm: one benchmark, model, model version, keel target,
agent version and binary checksum. Shards from the same run can be passed
together. A duplicate `(benchmark, task_id, trial)` or a second arm is an error
that names both sources; split the files and run separate reports for an A/B
comparison.

### Metric definitions

- **pass@1**: mean over tasks of each task's pass rate. Tasks weigh equally, so
  a task with three trials does not dominate a task with one. Every trial is in
  the denominator; `verifier_result: "not_run"` scores as a loss. The separate
  **trial pass rate** is passing trials divided by all trials and is shown so
  the task-weighted and trial-weighted estimators are never confused.
- **pass@k**: fraction of tasks with at least k trials where all of the first k
  trials pass. A task is eligible only when its trial indices are exactly
  `0..n-1`; tasks with gaps or a sequence starting later are excluded and
  counted as `non_contiguous_tasks`. Tasks with fewer than k trials are excluded
  and reported as `tasks_excluded_insufficient_trials`. If no task is eligible,
  the value is null. This is the "all k trials succeed" definition from the
  measurement protocol, not the unbiased pass@k estimator.
- **Unverified trials**: trials with `verifier_result: "not_run"`. They score as
  losses in pass@1 and pass@k because leaving them out would make a raw Harbor
  run look empty or inflated. Merge its `results.json` before scoring, or tag
  the trial to explain why verification did not run.
- **95% interval**: percentile bootstrap over tasks, not trials. Each resample
  draws whole tasks with replacement, keeping all of a task's trials together,
  then recomputes the mean of per-task success rates. The default is 2,000
  resamples and seed 0. The same seed and inputs give the same interval;
  `--seed` is recorded in the report. A single task, all-pass set or all-fail
  set collapses to a zero-width interval.
- **Cost**: sum of present costs for the total; per-solved-trial is total divided
  by passing trials. Per-solved-task is the sum of all trial costs for tasks
  with at least one pass, divided by the number of solved tasks. It is null
  when any solved task has a missing cost, and the missing count is reported.
  Missing costs are counted, not treated as zero.
- **Wall-clock**: mean and p95 over trials, not over tasks.
- **Usage**: turns, tool calls, input tokens, output tokens and reasoning tokens
  are reported as totals and per-trial means. Total tokens is the sum of the
  three token counters.
- **Patch**: mean and p95 of `patch_size` (bytes), empty-patch count and rate,
  patch-apply failures over trials where `patch_apply_ok` is a boolean, and the
  number not evaluated.
- **Verifier**: pass, fail, error and not-run counts.
- **Harness health**: `tool_errors`, `context_overflows`, `provider_retries`,
  `loops`, `nudges` and `test_command_failures`. A direct counter is used when
  any trace records it; otherwise `context_overflows`, `loops` and
  `test_command_failures` fall back to their failure tags. A counter no trace
  recorded is `n/a`, never a fabricated zero. Markdown and text both show the
  fallback provenance.
- **Failure tags**: counts for all 16 tags, plus the number of fail, error and
  not-run trials with no tag. Shares use all trials as the denominator, so a tag
  attached to a passing trial is still visible and cannot push a share above
  100%.

The Markdown report ends with a task table and a non-passing-trial index
containing the source file and line for every trial whose verifier result is not
`pass`. The JSON report contains the same index in `per_task[].records`. That is
the trace index: a reader can go from a failed or unverified task to its tags,
source file, line and session log without a separate database.

## Tagging workflow

```bash
# Inspect and validate before editing
python3 benchmarks/traces/tag_failure.py list
python3 benchmarks/traces/tag_failure.py validate runs/*.jsonl

# Add by task/trial, or by physical line
python3 benchmarks/traces/tag_failure.py add runs/run.jsonl \
  --task-id terminal-bench/fix-git --trial 0 \
  --tag localization --note "never read the repository" --by arndev

# Replace and remove are explicit, and keep the audit trail
python3 benchmarks/traces/tag_failure.py replace runs/run.jsonl \
  --line 4 --old localization --new wrong-file \
  --note "read the parser but edited the CLI wrapper"
python3 benchmarks/traces/tag_failure.py remove runs/run.jsonl \
  --task-id terminal-bench/fix-git --trial 0 --tag wrong-file \
  --note "the file choice was correct after all"

# Re-validate
python3 benchmarks/traces/tag_failure.py validate runs/*.jsonl
```

Every mutation requires a note. The helper:

- parses and validates every record before writing;
- rewrites only the selected line and preserves all other lines byte for byte;
- follows a symlink and atomically replaces its target, leaving the link intact;
- refuses to overwrite a file that changed while it was being read or edited;
- replaces the file atomically and preserves its mode and trailing newline;
- appends an entry to `failure_tag_notes` with the action, tag, previous tag,
  note, UTC time and optional author.

`--line N` is a 1-based physical line number. `--task-id` without `--trial` is
accepted only when exactly one record matches. `validate` exits 1 when any file
is invalid, and `--json` produces machine-readable results.

## jq recipes

```bash
# Non-passing trials with tags and session logs
jq -r 'select(.verifier_result != "pass")
  | [.task_id, .trial, .verifier_result, (.failure_tags | join(",")),
     .stop_reason, .session_log_path] | @tsv' runs/*.jsonl

# Failure-tag histogram
jq -r '.failure_tags[]' runs/*.jsonl | sort | uniq -c | sort -nr

# Cost and trial count per task
jq -s 'group_by(.task_id)
  | map({task: .[0].task_id, trials: length,
         cost_usd: (map(.cost_usd // 0) | add)})' runs/*.jsonl

# Non-passing trials with no failure tag
jq -r 'select(.verifier_result != "pass" and (.failure_tags | length) == 0)
  | [.task_id, .trial, .verifier_result, .session_log_path] | @tsv' runs/*.jsonl

# Extract a final patch
jq -r 'select(.task_id == "terminal-bench/fix-git" and .trial == 0) | .patch' \
  runs/*.jsonl

# Build an index of every trial
jq -r '[.task_id, .trial, .verifier_result, input_filename] | @tsv' runs/*.jsonl
```

## Fixtures and tests

`tests/fixtures/traces/all_tags.jsonl` contains one valid synthetic trace for
each of the 16 tags. `tests/fixtures/traces/valid_run.jsonl` is a two-task,
six-trial run used by the reporter CLI tests. The `malformed/` fixtures cover a
missing required field, a naive timestamp and an unknown tag.

Run the suite from the repository root:

```bash
python3 -m unittest discover -s benchmarks/tests -v
python3 -m compileall benchmarks
```

The suite does not require Harbor, `swebench`, Docker or network access.

## Limits

- Tags are applied by an adapter or a human reviewer; there is no LLM judge in
  this version.
- A trace records the final patch and aggregate usage, not every intermediate
  message. Use `session_log_path` for the transcript.
- `cost_usd` is only comparable when every producer uses the same pricing
  source. Record the source in the run notes.
- pass@k requires enough trials. The report says how many tasks were excluded;
  do not compare pass@3 values from runs with different trial counts without
  reading that field.
