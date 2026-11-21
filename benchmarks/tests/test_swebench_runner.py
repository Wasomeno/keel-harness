"""Unit tests for benchmarks/swebench/run_keel.py.

The tests use temporary local git repositories and fake command runners; they
never need Docker, a SWE-bench image, a Linux keel binary or the swebench
package.
"""

from __future__ import annotations

import contextlib
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from datetime import datetime, timedelta
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "benchmarks" / "swebench"))

import run_keel as runner  # noqa: E402


def _host_path(path: str) -> str:
    """Map the container's /tmp index path when tests run on Windows."""
    if os.name == "nt" and path.startswith("/tmp/"):
        return str(Path(tempfile.gettempdir()) / Path(path).name)
    return path


def _host_env(env):
    if env is None:
        return None
    values = dict(env)
    if "GIT_INDEX_FILE" in values:
        values["GIT_INDEX_FILE"] = _host_path(values["GIT_INDEX_FILE"])
    return {**os.environ, **values}


def local_exec(argv, *, stdin=None, timeout=None, capture_bytes=False, env=None):
    if os.name == "nt" and argv[:2] == ["rm", "-f"]:
        try:
            Path(_host_path(argv[-1])).unlink()
        except FileNotFoundError:
            pass
        return runner.CommandResult(tuple(argv), 0)
    command_env = _host_env(env)
    if capture_bytes:
        completed = subprocess.run(
            list(argv),
            input=stdin.encode("utf-8") if stdin is not None else None,
            capture_output=True,
            timeout=timeout,
            env=command_env,
        )
        return runner.CommandResult(
            argv=tuple(argv),
            returncode=completed.returncode,
            stdout=completed.stdout.decode("utf-8", errors="replace"),
            stderr=completed.stderr.decode("utf-8", errors="replace"),
            stdout_bytes=completed.stdout,
        )
    completed = subprocess.run(
        list(argv),
        input=stdin,
        capture_output=True,
        text=True,
        timeout=timeout,
        env=command_env,
    )
    return runner.CommandResult(
        argv=tuple(argv),
        returncode=completed.returncode,
        stdout=completed.stdout or "",
        stderr=completed.stderr or "",
    )


def real_subprocess_exec(argv, **kwargs):
    if os.name == "nt" and argv[:2] == ["rm", "-f"]:
        try:
            Path(_host_path(argv[-1])).unlink()
        except FileNotFoundError:
            pass
        return runner.CommandResult(tuple(argv), 0)
    kwargs["env"] = _host_env(kwargs.get("env"))
    return runner.SubprocessRunner().run(argv, **kwargs)


class GitRepo:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.root.mkdir(parents=True, exist_ok=True)
        self.git("init", "-q")
        self.git("config", "user.email", "test@example.com")
        self.git("config", "user.name", "Test")
        self.git("config", "commit.gpgsign", "false")

    def git(self, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            ["git", *args], cwd=self.root, text=True, capture_output=True
        )
        if check and result.returncode != 0:
            raise AssertionError(
                f"git {' '.join(args)} failed: {result.stderr or result.stdout}"
            )
        return result

    def write(self, path: str, text: str) -> None:
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")

    def commit(self, message: str = "base") -> str:
        self.git("add", "-A")
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD").stdout.strip()


class FakeRunner:
    """Records commands and delegates to a handler."""

    def __init__(self, handler=None) -> None:
        self.calls: list[tuple[list[str], dict]] = []
        self.handler = handler or (lambda argv, kwargs: runner.CommandResult(tuple(argv), 0))

    def run(self, argv, **kwargs):
        argv = list(argv)
        self.calls.append((argv, kwargs))
        return self.handler(argv, kwargs)


def make_instance(**overrides):
    values = {
        "instance_id": "django__django-11099",
        "base_commit": "abc1234",
        "problem_statement": "Fix the bug.",
    }
    values.update(overrides)
    return runner.Instance(**values)


def make_config(**overrides):
    values = {
        "keel_binary": Path("/tmp/keel"),
        "keel_target": "neuralwatt/deepseek-v4.1-flash",
        "model_label": "keel-neuralwatt-deepseek-v4.1-flash",
        "max_turns": 75,
        "timeout": 60,
        "output_dir": Path("/tmp/runs"),
        "run_id": "test-run",
        "image_template": runner.DEFAULT_IMAGE_TEMPLATE,
        "patch_cap_bytes": runner.DEFAULT_PATCH_CAP,
        "platform": "linux/amd64",
        "keep_container": False,
    }
    values.update(overrides)
    return runner.Config(**values)


def copy_fake_session(argv, *, stop="end_turn", error=None):
    """Fake ``docker cp <container>:/tmp/keel-swebench-data/. <dir>``."""
    destination = Path(argv[3])
    session = destination / "sessions" / "1.jsonl"
    session.parent.mkdir(parents=True, exist_ok=True)
    message = {
        "role": "assistant",
        "stop": stop,
        "usage": {"input_tokens": 7, "output_tokens": 3},
        "parts": [{"content": {"type": "tool_call", "name": "read"}}],
    }
    if error is not None:
        message["error"] = error
    session.write_text(
        json.dumps({"type": "meta", "model": "deepseek-v4.1-flash"})
        + "\n"
        + json.dumps({"type": "message", "message": message})
        + "\n"
    )
    return runner.CommandResult(tuple(argv), 0)


TEST_NETWORK = runner.NetworkPlan(
    policy="allowlist",
    allowed_hosts=("api.neuralwatt.com",),
    network_name="keel-test-internal",
    relay_name="keel-test-relay",
    relay_address="172.30.0.2",
    base_url="http://keel-test-relay:8080/v1",
)


