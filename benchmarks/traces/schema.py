"""Canonical benchmark trace schema, version 1.0.

A trace is one JSON object per benchmark trial, stored one object per line
(JSONL).  The schema is deliberately small: it records what the agent did, what
it cost, whether the patch applied, how the verifier graded it, and why it
failed.  Everything else (messages, tool output, diffs of intermediate files)
belongs in the session log referenced by ``session_log_path``.

The canonical required fields and their types are listed in
``benchmarks/traces/README.md``.  This module is the single source of truth for
validation; adapters and reporters must agree with it.

Only the Python standard library is used.
"""

from __future__ import annotations

import json
import math
import re
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

SCHEMA_VERSION = "1.0"

#: The fixed failure taxonomy from the benchmark plan.  Order is canonical and
#: the reporter always emits every tag, including zero counts.
FAILURE_TAGS: tuple[str, ...] = (
    "localization",
    "wrong-file",
    "missed-requirement",
    "integration-error",
    "wrong-logic",
    "syntax",
    "regression",
    "test-mismatch",
    "test-broken",
    "timeout",
    "context-overflow",
    "provider-error",
    "patch-failure",
    "environment-error",
    "premature-finish",
    "loop",
)

FAILURE_TAG_DESCRIPTIONS: dict[str, str] = {
    "localization": "The agent never found the code or artifact that had to change.",
    "wrong-file": "Work landed in the wrong file, package, or directory.",
    "missed-requirement": "The task instructions contained a requirement the patch ignored.",
    "integration-error": "Individually plausible changes do not work together or across components.",
    "wrong-logic": "The patch implements the wrong algorithm, condition, or data flow.",
    "syntax": "The delivered code or configuration does not parse or compile.",
    "regression": "Previously working behavior broke.",
    "test-mismatch": "The tests expect a different API, output, or behavior than the patch provides.",
    "test-broken": "The test command itself could not run or was left broken.",
    "timeout": "The trial hit a wall-clock, turn, or process deadline.",
    "context-overflow": "The context window filled or compaction lost required state.",
    "provider-error": "The model provider failed, throttled, or returned an unusable response.",
    "patch-failure": "The patch was empty, malformed, or did not apply.",
    "environment-error": "The container, network, dependency install, or tool bundle failed.",
    "premature-finish": "The agent stopped before completing or verifying the task.",
    "loop": "The agent repeated actions without making progress.",
}

VERIFIER_RESULTS: tuple[str, ...] = ("pass", "fail", "error", "not_run")

#: Optional harness counters a producer may attach.  They are not part of the
#: 26 required fields because not every adapter can observe them.  The reporter
#: falls back to failure tags where a direct counter is unavailable.
HARNESS_HEALTH_KEYS: tuple[str, ...] = (
    "tool_errors",
    "context_overflows",
    "provider_retries",
    "loops",
    "nudges",
    "test_command_failures",
)

REQUIRED_FIELDS: tuple[str, ...] = (
    "schema_version",
    "benchmark",
    "task_id",
    "trial",
    "model",
    "model_version",
    "keel_target",
    "agent_version",
    "binary_sha256",
    "start_time",
    "end_time",
    "turns",
    "tool_calls",
    "input_tokens",
    "output_tokens",
    "reasoning_tokens",
    "cost_usd",
    "stop_reason",
    "wall_clock_s",
    "patch",
    "patch_size",
    "patch_apply_ok",
    "verifier_result",
    "failure_tags",
    "session_log_path",
    "error",
)

#: Optional extension fields.  Unknown fields are ignored unless validation is
#: strict, which keeps room for future schema versions and adapter-specific
#: metadata without silently accepting typos.
OPTIONAL_FIELDS: tuple[str, ...] = ("harness_health", "failure_tag_notes")

_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
_NULLABLE_STRINGS = (
    "model_version",
    "agent_version",
    "binary_sha256",
    "session_log_path",
    "error",
)


