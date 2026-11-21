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
git add -N -- . <excluded pathspecs>
git diff --no-color --no-ext-diff --no-textconv \
  --src-prefix=a/ --dst-prefix=b/ <base_commit> -- . <excluded pathspecs>
```

New files are included through `git add -N`; deleted files appear normally.
Caches, virtualenvs, build output, `node_modules` and editor files are excluded.
Before a prediction is written:

- A patch with a binary diff or NUL bytes is rejected with an error; the runner
  does not submit it.
- A patch over `--patch-cap-bytes` (default 1 MiB) is rejected with an error.
- A non-empty patch is checked with
  `git apply --check --whitespace=error-all -` in a pristine worktree at the
  base commit. `patch_apply_ok` records the result.
- An empty patch is still written as `""` and is counted as unresolved, not as
  a failure of the harness.

The runner does not edit, reset or delete test files. If the agent modified
tests, `traces.jsonl` records `tests_modified: true` and the changed paths; the
official harness applies the test patch to a fresh checkout and grades the
pristine tests, so a test-only patch earns no credit.

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
  --dataset_name princeton-nlp/SWE-bench_Lite \
  --split test \
  --predictions_path <run-dir>/predictions.jsonl \
  --max_workers 1 \
  --run_id <run-id> \
  --timeout 1800
```

`--harness-dataset` and `--split` must match the dataset the instance ids came
from; the predictions file itself carries no dataset name. The harness writes a
report JSON with `resolved_ids`, `unresolved_ids`, `error_ids` and
`empty_patch_ids`, plus logs under `logs/run_evaluation/`. The runner maps the
report back into `verifier_result` in `traces.jsonl`.

To evaluate manually, use an absolute predictions path:

```sh
python -m swebench.harness.run_evaluation \
  --dataset_name princeton-nlp/SWE-bench_Lite \
  --split test \
  --predictions_path "$PWD/benchmarks/swebench/runs/<run-id>/predictions.jsonl" \
  --max_workers 1 \
  --run_id "<run-id>-manual"
```

## Bring-up vs frontier

`SWE-bench_Lite` over one trial is a bring-up signal: it exercises images,
provider latency, patch extraction and the harness. It is not a frontier
result.

Before comparing against published numbers:

- use `SWE-bench_Verified` and pass the matching `--harness-dataset`;
- run at least three trials per task and report pass@1 with the trial count;
- record the model slug and version, keel target, binary SHA-256 and agent
  version (all in the trace);
- keep the official harness as the grader and archive its report and logs;
- report `resolved / submitted` over the fixed subset, not a selected subset.

An empty or test-only patch is unresolved. Any number that comes from the
runner's own apply check rather than the official harness is not a benchmark
result.

## Live status (2026-11-21)

The runner, unit tests (55), dry-run and a Docker plumbing smoke on the cached
`alpine:latest` image are verified. The smoke covered `docker exec -e NAME`
forwarding, `docker cp` + `chmod`, `-w /testbed` and stdin piping for
`git apply --check`. No live SWE-bench instance has been run: the official
images are multi-gigabyte, the `swebench` package is not installed on the host,
and this wave did not pull an image. The next live step is the gold-patch check
above, then one small instance through keel, after installing `swebench` and
providing a Linux keel binary and `NEURALWATT_API_KEY`.
