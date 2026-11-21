# SWE-bench predictions with keel

`run_keel.py` runs the Linux `keel` binary against SWE-bench instances inside
the official instance images, extracts the agent's diff, and writes an official
predictions JSONL file. It then invokes the official
`swebench.harness.run_evaluation`; it never replaces the harness and never runs
keel's tools on the host.

Status: implemented and unit-tested. No live instance has been run in this
wave: the official images are multi-gigabyte and the `swebench` package is not
installed on the host. See [Live status](#live-status) for the exact command.

## Layout

```
benchmarks/swebench/
  run_keel.py       # host-side runner
  README.md         # this file
  data/             # exported instances (gitignored; not committed)
  bin/              # Linux keel binary (gitignored)
  runs/             # predictions, traces and logs (gitignored)
```

The runner is Python 3.11+ and uses only the standard library. `swebench` is
needed only to evaluate predictions, not to run keel or to run the unit tests.
Install it on the host with `python3 -m pip install swebench` and record the
version you use; the harness flags and report keys move between releases.

## Requirements

- Docker CLI and a running daemon. The default platform is `linux/amd64`
  because the official SWE-bench images are amd64; pass `--platform ""` to let
  Docker choose, and expect emulation on Apple Silicon.
- A **Linux** keel binary. `run_keel.py` refuses a Mach-O or Windows binary
  with a clear error instead of failing later inside the image.
- The provider key on the host. The runner forwards only environment
  **names** with `docker exec -e NAME`, so key values never appear in a command
  line, a trace or a log:

  ```sh
  export NEURALWATT_API_KEY="$(cat ~/.config/neuralwatt/key)"
  ```

  Base URL overrides are forwarded only when set (`KEEL_NEURALWATT_BASE_URL`,
  `KEEL_OPENAI_BASE_URL`, `KEEL_ANTHROPIC_BASE_URL`, `KEEL_CUSTOM_BASE_URL`).
  Use `--env NAME` to forward another host variable.

## Build a Linux keel binary

Prefer the static musl target:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

Pass the result with `--keel-binary`, or set `KEEL_BINARY`. A glibc build is
acceptable when the image has a compatible glibc. The runner records the binary
SHA-256 and the `keel --version` output in every trace.

## Export an instances file

The default dataset path is `benchmarks/swebench/data/swebench.jsonl`. It is not
committed; export the rows you intend to run. `datasets` is only used for this
export and is never imported by the runner or its unit tests.

```sh
python3 - <<'PY'
import json
from datasets import load_dataset

rows = load_dataset("princeton-nlp/SWE-bench_Lite", split="test")
with open("benchmarks/swebench/data/swebench.jsonl", "w", encoding="utf-8") as out:
    for row in rows:
        out.write(json.dumps({
            "instance_id": row["instance_id"],
            "base_commit": row["base_commit"],
            "problem_statement": row["problem_statement"],
            "patch": row.get("patch"),
        }) + "\n")
PY
```

Required fields per row: `instance_id`, `base_commit`, `problem_statement`.
Optional: `image` (or `docker_image`) overrides the image, and `patch` is the
gold patch used by `--gold-patch`. When the row has no image, the runner uses
`--image-template` (default `swebench/sweb.eval.x86_64.{docker_instance_id}:latest`)
where `docker_instance_id` replaces `__` with `_1776_`, matching the official
image names.

## Run

Dry-run first. It prints every docker command and the harness command without
pulling, starting a container or writing a file:

```sh
python3 benchmarks/swebench/run_keel.py \
  --dataset benchmarks/swebench/data/swebench.jsonl \
  --instances astropy__astropy-12907 \
  --keel-binary benchmarks/swebench/bin/keel \
  --run-id smoke-2026-11-21 \
  --dry-run
```

One instance for real:

```sh
python3 benchmarks/swebench/run_keel.py \
  --dataset benchmarks/swebench/data/swebench.jsonl \
  --instances astropy__astropy-12907 \
  --keel-binary benchmarks/swebench/bin/keel \
  --max-turns 75 \
  --timeout 3600 \
  --run-id smoke-2026-11-21
```

Useful flags:

| flag | meaning |
|---|---|
| `--keel-binary PATH` | Linux keel binary copied into each container |
| `--keel-target` | keel route (default `neuralwatt/deepseek-v4.1-flash`) |
| `--model-label` | `model_name_or_path` in predictions (default `keel-<provider>-<model>`) |
| `--max-turns N` | turn budget (default 75) |
| `--timeout SECONDS` | wall clock allowed for one keel run (default 3600) |
| `--output-dir DIR` | parent of run directories (default `benchmarks/swebench/runs`) |
| `--run-id ID` | run directory name (default UTC timestamp) |
| `--image-template` | image used when a row has no `image` |
| `--patch-cap-bytes N` | reject larger patches (default 1 MiB) |
| `--dry-run` | print commands without executing them |
| `--keep-container` | leave containers for inspection; remove them yourself |
| `--skip-harness` | write predictions but do not invoke `swebench` |
| `--gold-patch` | write gold patches as predictions instead of running keel |
| `--harness-dataset`, `--split` | dataset/split passed to the official harness |
| `--max-workers N` | harness parallelism (default 1) |
| `--instances id1,id2` | run only these ids from the dataset |

## What runs where

For each instance the runner:

1. Inspects the image and pulls it when missing.
2. Starts a container (`docker run -d ... sleep infinity`) and copies keel to
   `/usr/local/bin/keel`.
3. Sets `KEEL_DATA_DIR=/tmp/keel-swebench-data`, `KEEL_CACHE=none` and
   `KEEL_MCP=0` inside the container. MCP is never started (no `--mcp`, no
   `KEEL_MCP=1`) and keel has no web tool, so the agent sees only its built-in
   workspace tools.
4. Runs, in `/testbed`:

   ```text
   keel chat <keel-target> --workspace /testbed \
     --prompt <problem_statement> --yes --max-turns <N>
   ```

5. On timeout, kills and restarts the container so the agent cannot keep
   editing while the patch is extracted.
6. Copies the session JSONL out, then extracts the diff (see below).
7. Writes the prediction and trace, removes the container unless
   `--keep-container`, and finally runs the official harness over the whole
   predictions file.

Only `docker` and the official harness run on the host. Every repository
command (git, chmod, keel) runs through `docker exec` in the instance container.

## Patch extraction and validation

Inside `/testbed`:

```text
git add -N -- . <exclusions>
git diff --no-color --no-ext-diff --no-textconv \
  --src-prefix=a/ --dst-prefix=b/ <base_commit> -- . <exclusions>
```

`git add -N` makes new files visible to `git diff`; the diff is taken against
the instance's `base_commit`, so it includes new, modified and deleted files.
Exclusions cover `__pycache__`, `*.pyc`, `.venv`, `.pytest_cache`,
`.mypy_cache`, `.ruff_cache`, `node_modules`, `build`, `dist`, `target` and
common editor files (`.swp`, `.swo`, `*~`, `.DS_Store`, `.idea`, `.vscode`).
Gitignore also applies, so normal generated output never enters the patch.

The extracted patch is rejected, with a clear error and no prediction, when it
contains a binary diff or NUL bytes, exceeds `--patch-cap-bytes`, or is not a
unified git diff. A non-empty patch is checked in a pristine worktree:

```text
git worktree add --detach /tmp/keel-apply-check-<token> <base_commit>
git -C /tmp/keel-apply-check-<token> apply --check --whitespace=error-all -
```

`patch_apply_ok` records that result. A patch that fails the check is still
submitted (the official harness decides), but the trace sets `error` and the
`patch-failure` tag. The runner never edits, resets or removes test files. It
records whether the agent touched them (`tests_modified`) and lets the official
harness grade the pristine tests.

## Output

```
benchmarks/swebench/runs/<run-id>/
  predictions.jsonl          # one {instance_id, model_name_or_path, model_patch} per line
  traces.jsonl               # trace sidecar, one object per instance
  harness.log                # official harness stdout/stderr and its command
  <run-id>.<model>.json      # harness report, when it completes
  instances/<instance-id>/
    agent.log                # command, exit code, stdout/stderr
    patch.diff               # final diff (absent for an empty patch)
    session/                 # keel data dir copied from the container
```

Session logs and the final diff sit side by side per instance. Predictions are
appended as each instance finishes, so a partial run is still usable.

### Trace sidecar

`traces.jsonl` carries the canonical schema 1.0 fields:
`schema_version`, `benchmark`, `task_id`, `trial`, `model`, `model_version`,
`keel_target`, `agent_version`, `binary_sha256`, `start_time`, `end_time`,
`turns`, `tool_calls`, `input_tokens`, `output_tokens`, `reasoning_tokens`,
`cost_usd`, `stop_reason`, `wall_clock_s`, `patch`, `patch_size`,
`patch_apply_ok`, `verifier_result`, `failure_tags`, `session_log_path`,
`error`.

Minimal runner extensions (unknown fields are tolerated only in non-strict
validation):

- `patch_path` is the saved `instances/<id>/patch.diff`, or null.
- `empty_patch` is the empty-patch flag.
- `tests_modified` and `modified_test_files` are the test-change detection.

Details:

- `patch` is the **patch text**, not a path. `patch_size` is exactly
  `len(patch.encode("utf-8"))`.
- An empty patch is `patch: ""`, `patch_size: 0`, `patch_apply_ok: true`.
- A rejected binary or oversized patch is `patch: ""`, `patch_size: 0`,
  `patch_apply_ok: false`, with `error` and `failure_tags` explaining why.
- A run that fails before patch extraction is `patch: ""`,
  `patch_apply_ok: null`, with `error` set; that is distinct from a successful
  empty patch and from a rejected patch.
- `verifier_result` is `pass|fail|error|not_run` from the official report.
- `failure_tags` uses only the 16 canonical tags.
- `stop_reason` is the last assistant stop reason, or `timeout`, `error`, or
  `unknown` when no session stop was recorded.
- `cost_usd` is null because keel does not report cost; token counts are summed
  per-turn usage from the session JSONL. Timestamps are ISO 8601 UTC.

## Gold-patch check

Before spending tokens on keel, check the predictions format and the harness
with a known-good patch:

```sh
python3 benchmarks/swebench/run_keel.py \
  --dataset benchmarks/swebench/data/swebench.jsonl \
  --instances astropy__astropy-12907 \
  --gold-patch \
  --model-label gold \
  --run-id gold-check-2026-11-21
```

This writes the dataset's gold patch as `model_patch` and invokes the official
harness. Because the gold patch should apply, the instance should appear in
`resolved_ids`; an unresolved result points at the dataset/image/harness setup,
not at keel. `--skip-harness` writes the predictions without evaluating them.
Gold mode does not need Docker or a provider key.

## Evaluation

The runner invokes, from the run directory:

```sh
python -m swebench.harness.run_evaluation \
  --dataset_name princeton-nlp/SWE-bench_Lite \
  --split test \
  --predictions_path <run-dir>/predictions.jsonl \
  --max_workers 1 \
  --run_id <run-id> \
  --timeout 1800
```

`--harness-dataset` and `--split` must match the instances you ran. A manual
equivalent, if you want to re-run the harness yourself:

```sh
python -m swebench.harness.run_evaluation \
  --dataset_name princeton-nlp/SWE-bench_Lite \
  --split test \
  --predictions_path "$PWD/benchmarks/swebench/runs/<run-id>/predictions.jsonl" \
  --max_workers 1 \
  --run_id "<run-id>-manual"
```

The report is a JSON file in the run directory with `resolved_ids`,
`unresolved_ids`, `error_ids` and `empty_patch_ids`. The runner maps those to
`verifier_result` in `traces.jsonl`.

## Bring-up vs frontier

Run SWE-bench Lite (or a fixed subset of it) with one or a few trials to
bring up the pipeline and find harness, image and provider problems. Do not
call that a frontier result.

A frontier-style claim needs SWE-bench Verified with the matching
`--harness-dataset`, at least three trials per task, the exact model slug and
version, the binary checksum, the full trace for every trial, a stated deadline,
and the official harness report. An empty patch is unresolved, not a pass; a
patch that only changes tests earns no credit because the harness grades
pristine tests. Report `resolved / submitted`, not a hand-picked success rate.

## Unit tests

```sh
python3 -m unittest discover -s benchmarks/tests -v
python3 -m compileall benchmarks
```

The SWE-bench tests use temporary local git repositories and fake command
runners. They cover modified/new/deleted files, generated-artifact exclusion,
empty patches, whitespace errors, binary and oversized patch rejection,
predictions shape, command construction, timeout handling, trace schema and a
fake end-to-end instance run. They do not need Docker, a Linux binary,
`swebench` or a provider key.

## Live status

Checked on 2026-11-21 on macOS with Docker (OrbStack) 29.8.2:

- Docker daemon: running.
- Docker plumbing smoke on the cached `alpine:latest` image: passed. It
  verified `docker exec -e NAME` forwarding from the host, `docker cp` plus
  `chmod +x`, `-w /testbed`, and stdin piping for `git apply --check`.
- `swebench` package: not installed (`ModuleNotFoundError`).
- Official SWE-bench images: none cached; the images are multi-gigabyte, so
  this wave did not pull one or run a live SWE-bench instance.

The exact live command, after installing `swebench` and building a Linux keel
binary, is the one-instance command above with `--run-id` and no `--dry-run`.
Blockers today: harness installation, the multi-gigabyte image pull (and
amd64 emulation on Apple Silicon), and the provider key on the host. The unit
suite, dry-run and Docker plumbing smoke are the verification that this wave
can claim; no live SWE-bench pass is claimed.

Residual risks: image architecture/libc differences, provider latency against
the per-instance timeout, harness dataset/version drift, very large diffs being
captured in memory before the cap check, and cost not being tracked by keel.
