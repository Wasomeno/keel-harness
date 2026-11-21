"""Tests for the trace reporter and its CLI."""

from __future__ import annotations

import io
import json
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

from benchmarks.traces import report, schema

FIXTURES = Path(__file__).parent / "fixtures" / "traces"
PATCH = (
    "diff --git a/solution.py b/solution.py\n"
    "--- a/solution.py\n"
    "+++ b/solution.py\n"
    "@@ -1 +1 @@\n"
    "-old\n"
    "+new\n"
)


def make_trace(
    task_id: str = "task",
    trial: int = 0,
    verifier_result: str = "pass",
    failure_tags: list[str] | None = None,
    **overrides: object,
) -> dict:
    record = {
        "schema_version": "1.0",
        "benchmark": "terminal-bench-2.0",
        "task_id": task_id,
        "trial": trial,
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
        "verifier_result": verifier_result,
        "failure_tags": list(failure_tags or []),
        "session_log_path": None,
        "error": None,
    }
    record.update(overrides)
    if "patch_size" not in overrides:
        record["patch_size"] = len(record["patch"].encode("utf-8"))
    return record


class TraceReportTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self._temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self._temporary.cleanup)
        self.tmp = Path(self._temporary.name)
        self._writes = 0

    def write(self, records: list[dict], name: str | None = None) -> Path:
        self._writes += 1
        path = self.tmp / (name or f"run-{self._writes}.jsonl")
        path.write_text(
            "".join(
                json.dumps(record, ensure_ascii=False, separators=(",", ":")) + "\n"
                for record in records
            ),
            encoding="utf-8",
        )
        return path

    def compute(self, records: list[dict], **kwargs: object) -> dict:
        path = self.write(records)
        return report.compute_report(schema.read_trace_file(path), **kwargs)


