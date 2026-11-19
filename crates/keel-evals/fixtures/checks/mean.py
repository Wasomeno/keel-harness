"""Check the mean task. Run from the workspace root."""

import os
import sys

sys.path.insert(0, os.getcwd())

try:
    from keeltoy.numbers import mean
except Exception as error:
    print("FAIL")
    print(f"- import failed: {type(error).__name__}: {error}")
    raise SystemExit(1)

failures = []


def call(label, values, expected):
    try:
        actual = mean(values)
    except Exception as error:
        failures.append(f"{label}: raised {type(error).__name__}: {error}")
        return
    if actual != expected:
        failures.append(f"{label}: expected {expected!r}, got {actual!r}")


def expect_value_error(label, values):
    try:
        actual = mean(values)
    except ValueError:
        return
    except Exception as error:
        failures.append(f"{label}: expected ValueError, got {type(error).__name__}: {error}")
        return
    failures.append(f"{label}: expected ValueError, got {actual!r}")


call("mean([1, 2, 3])", [1, 2, 3], 2.0)
call("mean([2.0, 4.0])", [2.0, 4.0], 3.0)
expect_value_error("mean([])", [])

if failures:
    print("FAIL")
    for failure in failures:
        print(f"- {failure}")
    raise SystemExit(1)
print("ok: mean")
