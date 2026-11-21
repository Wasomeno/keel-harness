# Fixed benchmark subsets

Subsets are fixed on purpose. Choosing tasks after seeing results turns a
benchmark into a tuning set and makes comparisons meaningless. Change a subset
only at a batch boundary, with the new revision recorded in this directory and
in the run notes.

| file | benchmark | size | use |
| --- | --- | --- | --- |
| [`iteration-terminal-bench.txt`](iteration-terminal-bench.txt) | Terminal-Bench 2.0 | 10 | Tuning and A/B comparisons. |
| [`canary-terminal-bench.txt`](canary-terminal-bench.txt) | Terminal-Bench 2.0 | 20 (22.5%) | Guard against overfitting; never tuning. |
| [`terminal-bench-2.0-tasks.txt`](terminal-bench-2.0-tasks.txt) | Terminal-Bench 2.0 | 89 | Pinned manifest used to validate the subsets. |
| [`bringup-swebench-lite.txt`](bringup-swebench-lite.txt) | SWE-bench Lite | 10 | Adapter bring-up only, not a frontier claim. |
| [`swebench-pro-v2.placeholder.md`](swebench-pro-v2.placeholder.md) | SWE-bench Pro V2 | placeholder | Replacement method for the fixed split. |

All lists are plain text: comments start with `#`, blank lines are ignored, and
each remaining line is one task or instance ID. The manifest tests in
`benchmarks/tests/test_traces_subsets.py` enforce counts, uniqueness,
disjointness, the 20% canary floor and the documented hash rule.

## Terminal-Bench 2.0

