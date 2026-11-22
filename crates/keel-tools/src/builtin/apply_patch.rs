use std::io::Read;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{
    Tool, ToolContext, ToolError, ToolFuture, ToolOutcome, ToolSpec, parse_arguments, tool_spec,
};
use crate::workspace::Workspace;

pub struct ApplyPatchTool;

#[derive(Deserialize)]
struct ApplyPatchArguments {
    patch: String,
}

impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "apply_patch",
            "Use when: changing several places in one file, or several files, from one read of the code. It applies all hunks in one call instead of one `edit` round trip per place.\n\
             Prefer: `edit` for one exact replacement and `write` to create or rewrite a whole file. Read the file first: hunk positions and context lines must match its current contents.\n\
             Returns: one summary with the files changed, hunks applied, and lines added and removed; each file is replaced atomically. Every hunk is checked in memory before any file is written, so a mismatch leaves all target files untouched.\n\
             Accepted format (unified diff, not full `git apply`): one or more file sections, each `--- old` then `+++ new` then one or more `@@ -old_start,old_count +new_start,new_count @@` hunks. Each hunk body line starts with a space (context), `-` (remove) or `+` (add); the optional `\\ No newline at end of file` marker is accepted. `a/` and `b/` prefixes, a tab-separated timestamp, and `diff --git`/`index` metadata lines are ignored. Context and removed lines match strictly on content and leading whitespace, but trailing spaces and tabs may differ. Create a file with `--- /dev/null`; deleting or renaming a file is not supported. Binary patches, mode changes, copies and fuzzy matching are not supported.\n\
             On failure: a parse error names the patch line; a mismatch names the file, hunk number, expected line and closest actual line; an escaping path names the workspace root and nothing is written.",
            json!({
                "type": "object",
                "properties": {
                    "patch": {"type": "string", "description": "Unified diff text with ---/+++/@@ headers."}
                },
                "required": ["patch"]
            }),
        )
    }

    fn summarize(&self, arguments: &Value) -> String {
        let bytes = arguments
            .get("patch")
            .and_then(Value::as_str)
            .map(str::len)
            .unwrap_or(0);
        format!("apply_patch ({bytes} bytes)")
    }

    fn execute<'a>(&'a self, arguments: Value, context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            match apply_patch(arguments, context).await {
                Ok(text) => ToolOutcome::ok(text),
                Err(error) => error.into_outcome(),
            }
        })
    }
}

async fn apply_patch(arguments: Value, context: &ToolContext) -> Result<String, ToolError> {
    let arguments: ApplyPatchArguments = parse_arguments(arguments)?;
    let patch = parse_patch(&arguments.patch)?;
    let workspace = context.workspace.clone();
    tokio::task::spawn_blocking(move || apply(&workspace, &patch))
        .await
        .map_err(|error| {
            ToolError::Io(format!(
                "could not apply the patch: the blocking applier failed: {error}"
            ))
        })?
}

#[derive(Debug)]
struct Patch {
    files: Vec<FilePatch>,
}

#[derive(Debug)]
struct FilePatch {
    old_path: String,
    new_path: String,
    hunks: Vec<Hunk>,
}

#[derive(Debug)]
struct Hunk {
    old_start: usize,
    old_len: usize,
    new_len: usize,
    lines: Vec<HunkLine>,
    old_no_newline: bool,
    new_no_newline: bool,
}

#[derive(Debug)]
enum HunkLine {
    Context(String),
    Add(String),
    Remove(String),
}

#[derive(Clone, Copy)]
enum LastLine {
    Context,
    Add,
    Remove,
}

