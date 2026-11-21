"""Aggregate benchmark trace JSONL files into a comparable report.

Usage::

    python3 benchmarks/traces/report.py runs/*.jsonl
    python3 benchmarks/traces/report.py runs/*.jsonl --json
    python3 benchmarks/traces/report.py runs/*.jsonl --seed 7 --k 3

The reporter summarizes one arm at a time: one benchmark, model, model version,
keel target, agent version and binary checksum.  Shards of the same run can be
passed together.  Mixing arms or duplicating a ``(benchmark, task_id, trial)``
is rejected instead of silently averaging incomparable trials.

The 95% interval is a percentile bootstrap over **tasks**, not trials.  Tasks
are the resampling unit so the three trials of one task stay together and
within-task correlation is preserved.  ``--seed`` makes the interval
deterministic.

Only the Python standard library is used.
"""

from __future__ import annotations

import argparse
import json
import math
import random
import sys
from collections import Counter, defaultdict
from collections.abc import Iterable, Sequence
from pathlib import Path
from typing import Any

if __package__:
    from . import schema
else:  # ``python3 benchmarks/traces/report.py``
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
    from benchmarks.traces import schema


ARM_FIELDS: tuple[str, ...] = (
    "benchmark",
    "model",
    "model_version",
    "keel_target",
    "agent_version",
    "binary_sha256",
)

USAGE_FIELDS: tuple[str, ...] = (
    "turns",
    "tool_calls",
    "input_tokens",
    "output_tokens",
    "reasoning_tokens",
)


class ReportError(ValueError):
    """The inputs cannot be combined into one report."""


def percentile(values: Sequence[float], fraction: float) -> float | None:
    """Linear-interpolated percentile, matching the usual NumPy definition."""

    if not values:
        return None
    if not 0.0 <= fraction <= 1.0:
        raise ValueError("fraction must be between 0 and 1")
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * fraction
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    weight = position - lower
    return ordered[lower] * (1.0 - weight) + ordered[upper] * weight


def bootstrap_interval(
    task_rates: Sequence[float],
    *,
    confidence: float = 0.95,
    resamples: int = 2000,
    seed: int = 0,
) -> dict[str, Any]:
    """Percentile bootstrap interval for the mean of per-task success rates."""

    if not 0.0 < confidence < 1.0:
        raise ValueError("confidence must be between 0 and 1")
    if resamples < 1:
        raise ValueError("resamples must be at least 1")
    if not task_rates:
        raise ValueError("task_rates must not be empty")

    rng = random.Random(seed)
    count = len(task_rates)
    estimates: list[float] = []
    for _ in range(resamples):
        total = 0.0
        for _ in range(count):
            total += task_rates[rng.randrange(count)]
        estimates.append(total / count)

    tail = (1.0 - confidence) / 2.0
    return {
        "low": percentile(estimates, tail),
        "high": percentile(estimates, 1.0 - tail),
        "confidence": confidence,
        "resamples": resamples,
        "seed": seed,
        "resampling_unit": "task",
        "method": (
            "percentile bootstrap over per-task success rates; tasks are "
            "resampled with replacement, all trials of a task stay together"
        ),
    }


def _mean(values: Sequence[float]) -> float | None:
    if not values:
        return None
    return sum(values) / len(values)


def _sum_or_none(values: Sequence[float]) -> float | None:
    if not values:
        return None
    return sum(values)


def _arm_label(arm: Sequence[Any]) -> str:
    labels = []
    for field, value in zip(ARM_FIELDS, arm):
        labels.append(f"{field}={value!r}")
    return ", ".join(labels)


def _check_unique_and_one_arm(traces: Sequence[schema.LoadedTrace]) -> dict[str, Any]:
    arms = {tuple(trace.record[field] for field in ARM_FIELDS) for trace in traces}
    if len(arms) > 1:
        details = "\n".join(f"  - {_arm_label(arm)}" for arm in sorted(arms, key=repr))
        raise ReportError(
            "mixed arms in one report; summarize one benchmark/model/agent run "
            "at a time and run separate reports for comparisons:\n" + details
        )

    seen: dict[tuple[str, str, int], schema.LoadedTrace] = {}
    for trace in traces:
        key = (
            trace.record["benchmark"],
            trace.record["task_id"],
            trace.record["trial"],
        )
        previous = seen.get(key)
        if previous is not None:
            raise ReportError(
                "duplicate trial: "
                f"benchmark={key[0]!r} task_id={key[1]!r} trial={key[2]} "
                f"appears at {previous.source} and {trace.source}; "
                "pass each shard once"
            )
        seen[key] = trace
    return dict(zip(ARM_FIELDS, next(iter(arms))))


