# SWE-bench Pro V2 subset: PLACEHOLDER

**Status:** PLACEHOLDER. The fixed SWE-bench Pro V2 split is not available in
this repository yet. No SWE-bench Pro numbers may be reported until this file
is replaced by a pinned instance list.

`benchmarks/subsets/bringup-swebench-lite.txt` is only an adapter bring-up
subset of SWE-bench Lite. It is not a frontier claim and must not be used to
compare harness changes.

## Replacement method

When the fixed SWE-bench Pro V2 split is published:

1. Record the dataset name, revision or commit, checksum, and retrieval date in
   this file. Pin the revision; do not track a moving branch.
2. Keep only public instances that have a non-empty gold patch and test patch.
3. Sort the eligible `instance_id` values ascending and select 10 for the
   iteration subset with the deterministic rule
   `sha256("m12-swebench-pro-v2-iteration:" + instance_id)`, ascending, first
   10.
4. Select the canary independently with
   `sha256("m12-swebench-pro-v2-canary:" + instance_id)`, ascending, at least
   20% of the eligible split, excluding iteration instances.
5. Write the lists to `iteration-swebench-pro-v2.txt` and
   `canary-swebench-pro-v2.txt`, add them to the manifest tests, and run a
   gold-patch smoke test on one instance before any keel trial.
6. Keep the Lite bring-up subset for adapter work; use Pro V2 for frontier
   claims only after the smoke test and a full gold-patch pass.

The canary is never used for tuning. The iteration and canary lists must be
frozen before the first comparison run and recorded with the run's trace files.
