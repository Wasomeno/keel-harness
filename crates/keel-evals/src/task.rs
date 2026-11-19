//! The eval tasks: one prompt, one starting fixture and mechanical checks.
//!
//! Every task starts from the same small Python repository. The fixture is
//! deliberately broken or incomplete in several independent places; a task's
//! check only exercises its own target, and `tests/checks_bite.rs` proves that
//! each check fails on the untouched fixture.
//!
//! Checks are either a command run from the workspace root (always a Python
//! script under `fixtures/checks/`, so the agent cannot edit the grader) or a
//! file assertion. No LLM judges anything.

/// One eval task.
#[derive(Debug, Clone, Copy)]
pub struct Task {
    /// Stable id, also the cassette name suffix.
    pub id: &'static str,
    pub prompt: &'static str,
    /// Script under `fixtures/checks/`. The runner also copies it to the
    /// workspace as `tests/check.py` so the agent can run it.
    pub check_script: &'static str,
    pub checks: &'static [Check],
    /// Cassette name recorded by the live runner, e.g. `evals/truncate`.
    pub cassette: &'static str,
}

/// A mechanical pass/fail condition.
#[derive(Debug, Clone, Copy)]
pub enum Check {
    /// Run `python3 <fixtures/checks/<script>>` with the workspace as cwd.
    Command { script: &'static str },
    /// The workspace file must contain `needle`.
    FileContains {
        path: &'static str,
        needle: &'static str,
    },
}

impl Task {
    pub fn cassette_file(&self) -> String {
        format!("{}.json", self.cassette)
    }
}

pub fn task_by_id(id: &str) -> Option<&'static Task> {
    TASKS.iter().find(|task| task.id == id)
}

const TRUNCATE_PROMPT: &str = r#"Fix `truncate` in `keeltoy/text.py`.

A shortened result must be at most `limit` characters: keep the first `limit - 1`
characters and append the single character `…`. Text that already fits is
returned unchanged. For example `truncate("abcdef", 5)` must return `"abcd…"`
and `truncate("abc", 5)` must return `"abc"`.

Run `python3 tests/check.py` until it passes. Do not edit the check script."#;

const SLUGIFY_PROMPT: &str = r#"Implement `slugify` in `keeltoy/text.py`.

It returns a lowercase ASCII slug: strip surrounding whitespace, lowercase the
text, replace every run of characters that are not `a-z` or `0-9` with one
hyphen, and trim leading and trailing hyphens. Examples:

- `"Hello, World!"` -> `"hello-world"`
- `"  Rust 2024  "` -> `"rust-2024"`
- `"a__b"` -> `"a-b"`
- `"Café au lait"` -> `"caf-au-lait"`

Run `python3 tests/check.py` until it passes. Do not edit the check script."#;

const PARSE_INT_PROMPT: &str = r#"Implement `parse_int` in `keeltoy/numbers.py`.

It takes a string, strips surrounding whitespace, accepts an optional leading
`+` or `-`, accepts underscores between digits, and returns an `int`. It raises
`ValueError` for an empty string, a float literal, or any other invalid input.
Examples: `" 42 "` -> 42, `"+7"` -> 7, `"1_000"` -> 1000.

Run `python3 tests/check.py` until it passes. Do not edit the check script."#;

const MEAN_PROMPT: &str = r#"Implement `mean` in `keeltoy/numbers.py`.

It returns the arithmetic mean as a float. An empty sequence raises `ValueError`.
Examples: `[1, 2, 3]` -> 2.0, `[2.0, 4.0]` -> 3.0.

Run `python3 tests/check.py` until it passes. Do not edit the check script."#;

const CLAMP_PROMPT: &str = r#"Fix `clamp` in `keeltoy/numbers.py`.

It returns `low` when the value is below the range, `high` when it is above the
range, and the value itself inside the inclusive range. For example
`clamp(-1, 0, 10)` -> 0, `clamp(5, 0, 10)` -> 5, `clamp(11, 0, 10)` -> 10.

Run `python3 tests/check.py` until it passes. Do not edit the check script."#;

const UNIQUE_PROMPT: &str = r#"Fix `unique` in `keeltoy/lists.py`.

It returns a new list containing the first occurrence of each value, preserving
input order. For example `unique([3, 1, 2, 3])` must return `[3, 1, 2]`, not a
set-sorted list. `unique([])` returns `[]`.

Run `python3 tests/check.py` until it passes. Do not edit the check script."#;

const CHUNK_PROMPT: &str = r#"Implement `chunk` in `keeltoy/lists.py`.

It splits a list into consecutive sublists of at most `size` items; the last
sublist may be shorter. `size` must be positive; otherwise raise `ValueError`.
Examples: `chunk([1, 2, 3, 4, 5], 2)` -> `[[1, 2], [3, 4], [5]]`,
`chunk([], 3)` -> `[]`.

Run `python3 tests/check.py` until it passes. Do not edit the check script."#;

const FORMAT_TABLE_PROMPT: &str = r#"Implement `format_table(headers, rows)` in `keeltoy/report.py`.

It returns a string with a header line, a separator line, and one line per row.
Every column is padded with spaces to the widest cell in that column (the header
included), cells are joined with `" | "`, and trailing spaces are removed from
each line. The separator uses `-` for each column width and `+` at the joins.

For `headers=["id", "name"]` and `rows=[["1", "a"], ["22", "bb"]]` the exact
result is:

id | name
---+-----
1  | a
22 | bb