def _health_metric(
    traces: Sequence[schema.LoadedTrace],
    key: str,
    *,
    fallback_tag: str | None = None,
) -> dict[str, Any]:
    direct = [
        trace.record["harness_health"][key]
        for trace in traces
        if isinstance(trace.record.get("harness_health"), dict)
        and key in trace.record["harness_health"]
    ]
    if direct:
        return {
            "count": sum(direct),
            "rate": sum(direct) / len(direct),
            "available_trials": len(direct),
            "missing_trials": len(traces) - len(direct),
            "source": "harness_health",
        }
    if fallback_tag is not None:
        count = sum(
            1 for trace in traces if fallback_tag in trace.record["failure_tags"]
        )
        return {
            "count": count,
            "rate": count / len(traces),
            "available_trials": len(traces),
            "missing_trials": 0,
            "source": f"failure_tags:{fallback_tag}",
        }
    return {
        "count": None,
        "rate": None,
        "available_trials": 0,
        "missing_trials": len(traces),
        "source": "unavailable",
    }


def compute_report(
    traces: Sequence[schema.LoadedTrace],
    *,
    k: int = 3,
    bootstrap: int = 2000,
    seed: int = 0,
    confidence: float = 0.95,
) -> dict[str, Any]:
    """Compute every headline metric from already-validated traces."""

    if not traces:
        raise ReportError("no trace records to report")
    if not isinstance(k, int) or k < 1:
        raise ReportError("k must be an integer >= 1")
    if not isinstance(bootstrap, int) or bootstrap < 1:
        raise ReportError("bootstrap resamples must be an integer >= 1")

    arm = _check_unique_and_one_arm(traces)
    ordered = sorted(
        traces,
        key=lambda trace: (
            trace.record["benchmark"],
            trace.record["task_id"],
            trace.record["trial"],
            trace.source,
        ),
    )

    by_task: dict[tuple[str, str], list[schema.LoadedTrace]] = defaultdict(list)
    for trace in ordered:
        by_task[(trace.record["benchmark"], trace.record["task_id"])].append(trace)

    per_task: list[dict[str, Any]] = []
    task_rates: list[float] = []
    pass_k_values: list[bool] = []
    pass_k_excluded = 0
    pass_k_non_contiguous = 0
    solved_tasks = 0
    solved_tasks_missing_cost = 0
    solved_task_cost_total: float | None = None

    for (benchmark, task_id), group in sorted(by_task.items()):
        group = sorted(group, key=lambda trace: (trace.record["trial"], trace.source))
        trial_count = len(group)
        trial_indices = [trace.record["trial"] for trace in group]
        passes = sum(
            1 for trace in group if trace.record["verifier_result"] == "pass"
        )
        success_rate = passes / trial_count
        task_rates.append(success_rate)

        # pass@k needs the conventional 0..n-1 trial sequence.  Without that,
        # "the first k trials" is not a meaningful contiguous prefix and a row
        # count would silently stand in for k.
        if trial_count < k:
            pass_k: bool | None = None
            pass_k_excluded += 1
        elif trial_indices != list(range(trial_count)):
            pass_k = None
            pass_k_non_contiguous += 1
        else:
            pass_k = all(
                trace.record["verifier_result"] == "pass"
                for trace in group[:k]
            )
            pass_k_values.append(pass_k)

        costs = [
            trace.record["cost_usd"]
            for trace in group
            if trace.record["cost_usd"] is not None
        ]
        if passes:
            solved_tasks += 1
            if len(costs) == trial_count:
                task_cost = sum(costs)
                solved_task_cost_total = (
                    task_cost
                    if solved_task_cost_total is None
                    else solved_task_cost_total + task_cost
                )
            else:
                solved_tasks_missing_cost += 1
        wall_values = [trace.record["wall_clock_s"] for trace in group]
        tags = sorted(
            {tag for trace in group for tag in trace.record["failure_tags"]}
        )
        records = [
            {
                "trial": trace.record["trial"],
                "verifier_result": trace.record["verifier_result"],
                "failure_tags": list(trace.record["failure_tags"]),
                "cost_usd": trace.record["cost_usd"],
                "wall_clock_s": trace.record["wall_clock_s"],
                "stop_reason": trace.record["stop_reason"],
                "error": trace.record["error"],
                "session_log_path": trace.record["session_log_path"],
                "source": trace.source,
            }
            for trace in group
        ]
        per_task.append(
            {
                "benchmark": benchmark,
                "task_id": task_id,
                "trial_count": trial_count,
                "trial_indices": trial_indices,
                "passes": passes,
                "pass@1": success_rate,
                "pass@k": pass_k,
                "failure_tags": tags,
                "cost_usd": _sum_or_none(costs),
                "cost_missing_trials": trial_count - len(costs),
                "wall_clock_s_mean": _mean(wall_values),
                "records": records,
            }
        )

    pass_at_1 = _mean(task_rates)
    pass_at_k = _mean([1.0 if value else 0.0 for value in pass_k_values])
    ci = bootstrap_interval(
        task_rates,
        confidence=confidence,
        resamples=bootstrap,
        seed=seed,
    )

    trial_count = len(ordered)
    verifier_counts = Counter(
        trace.record["verifier_result"] for trace in ordered
    )
    solved_trials = verifier_counts.get("pass", 0)
    failed_trials = verifier_counts.get("fail", 0) + verifier_counts.get("error", 0)
    unverified_trials = verifier_counts.get("not_run", 0)
    non_passing_trials = trial_count - solved_trials
    trial_pass_rate = solved_trials / trial_count if trial_count else None

    cost_values = [
        trace.record["cost_usd"]
        for trace in ordered
        if trace.record["cost_usd"] is not None
    ]
    cost_total = _sum_or_none(cost_values)
    cost_per_solved = (
        cost_total / solved_trials
        if cost_total is not None and solved_trials > 0
        else None
    )
    solved_task_cost = solved_task_cost_total
    cost_per_solved_task = (
        solved_task_cost / solved_tasks
        if solved_task_cost is not None
        and solved_tasks > 0
        and solved_tasks_missing_cost == 0
        else None
    )

    wall_values = [trace.record["wall_clock_s"] for trace in ordered]
    usage: dict[str, dict[str, float | int | None]] = {}
    for field in USAGE_FIELDS:
        values = [trace.record[field] for trace in ordered]
        usage[field] = {"total": sum(values), "mean": _mean(values)}
    total_token_values = [
        trace.record["input_tokens"]
        + trace.record["output_tokens"]
        + trace.record["reasoning_tokens"]
        for trace in ordered
    ]
    usage["total_tokens"] = {
        "total": sum(total_token_values),
        "mean": _mean(total_token_values),
    }

    patch_sizes = [trace.record["patch_size"] for trace in ordered]
    empty_patches = sum(1 for trace in ordered if schema.is_empty_patch(trace.record))
    apply_values = [
        trace.record["patch_apply_ok"]
        for trace in ordered
        if trace.record["patch_apply_ok"] is not None
    ]
    apply_failures = sum(1 for value in apply_values if value is False)
    apply_failure_rate = (
        apply_failures / len(apply_values) if apply_values else None
    )

    failure_tag_counts = {tag: 0 for tag in schema.FAILURE_TAGS}
    for trace in ordered:
        for tag in trace.record["failure_tags"]:
            failure_tag_counts[tag] += 1
    failures_untagged = sum(
        1
        for trace in ordered
        if trace.record["verifier_result"] in ("fail", "error", "not_run")
        and not trace.record["failure_tags"]
    )

    health = {
        "tool_errors": _health_metric(ordered, "tool_errors"),
        "context_overflows": _health_metric(
            ordered, "context_overflows", fallback_tag="context-overflow"
        ),
        "provider_retries": _health_metric(ordered, "provider_retries"),
        "loops": _health_metric(ordered, "loops", fallback_tag="loop"),
        "nudges": _health_metric(ordered, "nudges"),
        "test_command_failures": _health_metric(
            ordered, "test_command_failures", fallback_tag="test-broken"
        ),
    }

    trial_counts = [task["trial_count"] for task in per_task]
    source_files: list[str] = []
    for trace in ordered:
        source = str(trace.path)
        if source not in source_files:
            source_files.append(source)

    return {
        "report_version": "1.0",
        "schema_version": schema.SCHEMA_VERSION,
        "source_files": source_files,
        "benchmark": arm["benchmark"],
        "model": arm["model"],
        "model_version": arm["model_version"],
        "keel_target": arm["keel_target"],
        "agent_version": arm["agent_version"],
        "binary_sha256": arm["binary_sha256"],
        "task_count": len(per_task),
        "trial_count": trial_count,
        "trials_per_task": {
            "min": min(trial_counts),
            "max": max(trial_counts),
            "mean": _mean(trial_counts),
        },
        "pass@1": pass_at_1,
        "pass@1_ci95": ci,
        "trial_pass_rate": trial_pass_rate,
        "pass@k": {
            "k": k,
            "value": pass_at_k,
            "tasks_considered": len(pass_k_values),
            "tasks_excluded_insufficient_trials": pass_k_excluded,
            "non_contiguous_tasks": pass_k_non_contiguous,
        },
        "solved_trials": solved_trials,
        "failed_trials": failed_trials,
        "unverified_trials": unverified_trials,
        "non_passing_trials": non_passing_trials,
        "verifier": {
            result: verifier_counts.get(result, 0)
            for result in schema.VERIFIER_RESULTS
        },
        "cost_usd": {
            "total": cost_total,
            "per_solved_trial": cost_per_solved,
            "per_solved_task": cost_per_solved_task,
            "solved_tasks": solved_tasks,
            "solved_task_cost_total": solved_task_cost,
            "solved_tasks_missing_cost": solved_tasks_missing_cost,
            "present_trials": len(cost_values),
            "missing_trials": trial_count - len(cost_values),
        },
        "wall_clock_s": {
            "total": sum(wall_values),
            "mean": _mean(wall_values),
            "p95": percentile(wall_values, 0.95),
        },
        "usage": usage,
        "patch": {
            "size_mean": _mean(patch_sizes),
            "size_p95": percentile(patch_sizes, 0.95),
            "empty_count": empty_patches,
            "empty_rate": empty_patches / trial_count,
            "apply_evaluated": len(apply_values),
            "apply_failures": apply_failures,
            "apply_failure_rate": apply_failure_rate,
            "apply_not_evaluated": trial_count - len(apply_values),
        },
        "harness_health": health,
        "failure_tags": failure_tag_counts,
        "failures_untagged": failures_untagged,
        "stop_reasons": dict(Counter(trace.record["stop_reason"] for trace in ordered)),
        "errors": sum(1 for trace in ordered if trace.record["error"] is not None),
        "session_logs": sum(
            1 for trace in ordered if trace.record["session_log_path"] is not None
        ),
        "per_task": per_task,
    }