class TraceValidationError(ValueError):
    """One or more trace records are malformed.

    ``errors`` holds every problem found in a single record so a fixer does not
    have to run the validator once per field.
    """

    def __init__(self, errors: Iterable[str], *, source: str | None = None):
        self.errors = list(errors)
        self.source = source
        prefix = f"{source}: " if source else ""
        detail = "\n  - ".join(self.errors)
        super().__init__(f"{prefix}invalid trace record:\n  - {detail}")


@dataclass(frozen=True, slots=True)
class LoadedTrace:
    """A parsed trace plus the place it came from, for error messages."""

    record: dict[str, Any]
    path: Path
    line: int

    @property
    def source(self) -> str:
        return f"{self.path}:{self.line}"


def _is_int(value: Any) -> bool:
    # bool is a subclass of int but is not a valid counter.
    return type(value) is int


def _is_number(value: Any) -> bool:
    if type(value) is int:
        return True
    return type(value) is float and math.isfinite(value)


def _is_nonempty_string(value: Any) -> bool:
    return isinstance(value, str) and bool(value.strip())


def _check_nonempty_string(
    record: Mapping[str, Any], field: str, errors: list[str]
) -> None:
    if field in record and not _is_nonempty_string(record[field]):
        errors.append(f"{field}: expected a non-empty string")


def _check_nullable_string(
    record: Mapping[str, Any], field: str, errors: list[str]
) -> None:
    if field in record and record[field] is not None and not _is_nonempty_string(
        record[field]
    ):
        errors.append(f"{field}: expected a non-empty string or null")


def _check_nonnegative_int(
    record: Mapping[str, Any], field: str, errors: list[str]
) -> None:
    if field not in record:
        return
    value = record[field]
    if not _is_int(value) or value < 0:
        errors.append(f"{field}: expected an integer >= 0, got {value!r}")


def _check_nonnegative_number(
    record: Mapping[str, Any], field: str, errors: list[str], *, nullable: bool = False
) -> None:
    if field not in record:
        return
    value = record[field]
    if nullable and value is None:
        return
    if not _is_number(value) or value < 0:
        suffix = " or null" if nullable else ""
        errors.append(f"{field}: expected a finite number >= 0{suffix}, got {value!r}")


def parse_utc_timestamp(value: Any) -> datetime:
    """Parse an ISO-8601 timestamp and require a UTC offset.

    ``Z`` and ``+00:00`` are both accepted; naive timestamps and non-zero
    offsets are rejected so traces from different machines sort correctly.
    """

    if not isinstance(value, str) or not value.strip():
        raise ValueError(
            "expected an ISO-8601 UTC timestamp such as 2026-11-21T10:00:00Z"
        )
    text = value.strip()
    if text.endswith(("Z", "z")):
        text = text[:-1] + "+00:00"
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError as exc:
        raise ValueError(
            "expected an ISO-8601 UTC timestamp such as 2026-11-21T10:00:00Z"
        ) from exc
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        raise ValueError("timestamp must include a UTC offset (Z or +00:00)")
    if parsed.utcoffset() != timedelta(0):
        raise ValueError("timestamp must be UTC (offset +00:00 or Z)")
    return parsed.astimezone(timezone.utc)


def _validate_failure_tags(value: Any, errors: list[str]) -> None:
    if not isinstance(value, list):
        errors.append("failure_tags: expected a list of tag strings")
        return
    seen: set[str] = set()
    for index, tag in enumerate(value):
        if not isinstance(tag, str):
            errors.append(f"failure_tags[{index}]: expected a string, got {tag!r}")
            continue
        if tag not in FAILURE_TAGS:
            errors.append(
                f"failure_tags[{index}]: unknown tag {tag!r}; "
                f"expected one of {', '.join(FAILURE_TAGS)}"
            )
        elif tag in seen:
            errors.append(f"failure_tags[{index}]: duplicate tag {tag!r}")
        seen.add(tag)


def _validate_harness_health(value: Any, errors: list[str]) -> None:
    if value is None:
        return
    if not isinstance(value, Mapping):
        errors.append("harness_health: expected an object of non-negative integers")
        return
    for key, count in value.items():
        if key not in HARNESS_HEALTH_KEYS:
            errors.append(
                f"harness_health.{key}: unknown counter; "
                f"expected one of {', '.join(HARNESS_HEALTH_KEYS)}"
            )
            continue
        if not _is_int(count) or count < 0:
            errors.append(
                f"harness_health.{key}: expected an integer >= 0, got {count!r}"
            )


