"""Discover a repository's own visible test command.

The SWE-bench runner reads a small, bounded set of files from the container and
asks this module for a likely test command. The result is a *hint* for the
prompt: it never replaces the repository's command and it is never executed by
the host.

The module is pure except for :func:`discover_test_command_in_container`, which
takes an injected command runner so unit tests need no Docker.
"""

from __future__ import annotations

import json
import re
import shlex
from collections.abc import Callable, Iterable, Mapping
from dataclasses import dataclass
from pathlib import PurePosixPath
from typing import Any

CONTAINER_REPO = "/testbed"

MAX_DISCOVERY_FILE_BYTES = 32_768
MAX_DISCOVERY_PATHS = 5_000

BUNDLE_MARKER = "===KEEL-DISCOVERY:"
BUNDLE_RE = re.compile(r"^===KEEL-DISCOVERY:(.+)===\s*$")

# One bounded shell pass reads the files that usually name the test command.
# Missing files are skipped; `head` keeps a huge README or Makefile bounded.
DISCOVERY_SCRIPT = r"""
for path in README.md README.rst README.txt README Makefile makefile GNUmakefile \
            pyproject.toml pytest.ini tox.ini setup.cfg package.json Cargo.toml go.mod; do
  [ -f "$path" ] || continue
  printf '\n===KEEL-DISCOVERY:%s===\n' "$path"
  head -c 32768 "$path" || true
done
for path in .github/workflows/*.yml .github/workflows/*.yaml; do
  [ -f "$path" ] || continue
  printf '\n===KEEL-DISCOVERY:%s===\n' "$path"
  head -c 32768 "$path" || true
done
"""

_TEST_RUNNERS = {
    "pytest",
    "py.test",
    "tox",
    "nox",
    "ctest",
    "rspec",
    "phpunit",
    "jest",
    "vitest",
    "mocha",
}

_PYTHON_RE = re.compile(r"^python(\d+(\.\d+)?)?$")

_TEST_SCRIPTS = {
    "test",
    "tests",
    "run_tests",
    "run-tests",
    "test.sh",
    "run_tests.sh",
}

_ENV_ASSIGNMENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")


@dataclass(frozen=True)
class TestCommand:
    """A hint with its provenance. `evidence` is one short line."""

    command: str
    source: str
    evidence: str = ""


def parse_discovery_bundle(text: str) -> dict[str, str]:
    """Split the bounded `sh` output into `{path: content}`."""
    files: dict[str, str] = {}
    current: str | None = None
    chunks: list[str] = []
    for line in text.splitlines(keepends=True):
        match = BUNDLE_RE.match(line)
        if match:
            if current is not None:
                files[current] = "".join(chunks)[:MAX_DISCOVERY_FILE_BYTES]
            current = match.group(1).strip()
            chunks = []
        elif current is not None:
            chunks.append(line)
    if current is not None:
        files[current] = "".join(chunks)[:MAX_DISCOVERY_FILE_BYTES]
    return {path: content for path, content in files.items() if content.strip()}


def discover_test_command(
    files: Mapping[str, str],
    paths: Iterable[str] = (),
) -> TestCommand | None:
    """Pick the most specific documented test command.

    Precedence: CI workflow, Makefile target, README example, project config,
    then a common layout convention. The runner passes the result to the prompt
    as a hint, never as a hardcoded answer.
    """
    files = {path.replace("\\", "/"): text for path, text in files.items()}
    path_list = [path.replace("\\", "/") for path in paths]

    for path in sorted(files):
        if _is_workflow(path):
            command = workflow_test_command(files[path])
            if command:
                return TestCommand(
                    command,
                    f"{path} run step",
                    _first_line(files[path], command),
                )

    for name in ("Makefile", "makefile", "GNUmakefile"):
        text = files.get(name)
        if text is None:
            continue
        target = makefile_test_target(text)
        if target:
            return TestCommand(f"make {target}", f"{name} {target} target")

    for name in ("README.md", "README.rst", "README.txt", "README"):
        text = files.get(name)
        if text is None:
            continue
        for command in readme_test_commands(text):
            return TestCommand(command, f"{name} example")

    for name, builder in (
        ("pyproject.toml", _pyproject_command),
        ("pytest.ini", lambda _text: "python -m pytest"),
        ("tox.ini", _tox_command),
        ("setup.cfg", _setup_cfg_command),
        ("package.json", lambda text: _package_command(text, path_list)),
        ("Cargo.toml", lambda _text: "cargo test"),
        ("go.mod", lambda _text: "go test ./..."),
    ):
        text = files.get(name)
        if text is None:
            continue
        command = builder(text)
        if command:
            return TestCommand(command, name)

    convention = _convention_command(path_list)
    if convention:
        return TestCommand(convention, "repository layout")
    return None


