"""Unit tests for benchmarks/swebench/test_discovery.py.

The tests are pure Python: a file mapping stands in for the container, and a
fake command runner stands in for Docker.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "benchmarks" / "swebench"))

import test_discovery as discovery  # noqa: E402


class DiscoveryTests(unittest.TestCase):
    def test_pytest_project_from_pyproject(self) -> None:
        hint = discovery.discover_test_command(
            {
                "pyproject.toml": "[tool.pytest.ini_options]\naddopts = '-q'\n",
                "src/app.py": "",
            }
        )
        self.assertIsNotNone(hint)
        self.assertEqual(hint.command, "python -m pytest")
        self.assertEqual(hint.source, "pyproject.toml")

    def test_pytest_project_from_pytest_ini(self) -> None:
        hint = discovery.discover_test_command({"pytest.ini": "[pytest]\n"})
        self.assertEqual(hint.command, "python -m pytest")

    def test_makefile_test_target(self) -> None:
        hint = discovery.discover_test_command(
            {
                "Makefile": "build:\n\tcc -o app app.c\n\ntest:\n\tpytest -q\n",
            }
        )
        self.assertIsNotNone(hint)
        self.assertEqual(hint.command, "make test")
        self.assertEqual(hint.source, "Makefile test target")

    def test_makefile_check_target_when_test_is_absent(self) -> None:
        hint = discovery.discover_test_command({"GNUmakefile": "check: lint\n\ttox\n"})
        self.assertEqual(hint.command, "make check")

    def test_github_actions_workflow(self) -> None:
        workflow = """\
name: ci
on: [push]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Unit tests
        run: python -m pytest -q
"""
        hint = discovery.discover_test_command({".github/workflows/ci.yml": workflow})
        self.assertIsNotNone(hint)
        self.assertEqual(hint.command, "python -m pytest -q")
        self.assertIn(".github/workflows/ci.yml", hint.source)

    def test_github_actions_multiline_run_block(self) -> None:
        workflow = """\
jobs:
  test:
    steps:
      - run: |
          pip install -e .
          cargo test --workspace
"""
        hint = discovery.discover_test_command({".github/workflows/ci.yaml": workflow})
        self.assertEqual(hint.command, "cargo test --workspace")

    def test_readme_example_when_no_config_file_exists(self) -> None:
        readme = """\
# Project

Run the suite with `tox -e py311` before opening a pull request.
"""
        hint = discovery.discover_test_command({"README.md": readme})
        self.assertIsNotNone(hint)
        self.assertEqual(hint.command, "tox -e py311")
        self.assertEqual(hint.source, "README.md example")

    def test_readme_fenced_example(self) -> None:
        readme = """\
# Project

