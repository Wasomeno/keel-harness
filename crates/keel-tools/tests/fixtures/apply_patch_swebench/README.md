# M12.11 SWE-bench `apply_patch` sample

These are the verbatim `patch` arguments from the 13 `apply_patch` calls in the
M12.11 trial-0 SWE-bench run at commit `78e0efb`. The source was the read-only
session logs under `/tmp/keel-swebench-m12.11/trial-0`; nothing in those logs
was modified.

The sample covers ten instances:

- `django__django-10914`: 2 calls, both parse errors (new-side overcount, and
  a Codex-style `*** End Patch` terminator).
- `pylint-dev__pylint-7080`: 3 calls, all parse errors (premature next hunk and
  impossible first-hunk counts).
- `pytest-dev__pytest-11143`: 4 calls, 1 success (exact `-19 +23` counts) and
  3 parse errors (extra body lines or a Codex terminator).
- `psf__requests-1963`: 1 parse error (both sides off by one).
- `scikit-learn__scikit-learn-12471`: 1 parse error (body counts much larger
  than the header).
- `sphinx-doc__sphinx-7738`: 2 calls; one parses but starts its context one
  line too late, and one has an extra body line after a short header.

The tests in `crates/keel-tools/src/builtin/apply_patch.rs` name the instance
and the failure family so a regression points back at the run that found it.
