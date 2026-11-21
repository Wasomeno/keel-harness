# Harbor adapter for keel

`keel_agent.py` runs keel as an installed agent in a
[Harbor](https://docs.harborframework.com/) Terminal-Bench task container. It
implements `BaseInstalledAgent`:

- `install()` uploads a Linux `keel` binary and makes `rg` available.
- `configure()` prepares a writable data directory and a non-secret runtime env
  file, locates the binary, records its version and checksum, and validates the
  keel target. It runs after `install()` and also with `--ak skip_install=true`.
- `run()` executes one non-interactive turn in the task workspace:

  ```
  keel chat <keel_target> --workspace <task-dir> --prompt <instruction> --yes --terminal --max-turns <N>
  ```

  The adapter never executes tools on the host and never runs the task
  verifier. Harbor owns the verifier.

## Build the Linux binary

The default path is
`benchmarks/harbor/bin/keel-x86_64-unknown-linux-musl`. A static musl build
avoids glibc differences between Terminal-Bench images. Build it in Docker from
the repository root; the binary is deliberately not committed:

```bash
mkdir -p benchmarks/harbor/bin
docker run --rm --platform linux/amd64 -v "$PWD":/src -w /src \
  rust:1.85-bookworm bash -lc '
  set -euo pipefail
  export PATH=/usr/local/cargo/bin:$PATH
  export RUSTUP_HOME=/usr/local/rustup
  export CARGO_HOME=/usr/local/cargo
  apt-get update
  apt-get install -y --no-install-recommends musl-tools
  rustup target add x86_64-unknown-linux-musl
  CC_x86_64_unknown_linux_musl=musl-gcc \
    cargo build --release -p keel-cli --target x86_64-unknown-linux-musl
  cp target/x86_64-unknown-linux-musl/release/keel \
     benchmarks/harbor/bin/keel-x86_64-unknown-linux-musl
'
sha256sum benchmarks/harbor/bin/keel-x86_64-unknown-linux-musl
```

For an arm64 image, build `aarch64-unknown-linux-musl` and pass
`--ak binary_path=...`; the adapter does not assume an architecture.

`rg` is required because keel's `search` tool currently shells out to ripgrep.
Either pass a static Linux ripgrep with `--ak rg_path=...`, bake `rg` into the
image, or let `install()` try the image's `apt-get`, `dnf`, `yum` or `apk`. The
adapter fails with a clear message when none of those paths works.

## Run a task

Install Harbor in a throwaway environment, set the provider key, and point
Harbor at the adapter module. `PYTHONPATH` makes `keel_agent` importable
without adding an `__init__.py` that would shadow Harbor's own package:

```bash
python3 -m venv .venv-harbor
. .venv-harbor/bin/activate
pip install harbor

export NEURALWATT_API_KEY=...            # never commit this
PYTHONPATH=benchmarks/harbor harbor run \
  -t hello-world/hello-world \
  -a keel_agent:KeelAgent \
  -m neuralwatt/deepseek-v4.1-flash \
  --ak binary_path="$PWD/benchmarks/harbor/bin/keel-x86_64-unknown-linux-musl" \
  --ak rg_path=/path/to/static/rg
```

Harbor resolves the model credential and base URL through `self.model_connection`
from host environment variables or `--agent-env`/`--base-url`, then the adapter
maps them to the environment keel reads:

| keel provider | key variable | base URL variables |
| --- | --- | --- |
| `neuralwatt` | `NEURALWATT_API_KEY` | `KEEL_NEURALWATT_BASE_URL` |
| `openai` | `OPENAI_API_KEY` | `KEEL_OPENAI_BASE_URL`, `OPENAI_BASE_URL` |
| `anthropic` | `ANTHROPIC_API_KEY` | `KEEL_ANTHROPIC_BASE_URL`, `ANTHROPIC_BASE_URL` |
| `custom` | `KEEL_CUSTOM_API_KEY` (optional) | `KEEL_CUSTOM_BASE_URL` |

The adapter forwards the key under the destination name in the table. It
forwards the resolved base URL under the `KEEL_*_BASE_URL` name keel's catalog
reads and, for `openai` and `anthropic`, also under `OPENAI_BASE_URL` or
`ANTHROPIC_BASE_URL` so another client in the container sees the same endpoint.
It does not forward unrelated provider variables. Harbor's candidate lists are
agent-wide, so the adapter also checks where the resolved values came from: a
key selected from `OPENAI_API_KEY`, or a URL selected from `OPENAI_BASE_URL`,
is rejected for a neuralwatt target before any container command runs, and a
neuralwatt key is rejected for a custom target. An explicit `--base-url` and a
matching provider variable are accepted.

The credential is passed as a per-exec environment value. Harbor may implement
that as `docker compose exec -e KEY=VALUE`, so the value can appear briefly in
the host process table; the adapter never writes it to the image, the runtime
env file, or a log.

The default `keel_target` is `neuralwatt/deepseek-v4.1-flash`, which is a keel
catalog route rather than a LiteLLM slug. Harbor's model slug is still recorded
in trial metadata. If `keel_target` is omitted or set to `auto`, the adapter
derives `<provider>/<model>` from the resolved Harbor connection. Both explicit
and derived targets are checked with `keel route <target> --workspace <dir>`
during `configure()`, so a typo or `custom/default` without a base URL fails
before the model call. An explicit target that does not match the resolved
Harbor provider is also rejected before the model call so a provider credential
is not sent to a different endpoint.

## Agent kwargs

| kwarg | default | meaning |
| --- | --- | --- |
| `binary_path` | `$KEEL_BINARY` or `benchmarks/harbor/bin/keel-x86_64-unknown-linux-musl` | Host path uploaded to the container. |
| `rg_path` | none | Host path of a static Linux `rg`; uploaded and chmodded. |
| `keel_target` | `neuralwatt/deepseek-v4.1-flash` | Explicit keel route. `None`/`auto` derives and validates from the Harbor model. |
| `max_turns` | `60` | Passed to `keel chat --max-turns`. |
| `remote_binary` | `/usr/local/bin/keel` | Container install path, or the baked path with `skip_install=true`. |
| `data_dir` | `/tmp/keel-data` | Writable `KEEL_DATA_DIR`; recreated at configure time. |
| `benchmark` | `terminal-bench-2.0` | Label for the trace's `benchmark` field. |
| `task_id` | derived | Trace task ID. Defaults to `task.name` or the task path from Harbor's trial `config.json`, then the trial-name prefix. |
| `trial` | `0` | Trace trial index. Set it per job when a run has multiple trials. |

`data_dir` must be a dedicated directory. `configure()` removes and recreates
it, so the adapter rejects `/`, `/tmp` itself, `/app`, `/testbed`, `/logs`,
`/tests`, `/solution`, system roots such as `/usr`, `/var`, `/etc`, `/home` and
`/root`, any path below those roots, any path containing `..`, any path equal
to or above the task workspace, and any path equal to or above Harbor's agent
logs directory. Dedicated children of `/tmp`, including the default
`/tmp/keel-data`, are allowed. `remote_binary` gets the absolute-path, no-`..`
and system-root checks, and must name a file rather than a directory. Pass the
trace identity with `--ak benchmark=...`, `--ak task_id=...`, and
`--ak trial=N` when the defaults do not fit the run.

Harbor's own installed-agent kwargs (`skip_install`, `version`,
`prompt_template_path`, `base_url`) still apply. With
`--ak skip_install=true` the image must already provide `keel` at
`remote_binary` or on `PATH`, and must provide `rg`:

```bash
PYTHONPATH=benchmarks/harbor harbor run \
  -t hello-world/hello-world \
  -a keel_agent:KeelAgent \
  -m neuralwatt/deepseek-v4.1-flash \
  --ak skip_install=true \
  --ak remote_binary=/usr/local/bin/keel
```

## Trial output

Every trial writes these files into Harbor's agent logs directory:

- `keel-session.jsonl`: a copy of the session log keel wrote under
  `KEEL_DATA_DIR`.
- `keel-chat.log`: stdout and stderr of the one `keel chat` invocation.
- `keel-trace.jsonl`: one JSON object in the benchmark trace schema v1.0, with
  exactly the 26 required fields (`schema_version`, `benchmark`, `task_id`,
  `trial`, `model`, `model_version`, `keel_target`, `agent_version`,
  `binary_sha256`, `start_time`, `end_time`, `turns`, `tool_calls`,
  `input_tokens`, `output_tokens`, `reasoning_tokens`, `cost_usd`,
  `stop_reason`, `wall_clock_s`, `patch`, `patch_size`, `patch_apply_ok`,
  `verifier_result`, `failure_tags`, `session_log_path`, `error`).
- `keel-trial.json`: Harbor-specific metadata with `kind`
  `keel-harbor-trial`, full provider/protocol/session details, the complete
  token map (input, output, cache, reasoning), the container session path, and a
  pointer to the trace. It is deliberately not a trace record and is not
  schema-validated.