CANONICAL_TRACE_FIELDS = (
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

CANONICAL_FAILURE_TAGS = {
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
}

TRACE_EXTENSION_FIELDS = (
    "patch_path",
    "empty_patch",
    "tests_modified",
    "modified_test_files",
    "image",
    "agent_exit_code",
    "agent_attempts",
    "retry_reason",
    "whitespace_ok",
    "whitespace_errors",
    "test_command_hint",
    "test_command_source",
    "test_runs",
    "tests_run",
    "test_last_status",
    "failing_tests",
    "test_excerpt",
    "verify_prompts",
    "network_policy",
    "allowed_hosts",
    "network_contaminated",
)


def assert_canonical_trace(testcase: unittest.TestCase, record: dict) -> None:
    testcase.assertEqual(set(CANONICAL_TRACE_FIELDS) - set(record), set())
    testcase.assertEqual(
        set(record),
        set(CANONICAL_TRACE_FIELDS) | set(TRACE_EXTENSION_FIELDS),
        "traces must carry the canonical fields and exactly the twenty-one extensions",
    )
    testcase.assertEqual(record["schema_version"], "1.0")
    testcase.assertIs(type(record["agent_attempts"]), int)
    testcase.assertGreaterEqual(record["agent_attempts"], 0)
    testcase.assertTrue(
        record["retry_reason"] is None
        or record["retry_reason"] in {"empty_patch", "whitespace"}
    )
    testcase.assertIs(type(record["whitespace_ok"]), bool)
    testcase.assertIsInstance(record["whitespace_errors"], list)
    testcase.assertTrue(
        all(isinstance(line, str) for line in record["whitespace_errors"])
    )
    for field in ("test_command_hint", "test_command_source", "test_excerpt"):
        value = record[field]
        testcase.assertTrue(value is None or (isinstance(value, str) and value))
    testcase.assertIs(type(record["test_runs"]), int)
    testcase.assertGreaterEqual(record["test_runs"], 0)
    testcase.assertIs(type(record["tests_run"]), bool)
    testcase.assertEqual(record["tests_run"], record["test_runs"] > 0)
    testcase.assertIn(record["test_last_status"], {None, "passed", "failed"})
    testcase.assertIsInstance(record["failing_tests"], list)
    testcase.assertTrue(
        all(isinstance(name, str) and name for name in record["failing_tests"])
    )
    testcase.assertIs(type(record["verify_prompts"]), int)
    testcase.assertGreaterEqual(record["verify_prompts"], 0)
    testcase.assertIn(
        record["network_policy"], {"allowlist", "open", "not_applicable"}
    )
    testcase.assertIsInstance(record["allowed_hosts"], list)
    testcase.assertTrue(
        all(isinstance(host, str) and host for host in record["allowed_hosts"])
    )
    testcase.assertIs(type(record["network_contaminated"]), bool)
    if record["network_policy"] == "open":
        testcase.assertTrue(record["network_contaminated"])
    for field in ("benchmark", "task_id", "model", "keel_target", "stop_reason"):
        testcase.assertIsInstance(record[field], str)
        testcase.assertTrue(record[field].strip())
    for field in ("model_version", "agent_version", "binary_sha256", "session_log_path", "error"):
        value = record[field]
        testcase.assertTrue(value is None or (isinstance(value, str) and value.strip()))
    if record["binary_sha256"] is not None:
        testcase.assertRegex(record["binary_sha256"], r"^[0-9a-f]{64}$")
    testcase.assertIs(type(record["trial"]), int)
    testcase.assertGreaterEqual(record["trial"], 0)
    for field in ("start_time", "end_time"):
        value = record[field]
        testcase.assertIsInstance(value, str)
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        testcase.assertIsNotNone(parsed.tzinfo)
        testcase.assertEqual(parsed.utcoffset(), timedelta(0))
    for field in (
        "turns",
        "tool_calls",
        "input_tokens",
        "output_tokens",
        "reasoning_tokens",
        "patch_size",
    ):
        testcase.assertIs(type(record[field]), int)
        testcase.assertGreaterEqual(record[field], 0)
    testcase.assertTrue(record["cost_usd"] is None or record["cost_usd"] >= 0)
    testcase.assertGreaterEqual(record["wall_clock_s"], 0)
    testcase.assertIsInstance(record["patch"], str)
    testcase.assertEqual(record["patch_size"], len(record["patch"].encode("utf-8")))
    testcase.assertTrue(record["patch_apply_ok"] is None or type(record["patch_apply_ok"]) is bool)
    testcase.assertIn(record["verifier_result"], {"pass", "fail", "error", "not_run"})
    testcase.assertIsInstance(record["failure_tags"], list)
    testcase.assertEqual(len(record["failure_tags"]), len(set(record["failure_tags"])))
    testcase.assertTrue(set(record["failure_tags"]) <= CANONICAL_FAILURE_TAGS)


class PatchExtractionTests(unittest.TestCase):
    def test_byte_capture_is_opt_in(self) -> None:
        subprocess_runner = runner.SubprocessRunner()
        text = subprocess_runner.run(
            [sys.executable, "-c", "print('ok')"]
        )
        self.assertEqual(text.stdout, "ok\n")
        self.assertIsNone(text.stdout_bytes)

        raw = subprocess_runner.run(
            [
                sys.executable,
                "-c",
                "import sys; sys.stdout.buffer.write(b'caf\\xc3\\xa9')",
            ],
            capture_bytes=True,
        )
        self.assertEqual(raw.stdout_bytes, "café".encode("utf-8"))
        self.assertEqual(raw.stdout, "café")

    def test_patch_extraction_captures_bytes_only_for_diff(self) -> None:
        fake = FakeRunner(lambda argv, kwargs: runner.CommandResult(tuple(argv), 0))
        patch = runner.extract_patch(fake.run, "abc1234", repo="/testbed")
        self.assertEqual(patch, "")
        for argv, kwargs in fake.calls:
            if "--no-color" in argv:
                self.assertTrue(kwargs.get("capture_bytes"))
            else:
                self.assertFalse(kwargs.get("capture_bytes", False))

    def test_patch_extraction_uses_and_removes_a_temp_index(self) -> None:
        fake = FakeRunner(lambda argv, kwargs: runner.CommandResult(tuple(argv), 0))
        self.assertEqual(runner.extract_patch(fake.run, "abc1234"), "")
        read_tree, add, diff, cleanup = fake.calls
        index_path = dict(read_tree[1]["env"])["GIT_INDEX_FILE"]
        self.assertTrue(index_path.startswith("/tmp/keel-extract-index-"))
        self.assertEqual(dict(add[1]["env"])["GIT_INDEX_FILE"], index_path)
        self.assertEqual(dict(diff[1]["env"])["GIT_INDEX_FILE"], index_path)
        self.assertIn("--cached", diff[0])
        self.assertEqual(cleanup[0], ["rm", "-f", index_path])

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = GitRepo(Path(self.temporary.name))
        self.repo.write("app.py", "value = 1\n")
        self.base = self.repo.commit()

    def extract(self) -> str:
        return runner.extract_patch(
            local_exec, self.base, repo=str(self.repo.root)
        )

    def check(self, patch: str):
        return runner.check_patch_applies(
            local_exec,
            self.base,
            patch,
            repo=str(self.repo.root),
        )

    def test_modified_file_is_in_patch_and_applies(self) -> None:
        self.repo.write("app.py", "value = 2\n")
        patch = runner.validate_patch(self.extract())
        self.assertIn("diff --git a/app.py b/app.py", patch)
        self.assertIn("-value = 1", patch)
        self.assertIn("+value = 2", patch)
        checked = self.check(patch)
        self.assertTrue(checked.ok, checked.error)

    def test_file_mode_changes_do_not_pollute_the_patch(self) -> None:
        path = self.repo.root / "app.py"
        self.repo.write("app.py", "value = 2\n")
        os.chmod(path, 0o755)
        # Avoid git treating this same-size rewrite as racily clean.
        os.utime(path, (time.time() + 5, time.time() + 5))
        raw = subprocess.run(
            [
                "git",
                "-c",
                "core.fileMode=true",
                "-C",
                str(self.repo.root),
                "diff",
                "--no-color",
                self.base,
                "--",
                "app.py",
            ],
            text=True,
            capture_output=True,
        )
        # Windows has no executable bit for git to observe; POSIX does.
        if os.name != "nt":
            self.assertIn("old mode 100644", raw.stdout)
            self.assertIn("new mode 100755", raw.stdout)

        patch = runner.validate_patch(self.extract())
        self.assertNotIn("old mode", patch)
        self.assertNotIn("new mode", patch)
        self.assertIn("+value = 2", patch)
        checked = self.check(patch)
        self.assertTrue(checked.ok, checked.error)

    @unittest.skipIf(os.name == "nt", "Windows has no executable bit for git")
    def test_index_mode_noise_does_not_pollute_the_patch(self) -> None:
        path = self.repo.root / "app.py"
        self.repo.write("app.py", "value = 2\n")
        os.chmod(path, 0o755)
        # Simulate the official image: the index says 755, base tree says 644.
        self.repo.git("update-index", "--chmod=+x", "app.py")
        os.utime(path, (time.time() + 5, time.time() + 5))
        self.assertIn("100755", self.repo.git("ls-files", "-s", "app.py").stdout)
        self.assertIn(
            "100644", self.repo.git("ls-tree", self.base, "app.py").stdout
        )
        raw = subprocess.run(
            [
                "git",
                "-c",
                "core.fileMode=true",
                "-C",
                str(self.repo.root),
                "diff",
                "--no-color",
                self.base,
                "--",
                "app.py",
            ],
            text=True,
            capture_output=True,
        )
        if os.name != "nt":
            self.assertIn("old mode 100644", raw.stdout)
            self.assertIn("new mode 100755", raw.stdout)

        patch = runner.validate_patch(self.extract())
        self.assertNotIn("old mode", patch)
        self.assertNotIn("new mode", patch)
        self.assertIn("+value = 2", patch)
        checked = self.check(patch)
        self.assertTrue(checked.ok, checked.error)

    def test_new_file_is_in_patch_and_applies(self) -> None:
        self.repo.write("new_module.py", "print('new')\n")
        patch = runner.validate_patch(self.extract())
        self.assertIn("diff --git a/new_module.py b/new_module.py", patch)
        self.assertIn("new file mode", patch)
        self.assertIn("+print('new')", patch)
        checked = self.check(patch)
        self.assertTrue(checked.ok, checked.error)

    def test_deleted_file_is_in_patch_and_applies(self) -> None:
        (self.repo.root / "app.py").unlink()
        patch = runner.validate_patch(self.extract())
        self.assertIn("diff --git a/app.py b/app.py", patch)
        self.assertIn("deleted file mode", patch)
        self.assertIn("-value = 1", patch)
        checked = self.check(patch)
        self.assertTrue(checked.ok, checked.error)

    def test_generated_artifacts_are_excluded(self) -> None:
        artifacts = [
            "__pycache__/app.cpython-311.pyc",
            ".venv/lib/site.py",
            "build/out.o",
            "dist/package.whl",
            "target/debug/keel",
            "node_modules/left-pad/index.js",
            ".pytest_cache/v/cache",
            ".mypy_cache/3.11/x.json",
            ".ruff_cache/x",
            "editor.py.swp",
            ".DS_Store",
            ".idea/workspace.xml",
            ".vscode/settings.json",
            "sub/build/generated.c",
        ]
        for path in artifacts:
            self.repo.write(path, "artifact\n")
        self.repo.write("app.py", "value = 2\n")
        patch = runner.validate_patch(self.extract())
        for path in artifacts:
            self.assertNotIn(path, patch)
        self.assertIn("diff --git a/app.py b/app.py", patch)
        checked = self.check(patch)
        self.assertTrue(checked.ok, checked.error)

    def test_empty_patch(self) -> None:
        patch = self.extract()
        self.assertEqual(patch, "")
        self.assertEqual(runner.validate_patch(patch), "")
        self.assertTrue(self.check(patch).ok)

    def test_trailing_whitespace_is_accepted_by_advisory_check(self) -> None:
        self.repo.write("app.py", "value = 1\ntrailing \n")
        patch = runner.validate_patch(self.extract())
        self.assertIn("+trailing ", patch)
        checked = self.check(patch)
        self.assertTrue(checked.ok, checked.error)

    def test_non_applying_patch_check_reports_failure(self) -> None:
        patch = (
            "diff --git a/app.py b/app.py\n"
            "--- a/app.py\n"
            "+++ b/app.py\n"
            "@@ -1 +1 @@\n"
            "-not the base content\n"
            "+value = 2\n"
        )
        checked = self.check(patch)
        self.assertFalse(checked.ok)
        self.assertTrue(checked.error)

    def test_non_utf8_diff_is_rejected(self) -> None:
        latin = self.repo.root / "latin.py"
        latin.write_bytes(b"value = 'caf\xe9'\n")
        self.repo.commit("latin")
        latin.write_bytes(b"value = 'caf\xe8'\n")
        with self.assertRaises(runner.PatchRejected) as caught:
            self.extract()
        self.assertIn("UTF-8", str(caught.exception))

    def test_real_subprocess_runner_rejects_non_utf8_diff(self) -> None:
        latin = self.repo.root / "latin.py"
        latin.write_bytes(b"value = 'caf\xe9'\n")
        self.repo.commit("latin")
        latin.write_bytes(b"value = 'caf\xe8'\n")
        with self.assertRaises(runner.PatchRejected) as caught:
            runner.extract_patch(
                real_subprocess_exec,
                self.base,
                repo=str(self.repo.root),
            )
        self.assertIn("UTF-8", str(caught.exception))

    def test_changed_files_include_new_files(self) -> None:
        self.repo.write("new_module.py", "print('new')\n")
        self.repo.write("app.py", "value = 2\n")
        runner.extract_patch(local_exec, self.base, repo=str(self.repo.root))
        self.assertEqual(
            sorted(
                runner.changed_files(
                    local_exec, self.base, repo=str(self.repo.root)
                )
            ),
            ["app.py", "new_module.py"],
        )

    def test_changed_files_ignore_mode_only_changes(self) -> None:
        os.chmod(self.repo.root / "app.py", 0o755)
        self.repo.git("update-index", "--chmod=+x", "app.py")
        self.assertEqual(
            runner.changed_files(local_exec, self.base, repo=str(self.repo.root)),
            [],
        )
        self.assertEqual(
            runner.extract_patch(local_exec, self.base, repo=str(self.repo.root)),
            "",
        )


class PatchFinalizationTests(unittest.TestCase):
    """M12.6: one helper stages, gates, detects test edits and checks apply."""

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = GitRepo(Path(self.temporary.name))
        self.repo.write("app.py", "value = 1\n")
        self.repo.write("tests/test_app.py", "def test_value():\n    assert True\n")
        self.base = self.repo.commit()

    def finalize(self, **kwargs):
        return runner.finalize_patch(
            local_exec, self.base, repo=str(self.repo.root), **kwargs
        )

    def test_new_file_is_included_after_staging(self) -> None:
        self.repo.write("new_module.py", "print('new')\n")
        finalized = self.finalize()
        self.assertIn("diff --git a/new_module.py b/new_module.py", finalized.patch)
        self.assertIn("new file mode", finalized.patch)
        self.assertIn("new_module.py", finalized.changed_files)
        self.assertTrue(finalized.apply_check.ok, finalized.apply_check.error)
        self.assertTrue(finalized.whitespace_ok)
        self.assertFalse(finalized.tests_modified)

    def test_generated_artifacts_are_excluded_from_patch_and_files(self) -> None:
        for path in (
            "__pycache__/app.cpython-311.pyc",
            ".venv/lib/site.py",
            "build/out.o",
            "dist/package.whl",
            "target/debug/keel",
            "node_modules/left-pad/index.js",
            "editor.py.swp",
            ".DS_Store",
        ):
            self.repo.write(path, "artifact\n")
        self.repo.write("app.py", "value = 2\n")
        finalized = self.finalize()
        self.assertIn("diff --git a/app.py b/app.py", finalized.patch)
        self.assertEqual(finalized.changed_files, ("app.py",))
        for path in (
            "__pycache__",
            ".venv",
            "build/",
            "dist/",
            "target/",
            "node_modules",
            ".swp",
            ".DS_Store",
        ):
            self.assertNotIn(path, finalized.patch)

    def test_test_file_edit_is_detected(self) -> None:
        self.repo.write("tests/test_app.py", "def test_value():\n    assert False\n")
        finalized = self.finalize()
        self.assertTrue(finalized.tests_modified)
        self.assertEqual(finalized.modified_test_files, ("tests/test_app.py",))
        self.assertIn("tests/test_app.py", finalized.changed_files)
        self.assertTrue(finalized.apply_check.ok, finalized.apply_check.error)

    def test_whitespace_error_is_detected(self) -> None:
        self.repo.write("app.py", "value = 1\ntrailing \n")
        finalized = self.finalize()
        self.assertFalse(finalized.whitespace_ok)
        self.assertTrue(
            any("trailing whitespace" in line for line in finalized.whitespace_errors),
            finalized.whitespace_errors,
        )
        self.assertIn("+trailing ", finalized.patch)
        # The advisory apply check still accepts the patch; the gate is
        # separate so the trace can tell noise from a non-applying diff.
        self.assertTrue(finalized.apply_check.ok, finalized.apply_check.error)

    def test_blank_line_at_eof_is_a_whitespace_error(self) -> None:
        self.repo.write("app.py", "value = 1\n\n")
        finalized = self.finalize()
        self.assertFalse(finalized.whitespace_ok)
        self.assertTrue(finalized.whitespace_errors)

    def test_empty_workspace_finalizes_as_empty(self) -> None:
        finalized = self.finalize()
        self.assertEqual(finalized.patch, "")
        self.assertTrue(finalized.empty)
        self.assertEqual(finalized.changed_files, ())
        self.assertTrue(finalized.whitespace_ok)
        self.assertTrue(finalized.apply_check.ok)

    def test_temp_index_is_shared_and_removed(self) -> None:
        fake = FakeRunner(lambda argv, kwargs: runner.CommandResult(tuple(argv), 0))
        runner.finalize_patch(fake.run, "abc1234", check_apply=False)
        commands = [argv for argv, _ in fake.calls]
        self.assertEqual(commands[0], runner.patch_read_tree_argv("abc1234"))
        self.assertEqual(commands[1], runner.patch_add_argv())
        self.assertEqual(commands[2], runner.patch_diff_argv("abc1234"))
        self.assertEqual(commands[3], runner.patch_diff_check_argv("abc1234"))
        self.assertEqual(commands[4], runner.changed_files_argv("abc1234"))
        index_path = dict(fake.calls[0][1]["env"])["GIT_INDEX_FILE"]
        self.assertTrue(index_path.startswith("/tmp/keel-extract-index-"))
        for _, kwargs in fake.calls[:5]:
            self.assertEqual(dict(kwargs["env"])["GIT_INDEX_FILE"], index_path)
        self.assertEqual(commands[5], ["rm", "-f", index_path])

    def test_apply_check_is_part_of_finalization(self) -> None:
        self.repo.write("app.py", "value = 2\n")
        finalized = self.finalize()
        self.assertTrue(finalized.apply_check.ok, finalized.apply_check.error)
        self.assertEqual(
            runner.apply_check_argv("/tmp/worktree"),
            ["git", "-C", "/tmp/worktree", "apply", "--check", "-"],
        )


class PatchValidationTests(unittest.TestCase):
    def test_binary_git_patch_is_rejected(self) -> None:
        with self.assertRaises(runner.PatchRejected) as caught:
            runner.validate_patch(
                "diff --git a/x.bin b/x.bin\nGIT binary patch\nliteral 3\nKcmZ\n"
            )
        self.assertIn("binary", str(caught.exception).lower())

    def test_binary_phrase_inside_a_text_line_is_allowed(self) -> None:
        patch = (
            "diff --git a/notes.txt b/notes.txt\n"
            "+This paragraph mentions GIT binary patch but is plain text.\n"
        )
        self.assertEqual(runner.validate_patch(patch), patch)

    def test_binary_file_diff_is_rejected(self) -> None:
        with self.assertRaises(runner.PatchRejected):
            runner.validate_patch(
                "diff --git a/x.bin b/x.bin\n"
                "index 0000000..1111111\n"
                "Binary files a/x.bin and b/x.bin differ\n"
            )

    def test_nul_bytes_are_rejected(self) -> None:
        with self.assertRaises(runner.PatchRejected):
            runner.validate_patch("diff --git a/x b/x\n+\x00\n")

    def test_size_cap_is_enforced(self) -> None:
        patch = "diff --git a/x b/x\n" + ("+" + "a" * 200 + "\n")
        with self.assertRaises(runner.PatchRejected) as caught:
            runner.validate_patch(patch, cap_bytes=100)
        self.assertIn("cap", str(caught.exception).lower())

    def test_non_diff_is_rejected(self) -> None:
        with self.assertRaises(runner.PatchRejected):
            runner.validate_patch("not a patch\n")

    def test_missing_final_newline_is_repaired(self) -> None:
        patch = "diff --git a/x b/x\n+one"
        self.assertTrue(runner.validate_patch(patch).endswith("\n"))

    def test_lone_surrogate_is_rejected(self) -> None:
        with self.assertRaises(runner.PatchRejected) as caught:
            runner.validate_patch("diff --git a/x b/x\n+\ud800\n")
        self.assertIn("UTF-8", str(caught.exception))


class PredictionsTests(unittest.TestCase):
    def test_predictions_shape(self) -> None:
        prediction = runner.Prediction(
            instance_id="django__django-11099",
            model_name_or_path="keel-neuralwatt-deepseek-v4.1-flash",
            model_patch="diff --git a/x b/x\n+one\n",
        )
        self.assertEqual(
            list(prediction.to_record()),
            ["instance_id", "model_name_or_path", "model_patch"],
        )
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "predictions.jsonl"
            runner.write_predictions(path, [prediction])
            rows = [json.loads(line) for line in path.read_text().splitlines()]
        self.assertEqual(len(rows), 1)
        self.assertEqual(
            set(rows[0]), {"instance_id", "model_name_or_path", "model_patch"}
        )
        self.assertEqual(rows[0]["instance_id"], "django__django-11099")

    def test_empty_patch_prediction_shape(self) -> None:
        record = runner.Prediction("x__x-1", "model", "").to_record()
        self.assertEqual(record["model_patch"], "")

    def test_append_jsonl_keeps_one_record_per_line(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "traces.jsonl"
            runner.append_jsonl(path, {"a": 1})
            runner.append_jsonl(path, {"b": 2})
            self.assertEqual(len(path.read_text().splitlines()), 2)

    def test_prediction_instance_ids_preserve_order_without_duplicates(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "predictions.jsonl"
            runner.write_predictions(
                path,
                [
                    runner.Prediction("b__b-2", "model", ""),
                    runner.Prediction("a__a-1", "model", "diff\n"),
                ],
            )
            runner.append_jsonl(
                path,
                {"instance_id": "b__b-2", "model_name_or_path": "model"},
            )
            self.assertEqual(
                runner.prediction_instance_ids(path),
                ["b__b-2", "a__a-1"],
            )


class CommandConstructionTests(unittest.TestCase):
    def test_keel_chat_command(self) -> None:
        argv = runner.keel_chat_argv("neuralwatt/deepseek-v4.1-flash", "Fix it.", 7)
        self.assertEqual(
            argv,
            [
                "/usr/local/bin/keel",
                "chat",
                "neuralwatt/deepseek-v4.1-flash",
                "--workspace",
                "/testbed",
                "--prompt",
                "Fix it.",
                "--yes",
                "--max-turns",
                "7",
            ],
        )

    def test_keel_chat_command_requires_changes_and_warns_about_tests(self) -> None:
        system = runner.benchmark_system(
            runner.test_discovery.TestCommand(
                "python -m pytest tests/test_app.py",
                "pyproject.toml",
            )
        )
        argv = runner.keel_chat_argv(
            "neuralwatt/deepseek-v4.1-flash",
            "Fix it.",
            7,
            system=system,
            require_changes=True,
            test_loop=True,
        )
        self.assertIn("--require-changes", argv)
        self.assertIn("--test-loop", argv)
        self.assertEqual(argv[argv.index("--system") + 1], system)
        self.assertIn("pristine", system)
        self.assertIn("failing test", system)
        self.assertIn("python -m pytest tests/test_app.py", system)
        self.assertIn("hint, not a guarantee", system)
        retry = runner.keel_retry_argv("try again", 7, test_loop=True)
        self.assertEqual(retry[:4], ["/usr/local/bin/keel", "chat", "--resume", "last"])
        self.assertIn("--require-changes", retry)
        self.assertIn("--test-loop", retry)
        self.assertNotIn("--system", retry)

    def test_benchmark_system_without_a_discovered_hint(self) -> None:
        system = runner.benchmark_system(None)
        self.assertIn(runner.TEST_EDIT_WARNING, system)
        self.assertIn(runner.TEST_LOOP_GUIDANCE, system)
        self.assertNotIn("likely runs its tests", system)

    def test_docker_exec_forwards_env_names_without_values(self) -> None:
        fake = FakeRunner()
        docker = runner.Docker(fake, platform="linux/amd64")
        docker.exec(
            "container",
            ["keel", "chat"],
            env=[("NEURALWATT_API_KEY", None), ("KEEL_DATA_DIR", "/tmp/data")],
            workdir="/testbed",
        )
        self.assertEqual(
            fake.calls[0][0],
            [
                "docker",
                "exec",
                "-w",
                "/testbed",
                "-e",
                "NEURALWATT_API_KEY",
                "-e",
                "KEEL_DATA_DIR=/tmp/data",
                "container",
                "keel",
                "chat",
            ],
        )

    def test_docker_exec_uses_stdin_flag_for_apply_check(self) -> None:
        fake = FakeRunner()
        docker = runner.Docker(fake)
        docker.exec("c", ["git", "apply", "--check", "-"], stdin="patch")
        self.assertEqual(fake.calls[0][0][:2], ["docker", "exec"])
        self.assertIn("-i", fake.calls[0][0])

    def test_apply_check_does_not_enforce_whitespace(self) -> None:
        self.assertEqual(
            runner.apply_check_argv("/tmp/worktree"),
            ["git", "-C", "/tmp/worktree", "apply", "--check", "-"],
        )

    def test_docker_run_uses_workdir_and_platform(self) -> None:
        fake = FakeRunner()
        docker = runner.Docker(fake, platform="linux/amd64")
        docker.run_container("name", "image:latest")
        self.assertEqual(
            fake.calls[0][0],
            [
                "docker",
                "run",
                "-d",
                "--name",
                "name",
                "--platform",
                "linux/amd64",
                "--workdir",
                "/testbed",
                "image:latest",
                "sleep",
                "infinity",
            ],
        )

    def test_docker_run_attaches_only_the_restricted_network(self) -> None:
        fake = FakeRunner()
        docker = runner.Docker(fake, platform="linux/amd64")
        docker.run_container(
            "name", "image:latest", network="keel-test-internal"
        )
        argv = fake.calls[0][0]
        self.assertEqual(argv[argv.index("--network") + 1], "keel-test-internal")
        self.assertNotIn("none", argv)
        self.assertEqual(
            argv,
            [
                "docker",
                "run",
                "-d",
                "--name",
                "name",
                "--network",
                "keel-test-internal",
                "--platform",
                "linux/amd64",
                "--workdir",
                "/testbed",
                "image:latest",
                "sleep",
                "infinity",
            ],
        )

    def test_image_template_escapes_double_underscore(self) -> None:
        image = runner.image_for_instance(make_instance())
        self.assertEqual(
            image,
            "swebench/sweb.eval.x86_64.django_1776_django-11099:latest",
        )

    def test_explicit_image_wins_over_template(self) -> None:
        instance = make_instance(image="registry/custom:tag")
        self.assertEqual(runner.image_for_instance(instance), "registry/custom:tag")

    def test_unknown_image_placeholder_is_an_error(self) -> None:
        with self.assertRaises(runner.RunnerError):
            runner.image_for_instance(
                make_instance(), "image/{not_a_placeholder}:latest"
            )

    def test_container_name_is_docker_safe(self) -> None:
        name = runner.container_name("django__django-11099", "run/../id")
        self.assertRegex(name, r"^[A-Za-z0-9][A-Za-z0-9_.-]*$")
        self.assertLessEqual(len(name), 60)

    def test_patch_commands_include_exclusions(self) -> None:
        add = runner.patch_add_argv()
        diff = runner.patch_diff_argv("abc123")
        self.assertEqual(
            add[:5], ["git", "-c", "core.fileMode=false", "-C", "/testbed"]
        )
        self.assertIn("add", add)
        self.assertIn("-A", add)
        self.assertNotIn("-N", add)
        self.assertEqual(
            runner.patch_read_tree_argv("abc123"),
            ["git", "-C", "/testbed", "read-tree", "abc123"],
        )
        self.assertEqual(
            runner.patch_remove_index_argv("/tmp/keel-extract-index"),
            ["rm", "-f", "/tmp/keel-extract-index"],
        )
        self.assertEqual(
            diff[:5],
            [
                "git",
                "-c",
                "core.fileMode=false",
                "-C",
                "/testbed",
            ],
        )
        self.assertIn("--cached", diff)
        changed = runner.changed_files_argv("abc123")
        self.assertEqual(
            changed[:5],
            [
                "git",
                "-c",
                "core.fileMode=false",
                "-C",
                "/testbed",
            ],
        )
        self.assertIn("--cached", changed)
        self.assertIn(":(exclude,glob)**/__pycache__/**", add)
        self.assertIn(":(exclude,glob)**/.venv/**", diff)
        self.assertIn(":(exclude,glob)**/node_modules/**", diff)
        check = runner.patch_diff_check_argv("abc123")
        self.assertIn("--cached", check)
        self.assertIn("--check", check)
        self.assertIn(":(exclude,glob)**/target/**", check)
        self.assertIn(":(exclude,glob)**/__pycache__/**", changed)
        self.assertIn(":(exclude,glob)**/build/**", changed)

    def test_harness_command(self) -> None:
        config = make_config()
        argv = runner.harness_argv(
            Path("/tmp/runs/test/predictions.jsonl"), "test", config, python="python3"
        )
        self.assertEqual(argv[:3], ["python3", "-m", "swebench.harness.run_evaluation"])
        self.assertIn("--dataset_name", argv)
        self.assertIn("SWE-bench/SWE-bench_Lite", argv)
        self.assertEqual(runner.DEFAULT_HARNESS_DATASET, "SWE-bench/SWE-bench_Lite")
        self.assertIn("--predictions_path", argv)
        self.assertIn("/tmp/runs/test/predictions.jsonl", argv)
        self.assertIn("--run_id", argv)
        self.assertIn("test", argv)

    def test_harness_command_filters_to_prediction_ids(self) -> None:
        argv = runner.harness_argv(
            Path("/tmp/runs/test/predictions.jsonl"),
            "test",
            make_config(),
            instance_ids=["a__a-1", "b__b-2"],
            python="python3",
        )
        self.assertEqual(
            argv[-3:], ["--instance_ids", "a__a-1", "b__b-2"]
        )

    def test_env_specs_include_reproducibility_literals(self) -> None:
        specs = runner.env_specs(["NEURALWATT_API_KEY"])
        self.assertIn(("NEURALWATT_API_KEY", None), specs)
        self.assertIn(("KEEL_DATA_DIR", runner.CONTAINER_DATA_DIR), specs)
        self.assertIn(("KEEL_MCP", "0"), specs)
        self.assertIn(("KEEL_CACHE", "none"), specs)

    def test_provider_env_names_cover_known_and_generic_targets(self) -> None:
        self.assertEqual(
            runner.provider_env_names("neuralwatt/deepseek-v4.1-flash"),
            ("NEURALWATT_API_KEY", "KEEL_NEURALWATT_BASE_URL"),
        )
        self.assertEqual(
            runner.provider_env_names("opencode/big"),
            ("OPENCODE_API_KEY", "KEEL_OPENCODE_BASE_URL"),
        )
        self.assertEqual(
            runner.provider_env_names("grok/grok-4"),
            ("XAI_API_KEY", "KEEL_GROK_BASE_URL"),
        )
        self.assertEqual(
            runner.provider_env_names("antigravity/gemini-2.5-pro"),
            ("KEEL_ANTIGRAVITY_ACCESS_TOKEN", "KEEL_ANTIGRAVITY_BASE_URL"),
        )


class NetworkTests(unittest.TestCase):
    def relay_handler(self, state):
        def handler(argv, kwargs):
            if argv[:3] == ["docker", "image", "inspect"]:
                return runner.CommandResult(tuple(argv), 0)
            if argv[:3] == ["docker", "network", "create"]:
                state["network"] = argv[-1]
                return runner.CommandResult(tuple(argv), 0)
            if argv[:3] == ["docker", "run", "-d"]:
                state["relay"] = argv[argv.index("--name") + 1]
                state["relay_run"] = argv
                return runner.CommandResult(tuple(argv), 0)
            if argv[:3] == ["docker", "network", "connect"]:
                state["connect"] = argv
                return runner.CommandResult(tuple(argv), 0)
            if argv[:2] == ["docker", "cp"]:
                state["copy"] = argv
                return runner.CommandResult(tuple(argv), 0)
            if argv[:2] == ["docker", "exec"]:
                joined = " ".join(argv)
                if "command -v python3" in joined:
                    return runner.CommandResult(
                        tuple(argv), 0, "/usr/local/bin/python3\n"
                    )
                if "-d" in argv:
                    state["launch"] = argv
                    return runner.CommandResult(tuple(argv), 0)
                if "socket.create_connection" in joined:
                    state["probe"] = argv
                    return runner.CommandResult(tuple(argv), 0)
            if argv[:2] == ["docker", "inspect"]:
                payload = [
                    {
                        "NetworkSettings": {
                            "Networks": {
                                state["network"]: {"IPAddress": "172.30.0.2"}
                            }
                        }
                    }
                ]
                return runner.CommandResult(tuple(argv), 0, json.dumps(payload))
            raise AssertionError(f"unexpected command: {argv}")

        return handler

    def test_restricted_plan_uses_an_internal_network_and_one_relay(self) -> None:
        state: dict[str, str] = {}
        fake = FakeRunner(self.relay_handler(state))
        plan = runner.prepare_network(
            make_config(), runner.Docker(fake), "swebench:latest"
        )

        self.assertEqual(plan.policy, "allowlist")
        self.assertEqual(plan.allowed_hosts, ("api.neuralwatt.com",))
        self.assertEqual(plan.network_name, state["network"])
        self.assertEqual(plan.relay_name, state["relay"])
        self.assertEqual(plan.relay_address, "172.30.0.2")
        self.assertEqual(plan.base_url, f"http://{state['relay']}:8080/v1")
        self.assertFalse(plan.contaminated)

        create = next(
            argv
            for argv, _ in fake.calls
            if argv[:3] == ["docker", "network", "create"]
        )
        self.assertEqual(
            create,
            ["docker", "network", "create", "--internal", plan.network_name],
        )
        self.assertEqual(state["connect"], [
            "docker", "network", "connect", "bridge", plan.relay_name
        ])
        self.assertEqual(state["relay_run"][5:7], ["--network", plan.network_name])
        self.assertIn("--upstream", state["launch"])
        self.assertIn(
            "https://api.neuralwatt.com/v1", state["launch"]
        )
        self.assertIn("0.0.0.0:8080", state["launch"])
        for argv, _ in fake.calls:
            self.assertNotIn("none", argv)

    def test_open_plan_skips_docker_and_is_contaminated(self) -> None:
        fake = FakeRunner()
        plan = runner.prepare_network(
            make_config(allow_network=True), runner.Docker(fake), "image"
        )
        self.assertEqual(plan.policy, "open")
        self.assertTrue(plan.contaminated)
        self.assertEqual(fake.calls, [])

    def test_agent_env_points_keel_at_the_relay_and_leaves_the_key_bare(
        self,
    ) -> None:
        specs = runner.agent_env_specs(
            make_config(),
            TEST_NETWORK,
            environ={"NEURALWATT_API_KEY": "super-secret"},
        )
        self.assertIn(
            ("KEEL_NEURALWATT_BASE_URL", "http://keel-test-relay:8080/v1"), specs
        )
        self.assertIn(("NEURALWATT_API_KEY", None), specs)
        self.assertEqual(
            [value for _, value in specs if value == "super-secret"], []
        )

    def test_network_metadata_marks_an_open_run_contaminated(self) -> None:
        self.assertEqual(
            runner.network_metadata(make_config(allow_network=True)),
            {
                "network_policy": "open",
                "allowed_hosts": [],
                "network_contaminated": True,
            },
        )
        self.assertEqual(
            runner.network_metadata(make_config()),
            {
                "network_policy": "allowlist",
                "allowed_hosts": ["api.neuralwatt.com"],
                "network_contaminated": False,
            },
        )

    def test_provider_base_url_prefers_the_host_override(self) -> None:
        config = make_config()
        self.assertEqual(
            runner.provider_base_url(
                config, environ={"KEEL_NEURALWATT_BASE_URL": "https://relay.test/v1"}
            ),
            "https://relay.test/v1",
        )
        self.assertEqual(
            runner.provider_base_url(config, environ={}),
            "https://api.neuralwatt.com/v1",
        )
        self.assertEqual(
            runner.provider_base_url(
                make_config(base_url="https://flag.test/v2"),
                environ={"KEEL_NEURALWATT_BASE_URL": "https://env.test/v1"},
            ),
            "https://flag.test/v2",
        )
        self.assertEqual(
            runner.provider_host("https://relay.test/v1"), "relay.test"
        )
        with self.assertRaises(runner.RunnerError):
            runner.provider_host("relay.test/v1")
        with self.assertRaises(runner.RunnerError):
            runner.provider_base_url(
                make_config(keel_target="custom/default"), environ={}
            )

    def test_relay_origin_follows_an_alternate_provider_endpoint(self) -> None:
        state: dict[str, str] = {}
        fake = FakeRunner(self.relay_handler(state))
        plan = runner.prepare_network(
            make_config(base_url="https://other.example/v2"),
            runner.Docker(fake),
            "swebench:latest",
        )
        self.assertEqual(plan.allowed_hosts, ("other.example",))
        self.assertEqual(plan.base_url, f"http://{state['relay']}:8080/v2")
        self.assertIn("https://other.example/v2", state["launch"])
        self.assertEqual(
            runner.network_metadata(
                make_config(base_url="https://other.example/v2")
            )["allowed_hosts"],
            ["other.example"],
        )

    def test_relay_rejects_paths_outside_the_provider_prefix(self) -> None:
        allowed = runner.llm_relay.path_allowed
        self.assertTrue(allowed("/v1", "/v1"))
        self.assertTrue(allowed("/v1/chat/completions", "/v1"))
        self.assertFalse(allowed("/v1foo", "/v1"))
        self.assertFalse(allowed("/v2/chat/completions", "/v1"))
        self.assertFalse(allowed("//api.github.com/v1", "/v1"))
        self.assertFalse(allowed("/", "/v1"))
        self.assertFalse(allowed("/v1/../v2", "/v1"))
        self.assertFalse(allowed("/v1/%2e%2e/v2", "/v1"))
        self.assertFalse(allowed("/v1%2f..%2fv2", "/v1"))
        self.assertTrue(allowed("/anything", "/"))

    def test_cleanup_removes_the_relay_and_network(self) -> None:
        fake = FakeRunner()
        runner.cleanup_network(runner.Docker(fake), TEST_NETWORK, keep=False)
        self.assertEqual(
            [argv for argv, _ in fake.calls],
            [
                ["docker", "rm", "-f", "keel-test-relay"],
                ["docker", "network", "rm", "keel-test-internal"],
            ],
        )

    def test_failed_relay_start_removes_the_network(self) -> None:
        def handler(argv, kwargs):
            if argv[:3] == ["docker", "image", "inspect"]:
                return runner.CommandResult(tuple(argv), 0)
            if argv[:3] == ["docker", "network", "create"]:
                return runner.CommandResult(tuple(argv), 0)
            if argv[:3] == ["docker", "run", "-d"]:
                return runner.CommandResult(tuple(argv), 1, stderr="boom")
            if argv[:3] == ["docker", "network", "rm"]:
                return runner.CommandResult(tuple(argv), 0)
            raise AssertionError(f"unexpected command: {argv}")

        fake = FakeRunner(handler)
        with self.assertRaises(runner.RunnerError):
            runner.prepare_network(
                make_config(), runner.Docker(fake), "swebench:latest"
            )
        self.assertEqual(
            [argv for argv, _ in fake.calls if argv[:3] == ["docker", "network", "rm"]],
            [
                [
                    "docker",
                    "network",
                    "rm",
                    next(
                        argv[-1]
                        for argv, _ in fake.calls
                        if argv[:3] == ["docker", "network", "create"]
                    ),
                ]
            ],
        )


class InstanceLoadingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def write(self, name: str, text: str) -> Path:
        path = self.root / name
        path.write_text(text, encoding="utf-8")
        return path

    def test_jsonl_loading_and_filtering(self) -> None:
        rows = [
            {
                "instance_id": "a__a-1",
                "base_commit": "aaa",
                "problem_statement": "first",
            },
            {
                "instance_id": "b__b-2",
                "base_commit": "bbb",
                "problem_statement": "second",
                "image": "custom:image",
                "patch": "diff --git a/x b/x\n+one\n",
            },
        ]
        path = self.write("rows.jsonl", "\n".join(json.dumps(row) for row in rows))
        instances = runner.load_instances(path, ["b__b-2"])
        self.assertEqual(len(instances), 1)
        self.assertEqual(instances[0].instance_id, "b__b-2")
        self.assertEqual(instances[0].image, "custom:image")
        self.assertTrue(instances[0].gold_patch.startswith("diff --git"))

    def test_json_array_loading(self) -> None:
        path = self.write(
            "rows.json",
            json.dumps(
                [
                    {
                        "instance_id": "a__a-1",
                        "base_commit": "aaa",
                        "problem_statement": "first",
                    }
                ]
            ),
        )
        self.assertEqual(len(runner.load_instances(path)), 1)

    def test_json_object_instances_key(self) -> None:
        path = self.write(
            "rows.json",
            json.dumps(
                {
                    "instances": [
                        {
                            "instance_id": "a__a-1",
                            "base_commit": "aaa",
                            "problem_statement": "first",
                        }
                    ]
                }
            ),
        )
        self.assertEqual(runner.load_instances(path)[0].instance_id, "a__a-1")

    def test_missing_required_field_is_an_error(self) -> None:
        path = self.write(
            "rows.jsonl",
            json.dumps({"instance_id": "a__a-1", "problem_statement": "x"}) + "\n",
        )
        with self.assertRaises(runner.RunnerError) as caught:
            runner.load_instances(path)
        self.assertIn("base_commit", str(caught.exception))

    def test_unknown_instance_in_filter_is_an_error(self) -> None:
        path = self.write(
            "rows.jsonl",
            json.dumps(
                {
                    "instance_id": "a__a-1",
                    "base_commit": "aaa",
                    "problem_statement": "first",
                }
            )
            + "\n",
        )
        with self.assertRaises(runner.RunnerError) as caught:
            runner.load_instances(path, ["missing__missing-1"])
        self.assertIn("missing__missing-1", str(caught.exception))

    def test_unsafe_instance_id_is_rejected(self) -> None:
        path = self.write(
            "rows.jsonl",
            json.dumps(
                {
                    "instance_id": "../../escape",
                    "base_commit": "aaa",
                    "problem_statement": "first",
                }
            )
            + "\n",
        )
        with self.assertRaises(runner.RunnerError) as caught:
            runner.load_instances(path)
        self.assertIn("unsafe instance_id", str(caught.exception))

    def test_missing_dataset_is_an_error(self) -> None:
        with self.assertRaises(runner.RunnerError) as caught:
            runner.load_instances(self.root / "absent.jsonl")
        self.assertIn("not found", str(caught.exception))

    def test_duplicate_instance_ids_are_rejected(self) -> None:
        row = {
            "instance_id": "a__a-1",
            "base_commit": "aaa",
            "problem_statement": "first",
        }
        path = self.write(
            "duplicates.jsonl",
            json.dumps(row) + "\n" + json.dumps(row) + "\n",
        )
        with self.assertRaises(runner.RunnerError) as caught:
            runner.load_instances(path)
        self.assertIn("duplicate", str(caught.exception))
        self.assertIn("a__a-1", str(caught.exception))


class SessionAndTraceTests(unittest.TestCase):
    def test_session_metrics(self) -> None:
        records = [
            {
                "type": "meta",
                "version": 2,
                "id": "1",
                "provider": "neuralwatt",
                "model": "deepseek-v4.1-flash",
            },
            {"type": "message", "message": {"role": "user", "parts": []}},
            {
                "type": "message",
                "message": {
                    "role": "assistant",
                    "stop": "tool_use",
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 5,
                        "reasoning_tokens": 2,
                    },
                    "parts": [
                        {"content": {"type": "tool_call", "name": "read"}},
                        {"content": {"type": "tool_call", "name": "edit"}},
                        {"content": {"type": "text", "text": "working"}},
                    ],
                },
            },
            {"type": "compaction", "summary": {}, "messages": []},
            {
                "type": "message",
                "message": {
                    "role": "assistant",
                    "stop": "end_turn",
                    "usage": {"input_tokens": 3, "output_tokens": 4},
                    "parts": [{"content": {"type": "text", "text": "done"}}],
                },
            },
            "not json",
        ]
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "session.jsonl"
            path.write_text(
                "\n".join(
                    json.dumps(record) if isinstance(record, dict) else record
                    for record in records
                )
                + "\n"
            )
            metrics = runner.parse_session_metrics(path)
        self.assertEqual(metrics["turns"], 2)
        self.assertEqual(metrics["tool_calls"], 2)
        self.assertEqual(metrics["input_tokens"], 13)
        self.assertEqual(metrics["output_tokens"], 9)
        self.assertEqual(metrics["reasoning_tokens"], 2)
        self.assertEqual(metrics["stop_reason"], "end_turn")
        self.assertEqual(metrics["model_version"], "deepseek-v4.1-flash")
        self.assertIsNone(metrics["session_error"])

    def test_session_metrics_capture_the_turn_error(self) -> None:
        records = [
            {"type": "meta", "model": "deepseek-v4.1-flash"},
            {
                "type": "message",
                "message": {
                    "role": "assistant",
                    "stop": "error",
                    "error": "provider stream failed",
                    "parts": [],
                },
            },
        ]
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "session.jsonl"
            path.write_text(
                "\n".join(json.dumps(record) for record in records) + "\n"
            )
            metrics = runner.parse_session_metrics(path)
        self.assertEqual(metrics["session_error"], "provider stream failed")

    def test_cap_exhaustion_wins_over_the_last_model_stop(self) -> None:
        capped = runner.CommandResult(
            ("keel", "chat"), 0, "[max_turns] turns 75 in 1 out 1\n"
        )
        ended = runner.CommandResult(
            ("keel", "chat"), 0, "[end] turns 3 in 1 out 1\n"
        )
        self.assertTrue(runner.max_turns_reached(capped))
        self.assertFalse(runner.max_turns_reached(ended))
        self.assertEqual(
            runner.trace_stop_reason("end_turn", capped), "max_turns"
        )
        self.assertEqual(runner.trace_stop_reason("end_turn", ended), "end_turn")
        self.assertEqual(
            runner.trace_stop_reason("end_turn", capped, failure_stop=True),
            "end_turn",
        )

    def test_session_metrics_read_the_latest_test_status_and_verify_prompt(self) -> None:
        records = [
            {"type": "meta", "model": "deepseek-v4.1-flash"},
            {
                "type": "test_status",
                "status": {
                    "runs": 1,
                    "last_command": "python -m pytest tests/test_app.py",
                    "last_outcome": "failed",
                    "failing_tests": ["tests/test_app.py::test_value"],
                    "excerpt": "assert 1 == 2",
                },
            },
            {
                "type": "message",
                "message": {
                    "role": "user",
                    "parts": [
                        {
                            "content": {
                                "type": "text",
                                "text": runner.VERIFY_PROMPT_PREFIX
                                + " state what changed.",
                            }
                        }
                    ],
                },
            },
            {
                "type": "test_status",
                "status": {
                    "runs": 2,
                    "last_command": "python -m pytest tests/test_app.py",
                    "last_outcome": "passed",
                    "failing_tests": [],
                    "excerpt": None,
                },
            },
            {
                "type": "message",
                "message": {
                    "role": "assistant",
                    "stop": "end_turn",
                    "usage": {"input_tokens": 10, "output_tokens": 2},
                    "parts": [{"content": {"type": "text", "text": "done"}}],
                },
            },
        ]
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "session.jsonl"
            path.write_text(
                "\n".join(json.dumps(record) for record in records) + "\n"
            )
            metrics = runner.parse_session_metrics(path)
        self.assertEqual(metrics["test_runs"], 2)
        self.assertTrue(metrics["tests_run"])
        self.assertEqual(metrics["test_last_status"], "passed")
        self.assertEqual(metrics["failing_tests"], [])
        self.assertEqual(metrics["verify_prompts"], 1)
        self.assertEqual(metrics["turns"], 1)

    def test_trace_extension_set_is_pinned(self) -> None:
        trace = runner.base_trace(
            make_instance(),
            make_config(),
            runner.iso_now(),
            binary_sha256=None,
        )
        runner.finish_trace(trace, time.monotonic())
        assert_canonical_trace(self, trace)

        extra = dict(trace, unexpected_extension=True)
        with self.assertRaises(AssertionError):
            assert_canonical_trace(self, extra)

        missing = dict(trace)
        missing.pop("agent_exit_code")
        with self.assertRaises(AssertionError):
            assert_canonical_trace(self, missing)

    def test_tests_modified_detection(self) -> None:
        self.assertEqual(
            runner.tests_modified(["src/app.py", "docs/readme.md"]),
            (False, []),
        )
        modified, paths = runner.tests_modified(
            [
                "src/app.py",
                "tests/test_app.py",
                "pkg/test_module.py",
                "pkg/module_test.py",
                "conftest.py",
            ]
        )
        self.assertTrue(modified)
        self.assertEqual(
            paths,
            [
                "tests/test_app.py",
                "pkg/test_module.py",
                "pkg/module_test.py",
                "conftest.py",
            ],
        )

    def test_trace_has_the_shared_schema_and_exact_patch_size(self) -> None:
        config = make_config()
        trace = runner.base_trace(
            make_instance(),
            config,
            runner.iso_now(),
            binary_sha256="a" * 64,
        )
        finished = runner.finish_trace(trace, time.monotonic())
        assert_canonical_trace(self, finished)
        self.assertEqual(finished["schema_version"], "1.0")
        self.assertEqual(finished["benchmark"], "swebench")
        self.assertEqual(finished["trial"], 0)
        self.assertEqual(finished["turns"], 0)
        self.assertEqual(finished["tool_calls"], 0)
        self.assertEqual(finished["input_tokens"], 0)
        self.assertEqual(finished["output_tokens"], 0)
        self.assertEqual(finished["reasoning_tokens"], 0)
        self.assertEqual(finished["stop_reason"], "unknown")
        self.assertEqual(finished["patch"], "")
        self.assertEqual(finished["patch_size"], 0)
        self.assertEqual(finished["verifier_result"], "not_run")
        self.assertEqual(finished["failure_tags"], [])
        self.assertTrue(finished["end_time"].endswith("Z"))
        self.assertGreaterEqual(finished["wall_clock_s"], 0)

        patch = "diff --git a/café.py b/café.py\n+naïve\n"
        finished["patch"] = patch
        finished["patch_size"] = len(patch.encode("utf-8"))
        finished["patch_apply_ok"] = True
        finished["empty_patch"] = False
        assert_canonical_trace(self, finished)
        self.assertEqual(finished["patch_size"], len(patch.encode("utf-8")))
        self.assertNotEqual(finished["patch_size"], len(patch))


class ConfigTests(unittest.TestCase):
    def parse(self, *args):
        return runner.build_parser().parse_args(list(args))

    def test_negative_trial_is_rejected(self) -> None:
        args = self.parse("--keel-target", "neuralwatt/model", "--trial", "-1")
        with self.assertRaises(runner.RunnerError):
            runner.config_from_args(args)

    def test_empty_target_half_is_rejected(self) -> None:
        for target in ("neuralwatt/", "/model", "no-slash"):
            args = self.parse("--keel-target", target)
            with self.assertRaises(runner.RunnerError):
                runner.config_from_args(args)

    def test_env_value_assignment_is_rejected_at_parse_time(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit):
            self.parse("--env", "SECRET_TOKEN=do-not-print")
        self.assertIn("bare environment variable name", stderr.getvalue())
        self.assertNotIn("do-not-print", stderr.getvalue())

    def test_bare_env_name_is_accepted(self) -> None:
        args = self.parse("--env", "EXTRA_TOKEN")
        config = runner.config_from_args(args)
        self.assertEqual(config.extra_env, ("EXTRA_TOKEN",))

    def test_trial_defaults_to_zero(self) -> None:
        config = runner.config_from_args(self.parse())
        self.assertEqual(config.trial, 0)

    def test_max_agent_attempts_defaults_to_two_and_must_be_positive(self) -> None:
        config = runner.config_from_args(self.parse())
        self.assertEqual(config.max_agent_attempts, 2)
        with self.assertRaises(runner.RunnerError):
            runner.config_from_args(self.parse("--max-agent-attempts", "0"))

    def test_network_is_restricted_unless_the_flag_opens_it(self) -> None:
        config = runner.config_from_args(self.parse())
        self.assertFalse(config.allow_network)
        self.assertIsNone(config.base_url)
        opened = runner.config_from_args(self.parse("--allow-network"))
        self.assertTrue(opened.allow_network)
        self.assertTrue(runner.config_record(opened)["allow_network"])
        alternate = runner.config_from_args(
            self.parse("--base-url", "https://other.example/v2")
        )
        self.assertEqual(alternate.base_url, "https://other.example/v2")
        self.assertEqual(
            runner.config_record(alternate)["base_url"],
            "https://other.example/v2",
        )

    def test_zero_one_two_trial_sequence_is_pass_at_k_eligible(self) -> None:
        traces = []
        for index in range(3):
            config = runner.config_from_args(
                self.parse("--trial", str(index))
            )
            traces.append(
                runner.base_trace(
                    make_instance(),
                    config,
                    runner.iso_now(),
                    binary_sha256=None,
                )
            )
        trials = [trace["trial"] for trace in traces]
        self.assertEqual(trials, [0, 1, 2])
        # The M12.3 reporter requires the conventional contiguous 0..n-1
        # sequence before it computes pass@k.
        self.assertEqual(trials, list(range(len(trials))))


class TraceReportTests(unittest.TestCase):
    def test_verifier_results_mapping(self) -> None:
        report = {
            "resolved_ids": ["a"],
            "unresolved_ids": ["b"],
            "error_ids": ["c"],
            "empty_patch_ids": ["d"],
        }
        self.assertEqual(runner.verifier_results(report, "a"), "pass")
        self.assertEqual(runner.verifier_results(report, "b"), "fail")
        self.assertEqual(runner.verifier_results(report, "c"), "error")
        self.assertEqual(runner.verifier_results(report, "d"), "fail")
        self.assertEqual(runner.verifier_results(report, "e"), "not_run")
        self.assertEqual(runner.verifier_results(None, "a"), "not_run")

    def test_empty_patch_report_id_is_fail_not_unresolved(self) -> None:
        report = {
            "resolved_ids": [],
            "unresolved_ids": [],
            "error_ids": [],
            "empty_patch_ids": ["empty__empty-1"],
        }
        self.assertEqual(runner.verifier_results(report, "empty__empty-1"), "fail")

    def test_harness_failure_marks_untouched_traces_error(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "traces.jsonl"
            runner.append_jsonl(path, {"task_id": "a", "verifier_result": "not_run"})
            runner.append_jsonl(path, {"task_id": "b", "verifier_result": "not_run"})
            runner.update_traces_from_report(
                path,
                {"resolved_ids": ["a"], "unresolved_ids": [], "error_ids": []},
                harness_ok=False,
            )
            rows = runner.read_jsonl(path)
        self.assertEqual(rows[0]["verifier_result"], "pass")
        self.assertEqual(rows[1]["verifier_result"], "error")

    def test_harness_failure_without_report_marks_error(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "traces.jsonl"
            runner.append_jsonl(path, {"task_id": "a", "verifier_result": "not_run"})
            runner.update_traces_from_report(path, None, harness_ok=False)
            rows = runner.read_jsonl(path)
        self.assertEqual(rows[0]["verifier_result"], "error")


class HarnessTests(unittest.TestCase):
    def test_run_harness_streams_to_log(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            run_dir = Path(temporary)
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                code = runner.run_harness(
                    [sys.executable, "-c", "print('harness ran')"], run_dir
                )
            log = (run_dir / "harness.log").read_text()
        self.assertEqual(code, 0)
        self.assertIn("harness ran", log)
        self.assertIn("harness ran", stdout.getvalue())
        self.assertTrue(log.startswith("$ "))

    def test_missing_harness_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            run_dir = Path(temporary)
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                code = runner.run_harness(["/definitely/not/a/harness"], run_dir)
            log = (run_dir / "harness.log").read_text()
        self.assertEqual(code, 127)
        self.assertIn("could not start", log)

    def test_find_report_ignores_unrelated_json(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            run_dir = Path(temporary)
            (run_dir / "notes.json").write_text('{"x": 1}')
            report_path = run_dir / "logs" / "report.json"
            report_path.parent.mkdir()
            report_path.write_text(
                json.dumps(
                    {
                        "resolved_ids": ["a"],
                        "unresolved_ids": ["b"],
                        "error_ids": [],
                        "empty_patch_ids": [],
                    }
                )
            )
            report = runner.find_report(run_dir)
        self.assertIsNotNone(report)
        self.assertEqual(report["resolved_ids"], ["a"])


class EnvironmentTests(unittest.TestCase):
    def test_forwarded_env_names_requires_provider_key(self) -> None:
        config = make_config()
        with self.assertRaises(runner.RunnerError) as caught:
            runner.forwarded_env_names(config, require=True, environ={})
        self.assertIn("NEURALWATT_API_KEY", str(caught.exception))

    def test_forwarded_env_names_skips_unset_optional_base_url(self) -> None:
        config = make_config()
        names = runner.forwarded_env_names(
            config, require=True, environ={"NEURALWATT_API_KEY": "secret"}
        )
        self.assertEqual(names, ["NEURALWATT_API_KEY"])

    def test_custom_target_requires_base_url(self) -> None:
        config = make_config(keel_target="custom/default")
        with self.assertRaises(runner.RunnerError) as caught:
            runner.forwarded_env_names(config, require=True, environ={})
        self.assertIn("KEEL_CUSTOM_BASE_URL", str(caught.exception))

    def test_dry_run_mode_does_not_require_values(self) -> None:
        config = make_config()
        names = runner.forwarded_env_names(config, require=False, environ={})
        self.assertEqual(
            names, ["NEURALWATT_API_KEY", "KEEL_NEURALWATT_BASE_URL"]
        )

    def test_generic_provider_without_key_fails_fast(self) -> None:
        config = make_config(keel_target="opencode/big")
        with self.assertRaises(runner.RunnerError) as caught:
            runner.forwarded_env_names(config, require=True, environ={})
        self.assertIn("OPENCODE_API_KEY", str(caught.exception))

    def test_extra_env_satisfies_provider_credential_check(self) -> None:
        config = make_config(
            keel_target="opencode/big", extra_env=("MY_PROVIDER_TOKEN",)
        )
        names = runner.forwarded_env_names(
            config,
            require=True,
            environ={"MY_PROVIDER_TOKEN": "secret"},
        )
        self.assertEqual(names, ["MY_PROVIDER_TOKEN"])

    def test_grok_and_antigravity_use_catalog_credentials(self) -> None:
        grok = make_config(keel_target="grok/grok-4")
        self.assertEqual(
            runner.forwarded_env_names(
                grok, require=True, environ={"XAI_API_KEY": "secret"}
            ),
            ["XAI_API_KEY"],
        )
        antigravity = make_config(keel_target="antigravity/gemini-2.5-pro")
        self.assertEqual(
            runner.forwarded_env_names(
                antigravity,
                require=True,
                environ={"KEEL_ANTIGRAVITY_ACCESS_TOKEN": "secret"},
            ),
            ["KEEL_ANTIGRAVITY_ACCESS_TOKEN"],
        )

    def test_anthropic_accepts_either_credential(self) -> None:
        config = make_config(keel_target="anthropic/claude-sonnet-4-5")
        names = runner.forwarded_env_names(
            config, require=True, environ={"ANTHROPIC_AUTH_TOKEN": "secret"}
        )
        self.assertEqual(names, ["ANTHROPIC_AUTH_TOKEN"])

    def test_secret_values_never_enter_the_command_line(self) -> None:
        config = make_config()
        names = runner.forwarded_env_names(
            config,
            require=True,
            environ={"NEURALWATT_API_KEY": "super-secret-value"},
        )
        argv = runner._docker_exec_argv(
            "container", ["keel", "chat"], runner.env_specs(names), "/testbed"
        )
        joined = " ".join(argv)
        self.assertNotIn("super-secret-value", joined)
        self.assertIn("-e", argv)
        self.assertIn("NEURALWATT_API_KEY", argv)


class OrchestrationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = GitRepo(self.root / "repo")
        self.repo.write("app.py", "value = 1\n")
        self.base = self.repo.commit()
        self.repo.write("app.py", "value = 2\n")
        self.patch = runner.extract_patch(
            local_exec, self.base, repo=str(self.repo.root)
        )
        self.binary = self.root / "keel"
        self.binary.write_bytes(b"\x7fELF" + b"\x00" * 16)

    def test_run_instance_writes_prediction_trace_and_session(self) -> None:
        session_dir = self.root / "session-copy"
        session_dir.mkdir()

        def handler(argv, kwargs):
            joined = " ".join(argv)
            if "===KEEL-DISCOVERY:" in joined:
                return runner.CommandResult(
                    tuple(argv),
                    0,
                    "===KEEL-DISCOVERY:pyproject.toml===\n"
                    "[tool.pytest.ini_options]\naddopts = '-q'\n",
                )
            if " ls-files" in joined:
                return runner.CommandResult(
                    tuple(argv), 0, "tests/test_app.py\nsrc/app.py\n"
                )
            if argv[:3] == ["docker", "image", "inspect"]:
                return runner.CommandResult(tuple(argv), 0)
            if argv[:2] == ["docker", "cp"] and ":/tmp/keel-swebench-data/." in argv[2]:
                session = session_dir / "sessions" / "1.jsonl"
                session.parent.mkdir(parents=True, exist_ok=True)
                session.write_text(
                    json.dumps(
                        {
                            "type": "meta",
                            "model": "deepseek-v4.1-flash",
                        }
                    )
                    + "\n"
                    + json.dumps(
                        {
                            "type": "test_status",
                            "status": {
                                "runs": 2,
                                "last_command": "python -m pytest tests/test_app.py",
                                "last_outcome": "passed",
                                "failing_tests": [],
                                "excerpt": None,
                            },
                        }
                    )
                    + "\n"
                    + json.dumps(
                        {
                            "type": "message",
                            "message": {
                                "role": "user",
                                "parts": [
                                    {
                                        "content": {
                                            "type": "text",
                                            "text": runner.VERIFY_PROMPT_PREFIX
                                            + " report the change.",
                                        }
                                    }
                                ],
                            },
                        }
                    )
                    + "\n"
                    + json.dumps(
                        {
                            "type": "message",
                            "message": {
                                "role": "assistant",
                                "stop": "end_turn",
                                "usage": {"input_tokens": 7, "output_tokens": 3},
                                "parts": [
                                    {"content": {"type": "tool_call", "name": "read"}}
                                ],
                            },
                        }
                    )
                    + "\n"
                )
                destination = Path(argv[3])
                destination.mkdir(parents=True, exist_ok=True)
                shutil.copytree(session_dir, destination, dirs_exist_ok=True)
                return runner.CommandResult(tuple(argv), 0)
            if " --version" in joined:
                return runner.CommandResult(tuple(argv), 0, "keel 1.2.3\n")
            if " --no-color --no-ext-diff " in joined:
                return runner.CommandResult(tuple(argv), 0, self.patch)
            if " diff --cached --name-only " in joined:
                return runner.CommandResult(tuple(argv), 0, "tests/test_app.py\n")
            if " apply --check " in joined:
                return runner.CommandResult(tuple(argv), 0)
            return runner.CommandResult(tuple(argv), 0)

        fake = FakeRunner(handler)
        config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id="orchestration",
        )
        instance = make_instance(base_commit=self.base)
        run_dir = self.root / "runs" / "orchestration"
        run_dir.mkdir(parents=True)
        with mock.patch.dict(os.environ, {"NEURALWATT_API_KEY": "test-key"}):
            trace = runner.run_instance(
                config, instance, run_dir, runner.Docker(fake), "a" * 64, TEST_NETWORK
            )
        assert_canonical_trace(self, trace)

        predictions = [
            json.loads(line)
            for line in (run_dir / "predictions.jsonl").read_text().splitlines()
        ]
        self.assertEqual(len(predictions), 1)
        self.assertEqual(predictions[0]["instance_id"], instance.instance_id)
        self.assertEqual(predictions[0]["model_patch"], self.patch)
        self.assertTrue((run_dir / "instances" / instance.instance_id / "patch.diff").is_file())
        self.assertEqual(trace["agent_version"], "keel 1.2.3")
        self.assertEqual(trace["model_version"], "deepseek-v4.1-flash")
        self.assertEqual(trace["turns"], 1)
        self.assertEqual(trace["tool_calls"], 1)
        self.assertEqual(trace["input_tokens"], 7)
        self.assertEqual(trace["patch"], self.patch)
        self.assertTrue(trace["patch_apply_ok"])
        self.assertTrue(trace["tests_modified"])
        self.assertEqual(trace["modified_test_files"], ["tests/test_app.py"])
        self.assertFalse(trace["empty_patch"])
        self.assertEqual(trace["patch_size"], len(self.patch.encode("utf-8")))
        self.assertTrue(trace["patch_path"].endswith("patch.diff"))
        self.assertTrue(trace["session_log_path"].endswith(".jsonl"))
        self.assertEqual(trace["verifier_result"], "not_run")
        self.assertIsNone(trace["error"])
        self.assertEqual(trace["image"], runner.image_for_instance(instance))
        self.assertEqual(trace["agent_exit_code"], 0)
        self.assertEqual(trace["test_command_hint"], "python -m pytest")
        self.assertEqual(trace["test_command_source"], "pyproject.toml")
        self.assertEqual(trace["test_runs"], 2)
        self.assertTrue(trace["tests_run"])
        self.assertEqual(trace["test_last_status"], "passed")
        self.assertEqual(trace["failing_tests"], [])
        self.assertEqual(trace["verify_prompts"], 1)
        calls = [argv for argv, _ in fake.calls]
        name = runner.container_name(instance.instance_id, config.run_id)
        mode_command = [
            "docker",
            "exec",
            name,
            "git",
            "-C",
            "/testbed",
            "config",
            "core.fileMode",
            "false",
        ]
        self.assertIn(mode_command, calls)
        chat_index = next(
            index
            for index, argv in enumerate(calls)
            if " chat " in f" {' '.join(argv)} "
        )
        self.assertLess(calls.index(mode_command), chat_index)
        chat = calls[chat_index]
        self.assertIn("--test-loop", chat)
        self.assertIn("--require-changes", chat)
        system_text = chat[chat.index("--system") + 1]
        self.assertIn("python -m pytest", system_text)
        self.assertIn("hint, not a guarantee", system_text)
        self.assertTrue(
            any("===KEEL-DISCOVERY:" in " ".join(argv) for argv in calls[:chat_index])
        )
        self.assertTrue(
            any(argv[:3] == ["docker", "rm", "-f"] for argv, _ in fake.calls)
        )

    def test_rejected_binary_patch_writes_no_prediction(self) -> None:
        def handler(argv, kwargs):
            joined = " ".join(argv)
            if " --version" in joined:
                return runner.CommandResult(tuple(argv), 0, "keel 1.2.3\n")
            if " --no-color --no-ext-diff " in joined:
                return runner.CommandResult(
                    tuple(argv),
                    0,
                    "diff --git a/x.bin b/x.bin\n"
                    "new file mode 100644\n"
                    "GIT binary patch\n"
                    "literal 3\n"
                    "KcmZ\n",
                )
            return runner.CommandResult(tuple(argv), 0)

        fake = FakeRunner(handler)
        config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id="rejected",
        )
        instance = make_instance(base_commit=self.base)
        run_dir = self.root / "runs" / "rejected"
        run_dir.mkdir(parents=True)
        with mock.patch.dict(os.environ, {"NEURALWATT_API_KEY": "test-key"}):
            trace = runner.run_instance(
                config, instance, run_dir, runner.Docker(fake), "b" * 64, TEST_NETWORK
            )
        assert_canonical_trace(self, trace)
        self.assertIn("binary", (trace["error"] or "").lower())
        self.assertIn("patch-failure", trace["failure_tags"])
        self.assertEqual(trace["patch"], "")
        self.assertEqual(trace["patch_size"], 0)
        self.assertFalse(trace["patch_apply_ok"])
        self.assertTrue(trace["empty_patch"])
        self.assertFalse((run_dir / "predictions.jsonl").exists())
        self.assertFalse(
            (run_dir / "instances" / instance.instance_id / "patch.diff").exists()
        )

    def test_empty_patch_trace_and_prediction(self) -> None:
        def handler(argv, kwargs):
            joined = " ".join(argv)
            if " --version" in joined:
                return runner.CommandResult(tuple(argv), 0, "keel 1.2.3\n")
            if " --no-color --no-ext-diff " in joined:
                return runner.CommandResult(tuple(argv), 0, "")
            if " diff --cached --name-only " in joined:
                return runner.CommandResult(tuple(argv), 0, "")
            return runner.CommandResult(tuple(argv), 0)

        fake = FakeRunner(handler)
        config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id="empty",
        )
        instance = make_instance(base_commit=self.base)
        run_dir = self.root / "runs" / "empty"
        run_dir.mkdir(parents=True)
        with mock.patch.dict(os.environ, {"NEURALWATT_API_KEY": "test-key"}):
            trace = runner.run_instance(
                config, instance, run_dir, runner.Docker(fake), "c" * 64, TEST_NETWORK
            )
        assert_canonical_trace(self, trace)
        self.assertEqual(trace["patch"], "")
        self.assertEqual(trace["patch_size"], 0)
        self.assertTrue(trace["patch_apply_ok"])
        self.assertTrue(trace["empty_patch"])
        self.assertEqual(trace["agent_attempts"], 2)
        self.assertEqual(trace["retry_reason"], "empty_patch")
        self.assertIsNone(trace["error"])
        self.assertIn("premature-finish", trace["failure_tags"])
        rows = [
            json.loads(line)
            for line in (run_dir / "predictions.jsonl").read_text().splitlines()
        ]
        self.assertEqual(rows[0]["model_patch"], "")

    def run_with_handler(self, handler, run_id):
        fake = FakeRunner(handler)
        config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id=run_id,
        )
        instance = make_instance(base_commit=self.base)
        run_dir = self.root / "runs" / run_id
        run_dir.mkdir(parents=True)
        with mock.patch.dict(os.environ, {"NEURALWATT_API_KEY": "test-key"}):
            trace = runner.run_instance(
                config, instance, run_dir, runner.Docker(fake), "f" * 64, TEST_NETWORK
            )
        return trace, run_dir, fake

    def test_no_changes_exit_is_retried_once_then_recorded(self) -> None:
        chats: list[list[str]] = []

        def handler(argv, kwargs):
            joined = " ".join(argv)
            if " chat " in f" {joined} ":
                chats.append(argv)
                if len(chats) == 1:
                    return runner.CommandResult(
                        tuple(argv),
                        runner.REQUIRE_CHANGES_EXIT,
                        "",
                        f"{runner.REQUIRE_CHANGES_MARKER}; retry once with a prompt "
                        "that asks for the required edit",
                    )
                return runner.CommandResult(tuple(argv), 0)
            if argv[:2] == ["docker", "cp"] and ":/tmp/keel-swebench-data/." in argv[2]:
                return copy_fake_session(argv, stop="end_turn")
            if " --version" in joined:
                return runner.CommandResult(tuple(argv), 0, "keel 1.2.3\n")
            if " --no-color --no-ext-diff " in joined:
                return runner.CommandResult(tuple(argv), 0, "")
            if " diff --cached --name-only " in joined:
                return runner.CommandResult(tuple(argv), 0, "")
            return runner.CommandResult(tuple(argv), 0)

        trace, run_dir, _ = self.run_with_handler(handler, "no-changes-retry")
        assert_canonical_trace(self, trace)
        self.assertEqual(len(chats), 2)
        self.assertIn("--require-changes", chats[0])
        self.assertIn("--test-loop", chats[0])
        system_text = chats[0][chats[0].index("--system") + 1]
        self.assertIn(runner.TEST_EDIT_WARNING, system_text)
        self.assertEqual(chats[1][chats[1].index("chat") + 1], "--resume")
        self.assertEqual(chats[1][chats[1].index("--resume") + 1], "last")
        self.assertIn("--test-loop", chats[1])
        self.assertEqual(trace["agent_attempts"], 2)
        self.assertEqual(trace["retry_reason"], "empty_patch")
        self.assertEqual(trace["agent_exit_code"], 0)
        self.assertIsNone(trace["error"])
        self.assertNotIn("environment-error", trace["failure_tags"])
        self.assertTrue(trace["empty_patch"])
        self.assertIn("premature-finish", trace["failure_tags"])
        rows = runner.read_jsonl(run_dir / "predictions.jsonl")
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["model_patch"], "")

    def test_empty_patch_retry_can_produce_the_patch(self) -> None:
        state = {"diffs": 0}

        def handler(argv, kwargs):
            joined = " ".join(argv)
            if " chat " in f" {joined} ":
                return runner.CommandResult(tuple(argv), 0)
            if argv[:2] == ["docker", "cp"] and ":/tmp/keel-swebench-data/." in argv[2]:
                return copy_fake_session(argv, stop="end_turn")
            if " --version" in joined:
                return runner.CommandResult(tuple(argv), 0, "keel 1.2.3\n")
            if " --no-color --no-ext-diff " in joined:
                state["diffs"] += 1
                return runner.CommandResult(
                    tuple(argv), 0, self.patch if state["diffs"] > 1 else ""
                )
            if " diff --cached --name-only " in joined:
                return runner.CommandResult(
                    tuple(argv), 0, "app.py\n" if state["diffs"] > 1 else ""
                )
            return runner.CommandResult(tuple(argv), 0)

        trace, run_dir, _ = self.run_with_handler(handler, "empty-retry-fixed")
        assert_canonical_trace(self, trace)
        self.assertEqual(trace["agent_attempts"], 2)
        self.assertEqual(trace["retry_reason"], "empty_patch")
        self.assertFalse(trace["empty_patch"])
        self.assertIsNone(trace["error"])
        self.assertNotIn("premature-finish", trace["failure_tags"])
        rows = runner.read_jsonl(run_dir / "predictions.jsonl")
        self.assertEqual(rows[0]["model_patch"], self.patch)

    def test_whitespace_gate_retries_then_fails_with_a_clear_tag(self) -> None:
        dirty_patch = (
            "diff --git a/app.py b/app.py\n"
            "--- a/app.py\n"
            "+++ b/app.py\n"
            "@@ -1 +1 @@\n"
            "-value = 1\n"
            "+value = 2 \n"
        )
        chats = 0

        def handler(argv, kwargs):
            nonlocal chats
            joined = " ".join(argv)
            if " chat " in f" {joined} ":
                chats += 1
                return runner.CommandResult(tuple(argv), 0)
            if argv[:2] == ["docker", "cp"] and ":/tmp/keel-swebench-data/." in argv[2]:
                return copy_fake_session(argv, stop="end_turn")
            if " --version" in joined:
                return runner.CommandResult(tuple(argv), 0, "keel 1.2.3\n")
            if " --no-color --no-ext-diff " in joined:
                return runner.CommandResult(tuple(argv), 0, dirty_patch)
            if " diff --cached --check " in joined:
                return runner.CommandResult(
                    tuple(argv), 2, "app.py:2: trailing whitespace.\n+value = 2 \n"
                )
            if " diff --cached --name-only " in joined:
                return runner.CommandResult(tuple(argv), 0, "app.py\n")
            return runner.CommandResult(tuple(argv), 0)

        trace, run_dir, _ = self.run_with_handler(handler, "whitespace-gate")
        assert_canonical_trace(self, trace)
        self.assertEqual(chats, 2, "one retry, then the gate fails the trial")
        self.assertEqual(trace["retry_reason"], "whitespace")
        self.assertFalse(trace["whitespace_ok"])
        self.assertTrue(trace["whitespace_errors"])
        self.assertIn("whitespace gate failed", trace["error"])
        self.assertIn("patch-failure", trace["failure_tags"])
        self.assertEqual(trace["patch"], dirty_patch)
        self.assertFalse((run_dir / "predictions.jsonl").exists())

    def test_whitespace_gate_retry_can_clean_the_patch(self) -> None:
        dirty_patch = (
            "diff --git a/app.py b/app.py\n"
            "--- a/app.py\n"
            "+++ b/app.py\n"
            "@@ -1 +1 @@\n"
            "-value = 1\n"
            "+value = 2 \n"
        )
        state = {"diffs": 0, "checks": 0}

        def handler(argv, kwargs):
            joined = " ".join(argv)
            if " chat " in f" {joined} ":
                return runner.CommandResult(tuple(argv), 0)
            if argv[:2] == ["docker", "cp"] and ":/tmp/keel-swebench-data/." in argv[2]:
                return copy_fake_session(argv, stop="end_turn")
            if " --version" in joined:
                return runner.CommandResult(tuple(argv), 0, "keel 1.2.3\n")
            if " --no-color --no-ext-diff " in joined:
                state["diffs"] += 1
                return runner.CommandResult(
                    tuple(argv), 0, dirty_patch if state["diffs"] == 1 else self.patch
                )
            if " diff --cached --check " in joined:
                state["checks"] += 1
                if state["checks"] == 1:
                    return runner.CommandResult(
                        tuple(argv), 2, "app.py:2: trailing whitespace.\n+value = 2 \n"
                    )
                return runner.CommandResult(tuple(argv), 0)
            if " diff --cached --name-only " in joined:
                return runner.CommandResult(tuple(argv), 0, "app.py\n")
            return runner.CommandResult(tuple(argv), 0)

        trace, run_dir, _ = self.run_with_handler(handler, "whitespace-fixed")
        assert_canonical_trace(self, trace)
        self.assertEqual(trace["retry_reason"], "whitespace")
        self.assertTrue(trace["whitespace_ok"])
        self.assertEqual(trace["whitespace_errors"], [])
        self.assertIsNone(trace["error"])
        rows = runner.read_jsonl(run_dir / "predictions.jsonl")
        self.assertEqual(rows[0]["model_patch"], self.patch)

    def test_agent_timeout_kills_and_restarts_container(self) -> None:
        def handler(argv, kwargs):
            if " chat " in " " + " ".join(argv) + " ":
                return runner.CommandResult(tuple(argv), 124, timed_out=True)
            return runner.CommandResult(tuple(argv), 0)

        fake = FakeRunner(handler)
        docker = runner.Docker(fake)
        result = runner.run_agent(docker, "container", ["keel", "chat"], [], 5)
        self.assertTrue(result.timed_out)
        calls = [argv for argv, _ in fake.calls]
        self.assertIn(["docker", "kill", "container"], calls)
        self.assertIn(["docker", "start", "container"], calls)
        self.assertLess(
            calls.index(["docker", "kill", "container"]),
            calls.index(["docker", "start", "container"]),
        )

    def test_nonzero_agent_exit_sets_error_and_keeps_exit_code(self) -> None:
        def handler(argv, kwargs):
            joined = " ".join(argv)
            if " chat " in f" {joined} ":
                return runner.CommandResult(
                    tuple(argv), 2, "", "provider unavailable"
                )
            if argv[:2] == ["docker", "cp"] and ":/tmp/keel-swebench-data/." in argv[2]:
                return copy_fake_session(
                    argv, stop="end_turn", error="provider stream failed"
                )
            if " --version" in joined:
                return runner.CommandResult(tuple(argv), 0, "keel 1.2.3\n")
            if " --no-color --no-ext-diff " in joined:
                return runner.CommandResult(tuple(argv), 0, self.patch)
            if " diff --cached --name-only " in joined:
                return runner.CommandResult(tuple(argv), 0, "app.py\n")
            if " apply --check " in joined:
                return runner.CommandResult(tuple(argv), 0)
            return runner.CommandResult(tuple(argv), 0)

        fake = FakeRunner(handler)
        config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id="nonzero",
        )
        instance = make_instance(base_commit=self.base)
        run_dir = self.root / "runs" / "nonzero"
        run_dir.mkdir(parents=True)
        with mock.patch.dict(os.environ, {"NEURALWATT_API_KEY": "test-key"}):
            trace = runner.run_instance(
                config, instance, run_dir, runner.Docker(fake), "d" * 64, TEST_NETWORK
            )
        assert_canonical_trace(self, trace)
        self.assertEqual(trace["agent_exit_code"], 2)
        self.assertEqual(trace["stop_reason"], "error")
        self.assertIn("exited with code 2", trace["error"])
        self.assertIn("provider-error", trace["failure_tags"])
        self.assertNotIn("environment-error", trace["failure_tags"])
        self.assertEqual(trace["model_version"], "deepseek-v4.1-flash")

    def test_nonzero_agent_exit_without_provider_evidence_is_environment(self) -> None:
        def handler(argv, kwargs):
            joined = " ".join(argv)
            if " chat " in f" {joined} ":
                return runner.CommandResult(tuple(argv), 2, "", "usage: bad flag")
            if argv[:2] == ["docker", "cp"] and ":/tmp/keel-swebench-data/." in argv[2]:
                return copy_fake_session(argv, stop="end_turn")
            if " --version" in joined:
                return runner.CommandResult(tuple(argv), 0, "keel 1.2.3\n")
            if " --no-color --no-ext-diff " in joined:
                return runner.CommandResult(tuple(argv), 0, self.patch)
            if " diff --cached --name-only " in joined:
                return runner.CommandResult(tuple(argv), 0, "app.py\n")
            if " apply --check " in joined:
                return runner.CommandResult(tuple(argv), 0)
            return runner.CommandResult(tuple(argv), 0)

        fake = FakeRunner(handler)
        config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id="nonzero-env",
        )
        instance = make_instance(base_commit=self.base)
        run_dir = self.root / "runs" / "nonzero-env"
        run_dir.mkdir(parents=True)
        with mock.patch.dict(os.environ, {"NEURALWATT_API_KEY": "test-key"}):
            trace = runner.run_instance(
                config, instance, run_dir, runner.Docker(fake), "2" * 64, TEST_NETWORK
            )
        assert_canonical_trace(self, trace)
        self.assertEqual(trace["agent_exit_code"], 2)
        self.assertIn("environment-error", trace["failure_tags"])
        self.assertNotIn("provider-error", trace["failure_tags"])

    def test_timeout_error_survives_patch_rejection(self) -> None:
        def handler(argv, kwargs):
            joined = " ".join(argv)
            if " chat " in f" {joined} ":
                return runner.CommandResult(tuple(argv), 124, timed_out=True)
            if argv[:2] == ["docker", "cp"] and ":/tmp/keel-swebench-data/." in argv[2]:
                return copy_fake_session(argv, stop="end_turn")
            if " --version" in joined:
                return runner.CommandResult(tuple(argv), 0, "keel 1.2.3\n")
            if " --no-color --no-ext-diff " in joined:
                return runner.CommandResult(
                    tuple(argv),
                    0,
                    "diff --git a/x.bin b/x.bin\nGIT binary patch\nliteral 3\nKcmZ\n",
                )
            return runner.CommandResult(tuple(argv), 0)

        fake = FakeRunner(handler)
        config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id="timeout-reject",
        )
        instance = make_instance(base_commit=self.base)
        run_dir = self.root / "runs" / "timeout-reject"
        run_dir.mkdir(parents=True)
        with mock.patch.dict(os.environ, {"NEURALWATT_API_KEY": "test-key"}):
            trace = runner.run_instance(
                config, instance, run_dir, runner.Docker(fake), "e" * 64, TEST_NETWORK
            )
        assert_canonical_trace(self, trace)
        self.assertEqual(trace["agent_exit_code"], 124)
        self.assertEqual(trace["stop_reason"], "timeout")
        self.assertIn("timed out", trace["error"])
        self.assertIn("timeout", trace["failure_tags"])
        self.assertIn("patch-failure", trace["failure_tags"])
        self.assertFalse(trace["patch_apply_ok"])

    def test_timeout_appends_to_earlier_error(self) -> None:
        def handler(argv, kwargs):
            joined = " ".join(argv)
            if " config --global --add safe.directory " in joined:
                return runner.CommandResult(tuple(argv), 1, "", "permission denied")
            if " chat " in f" {joined} ":
                return runner.CommandResult(tuple(argv), 124, timed_out=True)
            if argv[:2] == ["docker", "cp"] and ":/tmp/keel-swebench-data/." in argv[2]:
                return copy_fake_session(argv, stop="end_turn")
            if " --version" in joined:
                return runner.CommandResult(tuple(argv), 0, "keel 1.2.3\n")
            if " --no-color --no-ext-diff " in joined:
                return runner.CommandResult(tuple(argv), 0, "")
            if " diff --cached --name-only " in joined:
                return runner.CommandResult(tuple(argv), 0, "")
            return runner.CommandResult(tuple(argv), 0)

        fake = FakeRunner(handler)
        config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id="timeout-append",
        )
        instance = make_instance(base_commit=self.base)
        run_dir = self.root / "runs" / "timeout-append"
        run_dir.mkdir(parents=True)
        with mock.patch.dict(os.environ, {"NEURALWATT_API_KEY": "test-key"}):
            trace = runner.run_instance(
                config, instance, run_dir, runner.Docker(fake), "3" * 64, TEST_NETWORK
            )
        assert_canonical_trace(self, trace)
        self.assertIn("could not mark /testbed as a safe git directory", trace["error"])
        self.assertIn("timed out after 60s", trace["error"])
        self.assertIn("; ", trace["error"])

    def test_nonzero_exit_appends_to_earlier_error(self) -> None:
        def handler(argv, kwargs):
            joined = " ".join(argv)
            if " config --global --add safe.directory " in joined:
                return runner.CommandResult(tuple(argv), 1, "", "permission denied")
            if " chat " in f" {joined} ":
                return runner.CommandResult(tuple(argv), 2, "", "usage: bad flag")
            if argv[:2] == ["docker", "cp"] and ":/tmp/keel-swebench-data/." in argv[2]:
                return copy_fake_session(argv, stop="end_turn")
            if " --version" in joined:
                return runner.CommandResult(tuple(argv), 0, "keel 1.2.3\n")
            if " --no-color --no-ext-diff " in joined:
                return runner.CommandResult(tuple(argv), 0, "")
            if " diff --cached --name-only " in joined:
                return runner.CommandResult(tuple(argv), 0, "")
            return runner.CommandResult(tuple(argv), 0)

        fake = FakeRunner(handler)
        config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id="nonzero-append",
        )
        instance = make_instance(base_commit=self.base)
        run_dir = self.root / "runs" / "nonzero-append"
        run_dir.mkdir(parents=True)
        with mock.patch.dict(os.environ, {"NEURALWATT_API_KEY": "test-key"}):
            trace = runner.run_instance(
                config, instance, run_dir, runner.Docker(fake), "4" * 64, TEST_NETWORK
            )
        assert_canonical_trace(self, trace)
        self.assertIn("could not mark /testbed as a safe git directory", trace["error"])
        self.assertIn("exited with code 2", trace["error"])
        self.assertIn("; ", trace["error"])

    def test_advisory_apply_check_failure_does_not_set_error(self) -> None:
        def handler(argv, kwargs):
            joined = " ".join(argv)
            if " chat " in f" {joined} ":
                return runner.CommandResult(tuple(argv), 0)
            if " --version" in joined:
                return runner.CommandResult(tuple(argv), 0, "keel 1.2.3\n")
            if " --no-color --no-ext-diff " in joined:
                return runner.CommandResult(tuple(argv), 0, self.patch)
            if " diff --cached --name-only " in joined:
                return runner.CommandResult(tuple(argv), 0, "app.py\n")
            if " apply --check " in joined:
                return runner.CommandResult(
                    tuple(argv), 1, "", "patch does not apply"
                )
            return runner.CommandResult(tuple(argv), 0)

        fake = FakeRunner(handler)
        config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id="advisory",
        )
        instance = make_instance(base_commit=self.base)
        run_dir = self.root / "runs" / "advisory"
        run_dir.mkdir(parents=True)
        with mock.patch.dict(os.environ, {"NEURALWATT_API_KEY": "test-key"}):
            trace = runner.run_instance(
                config, instance, run_dir, runner.Docker(fake), "f" * 64, TEST_NETWORK
            )
        assert_canonical_trace(self, trace)
        self.assertFalse(trace["patch_apply_ok"])
        self.assertIsNone(trace["error"])
        self.assertNotIn("patch-failure", trace["failure_tags"])
        self.assertEqual(trace["patch"], self.patch)
        rows = [
            json.loads(line)
            for line in (run_dir / "predictions.jsonl").read_text().splitlines()
        ]
        self.assertEqual(rows[0]["model_patch"], self.patch)

    def test_failed_docker_run_attempts_container_cleanup(self) -> None:
        def handler(argv, kwargs):
            if argv[:2] == ["docker", "run"]:
                return runner.CommandResult(tuple(argv), 1, "", "port is busy")
            if argv[:3] == ["docker", "image", "inspect"]:
                return runner.CommandResult(tuple(argv), 0)
            if argv[:3] == ["docker", "rm", "-f"]:
                return runner.CommandResult(tuple(argv), 0)
            return runner.CommandResult(tuple(argv), 0)

        fake = FakeRunner(handler)
        config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id="run-fails",
        )
        instance = make_instance(base_commit=self.base)
        run_dir = self.root / "runs" / "run-fails"
        run_dir.mkdir(parents=True)
        with mock.patch.dict(os.environ, {"NEURALWATT_API_KEY": "test-key"}):
            trace = runner.run_instance(
                config, instance, run_dir, runner.Docker(fake), "1" * 64, TEST_NETWORK
            )
        assert_canonical_trace(self, trace)
        self.assertIn("docker run failed", trace["error"])
        name = runner.container_name(instance.instance_id, config.run_id)
        self.assertIn(["docker", "rm", "-f", name], [argv for argv, _ in fake.calls])

    def test_failed_docker_run_cleanup_ignores_missing_container(self) -> None:
        def handler(argv, kwargs):
            if argv[:2] == ["docker", "run"]:
                return runner.CommandResult(tuple(argv), 1, "", "port is busy")
            if argv[:3] == ["docker", "image", "inspect"]:
                return runner.CommandResult(tuple(argv), 0)
            if argv[:3] == ["docker", "rm", "-f"]:
                return runner.CommandResult(
                    tuple(argv),
                    1,
                    "",
                    "Error response from daemon: No such container",
                )
            return runner.CommandResult(tuple(argv), 0)

        fake = FakeRunner(handler)
        config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id="run-fails-missing",
        )
        instance = make_instance(base_commit=self.base)
        run_dir = self.root / "runs" / "run-fails-missing"
        run_dir.mkdir(parents=True)
        with mock.patch.dict(os.environ, {"NEURALWATT_API_KEY": "test-key"}):
            trace = runner.run_instance(
                config, instance, run_dir, runner.Docker(fake), "5" * 64, TEST_NETWORK
            )
        assert_canonical_trace(self, trace)
        self.assertIn("docker run failed", trace["error"])
        self.assertNotIn("cleanup failed", trace["error"])

    def test_ensure_image_pulls_only_when_missing(self) -> None:
        present = FakeRunner(lambda argv, kwargs: runner.CommandResult(tuple(argv), 0))
        runner.ensure_image(present, "image:latest", "linux/amd64")
        self.assertEqual(len(present.calls), 1)

        def missing(argv, kwargs):
            if argv[:3] == ["docker", "image", "inspect"]:
                return runner.CommandResult(tuple(argv), 1, "", "no such image")
            return runner.CommandResult(tuple(argv), 0)

        absent = FakeRunner(missing)
        runner.ensure_image(absent, "image:latest", "linux/amd64")
        pull = absent.calls[1][0]
        self.assertEqual(
            pull,
            [
                "docker",
                "pull",
                "--platform",
                "linux/amd64",
                "image:latest",
            ],
        )

    def test_linux_binary_check(self) -> None:
        runner.check_linux_binary(self.binary)
        macho = self.root / "macho"
        macho.write_bytes(b"\xcf\xfa\xed\xfe" + b"\x00" * 16)
        with self.assertRaises(runner.RunnerError) as caught:
            runner.check_linux_binary(macho)
        self.assertIn("Mach-O", str(caught.exception))
        with self.assertRaises(runner.RunnerError):
            runner.check_linux_binary(self.root / "absent")


class RunInstancesTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.binary = self.root / "keel"
        self.binary.write_bytes(b"\x7fELF" + b"\x00" * 16)
        self.config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id="wiring",
        )
        self.run_dir = self.root / "runs" / "wiring"
        self.run_dir.mkdir(parents=True)
        self.instances = [
            make_instance(instance_id="a__a-1", base_commit="abc"),
            make_instance(instance_id="b__b-2", base_commit="def"),
        ]
        prepare = mock.patch.object(
            runner, "prepare_network", return_value=TEST_NETWORK
        )
        prepare.start()
        self.addCleanup(prepare.stop)
        cleanup = mock.patch.object(runner, "cleanup_network")
        cleanup.start()
        self.addCleanup(cleanup.stop)

    def trace(self, instance, **overrides):
        trace = runner.finish_trace(
            runner.base_trace(
                instance,
                self.config,
                runner.iso_now(),
                binary_sha256="a" * 64,
            ),
            time.monotonic(),
        )
        trace.update(overrides)
        assert_canonical_trace(self, trace)
        return trace

    def fake_run_instance(self, *, prediction: bool, **overrides):
        def run(config, instance, run_dir, docker, binary_sha256, network):
            if prediction:
                runner.append_jsonl(
                    run_dir / "predictions.jsonl",
                    runner.Prediction(
                        instance.instance_id, config.model_label, ""
                    ).to_record(),
                )
            return self.trace(instance, **overrides)

        return run

    def test_appends_traces_maps_report_and_invokes_harness(self) -> None:
        report = {
            "resolved_ids": ["a__a-1"],
            "unresolved_ids": [],
            "error_ids": [],
            "empty_patch_ids": ["b__b-2"],
        }
        with mock.patch.object(
            runner,
            "run_instance",
            side_effect=self.fake_run_instance(prediction=True),
        ), mock.patch.object(runner, "run_harness", return_value=0) as harness, mock.patch.object(
            runner, "find_report", return_value=report
        ):
            code, found = runner.run_instances(
                self.config, self.instances, self.run_dir
            )
        self.assertEqual(code, 0)
        self.assertEqual(found, report)
        rows = runner.read_jsonl(self.run_dir / "traces.jsonl")
        self.assertEqual([row["task_id"] for row in rows], ["a__a-1", "b__b-2"])
        self.assertEqual([row["verifier_result"] for row in rows], ["pass", "fail"])
        self.assertEqual(harness.call_count, 1)
        argv = harness.call_args.args[0]
        self.assertEqual(argv[:3], [sys.executable, "-m", "swebench.harness.run_evaluation"])
        self.assertIn(str(self.run_dir / "predictions.jsonl"), argv)
        self.assertEqual(
            argv[argv.index("--dataset_name") + 1],
            "SWE-bench/SWE-bench_Lite",
        )
        self.assertEqual(
            argv[argv.index("--instance_ids") + 1 :],
            ["a__a-1", "b__b-2"],
        )

    def test_missing_predictions_marks_error_and_does_not_run_harness(self) -> None:
        with mock.patch.object(
            runner,
            "run_instance",
            side_effect=self.fake_run_instance(prediction=False),
        ), mock.patch.object(runner, "run_harness") as harness:
            code, report = runner.run_instances(
                self.config, self.instances, self.run_dir
            )
        self.assertEqual(code, 1)
        self.assertIsNone(report)
        harness.assert_not_called()
        rows = runner.read_jsonl(self.run_dir / "traces.jsonl")
        self.assertEqual([row["verifier_result"] for row in rows], ["error", "error"])

    def test_harness_success_without_report_marks_error(self) -> None:
        with mock.patch.object(
            runner,
            "run_instance",
            side_effect=self.fake_run_instance(prediction=True),
        ), mock.patch.object(runner, "run_harness", return_value=0), mock.patch.object(
            runner, "find_report", return_value=None
        ):
            code, report = runner.run_instances(
                self.config, self.instances, self.run_dir
            )
        self.assertEqual(code, 1)
        self.assertIsNone(report)
        rows = runner.read_jsonl(self.run_dir / "traces.jsonl")
        self.assertEqual([row["verifier_result"] for row in rows], ["error", "error"])

    def test_advisory_apply_check_failure_does_not_fail_run(self) -> None:
        report = {
            "resolved_ids": ["a__a-1"],
            "unresolved_ids": [],
            "error_ids": [],
            "empty_patch_ids": [],
        }
        trace = self.fake_run_instance(
            prediction=True, patch_apply_ok=False, failure_tags=[]
        )
        with mock.patch.object(runner, "run_instance", side_effect=trace), mock.patch.object(
            runner, "run_harness", return_value=0
        ), mock.patch.object(runner, "find_report", return_value=report):
            code, _ = runner.run_instances(
                self.config, self.instances[:1], self.run_dir
            )
        self.assertEqual(code, 0)
        rows = runner.read_jsonl(self.run_dir / "traces.jsonl")
        self.assertIsNone(rows[0]["error"])
        self.assertFalse(rows[0]["patch_apply_ok"])

    def test_nonzero_agent_exit_fails_the_run(self) -> None:
        config = make_config(
            keel_binary=self.binary,
            output_dir=self.root / "runs",
            run_id="agent-failed",
            skip_harness=True,
        )
        run_dir = self.root / "runs" / "agent-failed"
        run_dir.mkdir(parents=True)
        trace = self.fake_run_instance(
            prediction=True,
            stop_reason="error",
            error="keel exited with code 2",
            failure_tags=["provider-error"],
        )
        with mock.patch.object(runner, "run_instance", side_effect=trace):
            code, _ = runner.run_instances(config, self.instances[:1], run_dir)
        self.assertEqual(code, 1)