Run `python3 tests/check.py` until it passes. Do not edit the check script."#;

const REFACTOR_PROMPT: &str = r#"Refactor `keeltoy/text.py` so the whitespace collapse expression
`" ".join(text.split())` appears once.

Add a private helper `def _collapse(text):` containing it, and make both
`normalize_space` and `dedupe_whitespace` call `_collapse`. Their public behavior
must not change.

Run `python3 tests/check.py` until it passes. Do not edit the check script."#;

const RENAME_PROMPT: &str = r#"Rename `format_row` to `format_cells` in `keeltoy/report.py` and update the
re-export in `keeltoy/__init__.py`. The function behavior stays identical. No
`format_row` name should remain in the package.

Run `python3 tests/check.py` until it passes. Do not edit the check script."#;

const README_PROMPT: &str = r#"Add a `## slugify` section to `README.md` that documents the slugify rules:
lowercase ASCII output, runs of non-alphanumeric characters replaced by one
hyphen, and leading and trailing hyphens removed. Include the word `lowercase`
and the example `hello-world`.

Run `python3 tests/check.py` until it passes. Do not edit the check script."#;

const EXPORTS_PROMPT: &str = r#"Export `slugify`, `parse_int`, `mean`, `chunk`, and `format_table` from the
package root in `keeltoy/__init__.py`, and add them to `__all__`. The functions
already exist; only the exports change.

Run `python3 tests/check.py` until it passes. Do not edit the check script."#;

const TRUNCATE_CHECKS: &[Check] = &[Check::Command {
    script: "truncate.py",
}];
const SLUGIFY_CHECKS: &[Check] = &[Check::Command {
    script: "slugify.py",
}];
const PARSE_INT_CHECKS: &[Check] = &[Check::Command {
    script: "parse_int.py",
}];
const MEAN_CHECKS: &[Check] = &[Check::Command { script: "mean.py" }];
const CLAMP_CHECKS: &[Check] = &[Check::Command { script: "clamp.py" }];
const UNIQUE_CHECKS: &[Check] = &[Check::Command {
    script: "unique.py",
}];
const CHUNK_CHECKS: &[Check] = &[Check::Command { script: "chunk.py" }];
const FORMAT_TABLE_CHECKS: &[Check] = &[Check::Command {
    script: "format_table.py",
}];
const REFACTOR_CHECKS: &[Check] = &[Check::Command {
    script: "refactor_whitespace.py",
}];
const RENAME_CHECKS: &[Check] = &[Check::Command {
    script: "rename_format_row.py",
}];
const README_CHECKS: &[Check] = &[
    Check::FileContains {
        path: "README.md",
        needle: "## slugify",
    },
    Check::FileContains {
        path: "README.md",
        needle: "lowercase",
    },
    Check::FileContains {
        path: "README.md",
        needle: "hello-world",
    },
];
const EXPORTS_CHECKS: &[Check] = &[Check::Command {
    script: "package_exports.py",
}];

pub const TASKS: &[Task] = &[
    Task {
        id: "truncate",
        prompt: TRUNCATE_PROMPT,
        check_script: "truncate.py",
        checks: TRUNCATE_CHECKS,
        cassette: "evals/truncate",
    },
    Task {
        id: "slugify",
        prompt: SLUGIFY_PROMPT,
        check_script: "slugify.py",
        checks: SLUGIFY_CHECKS,
        cassette: "evals/slugify",
    },
    Task {
        id: "parse-int",
        prompt: PARSE_INT_PROMPT,
        check_script: "parse_int.py",
        checks: PARSE_INT_CHECKS,
        cassette: "evals/parse-int",
    },
    Task {
        id: "mean",
        prompt: MEAN_PROMPT,
        check_script: "mean.py",
        checks: MEAN_CHECKS,
        cassette: "evals/mean",
    },
    Task {
        id: "clamp",
        prompt: CLAMP_PROMPT,
        check_script: "clamp.py",
        checks: CLAMP_CHECKS,
        cassette: "evals/clamp",
    },
    Task {
        id: "unique",
        prompt: UNIQUE_PROMPT,
        check_script: "unique.py",
        checks: UNIQUE_CHECKS,
        cassette: "evals/unique",
    },
    Task {
        id: "chunk",
        prompt: CHUNK_PROMPT,
        check_script: "chunk.py",
        checks: CHUNK_CHECKS,
        cassette: "evals/chunk",
    },
    Task {
        id: "format-table",
        prompt: FORMAT_TABLE_PROMPT,
        check_script: "format_table.py",
        checks: FORMAT_TABLE_CHECKS,
        cassette: "evals/format-table",
    },
    Task {
        id: "refactor-whitespace",
        prompt: REFACTOR_PROMPT,
        check_script: "refactor_whitespace.py",
        checks: REFACTOR_CHECKS,
        cassette: "evals/refactor-whitespace",
    },
    Task {
        id: "rename-format-row",
        prompt: RENAME_PROMPT,
        check_script: "rename_format_row.py",
        checks: RENAME_CHECKS,
        cassette: "evals/rename-format-row",
    },
    Task {
        id: "readme-slugify",
        prompt: README_PROMPT,
        check_script: "readme_slugify.py",
        checks: README_CHECKS,
        cassette: "evals/readme-slugify",
    },
    Task {
        id: "package-exports",
        prompt: EXPORTS_PROMPT,
        check_script: "package_exports.py",
        checks: EXPORTS_CHECKS,
        cassette: "evals/package-exports",
    },
];
