//! The default system prompt.
//!
//! One prompt for every model and protocol, embedded at compile time. The M2
//! tool descriptions own when-to-use guidance for each tool; this file owns
//! keel's identity and general working style.

/// The prompt every new session starts with.
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("prompts/default_system.md");

/// Instructions injected when plan mode is enabled without shell access.
pub const PLAN_MODE_PROMPT: &str = include_str!("prompts/plan_mode.md");

/// Instructions injected when plan mode is enabled with sandboxed `bash`.
pub const PLAN_MODE_PROMPT_WITH_BASH: &str = include_str!("prompts/plan_mode_with_bash.md");

/// Selects the plan-mode prompt matching whether `bash` is advertised.
pub const fn plan_mode_prompt(bash_available: bool) -> &'static str {
    if bash_available {
        PLAN_MODE_PROMPT_WITH_BASH
    } else {
        PLAN_MODE_PROMPT
    }
}

/// Notice injected when plan mode is turned off mid-session.
pub const PLAN_MODE_OFF_NOTICE: &str =
    "Plan mode is now off. Mutating tools and normal execution are allowed again.";

/// Joins the prompt already in effect to text supplied by the host.
///
/// A horizontal rule is visible to the model as a boundary without looking
/// like another section of keel's own instructions.
pub const SYSTEM_PROMPT_SEPARATOR: &str = "\n\n---\n\n";

/// Builds the effective system prompt.
///
/// `base` is what is already in effect. A new session passes
/// [`DEFAULT_SYSTEM_PROMPT`]; a resumed session passes the header's stored
/// value, so the default is never prepended a second time. `None` means the
/// session predates the default and recorded no prompt: it stays prompt-less,
/// and `appended` alone is used rather than injecting the default into an
/// existing transcript.
pub fn effective_system_prompt(base: Option<&str>, appended: Option<&str>) -> Option<String> {
    match (base, appended) {
        (Some(base), Some(appended)) => Some(format!("{base}{SYSTEM_PROMPT_SEPARATOR}{appended}")),
        (Some(base), None) => Some(base.to_owned()),
        (None, Some(appended)) => Some(appended.to_owned()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_prompt_names_keel_and_the_agent_role() {
        assert!(
            DEFAULT_SYSTEM_PROMPT.starts_with("You are keel, a coding agent."),
            "{DEFAULT_SYSTEM_PROMPT}"
        );
    }

    #[test]
    fn appended_text_follows_the_default_after_a_clear_separator() {
        let effective =
            effective_system_prompt(Some(DEFAULT_SYSTEM_PROMPT), Some("Answer in haiku."))
                .expect("the default is present");

        assert_eq!(
            effective,
            format!("{DEFAULT_SYSTEM_PROMPT}{SYSTEM_PROMPT_SEPARATOR}Answer in haiku.")
        );
        assert!(effective.starts_with(DEFAULT_SYSTEM_PROMPT));
        assert!(effective.ends_with("Answer in haiku."));
    }

    #[test]
    fn a_stored_prompt_is_used_as_is() {
        assert_eq!(
            effective_system_prompt(Some("stored prompt"), None),
            Some("stored prompt".to_owned())
        );
    }

    #[test]
    fn appended_text_after_a_stored_prompt_does_not_add_the_default() {
        assert_eq!(
            effective_system_prompt(Some("stored prompt"), Some("more")),
            Some(format!("stored prompt{SYSTEM_PROMPT_SEPARATOR}more"))
        );
    }

    #[test]
    fn a_legacy_session_without_a_recorded_prompt_takes_appended_text_alone() {
        assert_eq!(effective_system_prompt(None, None), None);
        assert_eq!(
            effective_system_prompt(None, Some("extra")),
            Some("extra".to_owned())
        );
    }

    #[test]
    fn plan_mode_prompt_matches_bash_availability() {
        let without_bash = plan_mode_prompt(false);
        assert!(!without_bash.contains("You may also use `bash`"));
        assert!(without_bash.contains("shell commands"));

        let with_bash = plan_mode_prompt(true);
        assert!(with_bash.contains("You may also use `bash`"));
        assert!(with_bash.contains("read-only, network-disabled OS sandbox"));
    }
}
