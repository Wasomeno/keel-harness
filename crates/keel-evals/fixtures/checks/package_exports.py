"""Check the package exports task. Run from the workspace root."""

import os
import sys

sys.path.insert(0, os.getcwd())

failures = []

try:
    import keeltoy
except Exception as error:
    print("FAIL")
    print(f"- import failed: {type(error).__name__}: {error}")
    raise SystemExit(1)

required = ["slugify", "parse_int", "mean", "chunk", "format_table"]
for name in required:
    if not hasattr(keeltoy, name):
        failures.append(f"`keeltoy.{name}` is not exported")
    if name not in getattr(keeltoy, "__all__", []):
        failures.append(f"`{name}` is not in `keeltoy.__all__`")

if failures:
    print("FAIL")
    for failure in failures:
        print(f"- {failure}")
    raise SystemExit(1)
print("ok: package_exports")