class ReportMetricTests(TraceReportTestCase):
    def test_pass_at_1_and_pass_at_k(self) -> None:
        records = [
            make_trace("task-a", 0),
            make_trace("task-a", 1),
            make_trace("task-a", 2),
            make_trace("task-b", 0),
            make_trace("task-b", 1, "fail"),
            make_trace("task-b", 2, "fail"),
        ]
        result = self.compute(records)
        self.assertEqual(result["task_count"], 2)
        self.assertEqual(result["trial_count"], 6)
        self.assertEqual(result["solved_trials"], 4)
        self.assertEqual(result["failed_trials"], 2)
        self.assertAlmostEqual(result["pass@1"], 2 / 3)
        self.assertEqual(result["pass@k"]["k"], 3)
        self.assertEqual(result["pass@k"]["value"], 0.5)
        self.assertEqual(result["pass@k"]["tasks_considered"], 2)
        self.assertEqual(result["pass@k"]["tasks_excluded_insufficient_trials"], 0)
        task_b = next(item for item in result["per_task"] if item["task_id"] == "task-b")
        self.assertEqual(task_b["passes"], 1)
        self.assertEqual(task_b["pass@k"], False)

    def test_visible_test_run_rate_and_missing_field(self) -> None:
        records = [
            make_trace(
                "task-a",
                0,
                tests_run=True,
                test_runs=2,
                test_last_status="passed",
            ),
            make_trace(
                "task-a",
                1,
                tests_run=False,
                test_runs=0,
                test_last_status=None,
            ),
            make_trace("task-b", 0),
        ]
        result = self.compute(records)
        self.assertEqual(result["tests"]["evaluated"], 2)
        self.assertEqual(result["tests"]["ran"], 1)
        self.assertEqual(result["tests"]["run_rate"], 0.5)
        self.assertEqual(result["tests"]["last_failed"], 0)
        self.assertEqual(result["tests"]["missing"], 1)
        markdown = report.render_markdown(result)
        self.assertIn("Visible test runs", markdown)
        self.assertIn("1/2 evaluated trials (50.0%)", markdown)
        text = report.render_text(result)
        self.assertIn("visible test runs", text)

    def test_pass_at_k_one_uses_the_first_trial(self) -> None:
        # pass@1 averages every trial; pass@1 as the "all k trials" statistic
        # uses the first k trials in trial order.  For k=1 that is the first
        # trial of each task, which need not equal the all-trials mean.
        records = [
            make_trace("task-a", 0),
            make_trace("task-a", 1, "fail"),
            make_trace("task-b", 0),
        ]
        result = self.compute(records, k=1)
        self.assertAlmostEqual(result["pass@1"], (0.5 + 1.0) / 2)
        self.assertEqual(result["pass@k"]["value"], 1.0)

    def test_pass_at_k_excludes_tasks_with_too_few_trials(self) -> None:
        records = [
            make_trace("short", 0),
            make_trace("long", 0, "fail"),
            make_trace("long", 1, "fail"),
            make_trace("long", 2, "fail"),
        ]
        result = self.compute(records, k=3)
        self.assertAlmostEqual(result["pass@1"], 0.5)
        self.assertEqual(result["pass@k"]["value"], 0.0)
        self.assertEqual(result["pass@k"]["tasks_considered"], 1)
        self.assertEqual(result["pass@k"]["tasks_excluded_insufficient_trials"], 1)
        self.assertEqual(result["pass@k"]["non_contiguous_tasks"], 0)

        result = self.compute(records, k=4)
        self.assertIsNone(result["pass@k"]["value"])
        self.assertEqual(result["pass@k"]["tasks_considered"], 0)
        self.assertEqual(result["pass@k"]["tasks_excluded_insufficient_trials"], 2)

    def test_pass_at_k_excludes_non_contiguous_trial_indices(self) -> None:
        records = [
            make_trace("gap", 0),
            make_trace("gap", 2),
            make_trace("gap", 9),
            make_trace("late", 5, "fail"),
            make_trace("late", 6, "fail"),
            make_trace("late", 7, "fail"),
            make_trace("short", 0),
            make_trace("ok", 0, "fail"),
            make_trace("ok", 1, "fail"),
            make_trace("ok", 2, "fail"),
        ]
        result = self.compute(records, k=3)
        self.assertEqual(result["pass@k"]["value"], 0.0)
        self.assertEqual(result["pass@k"]["tasks_considered"], 1)
        self.assertEqual(result["pass@k"]["tasks_excluded_insufficient_trials"], 1)
        self.assertEqual(result["pass@k"]["non_contiguous_tasks"], 2)
        by_task = {task["task_id"]: task for task in result["per_task"]}
        self.assertIsNone(by_task["gap"]["pass@k"])
        self.assertIsNone(by_task["late"]["pass@k"])
        self.assertEqual(by_task["gap"]["trial_indices"], [0, 2, 9])
        self.assertFalse(by_task["ok"]["pass@k"])

    def test_not_run_scores_as_a_loss_and_has_an_unverified_bucket(self) -> None:
        records = [
            make_trace("a", 0, "pass"),
            make_trace("a", 1, "not_run"),
            make_trace("b", 0, "not_run"),
            make_trace("b", 1, "fail"),
        ]
        result = self.compute(records, k=2)
        self.assertAlmostEqual(result["pass@1"], 0.25)
        self.assertEqual(result["trial_pass_rate"], 0.25)
        self.assertEqual(result["solved_trials"], 1)
        self.assertEqual(result["failed_trials"], 1)
        self.assertEqual(result["unverified_trials"], 2)
        self.assertEqual(result["non_passing_trials"], 3)
        self.assertEqual(result["verifier"]["not_run"], 2)
        self.assertEqual(result["failures_untagged"], 3)
        self.assertEqual(result["pass@k"]["value"], 0.0)
        self.assertEqual(result["pass@k"]["tasks_considered"], 2)

        markdown = report.render_markdown(result)
        self.assertIn("| pass@1 | 25.0% (per-task mean", markdown)
        self.assertIn("| Trial pass rate | 25.0% (1 / 4 trials)", markdown)
        self.assertIn("| Unverified trials | 2 not_run", markdown)
        self.assertIn("1 failed trial(s) (verifier fail or error);", markdown)
        self.assertIn("2 unverified trial(s) (not_run, scored as losses);", markdown)
        self.assertIn("3 non-passing trial(s) carry no tag.", markdown)

        text = report.render_text(result)
        self.assertIn("pass@1: 25.0% (per-task mean; tasks weigh equally)", text)
        self.assertIn("trial pass rate: 25.0% (1/4 trials)", text)
        self.assertIn("failed: 1; unverified: 2", text)
        self.assertIn("untagged failures: 3", text)

    def test_all_pass_and_all_fail_intervals_are_degenerate(self) -> None:
        passing = self.compute(
            [make_trace("a", trial) for trial in range(3)]
        )
        self.assertEqual(passing["pass@1"], 1.0)
        self.assertEqual(passing["pass@1_ci95"]["low"], 1.0)
        self.assertEqual(passing["pass@1_ci95"]["high"], 1.0)

        failing = self.compute(
            [make_trace("a", trial, "fail") for trial in range(3)]
        )
        self.assertEqual(failing["pass@1"], 0.0)
        self.assertEqual(failing["pass@1_ci95"]["low"], 0.0)
        self.assertEqual(failing["pass@1_ci95"]["high"], 0.0)

    def test_degenerate_mixed_set_has_a_hand_checkable_interval(self) -> None:
        # Every task has exactly one pass in three trials, so every bootstrap
        # resample has the same mean and the interval collapses to 1/3.
        records = []
        for task in ("a", "b"):
            records += [
                make_trace(task, 0),
                make_trace(task, 1, "fail"),
                make_trace(task, 2, "fail"),
            ]
        result = self.compute(records)
        self.assertAlmostEqual(result["pass@1"], 1 / 3)
        self.assertAlmostEqual(result["pass@1_ci95"]["low"], 1 / 3, places=12)
        self.assertAlmostEqual(result["pass@1_ci95"]["high"], 1 / 3, places=12)

    def test_seed_makes_the_interval_deterministic(self) -> None:
        records = []
        for index, task in enumerate(("a", "b", "c", "d", "e", "f")):
            records += [
                make_trace(task, 0, "pass" if index % 2 == 0 else "fail"),
                make_trace(task, 1, "pass" if index % 3 == 0 else "fail"),
                make_trace(task, 2, "fail"),
            ]
        first = self.compute(records, seed=17)
        second = self.compute(records, seed=17)
        self.assertEqual(first["pass@1_ci95"], second["pass@1_ci95"])
        self.assertEqual(first["pass@1"], second["pass@1"])
        self.assertEqual(first["failure_tags"], second["failure_tags"])

    def test_cost_time_usage_and_patch_metrics(self) -> None:
        records = [
            make_trace(
                "task",
                0,
                "pass",
                cost_usd=0.01,
                wall_clock_s=10.0,
                turns=4,
                tool_calls=6,
                input_tokens=100,
                output_tokens=10,
                reasoning_tokens=1,
                patch="x" * 10,
                patch_apply_ok=True,
            ),
            make_trace(
                "task",
                1,
                "pass",
                cost_usd=0.02,
                wall_clock_s=20.0,
                turns=5,
                tool_calls=7,
                input_tokens=200,
                output_tokens=20,
                reasoning_tokens=2,
                patch="x" * 20,
                patch_apply_ok=False,
            ),
            make_trace(
                "task",
                2,
                "fail",
                failure_tags=["wrong-logic"],
                cost_usd=0.04,
                wall_clock_s=30.0,
                turns=6,
                tool_calls=8,
                input_tokens=300,
                output_tokens=30,
                reasoning_tokens=3,
                patch="",
                patch_apply_ok=None,
            ),
        ]
        result = self.compute(records)
        self.assertAlmostEqual(result["cost_usd"]["total"], 0.07)
        self.assertAlmostEqual(result["cost_usd"]["per_solved_trial"], 0.035)
        self.assertEqual(result["cost_usd"]["solved_tasks"], 1)
        self.assertTrue(result["cost_usd"]["solved_task_cost_complete"])
        self.assertAlmostEqual(result["cost_usd"]["solved_task_cost_total"], 0.07)
        self.assertAlmostEqual(result["cost_usd"]["per_solved_task"], 0.07)
        self.assertEqual(result["cost_usd"]["solved_tasks_missing_cost"], 0)
        self.assertEqual(result["cost_usd"]["missing_trials"], 0)
        self.assertAlmostEqual(result["wall_clock_s"]["mean"], 20.0)
        self.assertAlmostEqual(result["wall_clock_s"]["p95"], 29.0)
        self.assertEqual(result["wall_clock_s"]["total"], 60.0)

        self.assertEqual(result["usage"]["turns"]["total"], 15)
        self.assertEqual(result["usage"]["turns"]["mean"], 5)
        self.assertEqual(result["usage"]["tool_calls"]["total"], 21)
        self.assertEqual(result["usage"]["tool_calls"]["mean"], 7)
        self.assertEqual(result["usage"]["input_tokens"]["total"], 600)
        self.assertEqual(result["usage"]["input_tokens"]["mean"], 200)
        self.assertEqual(result["usage"]["output_tokens"]["total"], 60)
        self.assertEqual(result["usage"]["reasoning_tokens"]["total"], 6)
        self.assertEqual(result["usage"]["total_tokens"]["total"], 666)
        self.assertEqual(result["usage"]["total_tokens"]["mean"], 222)

        patch = result["patch"]
        self.assertAlmostEqual(patch["size_mean"], 10.0)
        self.assertAlmostEqual(patch["size_p95"], 19.0)
        self.assertEqual(patch["empty_count"], 1)
        self.assertAlmostEqual(patch["empty_rate"], 1 / 3)
        self.assertEqual(patch["apply_evaluated"], 2)
        self.assertEqual(patch["apply_failures"], 1)
        self.assertAlmostEqual(patch["apply_failure_rate"], 0.5)
        self.assertEqual(patch["apply_not_evaluated"], 1)

        self.assertEqual(result["verifier"]["pass"], 2)
        self.assertEqual(result["verifier"]["fail"], 1)
        self.assertEqual(result["failure_tags"]["wrong-logic"], 1)
        self.assertEqual(result["failures_untagged"], 0)

    def test_cost_per_solved_task_reports_missing_cost(self) -> None:
        result = self.compute(
            [
                make_trace("a", 0, "pass", cost_usd=None),
                make_trace("a", 1, "fail", cost_usd=0.02),
                make_trace("b", 0, "pass", cost_usd=0.03),
                make_trace("b", 1, "pass", cost_usd=0.04),
            ]
        )
        cost = result["cost_usd"]
        self.assertEqual(cost["solved_tasks"], 2)
        self.assertFalse(cost["solved_task_cost_complete"])
        self.assertIsNone(cost["solved_task_cost_total"])
        self.assertEqual(cost["solved_tasks_missing_cost"], 1)
        self.assertIsNone(cost["per_solved_task"])
        self.assertIn(
            "n/a (1 of 2 solved task(s) have missing cost)",
            report.render_markdown(result),
        )
        self.assertIn(
            "n/a (1 of 2 solved task(s) have missing cost)",
            report.render_text(result),
        )

    def test_cost_per_solved_task_is_unavailable_without_solved_tasks(self) -> None:
        result = self.compute(
            [
                make_trace("a", 0, "fail", cost_usd=0.01),
                make_trace("a", 1, "error", cost_usd=0.02),
            ]
        )
        cost = result["cost_usd"]
        self.assertEqual(cost["solved_tasks"], 0)
        self.assertFalse(cost["solved_task_cost_complete"])
        self.assertIsNone(cost["solved_task_cost_total"])
        self.assertIsNone(cost["per_solved_task"])
        self.assertIn("n/a (no solved tasks)", report.render_markdown(result))
        self.assertIn("n/a (no solved tasks)", report.render_text(result))

    def test_untagged_failure_is_counted(self) -> None:
        result = self.compute(
            [make_trace("a", 0, "fail"), make_trace("b", 0, "pass")]
        )
        self.assertEqual(result["failed_trials"], 1)
        self.assertEqual(result["failures_untagged"], 1)
        self.assertEqual(sum(result["failure_tags"].values()), 0)

    def test_tasks_weigh_equally_even_with_uneven_trials(self) -> None:
        result = self.compute(
            [
                make_trace("one-trial", 0, "pass"),
                make_trace("two-trials", 0, "fail"),
                make_trace("two-trials", 1, "fail"),
            ]
        )
        self.assertAlmostEqual(result["pass@1"], 0.5)
        self.assertEqual(result["trials_per_task"]["min"], 1)
        self.assertEqual(result["trials_per_task"]["max"], 2)

    def test_fixture_with_every_failure_tag(self) -> None:
        loaded = schema.read_trace_file(FIXTURES / "all_tags.jsonl")
        result = report.compute_report(loaded)
        self.assertEqual(result["pass@1"], 0.0)
        self.assertEqual(result["failed_trials"], 16)
        self.assertEqual(result["failures_untagged"], 0)
        self.assertEqual(result["task_count"], 16)
        for tag in schema.FAILURE_TAGS:
            with self.subTest(tag=tag):
                self.assertEqual(result["failure_tags"][tag], 1)
        self.assertIsNone(result["pass@k"]["value"])
        self.assertEqual(result["pass@k"]["tasks_excluded_insufficient_trials"], 16)
        self.assertEqual(result["harness_health"]["context_overflows"]["count"], 1)
        self.assertEqual(result["harness_health"]["loops"]["count"], 1)
        self.assertEqual(
            result["harness_health"]["test_command_failures"]["count"], 1
        )
        self.assertIsNone(result["harness_health"]["tool_errors"]["count"])

    def test_harness_health_prefers_direct_counters(self) -> None:
        records = [
            make_trace(
                "direct",
                0,
                "pass",
                harness_health={
                    "tool_errors": 2,
                    "context_overflows": 1,
                    "provider_retries": 3,
                    "loops": 1,
                    "nudges": 4,
                    "test_command_failures": 0,
                },
            ),
            make_trace(
                "tagged",
                0,
                "fail",
                failure_tags=["context-overflow", "loop", "test-broken"],
            ),
        ]
        result = self.compute(records)
        health = result["harness_health"]
        self.assertEqual(health["tool_errors"]["count"], 2)
        self.assertEqual(health["tool_errors"]["source"], "harness_health")
        self.assertEqual(health["tool_errors"]["available_trials"], 1)
        self.assertEqual(health["tool_errors"]["missing_trials"], 1)
        self.assertEqual(health["context_overflows"]["count"], 1)
        self.assertEqual(health["provider_retries"]["count"], 3)
        self.assertEqual(health["loops"]["count"], 1)
        self.assertEqual(health["nudges"]["count"], 4)
        self.assertEqual(health["test_command_failures"]["count"], 0)

    def test_harness_health_falls_back_to_tags(self) -> None:
        result = self.compute(
            [
                make_trace(
                    "tagged",
                    0,
                    "fail",
                    failure_tags=["context-overflow", "loop", "test-broken"],
                ),
                make_trace("clean", 0, "pass"),
            ]
        )
        health = result["harness_health"]
        self.assertEqual(health["context_overflows"]["count"], 1)
        self.assertEqual(
            health["context_overflows"]["source"],
            "failure_tags:context-overflow",
        )
        self.assertEqual(health["loops"]["count"], 1)
        self.assertEqual(health["test_command_failures"]["count"], 1)
        self.assertEqual(health["tool_errors"]["count"], None)
        self.assertEqual(health["tool_errors"]["source"], "unavailable")
        self.assertEqual(health["provider_retries"]["count"], None)
        self.assertEqual(health["nudges"]["count"], None)
        self.assertIn("loops: 1 (from `loop` tags)", report.render_text(result))

    def test_multiple_files_and_trace_index(self) -> None:
        first = self.write([make_trace("task-a", 0)], "shard-1.jsonl")
        second = self.write([make_trace("task-a", 1, "fail")], "shard-2.jsonl")
        loaded = schema.read_trace_files([first, second])
        result = report.compute_report(loaded)
        self.assertEqual(result["task_count"], 1)
        self.assertEqual(result["trial_count"], 2)
        records = result["per_task"][0]["records"]
        self.assertEqual(len(records), 2)
        self.assertIn("shard-1.jsonl", records[0]["source"])
        self.assertIn("shard-2.jsonl", records[1]["source"])

    def test_duplicate_trial_is_rejected(self) -> None:
        first = self.write([make_trace("task-a", 0)], "first.jsonl")
        second = self.write([make_trace("task-a", 0)], "second.jsonl")
        with self.assertRaises(report.ReportError) as raised:
            report.compute_report(schema.read_trace_files([first, second]))
        message = str(raised.exception)
        self.assertIn("duplicate trial", message)
        self.assertIn("first.jsonl", message)
        self.assertIn("second.jsonl", message)

    def test_mixed_arms_are_rejected(self) -> None:
        records = [
            make_trace("task-a", 0),
            make_trace("task-b", 0, model="openai/gpt-5"),
        ]
        with self.assertRaises(report.ReportError) as raised:
            report.compute_report(
                [
                    schema.LoadedTrace(record=record, path=Path("run.jsonl"), line=index + 1)
                    for index, record in enumerate(records)
                ]
            )
        self.assertIn("mixed arms", str(raised.exception))

    def test_malformed_record_is_rejected_by_the_reader(self) -> None:
        with self.assertRaises(schema.TraceValidationError) as raised:
            schema.read_trace_file(FIXTURES / "malformed" / "missing_field.jsonl")
        self.assertIn("verifier_result", str(raised.exception))


