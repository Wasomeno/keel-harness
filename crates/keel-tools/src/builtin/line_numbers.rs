//! The `read` line-number gutter.
//!
//! A numbered line is a right-aligned 1-based line number, the `→` marker,
//! then the file line verbatim. `edit` accepts a block copied from `read` and
//! strips the gutter when the literal text does not match, so the displayed
//! prefix cannot turn into a new edit failure.

/// Marks the boundary between the displayed line number and the file text.
const GUTTER: char = '→';

/// The fixed width of the line-number field. A longer number is not cut off;
/// it simply pushes the marker to the right.
const NUMBER_WIDTH: usize = 6;

pub(crate) fn format_line(number: usize, text: &str) -> String {
    format!("{number:>NUMBER_WIDTH$}{GUTTER}{text}")
}

/// Remove a uniform `read` gutter from a block copied out of the tool output.
///
/// The block must have the marker on every line and the numbers must be
/// consecutive, which is what a selection from one `read` result looks like.
/// Anything else is returned as `None` so a literal edit still fails with the
/// normal message instead of guessing.
pub(crate) fn strip_numbered_block(text: &str) -> Option<String> {
    let mut previous: Option<usize> = None;
    let mut stripped = Vec::new();
    for line in text.split('\n') {
        let (number, content) = parse_numbered_line(line)?;
        if let Some(previous) = previous {
            if number != previous + 1 {
                return None;
            }
        }
        previous = Some(number);
        stripped.push(content.to_owned());
    }
    Some(stripped.join("\n"))
}

fn parse_numbered_line(line: &str) -> Option<(usize, &str)> {
    let (prefix, content) = line.split_once(GUTTER)?;
    let digits = prefix.trim_start_matches(' ');
    // At least one leading space keeps ordinary file text such as `12→x`
    // from being mistaken for the read gutter. A number wide enough to fill
    // the six-character field legitimately has no leading space.
    let no_space = prefix.len() == digits.len();
    if prefix.len() < 2
        || digits.is_empty()
        || (no_space && digits.len() < NUMBER_WIDTH)
        || !digits.chars().all(|character| character.is_ascii_digit())
    {
        return None;
    }
    let number = digits.parse().ok()?;
    Some((number, content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_numbered_right_aligned_and_followed_by_the_marker() {
        assert_eq!(format_line(1, "fn main() {"), "     1→fn main() {");
        assert_eq!(format_line(42, "}"), "    42→}");
        assert_eq!(
            format_line(1_000_000, "big"),
            "1000000→big",
            "a number wider than the field is not truncated"
        );
    }

    #[test]
    fn a_copied_block_is_stripped_back_to_file_text() {
        let block = format!(
            "{}\n{}\n{}",
            format_line(10, "let a = 1;"),
            format_line(11, ""),
            format_line(12, "let b = 2;")
        );
        assert_eq!(
            strip_numbered_block(&block).as_deref(),
            Some("let a = 1;\n\nlet b = 2;")
        );
    }

    #[test]
    fn a_full_width_number_without_leading_space_is_still_stripped() {
        assert_eq!(strip_numbered_block("100000→x").as_deref(), Some("x"));
        assert_eq!(strip_numbered_block("12345→x"), None);
    }

    #[test]
    fn a_non_consecutive_or_unmarked_block_is_not_stripped() {
        let skipped = format!("{}\n{}", format_line(1, "a"), format_line(3, "b"));
        assert_eq!(strip_numbered_block(&skipped), None);
        assert_eq!(strip_numbered_block("plain\ntext"), None);
        assert_eq!(strip_numbered_block("12→not a gutter"), None);
    }
}