def workflow_test_command(text: str) -> str | None:
    """Find the first `run:` step that invokes a test runner."""
    lines = text.splitlines()
    for index, line in enumerate(lines):
        match = re.match(r"^(\s*)(?:-\s*)?run:\s*(.*)$", line)
        if not match:
            continue
        base_indent = len(match.group(1))
        value = _strip_yaml_comment(match.group(2).strip())
        if value in {"|", ">", "|-", ">-", "|+", ">+"}:
            block: list[str] = []
            for follow in lines[index + 1 :]:
                if not follow.strip():
                    block.append("")
                    continue
                indent = len(follow) - len(follow.lstrip())
                if indent <= base_indent:
                    break
                block.append(follow.strip())
            for command in block:
                candidate = clean_command(command)
                if candidate and is_test_command(candidate):
                    return candidate
            continue
        candidate = clean_command(_unquote(value))
        if candidate and is_test_command(candidate):
            return candidate
    return None


def makefile_test_target(text: str) -> str | None:
    """Return `test`, `tests` or `check` when the Makefile defines it."""
    targets = set(re.findall(r"(?m)^([A-Za-z0-9_.-]+)\s*:(?!=)", text))
    for target in ("test", "tests", "check"):
        if target in targets:
            return target
    return None


def readme_test_commands(text: str) -> list[str]:
    """Collect commands from fenced blocks and inline code spans, in order."""
    commands: list[str] = []
    in_fence = False
    for raw in text.splitlines():
        stripped = raw.strip()
        if stripped.startswith("```") or stripped.startswith("~~~"):
            in_fence = not in_fence
            continue
        if in_fence:
            command = clean_command(stripped)
            if command and is_test_command(command):
                commands.append(command)
        for match in re.finditer(r"`([^`\n]+)`", raw):
            command = clean_command(match.group(1))
            if command and is_test_command(command):
                commands.append(command)
    return commands


def is_test_command(command: str) -> bool:
    """Whether a shell command invokes a recognisable visible test runner."""
    for segment in re.split(r"&&|\|\||;|\n|\|", command):
        try:
            tokens = shlex.split(segment, posix=True)
        except ValueError:
            tokens = segment.split()
        if _segment_is_test(tokens):
            return True
    return False


def clean_command(text: str) -> str | None:
    """Strip shell prompts and YAML list markers from one candidate line."""
    text = text.strip()
    if not text or text.startswith("#"):
        return None
    while text.startswith(("$ ", "> ", "- ")):
        text = text[2:].strip()
    text = text.rstrip("\\").strip()
    if not text or "<" in text or ">" in text:
        return None
    return text


def discover_test_command_in_container(
    exec_in_container: Callable[..., Any],
    *,
    repo: str = CONTAINER_REPO,
) -> TestCommand | None:
    """Read the bounded discovery files and file list from a container.

    Discovery is advisory: a failed command or an unreadable file returns
    `None` (or a partial answer) and never fails the trial.
    """
    files: dict[str, str] = {}
    try:
        result = exec_in_container(
            ["sh", "-c", DISCOVERY_SCRIPT], timeout=60, workdir=repo
        )
        if getattr(result, "returncode", 1) == 0:
            files = parse_discovery_bundle(getattr(result, "stdout", "") or "")
    except Exception:  # noqa: BLE001 - discovery must not fail the trial
        files = {}

    paths: list[str] = []
    try:
        listing = exec_in_container(
            ["git", "-C", repo, "ls-files"], timeout=60
        )
        if getattr(listing, "returncode", 1) == 0:
            paths = (getattr(listing, "stdout", "") or "").splitlines()[
                :MAX_DISCOVERY_PATHS
            ]
    except Exception:  # noqa: BLE001 - discovery must not fail the trial
        paths = []

    return discover_test_command(files, paths)


def _is_workflow(path: str) -> bool:
    return path.startswith(".github/workflows/") and path.endswith(
        (".yml", ".yaml")
    )


def _pyproject_command(text: str) -> str | None:
    if "[tool.pytest" in text or re.search(r"(?m)^\[tool:pytest\]", text):
        return "python -m pytest"
    return None


