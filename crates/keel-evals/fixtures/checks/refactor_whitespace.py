"""Check the whitespace refactor task. Run from the workspace root."""

import os
import sys

sys.path.insert(0, os.getcwd())

try:
    from keeltoy.text import dedupe_whitespace, normalize_space
except Exception as error:
    print("FAIL")
    print(f"- import failed: {type(error).__name__}: {error}")
    raise SystemExit(1)

failures = []


def expect(label, actual, expected):
    if actual != expected:
        failures.append(f"{label}: expected {expected!r}, got {actual!r}")


try:
    source = open(os.path.join("keeltoy", "text.py"), encoding="utf-8").read()
except Exception as error:
    print("FAIL")
    print(f"- could not read keeltoy/text.py: {type(error).__name__}: {error}")
    raise SystemExit(1)

collapses = source.count('" ".join(text.split())')
if "def _collapse(" not in source:
    failures.append("keeltoy/text.py does not define `def _collapse(`")
if collapses != 1:
    failures.append(
        'the expression `" ".join(text.split())` must appear exactly once, '
        f"found {collapses}"
    )
if source.count("_collapse(") < 3:
    failures.append("`_collapse` must be defined and called from both public functions")

expect('normalize_space(" a\\tb\\nc ")', normalize_space(" a\tb\nc "), "a b c")
expect('dedupe_whitespace(" a\\tb\\nc ")', dedupe_whitespace(" a\tb\nc "), "a b c")

if failures:
    print("FAIL")
    for failure in failures:
        print(f"- {failure}")
    raise SystemExit(1)
print("ok: refactor_whitespace")