def _pct(value: float | None) -> str:
    return "n/a" if value is None else f"{value * 100:.1f}%"


def _number(value: float | None, digits: int = 2) -> str:
    if value is None:
        return "n/a"
    return f"{value:.{digits}f}"


def _integer(value: int | None) -> str:
    return "n/a" if value is None else f"{value:,}"


def _money(value: float | None) -> str:
    if value is None:
        return "n/a"
    if value == 0:
        return "$0.0000"
    if abs(value) < 0.0001:
        return f"${value:.6f}"
    return f"${value:.4f}"


def _count_rate(count: int | None, rate: float | None, noun: str) -> str:
    if count is None:
        return "n/a"
    return f"{count:,} ({_pct(rate)} of {noun})"


def _source_note(metric: dict[str, Any]) -> str:
    source = metric["source"]
    if source.startswith("failure_tags:"):
        return f" (from `{source.split(':', 1)[1]}` tags)"
    if source == "unavailable":
        return " (not recorded)"
    if metric["missing_trials"]:
        return f" ({metric['available_trials']:,} of {metric['available_trials'] + metric['missing_trials']:,} trials reported)"
    return ""


def _solved_task_cost(cost: dict[str, Any]) -> str:
    """One cost-per-solved-task cell with its completeness caveat."""

    solved = cost["solved_tasks"]
    if solved == 0:
        return "n/a (no solved tasks)"
    if cost["per_solved_task"] is None:
        return (
            f"n/a ({cost['solved_tasks_missing_cost']} of {solved} "
            "solved task(s) have missing cost)"
        )
    return (
        f"{_money(cost['per_solved_task'])} "
        f"({solved} solved task(s); "
        f"{cost['solved_tasks_missing_cost']} with missing cost)"
    )


