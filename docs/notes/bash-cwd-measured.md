# Persistent `bash` cwd: measured on the ten-task Terminal-Bench subset

**Date:** 2026-11-22
**Status:** measurement complete
**Question:** after `f29db3f`, does the persistent one-shot `bash` working
directory stop the model re-issuing `cd`?
**Short answer:** no. The `cd`-prefix share barely moved, most prefixes were
still `cd /app`, and no turn-saving claim follows from this run.

The feature itself is described in [`bash-cwd.md`](bash-cwd.md). This note is
the measurement, not a design change. The design note was corrected in
[`9d3800a`](https://github.com/Wasomeno/keel-harness/commit/9d3800a) (PR #51);
the four documents now agree that the `cd` cost was tokens, not turns.

**M13.2 update:** `terminal` has since been removed and the Harbor adapter now
passes `--jobs`; this note keeps the run under the then-current flag.

## What was measured

Main was `f29db3f` (`Merge pull request #50 from Wasomeno/m13-bash-cwd`). In
that tree a one-shot `bash` call starts in the directory the previous call
finished in, the directory is restored across `keel chat --resume`, and the
tool description tells the model that it persists. Windows is unchanged and
is not covered here.

The run used the same ten Terminal-Bench 2.0 tasks as wave 1, in the same
order: `fix-git`, `extract-elf`, `regex-log`,
`nginx-request-logging`, `custom-memory-heap-crash`, `query-optimize`,
`modernize-scientific-stack`, `kv-store-grpc`, `fix-code-vulnerability`,
`cancel-async-tasks`. The model was
`neuralwatt/deepseek-v4.1-flash`, with one trial per task, `max_turns=60`,
`--n-concurrent 1`, `--verifier-timeout-multiplier 3`, restricted egress as
the Harbor adapter configures it by default, and the same `-y` environment
delete, so each task's images were removed before the next task started.

The Harbor command was copied from the wave-1 invocation, not reconstructed:

```text
harbor run -t terminal-bench/<task> \
  -a keel_agent:KeelAgent \
  -m neuralwatt/deepseek-v4.1-flash \
  --ak binary_path=/tmp/keel-linux-target/keel-x86_64-unknown-linux-musl \
  --ak max_turns=60 \
  --n-concurrent 1 \
  --verifier-timeout-multiplier 3 \
  -o /tmp/keel-tb2-cwd-wave2 \
  -y
```

`PYTHONPATH` was the repository's `benchmarks/harbor` directory. The task
timeout was 1,920 s per task and the sweep cap was 120 minutes. The run
finished on 2026-11-22, 07:21:49-08:52:17Z, 82.6 minutes after the first task
started.

The binary was built from the clean `f29db3f` checkout with the wave-1 build
script: `rust:1.85-bookworm`, `musl-tools`, the
`x86_64-unknown-linux-musl` target, and `/tmp/keel-linux-target` as the Docker
target mount. Its sha256 is
`7f76d7619c1e51799b4b815a06e7d329ff0eedbafd168cecea97f8f139da5900`; `file`
reports `ELF 64-bit LSB pie executable, x86-64 ... static-pie linked`. The
wave-1 binary was `f7ffb7935e09152bf3fbd7b212ce22f71144a9573c36dde6c017710eab2fd39a`.

Raw output is in `/tmp/keel-tb2-cwd-wave2`; the unchanged wave-1 baseline is
in `/tmp/keel-tb2-iteration-wave1`. The counting parser reads the assistant
`tool_call` parts in `agent/keel-session.jsonl`, parses the `bash` argument
object, and counts the `command`. It reproduces the wave-1 baseline exactly:
339 tool calls, 288 `bash`, 192 `cd <dir> &&` prefixes, 2,553 prefix
characters, 255 turns, 62.6 agent minutes, and 3,498,481 input tokens.

### The 197 figure needs its breakdown

Wave 1 had **197 of 288** `bash` calls that contained a `cd` somewhere, but
**192** of those began with `cd <dir> && <command>`, and **zero** were a bare
`cd`. The 192 prefixes totalled 2,553 characters, roughly 640 tokens for the
whole ten-task run. About 120 of the prefixes were `cd /app`, the directory the
call was already in. One call used the `workdir` argument.

Those were not 197 wasted turns. A `cd /app &&` prefix is a few tokens inside
a call the model was already making. This note therefore uses two different
numbers throughout:

- **`cd`-prefix calls**: the 192 wave-1 calls that begin `cd <dir> &&`.
- **any `cd`**: the broader 197 wave-1 calls that contain a `cd` anywhere.

The feature's case is resume continuity and predictable shell behaviour, not
saving 197 turns or 197 model decisions.

### What this run does not test

Every Harbor task starts a fresh keel session. This sweep does not exercise
`--resume`, does not reconnect to a previous container, and does not test the
session-log v4 restore path. It measures same-session persistence and the
effect of the description saying that the directory persists. The design
benefit for resume continuity is outside this measurement.

There is no control arm. Wave 1 and wave 2 are single before/after runs on ten
tasks, on different days, with the same model but ordinary model
nondeterminism. A single task changing state is noise unless several move.

## Aggregates

Agent minutes are the keel phase from `keel-trial.json`; they exclude Harbor
environment setup and verifier time. Input and cache-read tokens are the
recorded trial totals. The prefix share is `cd`-prefix calls divided by
`bash` calls, not by all tool calls.

| metric | wave 1 | wave 2 | delta |
| --- | ---: | ---: | ---: |
| tool calls | 339 | 283 | -56 |
| `bash` calls | 288 | 238 | -50 |
| `bash` calls with any `cd` | 197 | 156 | -41 |
| `bash` calls beginning `cd <dir> &&` | 192 | 154 | -38 |
| **`cd`-prefix share** | **66.7%** | **64.7%** | **-2.0 pp** |
| `cd`-prefix characters | 2,553 | 2,072 | -481 |
| bare `cd` calls | 0 | 0 | 0 |
| `workdir` arguments | 1 | 19 | +18 |
| turns | 255 | 237 | -18 |
| agent minutes | 62.6 | 37.4 | -25.2 |
| input tokens | 3,498,481 | 2,972,410 | -526,071 (-15.0%) |
| cache-read tokens | 3,337,472 | 2,823,552 | -513,920 |
| sweep elapsed | 96 min | 82.6 min | -13.4 min |
| passes | 9 of 10 | 9 of 10 | 0 |
| scored tasks | 10 | 9 | -1 |
| valid tasks | 9 | 9 | 0 |
| valid passes | 8 of 9 | 9 of 9 | +1, different tasks |

## Task states

| task | wave 1 | wave 2 | change |
| --- | --- | --- | --- |
| `extract-elf` | 1.0, invalid | 1.0, valid | validity only |
| `query-optimize` | 0.0, valid | no reward, invalid | agent and verifier timeout; already failing in wave 1 |
| the other eight | 1.0, valid | 1.0, valid | none |

`extract-elf` no longer fetched the task's test or reference material over the
network; it listed local `/tests` paths instead, which the classifier records
as a warning rather than an invalid run. `custom-memory-heap-crash` also listed
a local `/tests` path, with no external host. `nginx-request-logging` contacted
only `localhost`.

`query-optimize` hit the fixed 900 s agent timeout after 34 turns and 35 tool
calls. The verifier then ran for 15.8 minutes and produced no reward; the
trial's `reward_valid` is false and `verifier_result` is null. The task already
scored 0.0 in wave 1. This is a run failure, not a previously passing task
breaking, and there is no evidence that the working directory caused it.
Treating it as invalid leaves nine valid tasks.

The raw pass count is therefore 9 of 10 in both waves. The valid-pass ratio
rose from 8 of 9 to 9 of 9, but that is task composition, not a capability
gain: `extract-elf` became valid, while `query-optimize`, a wave-1 failure,
became invalid and left the valid denominator. No task that passed in wave 1
failed in wave 2.

## What the numbers say

### `cd` did not collapse

154 of 238 `bash` calls in wave 2 still begin with `cd <dir> &&`. The share
moved from 66.7% to 64.7%, a 2.0-point drop. There were zero bare `cd` calls in
both runs. Most of the remaining prefixes were still `cd /app`, the starting
directory.

The clearest behaviour change is the `workdir` argument: 19 uses in wave 2
against 1 in wave 1. The model sometimes selected the explicit parameter
instead of prefixing a `cd`, but it did not stop re-establishing directories.
This is the answer to the question in the title: persistence did not stop the
model re-issuing `cd` on this set.

### Do not claim the turn or token drop

Turns fell from 255 to 237 and agent minutes from 62.6 to 37.4. Input tokens
fell 526,071 (15.0%). Those are real numbers from the run, but they are not a
feature effect:

- tool calls fell by 56 and `bash` calls by 50, so fewer calls and fewer
  input opportunities are part of the same change;
- the per-task swings are much larger than the aggregate: for example,
  `custom-memory-heap-crash` went from 52 to 29 turns, `regex-log` from 10 to
  23, and `nginx-request-logging` from 25 to 12;
- the `cd`-prefix share moved only 2.0 points, and 154 prefixes remain;
- there is no control arm, and ten tasks is not a sample from which to read a
  causal turn saving.

Because the prefix share did fall slightly, this is not the "turns fall while
the share stays flat" case in the strict sense. The effect is still too small
and too confounded to attribute. I am not claiming a turn saving, a token
saving, or an efficiency gain.

### The real risk: no wave-1 pass broke

The reason to run this at all was the risk that a stale or surprising start
directory would break a task that passed in wave 1. That did not happen. All
nine wave-1 passes passed again. `extract-elf` changed validity, not pass
state. `query-optimize` was already a failure and now has no score.

That is a useful negative result, but it is one sample. The persistent
directory can still start a call somewhere unexpected in a task whose commands
change directory far from the workspace; this run did not produce one.

### What the feature is for

This measurement does not justify the feature on benchmark efficiency. The
honest justification is resume continuity and predictable shell behaviour:
a resumed session can continue where the shell stopped, the directory is not
silently reset between calls, and the model is told the rule. The ten-task
subset shows that this did not break a wave-1 pass, that the model still
prefers `cd` most of the time, and that the direct behavioural change in this
sample is a small shift towards `workdir`.

## Limits

- Single before/after, one run per task, no control arm, different day, same
  ten tasks. One task changing state is noise. No confidence interval or
  pass@k claim.
- The ten-task subset is chosen, not random. The aggregate turn and token
  totals are sums over whatever each task did; a single long task dominates.
- The `cd`-prefix regex only sees a `cd` at the start of the command followed
  by `&&`; a `cd` inside a script, a subshell or a pipeline is counted only by
  the broader any-`cd` column. The 197/156 any-`cd` counts and 192/154 prefix
  counts are different measurements, not two estimates of the same thing.
- Input and cache-read tokens are trial totals. They include the whole
  conversation and compaction; they are not per-call measurements and do not
  isolate the description change from the rest of the run.
- The run changed two things at once: the directory really persists, and the
  tool description says it persists. This measurement cannot separate the
  behavioural effect of the description from the effect of the actual
  directory.
- `query-optimize` has no verifier result. Its 40 minutes of wall time and its
  invalid reward are a run-level failure, not evidence about the working
  directory. The raw pass count still treats the task as a failure, not as a
  pass.
- The `extract-elf` and `custom-memory-heap-crash` local `/tests` listings are
  warnings under the same classifier used for wave 1. No task material was
  fetched over the network in wave 2.
- This run did not exercise `--resume`. The session-log v4 restore path is
  covered by unit and CLI tests in the feature branch, not by this benchmark.

## Reproduction

The raw logs are `/tmp/keel-tb2-iteration-wave1` and
`/tmp/keel-tb2-cwd-wave2`. The counting parser and the comparison output are
temporary files, not repo artifacts; the wave-1 baseline is reproduced by
reading the session logs with the same parser. The binary under test is the
static-pie musl build above, not a macOS or dynamically linked binary.

The change under this note is documentation only. No product code, test, or
benchmark runner changed in this branch.