class PercentileAndBootstrapTests(unittest.TestCase):
    def test_percentile_matches_linear_interpolation(self) -> None:
        values = [1.0, 2.0, 3.0, 4.0]
        self.assertEqual(report.percentile(values, 0.0), 1.0)
        self.assertEqual(report.percentile(values, 0.5), 2.5)
        self.assertAlmostEqual(report.percentile(values, 0.95), 3.85)
        self.assertEqual(report.percentile(values, 1.0), 4.0)
        self.assertEqual(report.percentile([5.0], 0.95), 5.0)
        self.assertIsNone(report.percentile([], 0.5))

    def test_bootstrap_interval_shape_and_validation(self) -> None:
        interval = report.bootstrap_interval([1.0], resamples=10, seed=3)
        self.assertEqual(interval["low"], 1.0)
        self.assertEqual(interval["high"], 1.0)
        self.assertEqual(interval["resampling_unit"], "task")
        self.assertEqual(interval["resamples"], 10)
        self.assertEqual(interval["seed"], 3)
        with self.assertRaises(ValueError):
            report.bootstrap_interval([], resamples=10)
        with self.assertRaises(ValueError):
            report.bootstrap_interval([1.0], confidence=1.0)
        with self.assertRaises(ValueError):
            report.bootstrap_interval([1.0], resamples=0)


