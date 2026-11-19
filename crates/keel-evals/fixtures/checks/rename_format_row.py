"""Check the format_row rename task. Run from the workspace root."""

import os
import sys

sys.path.insert(0, os.getcwd())

failures = []

try:
    from keeltoy.report import format_cells
except Exception as error:
    print("FAIL")
    print(f"- import failed: {type(error).__name__}: {error}")
    raise SystemExit(1)

try:
    from keeltoy import format_cells as package_format_cells
except Exception as error:
    print("FAIL")
    print(f"- package re-export failed: {type(error).__name__}: {error}")
    raise SystemExit(1)


def expect(label, actual, expected):
    if actual != expected:
        failures.append(f"{label}: expected {expected!r}, got {actual!r}")


expect("format_cells output", format_cells(["a", "bb"], [2, 3]), "a  | bb ")
expect("package format_cells is the same function", package_format_cells, format_cells)

try:
    import keeltoy.report

    if hasattr(keeltoy.report, "format_row"):
        failures.append("keeltoy.report still defines `format_row`")
except Exception as error:
    failures.append(f"could not inspect keeltoy.report: {type(error).__name__}: {error}")

try:
    import keeltoy

    if hasattr(keeltoy, "format_row"):
        failures.append("the package root still exports `format_row`")
except Exception as error:
    failures.append(f"could not inspect keeltoy: {type(error).__name__}: {error}")

if failures:
    print("FAIL")
    for failure in failures:
        print(f"- {failure}")
    raise SystemExit(1)
print("ok: rename_format_row")