def _md_table(headers: Sequence[str], rows: Iterable[Sequence[str]]) -> list[str]:
    lines = [
        "| " + " | ".join(headers) + " |",
        "| " + " | ".join("---" for _ in headers) + " |",
    ]
    for row in rows:
        lines.append("| " + " | ".join(row) + " |")
    return lines


def render_markdown(report: dict[str, Any], *, trace_index: bool = True) -> str:
    """Render the report as Markdown for a PR, note or scorecard."""

    ci = report["pass@1_ci95"]
    pass_k = report["pass@k"]
    trials = report["trial_count"]
    tasks = report["task_count"]

    lines: list[str] = [
        "# Benchmark trace report",
        "",
        f"- Benchmark: `{report['benchmark']}`",
        f"- Model: `{report['model']}`"
        + (
            f" (`{report['model_version']}`)"
            if report["model_version"]
            else ""
        ),
        f"- Keel target: `{report['keel_target']}`",
        f"- Agent: `{report['agent_version'] or 'unknown'}`"
        + (f", binary `{report['binary_sha256']}`" if report["binary_sha256"] else ""),
        f"- Inputs: {len(report['source_files'])} file(s), "
        f"{tasks:,} task(s), {trials:,} trial(s)",
        f"- Trials per task: min {report['trials_per_task']['min']}, "
        f"max {report['trials_per_task']['max']}, "
        f"mean {_number(report['trials_per_task']['mean'])}",
        "",
        "## Result",
        "",
    ]
    lines += _md_table(
        ("metric", "value"),
        [
            (
                "pass@1",
                f"{_pct(report['pass@1'])} "
                "(per-task mean, so tasks weigh equally)",
            ),
            (
                "Trial pass rate",
                f"{_pct(report['trial_pass_rate'])} "
                f"({report['solved_trials']} / {trials} trials)",
            ),
            (
                "pass@1 95% CI",
                f"{_pct(ci['low'])} to {_pct(ci['high'])} "
                f"(task-level percentile bootstrap, {ci['resamples']:,} "
                f"resamples, seed {ci['seed']})",
            ),
            (
                f"pass@{pass_k['k']}",
                f"{_pct(pass_k['value'])} "
                f"({pass_k['tasks_considered']} task(s) with {pass_k['k']} "
                "contiguous trials starting at 0; "
                f"{pass_k['tasks_excluded_insufficient_trials']} insufficient, "
                f"{pass_k['non_contiguous_tasks']} non-contiguous excluded)",
            ),
            (
                "Unverified trials",
                f"{report['unverified_trials']:,} not_run "
                "(scored as losses in pass@1/pass@k)",
            ),
            ("Tasks", f"{tasks:,}"),
            ("Trials", f"{trials:,}"),
        ],
    )

    lines += ["", "## Cost, time and usage", ""]
    cost = report["cost_usd"]
    usage = report["usage"]
    cost_note = (
        f" ({cost['missing_trials']} trial(s) missing cost)"
        if cost["missing_trials"]
        else ""
    )
    lines += _md_table(
        ("metric", "value"),
        [
            ("Cost total", _money(cost["total"]) + cost_note),
            ("Cost per solved trial", _money(cost["per_solved_trial"])),
            ("Cost per solved task", _solved_task_cost(cost)),
            ("Wall-clock mean", f"{_number(report['wall_clock_s']['mean'])} s"),
            ("Wall-clock p95", f"{_number(report['wall_clock_s']['p95'])} s"),
            (
                "Turns",
                f"{_integer(usage['turns']['total'])} total, "
                f"{_number(usage['turns']['mean'])} mean",
            ),
            (
                "Tool calls",
                f"{_integer(usage['tool_calls']['total'])} total, "
                f"{_number(usage['tool_calls']['mean'])} mean",
            ),
            (
                "Input tokens",
                f"{_integer(usage['input_tokens']['total'])} total, "
                f"{_number(usage['input_tokens']['mean'], 1)} mean",
            ),
            (
                "Output tokens",
                f"{_integer(usage['output_tokens']['total'])} total, "
                f"{_number(usage['output_tokens']['mean'], 1)} mean",
            ),
            (
                "Reasoning tokens",
                f"{_integer(usage['reasoning_tokens']['total'])} total, "
                f"{_number(usage['reasoning_tokens']['mean'], 1)} mean",
            ),
            (
                "Total tokens",
                f"{_integer(usage['total_tokens']['total'])} total, "
                f"{_number(usage['total_tokens']['mean'], 1)} mean",
            ),
        ],
    )

    patch = report["patch"]
    health = report["harness_health"]
    verifier = report["verifier"]
    lines += ["", "## Patch, verifier and harness health", ""]
    lines += _md_table(
        ("metric", "value"),
        [
            (
                "Patch size",
                f"{_number(patch['size_mean'], 1)} bytes mean, "
                f"{_number(patch['size_p95'], 1)} bytes p95",
            ),
            (
                "Empty patches",
                _count_rate(patch["empty_count"], patch["empty_rate"], "trials"),
            ),
            (
                "Patch-apply failures",
                (
                    "n/a (not evaluated)"
                    if patch["apply_evaluated"] == 0
                    else f"{patch['apply_failures']:,} "
                    f"({_pct(patch['apply_failure_rate'])} of "
                    f"{patch['apply_evaluated']:,} evaluated; "
                    f"{patch['apply_not_evaluated']:,} not evaluated)"
                ),
            ),
            (
                "Verifier",
                f"{verifier['pass']:,} pass, {verifier['fail']:,} fail, "
                f"{verifier['error']:,} error, {verifier['not_run']:,} not run",
            ),
            (
                "Trace completeness",
                f"{report['errors']:,} error(s); "
                f"{report['session_logs']:,} session log(s)",
            ),
            (
                "Test-command failures",
                (
                    _integer(health["test_command_failures"]["count"])
                    + _source_note(health["test_command_failures"])
                ),
            ),
            (
                "Tool errors",
                _integer(health["tool_errors"]["count"])
                + _source_note(health["tool_errors"]),
            ),
            (
                "Context overflows",
                _integer(health["context_overflows"]["count"])
                + _source_note(health["context_overflows"]),
            ),
            (
                "Provider retries",
                _integer(health["provider_retries"]["count"])
                + _source_note(health["provider_retries"]),
            ),
            (
                "Loops",
                _integer(health["loops"]["count"]) + _source_note(health["loops"]),
            ),
            (
                "Nudges",
                _integer(health["nudges"]["count"]) + _source_note(health["nudges"]),
            ),
        ],
    )

    failed = report["failed_trials"]
    lines += ["", "## Failure tags", ""]
    lines.append(
        f"{failed:,} failed trial(s) (verifier fail or error); "
        f"{report['unverified_trials']:,} unverified trial(s) "
        "(not_run, scored as losses); "
        f"{report['failures_untagged']:,} non-passing trial(s) carry no tag."
    )
    lines.append("")
    rows = []
    for tag in schema.FAILURE_TAGS:
        count = report["failure_tags"][tag]
        share = _pct(count / trials) if trials else "n/a"
        rows.append((f"`{tag}`", f"{count:,}", share))
    lines += _md_table(("tag", "count", "share of trials"), rows)

    lines += ["", "## Tasks", ""]
    task_rows = []
    for task in report["per_task"]:
        tags = ", ".join(f"`{tag}`" for tag in task["failure_tags"]) or "-"
        pass_k_cell = (
            "n/a" if task["pass@k"] is None else ("yes" if task["pass@k"] else "no")
        )
        task_rows.append(
            (
                f"`{task['task_id']}`",
                f"{task['trial_count']}",
                f"{task['passes']}",
                _pct(task["pass@1"]),
                pass_k_cell,
                tags,
                _money(task["cost_usd"])
                + (
                    f" (+{task['cost_missing_trials']} missing)"
                    if task["cost_missing_trials"]
                    else ""
                ),
                f"{_number(task['wall_clock_s_mean'])} s",
            )
        )
    lines += _md_table(
        (
            "task",
            "trials",
            "passes",
            "pass@1",
            f"pass@{pass_k['k']}",
            "failure tags",
            "cost",
            "wall mean",
        ),
        task_rows,
    )

    if trace_index:
        lines += ["", "## Non-passing trials", ""]
        entries: list[str] = []
        for task in report["per_task"]:
            for record in task["records"]:
                if record["verifier_result"] == "pass":
                    continue
                tags = ", ".join(f"`{tag}`" for tag in record["failure_tags"])
                if not tags:
                    tags = "untagged"
                entries.append(
                    f"- `{task['task_id']}` trial {record['trial']}: "
                    f"{record['verifier_result']} [{tags}] - `{record['source']}`"
                )
        lines += entries or ["No non-passing trials."]

    return "\n".join(lines).rstrip() + "\n"