fn parse_patch(text: &str) -> Result<Patch, ToolError> {
    let text = strip_code_fence(text);
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let mut files = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.trim().is_empty() || is_ignored_metadata(line) {
            index += 1;
            continue;
        }
        let Some(old_header) = line.strip_prefix("--- ") else {
            return Err(parse_error(
                index + 1,
                format!("expected a `---` file header, found `{}`", preview(line)),
            ));
        };
        let file_line = index + 1;
        let old_path = strip_git_prefix(parse_header_path(old_header), "a/");
        index += 1;
        while index < lines.len() && is_ignored_metadata(lines[index]) {
            index += 1;
        }
        let Some(new_header) = lines.get(index).and_then(|line| line.strip_prefix("+++ ")) else {
            return Err(parse_error(
                index + 1,
                "expected a `+++` header after `---`",
            ));
        };
        let new_path = strip_git_prefix(parse_header_path(new_header), "b/");
        index += 1;

        let mut hunks = Vec::new();
        while index < lines.len() {
            let line = lines[index];
            if line.trim().is_empty() || is_ignored_metadata(line) {
                index += 1;
                continue;
            }
            if line.starts_with("--- ") {
                break;
            }
            if line.starts_with("@@") {
                let (hunk, next) = parse_hunk(&lines, index)?;
                hunks.push(hunk);
                index = next;
                continue;
            }
            return Err(parse_error(
                index + 1,
                format!(
                    "expected an `@@` hunk header or the next `---` file header, found `{}`",
                    preview(line)
                ),
            ));
        }
        if hunks.is_empty() {
            return Err(parse_error(
                file_line,
                format!("file section for `{new_path}` has no hunks"),
            ));
        }
        files.push(FilePatch {
            old_path,
            new_path,
            hunks,
        });
    }
    if files.is_empty() {
        return Err(ToolError::InvalidArguments(
            "`patch` contains no `---`/`+++` file sections; pass a unified diff".to_owned(),
        ));
    }
    Ok(Patch { files })
}

fn parse_hunk(lines: &[&str], start: usize) -> Result<(Hunk, usize), ToolError> {
    let header_line = start + 1;
    let (old_start, old_len, _new_start, new_len) =
        parse_hunk_header(lines[start]).ok_or_else(|| {
            parse_error(
                header_line,
                format!(
                    "invalid hunk header `{}`; expected `@@ -old_start,old_count +new_start,new_count @@`",
                    preview(lines[start])
                ),
            )
        })?;
    if old_len > 0 && old_start == 0 {
        return Err(parse_error(
            header_line,
            "a hunk cannot match old lines starting at line 0; a pure insertion uses `-0,0`",
        ));
    }

    let mut index = start + 1;
    let mut old_seen = 0usize;
    let mut new_seen = 0usize;
    let mut body = Vec::new();
    let mut last = None;
    let mut old_no_newline = false;
    let mut new_no_newline = false;

    while old_seen < old_len || new_seen < new_len {
        let Some(line) = lines.get(index) else {
            return Err(parse_error(
                index + 1,
                format!(
                    "hunk expects {old_len} old and {new_len} new lines; the patch ended after {old_seen} old and {new_seen} new lines"
                ),
            ));
        };
        match line.chars().next() {
            Some(' ') => {
                body.push(HunkLine::Context(line[1..].to_owned()));
                old_seen += 1;
                new_seen += 1;
                last = Some(LastLine::Context);
            }
            Some('-') => {
                body.push(HunkLine::Remove(line[1..].to_owned()));
                old_seen += 1;
                last = Some(LastLine::Remove);
            }
            Some('+') => {
                body.push(HunkLine::Add(line[1..].to_owned()));
                new_seen += 1;
                last = Some(LastLine::Add);
            }
            Some('\\') => {
                handle_no_newline_marker(
                    line,
                    index + 1,
                    last,
                    old_seen,
                    old_len,
                    new_seen,
                    new_len,
                    &mut old_no_newline,
                    &mut new_no_newline,
                )?;
            }
            _ => {
                return Err(parse_error(
                    index + 1,
                    format!(
                        "hunk line must start with a space (context), `-` (remove), `+` (add) or `\\` (no-newline marker), found `{}`",
                        preview(line)
                    ),
                ));
            }
        }
        index += 1;
        if old_seen > old_len || new_seen > new_len {
            return Err(parse_error(
                index,
                "the hunk body has more lines than its header counts",
            ));
        }
    }

    while index < lines.len() && lines[index].starts_with('\\') {
        handle_no_newline_marker(
            lines[index],
            index + 1,
            last,
            old_seen,
            old_len,
            new_seen,
            new_len,
            &mut old_no_newline,
            &mut new_no_newline,
        )?;
        index += 1;
    }

    Ok((
        Hunk {
            old_start,
            old_len,
            new_len,
            lines: body,
            old_no_newline,
            new_no_newline,
        },
        index,
    ))
}