def _validate_failure_tag_notes(value: Any, errors: list[str]) -> None:
    if value is None:
        return
    if not isinstance(value, list):
        errors.append("failure_tag_notes: expected a list of audit entries")
        return
    for index, note in enumerate(value):
        prefix = f"failure_tag_notes[{index}]"
        if not isinstance(note, Mapping):
            errors.append(f"{prefix}: expected an object")
            continue
        action = note.get("action")
        if action not in ("add", "replace", "remove"):
            errors.append(f"{prefix}.action: expected add, replace, or remove")
        tag = note.get("tag")
        if tag not in FAILURE_TAGS:
            errors.append(f"{prefix}.tag: expected one of the 16 failure tags")
        previous = note.get("previous")
        if previous is not None and previous not in FAILURE_TAGS:
            errors.append(f"{prefix}.previous: expected a failure tag or null")
        if not _is_nonempty_string(note.get("note")):
            errors.append(f"{prefix}.note: expected a non-empty string")
        if "by" in note and note["by"] is not None and not _is_nonempty_string(
            note["by"]
        ):
            errors.append(f"{prefix}.by: expected a non-empty string or null")
        if "at" in note:
            try:
                parse_utc_timestamp(note["at"])
            except ValueError as exc:
                errors.append(f"{prefix}.at: {exc}")


def trace_errors(record: Mapping[str, Any] | Any, *, strict: bool = False) -> list[str]:
    """Return every schema problem in ``record`` (an empty list means valid)."""

    if not isinstance(record, Mapping):
        return ["record: expected a JSON object"]

    errors: list[str] = []
    for field in REQUIRED_FIELDS:
        if field not in record:
            errors.append(f"{field}: required field is missing")

    if "schema_version" in record and record["schema_version"] != SCHEMA_VERSION:
        errors.append(
            f"schema_version: expected {SCHEMA_VERSION!r}, "
            f"got {record['schema_version']!r}"
        )

    for field in ("benchmark", "task_id", "model", "keel_target", "stop_reason"):
        _check_nonempty_string(record, field, errors)
    for field in _NULLABLE_STRINGS:
        _check_nullable_string(record, field, errors)

    if "binary_sha256" in record and record["binary_sha256"] is not None:
        sha = record["binary_sha256"]
        if not isinstance(sha, str) or not _SHA256_RE.match(sha):
            errors.append("binary_sha256: expected 64 lowercase hexadecimal characters")

    if "trial" in record:
        trial = record["trial"]
        if not _is_int(trial) or trial < 0:
            errors.append(f"trial: expected an integer >= 0, got {trial!r}")

    timestamps: dict[str, datetime] = {}
    for field in ("start_time", "end_time"):
        if field not in record:
            continue
        try:
            timestamps[field] = parse_utc_timestamp(record[field])
        except ValueError as exc:
            errors.append(f"{field}: {exc}")
    if len(timestamps) == 2 and timestamps["end_time"] < timestamps["start_time"]:
        errors.append("end_time: must be greater than or equal to start_time")

    for field in (
        "turns",
        "tool_calls",
        "input_tokens",
        "output_tokens",
        "reasoning_tokens",
        "patch_size",
    ):
        _check_nonnegative_int(record, field, errors)
    _check_nonnegative_number(record, "cost_usd", errors, nullable=True)
    _check_nonnegative_number(record, "wall_clock_s", errors)

    if "patch" in record and not isinstance(record["patch"], str):
        errors.append(f"patch: expected a string, got {type(record['patch']).__name__}")
    if (
        isinstance(record.get("patch"), str)
        and _is_int(record.get("patch_size"))
        and record["patch_size"] >= 0
    ):
        actual = len(record["patch"].encode("utf-8"))
        if actual != record["patch_size"]:
            errors.append(
                f"patch_size: expected {actual} (UTF-8 bytes of patch), "
                f"got {record['patch_size']}"
            )

    if "patch_apply_ok" in record:
        value = record["patch_apply_ok"]
        if value is not None and type(value) is not bool:
            errors.append(f"patch_apply_ok: expected true, false, or null, got {value!r}")

    if "verifier_result" in record and record["verifier_result"] not in VERIFIER_RESULTS:
        errors.append(
            f"verifier_result: expected one of {', '.join(VERIFIER_RESULTS)}, "
            f"got {record['verifier_result']!r}"
        )

    if "failure_tags" in record:
        _validate_failure_tags(record["failure_tags"], errors)

    if "harness_health" in record:
        _validate_harness_health(record["harness_health"], errors)
    if "failure_tag_notes" in record:
        _validate_failure_tag_notes(record["failure_tag_notes"], errors)

    if strict:
        allowed = set(REQUIRED_FIELDS) | set(OPTIONAL_FIELDS)
        for field in record:
            if field not in allowed:
                errors.append(f"{field}: unknown field in strict mode")

    return errors