def render_text(report: dict[str, Any], *, trace_index: bool = True) -> str:
    """Plain-text form for terminals and logs."""

    ci = report["pass@1_ci95"]
    pass_k = report["pass@k"]
    usage = report["usage"]
    patch = report["patch"]
    health = report["harness_health"]
    verifier = report["verifier"]
    lines = [
        "Benchmark trace report",
        "======================",
        f"benchmark: {report['benchmark']}",
        f"model: {report['model']}",
        f"model_version: {report['model_version'] or 'n/a'}",
        f"keel_target: {report['keel_target']}",
        f"agent_version: {report['agent_version'] or 'n/a'}",
        f"binary_sha256: {report['binary_sha256'] or 'n/a'}",
        f"tasks: {report['task_count']}",
        f"trials: {report['trial_count']}",
        "",
        "Result",
        "------",
        f"pass@1: {_pct(report['pass@1'])} (per-task mean; tasks weigh equally)",
        f"trial pass rate: {_pct(report['trial_pass_rate'])} "
        f"({report['solved_trials']}/{report['trial_count']} trials)",
        f"pass@1 95% CI: {_pct(ci['low'])} to {_pct(ci['high'])} "
        f"(task bootstrap, {ci['resamples']} resamples, seed {ci['seed']})",
        f"pass@{pass_k['k']}: {_pct(pass_k['value'])} "
        f"({pass_k['tasks_considered']} task(s) with {pass_k['k']} contiguous "
        f"trials from 0; {pass_k['tasks_excluded_insufficient_trials']} "
        f"insufficient, {pass_k['non_contiguous_tasks']} non-contiguous excluded)",
        f"unverified: {report['unverified_trials']} not_run "
        "(scored as losses in pass@1/pass@k)",
        "",
        "Cost, time and usage",
        "--------------------",
        f"cost total: {_money(report['cost_usd']['total'])}",
        f"cost per solved trial: {_money(report['cost_usd']['per_solved_trial'])}",
        f"cost per solved task: {_solved_task_cost(report['cost_usd'])}",
        f"wall-clock mean: {_number(report['wall_clock_s']['mean'])} s",
        f"wall-clock p95: {_number(report['wall_clock_s']['p95'])} s",
        f"turns: {_integer(usage['turns']['total'])} total, {_number(usage['turns']['mean'])} mean",
        f"tool calls: {_integer(usage['tool_calls']['total'])} total, {_number(usage['tool_calls']['mean'])} mean",
        f"input tokens: {_integer(usage['input_tokens']['total'])} total, {_number(usage['input_tokens']['mean'], 1)} mean",
        f"output tokens: {_integer(usage['output_tokens']['total'])} total, {_number(usage['output_tokens']['mean'], 1)} mean",
        f"reasoning tokens: {_integer(usage['reasoning_tokens']['total'])} total, {_number(usage['reasoning_tokens']['mean'], 1)} mean",
        f"total tokens: {_integer(usage['total_tokens']['total'])} total, {_number(usage['total_tokens']['mean'], 1)} mean",
        "",
        "Patch, verifier and harness health",
        "----------------------------------",
        f"patch size: {_number(patch['size_mean'], 1)} bytes mean, {_number(patch['size_p95'], 1)} bytes p95",
        f"empty patches: {patch['empty_count']}/{report['trial_count']} ({_pct(patch['empty_rate'])})",
        (
            "patch-apply failures: n/a (not evaluated)"
            if patch["apply_evaluated"] == 0
            else f"patch-apply failures: {patch['apply_failures']}/{patch['apply_evaluated']} evaluated ({_pct(patch['apply_failure_rate'])})"
        ),
        f"verifier: {verifier['pass']} pass, {verifier['fail']} fail, {verifier['error']} error, {verifier['not_run']} not run",
        f"trace completeness: {report['errors']} error(s), {report['session_logs']} session log(s)",
        "test-command failures: "
        + _integer(health["test_command_failures"]["count"])
        + _source_note(health["test_command_failures"]),
        "tool errors: "
        + _integer(health["tool_errors"]["count"])
        + _source_note(health["tool_errors"]),
        "context overflows: "
        + _integer(health["context_overflows"]["count"])
        + _source_note(health["context_overflows"]),
        "provider retries: "
        + _integer(health["provider_retries"]["count"])
        + _source_note(health["provider_retries"]),
        "loops: "
        + _integer(health["loops"]["count"])
        + _source_note(health["loops"]),
        "nudges: "
        + _integer(health["nudges"]["count"])
        + _source_note(health["nudges"]),
        "",
        "Failure tags",
        "------------",
    ]
    failed = report["failed_trials"]
    for tag in schema.FAILURE_TAGS:
        count = report["failure_tags"][tag]
        lines.append(f"{tag}: {count}")
    lines.append(
        f"failed: {failed}; unverified: {report['unverified_trials']} "
        f"(not_run, scored as losses); "
        f"untagged failures: {report['failures_untagged']}"
    )

    if trace_index:
        lines += ["", "Non-passing trials", "------------------"]
        entries: list[str] = []
        for task in report["per_task"]:
            for record in task["records"]:
                if record["verifier_result"] == "pass":
                    continue
                tags = ", ".join(record["failure_tags"]) or "untagged"
                entries.append(
                    f"{task['task_id']} trial {record['trial']}: "
                    f"{record['verifier_result']} [{tags}] {record['source']}"
                )
        lines += entries or ["No non-passing trials."]

    return "\n".join(lines).rstrip() + "\n"


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Aggregate one or more benchmark trace JSONL files into pass@1, "
            "pass@k, a task-level bootstrap interval and harness metrics."
        )
    )
    parser.add_argument("traces", nargs="+", metavar="TRACE.jsonl")
    output = parser.add_mutually_exclusive_group()
    output.add_argument(
        "--json", action="store_true", help="emit the full report as JSON"
    )
    output.add_argument(
        "--text", action="store_true", help="emit plain text instead of Markdown"
    )
    parser.add_argument(
        "--k",
        type=int,
        default=3,
        metavar="K",
        help="pass@k trial count (default: 3)",
    )
    parser.add_argument(
        "--bootstrap",
        type=int,
        default=2000,
        metavar="N",
        help="bootstrap resamples for the 95%% interval (default: 2000)",
    )
    parser.add_argument(
        "--seed",
        type=int,
        default=0,
        help="bootstrap seed; the same seed gives the same interval (default: 0)",
    )
    parser.add_argument(
        "--no-trace-index",
        action="store_true",
        help="omit the non-passing trial index from Markdown/text output",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    loaded: list[schema.LoadedTrace] = []
    for raw_path in args.traces:
        try:
            loaded.extend(schema.read_trace_file(raw_path))
        except OSError as exc:
            print(
                f"report.py: error: cannot read {raw_path}: "
                f"{exc.strerror or exc}",
                file=sys.stderr,
            )
            return 2
        except schema.TraceValidationError as exc:
            print(f"report.py: error: {exc}", file=sys.stderr)
            return 2

    try:
        report = compute_report(
            loaded,
            k=args.k,
            bootstrap=args.bootstrap,
            seed=args.seed,
        )
    except ReportError as exc:
        print(f"report.py: error: {exc}", file=sys.stderr)
        return 2

    if args.json:
        print(json.dumps(report, indent=2, ensure_ascii=False))
    elif args.text:
        print(render_text(report, trace_index=not args.no_trace_index), end="")
    else:
        print(render_markdown(report, trace_index=not args.no_trace_index), end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())


__all__ = [
    "ARM_FIELDS",
    "ReportError",
    "bootstrap_interval",
    "build_parser",
    "compute_report",
    "main",
    "percentile",
    "render_markdown",
    "render_text",
]