#[allow(clippy::too_many_arguments)]
fn handle_no_newline_marker(
    line: &str,
    line_number: usize,
    last: Option<LastLine>,
    old_seen: usize,
    old_len: usize,
    new_seen: usize,
    new_len: usize,
    old_no_newline: &mut bool,
    new_no_newline: &mut bool,
) -> Result<(), ToolError> {
    if !line.starts_with("\\ No newline at end of file") {
        return Err(parse_error(
            line_number,
            format!(
                "unknown `\\` line `{}`; only `\\ No newline at end of file` is accepted",
                preview(line)
            ),
        ));
    }
    match last {
        Some(LastLine::Context) if old_seen == old_len && new_seen == new_len => {
            *old_no_newline = true;
            *new_no_newline = true;
        }
        Some(LastLine::Remove) if old_seen == old_len => *old_no_newline = true,
        Some(LastLine::Add) if new_seen == new_len => *new_no_newline = true,
        Some(_) => {
            return Err(parse_error(
                line_number,
                "`\\ No newline at end of file` must follow the last line on its side of the hunk",
            ));
        }
        None => {
            return Err(parse_error(
                line_number,
                "`\\ No newline at end of file` has no preceding hunk line",
            ));
        }
    }
    Ok(())
}

fn parse_hunk_header(line: &str) -> Option<(usize, usize, usize, usize)> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.len() < 4 || tokens[0] != "@@" || tokens[3] != "@@" {
        return None;
    }
    let (old_start, old_len) = parse_range(tokens[1], '-')?;
    let (new_start, new_len) = parse_range(tokens[2], '+')?;
    Some((old_start, old_len, new_start, new_len))
}

fn parse_range(token: &str, sign: char) -> Option<(usize, usize)> {
    let rest = token.strip_prefix(sign)?;
    let mut parts = rest.split(',');
    let start = parts.next()?.parse().ok()?;
    let len = match parts.next() {
        Some(text) => text.parse().ok()?,
        None => 1,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((start, len))
}

fn parse_header_path(rest: &str) -> String {
    let path = rest.split('\t').next().unwrap_or(rest).trim();
    if path.len() >= 2 && path.starts_with('"') && path.ends_with('"') {
        path[1..path.len() - 1].to_owned()
    } else {
        path.to_owned()
    }
}

fn strip_git_prefix(path: String, prefix: &str) -> String {
    if path == "/dev/null" {
        return path;
    }
    path.strip_prefix(prefix).map(str::to_owned).unwrap_or(path)
}

fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim_matches('\n');
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let Some(newline) = rest.find('\n') else {
        return trimmed;
    };
    let body = rest[newline + 1..].trim_end();
    body.strip_suffix("```").map(str::trim_end).unwrap_or(body)
}