**Provenance:** [`harbor-framework/terminal-bench-2`](https://github.com/harbor-framework/terminal-bench-2),
revision `2fd12b88aafdd04a52c298e3940bcb189f9766d6`, 89 task directories each
with a `task.toml` whose `task.name` is `terminal-bench/<id>`. The
`harbor-framework/terminal-bench` main manifest currently describes
Terminal-Bench 3, so it is not the source for Terminal-Bench 2.0 IDs. The pinned
manifest here is the reference.

IDs keep the `terminal-bench/` prefix because that is the `task.name` in each
`task.toml`. An adapter that wants the bare slug can strip the first
`terminal-bench/` component; the trace's `task_id` should keep the full ID used
in the subset file.

A full run is all 89 tasks. Run it only at the start and end of the batch, not
after every harness change.

### Iteration subset: 10 tasks

| task | category | difficulty | expert estimate |
| --- | --- | --- | --- |
| `terminal-bench/fix-git` | software-engineering | easy | 5 min |
| `terminal-bench/extract-elf` | file-operations | medium | 30 min |
| `terminal-bench/regex-log` | data-processing | medium | 45 min |
| `terminal-bench/nginx-request-logging` | system-administration | medium | 20 min |
| `terminal-bench/custom-memory-heap-crash` | debugging | medium | 30 min |
| `terminal-bench/query-optimize` | data-science | medium | 60 min |
| `terminal-bench/modernize-scientific-stack` | scientific-computing | medium | 120 min |
| `terminal-bench/kv-store-grpc` | software-engineering | medium | 15 min |
| `terminal-bench/fix-code-vulnerability` | security | hard | 120 min |
| `terminal-bench/cancel-async-tasks` | software-engineering | hard | 120 min |

Selection rules:

1. Cover at least eight categories and all three difficulty levels. The chosen
   set has 8 categories, 1 easy, 7 medium and 2 hard tasks.
2. Cover the failure modes the batch is supposed to move: localization and
   wrong-file (`fix-code-vulnerability`), missed-requirement (`regex-log`),
   integration-error (`nginx-request-logging`, `kv-store-grpc`), wrong-logic
   (`custom-memory-heap-crash`, `modernize-scientific-stack`), regression and
   test mismatch (`modernize-scientific-stack`, `fix-code-vulnerability`) and
   timeout or premature finish (`cancel-async-tasks`, `fix-git`).
3. Keep a cheap smoke task (`fix-git`) so a broken adapter is caught before
   spending hours.
4. Freeze the list before the first comparison. Do not add a task because it
   failed in a run, and do not remove one because it passed. A subset change is
   a new batch, not a tuning step.

Three trials per task are the default. A/B runs use the same subset, model,
limits and trial count.

### Canary subset: 20 tasks (22.5%)

The canary is selected by a deterministic rule, not by hand:

```text
exclude the iteration tasks
sort the remaining 79 task IDs by sha256("m12-canary-v1:" + task_id)
take the first 20
```

The result covers 10 categories: scientific-computing 3, system-administration
3, software-engineering 4, mathematics 3, security 2, data-querying 1,
debugging 1, model-training 1, optimization 1, file-operations 1. Six are hard,
13 are medium and one is easy. The exact list is in
`canary-terminal-bench.txt`.

**The canary is never used for tuning.** Do not inspect canary failures to pick
a prompt, tool or patch, and do not run the canary on intermediate branches
until the iteration subset has chosen a candidate. After a candidate is chosen,
run the canary as a check. If the iteration subset improves but the canary gets
worse, do not promote the change. If both are flat, say so; keep the change only
if it removes a known failure mode.

### Full set

The full set is the 89-task manifest. Report it as the batch baseline and final
result only. A full set is not an iteration loop: it is too expensive and too
easy to overfit by repeated inspection.

## SWE-bench

### Lite bring-up subset: 10 instances

**Provenance:** [`princeton-nlp/SWE-bench_Lite`](https://huggingface.co/datasets/princeton-nlp/SWE-bench_Lite),
test split, 300 instances, revision
`6ec7bb89b9342f664a54a6e0a6ea6501d3437cc2`.

This subset exists to bring up the adapter, patch extraction and the official
`run_evaluation` path. It is **not** a frontier claim and must not be reported
as a SWE-bench score or used to compare harness changes.

Selection rules:

1. Rank repositories by Lite instance count descending, then repository name
   ascending. Take the first 10 repositories.
2. In each chosen repository, pick the instance whose gold `patch` has the
   fewest changed lines (the line count of the dataset `patch` field). Ties are
   broken by `instance_id` ascending.
3. Freeze the result before running keel. The small gold patches keep the
   bring-up loop short; they do not make the instances easy for the agent.

| instance | repository | gold patch lines |
| --- | --- | --- |
| `astropy__astropy-12907` | astropy/astropy | 12 |
| `django__django-10914` | django/django | 12 |
| `matplotlib__matplotlib-23314` | matplotlib/matplotlib | 12 |
| `psf__requests-1963` | psf/requests | 16 |
| `pydata__xarray-4094` | pydata/xarray | 12 |
| `pylint-dev__pylint-7080` | pylint-dev/pylint | 11 |
| `pytest-dev__pytest-11143` | pytest-dev/pytest | 11 |
| `scikit-learn__scikit-learn-12471` | scikit-learn/scikit-learn | 19 |
| `sphinx-doc__sphinx-7738` | sphinx-doc/sphinx | 12 |
| `sympy__sympy-11897` | sympy/sympy | 17 |

Use at least one instance to prove that a patch is produced, applies cleanly,
and is graded by the pristine official harness. Keep the predictions file and
the harness logs next to the trace.

### SWE-bench Pro V2 replacement

SWE-bench Pro V2 is the intended frontier split. It is not available in this
repository yet, so no Pro V2 numbers may be reported. The replacement method is
documented in [`swebench-pro-v2.placeholder.md`](swebench-pro-v2.placeholder.md):
pin the dataset revision, keep public instances with non-empty gold and test
patches, select 10 iteration instances by
`sha256("m12-swebench-pro-v2-iteration:" + instance_id)` and a canary of at
least 20% by the distinct `m12-swebench-pro-v2-canary` hash, then run a
gold-patch smoke test before any keel trial.

Lite remains the adapter bring-up set; Pro V2 replaces it for frontier claims,
not for unit tests.

## A/B and promotion

- One harness change per branch.
- Same subset, same model, same limits, three trials.
- Compare per-task outcomes, not only aggregates. Use a paired bootstrap on the
  per-task difference and report the interval.
- Promote only if the iteration subset improves and the canary does not get
  worse.
- If the result is flat, say it is flat. Keep the change only when it removes a
  known failure mode, and record the failure tag it addresses.
- Never tune against the canary, and never edit a subset after seeing a result.

## Changing a subset

Do this only at a batch boundary:

1. Record the dataset name, revision and checksum in the file header and here.
2. Regenerate the iteration and canary lists with the documented rule.
3. Keep iteration and canary disjoint.
4. Update the manifest tests and run
   `python3 -m unittest discover -s benchmarks/tests -v`.
5. Record the new subset revision in the first trace or run note.

A subset edit without a new revision is a benchmark change disguised as a
harness result.
