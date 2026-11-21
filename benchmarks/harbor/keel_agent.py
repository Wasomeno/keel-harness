"""Harbor installed-agent adapter for keel.

Harbor installs the agent CLI inside the task container. This adapter uploads a
Linux ``keel`` binary, prepares a per-trial runtime directory, and runs exactly
one non-interactive ``keel chat`` turn in the task workspace. Harbor runs the
verifier; the adapter never does.

Use it as a custom agent module::

    PYTHONPATH=benchmarks/harbor harbor run \
        -t hello-world/hello-world \
        --agent keel_agent:KeelAgent \
        --model neuralwatt/deepseek-v4.1-flash \
        --ak binary_path="$PWD/benchmarks/harbor/bin/keel-x86_64-unknown-linux-musl"

See ``benchmarks/harbor/README.md`` for the build command and options.
"""

from __future__ import annotations

import hashlib
import json
import os
import posixpath
import re
import shlex
import time
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath
from typing import TYPE_CHECKING, Any, Mapping

try:
    from harbor.agents.agent_auth_resolution import ModelConnectionSpec, ProviderAccess
    from harbor.agents.capabilities import AgentCapabilities
    from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
    from harbor.models.model_api_format import ModelApiFormat
except ImportError as exc:  # pragma: no cover - exercised only without Harbor
    raise ImportError(
        "benchmarks/harbor/keel_agent.py requires Harbor. Install it with "
        "`pip install harbor`, then make this directory importable, for example "
        "`PYTHONPATH=benchmarks/harbor harbor run ... --agent keel_agent:KeelAgent`."
    ) from exc

if TYPE_CHECKING:
    from harbor.environments.base import BaseEnvironment
    from harbor.models.agent.context import AgentContext


DEFAULT_KEEL_TARGET = "neuralwatt/deepseek-v4.1-flash"
DEFAULT_MAX_TURNS = 60
DEFAULT_REMOTE_BINARY = "/usr/local/bin/keel"
DEFAULT_REMOTE_RG = "/usr/local/bin/rg"
DEFAULT_DATA_DIR = "/tmp/keel-data"
DEFAULT_RUNTIME_ENV = "/tmp/keel-runtime.env"
DEFAULT_BINARY_NAME = "keel-x86_64-unknown-linux-musl"

_SESSION_COPY_NAME = "keel-session.jsonl"
_CHAT_LOG_NAME = "keel-chat.log"
_TRIAL_METADATA_NAME = "keel-trial.json"
_TRACE_NAME = "keel-trace.jsonl"
_TRACE_SCHEMA_VERSION = "1.0"
_DEFAULT_BENCHMARK = "terminal-bench-2.0"

# configure() removes and recreates the data directory. Never let a user point
# it at a shared system directory, a filesystem root, or a task workspace.
_PROTECTED_CONTAINER_PATHS = frozenset(
    {
        "/",
        "/tmp",
        "/app",
        "/testbed",
        "/logs",
        "/tests",
        "/solution",
        "/usr",
        "/var",
        "/etc",
        "/home",
        "/root",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/boot",
        "/dev",
        "/proc",
        "/sys",
        "/opt",
        "/srv",
        "/run",
        "/mnt",
        "/media",
    }
)
# configure() deletes the data directory, so a data directory may not live
# anywhere below these system roots. The one exception is /tmp and dedicated
# children of /tmp.
_PROTECTED_DATA_DIR_PREFIXES = (
    "/app",
    "/testbed",
    "/logs",
    "/tests",
    "/solution",
    "/usr",
    "/var",
    "/etc",
    "/home",
    "/root",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/boot",
    "/dev",
    "/proc",
    "/sys",
    "/opt",
    "/srv",
    "/run",
    "/mnt",
    "/media",
)

# The provider prefix of keel_target decides which environment variable names
# the keel process reads. Base URLs are not secrets, but keep them beside the
# key so a trial never inherits a stale endpoint from the image.
_PROVIDER_ENV: Mapping[str, tuple[str, tuple[str, ...]]] = {
    "neuralwatt": ("NEURALWATT_API_KEY", ("KEEL_NEURALWATT_BASE_URL",)),
    "openai": ("OPENAI_API_KEY", ("KEEL_OPENAI_BASE_URL", "OPENAI_BASE_URL")),
    "anthropic": (
        "ANTHROPIC_API_KEY",
        ("KEEL_ANTHROPIC_BASE_URL", "ANTHROPIC_BASE_URL"),
    ),
    "custom": ("KEEL_CUSTOM_API_KEY", ("KEEL_CUSTOM_BASE_URL",)),
}

# Providers Harbor can resolve automatically. An unparseable model slug keeps
# provider None, which is the normal case for the neuralwatt/... default.
_HARBOR_TO_KEEL_PROVIDER: Mapping[str, str] = {
    "neuralwatt": "neuralwatt",
    "openai": "openai",
    "openai-responses": "openai",
    "anthropic": "anthropic",
    "custom": "custom",
}

_NO_KEY_REQUIRED = frozenset({"custom"})

_SUMMARY_RE = re.compile(
    r"^\[(end|max_turns|cancelled|failed)\] "
    r"turns (?P<turns>\d+) in (?P<input>\d+) out (?P<output>\d+) "
    r"cache read (?P<cache_read>\d+) write (?P<cache_write>\d+)"
    r"(?: \| compact in (?P<compact_input>\d+) out (?P<compact_output>\d+))?\s*$",
    re.MULTILINE,
)
_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
_VERSION_RE = re.compile(r"\b(\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?)\b")
_SHA256_RE = re.compile(r"\b[0-9a-fA-F]{64}\b")


