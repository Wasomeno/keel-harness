"""Check the unique task. Run from the workspace root."""

import os
import sys

sys.path.insert(0, os.getcwd())

try:
    from keeltoy.lists import unique
except Exception as error:
    print("FAIL")
    print(f"- import failed: {type(error).__name__}: {error}")
    raise SystemExit(1)

failures = []


def expect(label, actual, expected):
    if actual != expected:
        failures.append(f"{label}: expected {expected!r}, got {actual!r}")


expect("unique([3, 1, 2, 3])", unique([3, 1, 2, 3]), [3, 1, 2])
expect("unique([])", unique([]), [])
expect("unique([1, 1, 1])", unique([1, 1, 1]), [1])

if failures:
    print("FAIL")
    for failure in failures:
        print(f"- {failure}")
    raise SystemExit(1)
print("ok: unique")
