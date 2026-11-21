# M12 wave 1 SWE-bench Lite exploratory run: patch finalization and test loop

**Date:** 2026-11-22
**Status:** exploratory, contaminated; **not a benchmark result**
**Scope:** first live exercise of M12.6 patch finalization and M12.7 test loop on the ten-instance SWE-bench Lite bring-up set
**Plan:** [`benchmark-readiness.md`](benchmark-readiness.md)
**Companion:** [`m12-wave1-results.md`](m12-wave1-results.md), which covers the separate Terminal-Bench rerun

This run used the binary built from clean `5e461aa`, sha256
`f7ffb7935e09152bf3fbd7b212ce22f71144a9573c36dde6c017710eab2fd39a`, from
repository HEAD `a035307` (docs only). It ran one trial per instance with
`max_turns=75`, a 3,600 s agent timeout and the ten-instance Lite bring-up
dataset. The full sweep took 96 minutes and 85.1 agent minutes. Raw artifacts
are in `/tmp/keel-swebench-m12-wave1`; the detailed run report is
`engineering/m12.6-m12.7-swebench-lite.md` in the shared space.

The official harness resolved **9 of 10** instances. That number is
contaminated and must not be reported as a pass@1. Seven agents fetched
upstream reference material or the benchmark dataset over the network. Only
**astropy, django and pytest** were clean, which is **3 of 10**, not a pass
rate. The runner is being changed to start the agent container without network
access before any further M12.6 or M12.7 measurement.

This is a different measurement from the Terminal-Bench rerun. It has different
tasks, a different harness path and no comparable baseline; do not combine the
two results.

## M12.6 patch finalization held at ten-instance scale

All ten prediction patches:

- applied cleanly;
- had **zero `old mode`/`new mode` pairs**, the PR #32 regression;
- contained no generated-file paths (`__pycache__`, caches, build output,
  editor files) and no final test-file changes;
- passed the whitespace gate; and
- ranged from 438 to 2,763 bytes.

Every instance finished with `agent_attempts=1` and `retry_reason=null`; there
were no empty patches. The empty-patch and whitespace retry path was therefore
**still not exercised**, and this run cannot say whether it repairs a patch.

One instance, astropy, transiently edited a test assertion and reverted it with
`git checkout`. Its final patch is test-free. The astropy patch also pins
`setuptools` in `pyproject.toml`; the sphinx patch pins dependencies in
`setup.py` and adds `-rA` to `tox.ini`. Those are unrelated environment edits,
not generated-file noise, and the finalization guard did not treat them as
generated files.

## M12.7 test loop fired on real repositories

Tests ran in all ten instances: 84 recognized completed runs in total, 1 to 12
per instance. Test discovery produced:

- a usable command on three: matplotlib and xarray (`python -m pytest`), and
  sympy (`bin/test`, used as `python bin/test`);
- a broad but executable command on four: django (`tox`), requests
  (`make test`), sklearn (`make test`) and sphinx (`make test`); and
- an unusable or wrong-scope command on three: astropy (templated `tox` with
  GitHub Actions placeholders), pylint (a spelling CI job) and pytest (a
  release-notes `tox` environment).

Two real defects surfaced:

- **django undercounted test runs.** The agent issued 24 test commands, but the
  loop recorded only 2. The last recorded status was `failed`, yet
  `failing_tests` was empty and the pre-final prompt said "0 failing test(s)".
  The unittest parser missed output piped through `grep` and `tail`.
- **The 75-turn cap cut off pre-final verification.** pylint hit the cap with
  `tool_use` and never received the prompt. sympy also hit the cap with
  `tool_use`; its trace records one prompt, but the run stopped before a
  completed verification turn. The cap, not a disabled test loop, is the cause.

The run also showed that a passing last visible test does not predict the
official result: requests recorded `passed`, then failed two PASS_TO_PASS tests
in the official verifier with `SSLCertVerificationError`, an environment
failure unrelated to its patch.

## Contamination

The seven contaminated instances:

| instance | reference access |
| --- | --- |
| matplotlib | later `axes3d.py` releases and the upstream commit containing the fix |
| requests | later releases and upstream repository history; also an environmental SSL verifier failure |
| xarray | xarray 0.16.2's fixed `to_unstacked_dataset` |
| pylint | exact PR 7080 diff and metadata |
| sklearn | PR 12471 metadata plus 0.20.X/0.21.X encoder source and tests |
| sphinx | Sphinx 3.1.0 source and tests, including the upstream test file |
| sympy | HuggingFace SWE-bench query returning the task's test patch and `FAIL_TO_PASS`, plus upstream releases and repository |

Only astropy, django and pytest had no network reference access. If astropy's
transient test edit is treated as disqualifying, the clean count falls to 2 of
10, not 3.

## Limits

- Ten-instance bring-up subset, not a SWE-bench Lite-wide score.
- One model, one trial, 75 turns and a 3,600 s agent timeout; model variance is
  not measured.
- The agent containers had internet access, which is the main confound.
- The official 9/10 is contaminated; the clean 3/10 is the honest signal from
  this subset, and it does not establish a clean M12.6 or M12.7 gain.
- The empty-patch retry path and the unexercised parts of the test parser remain
  unmeasured.
- No valid before/after comparison exists for M12.7's test-run rate or
  `premature-finish` rate.

## Next

Run the prepared ten-instance set again only after the agent container starts
without network access. Treat the current run as exploratory evidence that the
patch-finalization guards and the test loop operate on real repositories, not
as a benchmark result or a pass@1 improvement.