The trace leaves `patch` empty and `patch_apply_ok` null because Harbor applies
the patch and runs the verifier after the agent. Its `verifier_result` is
`not_run`; this is an agent-phase trace, not a Harbor result. Harbor's
`results.json` remains authoritative for the verifier outcome. Merging that
result into the trace after verification is a follow-up, not part of this
adapter. `cost_usd` is null because keel does not report a price and the
repository has no per-model price table. The adapter does fill Harbor's
`AgentContext` input/output/cache token fields.

`start_time`, `end_time` and `wall_clock_s` cover the `keel chat` phase, not
the full Harbor trial (image build, setup, and verification are outside them).
The trace's `input_tokens` excludes cache reads: keel's OpenAI protocols include
cached prompt tokens in their input count, so the adapter subtracts the
reported cache reads there; Anthropic already reports them separately. If the
session copy is missing, the adapter uses the protocol printed by `keel route`
during `configure()`; if that line cannot be read, it assumes
`anthropic-messages` for Anthropic and `openai-chat` for the other providers.
Harbor's `AgentContext.n_input_tokens` follows Harbor's convention instead and
adds cache tokens for the `anthropic-messages` protocol; `n_cache_tokens`
carries cache reads plus writes. A cancelled or timed-out chat writes
`stop_reason="timeout"`, `error="agent cancelled or timed out"`, and
`failure_tags=["timeout"]`. A Harbor verifier timeout happens after the
agent has finished, so the adapter's trace can still say `completed` in that
case.

Harbor's current `BaseInstalledAgent` API does not expose the trial index to
the agent, so the trace defaults `trial` to `0`. For a multi-trial run, pass
`--ak trial=N` per job or rewrite the field from Harbor's trial order when
collecting traces. `task_id` comes from Harbor's per-trial `config.json`; pass
`--ak task_id=...` when a local task path does not carry the subset ID.

Use the exact trace glob `*/agent/keel-trace.jsonl` when collecting a job. A
broader `*.jsonl` glob also matches `keel-session.jsonl`, which is a transcript
and will fail the trace reader. The Harbor metadata sidecar records the same
path at `agent_result.metadata.keel.trace_path`.

## Live smoke

On 2026-11-21, Harbor 0.27.0 with Python 3.13 ran
`hello-world/hello-world` through this adapter. The smoke used the x86_64 musl
binary under Rosetta in an arm64 hello-world image; a native aarch64 artifact
is still unbuilt. The agent phase completed: keel created `/app/hello.txt` and
exited `[end]`; the trial wrote `keel-trace.jsonl` (26 fields, turns 3, tool
calls 2, input 5630, output 169, reasoning 3), `keel-trial.json`,
`keel-session.jsonl`, and `keel-chat.log`. Harbor's own verifier then timed out
after 120 seconds in `tests/test.sh` while running `apt-get update` and
installing `uv`; the adapter had already finished and the trial exception was
`VerifierTimeoutError`. That smoke therefore covers the adapter path, not a
verifier pass or fail. The trace correctly says `verifier_result="not_run"`.

## Tests

The unit tests stub Harbor and drive a fake environment, so they need neither
Harbor nor Docker:

```bash
python3 -m unittest discover -s benchmarks/tests -v
```

## Limitations

- Linux only; x86_64 musl is the default artifact. The adapter declares no
  Windows capability.
- `rg` must exist in the image, be uploaded with `rg_path`, or be installable by
  a package manager. keel has no `grep -rn` fallback yet.
- The instruction is passed as an argv value. Very large instructions can hit
  the kernel's argument-size limit; Terminal-Bench instructions are far below
  it.
- One `keel chat --prompt` turn per trial. There is no resume, streaming
  trajectory, or ATIF conversion.
- `--terminal` opts into keel's persistent shell, so the image needs `bash` and
  a working PTY (`/dev/ptmx`). stdout and stderr arrive together in that shell.
  If the image cannot allocate a PTY, pass a binary without the tool or drop the
  flag; the default `bash` tool remains one-shot.
- MCP is forcibly disabled (`KEEL_MCP=0`, no `--mcp`), and no web tool is
  exposed.
- A `skip_install=true` image needs both `keel` and `rg`; the adapter only
  verifies them.
- The trace's `verifier_result` is always `not_run` and its patch fields are
  empty because the adapter cannot observe Harbor's post-agent verification;
  the trace is agent-phase only. Merging Harbor's `results.json` into the trace
  is a follow-up.
- Harbor does not pass a trial index to the agent, so `keel-trace.jsonl`
  defaults `trial` to `0`; see the trial-output section for the override.
- The session JSONL is copied after the process exits. A kill during the turn
  can lose the final turn, though the CLI appends completed messages as it goes.
- The adapter targets Harbor's current `BaseInstalledAgent` API
  (`install`/`configure`/`run`/`populate_context_post_run`). Re-check it when
  Harbor changes.
