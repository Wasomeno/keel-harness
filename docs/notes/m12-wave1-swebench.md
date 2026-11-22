# M12 wave 1 SWE-bench Lite: restricted-egress rerun

**Date:** 2026-11-22
**Status:** complete; network-clean; 7/10 raw and 7/9 environment-valid; one trial, not a Lite-wide score
**Scope:** M12.6 patch finalization and M12.7 test loop on the ten-instance SWE-bench Lite bring-up set
**Plan:** [`benchmark-readiness.md`](benchmark-readiness.md)
**Companion:** [`m12-wave1-results.md`](m12-wave1-results.md), which covers the separate Terminal-Bench rerun

This note records the final SWE-bench Lite run under restricted egress and keeps
the earlier open-egress run only as contrast. The run used the binary built from
clean `e5a1707`, sha256
`bf8e9977eeeedf24fed980e13e2d0bd69c8e72f379927da890017badf19219bc`, with the
official `swebench==5.0.2` harness, one trial per instance, `max_turns=75` and a
3,600 s agent timeout. The sweep took 4 h 02 min and 162.4 agent minutes. Raw
artifacts are in `/tmp/keel-swebench-m12-wave1-restricted`; the detailed run
report is `engineering/m12.6-m12.7-swebench-lite-restricted.md` v2 in the shared
space.

The earlier open-egress run at `a035307` resolved 9 of 10 officially, but seven
agents fetched upstream reference material or benchmark data and only three of
ten were clean. That score was contaminated and is not a baseline for this one.

This is a different measurement from the Terminal-Bench rerun. It has different
tasks, a different harness path and no comparable control arm; do not combine the
two results or read either as a pass@1 improvement.

## Results

| Instance | Official outcome | Patch bytes | Test runs | Stop reason | Turns | Agent min |
| --- | --- | ---: | ---: | --- | ---: | ---: |
| `astropy__astropy-12907` | resolved | 1,421 | 14 | `end_turn` | 57 | 6.6 |
| `django__django-10914` | resolved | 2,371 | 21 | `end_turn` | 66 | 18.1 |
| `matplotlib__matplotlib-23314` | resolved | 903 | 22 | `max_turns` | 75 | 27.5 |
| `psf__requests-1963` | unresolved | 1,308 | 12 | `end_turn` | 44 | 6.2 |
| `pydata__xarray-4094` | resolved | 2,142 | 9 | `end_turn` | 48 | 5.2 |
| `pylint-dev__pylint-7080` | resolved | 449 | 5 | `max_turns` | 75 | 13.3 |
| `pytest-dev__pytest-11143` | resolved | 999 | 18 | `end_turn` | 42 | 6.4 |
| `scikit-learn__scikit-learn-12471` | resolved | 1,934 | 16 | `end_turn` | 69 | 10.5 |
| `sphinx-doc__sphinx-7738` | unresolved | 4,296 | 8 | `max_turns` | 150 | 37.3 |
| `sympy__sympy-11897` | empty patch | 0 | 2 | `max_turns` | 150 | 31.3 |

Raw: **7 of 10 resolved (0.700)**, with 127 recorded test runs and 162.4 agent
minutes. `psf__requests-1963` is officially unresolved but environment-invalid:
`FAIL_TO_PASS` passed 7/7, `PASS_TO_PASS` passed 110/112, and the two graded
failures, `test_pyopenssl_redirect` and `test_stream_timeout`, are the same SSL
`SSLCertVerificationError` failures seen with egress open. It stays in the raw
denominator and is excluded only from the second aggregate: **7 of 9 (0.778)
environment-valid**, with sympy still counted as a harness miss.

## Egress restriction was measured

All ten runs recorded `network_policy="allowlist"`, allowed host
`api.neuralwatt.com`, and `network_contaminated=false`. The ten sessions contain
16 network-tool commands: 14 were blocked by DNS or routing, two were local
fake-adapter tests, and no non-provider host was reached. The canary's direct
egress probes also failed while model calls completed. In the earlier run, seven
of ten agents reached reference material.

## The earlier contaminated solves did not survive

- **Sphinx** previously resolved after downloading Sphinx 3.1.0 and copying the
  guarded fix. Under restricted egress both `pip download` attempts were blocked,
  the target `test_underscore_in_attribute` failed, and the run ended unresolved.
- **Sympy** previously resolved after a HuggingFace SWE-bench query returned the
  task's own test patch. Under restricted egress both attempts hit the 75-turn
  cap, exited 3 with no workspace change, and the official result is an empty
  patch.
- **Requests** is an environment failure, not a harness miss. Its patch applied
  and its target tests passed; only the two SSL environment tests failed.

## M12.6 patch finalization

The nine non-empty patches are clean. All have zero `old mode`/`new mode` pairs,
no generated-file paths, no final test-file changes, and pass both the whitespace
gate and the pristine-worktree apply check. Sizes range from 449 to 4,296 bytes.
Astropy's and sphinx's dependency/config edits are environment edits, not
generated noise. Sympy has no patch.

The empty-patch retry was exercised end to end for the first time. Sphinx's first
attempt spent 75 turns without changing the workspace; the runner retried once,
and the retry produced the 4,296-byte patch that applied cleanly. It still failed
the target test, but the retry path worked. Sympy also exhausted two 75-turn
attempts and still produced no patch, so an empty patch is submitted. The retry
works; it also costs a second full turn budget.

## M12.7 test loop

Tests ran in all ten instances: 127 recognized runs, up from 84 in the earlier
run. Django recorded 21 runs instead of 2, the old `cd: tests` shell error is
gone, and the trace contains a real `FAILED (errors=19)` excerpt. `stop_reason`
now reports `max_turns` on every capped run: matplotlib and pylint at 75 turns,
sphinx and sympy at 150 across two attempts. Discovery is still mixed; it does
not reliably select the covering test.

There is no control arm: no run with the test loop disabled. The run shows that
the patch-finalization and test-loop mechanisms function on real repositories.
It does **not** show that either raises the score.

## Limits

- Ten-instance bring-up subset, one model and one trial per instance; this is not
  a SWE-bench Lite-wide score and model variance is not measured.
- The 75-turn cap binds on three patched instances (matplotlib, pylint and
  sphinx) and on both sympy attempts. A larger turn budget could change results.
- There is no control arm. No score improvement is claimed for M12.6 or M12.7.
- Requests is environment-invalid but remains in the raw 7/10 count. Sympy is a
  harness miss and remains in both denominators.
- Official harness evaluation containers have their own internet access; that is
  separate from the agent network policy.
- The 10-task Terminal-Bench adapter still does not pass `--test-loop`, so that
  benchmark cannot measure M12.7 yet.

## Earlier open-egress run

The earlier run at `a035307` used the same ten-instance set but had no egress
restriction. It resolved 9 of 10 officially, with seven agents reaching upstream
source, tests or the benchmark dataset, including the exact pylint PR diff and a
HuggingFace query returning sympy's test patch. Only astropy, django and pytest
were clean (3 of 10, not a pass rate), and the run recorded 84 test runs. It
remains useful as the contrast that shows why the restricted-egress result is
the one to keep.

## Next

No SWE-bench run is pending. The next measurement that could support a score
claim needs a control arm and more than one trial; until then the honest claim is
that both features function on real repositories, not that they improve pass@1.