```sh
$ make test
```
"""
        hint = discovery.discover_test_command({"README.md": readme})
        self.assertEqual(hint.command, "make test")

    def test_package_json_uses_the_lockfile_package_manager(self) -> None:
        package = '{"scripts": {"test": "jest --ci"}}'
        for lockfile, expected in (
            ("pnpm-lock.yaml", "pnpm test"),
            ("yarn.lock", "yarn test"),
            ("bun.lockb", "bun test"),
            ("package-lock.json", "npm test"),
        ):
            with self.subTest(lockfile=lockfile):
                hint = discovery.discover_test_command(
                    {"package.json": package}, [lockfile]
                )
                self.assertEqual(hint.command, expected)

    def test_cargo_and_go_conventions(self) -> None:
        cargo = discovery.discover_test_command({"Cargo.toml": "[package]\n"})
        self.assertEqual(cargo.command, "cargo test")
        go = discovery.discover_test_command({"go.mod": "module example.com/app\n"})
        self.assertEqual(go.command, "go test ./...")

    def test_layout_convention_after_config(self) -> None:
        hint = discovery.discover_test_command(
            {},
            ["src/app.py", "tests/test_app.py", "tests/helpers.py"],
        )
        self.assertEqual(hint.command, "python -m pytest")
        self.assertEqual(hint.source, "repository layout")

    def test_ci_wins_over_makefile_and_config(self) -> None:
        hint = discovery.discover_test_command(
            {
                ".github/workflows/ci.yml": "run: pytest -q\n",
                "Makefile": "test:\n\tpytest -q\n",
                "pyproject.toml": "[tool.pytest.ini_options]\n",
            }
        )
        self.assertEqual(hint.command, "pytest -q")
        self.assertIn("ci.yml", hint.source)

    def test_no_evidence_means_no_hint(self) -> None:
        self.assertIsNone(discovery.discover_test_command({}, ["src/app.py"]))
        self.assertIsNone(
            discovery.discover_test_command({"README.md": "No tests yet.\n"})
        )

    def test_malformed_files_do_not_crash_discovery(self) -> None:
        hint = discovery.discover_test_command(
            {
                "package.json": "{not json",
                "pyproject.toml": "[tool.pytest.ini_options]\n",
            }
        )
        self.assertEqual(hint.command, "python -m pytest")
        self.assertIsNone(
            discovery.discover_test_command({"package.json": "[]"})
        )

    def test_test_commands_are_recognised(self) -> None:
        for command in (
            "pytest -q",
            "python -m pytest tests/test_app.py",
            "python3 -m unittest discover -s tests",
            "uv run pytest",
            "poetry run pytest",
            "npx vitest run",
            "cargo test --workspace",
            "cargo nextest run",
            "go test ./...",
            "npm test",
            "npm run test -- --watch=false",
            "pnpm run test",
            "yarn test",
            "make test",
            "make -C repo check",
            "mvn test",
            "./gradlew test",
            "ctest",
            "dotnet test",
            "tox -e py311",
            "./tests/run_tests.sh",
            "timeout 60 python3 -m unittest discover",
            "cd repo && pytest",
            "FOO=1 pytest",
        ):
            with self.subTest(command=command):
                self.assertTrue(discovery.is_test_command(command))

    def test_ordinary_commands_are_not_tests(self) -> None:
        for command in (
            "grep -rn pytest src",
            "cat pytest.ini",
            "python -m pip install pytest",
            "cargo build",
            "make build",
            "echo cargo test",
            "npm install",
            "ls tests",
            "cd tests",
        ):
            with self.subTest(command=command):
                self.assertFalse(discovery.is_test_command(command))


class BundleTests(unittest.TestCase):
    def test_bundle_is_split_by_marker(self) -> None:
        text = (
            "\n===KEEL-DISCOVERY:Makefile===\n"
            "test:\n\tpytest\n"
            "===KEEL-DISCOVERY:.github/workflows/ci.yml===\n"
            "run: cargo test\n"
        )
        files = discovery.parse_discovery_bundle(text)
        self.assertEqual(set(files), {"Makefile", ".github/workflows/ci.yml"})
        self.assertIn("pytest", files["Makefile"])

    def test_bundle_ignores_text_before_the_first_marker(self) -> None:
        self.assertEqual(
            discovery.parse_discovery_bundle("noise\n===KEEL-DISCOVERY:x===\n"),
            {},
        )

    def test_container_helper_reads_files_and_paths(self) -> None:
        calls = []

        class Result:
            def __init__(self, returncode: int, stdout: str = "") -> None:
                self.returncode = returncode
                self.stdout = stdout

        def exec_in_container(argv, **kwargs):
            calls.append((argv, kwargs))
            if argv[:2] == ["sh", "-c"]:
                return Result(
                    0,
                    "===KEEL-DISCOVERY:pyproject.toml===\n"
                    "[tool.pytest.ini_options]\n",
                )
            if argv[:3] == ["git", "-C", "/testbed"]:
                return Result(0, "tests/test_app.py\nsrc/app.py\n")
            return Result(1)

        hint = discovery.discover_test_command_in_container(exec_in_container)
        self.assertEqual(hint.command, "python -m pytest")
        self.assertEqual(hint.source, "pyproject.toml")
        self.assertEqual(calls[0][1]["workdir"], "/testbed")
        self.assertEqual(calls[1][0], ["git", "-C", "/testbed", "ls-files"])

    def test_a_failed_discovery_command_yields_no_hint(self) -> None:
        class Result:
            returncode = 1
            stdout = ""

        hint = discovery.discover_test_command_in_container(lambda *args, **kwargs: Result())
        self.assertIsNone(hint)


if __name__ == "__main__":
    unittest.main()
