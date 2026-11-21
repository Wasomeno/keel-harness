"""Tests for the fixed benchmark subsets and their documented selection rules."""

from __future__ import annotations

import hashlib
import re
import unittest
from pathlib import Path

SUBSETS = Path(__file__).resolve().parents[2] / "benchmarks" / "subsets"
SWE_ID = re.compile(r"^[A-Za-z0-9_.-]+__[A-Za-z0-9_.-]+-\d+$")


def read_ids(path: Path) -> list[str]:
    return [
        line.strip()
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]


class SubsetTests(unittest.TestCase):
    def test_terminal_bench_manifest_is_the_full_89(self) -> None:
        manifest = read_ids(SUBSETS / "terminal-bench-2.0-tasks.txt")
        self.assertEqual(len(manifest), 89)
        self.assertEqual(len(set(manifest)), 89)
        self.assertTrue(all(item.startswith("terminal-bench/") for item in manifest))

    def test_iteration_subset_is_fixed_and_real(self) -> None:
        manifest = set(read_ids(SUBSETS / "terminal-bench-2.0-tasks.txt"))
        iteration = read_ids(SUBSETS / "iteration-terminal-bench.txt")
        self.assertEqual(len(iteration), 10)
        self.assertEqual(len(set(iteration)), 10)
        self.assertEqual(set(iteration) - manifest, set())

    def test_canary_subset_is_at_least_twenty_percent_and_disjoint(self) -> None:
        manifest = read_ids(SUBSETS / "terminal-bench-2.0-tasks.txt")
        iteration = read_ids(SUBSETS / "iteration-terminal-bench.txt")
        canary = read_ids(SUBSETS / "canary-terminal-bench.txt")
        self.assertGreaterEqual(len(canary), 0.20 * len(manifest))
        self.assertEqual(len(canary), 20)
        self.assertEqual(len(set(canary)), len(canary))
        self.assertEqual(set(canary) - set(manifest), set())
        self.assertEqual(set(canary) & set(iteration), set())

    def test_canary_matches_the_documented_hash_rule(self) -> None:
        manifest = read_ids(SUBSETS / "terminal-bench-2.0-tasks.txt")
        iteration = set(read_ids(SUBSETS / "iteration-terminal-bench.txt"))
        expected = sorted(
            set(manifest) - iteration,
            key=lambda task: hashlib.sha256(
                ("m12-canary-v1:" + task).encode("utf-8")
            ).hexdigest(),
        )[:20]
        self.assertEqual(read_ids(SUBSETS / "canary-terminal-bench.txt"), expected)

    def test_swebench_bringup_subset_shape(self) -> None:
        instances = read_ids(SUBSETS / "bringup-swebench-lite.txt")
        self.assertEqual(len(instances), 10)
        self.assertEqual(len(set(instances)), 10)
        for instance in instances:
            with self.subTest(instance=instance):
                self.assertRegex(instance, SWE_ID)
                self.assertIn("__", instance)
                self.assertRegex(instance.rsplit("-", 1)[1], r"^\d+$")

    def test_swebench_pro_placeholder_exists(self) -> None:
        placeholder = SUBSETS / "swebench-pro-v2.placeholder.md"
        self.assertTrue(placeholder.exists())
        text = placeholder.read_text(encoding="utf-8")
        self.assertIn("PLACEHOLDER", text)
        self.assertIn("SWE-bench Pro V2", text)
        self.assertIn("not available", text.lower())


if __name__ == "__main__":
    unittest.main()
