//! Session loop, route resolution and event assembly.
//!
//! Dependency rule: this is the top of the provider stack. Types, protocols and
//! routes flow upward into it; nothing here leaks into them.

pub mod assemble;
pub mod builder;
pub mod compact;
pub mod prompt;
pub mod run;
pub mod session;
pub mod store;
pub mod test_loop;
pub mod tokens;

pub use assemble::{Assembler, Turn};
pub use builder::build_route;
pub use compact::{
    COMPACT_USER_MESSAGE_MAX_TOKENS, COMPACTION_PROMPT, CompactionReason, CompactionStats,
    SUMMARY_MAX_OUTPUT_TOKENS, SUMMARY_MIN_OUTPUT_TOKENS, SUMMARY_PREFIX, compacted_view,
    compacted_view_with_budget, is_verbatim_user_message, summary_message, summary_output_budget,
    user_message_budget,
};
pub use prompt::{DEFAULT_SYSTEM_PROMPT, SYSTEM_PROMPT_SEPARATOR, effective_system_prompt};
pub use run::{Harness, RunEvent, RunOutcome, RunStop, short_limits};
pub use session::{Session, SessionSink, StepOutcome};
pub use store::{
    CompactionRecord, LOG_VERSION, SessionLog, SessionMeta, SessionSummary, StoreError,
};
pub use test_loop::{
    TestOutcome, TestRun, TestStatus, VERIFY_PROMPT_PREFIX, is_test_command, parse_test_output,
    test_command, verification_prompt,
};
pub use tokens::{
    estimate_message_tokens, estimate_messages_tokens, estimate_request_input_tokens,
    estimate_text_tokens, estimate_tools_tokens,
};
