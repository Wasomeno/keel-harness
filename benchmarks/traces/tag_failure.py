"""Inspect and edit benchmark failure tags with an audit trail.

Examples::

    python3 benchmarks/traces/tag_failure.py list
    python3 benchmarks/traces/tag_failure.py validate runs/*.jsonl
    python3 benchmarks/traces/tag_failure.py add runs/run.jsonl \\
        --task-id terminal-bench/fix-git --trial 0 \\
        --tag localization --note "never opened the repository"
    python3 benchmarks/traces/tag_failure.py replace runs/run.jsonl \\
        --line 4 --old localization --new wrong-file --note "edited the adapter instead"
    python3 benchmarks/traces/tag_failure.py remove runs/run.jsonl \\
        --task-id terminal-bench/fix-git --trial 0 --tag wrong-file \\
        --note "the file was correct after all"

Every mutation requires a note and appends an entry to the trace's optional
``failure_tag_notes`` array.  Only the selected line is rewritten; all other
records are preserved byte for byte.  Symlinks are followed without replacing
the link, and the file is replaced atomically only if it did not change while
being read and edited.  The mutated record is validated before anything is
written.

Only the Python standard library is used.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import tempfile
from collections.abc import Sequence
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

if __package__:
    from . import schema
else:  # ``python3 benchmarks/traces/tag_failure.py``
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
    from benchmarks.traces import schema


class TagError(ValueError):
    """The requested tag edit cannot be applied."""


def _utc_now() -> str:
    return (
        datetime.now(timezone.utc)
        .replace(microsecond=0)
        .isoformat()
        .replace("+00:00", "Z")
    )


def list_tags() -> list[dict[str, str]]:
    return [
        {"tag": tag, "description": schema.FAILURE_TAG_DESCRIPTIONS[tag]}
        for tag in schema.FAILURE_TAGS
    ]


def _stat_snapshot(stat: os.stat_result) -> tuple[int, int, int, int]:
    return (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns)


def _file_snapshot(path: Path) -> tuple[int, int, int, int]:
    return _stat_snapshot(path.stat())


def _ensure_unchanged(path: Path, expected: tuple[int, int, int, int]) -> None:
    try:
        current = _file_snapshot(path)
    except OSError as exc:
        raise TagError(f"{path}: cannot stat before replacing: {exc}") from exc
    if current != expected:
        raise TagError(
            f"{path}: changed while editing; refusing to overwrite "
            "(re-read the file and retry)"
        )


def _read_text(
    path: Path,
) -> tuple[list[str], list[str], tuple[int, int, int, int]]:
    try:
        with path.open("r", encoding="utf-8", newline="") as handle:
            before = _stat_snapshot(os.fstat(handle.fileno()))
            raw = handle.read()
            after = _stat_snapshot(os.fstat(handle.fileno()))
    except UnicodeDecodeError as exc:
        raise TagError(f"{path}: file is not UTF-8: {exc}") from exc
    if before != after:
        raise TagError(
            f"{path}: changed while reading; refusing to edit a moving file"
        )
    contents: list[str] = []
    endings: list[str] = []
    for line in raw.splitlines(keepends=True):
        content = line.rstrip("\r\n")
        contents.append(content)
        endings.append(line[len(content) :])
    return contents, endings, after


def _parse_all(
    path: Path, lines: Sequence[str]
) -> dict[int, dict[str, Any]]:
    """Parse every non-blank line and validate it before touching the file."""

    records: dict[int, dict[str, Any]] = {}
    for index, line in enumerate(lines):
        if not line.strip():
            continue
        source = f"{path}:{index + 1}"
        try:
            record = json.loads(line)
        except json.JSONDecodeError as exc:
            raise schema.TraceValidationError(
                [f"invalid JSON: {exc.msg} (column {exc.colno})"], source=source
            ) from exc
        if not isinstance(record, dict):
            raise schema.TraceValidationError(
                ["expected a JSON object"], source=source
            )
        schema.validate_trace(record, source=source)
        records[index] = record
    if not records:
        raise TagError(f"{path}: no trace records found")
    return records


def _select(
    path: Path,
    *,
    line: int | None,
    task_id: str | None,
    trial: int | None,
) -> tuple[
    list[str],
    list[str],
    int,
    dict[str, Any],
    tuple[int, int, int, int],
]:
    if line is None and task_id is None:
        raise TagError("select a record with --line or --task-id")
    if line is not None and task_id is not None:
        raise TagError("use either --line or --task-id, not both")
    if trial is not None and task_id is None:
        raise TagError("--trial requires --task-id")

    lines, endings, snapshot = _read_text(path)
    records = _parse_all(path, lines)

    if line is not None:
        if line < 1 or line > len(lines):
            raise TagError(
                f"{path}: --line {line} is outside the file (1..{len(lines)})"
            )
        index = line - 1
        if index not in records:
            raise TagError(f"{path}:{line}: line is blank or has no trace record")
        return lines, endings, index, records[index], snapshot

    matches = [
        index
        for index, record in records.items()
        if record["task_id"] == task_id
        and (trial is None or record["trial"] == trial)
    ]
    if not matches:
        trial_note = f" trial {trial}" if trial is not None else ""
        raise TagError(f"{path}: no trace for task_id {task_id!r}{trial_note}")
    if len(matches) > 1:
        lines_found = ", ".join(str(index + 1) for index in matches)
        raise TagError(
            f"{path}: task_id {task_id!r} matches {len(matches)} records "
            f"(lines {lines_found}); add --trial to choose one"
        )
    index = matches[0]
    return lines, endings, index, records[index], snapshot


def _atomic_write(
    path: Path,
    text: str,
    expected: tuple[int, int, int, int],
) -> None:
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.", suffix=".tmp", dir=path.parent
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(
            descriptor, "w", encoding="utf-8", newline=""
        ) as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        shutil.copymode(path, temporary)
        _ensure_unchanged(path, expected)
        os.replace(temporary, path)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise


def mutate(
    path: str | Path,
    action: str,
    *,
    tag: str,
    note: str,
    old: str | None = None,
    by: str | None = None,
    line: int | None = None,
    task_id: str | None = None,
    trial: int | None = None,
) -> dict[str, Any]:
    """Add, replace or remove one tag and append a note to the record."""

    target = Path(path)
    if action not in ("add", "replace", "remove"):
        raise TagError(f"unknown action {action!r}")
    if tag not in schema.FAILURE_TAGS:
        raise TagError(
            f"unknown tag {tag!r}; choose one of {', '.join(schema.FAILURE_TAGS)}"
        )
    note = note.strip()
    if not note:
        raise TagError("--note must not be empty")
    by = (by or "").strip() or None

    # Follow symlinks so an atomic replace updates the target, not the link.
    target = target.resolve()
    lines, endings, index, record, snapshot = _select(
        target, line=line, task_id=task_id, trial=trial
    )
    tags = list(record["failure_tags"])

    previous: str | None = None
    if action == "add":
        if tag in tags:
            raise TagError(
                f"{target}:{index + 1}: tag {tag!r} is already present; "
                "use replace or remove first"
            )
        tags.append(tag)
    elif action == "replace":
        if old not in schema.FAILURE_TAGS:
            raise TagError(
                f"unknown old tag {old!r}; choose one of "
                f"{', '.join(schema.FAILURE_TAGS)}"
            )
        if old == tag:
            raise TagError("--old and --new must be different tags")
        if old not in tags:
            raise TagError(
                f"{target}:{index + 1}: tag {old!r} is not present"
            )
        if tag in tags:
            raise TagError(
                f"{target}:{index + 1}: tag {tag!r} is already present"
            )
        previous = old
        tags[tags.index(old)] = tag
    else:
        if tag not in tags:
            raise TagError(
                f"{target}:{index + 1}: tag {tag!r} is not present"
            )
        previous = tag
        tags.remove(tag)

    record["failure_tags"] = tags
    notes = record.get("failure_tag_notes")
    if notes is None:
        notes = []
    elif not isinstance(notes, list):
        raise TagError(
            f"{target}:{index + 1}: failure_tag_notes is not a list"
        )
    entry: dict[str, Any] = {
        "action": action,
        "tag": tag,
        "note": note,
        "at": _utc_now(),
    }
    if previous is not None:
        entry["previous"] = previous
    if by is not None:
        entry["by"] = by
    notes.append(entry)
    record["failure_tag_notes"] = notes

    schema.validate_trace(record, source=f"{target}:{index + 1}")
    lines[index] = schema.dump_trace(record)
    text = "".join(line + ending for line, ending in zip(lines, endings))
    _atomic_write(target, text, snapshot)

    return {
        "path": str(path),
        "line": index + 1,
        "action": action,
        "tag": tag,
        "previous": previous,
        "failure_tags": tags,
        "note": note,
        "by": by,
    }


def validate_files(paths: Sequence[str | Path]) -> list[dict[str, Any]]:
    """Validate files and return one result object per file."""

    results = []
    for raw_path in paths:
        path = Path(raw_path)
        try:
            loaded = schema.read_trace_file(path)
        except schema.TraceValidationError as exc:
            results.append(
                {
                    "path": str(path),
                    "ok": False,
                    "traces": 0,
                    "errors": exc.errors,
                }
            )
        except OSError as exc:
            results.append(
                {
                    "path": str(path),
                    "ok": False,
                    "traces": 0,
                    "errors": [f"cannot read file: {exc.strerror or exc}"],
                }
            )
        else:
            results.append(
                {
                    "path": str(path),
                    "ok": True,
                    "traces": len(loaded),
                    "errors": [],
                }
            )
    return results


def _add_selection(parser: argparse.ArgumentParser) -> None:
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument(
        "--line",
        type=int,
        metavar="N",
        help="1-based physical line in the JSONL file",
    )
    group.add_argument("--task-id", help="select by task_id")
    parser.add_argument(
        "--trial", type=int, help="trial number when --task-id matches several"
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Inspect and edit benchmark failure tags."
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    listing = subparsers.add_parser("list", help="list the 16 failure tags")
    listing.add_argument("--json", action="store_true", help="emit JSON")

    validating = subparsers.add_parser(
        "validate", help="validate one or more trace JSONL files"
    )
    validating.add_argument("files", nargs="+", metavar="TRACE.jsonl")
    validating.add_argument("--json", action="store_true", help="emit JSON")

    adding = subparsers.add_parser("add", help="add a tag")
    adding.add_argument("file", metavar="TRACE.jsonl")
    _add_selection(adding)
    adding.add_argument("--tag", required=True)
    adding.add_argument("--note", required=True)
    adding.add_argument("--by", help="name recorded in the audit entry")
    adding.add_argument("--json", action="store_true", help="emit JSON")

    replacing = subparsers.add_parser("replace", help="replace one tag")
    replacing.add_argument("file", metavar="TRACE.jsonl")
    _add_selection(replacing)
    replacing.add_argument("--old", required=True)
    replacing.add_argument("--new", required=True, dest="tag")
    replacing.add_argument("--note", required=True)
    replacing.add_argument("--by", help="name recorded in the audit entry")
    replacing.add_argument("--json", action="store_true", help="emit JSON")

    removing = subparsers.add_parser("remove", help="remove a tag")
    removing.add_argument("file", metavar="TRACE.jsonl")
    _add_selection(removing)
    removing.add_argument("--tag", required=True)
    removing.add_argument("--note", required=True)
    removing.add_argument("--by", help="name recorded in the audit entry")
    removing.add_argument("--json", action="store_true", help="emit JSON")

    return parser


def _print_error(message: str) -> None:
    print(f"tag_failure.py: error: {message}", file=sys.stderr)


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    if args.command == "list":
        tags = list_tags()
        if args.json:
            print(json.dumps(tags, indent=2, ensure_ascii=False))
        else:
            width = max(len(item["tag"]) for item in tags)
            for item in tags:
                print(f"{item['tag']:<{width}}  {item['description']}")
        return 0

    if args.command == "validate":
        results = validate_files(args.files)
        if args.json:
            print(json.dumps(results, indent=2, ensure_ascii=False))
        else:
            for result in results:
                if result["ok"]:
                    print(f"OK {result['path']} ({result['traces']} traces)")
                else:
                    for error in result["errors"]:
                        print(
                            f"INVALID {result['path']}: {error}",
                            file=sys.stderr,
                        )
        return 0 if all(result["ok"] for result in results) else 1

    try:
        result = mutate(
            args.file,
            args.command,
            tag=args.tag,
            note=args.note,
            old=getattr(args, "old", None),
            by=args.by,
            line=args.line,
            task_id=args.task_id,
            trial=args.trial,
        )
    except (TagError, schema.TraceValidationError) as exc:
        _print_error(str(exc))
        return 2
    except OSError as exc:
        _print_error(f"cannot update {args.file}: {exc.strerror or exc}")
        return 2

    if args.json:
        print(json.dumps(result, indent=2, ensure_ascii=False))
    else:
        print(
            f"updated {result['path']}:{result['line']}: "
            f"{result['action']} {result['tag']} "
            f"(tags: {', '.join(result['failure_tags']) or 'none'})"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())


__all__ = [
    "TagError",
    "build_parser",
    "list_tags",
    "main",
    "mutate",
    "validate_files",
]