def _quote(value: object) -> str:
    return shlex.quote(str(value))


def _utc_now() -> str:
    return (
        datetime.now(timezone.utc)
        .isoformat(timespec="milliseconds")
        .replace("+00:00", "Z")
    )


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


class KeelAgent(BaseInstalledAgent):
    """Run one keel chat turn inside a Terminal-Bench task container."""

    capabilities = AgentCapabilities(base_url=True)

    MODEL_CONNECTION = ModelConnectionSpec(
        # neuralwatt/... is not a LiteLLM provider, so an unparsed slug lands
        # here and the agent-level candidate names select NEURALWATT_API_KEY.
        default_provider="neuralwatt",
        agent_provider_access=ProviderAccess(
            api_key_candidate_names=[
                "NEURALWATT_API_KEY",
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
                "KEEL_CUSTOM_API_KEY",
            ],
            base_url_candidate_names=[
                "KEEL_NEURALWATT_BASE_URL",
                "KEEL_OPENAI_BASE_URL",
                "OPENAI_BASE_URL",
                "KEEL_ANTHROPIC_BASE_URL",
                "ANTHROPIC_BASE_URL",
                "KEEL_CUSTOM_BASE_URL",
            ],
        ),
        supported_api_formats=[
            ModelApiFormat.OPENAI_CHAT_COMPLETIONS,
            ModelApiFormat.OPENAI_RESPONSES,
            ModelApiFormat.ANTHROPIC_MESSAGES,
        ],
        api_key_destination_names={
            "neuralwatt": "NEURALWATT_API_KEY",
            "openai": "OPENAI_API_KEY",
            "anthropic": "ANTHROPIC_API_KEY",
            "custom": "KEEL_CUSTOM_API_KEY",
            "default": "NEURALWATT_API_KEY",
        },
        base_url_destination_names={
            "neuralwatt": "KEEL_NEURALWATT_BASE_URL",
            "openai": "KEEL_OPENAI_BASE_URL",
            "anthropic": "KEEL_ANTHROPIC_BASE_URL",
            "custom": "KEEL_CUSTOM_BASE_URL",
            "default": "KEEL_NEURALWATT_BASE_URL",
        },
        default_api_format_by_provider={
            "neuralwatt": ModelApiFormat.OPENAI_CHAT_COMPLETIONS,
            "openai": ModelApiFormat.OPENAI_CHAT_COMPLETIONS,
            "openai-responses": ModelApiFormat.OPENAI_RESPONSES,
            "anthropic": ModelApiFormat.ANTHROPIC_MESSAGES,
            "custom": ModelApiFormat.OPENAI_CHAT_COMPLETIONS,
            "default": ModelApiFormat.OPENAI_CHAT_COMPLETIONS,
        },
    )

    @staticmethod
    def name() -> str:
        return "keel"

    def __init__(
        self,
        *args: Any,
        binary_path: str | os.PathLike[str] | None = None,
        rg_path: str | os.PathLike[str] | None = None,
        keel_target: str | None = DEFAULT_KEEL_TARGET,
        max_turns: int | str = DEFAULT_MAX_TURNS,
        remote_binary: str = DEFAULT_REMOTE_BINARY,
        data_dir: str = DEFAULT_DATA_DIR,
        benchmark: str = _DEFAULT_BENCHMARK,
        task_id: str | None = None,
        trial: int | str = 0,
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)

        benchmark = str(benchmark).strip()
        if not benchmark:
            raise ValueError("benchmark must be a non-empty trace label")
        self._benchmark = benchmark

        if task_id is None:
            self._task_id: str | None = None
        else:
            task_id = str(task_id).strip()
            if not task_id or "\x00" in task_id or "\n" in task_id:
                raise ValueError("task_id must be a non-empty single-line string")
            self._task_id = task_id

        try:
            self._trial = int(trial)
        except (TypeError, ValueError) as exc:
            raise ValueError(f"trial must be an integer, got {trial!r}") from exc
        if self._trial < 0:
            raise ValueError("trial must be at least 0")

        self._binary_path = self._resolve_host_path(
            binary_path,
            env_name="KEEL_BINARY",
            default=Path(__file__).resolve().parent / "bin" / DEFAULT_BINARY_NAME,
        )
        self._rg_path = (
            self._resolve_host_path(rg_path) if rg_path is not None else None
        )

        if keel_target is None or str(keel_target).strip().lower() == "auto":
            self._configured_target: str | None = None
            self._derive_target = True
        else:
            self._configured_target = self._validate_target(str(keel_target))
            self._derive_target = False

        try:
            self._max_turns = int(max_turns)
        except (TypeError, ValueError) as exc:
            raise ValueError(f"max_turns must be an integer, got {max_turns!r}") from exc
        if self._max_turns < 1:
            raise ValueError("max_turns must be at least 1")

        self._remote_binary = self._validate_remote_path(
            remote_binary, "remote_binary"
        )
        self._remote_rg = DEFAULT_REMOTE_RG
        self._data_dir = self._validate_remote_path(data_dir, "data_dir")
        self._runtime_env = DEFAULT_RUNTIME_ENV

        self._resolved_keel_target: str | None = self._configured_target
        self._keel_executable: str | None = None
        self._binary_sha256: str | None = None
        self._container_session_log: str | None = None
        self._wall_clock_s: float | None = None
        self._run_started_at: str | None = None
        self._run_finished_at: str | None = None
        self._run_output = ""
        self._run_error: str | None = None
        self._rg_ready = False

    # ------------------------------------------------------------------
    # Construction helpers
    # ------------------------------------------------------------------

    @staticmethod
    def _resolve_host_path(
        value: str | os.PathLike[str] | None,
        *,
        env_name: str | None = None,
        default: Path | None = None,
    ) -> Path:
        candidate: str | os.PathLike[str] | None = value
        if candidate is None and env_name is not None:
            candidate = os.environ.get(env_name)
        if candidate is None:
            if default is None:
                raise ValueError("a host path is required")
            return default
        text = os.fspath(candidate).strip()
        if not text:
            if default is not None:
                return default
            raise ValueError("a host path must not be empty")
        return Path(text).expanduser()

    @staticmethod
    def _validate_remote_path(value: str, name: str) -> str:
        text = str(value).strip()
        if not text.startswith("/"):
            raise ValueError(f"{name} must be an absolute container path, got {value!r}")
        if "\x00" in text or "\n" in text:
            raise ValueError(f"{name} must be a single path without control characters")
        if text.startswith("//"):
            raise ValueError(f"{name} must use a single leading slash, got {value!r}")

        path = PurePosixPath(text)
        if ".." in path.parts:
            raise ValueError(f"{name} must not contain '..', got {value!r}")
        normalized = path.as_posix()
        if normalized in _PROTECTED_CONTAINER_PATHS:
            raise ValueError(
                f"{name} {value!r} is a shared system path. Choose a dedicated "
                "subdirectory such as /tmp/keel-data."
            )
        if name == "data_dir" and any(
            normalized == prefix or normalized.startswith(prefix + "/")
            for prefix in _PROTECTED_DATA_DIR_PREFIXES
        ):
            raise ValueError(
                f"{name} {value!r} is below a shared system path. Choose a "
                "dedicated subdirectory such as /tmp/keel-data."
            )
        if name == "remote_binary" and text.endswith("/"):
            raise ValueError(f"{name} must name a file, not a directory: {value!r}")
        return normalized

    @staticmethod
    def _validate_data_dir_against_workspace(data_dir: str, workspace: str) -> None:
        try:
            data = PurePosixPath(posixpath.normpath(data_dir))
            work = PurePosixPath(posixpath.normpath(str(workspace).strip()))
        except (TypeError, ValueError) as exc:
            raise ValueError(
                f"cannot validate data_dir {data_dir!r} against workspace {workspace!r}"
            ) from exc
        if not work.is_absolute():
            return
        if data == work or data in work.parents:
            raise ValueError(
                f"data_dir {str(data)!r} is the task workspace or one of its "
                "parents. configure() removes and recreates data_dir; choose a "
                "dedicated directory such as /tmp/keel-data."
            )

    @staticmethod
    def _validate_data_dir_against_logs(data_dir: str, logs_dir: str) -> None:
        try:
            data = PurePosixPath(posixpath.normpath(data_dir))
            logs = PurePosixPath(posixpath.normpath(logs_dir))
        except (TypeError, ValueError) as exc:
            raise ValueError(
                f"cannot validate data_dir {data_dir!r} against logs dir {logs_dir!r}"
            ) from exc
        if not logs.is_absolute():
            return
        if data == logs or data in logs.parents:
            raise ValueError(
                f"data_dir {str(data)!r} is the Harbor agent logs directory or "
                "one of its parents. configure() removes and recreates data_dir; "
                "choose a dedicated directory such as /tmp/keel-data."
            )

    @staticmethod
    def _validate_target(value: str) -> str:
        text = value.strip()
        provider, separator, model = text.partition("/")
        if not separator or not provider or not model:
            raise ValueError(
                "keel_target must be `<provider>/<model>`, "
                f"for example {DEFAULT_KEEL_TARGET!r}; got {value!r}"
            )
        if provider not in _PROVIDER_ENV:
            supported = ", ".join(sorted(_PROVIDER_ENV))
            raise ValueError(
                f"keel_target provider {provider!r} is not supported by this "
                f"adapter; supported providers: {supported}. Use keel_target=custom/default "
                "with --base-url for another OpenAI-compatible endpoint."
            )
        return f"{provider}/{model}"

    # ------------------------------------------------------------------
    # Version reporting
    # ------------------------------------------------------------------

    def get_version_command(self) -> str | None:
        executable = self._keel_executable or self._remote_binary
        return f"{_quote(executable)} --version"

    def parse_version(self, stdout: str) -> str:
        text = _ANSI_RE.sub("", stdout or "").strip()
        match = _VERSION_RE.search(text)
        return match.group(1) if match else text

    # ------------------------------------------------------------------
    # Install and configure
    # ------------------------------------------------------------------

    async def install(self, environment: "BaseEnvironment") -> None:
        binary = self._binary_path
        if binary is None or not binary.is_file():
            raise FileNotFoundError(
                f"keel Linux binary not found at {binary}. Build it with the Docker "
                "command in benchmarks/harbor/README.md, or pass "
                "--ak binary_path=/path/to/keel (or set KEEL_BINARY)."
            )
        if binary.stat().st_size == 0:
            raise ValueError(f"keel binary at {binary} is empty")

        self._binary_sha256 = _sha256_file(binary)
        await self.exec_as_root(
            environment,
            command=f"mkdir -p {_quote(PurePosixPath(self._remote_binary).parent)}",
        )
        await environment.upload_file(binary, self._remote_binary)
        chmod = await self.exec_as_root(
            environment,
            command=(
                f"chmod 755 {_quote(self._remote_binary)} && "
                f"{_quote(self._remote_binary)} --version"
            ),
        )
        self._keel_executable = self._remote_binary
        self._version = self._parse_version_or_none(chmod.stdout)

        await self._install_rg(environment)

    async def _install_rg(self, environment: "BaseEnvironment") -> None:
        if self._rg_path is not None:
            if not self._rg_path.is_file():
                raise FileNotFoundError(
                    f"rg binary not found at {self._rg_path}; pass a static Linux "
                    "ripgrep build or omit rg_path to use the image's rg."
                )
            await self.exec_as_root(
                environment,
                command=f"mkdir -p {_quote(PurePosixPath(self._remote_rg).parent)}",
            )
            await environment.upload_file(self._rg_path, self._remote_rg)
            try:
                await self.exec_as_root(
                    environment,
                    command=(
                        f"chmod 755 {_quote(self._remote_rg)} && "
                        f"{_quote(self._remote_rg)} --version"
                    ),
                )
            except RuntimeError as exc:
                raise RuntimeError(
                    f"the uploaded rg at {self._rg_path} is not executable in the "
                    f"task container ({exc}); upload a static Linux ripgrep build"
                ) from exc
            self._rg_ready = True
            return

        if await self._rg_available(environment):
            self._rg_ready = True
            return

        # Do not assume apt: Harbor's helper probes apt-get, dnf, yum and apk.
        await self.ensure_system_dependencies(environment, ("ripgrep",))
        if await self._rg_available(environment):
            self._rg_ready = True
            return

        raise RuntimeError(
            "ripgrep (`rg`) is required by keel's search tool but is not on PATH "
            "and no package manager installed it. Pass --ak rg_path=/path/to/static/rg, "
            "bake rg into the image, or enable package installation for this task."
        )

    async def _rg_available(self, environment: "BaseEnvironment") -> bool:
        result = await self.exec_as_agent(
            environment,
            command="command -v rg >/dev/null 2>&1 && printf 'yes\\n' || printf 'no\\n'",
        )
        return "yes" in (result.stdout or "")

    async def configure(self, environment: "BaseEnvironment") -> None:
        logs_dir = PurePosixPath(self.environment_logs_dir)
        await self.exec_as_agent(
            environment,
            command=(
                f"mkdir -p {_quote(logs_dir)} && "
                f"test -w {_quote(logs_dir)}"
            ),
        )

        # KEEL_DATA_DIR must be writable by the agent user and empty at the
        # start of the trial so the captured JSONL cannot be a stale session.
        # Refuse any directory that could delete the workspace or a shared
        # system path before running the destructive command.
        self._validate_data_dir_against_workspace(
            self._data_dir, self._workspace(environment)
        )
        self._validate_data_dir_against_logs(
            self._data_dir, str(self.environment_logs_dir)
        )
        await self.exec_as_agent(
            environment,
            command=(
                f"rm -rf {_quote(self._data_dir)} && "
                f"mkdir -p {_quote(self._data_dir)} && "
                f"chmod 700 {_quote(self._data_dir)} && "
                f"test -w {_quote(self._data_dir)}"
            ),
        )

        await self._resolve_executable(environment)
        await self._detect_executable_version(environment)
        await self._verify_rg(environment)
        await self._detect_binary_checksum(environment)

        target = await self._resolve_target(environment)
        # Fail at setup, not mid-trial, if the target provider has no mapping.
        self._provider_env(target)
        await self._write_runtime_env(environment)

    async def _resolve_executable(self, environment: "BaseEnvironment") -> None:
        remote = _quote(self._remote_binary)
        command = (
            f"if [ -x {remote} ]; then printf 'keel-path:%s\\n' {remote}; "
            "elif command -v keel >/dev/null 2>&1; then "
            "printf 'keel-path:%s\\n' \"$(command -v keel)\"; "
            "else printf 'keel-path-missing\\n'; fi"
        )
        result = await self.exec_as_agent(environment, command=command)
        path = _marker_value(result.stdout, "keel-path:")
        if path is None:
            raise RuntimeError(
                f"keel executable not found at {self._remote_binary} or on PATH. "
                "With skip_install=true the image must already contain the binary; "
                "pass --ak remote_binary=/path/to/keel if it is elsewhere, or drop "
                "skip_install and pass --ak binary_path=... ."
            )
        self._keel_executable = path

    async def _detect_executable_version(
        self, environment: "BaseEnvironment"
    ) -> None:
        try:
            result = await self.exec_as_agent(
                environment,
                command=f"{_quote(self._keel_executable)} --version",
            )
        except RuntimeError as exc:
            raise RuntimeError(
                f"keel at {self._keel_executable} cannot run in this container: {exc}"
            ) from exc
        detected = self._parse_version_or_none(result.stdout)
        if detected:
            self._version = detected

    async def _verify_rg(self, environment: "BaseEnvironment") -> None:
        if await self._rg_available(environment):
            self._rg_ready = True
            return
        raise RuntimeError(
            "ripgrep (`rg`) is not available in the task container. With "
            "skip_install=true the image must provide it; otherwise drop "
            "skip_install, pass --ak rg_path=/path/to/static/rg, or let install() "
            "use the image's package manager."
        )

    async def _detect_binary_checksum(self, environment: "BaseEnvironment") -> None:
        executable = _quote(self._keel_executable)
        command = (
            'if command -v sha256sum >/dev/null 2>&1; then '
            f"sha256sum {executable} | awk '{{print $1}}'; "
            'elif command -v shasum >/dev/null 2>&1; then '
            f"shasum -a 256 {executable} | awk '{{print $1}}'; "
            'elif command -v openssl >/dev/null 2>&1; then '
            f"openssl dgst -sha256 -r {executable} | awk '{{print $1}}'; "
            "else printf '\\n'; fi"
        )
        result = await self.exec_as_agent(environment, command=command)
        match = _SHA256_RE.search(result.stdout or "")
        if match is None:
            if self._binary_sha256 is None:
                self.logger.warning(
                    "could not compute a sha256 for %s; trial metadata will omit it",
                    self._keel_executable,
                )
            return
        digest = match.group(0).lower()
        if self._binary_sha256 is not None and digest != self._binary_sha256:
            raise RuntimeError(
                "the keel binary in the container does not match the uploaded "
                f"artifact ({digest} != {self._binary_sha256})"
            )
        self._binary_sha256 = digest

    async def _write_runtime_env(self, environment: "BaseEnvironment") -> None:
        lines = [
            "# Generated per trial by Harbor's KeelAgent. No credentials are stored here.",
            f"export KEEL_DATA_DIR={_quote(self._data_dir)}",
            "export KEEL_MCP=0",
            "export NO_COLOR=1",
        ]
        command = (
            "printf '%s\\n' "
            + " ".join(_quote(line) for line in lines)
            + f" > {_quote(self._runtime_env)} && "
            f"chmod 600 {_quote(self._runtime_env)}"
        )
        await self.exec_as_agent(environment, command=command)

    # ------------------------------------------------------------------
    # Target resolution
    # ------------------------------------------------------------------

    async def _resolve_target(self, environment: "BaseEnvironment") -> str:
        if not self._derive_target and self._configured_target is not None:
            self._check_provider_consistency(self._configured_target)
            self._resolved_keel_target = self._configured_target
            return self._configured_target

        provider, model = self._derive_target_parts()
        target = self._validate_target(f"{provider}/{model}")
        workspace = self._workspace(environment)
        try:
            await self.exec_as_agent(
                environment,
                command=(
                    f"{_quote(self._keel_executable)} route {_quote(target)} "
                    f"--workspace {_quote(workspace)}"
                ),
                cwd=workspace,
            )
        except RuntimeError as exc:
            raise RuntimeError(
                f"derived keel target {target!r} is not in keel's catalog: {exc}. "
                "Pass --ak keel_target=<provider>/<model> with a target from "
                "`keel models`."
            ) from exc
        self._resolved_keel_target = target
        return target

    def _derive_target_parts(self) -> tuple[str, str]:
        connection = self.model_connection
        harbor_provider = (getattr(connection, "provider", None) or "").strip().lower()
        model_id = (getattr(connection, "model_id", None) or "").strip()

        if not model_id:
            model_id = (self.model_name or "").strip()

        if harbor_provider and harbor_provider != "harbor-skip-parsing":
            provider = _HARBOR_TO_KEEL_PROVIDER.get(harbor_provider)
            if provider is None:
                raise ValueError(
                    f"Harbor provider {harbor_provider!r} has no keel mapping; pass "
                    "--ak keel_target=<provider>/<model> explicitly, or use "
                    "keel_target=custom/default with --base-url."
                )
        else:
            candidate, separator, rest = model_id.partition("/")
            if not separator:
                raise ValueError(
                    "cannot derive a keel target from an unparsed model slug "
                    f"{self.model_name!r}; pass --ak keel_target=<provider>/<model>."
                )
            provider = candidate.strip().lower()
            if provider not in _PROVIDER_ENV:
                raise ValueError(
                    f"cannot derive a keel provider from {self.model_name!r}; "
                    "pass --ak keel_target=<provider>/<model>."
                )
            model_id = rest

        # LiteLLM returns the model id without the provider for known
        # providers; an unparsed slug keeps the full string.
        prefix, separator, rest = model_id.partition("/")
        if separator and prefix.strip().lower() == provider:
            model_id = rest
        if not model_id:
            raise ValueError(
                f"cannot derive a keel model from {self.model_name!r}; pass "
                "--ak keel_target=<provider>/<model>."
            )
        return provider, model_id

    def _check_provider_consistency(self, target: str) -> None:
        connection = self.model_connection
        harbor_provider = (getattr(connection, "provider", None) or "").strip().lower()
        if not harbor_provider or harbor_provider == "harbor-skip-parsing":
            return
        mapped = _HARBOR_TO_KEEL_PROVIDER.get(harbor_provider)
        target_provider = target.partition("/")[0]
        if target_provider == "custom":
            return
        if mapped is None:
            raise ValueError(
                f"Harbor provider {harbor_provider!r} has no keel mapping and "
                f"keel_target is {target!r}; pass keel_target=custom/default with "
                "--base-url for an arbitrary OpenAI-compatible endpoint."
            )
        if mapped != target_provider:
            raise ValueError(
                f"Harbor resolved provider {harbor_provider!r} but keel_target is "
                f"{target!r}. Pass a matching keel_target (or custom/default) so the "
                "resolved credential is sent to the provider keel actually calls."
            )

    # ------------------------------------------------------------------
    # Run
    # ------------------------------------------------------------------

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: "BaseEnvironment",
        context: "AgentContext",
    ) -> None:
        if "\x00" in instruction:
            raise ValueError("the task instruction contains a NUL byte")
        target = self._resolved_keel_target
        if target is None:
            raise RuntimeError("configure() must resolve a keel target before run()")
        if self._keel_executable is None:
            raise RuntimeError("configure() must locate the keel executable before run()")

        workspace = self._workspace(environment)
        env = self._runtime_env_dict(target)
        chat_log = self._logs_path(_CHAT_LOG_NAME)
        command = (
            "set -eu; set -o pipefail; "
            f"set -a; . {_quote(self._runtime_env)}; set +a; "
            f"{_quote(self._keel_executable)} chat {_quote(target)} "
            f"--workspace {_quote(workspace)} "
            f"--prompt {_quote(instruction)} --yes --max-turns {self._max_turns} "
            f"< /dev/null 2>&1 | tee {_quote(chat_log)}"
        )

        self._run_started_at = _utc_now()
        started = time.monotonic()
        try:
            result = await self.exec_as_agent(
                environment,
                command=command,
                env=env,
                cwd=workspace,
            )
            self._run_output = result.stdout or ""
        except BaseException as exc:
            self._run_error = str(exc) or type(exc).__name__
            try:
                await self._collect_session(environment)
            except Exception:  # pragma: no cover - log capture is best effort
                self.logger.exception("failed to collect the keel session log")
            raise
        else:
            await self._collect_session(environment)
        finally:
            self._wall_clock_s = time.monotonic() - started
            self._run_finished_at = _utc_now()

    def _runtime_env_dict(self, target: str) -> dict[str, str]:
        api_key_env, base_url_envs = self._provider_env(target)
        connection = self.model_connection
        env = {
            "KEEL_DATA_DIR": self._data_dir,
            "KEEL_MCP": "0",
            "NO_COLOR": "1",
        }
        api_key = getattr(connection, "api_key", None)
        if api_key:
            env[api_key_env] = api_key
        elif target.partition("/")[0] not in _NO_KEY_REQUIRED:
            raise RuntimeError(
                f"no API key was resolved for keel target {target!r}. Set "
                f"{api_key_env} on the host (or pass --agent-env {api_key_env}=...). "
                "For a keyless OpenAI-compatible endpoint use keel_target=custom/default."
            )
        base_url = getattr(connection, "execution_base_url", None)
        if base_url:
            for name in base_url_envs:
                env[name] = base_url
        return env

    @staticmethod
    def _provider_env(target: str) -> tuple[str, tuple[str, ...]]:
        provider = target.partition("/")[0]
        try:
            return _PROVIDER_ENV[provider]
        except KeyError as exc:
            supported = ", ".join(sorted(_PROVIDER_ENV))
            raise ValueError(
                f"keel target provider {provider!r} is not supported by this "
                f"adapter; supported providers: {supported}."
            ) from exc

    @staticmethod
    def _workspace(environment: "BaseEnvironment") -> str:
        config = getattr(environment, "task_env_config", None)
        workdir = getattr(config, "workdir", None)
        return str(workdir).strip() if workdir else "/app"

    async def _collect_session(self, environment: "BaseEnvironment") -> None:
        """Copy keel's newest session JSONL into the mounted agent logs dir."""
        chat_log = self._logs_path(_CHAT_LOG_NAME)
        destination = self._logs_path(_SESSION_COPY_NAME)
        sessions = f"{self._data_dir.rstrip('/')}/sessions"
        command = (
            "set -u; "
            "src=''; "
            f"if [ -f {_quote(chat_log)} ]; then "
            f"src=$(sed -n 's/^keel: session log //p' {_quote(chat_log)} | tail -n 1); "
            "fi; "
            f'if [ -z "$src" ] || [ ! -f "$src" ]; then '
            f'src=$(ls -t {_quote(sessions)}/*/*.jsonl 2>/dev/null | head -n 1 || true); '
            "fi; "
            f'if [ -n "$src" ] && [ -f "$src" ]; then '
            f"cp \"$src\" {_quote(destination)} && "
            f"printf 'keel-session-source:%s\\n' \"$src\"; "
            "fi"
        )
        result = await self.exec_as_agent(environment, command=command)
        source = _marker_value(result.stdout, "keel-session-source:")
        if source is not None:
            self._container_session_log = source

    def _logs_path(self, name: str) -> str:
        return (PurePosixPath(self.environment_logs_dir) / name).as_posix()

    # ------------------------------------------------------------------
    # Trial metadata
    # ------------------------------------------------------------------

    def populate_context_post_run(self, context: "AgentContext") -> None:
        session_path = self.logs_dir / _SESSION_COPY_NAME
        chat_path = self.logs_dir / _CHAT_LOG_NAME

        chat_text = self._run_output
        if not chat_text and chat_path.is_file():
            try:
                chat_text = chat_path.read_text(encoding="utf-8", errors="replace")
            except OSError:
                chat_text = ""
        summary = self._parse_summary(chat_text)
        session = self._parse_session(session_path)

        target = (
            self._resolved_keel_target
            or self._configured_target
            or DEFAULT_KEEL_TARGET
        )
        tokens = session["tokens"] if session else None
        if tokens is None and summary is not None:
            tokens = summary["tokens"]
        turns = session["turns"] if session else None
        if turns is None and summary is not None:
            turns = summary["turns"]
        stop_reason = summary["stop_reason"] if summary else None
        if stop_reason is None and session:
            stop_reason = session["stop_reason"]

        trace = self._build_trace(
            target=target,
            session=session,
            summary=summary,
            turns=turns,
            tokens=tokens,
            session_path=session_path,
        )
        self._write_trace(trace)

        metadata: dict[str, Any] = {
            "kind": "keel-harbor-trial",
            "format_version": 1,
            "agent": self.name(),
            "agent_version": self.version(),
            "harbor_model": self.model_name,
            "model": self.model_name,
            "keel_target": target,
            "keel_provider": session["provider"] if session else target.partition("/")[0],
            "keel_model": session["model"] if session else target.partition("/")[2],
            "protocol": session["protocol"] if session else None,
            "binary_sha256": self._binary_sha256,
            "binary_path": self._keel_executable or self._remote_binary,
            "turns": turns,
            "tool_calls": session["tool_calls"] if session else None,
            "tokens": tokens,
            "summary_tokens": summary["tokens"] if summary else None,
            "wall_clock_s": (
                round(self._wall_clock_s, 3)
                if self._wall_clock_s is not None
                else None
            ),
            "stop_reason": stop_reason,
            "start_time": self._run_started_at,
            "end_time": self._run_finished_at,
            "max_turns": self._max_turns,
            "session_log_path": str(session_path),
            "container_session_log_path": self._container_session_log,
            "session_id": session["session_id"] if session else None,
            "trace_path": str(self.logs_dir / _TRACE_NAME),
            "error": self._run_error,
        }

        self._write_trial_metadata(metadata)

        if tokens:
            context.n_input_tokens = self._harbor_input_tokens(
                tokens,
                metadata["protocol"],
            )
            context.n_cache_tokens = tokens["cache_read"] + tokens["cache_write"]
            context.n_output_tokens = tokens["output"]
        context.metadata = {"keel": metadata, "trace": trace}

    def _build_trace(
        self,
        *,
        target: str,
        session: Mapping[str, Any] | None,
        summary: Mapping[str, Any] | None,
        turns: int | None,
        tokens: Mapping[str, int] | None,
        session_path: Path,
    ) -> dict[str, Any]:
        """Build the compact record defined by the benchmark trace schema.

        Harbor runs its verifier after the agent, so the adapter cannot observe
        the final verifier result. The trace says ``not_run`` and leaves the
        patch fields empty; Harbor's ``results.json`` remains authoritative for
        verification.
        """
        now = _utc_now()
        start_time = self._run_started_at or now
        end_time = self._run_finished_at or start_time
        if end_time < start_time:
            end_time = start_time
        wall_clock_s = (
            max(0.0, round(self._wall_clock_s, 3))
            if self._wall_clock_s is not None
            else 0.0
        )
        token_values = tokens or {}
        patch = ""

        return {
            "schema_version": _TRACE_SCHEMA_VERSION,
            "benchmark": self._benchmark,
            "task_id": self._trace_task_id(),
            "trial": self._trial,
            "model": str(self.model_name or target),
            "model_version": None,
            "keel_target": target,
            "agent_version": self.version(),
            "binary_sha256": self._binary_sha256,
            "start_time": start_time,
            "end_time": end_time,
            "turns": max(0, int(turns or 0)),
            "tool_calls": max(
                0, int(session["tool_calls"]) if session else 0
            ),
            "input_tokens": max(0, int(token_values.get("input", 0))),
            "output_tokens": max(0, int(token_values.get("output", 0))),
            "reasoning_tokens": max(0, int(token_values.get("reasoning", 0))),
            "cost_usd": None,
            "stop_reason": self._trace_stop_reason(summary, session),
            "wall_clock_s": wall_clock_s,
            "patch": patch,
            "patch_size": len(patch.encode("utf-8")),
            "patch_apply_ok": None,
            "verifier_result": "not_run",
            "failure_tags": [],
            "session_log_path": str(session_path) if session_path.is_file() else None,
            "error": self._run_error,
        }

    def _trace_task_id(self) -> str:
        if self._task_id is not None:
            return self._task_id

        config = self._read_trial_config()
        task = config.get("task")
        if isinstance(task, dict):
            name = task.get("name")
            if isinstance(name, str) and name.strip():
                return name.strip()
            path = task.get("path")
            if isinstance(path, str):
                name = Path(path.rstrip("/")).name
                if name:
                    return name

        trial_name = config.get("trial_name")
        if not isinstance(trial_name, str) or not trial_name.strip():
            trial_name = getattr(self, "session_id", None)
        if isinstance(trial_name, str):
            stem = trial_name.strip()
            if stem.endswith("__agent"):
                stem = stem[: -len("__agent")]
            short_name = stem.split("__", 1)[0]
            if short_name:
                return short_name
        return "unknown"

    def _read_trial_config(self) -> Mapping[str, Any]:
        path = Path(self.logs_dir).parent / "config.json"
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError, UnicodeDecodeError):
            return {}
        return data if isinstance(data, dict) else {}

    def _trace_stop_reason(
        self,
        summary: Mapping[str, Any] | None,
        session: Mapping[str, Any] | None,
    ) -> str:
        if self._run_error:
            return "error"
        raw: Any = summary.get("stop_reason") if summary else None
        if not raw and session:
            raw = session.get("stop_reason")
        if not raw:
            raw = "completed" if self._run_finished_at else "unknown"
        text = str(raw).strip() or "unknown"
        return {
            "end": "completed",
            "end_turn": "completed",
            "stop_sequence": "completed",
            "failed": "error",
        }.get(text, text)

    def _write_trace(self, trace: Mapping[str, Any]) -> None:
        path = self.logs_dir / _TRACE_NAME
        temporary = path.with_suffix(path.suffix + ".tmp")
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            temporary.write_text(
                json.dumps(trace, ensure_ascii=False, separators=(",", ":")) + "\n",
                encoding="utf-8",
            )
            os.replace(temporary, path)
        except OSError:
            self.logger.exception("failed to write keel trace to %s", path)

    def _write_trial_metadata(self, metadata: Mapping[str, Any]) -> None:
        path = self.logs_dir / _TRIAL_METADATA_NAME
        temporary = path.with_suffix(path.suffix + ".tmp")
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            temporary.write_text(
                json.dumps(metadata, indent=2, sort_keys=True) + "\n",
                encoding="utf-8",
            )
            os.replace(temporary, path)
        except OSError:
            self.logger.exception("failed to write keel trial metadata to %s", path)

    @staticmethod
    def _harbor_input_tokens(tokens: Mapping[str, int], protocol: str | None) -> int:
        """Harbor's n_input_tokens includes cache tokens.

        Anthropic reports input, cache read and cache write separately; OpenAI
        reports cache reads inside input_tokens, so adding them would double
        count. keel keeps the protocol in the session header.
        """
        input_tokens = tokens["input"]
        if protocol == "anthropic-messages":
            return input_tokens + tokens["cache_read"] + tokens["cache_write"]
        return input_tokens

    @staticmethod
    def _parse_summary(text: str) -> dict[str, Any] | None:
        clean = _ANSI_RE.sub("", text or "")
        matches = list(_SUMMARY_RE.finditer(clean))
        if not matches:
            return None
        match = matches[-1]
        stop = match.group(1)
        return {
            "stop_reason": "end_turn" if stop == "end" else stop,
            "turns": int(match.group("turns")),
            "tokens": {
                "input": int(match.group("input")),
                "output": int(match.group("output")),
                "cache_read": int(match.group("cache_read")),
                "cache_write": int(match.group("cache_write")),
                "reasoning": 0,
                "compaction_input": int(match.group("compact_input") or 0),
                "compaction_output": int(match.group("compact_output") or 0),
            },
        }

    @staticmethod
    def _parse_session(path: Path) -> dict[str, Any] | None:
        if not path.is_file():
            return None
        meta: dict[str, Any] = {}
        messages: list[dict[str, Any]] = []
        try:
            lines = path.read_text(encoding="utf-8").splitlines()
        except OSError:
            return None
        for line in lines:
            if not line.strip():
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(record, dict):
                continue
            kind = record.get("type")
            if kind == "meta" and not meta:
                meta = record
            elif kind == "message" and isinstance(record.get("message"), dict):
                messages.append(record["message"])

        if not messages and not meta:
            return None

        tool_calls = 0
        assistant_turns = 0
        last_stop: str | None = None
        totals = {
            "input": 0,
            "output": 0,
            "cache_read": 0,
            "cache_write": 0,
            "reasoning": 0,
        }
        for message in messages:
            if message.get("role") != "assistant":
                continue
            assistant_turns += 1
            if message.get("stop"):
                last_stop = str(message["stop"])
            usage = message.get("usage")
            if isinstance(usage, dict):
                for field, source in (
                    ("input", "input_tokens"),
                    ("output", "output_tokens"),
                    ("cache_read", "cache_read_tokens"),
                    ("cache_write", "cache_write_tokens"),
                    ("reasoning", "reasoning_tokens"),
                ):
                    value = usage.get(source)
                    if isinstance(value, int):
                        totals[field] += value
            parts = message.get("parts")
            if isinstance(parts, list):
                for part in parts:
                    content = part.get("content") if isinstance(part, dict) else None
                    if isinstance(content, dict) and content.get("type") == "tool_call":
                        tool_calls += 1

        totals["compaction_input"] = 0
        totals["compaction_output"] = 0
        return {
            "session_id": meta.get("id"),
            "provider": meta.get("provider"),
            "model": meta.get("model"),
            "protocol": meta.get("protocol"),
            "workspace": meta.get("workspace"),
            "turns": assistant_turns,
            "tool_calls": tool_calls,
            "stop_reason": last_stop,
            "tokens": totals,
        }

    @staticmethod
    def _parse_version_or_none(stdout: str | None) -> str | None:
        if not stdout:
            return None
        text = _ANSI_RE.sub("", stdout).strip()
        match = _VERSION_RE.search(text)
        return match.group(1) if match else (text or None)


def _marker_value(stdout: str | None, marker: str) -> str | None:
    """Return the last non-empty value after ``marker`` on its own line."""
    value: str | None = None
    for line in (stdout or "").splitlines():
        if line.startswith(marker):
            candidate = line[len(marker) :].strip()
            if candidate:
                value = candidate
    return value
