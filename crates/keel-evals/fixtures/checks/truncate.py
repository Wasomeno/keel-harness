"""Check the truncate task. Run from the workspace root."""

import os
import sys

sys.path.insert(0, os.getcwd())

try:
    from keeltoy.text import truncate
except Exception as error:
    print("FAIL")
    print(f"- import failed: {type(error).__name__}: {error}")
    raise SystemExit(1)

failures = []


def expect(label, actual, expected):
    if actual != expected:
        failures.append(f"{label}: expected {expected!r}, got {actual!r}")


expect('truncate("abcdef", 5)', truncate("abcdef", 5), "abcd…")
expect('truncate("abcde", 5)', truncate("abcde", 5), "abcde")
expect('truncate("ab", 5)', truncate("ab", 5), "ab")
expect('truncate("", 3)', truncate("", 3), "")

if failures:
    print("FAIL")
    for failure in failures:
        print(f"- {failure}")
    raise SystemExit(1)
print("ok: truncate")
