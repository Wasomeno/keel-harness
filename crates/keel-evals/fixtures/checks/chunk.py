"""Check the chunk task. Run from the workspace root."""

import os
import sys

sys.path.insert(0, os.getcwd())

try:
    from keeltoy.lists import chunk
except Exception as error:
    print("FAIL")
    print(f"- import failed: {type(error).__name__}: {error}")
    raise SystemExit(1)

failures = []


def call(label, values, size, expected):
    try:
        actual = chunk(values, size)
    except Exception as error:
        failures.append(f"{label}: raised {type(error).__name__}: {error}")
        return
    if actual != expected:
        failures.append(f"{label}: expected {expected!r}, got {actual!r}")


def expect_value_error(label, values, size):
    try:
        actual = chunk(values, size)
    except ValueError:
        return
    except Exception as error:
        failures.append(f"{label}: expected ValueError, got {type(error).__name__}: {error}")
        return
    failures.append(f"{label}: expected ValueError, got {actual!r}")


call("chunk([1, 2, 3, 4, 5], 2)", [1, 2, 3, 4, 5], 2, [[1, 2], [3, 4], [5]])
call("chunk([], 3)", [], 3, [])
call("chunk([1, 2], 5)", [1, 2], 5, [[1, 2]])
expect_value_error("chunk([1], 0)", [1], 0)
expect_value_error("chunk([1], -1)", [1], -1)

if failures:
    print("FAIL")
    for failure in failures:
        print(f"- {failure}")
    raise SystemExit(1)
print("ok: chunk")