class ManifestTests(unittest.TestCase):
    def test_run_manifest_records_config_argv_dataset_hash_and_mode(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dataset = root / "instances.jsonl"
            dataset.write_text(
                json.dumps(
                    {
                        "instance_id": "a__a-1",
                        "base_commit": "aaa",
                        "problem_statement": "first",
                    }
                )
                + "\n"
            )
            run_dir = root / "run"
            run_dir.mkdir()
            binary = root / "keel"
            binary.write_bytes(b"\x7fELF" + b"\x00" * 16)
            config = make_config(keel_binary=binary)
            argv = ["--dataset", str(dataset), "--run-id", "run"]
            path = runner.write_run_manifest(
                run_dir, config, dataset, argv, mode="agent"
            )
            record = json.loads(path.read_text())
            dataset_sha = hashlib.sha256(dataset.read_bytes()).hexdigest()
            binary_sha = hashlib.sha256(binary.read_bytes()).hexdigest()
        self.assertEqual(record["mode"], "agent")
        self.assertEqual(record["argv"], argv)
        self.assertEqual(record["dataset"]["path"], str(dataset.resolve()))
        self.assertEqual(record["dataset"]["sha256"], dataset_sha)
        self.assertEqual(record["config"]["keel_target"], config.keel_target)
        self.assertEqual(record["config"]["extra_env"], [])
        self.assertEqual(record["network_policy"], "allowlist")
        self.assertEqual(record["allowed_hosts"], ["api.neuralwatt.com"])
        self.assertFalse(record["network_contaminated"])
        self.assertEqual(record["keel_binary_sha256"], binary_sha)

    def test_run_manifest_marks_an_open_run_as_contaminated(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dataset = root / "instances.jsonl"
            dataset.write_text("{}\n")
            run_dir = root / "run"
            run_dir.mkdir()
            binary = root / "keel"
            binary.write_bytes(b"\x7fELF")
            config = make_config(keel_binary=binary, allow_network=True)
            record = json.loads(
                runner.write_run_manifest(
                    run_dir, config, dataset, [], mode="agent"
                ).read_text()
            )
        self.assertEqual(record["network_policy"], "open")
        self.assertEqual(record["allowed_hosts"], [])
        self.assertTrue(record["network_contaminated"])

    def test_gold_manifest_records_network_as_not_applicable(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dataset = root / "instances.jsonl"
            dataset.write_text("{}\n")
            run_dir = root / "run"
            run_dir.mkdir()
            binary = root / "keel"
            binary.write_bytes(b"\x7fELF")
            config = make_config(keel_binary=binary)
            record = json.loads(
                runner.write_run_manifest(
                    run_dir, config, dataset, [], mode="gold"
                ).read_text()
            )
        self.assertEqual(record["network_policy"], "not_applicable")
        self.assertEqual(record["allowed_hosts"], [])
        self.assertFalse(record["network_contaminated"])

    def test_network_plan_records_the_relay_name_and_resolved_address(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dataset = root / "instances.jsonl"
            dataset.write_text("{}\n")
            run_dir = root / "run"
            run_dir.mkdir()
            binary = root / "keel"
            binary.write_bytes(b"\x7fELF")
            config = make_config(keel_binary=binary)
            runner.write_run_manifest(run_dir, config, dataset, [], mode="agent")
            runner.record_network_plan(run_dir, TEST_NETWORK)
            record = json.loads((run_dir / "run.json").read_text())
        self.assertEqual(record["network_name"], "keel-test-internal")
        self.assertEqual(record["relay_name"], "keel-test-relay")
        self.assertEqual(record["relay_address"], "172.30.0.2")
        self.assertEqual(record["relay_base_url"], "http://keel-test-relay:8080/v1")
        self.assertEqual(record["network_policy"], "allowlist")


class GoldPatchTests(unittest.TestCase):
    def test_gold_patch_mode_writes_predictions(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dataset = root / "instances.jsonl"
            dataset.write_text(
                json.dumps(
                    {
                        "instance_id": "django__django-11099",
                        "base_commit": "abc1234",
                        "problem_statement": "Fix the bug.",
                        "patch": "diff --git a/x.py b/x.py\n"
                        "new file mode 100644\n"
                        "--- /dev/null\n"
                        "+++ b/x.py\n"
                        "@@ -0,0 +1 @@\n"
                        "+one\n",
                    }
                )
                + "\n"
            )
            output = root / "runs"
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                code = runner.main(
                    [
                        "--gold-patch",
                        "--skip-harness",
                        "--dataset",
                        str(dataset),
                        "--output-dir",
                        str(output),
                        "--run-id",
                        "gold",
                        "--model-label",
                        "gold",
                    ]
                )
            self.assertEqual(code, 0)
            manifest = json.loads((output / "gold" / "run.json").read_text())
            self.assertEqual(manifest["mode"], "gold")
            self.assertEqual(manifest["dataset"]["path"], str(dataset.resolve()))
            self.assertEqual(manifest["config"]["run_id"], "gold")
            self.assertEqual(
                manifest["dataset"]["sha256"],
                hashlib.sha256(dataset.read_bytes()).hexdigest(),
            )
            rows = [
                json.loads(line)
                for line in (output / "gold" / "predictions.jsonl")
                .read_text()
                .splitlines()
            ]
        self.assertEqual(
            rows,
            [
                {
                    "instance_id": "django__django-11099",
                    "model_name_or_path": "gold",
                    "model_patch": (
                        "diff --git a/x.py b/x.py\n"
                        "new file mode 100644\n"
                        "--- /dev/null\n"
                        "+++ b/x.py\n"
                        "@@ -0,0 +1 @@\n"
                        "+one\n"
                    ),
                }
            ],
        )

    def test_gold_mode_fails_when_harness_report_is_missing(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dataset = root / "instances.jsonl"
            dataset.write_text(
                json.dumps(
                    {
                        "instance_id": "django__django-11099",
                        "base_commit": "abc1234",
                        "problem_statement": "Fix the bug.",
                        "patch": "diff --git a/x.py b/x.py\n+one\n",
                    }
                )
                + "\n"
            )
            stderr = io.StringIO()
            with mock.patch.object(runner, "run_harness", return_value=0), mock.patch.object(
                runner, "find_report", return_value=None
            ), contextlib.redirect_stderr(stderr):
                code = runner.main(
                    [
                        "--gold-patch",
                        "--dataset",
                        str(dataset),
                        "--output-dir",
                        str(root / "runs"),
                        "--run-id",
                        "gold-missing-report",
                        "--model-label",
                        "gold",
                    ]
                )
        self.assertEqual(code, 1)
        self.assertIn("no report", stderr.getvalue())


class DryRunTests(unittest.TestCase):
    def test_dry_run_prints_commands_and_writes_nothing(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dataset = root / "instances.jsonl"
            dataset.write_text(
                json.dumps(
                    {
                        "instance_id": "django__django-11099",
                        "base_commit": "abc1234",
                        "problem_statement": "Fix the bug.",
                    }
                )
                + "\n"
            )
            output = root / "runs"
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                code = runner.main(
                    [
                        "--dry-run",
                        "--dataset",
                        str(dataset),
                        "--keel-binary",
                        str(root / "missing-keel"),
                        "--output-dir",
                        str(output),
                        "--run-id",
                        "dry",
                    ]
                )
            text = stdout.getvalue()
        self.assertEqual(code, 0)
        for fragment in (
            "docker image inspect",
            "docker pull",
            "docker run -d",
            "keel chat neuralwatt/deepseek-v4.1-flash",
            "--workspace /testbed",
            "--prompt",
            "--require-changes",
            "--system",
            "chat --resume last",
            "git -C /testbed read-tree",
            "git -c core.fileMode=false -C /testbed add -A",
            "git -c core.fileMode=false -C /testbed diff --cached --no-color",
            "git -c core.fileMode=false -C /testbed diff --cached --check",
            "rm -f /tmp/keel-extract-index-",
            "git apply --check",
            "swebench.harness.run_evaluation",
            "--dataset_name SWE-bench/SWE-bench_Lite",
            "--instance_ids django__django-11099",
        ):
            self.assertIn(fragment, text)
        self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