class ReportCliTests(TraceReportTestCase):
    def run_cli(self, argv: list[str]) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            code = report.main(argv)
        return code, stdout.getvalue(), stderr.getvalue()

    def test_json_output(self) -> None:
        path = FIXTURES / "valid_run.jsonl"
        code, stdout, stderr = self.run_cli(["--json", str(path), "--seed", "5"])
        self.assertEqual(code, 0, stderr)
        data = json.loads(stdout)
        self.assertEqual(data["benchmark"], "fixture")
        self.assertEqual(data["task_count"], 2)
        self.assertEqual(data["trial_count"], 6)
        self.assertAlmostEqual(data["pass@1"], 2 / 3)
        self.assertEqual(data["pass@k"]["value"], 0.5)
        self.assertEqual(data["pass@1_ci95"]["resampling_unit"], "task")
        self.assertEqual(data["failure_tags"]["localization"], 1)
        self.assertEqual(data["failure_tags"]["missed-requirement"], 1)
        self.assertEqual(data["patch"]["empty_count"], 2)
        self.assertEqual(len(data["per_task"]), 2)
        self.assertTrue(data["per_task"][0]["records"])

    def test_markdown_output_and_trace_index(self) -> None:
        path = FIXTURES / "valid_run.jsonl"
        code, stdout, stderr = self.run_cli([str(path)])
        self.assertEqual(code, 0, stderr)
        self.assertIn("## Result", stdout)
        self.assertIn("pass@1", stdout)
        self.assertIn("task-level percentile bootstrap", stdout)
        self.assertIn("## Failure tags", stdout)
        self.assertIn("## Non-passing trials", stdout)
        self.assertIn("terminal-bench/regex-log", stdout)
        self.assertIn("localization", stdout)
        self.assertIn("| pass@1 | 66.7% (per-task mean", stdout)
        self.assertIn("| Trial pass rate | 66.7% (4 / 6 trials)", stdout)

        code, stdout, stderr = self.run_cli([str(path), "--no-trace-index"])
        self.assertEqual(code, 0, stderr)
        self.assertNotIn("## Non-passing trials", stdout)

    def test_text_output(self) -> None:
        code, stdout, stderr = self.run_cli(
            ["--text", str(FIXTURES / "valid_run.jsonl")]
        )
        self.assertEqual(code, 0, stderr)
        self.assertIn("Benchmark trace report", stdout)
        self.assertIn("pass@1:", stdout)
        self.assertIn("Failure tags", stdout)

    def test_seed_is_deterministic_across_cli_runs(self) -> None:
        path = FIXTURES / "valid_run.jsonl"
        first = self.run_cli(["--json", str(path), "--seed", "19"])[1]
        second = self.run_cli(["--json", str(path), "--seed", "19"])[1]
        self.assertEqual(first, second)

    def test_k_option_changes_pass_at_k(self) -> None:
        path = FIXTURES / "valid_run.jsonl"
        code, stdout, stderr = self.run_cli(["--json", str(path), "--k", "1"])
        self.assertEqual(code, 0, stderr)
        data = json.loads(stdout)
        self.assertEqual(data["pass@k"]["k"], 1)
        # Both tasks pass their first trial, though regex-log fails two of
        # three overall.
        self.assertEqual(data["pass@k"]["value"], 1.0)
        self.assertAlmostEqual(data["pass@1"], 2 / 3)

        code, stdout, stderr = self.run_cli(["--json", str(path), "--k", "3"])
        self.assertEqual(code, 0, stderr)
        self.assertEqual(json.loads(stdout)["pass@k"]["value"], 0.5)

    def test_malformed_trace_returns_two_with_a_useful_error(self) -> None:
        path = FIXTURES / "malformed" / "missing_field.jsonl"
        code, stdout, stderr = self.run_cli([str(path)])
        self.assertEqual(code, 2)
        self.assertEqual(stdout, "")
        self.assertIn("verifier_result", stderr)
        self.assertIn(str(path), stderr)

    def test_missing_file_returns_two(self) -> None:
        code, stdout, stderr = self.run_cli([str(self.tmp / "nope.jsonl")])
        self.assertEqual(code, 2)
        self.assertEqual(stdout, "")
        self.assertIn("cannot read", stderr)

    def test_invalid_k_returns_two(self) -> None:
        code, stdout, stderr = self.run_cli(
            ["--k", "0", str(FIXTURES / "valid_run.jsonl")]
        )
        self.assertEqual(code, 2)
        self.assertEqual(stdout, "")
        self.assertIn("k must be", stderr)


if __name__ == "__main__":
    unittest.main()
