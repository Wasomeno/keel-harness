"""Tests for the failure-tag editing CLI and audit trail."""

from __future__ import annotations

import io
import json
import os
import shutil
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

from benchmarks.traces import schema, tag_failure

FIXTURES = Path(__file__).parent / "fixtures" / "traces"
ALL_TAGS = FIXTURES / "all_tags.jsonl"


class TagFailureTests(unittest.TestCase):
    def setUp(self) -> None:
        self._temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self._temporary.cleanup)
        self.tmp = Path(self._temporary.name)
        self.path = self.tmp / "run.jsonl"
        shutil.copy(ALL_TAGS, self.path)

    def records(self, path: Path | None = None) -> list[dict]:
        text = (path or self.path).read_text(encoding="utf-8")
        return [json.loads(line) for line in text.splitlines() if line.strip()]

    def run_cli(self, argv: list[str]) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            code = tag_failure.main(argv)
        return code, stdout.getvalue(), stderr.getvalue()

    def test_list_tags_plain_and_json(self) -> None:
        code, stdout, stderr = self.run_cli(["list"])
        self.assertEqual(code, 0, stderr)
        for tag in schema.FAILURE_TAGS:
            self.assertIn(tag, stdout)

        code, stdout, stderr = self.run_cli(["list", "--json"])
        self.assertEqual(code, 0, stderr)
        tags = json.loads(stdout)
        self.assertEqual(len(tags), 16)
        self.assertEqual({item["tag"] for item in tags}, set(schema.FAILURE_TAGS))

    def test_add_by_task_and_trial_records_a_note(self) -> None:
        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(self.path),
                "--task-id",
                "fixture/localization",
                "--trial",
                "0",
                "--tag",
                "timeout",
                "--note",
                "hit the wall clock while searching",
                "--by",
                "tester",
            ]
        )
        self.assertEqual(code, 0, stderr)
        self.assertIn("updated", stdout)
        record = self.records()[0]
        self.assertEqual(record["failure_tags"], ["localization", "timeout"])
        note = record["failure_tag_notes"][-1]
        self.assertEqual(note["action"], "add")
        self.assertEqual(note["tag"], "timeout")
        self.assertEqual(note["note"], "hit the wall clock while searching")
        self.assertEqual(note["by"], "tester")
        self.assertTrue(note["at"].endswith("Z"))
        self.assertEqual(schema.trace_errors(record), [])

    def test_add_by_line_preserves_every_other_line(self) -> None:
        original = self.path.read_text(encoding="utf-8").splitlines()
        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(self.path),
                "--line",
                "2",
                "--tag",
                "timeout",
                "--note",
                "slow environment",
            ]
        )
        self.assertEqual(code, 0, stderr)
        updated = self.path.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(updated), len(original))
        self.assertEqual(updated[0], original[0])
        self.assertEqual(updated[2:], original[2:])
        record = self.records()[1]
        self.assertIn("timeout", record["failure_tags"])

    def test_add_duplicate_fails_without_writing(self) -> None:
        before = self.path.read_bytes()
        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(self.path),
                "--task-id",
                "fixture/localization",
                "--trial",
                "0",
                "--tag",
                "localization",
                "--note",
                "duplicate",
            ]
        )
        self.assertEqual(code, 2)
        self.assertEqual(stdout, "")
        self.assertIn("already present", stderr)
        self.assertEqual(self.path.read_bytes(), before)

    def test_replace_records_the_previous_tag(self) -> None:
        code, stdout, stderr = self.run_cli(
            [
                "replace",
                str(self.path),
                "--task-id",
                "fixture/localization",
                "--trial",
                "0",
                "--old",
                "localization",
                "--new",
                "wrong-file",
                "--note",
                "actually edited the wrong module",
            ]
        )
        self.assertEqual(code, 0, stderr)
        record = self.records()[0]
        self.assertEqual(record["failure_tags"], ["wrong-file"])
        note = record["failure_tag_notes"][-1]
        self.assertEqual(note["action"], "replace")
        self.assertEqual(note["previous"], "localization")
        self.assertEqual(note["tag"], "wrong-file")

    def test_replace_missing_old_tag_fails(self) -> None:
        before = self.path.read_bytes()
        code, stdout, stderr = self.run_cli(
            [
                "replace",
                str(self.path),
                "--line",
                "1",
                "--old",
                "syntax",
                "--new",
                "wrong-file",
                "--note",
                "nope",
            ]
        )
        self.assertEqual(code, 2)
        self.assertIn("not present", stderr)
        self.assertEqual(self.path.read_bytes(), before)

    def test_replace_with_the_same_tag_fails(self) -> None:
        code, stdout, stderr = self.run_cli(
            [
                "replace",
                str(self.path),
                "--line",
                "1",
                "--old",
                "localization",
                "--new",
                "localization",
                "--note",
                "no change",
            ]
        )
        self.assertEqual(code, 2)
        self.assertIn("must be different", stderr)

    def test_remove_records_the_removed_tag(self) -> None:
        code, stdout, stderr = self.run_cli(
            [
                "remove",
                str(self.path),
                "--task-id",
                "fixture/localization",
                "--trial",
                "0",
                "--tag",
                "localization",
                "--note",
                "the trace was tagged too aggressively",
            ]
        )
        self.assertEqual(code, 0, stderr)
        record = self.records()[0]
        self.assertEqual(record["failure_tags"], [])
        note = record["failure_tag_notes"][-1]
        self.assertEqual(note["action"], "remove")
        self.assertEqual(note["previous"], "localization")

    def test_remove_missing_tag_fails(self) -> None:
        code, stdout, stderr = self.run_cli(
            [
                "remove",
                str(self.path),
                "--line",
                "1",
                "--tag",
                "timeout",
                "--note",
                "not there",
            ]
        )
        self.assertEqual(code, 2)
        self.assertIn("not present", stderr)

    def test_unknown_tag_fails(self) -> None:
        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(self.path),
                "--line",
                "1",
                "--tag",
                "flaky",
                "--note",
                "not in the taxonomy",
            ]
        )
        self.assertEqual(code, 2)
        self.assertIn("unknown tag", stderr)

    def test_blank_note_is_rejected(self) -> None:
        with self.assertRaises(tag_failure.TagError):
            tag_failure.mutate(
                self.path,
                "add",
                tag="timeout",
                note="   ",
                line=1,
            )

    def test_ambiguous_task_id_requires_trial(self) -> None:
        base = self.records()[0]
        first = dict(base)
        first["task_id"] = "fixture/duplicate"
        first["trial"] = 0
        second = json.loads(json.dumps(first))
        second["trial"] = 1
        ambiguous = self.tmp / "ambiguous.jsonl"
        ambiguous.write_text(
            json.dumps(first, separators=(",", ":"))
            + "\n"
            + json.dumps(second, separators=(",", ":"))
            + "\n",
            encoding="utf-8",
        )
        before = ambiguous.read_bytes()
        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(ambiguous),
                "--task-id",
                "fixture/duplicate",
                "--tag",
                "timeout",
                "--note",
                "needs a trial",
            ]
        )
        self.assertEqual(code, 2)
        self.assertIn("add --trial", stderr)
        self.assertEqual(ambiguous.read_bytes(), before)

        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(ambiguous),
                "--task-id",
                "fixture/duplicate",
                "--trial",
                "1",
                "--tag",
                "timeout",
                "--note",
                "chosen trial",
            ]
        )
        self.assertEqual(code, 0, stderr)
        updated = self.records(ambiguous)
        self.assertNotIn("timeout", updated[0]["failure_tags"])
        self.assertIn("timeout", updated[1]["failure_tags"])

    def test_line_out_of_range(self) -> None:
        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(self.path),
                "--line",
                "999",
                "--tag",
                "timeout",
                "--note",
                "outside",
            ]
        )
        self.assertEqual(code, 2)
        self.assertIn("outside the file", stderr)

    def test_trial_requires_task_id(self) -> None:
        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(self.path),
                "--line",
                "1",
                "--trial",
                "0",
                "--tag",
                "timeout",
                "--note",
                "missing task",
            ]
        )
        self.assertEqual(code, 2)
        self.assertIn("--trial requires --task-id", stderr)

    def test_validate_all_tags_and_malformed_files(self) -> None:
        code, stdout, stderr = self.run_cli(["validate", str(ALL_TAGS)])
        self.assertEqual(code, 0, stderr)
        self.assertIn("OK", stdout)
        self.assertIn("16 traces", stdout)

        code, stdout, stderr = self.run_cli(
            ["validate", str(FIXTURES / "malformed" / "missing_field.jsonl")]
        )
        self.assertEqual(code, 1)
        self.assertIn("INVALID", stderr)
        self.assertIn("verifier_result", stderr)

    def test_validate_json_output(self) -> None:
        code, stdout, stderr = self.run_cli(
            [
                "validate",
                "--json",
                str(ALL_TAGS),
                str(FIXTURES / "malformed" / "missing_field.jsonl"),
            ]
        )
        self.assertEqual(code, 1)
        results = json.loads(stdout)
        self.assertEqual(len(results), 2)
        self.assertTrue(results[0]["ok"])
        self.assertEqual(results[0]["traces"], 16)
        self.assertFalse(results[1]["ok"])
        self.assertTrue(any("verifier_result" in error for error in results[1]["errors"]))

    def test_refuses_to_edit_an_invalid_file(self) -> None:
        path = self.tmp / "invalid.jsonl"
        shutil.copy(FIXTURES / "malformed" / "missing_field.jsonl", path)
        before = path.read_bytes()
        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(path),
                "--line",
                "1",
                "--tag",
                "timeout",
                "--note",
                "should not write",
            ]
        )
        self.assertEqual(code, 2)
        self.assertIn("verifier_result", stderr)
        self.assertEqual(path.read_bytes(), before)

    def test_preserves_the_original_trailing_newline(self) -> None:
        record = self.records()[0]
        no_newline = self.tmp / "no-newline.jsonl"
        no_newline.write_text(json.dumps(record, separators=(",", ":")), encoding="utf-8")
        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(no_newline),
                "--line",
                "1",
                "--tag",
                "timeout",
                "--note",
                "keep the file shape",
            ]
        )
        self.assertEqual(code, 0, stderr)
        self.assertFalse(no_newline.read_text(encoding="utf-8").endswith("\n"))

        with_newline = self.tmp / "with-newline.jsonl"
        with_newline.write_text(
            json.dumps(record, separators=(",", ":")) + "\n", encoding="utf-8"
        )
        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(with_newline),
                "--line",
                "1",
                "--tag",
                "timeout",
                "--note",
                "keep the newline",
            ]
        )
        self.assertEqual(code, 0, stderr)
        self.assertTrue(with_newline.read_text(encoding="utf-8").endswith("\n"))

    def test_preserves_crlf_line_endings(self) -> None:
        record = self.records()[0]
        second = json.loads(json.dumps(record))
        second["task_id"] = "fixture/crlf"
        second["trial"] = 1
        first_line = json.dumps(record, separators=(",", ":")) + "\r\n"
        second_line = json.dumps(second, separators=(",", ":")) + "\r\n"
        path = self.tmp / "crlf.jsonl"
        path.write_bytes((first_line + second_line).encode("utf-8"))

        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(path),
                "--line",
                "2",
                "--tag",
                "timeout",
                "--note",
                "keep CRLF",
            ]
        )
        self.assertEqual(code, 0, stderr)
        data = path.read_bytes()
        self.assertTrue(data.startswith(first_line.encode("utf-8")))
        self.assertIn(b"\r\n", data)
        self.assertEqual(data.count(b"\r\n"), 2)

    @unittest.skipIf(os.name == "nt", "symlink creation needs extra privileges on Windows")
    def test_mutation_preserves_a_symlink_and_updates_the_target(self) -> None:
        target = self.tmp / "target.jsonl"
        shutil.copy(ALL_TAGS, target)
        link = self.tmp / "link.jsonl"
        try:
            link.symlink_to(target)
        except (OSError, NotImplementedError) as exc:
            self.skipTest(f"symlinks unavailable: {exc}")

        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(link),
                "--line",
                "1",
                "--tag",
                "timeout",
                "--note",
                "edit through the link",
            ]
        )
        self.assertEqual(code, 0, stderr)
        self.assertTrue(link.is_symlink())
        self.assertEqual(link.resolve(), target.resolve())
        self.assertIn("timeout", self.records(target)[0]["failure_tags"])

    def test_append_during_edit_aborts_without_losing_records(self) -> None:
        before = self.path.read_bytes()
        extra = self.records()[0]
        extra["task_id"] = "fixture/appended"
        extra["trial"] = 99
        original = tag_failure._atomic_write

        def append_then_write(path, text, snapshot):
            with path.open("a", encoding="utf-8") as handle:
                handle.write(json.dumps(extra, separators=(",", ":")) + "\n")
            return original(path, text, snapshot)

        with mock.patch.object(tag_failure, "_atomic_write", append_then_write):
            with self.assertRaises(tag_failure.TagError) as raised:
                tag_failure.mutate(
                    self.path,
                    "add",
                    tag="timeout",
                    note="racing writer",
                    line=1,
                )
        self.assertIn("changed while editing", str(raised.exception))
        data = self.path.read_bytes()
        self.assertTrue(data.startswith(before))
        self.assertIn(b"fixture/appended", data)
        self.assertNotIn("timeout", self.records()[0]["failure_tags"])

    def test_append_during_copymode_aborts_without_losing_records(self) -> None:
        before = self.path.read_bytes()
        extra = self.records()[0]
        extra["task_id"] = "fixture/copymode-appended"
        extra["trial"] = 98
        original = shutil.copymode

        def append_then_copymode(src, dst, *args, **kwargs):
            with Path(src).open("a", encoding="utf-8") as handle:
                handle.write(json.dumps(extra, separators=(",", ":")) + "\n")
            return original(src, dst, *args, **kwargs)

        with mock.patch.object(
            tag_failure.shutil, "copymode", append_then_copymode
        ):
            with self.assertRaises(tag_failure.TagError) as raised:
                tag_failure.mutate(
                    self.path,
                    "add",
                    tag="timeout",
                    note="racing copymode",
                    line=1,
                )
        self.assertIn("changed while editing", str(raised.exception))
        data = self.path.read_bytes()
        self.assertTrue(data.startswith(before))
        self.assertIn(b"fixture/copymode-appended", data)
        self.assertNotIn("timeout", self.records()[0]["failure_tags"])

    def test_lone_surrogate_note_is_rejected_without_writing(self) -> None:
        before = self.path.read_bytes()
        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(self.path),
                "--line",
                "1",
                "--tag",
                "timeout",
                "--note",
                json.loads('"\\ud800"'),
            ]
        )
        self.assertEqual(code, 2)
        self.assertIn("not valid UTF-8 text", stderr)
        self.assertEqual(self.path.read_bytes(), before)

    def test_json_mutation_output(self) -> None:
        code, stdout, stderr = self.run_cli(
            [
                "add",
                str(self.path),
                "--line",
                "1",
                "--tag",
                "timeout",
                "--note",
                "json output",
                "--json",
            ]
        )
        self.assertEqual(code, 0, stderr)
        result = json.loads(stdout)
        self.assertEqual(result["action"], "add")
        self.assertEqual(result["tag"], "timeout")
        self.assertEqual(result["line"], 1)
        self.assertIn("timeout", result["failure_tags"])


if __name__ == "__main__":
    unittest.main()
