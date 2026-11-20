# Windows CI

## Matrix and setup

`.github/workflows/ci.yml` runs one `test` job on
`[ubuntu-latest, macos-latest, windows-latest]` with `fail-fast: false`. All
three jobs run the same commands, in the same order:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Keeping one job and one command list means a failure in any column is directly
comparable with the other two; a Windows-only failure is a portability bug, not
a missing check.

`dtolnay/rust-toolchain@stable` installs rustfmt and clippy, and
`Swatinem/rust-cache@v2` caches the target directory per OS. The `Install
ripgrep for the search tool` step runs under `shell: bash` (Git Bash on
Windows). It exits early when `rg` is already on `PATH`; otherwise it installs
with Chocolatey on Windows, Homebrew on macOS and apt on Linux. The search
tests still probe `rg --version` themselves and print a skip line when the
binary is absent, so a broken install is visible rather than silently turning
into a pass.

`.gitattributes` checks text files out with LF on every platform
(`* text=auto eol=lf`). Windows scripts (`.bat`, `.cmd`, `.ps1`) keep CRLF.
This matters because several golden request bodies and catalog files are
embedded with `include_str!`; a CRLF checkout would make the program's LF
output differ from the fixture.

## What runs on Windows

The whole default suite runs, offline, including:

- CLI tests that spawn the `keel` binary, including `auth`, `chat`, `route`,
  `tools` and catalog output tests.
- Built-in tool tests: `read`, `write`, `edit`, `glob`, `search` and `bash`,
  including the error text each failure path returns.
- The `bash` tests use Git Bash rather than the WSL launcher in
  `C:\Windows\System32`. The tool resolves the shell in this order:
  `KEEL_BASH` when set, `%ProgramFiles%\Git\bin\bash.exe`,
  `%ProgramFiles(x86)%\Git\bin\bash.exe`,
  `%LOCALAPPDATA%\Programs\Git\bin\bash.exe`, the install root of the first
  `git.exe` on `PATH`, then plain `bash`.
- Bash results normalize CRLF to LF, and Windows bash children get
  `PYTHONUTF8=1` and `PYTHONIOENCODING=utf-8`, so a command that prints
  non-ASCII text produces the same tool result as on Unix. Python on Windows
  otherwise writes the ANSI code page to a pipe and non-ASCII bytes become
  replacement characters.
- All twelve behavioral eval replays. `python3` is present on the runner;
  the check scripts use the same Git Bash. The eval suite is not gated.
- MCP stdio and Streamable HTTP tests, cassette replay, session logs, retry,
  compaction and the parallel run loop.

Tool paths are rendered with `/` on every platform. `Workspace::relative` and
`Workspace::display_path` convert separators, `glob` uses the same helper, and
`search` keeps ripgrep's `./` prefix but converts `\` to `/`. Error text for a
directory, a file used as a directory, and a missing path is therefore the same
on Windows and Unix.

The non-Unix file-tool fallback still canonicalizes the nearest existing
ancestor instead of walking descriptors from the root. It follows symlinks,
does not preserve modes, and keeps the TOCTOU window described in
`docs/architecture.md`. Windows coverage makes that fallback tested; it does
not make it equivalent to the Unix path.

## Gated tests and why

28 tests compile only on Unix. Everything else runs on Windows.

**Symlink semantics** (Windows symlinks need developer mode or an elevated
process, and the product refuses them differently):

- `read_refuses_a_symlink_even_inside_the_workspace`
- `read_quotes_a_symlink_path_in_the_bash_hint`
- `write_refuses_to_replace_a_symlink`
- `workspace::tests::a_symlink_out_of_the_root_is_rejected`
- `workspace::tests::open_existing_refuses_an_internal_symlink`
- `workspace::tests::open_existing_names_an_ancestor_symlink`
- `workspace::tests::open_survives_a_concurrent_symlink_swap`
- `workspace::tests::a_symlink_swapped_in_before_the_rename_is_replaced_not_followed`
- `a_symlinked_credential_file_is_refused`
- `credentials::tests::a_symlinked_private_parent_is_followed_and_not_chmodded`

**Unix permission bits and umask** (Windows uses a read-only attribute and ACLs,
not mode bits):

- `writing_preserves_the_existing_file_mode`
- `editing_preserves_the_file_mode`
- `writing_drops_special_mode_bits`
- `a_new_file_gets_the_process_umask_mode`
- `the_file_is_0600_and_the_created_directory_is_0700`
- `credentials::tests::a_private_parent_is_tightened_to_0700`
- `an_unreadable_runtime_overlay_warns_and_uses_the_embedded_catalog` (a
  Windows read-only attribute still allows reading)
- `an_empty_override_falls_back_to_the_platform_default` (HOME/XDG and 0600)
- `live::tests::a_key_file_must_not_be_group_or_world_readable`

**Unix special files and directory locking**:

- `search_timeout_names_the_pattern_and_next_step` (`mkfifo`)
- `write_refuses_a_fifo_without_blocking` (`mkfifo`)
- `write_refuses_a_socket_without_replacing_it` (Unix socket)
- `a_fifo_at_the_credential_path_is_refused_without_blocking` (`mkfifo`)
- `a_failed_write_leaves_the_old_file_intact` (locks the directory with chmod)
- `a_failed_edit_leaves_the_old_file_intact` (same)
- `an_atomic_replace_leaves_no_temp_file` (same)
- `workspace::tests::an_uncommitted_temp_entry_is_removed_by_the_drop_guard`
  (exercises the Unix `openat` temp path)
- `a_directory_at_the_credential_path_is_refused` (Windows cannot open a
  directory as a file, so the store reports an I/O error rather than the typed
  "not a regular file" error; the behavior is documented, not asserted)

Two portable tests that used to be behind `#[cfg(unix)]` now run on Windows:
`workspace::tests::not_a_directory_messages_are_mode_specific` and
`workspace::tests::write_file_creates_parents`. The live executor module was
also narrowed to gate only its key-file mode test, so its three cassette tests
run on Windows.

## Test counts

From CI run `44740017384` (head `51a44aa`), counting the `test result:` lines
from all 52 test targets:

| runner | passed | failed | ignored |
|---|---|---|---|
| ubuntu-latest | 589 | 0 | 2 |
| macos-latest | 589 | 0 | 2 |
| windows-latest | 561 | 0 | 2 |

The two ignored tests are the opt-in live recording paths. Windows runs 28
fewer tests than macOS and Ubuntu, exactly the gated list above.

## What Windows CI does not cover

- The Windows credential ACL. Windows inherits the per-user `%APPDATA%` ACL
  for the store and its directory; the 0600/0700 tests are Unix-only, and no
  test asserts the resulting Windows ACL. The `%APPDATA%` default path with an
  empty `KEEL_CREDENTIALS` override is also untested on Windows.
- Symlink refusal, FIFO/socket refusal, umask and special mode bits, for the
  reasons above. The read-only attribute path is tested by
  `edit_reports_a_read_only_file_as_not_writable` and
  `a_read_only_file_is_not_replaced`.
- Installer, packaging and shell integration outside Git Bash. The `bash` tool
  requires a bash; Git for Windows is the supported source.
