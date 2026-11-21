"""Unit tests for the Harbor keel adapter without Harbor or Docker installed.

The adapter imports Harbor at module load, so the tests install small stub
modules first. The fake environment records uploads and exec calls and returns
canned keel output; no process on the host is started.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import os
import shlex
import sys
import tempfile
import types
import unittest
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path, PurePosixPath
from types import SimpleNamespace
from typing import Any
from unittest.mock import patch


REPO_ROOT = Path(__file__).resolve().parents[2]
HARBOR_ADAPTER_DIR = REPO_ROOT / "benchmarks" / "harbor"

TRACE_FIELDS = (
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


# ---------------------------------------------------------------------------
# Harbor stubs
# ---------------------------------------------------------------------------


class StubAgentCapabilities:
    def __init__(self, **kwargs: Any) -> None:
        self.__dict__.update(kwargs)


class StubProviderAccess:
    def __init__(self, **kwargs: Any) -> None:
        self.__dict__.update(kwargs)


class StubModelConnectionSpec:
    def __init__(self, **kwargs: Any) -> None:
        self.__dict__.update(kwargs)


class StubModelApiFormat(StrEnum):
    OPENAI_CHAT_COMPLETIONS = "openai_chat_completions"
    OPENAI_RESPONSES = "openai_responses"
    ANTHROPIC_MESSAGES = "anthropic_messages"


class StubAgentContext:
    def __init__(self) -> None:
        self.n_input_tokens: int | None = None
        self.n_cache_tokens: int | None = None
        self.n_output_tokens: int | None = None
        self.cost_usd: float | None = None
        self.metadata: dict[str, Any] | None = None


@dataclass
class StubConnection:
    provider: str | None = None
    model_id: str | None = None
    api_key: str | None = None
    execution_base_url: str | None = None
    env: dict[str, str] | None = None
    api_key_source_name: str | None = None
    base_url_source_name: str | None = None


class StubBaseInstalledAgent:
    def __init__(
        self,
        logs_dir: Path,
        *args: Any,
        version: str | None = None,
        skip_install: bool = False,
        extra_env: dict[str, str] | None = None,
        environment_logs_dir: PurePosixPath | str | None = None,
        model_name: str | None = None,
        model_connection: StubConnection | None = None,
        **kwargs: Any,
    ) -> None:
        self.logs_dir = Path(logs_dir)
        self.environment_logs_dir = PurePosixPath(
            environment_logs_dir or "/logs/agent"
        )
        self._version = version
        self._skip_install = skip_install
        self._extra_env = dict(extra_env or {})
        self.model_name = model_name
        self._test_model_connection = model_connection or StubConnection()
        self.logger = logging.getLogger("test.keel")
        self.ensure_system_dependencies_calls: list[tuple[str, ...]] = []
        self.init_kwargs = kwargs

    @property
    def model_connection(self) -> StubConnection:
        return self._test_model_connection

    def version(self) -> str | None:
        return self._version

    def get_version_command(self) -> str | None:
        return None

    def parse_version(self, stdout: str) -> str:
        return stdout.strip()

    async def exec_as_root(
        self,
        environment: Any,
        command: str,
        env: dict[str, str] | None = None,
        cwd: str | None = None,
        timeout_sec: int | None = None,
    ) -> Any:
        return await self._exec(environment, command, "root", env, cwd, timeout_sec)

    async def exec_as_agent(
        self,
        environment: Any,
        command: str,
        env: dict[str, str] | None = None,
        cwd: str | None = None,
        timeout_sec: int | None = None,
    ) -> Any:
        return await self._exec(environment, command, None, env, cwd, timeout_sec)

    async def _exec(
        self,
        environment: Any,
        command: str,
        user: str | None,
        env: dict[str, str] | None,
        cwd: str | None,
        timeout_sec: int | None,
    ) -> Any:
        result = await environment.exec(
            command=command, user=user, env=env, cwd=cwd, timeout_sec=timeout_sec
        )
        if result.return_code != 0:
            raise RuntimeError(
                f"Command failed (exit {result.return_code}): "
                f"{result.stderr or result.stdout}"
            )
        return result

    async def ensure_system_dependencies(
        self, environment: Any, dependencies: tuple[str, ...]
    ) -> None:
        self.ensure_system_dependencies_calls.append(tuple(dependencies))
        await environment.ensure_system_dependencies(dependencies)

    def with_prompt_template(self, fn: Any) -> Any:
        return fn


def _install_harbor_stubs() -> None:
    harbor = types.ModuleType("harbor")
    harbor.__path__ = []  # type: ignore[attr-defined]
    agents = types.ModuleType("harbor.agents")
    agents.__path__ = []  # type: ignore[attr-defined]
    installed = types.ModuleType("harbor.agents.installed")
    installed.__path__ = []  # type: ignore[attr-defined]
    models = types.ModuleType("harbor.models")
    models.__path__ = []  # type: ignore[attr-defined]

    capabilities = types.ModuleType("harbor.agents.capabilities")
    capabilities.AgentCapabilities = StubAgentCapabilities

    auth = types.ModuleType("harbor.agents.agent_auth_resolution")
    auth.ModelConnectionSpec = StubModelConnectionSpec
    auth.ProviderAccess = StubProviderAccess

    base = types.ModuleType("harbor.agents.installed.base")
    base.BaseInstalledAgent = StubBaseInstalledAgent

    def with_prompt_template(fn: Any) -> Any:
        return fn

    base.with_prompt_template = with_prompt_template

    api_format = types.ModuleType("harbor.models.model_api_format")
    api_format.ModelApiFormat = StubModelApiFormat

    harbor.agents = agents
    harbor.models = models
    agents.installed = installed
    agents.capabilities = capabilities
    agents.agent_auth_resolution = auth
    installed.base = base
    models.model_api_format = api_format

    sys.modules.update(
        {
            "harbor": harbor,
            "harbor.agents": agents,
            "harbor.agents.installed": installed,
            "harbor.agents.installed.base": base,
            "harbor.agents.capabilities": capabilities,
            "harbor.agents.agent_auth_resolution": auth,
            "harbor.models": models,
            "harbor.models.model_api_format": api_format,
        }
    )


_install_harbor_stubs()
if str(HARBOR_ADAPTER_DIR) not in sys.path:
    sys.path.insert(0, str(HARBOR_ADAPTER_DIR))
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))
import keel_agent  # noqa: E402

try:
    from benchmarks.traces import schema as trace_schema  # noqa: E402
except ImportError:
    trace_schema = None


# ---------------------------------------------------------------------------
# Fake environment
# ---------------------------------------------------------------------------


@dataclass
class FakeExecResult:
    stdout: str = ""
    stderr: str = ""
    return_code: int = 0


class FakeEnvironment:
    """Records Harbor-style environment calls and returns canned output."""

    def __init__(self, workdir: str = "/app") -> None:
        self.task_env_config = SimpleNamespace(workdir=workdir)
        self.default_user = "app"
        self.uploads: list[tuple[Path, str]] = []
        self.uploaded_targets: list[str] = []
        self.execs: list[dict[str, Any]] = []
        self.ensure_calls: list[tuple[str, ...]] = []
        self.rg_available = False
        self.package_manager_installs_rg = False
        self.keel_path = "/opt/bin/keel"
        self.checksum = "a" * 64
        self.route_error: str | None = None
        self.chat_stdout = (
            "[end] turns 2 in 250 out 30 cache read 5 write 0\n"
        )
        self.session_source: str | None = None
        self.handler: Any = None

    async def upload_file(self, source_path: Path | str, target_path: str) -> None:
        self.uploads.append((Path(source_path), str(target_path)))
        self.uploaded_targets.append(str(target_path))

    async def ensure_system_dependencies(self, dependencies: tuple[str, ...]) -> None:
        self.ensure_calls.append(tuple(dependencies))
        if self.package_manager_installs_rg and "ripgrep" in dependencies:
            self.rg_available = True

    async def exec(
        self,
        command: str,
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        timeout_sec: int | None = None,
        user: str | int | None = None,
    ) -> FakeExecResult:
        self.execs.append(
            {
                "command": command,
                "cwd": cwd,
                "env": dict(env or {}),
                "timeout_sec": timeout_sec,
                "user": user,
            }
        )
        if self.handler is not None:
            response = self.handler(command, self)
            if response is not None:
                return response
        return self._default(command)

    def _default(self, command: str) -> FakeExecResult:
        if command.startswith("if [ -x ") and "keel-path-missing" in command:
            uploaded = [
                target
                for target in self.uploaded_targets
                if target.endswith("/keel")
            ]
            path = uploaded[-1] if uploaded else self.keel_path
            if path:
                return FakeExecResult(f"keel-path:{path}\n")
            return FakeExecResult("keel-path-missing\n")
        if "command -v rg" in command:
            return FakeExecResult("yes\n" if self.rg_available else "no\n")
        if "keel route" in command:
            if self.route_error:
                return FakeExecResult("", self.route_error, 1)
            return FakeExecResult("provider   neuralwatt\nmodel      deepseek-v4.1-flash\n")
        if "keel chat" in command:
            return FakeExecResult(self.chat_stdout)
        if "keel-session-source" in command:
            if self.session_source:
                return FakeExecResult(
                    f"keel-session-source:{self.session_source}\n"
                )
            return FakeExecResult("")
        if "--version" in command:
            if "/rg" in command or " rg " in command:
                return FakeExecResult("ripgrep 14.1.0\n")
            return FakeExecResult("keel 0.1.0\n")
        if "sha256sum" in command or "shasum" in command or "openssl" in command:
            return FakeExecResult(f"{self.checksum}  /opt/bin/keel\n")
        return FakeExecResult("")

    @property
    def commands(self) -> list[str]:
        return [entry["command"] for entry in self.execs]

    def last_exec(self, needle: str) -> dict[str, Any]:
        matches = [entry for entry in self.execs if needle in entry["command"]]
        if not matches:
            raise AssertionError(f"no exec command containing {needle!r}")
        return matches[-1]


class HarborAdapterTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.root = Path(self.tempdir.name)
        self.logs = self.root / "agent"
        self.logs.mkdir()
        self.binary = self.root / "keel"
        self.binary.write_bytes(b"\x7fELF" + b"\x00" * 64)
        self.rg = self.root / "rg"
        self.rg.write_bytes(b"\x7fELF" + b"\x01" * 64)
        self.env = FakeEnvironment()
        self.connection = StubConnection(
            provider="neuralwatt",
            model_id="deepseek-v4.1-flash",
            api_key="sk-test",
            execution_base_url="https://api.neuralwatt.com/v1",
        )

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def make_agent(self, **kwargs: Any) -> keel_agent.KeelAgent:
        options: dict[str, Any] = {
            "logs_dir": self.logs,
            "model_name": "neuralwatt/deepseek-v4.1-flash",
            "model_connection": self.connection,
        }
        options.update(kwargs)
        return keel_agent.KeelAgent(**options)

    async def configure_skip_install(
        self, agent: keel_agent.KeelAgent, env: FakeEnvironment | None = None
    ) -> keel_agent.KeelAgent:
        self.env.rg_available = True
        await agent.configure(env or self.env)
        return agent

    # -- basics ---------------------------------------------------------

    def test_name_and_defaults(self) -> None:
        agent = self.make_agent()
        self.assertEqual(keel_agent.KeelAgent.name(), "keel")
        self.assertEqual(agent._resolved_keel_target, "neuralwatt/deepseek-v4.1-flash")
        self.assertEqual(agent._max_turns, 60)
        self.assertEqual(agent._data_dir, "/tmp/keel-data")

    def test_explicit_unsupported_target_fails_at_construction(self) -> None:
        with self.assertRaisesRegex(ValueError, "not supported"):
            self.make_agent(keel_target="opencode/claude-sonnet-4-5")

    def test_invalid_max_turns_fails(self) -> None:
        with self.assertRaisesRegex(ValueError, "at least 1"):
            self.make_agent(max_turns=0)

    def test_rejects_dangerous_data_dirs(self) -> None:
        for value in (
            "/",
            "//",
            "/tmp",
            "/tmp/",
            "/app",
            "/testbed",
            "/usr",
            "/var",
            "/etc",
            "/home",
            "/root",
            "/usr/local",
            "/var/tmp",
            "/home/user",
            "/app/sub",
            "/testbed/task",
            "/logs",
            "/logs/agent",
            "/logs/agent/sub",
            "/tests",
            "/tests/agent",
            "/solution",
            "/solution/sub",
            "/etc/keel",
            "/tmp/../etc",
            "/tmp/keel-data/../../etc",
        ):
            with self.subTest(value=value):
                with self.assertRaisesRegex(ValueError, "data_dir"):
                    self.make_agent(data_dir=value)

    def test_rejects_dangerous_remote_binaries(self) -> None:
        for value in (
            "/",
            "//",
            "/tmp",
            "/usr",
            "/etc",
            "/root",
            "/usr/bin/../etc/keel",
            "/tmp/keel/",
        ):
            with self.subTest(value=value):
                with self.assertRaisesRegex(ValueError, "remote_binary"):
                    self.make_agent(remote_binary=value)

    async def test_configure_allows_dedicated_tmp_data_dir(self) -> None:
        self.env.rg_available = True
        agent = await self.configure_skip_install(
            self.make_agent(data_dir="/tmp/keel-data-dedicated")
        )
        self.assertEqual(agent._data_dir, "/tmp/keel-data-dedicated")

    async def test_configure_rejects_data_dir_ancestor_of_workspace(self) -> None:
        for data_dir, workdir in (
            ("/data", "/data/task"),
            ("/data/task", "/data/task"),
        ):
            with self.subTest(data_dir=data_dir, workdir=workdir):
                env = FakeEnvironment(workdir=workdir)
                env.rg_available = True
                agent = self.make_agent(data_dir=data_dir)
                with self.assertRaisesRegex(ValueError, "workspace"):
                    await agent.configure(env)
                self.assertFalse(
                    any("rm -rf" in command for command in env.commands),
                    "configure() must reject the directory before rm -rf",
                )

    async def test_configure_rejects_data_dir_over_agent_logs(self) -> None:
        for data_dir, logs_dir in (
            ("/data/logs", "/data/logs/agent"),
            ("/data/logs/agent", "/data/logs/agent"),
        ):
            with self.subTest(data_dir=data_dir, logs_dir=logs_dir):
                env = FakeEnvironment()
                env.rg_available = True
                agent = self.make_agent(
                    data_dir=data_dir,
                    environment_logs_dir=logs_dir,
                )
                with self.assertRaisesRegex(ValueError, "logs"):
                    await agent.configure(env)
                self.assertFalse(
                    any("rm -rf" in command for command in env.commands),
                    "configure() must reject the logs tree before rm -rf",
                )

    # -- install --------------------------------------------------------

    async def test_install_uploads_binary_and_rg_then_chmods(self) -> None:
        agent = self.make_agent(binary_path=self.binary, rg_path=self.rg)
        await agent.install(self.env)

        targets = [target for _, target in self.env.uploads]
        self.assertIn("/usr/local/bin/keel", targets)
        self.assertIn("/usr/local/bin/rg", targets)
        joined = "\n".join(self.env.commands)
        self.assertIn("chmod 755 /usr/local/bin/keel", joined)
        self.assertIn("chmod 755 /usr/local/bin/rg", joined)
        self.assertIn("/usr/local/bin/keel --version", joined)
        self.assertEqual(agent.version(), "0.1.0")
        self.assertEqual(
            agent._binary_sha256, hashlib.sha256(self.binary.read_bytes()).hexdigest()
        )
        self.assertEqual(self.env.ensure_calls, [])

    async def test_install_uses_existing_rg_without_package_manager(self) -> None:
        self.env.rg_available = True
        agent = self.make_agent(binary_path=self.binary)
        await agent.install(self.env)

        targets = [target for _, target in self.env.uploads]
        self.assertEqual(targets, ["/usr/local/bin/keel"])
        self.assertEqual(self.env.ensure_calls, [])

    async def test_install_tries_package_manager_when_rg_missing(self) -> None:
        self.env.package_manager_installs_rg = True
        agent = self.make_agent(binary_path=self.binary)
        await agent.install(self.env)

        self.assertEqual(self.env.ensure_calls, [("ripgrep",)])
        targets = [target for _, target in self.env.uploads]
        self.assertEqual(targets, ["/usr/local/bin/keel"])

    async def test_install_fails_clearly_when_rg_is_unavailable(self) -> None:
        agent = self.make_agent(binary_path=self.binary)
        with self.assertRaisesRegex(RuntimeError, "rg_path"):
            await agent.install(self.env)
        self.assertEqual(self.env.ensure_calls, [("ripgrep",)])

    async def test_install_fails_when_binary_is_missing(self) -> None:
        agent = self.make_agent(binary_path=self.root / "missing-keel")
        with self.assertRaisesRegex(FileNotFoundError, "README"):
            await agent.install(self.env)

    async def test_install_then_configure_verifies_uploaded_checksum(self) -> None:
        self.env.rg_available = True
        self.env.checksum = hashlib.sha256(self.binary.read_bytes()).hexdigest()
        agent = self.make_agent(binary_path=self.binary)
        await agent.install(self.env)
        await agent.configure(self.env)

        self.assertEqual(
            agent._binary_sha256, hashlib.sha256(self.binary.read_bytes()).hexdigest()
        )
        self.assertEqual(agent._keel_executable, "/usr/local/bin/keel")
        self.assertEqual(
            [target for _, target in self.env.uploads], ["/usr/local/bin/keel"]
        )

    # -- configure ------------------------------------------------------

    async def test_skip_install_configure_reconstructs_state(self) -> None:
        agent = self.make_agent(skip_install=True)
        self.env.rg_available = True
        await agent.configure(self.env)

        self.assertEqual(self.env.uploads, [])
        self.assertEqual(agent._keel_executable, "/opt/bin/keel")
        self.assertEqual(agent.version(), "0.1.0")
        self.assertEqual(agent._binary_sha256, "a" * 64)
        joined = "\n".join(self.env.commands)
        self.assertIn("mkdir -p /tmp/keel-data", joined)
        self.assertIn("chmod 700 /tmp/keel-data", joined)
        self.assertIn("KEEL_DATA_DIR=/tmp/keel-data", joined)
        self.assertIn("KEEL_MCP=0", joined)
        self.assertIn("/tmp/keel-runtime.env", joined)

    async def test_skip_install_configure_fails_without_binary(self) -> None:
        self.env.keel_path = ""
        agent = self.make_agent(skip_install=True)
        with self.assertRaisesRegex(RuntimeError, "skip_install"):
            await agent.configure(self.env)

    async def test_skip_install_configure_fails_without_rg(self) -> None:
        self.env.rg_available = False
        agent = self.make_agent(skip_install=True)
        with self.assertRaisesRegex(RuntimeError, "ripgrep"):
            await agent.configure(self.env)

    async def test_configure_rejects_provider_mismatch(self) -> None:
        self.connection = StubConnection(
            provider="openai",
            model_id="gpt-4.1",
            api_key="sk-openai",
        )
        agent = self.make_agent()
        self.env.rg_available = True
        with self.assertRaisesRegex(ValueError, "Harbor resolved provider"):
            await agent.configure(self.env)

    async def test_rejects_cross_provider_key_source_before_exec(self) -> None:
        for target, source in (
            ("neuralwatt/deepseek-v4.1-flash", "OPENAI_API_KEY"),
            ("custom/default", "NEURALWATT_API_KEY"),
        ):
            with self.subTest(target=target, source=source):
                self.connection = StubConnection(
                    provider=None,
                    model_id=None,
                    api_key="sk-wrong-provider",
                    api_key_source_name=source,
                    execution_base_url="http://localhost:8000/v1",
                    base_url_source_name="KEEL_CUSTOM_BASE_URL",
                )
                env = FakeEnvironment()
                env.rg_available = True
                agent = self.make_agent(keel_target=target, skip_install=True)
                with self.assertRaisesRegex(ValueError, source):
                    await agent.configure(env)
                self.assertEqual(env.execs, [])

    async def test_matching_key_source_passes(self) -> None:
        self.connection = StubConnection(
            provider=None,
            model_id=None,
            api_key="sk-nw",
            api_key_source_name="NEURALWATT_API_KEY",
        )
        self.env.rg_available = True
        agent = self.make_agent(skip_install=True)
        await agent.configure(self.env)
        self.assertEqual(agent._resolved_keel_target, "neuralwatt/deepseek-v4.1-flash")

    async def test_anthropic_auth_token_source_passes(self) -> None:
        self.connection = StubConnection(
            provider="anthropic",
            model_id="claude-sonnet-4-5",
            api_key="sk-ant",
            api_key_source_name="ANTHROPIC_AUTH_TOKEN",
        )
        self.env.rg_available = True
        agent = self.make_agent(
            keel_target="anthropic/claude-sonnet-4-5",
            skip_install=True,
        )
        await agent.configure(self.env)

    async def test_rejects_cross_provider_base_url_source(self) -> None:
        self.connection = StubConnection(
            provider=None,
            model_id=None,
            api_key="sk-nw",
            api_key_source_name="NEURALWATT_API_KEY",
            execution_base_url="https://openai-proxy.example/v1",
            base_url_source_name="OPENAI_BASE_URL",
        )
        env = FakeEnvironment()
        env.rg_available = True
        agent = self.make_agent(skip_install=True)
        with self.assertRaisesRegex(ValueError, "OPENAI_BASE_URL"):
            await agent.configure(env)
        self.assertEqual(env.execs, [])

    async def test_matching_base_url_source_passes_and_reaches_route(self) -> None:
        self.connection = StubConnection(
            provider=None,
            model_id=None,
            api_key="sk-nw",
            api_key_source_name="NEURALWATT_API_KEY",
            execution_base_url="https://neuralwatt.example/v1",
            base_url_source_name="KEEL_NEURALWATT_BASE_URL",
        )
        self.env.rg_available = True
        agent = self.make_agent(skip_install=True)
        await agent.configure(self.env)

        route = self.env.last_exec("keel route")
        self.assertEqual(
            route["env"]["KEEL_NEURALWATT_BASE_URL"],
            "https://neuralwatt.example/v1",
        )

    async def test_explicit_target_is_validated_with_route(self) -> None:
        agent = self.make_agent(skip_install=True)
        self.env.rg_available = True
        await agent.configure(self.env)

        route = self.env.last_exec("keel route")
        self.assertIn(
            "keel route neuralwatt/deepseek-v4.1-flash --workspace /app",
            route["command"],
        )

    async def test_explicit_target_outside_catalog_fails(self) -> None:
        self.env.route_error = "provider `neuralwatt` has no model `typo`"
        agent = self.make_agent(
            keel_target="neuralwatt/typo",
            skip_install=True,
        )
        self.env.rg_available = True
        with self.assertRaisesRegex(RuntimeError, "not in keel's catalog"):
            await agent.configure(self.env)

    async def test_auto_target_derives_and_validates_in_catalog(self) -> None:
        self.connection = StubConnection(
            provider="openai",
            model_id="gpt-4.1",
            api_key="sk-openai",
            execution_base_url="https://api.openai.com/v1",
        )
        agent = self.make_agent(keel_target=None)
        self.env.rg_available = True
        await agent.configure(self.env)

        self.assertEqual(agent._resolved_keel_target, "openai/gpt-4.1")
        route = self.env.last_exec("keel route")
        self.assertIn("keel route openai/gpt-4.1", route["command"])

    async def test_auto_target_fails_clearly_outside_catalog(self) -> None:
        self.connection = StubConnection(
            provider="openai",
            model_id="not-in-catalog",
            api_key="sk-openai",
        )
        self.env.route_error = "provider `openai` has no model `not-in-catalog`"
        agent = self.make_agent(keel_target=None)
        self.env.rg_available = True
        with self.assertRaisesRegex(RuntimeError, "not in keel's catalog"):
            await agent.configure(self.env)

    # -- run ------------------------------------------------------------

    async def _run_agent(
        self,
        *,
        target: str | None = "neuralwatt/deepseek-v4.1-flash",
        instruction: str = "fix the bug",
        max_turns: int = 60,
        workdir: str = "/app",
        connection: StubConnection | None = None,
    ) -> tuple[keel_agent.KeelAgent, FakeEnvironment, StubAgentContext]:
        self.connection = connection or self.connection
        env = FakeEnvironment(workdir=workdir)
        env.rg_available = True
        agent = self.make_agent(keel_target=target, max_turns=max_turns, skip_install=True)
        await agent.configure(env)
        context = StubAgentContext()
        await agent.run(instruction, env, context)
        return agent, env, context

    async def test_run_command_quotes_prompt_and_sets_env(self) -> None:
        instruction = "fix the bug; echo 'no' && exit 1\nsecond line"
        agent, env, _ = await self._run_agent(instruction=instruction, max_turns=7)

        chat = env.last_exec("keel chat")
        self.assertIn(
            "keel chat neuralwatt/deepseek-v4.1-flash --workspace /app",
            chat["command"],
        )
        self.assertIn(f"--prompt {shlex.quote(instruction)}", chat["command"])
        self.assertIn("--yes --max-turns 7", chat["command"])
        self.assertIn("< /dev/null 2>&1 | tee /logs/agent/keel-chat.log", chat["command"])
        self.assertEqual(chat["cwd"], "/app")
        self.assertEqual(
            chat["env"],
            {
                "KEEL_DATA_DIR": "/tmp/keel-data",
                "KEEL_MCP": "0",
                "NO_COLOR": "1",
                "NEURALWATT_API_KEY": "sk-test",
                "KEEL_NEURALWATT_BASE_URL": "https://api.neuralwatt.com/v1",
            },
        )
        self.assertNotIn("verifier", "\n".join(env.commands))
        self.assertEqual(agent._run_error, None)
        self.assertIsNotNone(agent._wall_clock_s)

    async def test_run_uses_task_workdir(self) -> None:
        _, env, _ = await self._run_agent(workdir="/workspace/task")
        chat = env.last_exec("keel chat")
        self.assertIn("--workspace /workspace/task", chat["command"])
        self.assertEqual(chat["cwd"], "/workspace/task")

    async def test_run_maps_openai_env_both_base_url_names(self) -> None:
        connection = StubConnection(
            provider="openai",
            model_id="gpt-4.1",
            api_key="sk-openai",
            execution_base_url="https://gateway.example/v1",
        )
        _, env, _ = await self._run_agent(
            target="openai/gpt-4.1", connection=connection
        )
        chat = env.last_exec("keel chat")
        self.assertEqual(chat["env"]["OPENAI_API_KEY"], "sk-openai")
        self.assertEqual(chat["env"]["KEEL_OPENAI_BASE_URL"], "https://gateway.example/v1")
        self.assertEqual(chat["env"]["OPENAI_BASE_URL"], "https://gateway.example/v1")

    async def test_run_maps_anthropic_env_both_base_url_names(self) -> None:
        connection = StubConnection(
            provider="anthropic",
            model_id="claude-sonnet-4-5",
            api_key="sk-ant",
            execution_base_url="https://anthropic.example",
        )
        _, env, _ = await self._run_agent(
            target="anthropic/claude-sonnet-4-5", connection=connection
        )
        chat = env.last_exec("keel chat")
        self.assertEqual(chat["env"]["ANTHROPIC_API_KEY"], "sk-ant")
        self.assertEqual(
            chat["env"]["KEEL_ANTHROPIC_BASE_URL"], "https://anthropic.example"
        )
        self.assertEqual(
            chat["env"]["ANTHROPIC_BASE_URL"], "https://anthropic.example"
        )

    async def test_run_requires_a_key_for_keyed_providers(self) -> None:
        connection = StubConnection(
            provider="neuralwatt",
            model_id="deepseek-v4.1-flash",
            api_key=None,
        )
        self.connection = connection
        env = FakeEnvironment()
        env.rg_available = True
        agent = self.make_agent(skip_install=True)
        await agent.configure(env)
        with self.assertRaisesRegex(RuntimeError, "NEURALWATT_API_KEY"):
            await agent.run("do the task", env, StubAgentContext())

    async def test_run_allows_keyless_custom_endpoint(self) -> None:
        connection = StubConnection(
            provider=None,
            model_id=None,
            api_key=None,
            execution_base_url="http://localhost:8000/v1",
        )
        self.connection = connection
        env = FakeEnvironment()
        env.rg_available = True
        agent = self.make_agent(keel_target="custom/default", skip_install=True)
        await agent.configure(env)
        route = env.last_exec("keel route")
        self.assertEqual(
            route["env"]["KEEL_CUSTOM_BASE_URL"], "http://localhost:8000/v1"
        )
        await agent.run("do the task", env, StubAgentContext())
        chat = env.last_exec("keel chat")
        self.assertEqual(chat["env"]["KEEL_CUSTOM_BASE_URL"], "http://localhost:8000/v1")
        self.assertNotIn("KEEL_CUSTOM_API_KEY", chat["env"])

    async def test_run_copies_session_and_records_source(self) -> None:
        env = FakeEnvironment()
        env.rg_available = True
        env.session_source = "/tmp/keel-data/sessions/project/session.jsonl"
        agent = self.make_agent(skip_install=True)
        await agent.configure(env)
        await agent.run("do the task", env, StubAgentContext())

        collect = env.last_exec("keel-session-source")
        self.assertIn("cp \"$src\" /logs/agent/keel-session.jsonl", collect["command"])
        self.assertEqual(
            agent._container_session_log,
            "/tmp/keel-data/sessions/project/session.jsonl",
        )

    async def test_cancelled_run_records_timeout_trace(self) -> None:
        env = FakeEnvironment()
        env.rg_available = True

        def handler(command: str, fake: FakeEnvironment) -> FakeExecResult | None:
            if "keel chat" in command:
                raise asyncio.CancelledError()
            return None

        env.handler = handler
        agent = self.make_agent(skip_install=True)
        await agent.configure(env)
        context = StubAgentContext()

        with self.assertRaises(asyncio.CancelledError):
            await agent.run("do the task", env, context)
        agent.populate_context_post_run(context)

        trace = json.loads((self.logs / "keel-trace.jsonl").read_text())
        self.assertEqual(trace["stop_reason"], "timeout")
        self.assertEqual(trace["failure_tags"], ["timeout"])
        self.assertEqual(trace["error"], "agent cancelled or timed out")
        if trace_schema is not None:
            self.assertEqual(trace_schema.trace_errors(trace, strict=True), [])

    async def test_successful_run_survives_session_capture_failure(self) -> None:
        env = FakeEnvironment()
        env.rg_available = True

        def handler(command: str, fake: FakeEnvironment) -> FakeExecResult | None:
            if "keel-session-source" in command:
                return FakeExecResult("", "cp failed", 1)
            return None

        env.handler = handler
        agent = self.make_agent(skip_install=True)
        await agent.configure(env)
        with self.assertLogs("test.keel", level="ERROR"):
            await agent.run("do the task", env, StubAgentContext())
        self.assertIsNone(agent._run_error)

    async def test_failed_run_survives_session_capture_failure(self) -> None:
        env = FakeEnvironment()
        env.rg_available = True

        def handler(command: str, fake: FakeEnvironment) -> FakeExecResult | None:
            if "keel chat" in command:
                return FakeExecResult("", "chat failed", 1)
            if "keel-session-source" in command:
                return FakeExecResult("", "cp failed", 1)
            return None

        env.handler = handler
        agent = self.make_agent(skip_install=True)
        await agent.configure(env)

        with self.assertRaisesRegex(RuntimeError, "chat failed"):
            with self.assertLogs("test.keel", level="ERROR"):
                await agent.run("do the task", env, StubAgentContext())
        self.assertIn("chat failed", agent._run_error or "")

    # -- metadata -------------------------------------------------------

    def _write_session(self, protocol: str = "openai-chat", cache: int = 5) -> Path:
        session = self.logs / "keel-session.jsonl"
        records = [
            {
                "type": "meta",
                "version": 2,
                "id": "session-1",
                "created_at": 1,
                "provider": "neuralwatt",
                "model": "deepseek-v4.1-flash",
                "protocol": protocol,
                "workspace": "/app",
            },
            {
                "type": "message",
                "at": 1,
                "message": {
                    "id": "m1",
                    "role": "user",
                    "parts": [{"content": {"type": "text", "text": "fix it"}}],
                },
            },
            {
                "type": "message",
                "at": 2,
                "message": {
                    "id": "m2",
                    "role": "assistant",
                    "parts": [
                        {"content": {"type": "text", "text": "working"}},
                        {
                            "content": {
                                "type": "tool_call",
                                "id": "t1",
                                "name": "bash",
                                "arguments": "{}",
                            }
                        },
                        {
                            "content": {
                                "type": "tool_call",
                                "id": "t2",
                                "name": "read",
                                "arguments": "{}",
                            }
                        },
                    ],
                    "stop": "tool_use",
                    "usage": {
                        "input_tokens": 100,
                        "output_tokens": 20,
                        "cache_read_tokens": cache // 2,
                        "cache_write_tokens": 0,
                        "reasoning_tokens": 3,
                    },
                },
            },
            {
                "type": "message",
                "at": 3,
                "message": {
                    "id": "m3",
                    "role": "assistant",
                    "parts": [{"content": {"type": "text", "text": "done"}}],
                    "stop": "end_turn",
                    "usage": {
                        "input_tokens": 150,
                        "output_tokens": 10,
                        "cache_read_tokens": cache - cache // 2,
                        "cache_write_tokens": 0,
                        "reasoning_tokens": 0,
                    },
                },
            },
        ]
        session.write_text(
            "\n".join(json.dumps(record) for record in records) + "\n",
            encoding="utf-8",
        )
        return session

    def _metadata_agent(self, **kwargs: Any) -> keel_agent.KeelAgent:
        agent = self.make_agent(skip_install=True, **kwargs)
        agent._version = "0.1.0"
        agent._binary_sha256 = "b" * 64
        agent._resolved_keel_target = "neuralwatt/deepseek-v4.1-flash"
        agent._wall_clock_s = 1.234
        agent._run_started_at = "2026-11-21T00:00:00.000Z"
        agent._run_finished_at = "2026-11-21T00:00:01.234Z"
        agent._run_output = "[end] turns 2 in 250 out 30 cache read 5 write 0\n"
        return agent

    def test_metadata_extracts_session_and_sets_context(self) -> None:
        session = self._write_session()
        agent = self._metadata_agent()
        context = StubAgentContext()

        agent.populate_context_post_run(context)

        metadata = json.loads((self.logs / "keel-trial.json").read_text())
        self.assertEqual(metadata["kind"], "keel-harbor-trial")
        self.assertEqual(metadata["agent"], "keel")
        self.assertEqual(metadata["agent_version"], "0.1.0")
        self.assertEqual(metadata["harbor_model"], "neuralwatt/deepseek-v4.1-flash")
        self.assertEqual(metadata["keel_target"], "neuralwatt/deepseek-v4.1-flash")
        self.assertEqual(metadata["binary_sha256"], "b" * 64)
        self.assertEqual(metadata["turns"], 2)
        self.assertEqual(metadata["tool_calls"], 2)
        self.assertEqual(metadata["stop_reason"], "end_turn")
        self.assertEqual(metadata["wall_clock_s"], 1.234)
        self.assertEqual(metadata["protocol"], "openai-chat")
        self.assertEqual(metadata["session_id"], "session-1")
        self.assertEqual(metadata["session_log_path"], str(session))
        self.assertEqual(metadata["tokens"]["input"], 250)
        self.assertEqual(metadata["tokens"]["output"], 30)
        self.assertEqual(metadata["tokens"]["cache_read"], 5)
        self.assertEqual(metadata["tokens"]["reasoning"], 3)
        self.assertEqual(metadata["summary_tokens"]["input"], 250)
        self.assertEqual(context.n_input_tokens, 250)
        self.assertEqual(context.n_cache_tokens, 5)
        self.assertEqual(context.n_output_tokens, 30)
        self.assertEqual(
            context.metadata["keel"]["keel_target"], "neuralwatt/deepseek-v4.1-flash"
        )

    def test_trace_matches_the_canonical_schema(self) -> None:
        session = self._write_session()
        agent = self._metadata_agent(
            benchmark="terminal-bench-2.0",
            task_id="terminal-bench/hello-world",
            trial=2,
        )
        context = StubAgentContext()

        agent.populate_context_post_run(context)

        trace = json.loads((self.logs / "keel-trace.jsonl").read_text())
        self.assertEqual(tuple(trace), TRACE_FIELDS)
        self.assertEqual(trace["schema_version"], "1.0")
        self.assertEqual(trace["benchmark"], "terminal-bench-2.0")
        self.assertEqual(trace["task_id"], "terminal-bench/hello-world")
        self.assertEqual(trace["trial"], 2)
        self.assertEqual(trace["model"], "neuralwatt/deepseek-v4.1-flash")
        self.assertIsNone(trace["model_version"])
        self.assertEqual(trace["keel_target"], "neuralwatt/deepseek-v4.1-flash")
        self.assertEqual(trace["agent_version"], "0.1.0")
        self.assertEqual(trace["binary_sha256"], "b" * 64)
        self.assertEqual(trace["start_time"], "2026-11-21T00:00:00.000Z")
        self.assertEqual(trace["end_time"], "2026-11-21T00:00:01.234Z")
        self.assertEqual(trace["turns"], 2)
        self.assertEqual(trace["tool_calls"], 2)
        self.assertEqual(trace["input_tokens"], 245)
        self.assertEqual(trace["output_tokens"], 30)
        self.assertEqual(trace["reasoning_tokens"], 3)
        self.assertIsNone(trace["cost_usd"])
        self.assertEqual(trace["stop_reason"], "completed")
        self.assertEqual(trace["wall_clock_s"], 1.234)
        self.assertEqual(trace["patch"], "")
        self.assertEqual(trace["patch_size"], len(trace["patch"].encode("utf-8")))
        self.assertIsNone(trace["patch_apply_ok"])
        self.assertEqual(trace["verifier_result"], "not_run")
        self.assertEqual(trace["failure_tags"], [])
        self.assertEqual(trace["session_log_path"], str(session))
        self.assertIsNone(trace["error"])
        if trace_schema is not None:
            self.assertEqual(trace_schema.trace_errors(trace, strict=True), [])

        metadata = json.loads((self.logs / "keel-trial.json").read_text())
        self.assertEqual(metadata["kind"], "keel-harbor-trial")
        self.assertEqual(metadata["trace_path"], str(self.logs / "keel-trace.jsonl"))

    def test_trace_passes_canonical_schema_when_available(self) -> None:
        if trace_schema is None:
            self.skipTest("benchmarks.traces.schema is not available in this checkout")
        self._write_session()
        agent = self._metadata_agent(
            benchmark="terminal-bench-2.0",
            task_id="terminal-bench/hello-world",
            trial=2,
        )
        agent.populate_context_post_run(StubAgentContext())
        trace = json.loads((self.logs / "keel-trace.jsonl").read_text())
        self.assertEqual(trace_schema.trace_errors(trace, strict=True), [])

    def test_trace_derives_task_id_from_harbor_trial_config(self) -> None:
        (self.root / "config.json").write_text(
            json.dumps(
                {
                    "task": {"name": "terminal-bench/hello-world"},
                    "trial_name": "hello-world__abc1234",
                }
            )
        )
        agent = self._metadata_agent()
        context = StubAgentContext()

        agent.populate_context_post_run(context)

        trace = json.loads((self.logs / "keel-trace.jsonl").read_text())
        self.assertEqual(trace["task_id"], "terminal-bench/hello-world")
        self.assertEqual(trace["trial"], 0)

    def test_trace_derives_task_id_from_trial_name_without_config(self) -> None:
        agent = self._metadata_agent()
        agent.session_id = "hello-world__abc1234__agent"
        context = StubAgentContext()

        agent.populate_context_post_run(context)

        trace = json.loads((self.logs / "keel-trace.jsonl").read_text())
        self.assertEqual(trace["task_id"], "hello-world")

    def test_trace_fills_required_defaults_without_run_data(self) -> None:
        agent = self.make_agent(skip_install=True)
        agent._version = "0.1.0"
        agent._run_error = "keel exited 1"
        context = StubAgentContext()

        agent.populate_context_post_run(context)

        trace = json.loads((self.logs / "keel-trace.jsonl").read_text())
        self.assertEqual(tuple(trace), TRACE_FIELDS)
        self.assertEqual(trace["turns"], 0)
        self.assertEqual(trace["tool_calls"], 0)
        self.assertEqual(trace["input_tokens"], 0)
        self.assertEqual(trace["output_tokens"], 0)
        self.assertEqual(trace["reasoning_tokens"], 0)
        self.assertEqual(trace["stop_reason"], "error")
        self.assertEqual(trace["wall_clock_s"], 0.0)
        self.assertIsNone(trace["session_log_path"])
        self.assertEqual(trace["error"], "keel exited 1")
        self.assertGreaterEqual(trace["end_time"], trace["start_time"])

    def test_metadata_adds_cache_for_anthropic_input_tokens(self) -> None:
        self._write_session(protocol="anthropic-messages", cache=5)
        agent = self._metadata_agent()
        context = StubAgentContext()

        agent.populate_context_post_run(context)

        self.assertEqual(context.n_input_tokens, 255)
        self.assertEqual(context.n_cache_tokens, 5)
        trace = json.loads((self.logs / "keel-trace.jsonl").read_text())
        self.assertEqual(trace["input_tokens"], 250)

    def test_metadata_falls_back_to_summary_without_session(self) -> None:
        agent = self._metadata_agent()
        agent._run_error = "keel exited 1"
        context = StubAgentContext()

        agent.populate_context_post_run(context)

        metadata = json.loads((self.logs / "keel-trial.json").read_text())
        self.assertEqual(metadata["turns"], 2)
        self.assertIsNone(metadata["tool_calls"])
        self.assertEqual(metadata["tokens"]["input"], 250)
        self.assertEqual(metadata["stop_reason"], "end_turn")
        self.assertEqual(metadata["error"], "keel exited 1")
        self.assertEqual(context.n_input_tokens, 250)

    def test_metadata_handles_missing_session_and_summary(self) -> None:
        agent = self.make_agent(skip_install=True)
        agent._version = "0.1.0"
        context = StubAgentContext()

        agent.populate_context_post_run(context)

        metadata = json.loads((self.logs / "keel-trial.json").read_text())
        self.assertIsNone(metadata["turns"])
        self.assertIsNone(metadata["tokens"])
        self.assertIsNone(context.n_input_tokens)
        self.assertEqual(metadata["keel_target"], "neuralwatt/deepseek-v4.1-flash")

    # -- no host execution ---------------------------------------------

    async def test_adapter_never_executes_on_the_host(self) -> None:
        with (
            patch("subprocess.run", side_effect=AssertionError("host run")),
            patch("subprocess.Popen", side_effect=AssertionError("host Popen")),
            patch("os.system", side_effect=AssertionError("host system")),
        ):
            agent = self.make_agent(binary_path=self.binary)
            self.env.rg_available = True
            self.env.checksum = hashlib.sha256(self.binary.read_bytes()).hexdigest()
            await agent.install(self.env)
            await agent.configure(self.env)
            await agent.run("do the task", self.env, StubAgentContext())

        self.assertTrue(any("keel chat" in command for command in self.env.commands))


if __name__ == "__main__":
    unittest.main()