fn is_ignored_metadata(line: &str) -> bool {
    [
        "diff --git ",
        "index ",
        "new file mode ",
        "deleted file mode ",
        "old mode ",
        "new mode ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn parse_error(line: usize, message: impl std::fmt::Display) -> ToolError {
    ToolError::Message(format!("patch parse error at patch line {line}: {message}"))
}

fn preview(text: &str) -> String {
    let mut short: String = text.chars().take(120).collect();
    if text.chars().count() > 120 {
        short.push('…');
    }
    short
}

struct PreparedFile {
    path: String,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
    hunks: usize,
    added: usize,
    removed: usize,
    created: bool,
}

fn apply(workspace: &Workspace, patch: &Patch) -> Result<String, ToolError> {
    let mut prepared: Vec<PreparedFile> = Vec::new();
    for file in &patch.files {
        let prepared_file = prepare_file(workspace, file)?;
        if prepared
            .iter()
            .any(|existing| existing.path == prepared_file.path)
        {
            return Err(ToolError::Message(format!(
                "`{}` appears in more than one file section; combine its changes into one section",
                prepared_file.path
            )));
        }
        prepared.push(prepared_file);
    }

    let mut written: Vec<&PreparedFile> = Vec::new();
    for file in &prepared {
        let result = if file.created {
            workspace.write_file(&file.path, &file.after)
        } else {
            workspace.edit_file(&file.path, &file.after)
        };
        if let Err(error) = result {
            rollback(workspace, &written);
            return Err(ToolError::Io(format!(
                "{error}; the patch was not applied: files written before the failure were restored best-effort"
            )));
        }
        written.push(file);
    }

    Ok(summary(&prepared))
}

fn prepare_file(workspace: &Workspace, file: &FilePatch) -> Result<PreparedFile, ToolError> {
    let old_is_null = file.old_path == "/dev/null";
    let new_is_null = file.new_path == "/dev/null";
    if new_is_null {
        return Err(ToolError::Message(format!(
            "deleting `{}` is not supported by apply_patch; use `bash rm <path>` if you intend to delete it",
            file.old_path
        )));
    }
    let target = if old_is_null {
        file.new_path.clone()
    } else {
        if file.old_path != file.new_path {
            return Err(ToolError::Message(format!(
                "renaming `{}` to `{}` is not supported by apply_patch; keep the path the same",
                file.old_path, file.new_path
            )));
        }
        file.new_path.clone()
    };
    workspace.validate_path(&target)?;

    let (before, content) = if old_is_null {
        match workspace.open_existing(&target) {
            Ok(_) => {
                return Err(ToolError::Message(format!(
                    "`{target}` already exists; a new-file section needs `--- /dev/null` and a path that does not exist"
                )));
            }
            Err(ToolError::NotFound { .. }) => (None, String::new()),
            Err(error) => return Err(error),
        }
    } else {
        let mut handle = workspace.open_existing(&target)?;
        let metadata = handle.metadata().map_err(|error| {
            ToolError::Io(format!(
                "could not inspect `{target}` to apply the patch: {error}; check that it is a regular file"
            ))
        })?;
        if metadata.is_dir() {
            return Err(ToolError::Message(format!(
                "`{target}` is a directory; patch a file inside it"
            )));
        }
        let mut text = String::new();
        handle.read_to_string(&mut text).map_err(|error| {
            ToolError::Io(format!(
                "could not read `{target}` to apply the patch: {error}; apply_patch needs UTF-8 text"
            ))
        })?;
        (Some(text.clone().into_bytes()), text)
    };

    let (after, added, removed) = apply_hunks(&target, &content, file, old_is_null)?;
    Ok(PreparedFile {
        path: target,
        before,
        after: after.into_bytes(),
        hunks: file.hunks.len(),
        added,
        removed,
        created: old_is_null,
    })
}

fn apply_hunks(
    path: &str,
    content: &str,
    file: &FilePatch,
    new_file: bool,
) -> Result<(String, usize, usize), ToolError> {
    let crlf = content.contains("\r\n");
    // A new file ends with a newline unless its patch says otherwise. An
    // existing file keeps the ending it already had.
    let mut ends_with_newline = if new_file {
        true
    } else {
        content.ends_with('\n')
    };
    let mut lines = split_lines(content);
    let mut delta: isize = 0;
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut previous_end = 0usize;

    for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        let number = hunk_index + 1;
        let index = hunk_old_index(hunk);
        if index < previous_end {
            return Err(ToolError::Message(format!(
                "patch failed for `{path}`, hunk {number}: it starts at old line {} but the previous hunk ends at old line {}; keep hunks in ascending, non-overlapping order",
                index + 1,
                previous_end
            )));
        }
        previous_end = index + hunk.old_len;

        let target = index as isize + delta;
        if target < 0 {
            return Err(ToolError::Message(format!(
                "patch failed for `{path}`, hunk {number}: the hunk resolves before the start of the file"
            )));
        }
        let target = target as usize;
        if target + hunk.old_len > lines.len() {
            let expected = hunk
                .lines
                .iter()
                .find_map(|line| match line {
                    HunkLine::Context(text) | HunkLine::Remove(text) => Some(text.as_str()),
                    HunkLine::Add(_) => None,
                })
                .unwrap_or("");
            return Err(context_failure(
                path,
                number,
                index + 1,
                expected,
                None,
                &lines,
            ));
        }

        let old_slice: Vec<String> = lines[target..target + hunk.old_len].to_vec();
        let mut cursor = 0usize;
        let mut replacement = Vec::new();
        for line in &hunk.lines {
            match line {
                HunkLine::Context(expected) => {
                    let actual = &old_slice[cursor];
                    if !same_content(expected, actual) {
                        return Err(context_failure(
                            path,
                            number,
                            index + cursor + 1,
                            expected,
                            Some((target + cursor + 1, actual)),
                            &lines,
                        ));
                    }
                    replacement.push(actual.clone());
                    cursor += 1;
                }
                HunkLine::Remove(expected) => {
                    let actual = &old_slice[cursor];
                    if !same_content(expected, actual) {
                        return Err(context_failure(
                            path,
                            number,
                            index + cursor + 1,
                            expected,
                            Some((target + cursor + 1, actual)),
                            &lines,
                        ));
                    }
                    cursor += 1;
                    removed += 1;
                }
                HunkLine::Add(text) => {
                    replacement.push(text.clone());
                    added += 1;
                }
            }
        }
        debug_assert_eq!(cursor, hunk.old_len);
        lines.splice(target..target + hunk.old_len, replacement);
        delta += hunk.new_len as isize - hunk.old_len as isize;
        if hunk.new_no_newline {
            ends_with_newline = false;
        } else if hunk.old_no_newline {
            ends_with_newline = true;
        }
    }

    let newline = if crlf { "\r\n" } else { "\n" };
    let mut output = lines.join(newline);
    if ends_with_newline && !lines.is_empty() {
        output.push_str(newline);
    }
    Ok((output, added, removed))
}

