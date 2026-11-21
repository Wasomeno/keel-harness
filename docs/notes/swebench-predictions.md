# SWE-bench predictions from keel

This note records the predictions format produced by
`benchmarks/swebench/run_keel.py`, how to sanity-check it with a gold patch,
how to evaluate it with the official harness, and what a bring-up run may and
may not claim. The runner itself is documented in
`benchmarks/swebench/README.md`.

## Predictions format

The runner writes one JSON object per line:

```json
{"instance_id": "astropy__astropy-12907", "model_name_or_path": "keel-neuralwatt-deepseek-v4.1-flash", "model_patch": "diff --git a/astropy/modeling/separable.py b/astropy/modeling/separable.py\n..."}
```

- `instance_id` is copied from the dataset row.
- `model_name_or_path` is the stable label for the arm (`--model-label`). Keep
  it identical across trials so the report groups the runs.
- `model_patch` is a unified git diff against the row's `base_commit`, or `""`
  for an empty patch. It is never synthesized by the harness.

The file must be JSONL, not a JSON array, and the extension should be
`.jsonl`. The official harness looks instances up by `instance_id`; ids missing
from the dataset are ignored or reported as errors by the harness.

## Patch rules

Patch extraction runs inside `/testbed`:

```text
git -C /testbed config core.fileMode false
GIT_INDEX_FILE=/tmp/keel-extract-index git -C /testbed read-tree <base_commit>
GIT_INDEX_FILE=/tmp/keel-extract-index git -c core.fileMode=false -C /testbed \
  add -A -- . <excluded pathspecs>
GIT_INDEX_FILE=/tmp/keel-extract-index git -c core.fileMode=false -C /testbed \
  diff --cached --no-color --no-ext-diff --no-textconv \
  --src-prefix=a/ --dst-prefix=b/ <base_commit> -- . <excluded pathspecs>
rm -f /tmp/keel-extract-index
```

The temporary index is seeded from the base commit because official images can
ship an index that records files at 755 while the base tree records 644.
`core.fileMode=false` does not hide index-vs-tree mode changes; the base-seeded
index does, while preserving base modes, content changes, new files and
deletions.

`git add -A` into the temporary index makes new files visible; the cached diff
is taken against the base commit, so new, modified and deleted files are
included.

Caches, virtualenvs, build output, `node_modules` and editor files are excluded.
Before a prediction is written:

- A patch with a binary diff, NUL bytes or invalid UTF-8 is rejected with an
  error; the runner does not submit it. `git diff` is captured as bytes and
  decoded strictly, so a replacement-decoded patch is never submitted.
- A patch over `--patch-cap-bytes` (default 1 MiB) is rejected with an error.
- A non-empty patch is checked with `git apply --check -` in a pristine
  worktree at the base commit. The check is advisory: `patch_apply_ok` records
  the result, but a failure does not set `error`, add `patch-failure` or fail
  the run. The official harness decides whether the patch applies.
- An empty patch is still written as `""` and appears in the official
  `empty_patch_ids`, which maps to `verifier_result: fail`.

The runner does not edit, reset or delete test files. If the agent modified
tests, `traces.jsonl` records `tests_modified: true` and the changed paths; the
official harness applies the test patch to a fresh checkout and grades the
pristine tests, so a test-only patch earns no credit.

## Trace sidecar

`traces.jsonl` uses canonical schema version `"1.0"` and the 26 required
fields: `schema_version`, `benchmark`, `task_id`, `trial`, `model`,
`model_version`, `keel_target`, `agent_version`, `binary_sha256`,
`start_time`, `end_time`, `turns`, `tool_calls`, `input_tokens`,
`output_tokens`, `reasoning_tokens`, `cost_usd`, `stop_reason`,
`wall_clock_s`, `patch`, `patch_size`, `patch_apply_ok`, `verifier_result`,
`failure_tags`, `session_log_path`, `error`.

`patch` is the patch **text**, not a path, and `patch_size` is exactly
`len(patch.encode("utf-8"))`. An empty patch is `patch: ""`,
`patch_size: 0`, `patch_apply_ok: true`. A rejected binary, oversized or
non-UTF-8 patch is `patch: ""`, `patch_size: 0`, `patch_apply_ok: false`, with
`error` and a canonical `failure_tags` entry explaining why. A valid patch that
fails the advisory `git apply --check` keeps its text and sets
`patch_apply_ok: false`, but does not set `error`.

The runner adds six non-strict extensions: `patch_path`, `empty_patch`,
`tests_modified`, `modified_test_files`, `image` and `agent_exit_code`. A
nonzero keel exit sets `error`, `stop_reason: error` and a failure tag derived
from the session's last turn error: `provider-error` for provider/transport
evidence, otherwise `environment-error`. Earlier error text is kept alongside a
later one, a session stop never overwrites the failure reason, and a timeout
keeps `stop_reason: timeout`. `verifier_result` is mapped from
the official report,
with `empty_patch_ids` mapping to `fail`. `cost_usd` is null because keel does
not report cost; `stop_reason` is the last assistant stop or
`timeout`/`error`/`unknown`. If the run fails before patch extraction, `patch`
is `""` with `patch_apply_ok: null` and `error` explains the infrastructure
failure; that is distinct from a successful empty patch (`patch_apply_ok:
true`) and a rejected patch (`patch_apply_ok: false`).

