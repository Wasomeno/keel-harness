"""Check the clamp task. Run from the workspace root."""

import os
import sys

sys.path.insert(0, os.getcwd())

try:
    from keeltoy.numbers import clamp
except Exception as error:
    print("FAIL")
    print(f"- import failed: {type(error).__name__}: {error}")
    raise SystemExit(1)

failures = []


def expect(label, actual, expected):
    if actual != expected:
        failures.append(f"{label}: expected {expected!r}, got {actual!r}")


expect("clamp(-1, 0, 10)", clamp(-1, 0, 10), 0)
expect("clamp(0, 0, 10)", clamp(0, 0, 10), 0)
expect("clamp(5, 0, 10)", clamp(5, 0, 10), 5)
expect("clamp(10, 0, 10)", clamp(10, 0, 10), 10)
expect("clamp(11, 0, 10)", clamp(11, 0, 10), 10)

if failures:
    print("FAIL")
    for failure in failures:
        print(f"- {failure}")
    raise SystemExit(1)
print("ok: clamp")
