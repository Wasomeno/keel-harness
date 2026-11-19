"""Check the format_table task. Run from the workspace root."""

import os
import sys

sys.path.insert(0, os.getcwd())

try:
    from keeltoy.report import format_table
except Exception as error:
    print("FAIL")
    print(f"- import failed: {type(error).__name__}: {error}")
    raise SystemExit(1)

failures = []


def call(label, headers, rows, expected):
    try:
        actual = format_table(headers, rows)
    except Exception as error:
        failures.append(f"{label}: raised {type(error).__name__}: {error}")
        return
    if actual != expected:
        failures.append(f"{label}: expected {expected!r}, got {actual!r}")


call(
    "format_table(id/name)",
    ["id", "name"],
    [["1", "a"], ["22", "bb"]],
    "id | name\n---+-----\n1  | a\n22 | bb",
)
call(
    "format_table(single column)",
    ["n"],
    [["1"], ["10"]],
    "n\n--\n1\n10",
)

if failures:
    print("FAIL")
    for failure in failures:
        print(f"- {failure}")
    raise SystemExit(1)
print("ok: format_table")