def _tox_command(text: str) -> str | None:
    if "[pytest]" in text:
        return "python -m pytest"
    return "tox"


def _setup_cfg_command(text: str) -> str | None:
    return "python -m pytest" if "[tool:pytest]" in text else None


def _package_command(text: str, paths: Iterable[str]) -> str | None:
    try:
        package = json.loads(text)
    except json.JSONDecodeError:
        return None
    if not isinstance(package, dict):
        return None
    scripts = package.get("scripts")
    if not isinstance(scripts, dict) or not scripts.get("test"):
        return None
    path_set = set(paths)
    if "pnpm-lock.yaml" in path_set:
        return "pnpm test"
    if "yarn.lock" in path_set:
        return "yarn test"
    if "bun.lockb" in path_set or "bun.lock" in path_set:
        return "bun test"
    return "npm test"


def _convention_command(paths: Iterable[str]) -> str | None:
    path_list = list(paths)
    if any(
        re.search(r"(^|/)tests?/.+\.py$", path)
        or re.search(r"(^|/)test_[^/]+\.py$", path)
        for path in path_list
    ):
        return "python -m pytest"
    if any(path.endswith((".test.js", ".spec.js", ".test.ts", ".spec.ts")) for path in path_list):
        return "npm test"
    if any(path.endswith(".rs") for path in path_list):
        return "cargo test"
    return None


def _segment_is_test(tokens: list[str]) -> bool:
    tokens = _strip_prefixes(tokens)
    if not tokens:
        return False
    first = tokens[0]
    name = PurePosixPath(first).name
    rest = tokens[1:]

    if name in _TEST_RUNNERS:
        return True
    if _PYTHON_RE.match(name):
        if "-m" in rest:
            position = rest.index("-m")
            if position + 1 < len(rest) and rest[position + 1] in {
                "pytest",
                "unittest",
                "tox",
                "nox",
            }:
                return True
        return False
    wrapped = _wrapper_remainder(name, rest)
    if wrapped is not None:
        return is_test_command(wrapped)

    if name == "cargo":
        return bool(rest) and rest[0] in {"test", "nextest"}
    if name == "go":
        return bool(rest) and rest[0] == "test"
    if name in {"npm", "pnpm", "yarn", "bun"}:
        return (bool(rest) and rest[0] == "test") or (
            len(rest) >= 2 and rest[0] == "run" and rest[1] == "test"
        )
    if name == "make":
        return any(token in {"test", "tests", "check"} for token in rest)
    if name in {"mvn", "mvnw"}:
        return any(token in {"test", "verify"} for token in rest)
    if name in {"gradle", "gradlew"}:
        return any(token in {"test", "check"} for token in rest)
    if name in {"dotnet", "swift", "mix"}:
        return bool(rest) and rest[0] == "test"
    return name in _TEST_SCRIPTS


def _strip_prefixes(tokens: list[str]) -> list[str]:
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if _ENV_ASSIGNMENT_RE.match(token) and not token.startswith("-"):
            index += 1
        elif token in {"sudo", "time", "command", "exec", "builtin"}:
            index += 1
        elif token == "env":
            index += 1
            while index < len(tokens) and (
                tokens[index].startswith("-") or _ENV_ASSIGNMENT_RE.match(tokens[index])
            ):
                index += 1
        elif token == "timeout":
            index += 1
            while index < len(tokens) and tokens[index].startswith("-"):
                index += 1
            if index < len(tokens) and tokens[index].isdigit():
                index += 1
        elif token in {"cd", "pushd", "popd"}:
            return []
        else:
            break
    return tokens[index:]


def _wrapper_remainder(name: str, rest: list[str]) -> str | None:
    if name in {"uv", "poetry", "pdm", "hatch", "pipenv", "rye"}:
        if rest[:1] == ["run"]:
            return " ".join(shlex.quote(token) for token in rest[1:])
        return None
    if name == "bundle" and rest[:1] == ["exec"]:
        return " ".join(shlex.quote(token) for token in rest[1:])
    if name in {"npx", "pnpx"}:
        return " ".join(shlex.quote(token) for token in rest)
    return None


def _unquote(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {"'", '"'}:
        return value[1:-1]
    return value


def _strip_yaml_comment(value: str) -> str:
    if " #" in value:
        return value.split(" #", 1)[0].strip()
    return value


def _first_line(text: str, needle: str) -> str:
    for line in text.splitlines():
        if needle in line:
            return line.strip()[:160]
    return ""
