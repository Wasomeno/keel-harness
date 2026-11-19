"""Check the README slugify task. Run from the workspace root."""

import os

failures = []

try:
    readme = open("README.md", encoding="utf-8").read()
except Exception as error:
    print("FAIL")
    print(f"- could not read README.md: {type(error).__name__}: {error}")
    raise SystemExit(1)

for needle, label in [
    ("## slugify", "a `## slugify` section"),
    ("lowercase", "the word `lowercase`"),
    ("hello-world", "the example `hello-world`"),
]:
    if needle not in readme:
        failures.append(f"README.md is missing {label}")

if failures:
    print("FAIL")
    for failure in failures:
        print(f"- {failure}")
    raise SystemExit(1)
print("ok: readme_slugify")
