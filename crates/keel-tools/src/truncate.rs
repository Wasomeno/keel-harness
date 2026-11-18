//! Output truncation.
//!
//! Tool output can be arbitrarily large; the context window cannot. Keep the
//! head and the tail, where errors and summaries usually are, and say how much
//! was dropped.

/// Truncate to roughly `max_bytes`, never splitting a UTF-8 character.
pub fn truncate_output(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }

    let max = max_bytes.max(256);
    let head_budget = max * 6 / 10;
    let tail_budget = max - head_budget;

    let head_end = floor_boundary(text, head_budget.min(text.len()));
    let tail_start = ceil_boundary(text, text.len().saturating_sub(tail_budget));
    let omitted = tail_start.saturating_sub(head_end);

    format!(
        "{}\n\n[... {omitted} bytes truncated ...]\n\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_unchanged() {
        assert_eq!(truncate_output("hello", 100), "hello");
    }

    #[test]
    fn long_output_keeps_head_and_tail() {
        let text = format!("START{}END", "x".repeat(10_000));
        let truncated = truncate_output(&text, 1024);
        assert!(truncated.starts_with("START"));
        assert!(truncated.ends_with("END"));
        assert!(truncated.contains("bytes truncated"));
        assert!(truncated.len() < 1200);
    }

    #[test]
    fn never_splits_a_multibyte_character() {
        let text = "é".repeat(5000);
        let truncated = truncate_output(&text, 300);
        // The result is valid UTF-8 because it came from str slices; the real
        // check is that construction did not panic.
        assert!(truncated.contains("bytes truncated"));
    }
}
