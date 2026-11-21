"""Unit tests for benchmarks/swebench/run_keel.py.

The tests use temporary local git repositories and fake command runners; they
never need Docker, a SWE-bench image, a Linux keel binary or the swebench
package.
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "benchmarks" / "swebench"))

import run_keel as runner  # noqa: E402


def local_exec(argv, *, stdin=None, timeout=None):
    completed = subprocess.run(
        list(argv),
        input=stdin,
        text=True,
        capture_output=True,
        timeout=timeout,
    )
    return runner.CommandResult(
        argv=tuple(argv),
        returncode=completed.returncode,
        stdout=completed.stdout or "",
        stderr=completed.stderr or "",
    )


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


class PatchExtractionTests(unittest.TestCase):
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
            token="unit-test",
        )

    def test_modified_file_is_in_patch_and_applies(self) -> None:
        self.repo.write("app.py", "value = 2\n")
        patch = runner.validate_patch(self.extract())
        self.assertIn("diff --git a/app.py b/app.py", patch)
        self.assertIn("-value = 1", patch)
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

    def test_whitespace_error_fails_apply_check(self) -> None:
        self.repo.write("app.py", "value = 1\ntrailing \n")
        patch = runner.validate_patch(self.extract())
        self.assertIn("+trailing ", patch)
        checked = self.check(patch)
        self.assertFalse(checked.ok)
        self.assertIn("whitespace", (checked.error or "").lower())

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


class PatchValidationTests(unittest.TestCase):
    def test_binary_git_patch_is_rejected(self) -> None:
        with self.assertRaises(runner.PatchRejected) as caught:
            runner.validate_patch(
                "diff --git a/x.bin b/x.bin\nGIT binary patch\nliteral 3\nKcmZ\n"
            )
        self.assertIn("binary", str(caught.exception).lower())

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
        self.assertEqual(add[:5], ["git", "-C", "/testbed", "add", "-N"])
        self.assertIn(":(exclude,glob)**/__pycache__/**", add)
        self.assertIn(":(exclude,glob)**/.venv/**", diff)
        self.assertIn(":(exclude,glob)**/node_modules/**", diff)

    def test_harness_command(self) -> None:
        config = make_config()
        argv = runner.harness_argv(
            Path("/tmp/runs/test/predictions.jsonl"), "test", config, python="python3"
        )
        self.assertEqual(argv[:3], ["python3", "-m", "swebench.harness.run_evaluation"])
        self.assertIn("--dataset_name", argv)
        self.assertIn(config.harness_dataset, argv)
        self.assertIn("--predictions_path", argv)
        self.assertIn("/tmp/runs/test/predictions.jsonl", argv)
        self.assertIn("--run_id", argv)
        self.assertIn("test", argv)

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

    def test_trace_has_the_shared_schema(self) -> None:
        config = make_config()
        trace = runner.base_trace(
            make_instance(),
            config,
            runner.iso_now(),
            image="image",
            binary_sha256="abc",
        )
        required = {
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
        }
        self.assertTrue(required <= set(trace))
        self.assertEqual(trace["benchmark"], "swebench")
        self.assertEqual(trace["verifier_result"], "not_run")
        self.assertEqual(trace["failure_tags"], [])
        self.assertIn("empty_patch", trace)
        self.assertIn("tests_modified", trace)
        finished = runner.finish_trace(trace, time.monotonic())
        self.assertTrue(finished["end_time"].endswith("Z"))
        self.assertGreaterEqual(finished["wall_clock_s"], 0)


class TraceReportTests(unittest.TestCase):
    def test_verifier_results_mapping(self) -> None:
        report = {
            "resolved_ids": ["a"],
            "unresolved_ids": ["b"],
            "error_ids": ["c"],
        }
        self.assertEqual(runner.verifier_results(report, "a"), "pass")
        self.assertEqual(runner.verifier_results(report, "b"), "fail")
        self.assertEqual(runner.verifier_results(report, "c"), "error")
        self.assertEqual(runner.verifier_results(report, "d"), "not_run")
        self.assertEqual(runner.verifier_results(None, "a"), "not_run")

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
            if " diff --name-only " in joined:
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
                config, instance, run_dir, runner.Docker(fake), "binary-sha"
            )

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
        self.assertTrue(trace["patch_apply_ok"])
        self.assertTrue(trace["tests_modified"])
        self.assertEqual(trace["modified_test_files"], ["tests/test_app.py"])
        self.assertFalse(trace["empty_patch"])
        self.assertEqual(trace["patch_size"], len(self.patch.encode()))
        self.assertTrue(trace["session_log_path"].endswith(".jsonl"))
        self.assertEqual(trace["verifier_result"], "not_run")
        self.assertIsNone(trace["error"])
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
                config, instance, run_dir, runner.Docker(fake), "binary-sha"
            )
        self.assertIn("binary", (trace["error"] or "").lower())
        self.assertIn("patch-failure", trace["failure_tags"])
        self.assertFalse((run_dir / "predictions.jsonl").exists())
        self.assertFalse(
            (run_dir / "instances" / instance.instance_id / "patch.diff").exists()
        )

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
            "git -C /testbed add -N",
            "diff --no-color --no-ext-diff",
            "git apply --check",
            "swebench.harness.run_evaluation",
        ):
            self.assertIn(fragment, text)
        self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
