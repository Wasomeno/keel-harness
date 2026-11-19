"""Check the parse_int task. Run from the workspace root."""

import os
import sys

sys.path.insert(0, os.getcwd())

try:
    from keeltoy.numbers import parse_int
except Exception as error:
    print("FAIL")
    print(f"- import failed: {type(error).__name__}: {error}")
    raise SystemExit(1)

failures = []


def call(label, text, expected):
    try:
        actual = parse_int(text)
    except Exception as error:
        failures.append(f"{label}: raised {type(error).__name__}: {error}")
        return
    if actual != expected:
        failures.append(f"{label}: expected {expected!r}, got {actual!r}")


def expect_value_error(label, text):
    try:
        actual = parse_int(text)
    except ValueError:
        return
    except Exception as error:
        failures.append(f"{label}: expected ValueError, got {type(error).__name__}: {error}")
        return
    failures.append(f"{label}: expected ValueError, got {actual!r}")


call('parse_int(" 42 ")', " 42 ", 42)
call('parse_int("+7")', "+7", 7)
call('parse_int("-3")', "-3", -3)
call('parse_int("1_000")', "1_000", 1000)
expect_value_error('parse_int("")', "")
expect_value_error('parse_int("4.2")', "4.2")
expect_value_error('parse_int("12x")', "12x")

if failures:
    print("FAIL")
    for failure in failures:
        print(f"- {failure}")
    raise SystemExit(1)
print("ok: parse_int")