def validate_trace(
    record: Mapping[str, Any] | Any,
    *,
    strict: bool = False,
    source: str | None = None,
) -> None:
    """Raise :class:`TraceValidationError` when ``record`` is malformed."""

    errors = trace_errors(record, strict=strict)
    if errors:
        raise TraceValidationError(errors, source=source)


def is_empty_patch(record: Mapping[str, Any]) -> bool:
    """An empty patch is whitespace-only text or a declared size of zero."""

    patch = record.get("patch")
    if not isinstance(patch, str) or not patch.strip():
        return True
    return record.get("patch_size") == 0


def dump_trace(record: Mapping[str, Any], *, pretty: bool = False) -> str:
    if pretty:
        return json.dumps(record, ensure_ascii=False, indent=2, sort_keys=False)
    return json.dumps(record, ensure_ascii=False, separators=(",", ":"))


def read_trace_file(path: str | Path, *, strict: bool = False) -> list[LoadedTrace]:
    """Read and validate one JSONL trace file.

    Blank lines are ignored.  The first malformed line stops the read and
    reports the physical line number.
    """

    trace_path = Path(path)
    try:
        text = trace_path.read_text(encoding="utf-8")
    except UnicodeDecodeError as exc:
        raise TraceValidationError(
            [f"file is not UTF-8: {exc}"], source=str(trace_path)
        ) from exc

    loaded: list[LoadedTrace] = []
    for line_number, line in enumerate(text.splitlines(), start=1):
        if not line.strip():
            continue
        source = f"{trace_path}:{line_number}"
        try:
            record = json.loads(line)
        except json.JSONDecodeError as exc:
            raise TraceValidationError(
                [f"invalid JSON: {exc.msg} (column {exc.colno})"], source=source
            ) from exc
        if not isinstance(record, dict):
            raise TraceValidationError(
                [
                    "expected a JSON object, "
                    f"got {type(record).__name__}"
                ],
                source=source,
            )
        validate_trace(record, strict=strict, source=source)
        loaded.append(LoadedTrace(record=record, path=trace_path, line=line_number))

    if not loaded:
        raise TraceValidationError(["no trace records found"], source=str(trace_path))
    return loaded


def read_trace_files(
    paths: Sequence[str | Path], *, strict: bool = False
) -> list[LoadedTrace]:
    loaded: list[LoadedTrace] = []
    for path in paths:
        loaded.extend(read_trace_file(path, strict=strict))
    return loaded


__all__ = [
    "FAILURE_TAGS",
    "FAILURE_TAG_DESCRIPTIONS",
    "HARNESS_HEALTH_KEYS",
    "LoadedTrace",
    "OPTIONAL_FIELDS",
    "REQUIRED_FIELDS",
    "SCHEMA_VERSION",
    "TraceValidationError",
    "VERIFIER_RESULTS",
    "dump_trace",
    "is_empty_patch",
    "parse_utc_timestamp",
    "read_trace_file",
    "read_trace_files",
    "trace_errors",
    "validate_trace",
]
