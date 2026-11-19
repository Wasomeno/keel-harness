# Atomic `write` and `edit`

## Problem

`write` opened the live file with `O_CREAT | O_TRUNC` and wrote in place;
`edit` opened it `O_RDWR`, read, called `set_len(0)`, and wrote back. A disk-full
or I/O error between truncation and the last byte left the target partial, and
a crash could leave it empty. The README and BLUEPRINT recorded this as a
data-loss gap.

## Design

On Unix both tools go through `Workspace::replace_file`:

1. Walk the parent components from the workspace root descriptor with
   `openat` and `O_NOFOLLOW | O_DIRECTORY`, creating missing directories with
   `mkdirat` for `write` as before. The final component name is only a name
   relative to that directory descriptor.
2. Open the existing target `O_WRONLY | O_NOFOLLOW` without `O_TRUNC`. Success
   proves the process can write the file and gives its mode; `ENOENT` means a
   new file for `write` and a not-found error for `edit`. Any other error keeps
   the existing mode-specific wording, including the "check permissions" hint.
3. Create `.keel-<pid>-<counter>.tmp` in the same directory with
   `O_CREAT | O_EXCL | O_NOFOLLOW | O_WRONLY` and mode `0o666`. A mode-`0o666`
   create is filtered by the process umask, so a new file gets normal umask
   semantics. Names are retried on collision.
4. If the target existed, `fchmod` the temp file to its mode masked with
   `0o7777` immediately, so the temp is never more readable than the file it
   replaces. Write the complete bytes, `fchmod` again to restore any
   setuid/setgid bits the write cleared, then `fsync` the temp file.
5. Close the temp file, `renameat` it over the target, and `fsync` the
   directory. The directory sync is best-effort: the rename is already
   visible, so its failure is not reported as a failed write.
6. A drop guard unlinks the temp entry on every error path, including write,
   chmod, sync and rename failures.

`edit` reads the file read-only, computes the replacement exactly as before
(count, not-found and ambiguous errors), and calls the same replace path. It
never truncates the live file.

Non-Unix keeps the existing canonicalizing resolver, writes a temp file in the
target's directory, `sync_all`s it, and uses `std::fs::rename`. It performs the
same "open the existing target for write" check but does not preserve mode.

## Guarantees

- A reader sees the old file or the complete new one, never a partial one.
- A failed `write` or `edit` leaves the old contents in place.
- The temp file is removed on every failure path.
- An existing target's permission bits are preserved; a new file gets the
  process umask.
- A target that is a symlink, a directory, or not writable is refused with the
  previous wording. A symlink swapped in after the check cannot redirect the
  rename: `renameat` replaces the link entry, not its target.
- Hard links keep pointing at the old inode and therefore the old contents.

## Trade-offs

- **Hard links break.** POSIX cannot replace a file's bytes and keep its inode
  atomically. A replaced file gets a new inode; hard links keep the old
  contents. This is documented in the `write`/`edit` descriptions and the
  architecture docs.
- **Other inode metadata is not copied.** Only permission bits are preserved;
  ownership (normally the same user), ACLs and extended attributes are not.
- **Concurrent writers lose updates.** As before, two processes that read the
  same file and replace it can overwrite each other; the rename only makes each
  individual replacement complete.
- **The writability check is racy by design.** The target can change between
  the check and the rename, but the check is only about refusing an unwritable
  file; the rename itself is atomic and symlink-safe.
- **The directory is fsynced best-effort.** A failure there is not surfaced as
  a failed write because the new contents are already visible.

## Tests

`crates/keel-tools/tests/builtin.rs`:

- `a_failed_write_leaves_the_old_file_intact` and
  `a_failed_edit_leaves_the_old_file_intact`: a read-only parent makes temp
  creation fail; the old bytes remain.
- `writing_preserves_the_existing_file_mode` and
  `editing_preserves_the_file_mode`: `0o640` stays `0o640`.
- `a_new_file_gets_the_process_umask_mode`: umask `0o077` with a `0o666` temp
  create yields `0o600`.
- `an_atomic_replace_leaves_no_temp_file`: no `.keel-*` entry after success or
  a forced failure.
- `a_read_only_file_is_not_replaced`: `0o444` in a writable directory fails
  and keeps its contents.
- `an_edit_that_fails_on_old_string_leaves_the_file_byte_identical`: both the
  zero-match and ambiguous paths leave the file untouched.

`crates/keel-tools/src/workspace.rs`:

- `a_symlink_swapped_in_before_the_rename_is_replaced_not_followed`: a symlink
  in place of the target at commit time is replaced as an entry; the outside
  file is unchanged.

The read-only-parent and read-only-file tests skip when the process can bypass
mode bits (root or a filesystem that ignores modes); the temp-cleanup test
covers the success path and skips only its failure half in that environment.