If the harness exits 0 without a report, all rows become `error` and the run
exits nonzero. Duplicate instance IDs are rejected at load time. `run.json`
records the effective config, argv, dataset path and SHA-256 so a row can be
reproduced.

## Gold-patch check

Use a known-good patch first to validate the export, image and harness:

```sh
python3 benchmarks/swebench/run_keel.py \
  --dataset benchmarks/swebench/data/swebench.jsonl \
  --instances astropy__astropy-12907 \
  --gold-patch \
  --model-label gold \
  --run-id gold-check-2026-11-21
```

This writes the dataset's `patch` field as `model_patch` and runs the official
harness. Expect that instance in `resolved_ids`; if it is unresolved, fix the
dataset/image/harness setup before blaming keel. `--skip-harness` only writes
the predictions and is useful for checking the JSONL shape without installing
`swebench`.

## Evaluation command

The runner invokes this once, from the run directory, after all instances:

```sh
python -m swebench.harness.run_evaluation \
  --dataset_name SWE-bench/SWE-bench_Lite \
  --split test \
  --predictions_path <run-dir>/predictions.jsonl \
  --instance_ids <id1> <id2> \
  --max_workers 1 \
  --run_id <run-id> \
  --timeout 1800
```

The default dataset is `SWE-bench/SWE-bench_Lite`. `swebench==5.0.2` requires
its `image`, `eval_script`, `log_parser` and `eval_type` fields; the older
`princeton-nlp/SWE-bench_Lite` mirror lacks them. The agent's `--dataset` JSONL
only needs `instance_id`, `base_commit` and `problem_statement`; that format is
unchanged. The runner passes `--instance_ids` with the submitted predictions so
the harness filters consistently.

`--harness-dataset` and `--split` must match the dataset the instance ids came
from; the predictions file itself carries no dataset name. The harness writes a
report JSON with `resolved_ids`, `unresolved_ids`, `error_ids` and
`empty_patch_ids`, plus logs under `logs/run_evaluation/`. The runner maps
`resolved_ids` to `pass`, `unresolved_ids` and `empty_patch_ids` to `fail`, and
`error_ids` to `error`. If the harness exits 0 but no report is found, every row
is marked `error` and the run exits nonzero.

To evaluate manually, use an absolute predictions path:

```sh
python -m swebench.harness.run_evaluation \
  --dataset_name SWE-bench/SWE-bench_Lite \
  --split test \
  --predictions_path "$PWD/benchmarks/swebench/runs/<run-id>/predictions.jsonl" \
  --instance_ids <id1> <id2> \
  --max_workers 1 \
  --run_id "<run-id>-manual"
```

## Bring-up vs frontier

`SWE-bench_Lite` over one trial is a bring-up signal: it exercises images,
provider latency, patch extraction and the harness. It is not a frontier
result.

Before comparing against published numbers:

- use `SWE-bench_Verified` and pass the matching `--harness-dataset`;
- run at least three trials per task with contiguous `--trial 0`, `1`, `2` and
  report pass@1 with the trial count; the reporter excludes non-contiguous
  sequences from pass@k;
- record the model slug and version, keel target, binary SHA-256 and agent
  version (all in the trace);
- keep the official harness as the grader and archive its report and logs;
- report `resolved / submitted` over the fixed subset, not a selected subset.

An empty or test-only patch is unresolved. Any number that comes from the
runner's own apply check rather than the official harness is not a benchmark
result.

## Live status (2026-11-21)

The runner, unit tests (100), dry-run and a Docker plumbing smoke on the cached
`alpine:latest` image are verified. The smoke covered `docker exec -e NAME`
forwarding, `docker cp` + `chmod`, `-w /testbed` and stdin piping for

A live bring-up with `swebench==5.0.2` and the official astropy image ran one
instance. The agent reached max turns after real edits, then the harness
rejected the old default dataset because it lacks `image`, `eval_script`,
`log_parser` and `eval_type`. A second run showed the patch still contained
1,861 mode-only pairs because the image's own index records 755 while the base
tree records 644. This branch extracts from a base-seeded temporary index and
uses `SWE-bench/SWE-bench_Lite`. The rerun patch was 4,712 bytes with 0
mode-only pairs (down from 226,909 bytes with 1,861), containing only the
`separable.py` fix plus test additions, and the official harness graded it as
1 resolved, 0 unresolved, 0 empty and 0 errors. PR #32 is merged as `73f79ce`.
That is one instance, not a Lite-wide score;
`docs/notes/m12-wave0-results.md` has the caveats.
