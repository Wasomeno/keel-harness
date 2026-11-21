"""Tests for the canonical trace schema, version 1.0."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from benchmarks.traces import schema

FIXTURES = Path(__file__).parent / "fixtures" / "traces"
PATCH = (
    "diff --git a/solution.py b/solution.py\n"
    "--- a/solution.py\n"
    "+++ b/solution.py\n"
    "@@ -1 +1 @@\n"
    "-old\n"
    "+new\n"
)


def valid_trace(**overrides: object) -> dict:
    record = {
        "schema_version": "1.0",
        "benchmark": "terminal-bench-2.0",
        "task_id": "terminal-bench/fix-git",
        "trial": 0,
        "model": "neuralwatt/deepseek-v4.1-flash",
        "model_version": "deepseek-v4.1-flash-1017",
        "keel_target": "neuralwatt/deepseek-v4.1-flash",
        "agent_version": "0.12.0",
        "binary_sha256": "a" * 64,
        "start_time": "2026-11-21T10:00:00Z",
        "end_time": "2026-11-21T10:00:30Z",
        "turns": 4,
        "tool_calls": 6,
        "input_tokens": 1000,
        "output_tokens": 200,
        "reasoning_tokens": 50,
        "cost_usd": 0.01,
        "stop_reason": "completed",
        "wall_clock_s": 30.0,
        "patch": PATCH,
        "patch_size": len(PATCH.encode("utf-8")),
        "patch_apply_ok": True,
        "verifier_result": "pass",
        "failure_tags": [],
        "session_log_path": "/tmp/keel-session.jsonl",
        "error": None,
    }
    record.update(overrides)
    return record


class SchemaTests(unittest.TestCase):
    def test_valid_trace_has_no_errors(self) -> None:
        self.assertEqual(schema.trace_errors(valid_trace()), [])
        schema.validate_trace(valid_trace())

    def test_required_field_missing(self) -> None:
        record = valid_trace()
        del record["verifier_result"]
        errors = schema.trace_errors(record)
        self.assertTrue(any("verifier_result" in error for error in errors))
        with self.assertRaises(schema.TraceValidationError) as raised:
            schema.validate_trace(record)
        self.assertIn("verifier_result: required field is missing", str(raised.exception))

    def test_missing_nullable_fields_are_still_required(self) -> None:
        record = valid_trace()
        del record["model_version"]
        self.assertTrue(
            any("model_version" in error for error in schema.trace_errors(record))
        )

    def test_bad_types(self) -> None:
        cases = {
            "turns": "4",
            "tool_calls": -1,
            "input_tokens": 1.5,
            "patch_size": -2,
            "wall_clock_s": "30",
            "cost_usd": True,
            "patch_apply_ok": "yes",
            "patch": 3,
        }
        for field, value in cases.items():
            with self.subTest(field=field):
                record = valid_trace(**{field: value})
                self.assertTrue(
                    any(error.startswith(f"{field}:") for error in schema.trace_errors(record)),
                    schema.trace_errors(record),
                )

    def test_cost_can_be_null(self) -> None:
        self.assertEqual(schema.trace_errors(valid_trace(cost_usd=None)), [])

    def test_timestamp_rules(self) -> None:
        invalid = [
            "2026-11-21T10:00:00",  # naive
            "2026-11-21T10:00:00+01:00",  # not UTC
            "not-a-time",
            "",
        ]
        for value in invalid:
            with self.subTest(value=value):
                record = valid_trace(start_time=value)
                self.assertTrue(
                    any("start_time" in error for error in schema.trace_errors(record))
                )
        self.assertEqual(
            schema.trace_errors(
                valid_trace(
                    start_time="2026-11-21T10:00:00+00:00",
                    end_time="2026-11-21T10:00:01Z",
                )
            ),
            [],
        )

    def test_end_before_start(self) -> None:
        record = valid_trace(
            start_time="2026-11-21T10:00:30Z", end_time="2026-11-21T10:00:00Z"
        )
        self.assertIn(
            "end_time: must be greater than or equal to start_time",
            schema.trace_errors(record),
        )

    def test_verifier_result_enum(self) -> None:
        self.assertEqual(schema.trace_errors(valid_trace(verifier_result="not_run")), [])
        for value in ("passed", "", None, "PASS"):
            with self.subTest(value=value):
                record = valid_trace(verifier_result=value)
                self.assertTrue(
                    any("verifier_result" in error for error in schema.trace_errors(record))
                )

    def test_failure_tags_are_exact_and_unique(self) -> None:
        self.assertEqual(schema.trace_errors(valid_trace(failure_tags=[])), [])
        self.assertEqual(
            schema.trace_errors(
                valid_trace(failure_tags=["localization", "timeout"])
            ),
            [],
        )
        unknown = schema.trace_errors(valid_trace(failure_tags=["flaky"]))
        self.assertTrue(any("unknown tag 'flaky'" in error for error in unknown))
        duplicate = schema.trace_errors(
            valid_trace(failure_tags=["timeout", "timeout"])
        )
        self.assertTrue(any("duplicate tag" in error for error in duplicate))
        not_a_list = schema.trace_errors(valid_trace(failure_tags="timeout"))
        self.assertTrue(any("expected a list" in error for error in not_a_list))
        self.assertEqual(len(schema.FAILURE_TAGS), 16)
        self.assertEqual(len(set(schema.FAILURE_TAGS)), 16)

    def test_binary_sha256(self) -> None:
        self.assertEqual(schema.trace_errors(valid_trace(binary_sha256=None)), [])
        for value in ("abc", "A" * 64, "a" * 63, "a" * 65, 123):
            with self.subTest(value=value):
                record = valid_trace(binary_sha256=value)
                self.assertTrue(
                    any("binary_sha256" in error for error in schema.trace_errors(record))
                )

    def test_trial_must_be_nonnegative_integer(self) -> None:
        self.assertEqual(schema.trace_errors(valid_trace(trial=0)), [])
        self.assertEqual(schema.trace_errors(valid_trace(trial=12)), [])
        for value in (-1, 1.0, "0", True):
            with self.subTest(value=value):
                record = valid_trace(trial=value)
                self.assertTrue(
                    any(error.startswith("trial:") for error in schema.trace_errors(record))
                )

    def test_patch_size_must_match_utf8_bytes(self) -> None:
        record = valid_trace(patch="", patch_size=0)
        self.assertEqual(schema.trace_errors(record), [])
        bad = valid_trace(patch="abc", patch_size=2)
        self.assertTrue(
            any("patch_size" in error and "expected 3" in error for error in schema.trace_errors(bad))
        )
        unicode_patch = "diff --git a/é b/é\n"
        record = valid_trace(
            patch=unicode_patch, patch_size=len(unicode_patch.encode("utf-8"))
        )
        self.assertEqual(schema.trace_errors(record), [])

    def test_harness_health_shape(self) -> None:
        valid = valid_trace(
            harness_health={
                "tool_errors": 2,
                "context_overflows": 0,
                "provider_retries": 3,
                "loops": 1,
                "nudges": 4,
                "test_command_failures": 0,
            }
        )
        self.assertEqual(schema.trace_errors(valid), [])
        for value in ({"tool_errors": -1}, {"tool_errors": True}, {"mystery": 1}):
            with self.subTest(value=value):
                record = valid_trace(harness_health=value)
                self.assertTrue(
                    any("harness_health" in error for error in schema.trace_errors(record))
                )
        self.assertEqual(schema.trace_errors(valid_trace(harness_health=None)), [])

    def test_failure_tag_notes_shape(self) -> None:
        record = valid_trace(
            failure_tags=["timeout"],
            failure_tag_notes=[
                {
                    "action": "add",
                    "tag": "timeout",
                    "note": "hit the wall clock",
                    "at": "2026-11-21T11:00:00Z",
                    "by": "tester",
                }
            ],
        )
        self.assertEqual(schema.trace_errors(record), [])
        bad = valid_trace(
            failure_tag_notes=[
                {"action": "add", "tag": "flaky", "note": "", "at": "yesterday"}
            ]
        )
        errors = schema.trace_errors(bad)
        self.assertTrue(any("failure_tag_notes[0].tag" in error for error in errors))
        self.assertTrue(any("failure_tag_notes[0].note" in error for error in errors))
        self.assertTrue(any("failure_tag_notes[0].at" in error for error in errors))

    def test_failure_tag_notes_require_at_and_previous_semantics(self) -> None:
        base = {
            "action": "add",
            "tag": "timeout",
            "note": "hit the wall clock",
            "at": "2026-11-21T11:00:00Z",
        }
        missing_at = dict(base)
        del missing_at["at"]
        cases = {
            "missing at": (missing_at, "failure_tag_notes[0].at: required"),
            "add with previous": (
                {**base, "previous": "timeout"},
                "must be absent",
            ),
            "replace without previous": (
                {**base, "action": "replace"},
                "required for a replace",
            ),
            "replace with same previous": (
                {**base, "action": "replace", "previous": "timeout"},
                "must differ",
            ),
            "remove with other previous": (
                {**base, "action": "remove", "previous": "loop"},
                "must equal tag",
            ),
        }
        for label, (note, expected) in cases.items():
            with self.subTest(label=label):
                errors = schema.trace_errors(
                    valid_trace(failure_tag_notes=[note])
                )
                self.assertTrue(
                    any(expected in error for error in errors),
                    errors,
                )

    def test_failure_tag_notes_only_check_unknown_keys_in_strict_mode(self) -> None:
        note = {
            "action": "add",
            "tag": "timeout",
            "note": "hit the wall clock",
            "at": "2026-11-21T11:00:00Z",
            "extra": "future field",
        }
        record = valid_trace(failure_tag_notes=[note])
        self.assertEqual(schema.trace_errors(record), [])
        self.assertTrue(
            any(
                "failure_tag_notes[0].extra: unknown field" in error
                for error in schema.trace_errors(record, strict=True)
            )
        )

    def test_lone_surrogate_is_a_normal_validation_error(self) -> None:
        record = valid_trace(
            patch=json.loads('"\\ud800"'),
            patch_size=0,
        )
        errors = schema.trace_errors(record)
        self.assertIn("patch: not valid UTF-8 text", errors)
        with self.assertRaises(schema.TraceValidationError) as raised:
            schema.validate_trace(record)
        self.assertIn("patch: not valid UTF-8 text", str(raised.exception))
        self.assertNotIn("UnicodeEncodeError", str(raised.exception))

    def test_dump_trace_rejects_lone_surrogate(self) -> None:
        record = valid_trace(
            patch=json.loads('"\\ud800"'),
            patch_size=0,
        )
        with self.assertRaises(schema.TraceValidationError) as raised:
            schema.dump_trace(record)
        self.assertIn("patch: not valid UTF-8 text", str(raised.exception))

    def test_read_trace_file_rejects_lone_surrogate_escape(self) -> None:
        record = valid_trace(
            patch=json.loads('"\\ud800"'),
            patch_size=0,
        )
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "surrogate.jsonl"
            # ensure_ascii=True writes the lone surrogate as a JSON escape,
            # which json.loads accepts and then validates normally.
            path.write_text(json.dumps(record) + "\n", encoding="utf-8")
            with self.assertRaises(schema.TraceValidationError) as raised:
                schema.read_trace_file(path)
        self.assertIn("patch: not valid UTF-8 text", str(raised.exception))
        self.assertIn("surrogate.jsonl:1", str(raised.exception))

    def test_any_lone_surrogate_string_is_reported(self) -> None:
        record = valid_trace(session_log_path=json.loads('"\\ud800"'))
        self.assertIn(
            "session_log_path: not valid UTF-8 text",
            schema.trace_errors(record),
        )

    def test_unknown_fields_only_fail_in_strict_mode(self) -> None:
        record = valid_trace(extra="future field")
        self.assertEqual(schema.trace_errors(record), [])
        self.assertTrue(
            any("extra: unknown field" in error for error in schema.trace_errors(record, strict=True))
        )

    def test_read_trace_file_skips_blank_lines(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "run.jsonl"
            path.write_text(
                "\n" + json.dumps(valid_trace()) + "\n\n" + json.dumps(valid_trace(trial=1)) + "\n",
                encoding="utf-8",
            )
            loaded = schema.read_trace_file(path)
            self.assertEqual(len(loaded), 2)
            self.assertEqual(loaded[0].line, 2)
            self.assertEqual(loaded[1].line, 4)
            self.assertTrue(loaded[0].source.endswith("run.jsonl:2"))

    def test_read_trace_file_rejects_bad_json_and_non_objects(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "bad.jsonl"
            path.write_text("{not json}\n", encoding="utf-8")
            with self.assertRaises(schema.TraceValidationError) as raised:
                schema.read_trace_file(path)
            self.assertIn("invalid JSON", str(raised.exception))
            self.assertIn(":1", str(raised.exception))

            path.write_text("[1, 2, 3]\n", encoding="utf-8")
            with self.assertRaises(schema.TraceValidationError) as raised:
                schema.read_trace_file(path)
            self.assertIn("expected a JSON object", str(raised.exception))

            path.write_text("\n\n", encoding="utf-8")
            with self.assertRaises(schema.TraceValidationError) as raised:
                schema.read_trace_file(path)
            self.assertIn("no trace records", str(raised.exception))

    def test_read_trace_file_reports_the_malformed_line(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "run.jsonl"
            path.write_text(
                json.dumps(valid_trace())
                + "\n"
                + json.dumps(valid_trace(trial=1, verifier_result="maybe"))
                + "\n",
                encoding="utf-8",
            )
            with self.assertRaises(schema.TraceValidationError) as raised:
                schema.read_trace_file(path)
            self.assertIn(":2", str(raised.exception))
            self.assertIn("verifier_result", str(raised.exception))

    def test_is_empty_patch(self) -> None:
        self.assertTrue(schema.is_empty_patch(valid_trace(patch="", patch_size=0)))
        self.assertTrue(
            schema.is_empty_patch(valid_trace(patch=" \n", patch_size=2))
        )
        self.assertFalse(schema.is_empty_patch(valid_trace()))

    def test_dump_trace_round_trip(self) -> None:
        record = valid_trace(failure_tags=["timeout"])
        compact = schema.dump_trace(record)
        self.assertEqual(json.loads(compact), record)
        self.assertNotIn("\n", compact)
        self.assertIn("\n", schema.dump_trace(record, pretty=True))

    def test_all_tag_fixtures_validate(self) -> None:
        loaded = schema.read_trace_file(FIXTURES / "all_tags.jsonl")
        self.assertEqual(len(loaded), 16)
        seen = []
        for item in loaded:
            self.assertEqual(item.record["verifier_result"], "fail")
            self.assertEqual(len(item.record["failure_tags"]), 1)
            seen.append(item.record["failure_tags"][0])
        self.assertEqual(sorted(seen), sorted(schema.FAILURE_TAGS))

    def test_malformed_fixtures_are_rejected(self) -> None:
        expected = {
            "missing_field.jsonl": "verifier_result",
            "bad_timestamp.jsonl": "start_time",
            "unknown_tag.jsonl": "unknown tag",
        }
        for name, needle in expected.items():
            with self.subTest(name=name):
                with self.assertRaises(schema.TraceValidationError) as raised:
                    schema.read_trace_file(FIXTURES / "malformed" / name)
                self.assertIn(needle, str(raised.exception))


if __name__ == "__main__":
    unittest.main()