fn hunk_old_index(hunk: &Hunk) -> usize {
    if hunk.old_len == 0 {
        hunk.old_start
    } else {
        hunk.old_start - 1
    }
}

fn split_lines(content: &str) -> Vec<String> {
    if content.is_empty() {
        return Vec::new();
    }
    let body = content.strip_suffix('\n').unwrap_or(content);
    body.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect()
}

fn same_content(expected: &str, actual: &str) -> bool {
    trim_trailing(expected) == trim_trailing(actual)
}

fn trim_trailing(text: &str) -> &str {
    text.trim_end_matches([' ', '\t'])
}

fn context_failure(
    path: &str,
    hunk: usize,
    expected_line: usize,
    expected: &str,
    actual: Option<(usize, &str)>,
    lines: &[String],
) -> ToolError {
    let mut message = format!(
        "patch failed for `{path}`, hunk {hunk}: expected line {expected_line} `{}`",
        preview(expected)
    );
    match actual {
        Some((line, text)) => {
            message.push_str(&format!(" but found line {line} `{}`", preview(text)))
        }
        None => message.push_str(&format!(" but `{path}` has only {} lines", lines.len())),
    }
    if let Some((line, text)) = closest_line(lines, expected, expected_line) {
        message.push_str(&format!(
            "; closest actual line {line}: `{}`",
            preview(text)
        ));
    }
    message
        .push_str(". Read the file again and regenerate the patch against its current contents.");
    ToolError::Message(message)
}

fn closest_line<'a>(
    lines: &'a [String],
    expected: &str,
    expected_line: usize,
) -> Option<(usize, &'a str)> {
    let expected_index = expected_line.saturating_sub(1);
    let mut best: Option<(usize, &'a str, (usize, usize, usize))> = None;
    for (index, line) in lines.iter().enumerate() {
        let prefix = common_prefix(expected, line);
        let suffix = common_suffix(expected, line);
        let key = (
            prefix + suffix,
            prefix,
            usize::MAX - index.abs_diff(expected_index),
        );
        let replace = match &best {
            Some((_, _, best_key)) => key > *best_key,
            None => true,
        };
        if replace {
            best = Some((index + 1, line.as_str(), key));
        }
    }
    best.map(|(number, line, _)| (number, line))
}

fn common_prefix(left: &str, right: &str) -> usize {
    left.chars()
        .zip(right.chars())
        .take_while(|(left, right)| left == right)
        .count()
}

fn common_suffix(left: &str, right: &str) -> usize {
    left.chars()
        .rev()
        .zip(right.chars().rev())
        .take_while(|(left, right)| left == right)
        .count()
}

fn summary(files: &[PreparedFile]) -> String {
    let file_count = files.len();
    let hunk_count: usize = files.iter().map(|file| file.hunks).sum();
    let added: usize = files.iter().map(|file| file.added).sum();
    let removed: usize = files.iter().map(|file| file.removed).sum();
    let mut text = format!(
        "applied {hunk_count} {} to {file_count} {}: +{added} -{removed}",
        plural(hunk_count, "hunk"),
        plural(file_count, "file")
    );
    for file in files {
        text.push_str(&format!(
            "\n- {}: {} {}, +{} -{}{}",
            file.path,
            file.hunks,
            plural(file.hunks, "hunk"),
            file.added,
            file.removed,
            if file.created { " (new file)" } else { "" }
        ));
    }
    text
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        word.to_owned()
    } else {
        format!("{word}s")
    }
}

fn rollback(workspace: &Workspace, written: &[&PreparedFile]) {
    for file in written.iter().rev() {
        match &file.before {
            Some(before) => {
                let _ = workspace.edit_file(&file.path, before);
            }
            None => {
                let _ = workspace.remove_file(&file.path);
            }
        }
    }
}
