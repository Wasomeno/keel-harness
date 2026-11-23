# One-shot `bash`: persistent working directory

**Status:** merged at `f29db3f`
(PR [#50](https://github.com/Wasomeno/keel-harness/pull/50)); not yet measured
on a benchmark.

## Why

The wave-1 Terminal-Bench 2.0 trace had 339 tool calls, 288 of them `bash`,
and **197 of the 288 bash calls re-issued `cd`**. That did not cost turns: 192
calls begin with `cd <dir> && <command>` in the same call, and none is a bare
`cd`. The prefixes total 2,553 characters, about 640 tokens across the entire
ten-task run; 120 of the 126 calls with a literal path were `cd /app`, the
workspace root and already the starting directory. Exactly one call used the
`workdir` argument.

A persistent directory therefore saves tokens at most. The reasons for the
change are that a resumed session keeps its place and that a shell that
remembers where it is matches what anyone expects. The earlier turn-cost
framing in this note was wrong. The opt-in `terminal` tool exists for persistent
shell state, but it was called 0 times in that run, so the fix belongs in the
default shell.

## Contract

- **Starting directory.** On Unix, a one-shot `bash` call starts in the
  directory the previous call finished in for the same session. The first call
  of a session, a session with no record, and every call on Windows start in
  the workspace root.
- **What is remembered.** Only the working directory, and only when the shell
  reaches its exit trap. The shell is still new on every call: exported
  variables, shell functions, aliases, jobs and open file descriptors do not
  survive. Use `jobs` for a background process; it starts in the workspace
  root or its own `workdir`.
- **Where it is stored.** The session log gets one auxiliary record per
  completed call:

  ```json
  {"type":"bash_cwd","at":1763550012,"cwd":"/tmp/project/sub"}
  ```

  The newest record wins. `SessionLog::open_with_state` restores it, so
  `keel chat --resume` starts the next bash call where the previous process
  stopped. The log version is 4; version-1, -2 and -3 logs still load and
  simply start at the workspace root.
- **Per session, not per tool.** The CLI attaches the open `SessionLog` to the
  harness, and the harness passes it to every tool call. A default registry or
  an embedder without a session store keeps the directory in the `BashTool`
  instance instead, which lasts for that run but not across a resume.
- **Concurrency.** `bash` is not `parallel_safe`; the run loop already treats
  it as a barrier. The tool also holds a `tokio::sync::Mutex` for the whole
  call, so two concurrent calls on the same tool instance (a direct embedder)
  serialize: the second starts where the first finished and neither can
  overwrite the other's record with a stale value.
- **`workdir`.** An explicit `workdir` starts that one call there, resolved
  from the workspace root as before. The directory it finishes in is recorded
  like any other call.
- **Missing directory.** If the record no longer exists, is not a directory,
  or cannot be entered, the call starts in the workspace root and adds one
  line to the result:

  ```text
  exit: 0
  note: the recorded working directory `/tmp/gone` no longer exists; this call starts in the workspace root `/work`
  ```

  The call is not an error, and its exit code, stdout and stderr are
  unchanged.
- **Windows.** No persistence. Every call starts in the workspace root, as it
  did before this change. The shell is still Git Bash (or `KEEL_BASH`), and
  the build and behavior are otherwise unchanged.

## Other tools stay workspace-rooted

`read`, `write`, `edit`, `apply_patch`, `glob`, `search` and `view_image`
always resolve paths from the workspace root. A `cd` in `bash` does not move
them, and their behavior is unchanged. The `bash` description says this explicitly, so the
model does not try to use a shell directory as a second root.

`jobs` is unchanged. It starts in the workspace root or its own `workdir` and
does not inherit the one-shot `bash` record.

## How the trailer works

The command is not run as `bash -lc <command>` directly on Unix. The tool runs
a small wrapper in the same login shell:

1. Save the caller's command, start directory and workspace into shell
   variables, then `set --` so the caller sees no extra positional parameters.
2. `cd` to the start directory. If that fails, write `fallback` to a private
   state file and `cd` to the workspace root instead.
3. Install an `EXIT` trap that saves `$?`, appends `pwd -P` to the state file
   and re-exits with the saved status.
4. `eval` the caller's command.

The state file is a freshly created file in the system temporary directory
with a unique name, removed after the call. It is never stdout or stderr, so
both streams are byte-for-byte what the command wrote. The trap preserves the
exact exit code, including for a command that calls `exit` or runs under
`set -e`. A command that `exec`s or kills its shell skips the trap; the
previous record stays valid and the next call works. A timeout or
cancellation also leaves the previous record.

The recorded path is the shell's physical `pwd -P`, so symlinked directories
are stored as their real target. If the command deletes its own directory,
`pwd -P` fails and the record is not updated; the next call detects the stale
record and falls back with the note.

## Limits

- A command whose only effect is `cd` in a subshell or pipeline does not
  change the outer shell's directory; only the final directory of the shell
  process is recorded.
- Environment changes do not persist by design. A shell function or alias the
  model defines is gone next call.
- The record can point outside the workspace. Bash was already able to `cd`
  anywhere; persisting that directory does not add a capability, but a resumed
  session can start outside the workspace. The structured tools still refuse
  paths outside the root.
- Non-UTF-8 paths are stored lossily in JSON; if the lossy path does not
  exist, the next call falls back with the normal note.
- The state file is only a transport. If it cannot be created, the call runs
  without the trailer (today's behavior) and the result says the record was
  not updated.
- Windows does not persist. This is a deliberate fallback, not an unimplemented
  path: Git Bash's path spelling differs from the Win32 path a child process
  needs, and the benchmark target is Linux.

## Tests

`crates/keel-tools/src/builtin/bash.rs` covers: first call at the workspace
root, `cd` visible in the next call, a fresh tool reading a session store, an
explicit `workdir` becoming the record, an exact failing exit code, stdout with
no trailing newline, removed-directory fallback with the note, a command that
deletes its own directory, `exec` and `kill -9 $$`, concurrent calls leaving one
coherent record, and the Windows workspace-root behavior. `keel-core` tests the
`bash_cwd` record round trip, newest-wins, auxiliary-record handling and a log
with no record. CLI tests drive two bash calls in one turn and a resume from a
seeded record.
