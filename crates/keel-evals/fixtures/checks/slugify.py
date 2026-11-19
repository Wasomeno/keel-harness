"""Check the slugify task. Run from the workspace root."""

import os
import sys

sys.path.insert(0, os.getcwd())

try:
    from keeltoy.text import slugify
except Exception as error:
    print("FAIL")
    print(f"- import failed: {type(error).__name__}: {error}")
    raise SystemExit(1)

failures = []


def call(label, text, expected):
    try:
        actual = slugify(text)
    except Exception as error:
        failures.append(f"{label}: raised {type(error).__name__}: {error}")
        return
    if actual != expected:
        failures.append(f"{label}: expected {expected!r}, got {actual!r}")


call('slugify("Hello, World!")', "Hello, World!", "hello-world")
call('slugify("  Rust 2024  ")', "  Rust 2024  ", "rust-2024")
call('slugify("a__b")', "a__b", "a-b")
call('slugify("Café au lait")', "Café au lait", "caf-au-lait")

if failures:
    print("FAIL")
    for failure in failures:
        print(f"- {failure}")
    raise SystemExit(1)
print("ok: slugify")
