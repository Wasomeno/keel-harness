//! Visible-test tracking and the pre-final verification prompt.
//!
//! Benchmark runs need two things normal chat does not: a record of whether the
//! agent ran the repository's own tests, and one final nudge that asks for the
//! change/test/unverified summary instead of accepting the first `EndTurn`.
//!
//! The loop is opt-in through [`Harness::with_test_loop`](crate::Harness::with_test_loop).
//! It never runs tests itself: it watches `bash` and `terminal` calls whose
//! command looks like a test runner, parses the result the tool already
//! returned, and keeps the last status on the session so compaction can carry
//! it forward.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Prefix of the one pre-final user message this module injects.
pub const VERIFY_PROMPT_PREFIX: &str = "Before you finish:";

/// At most this many failing test names are kept from one run.
const MAX_FAILING_TESTS: usize = 8;

/// At most this many bytes of failure excerpt are kept.
const MAX_EXCERPT_BYTES: usize = 600;

/// One visible test command's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    Passed,
    Failed,
}

impl TestOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

/// What one test command did. `failing_tests` and `excerpt` are empty on a pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestRun {
    pub command: String,
    pub outcome: TestOutcome,
    pub failing_tests: Vec<String>,
    pub excerpt: Option<String>,
}

/// The latest visible-test status for a session.
///
/// `runs` counts test commands started, including a command that failed to
/// start. `failing_tests` and `excerpt` describe the latest run only, so a
/// later pass clears an earlier failure.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestStatus {
    pub runs: usize,
    pub last_command: Option<String>,
    pub last_outcome: Option<TestOutcome>,
    pub failing_tests: Vec<String>,
    pub excerpt: Option<String>,
}

impl TestStatus {
    pub fn tests_run(&self) -> bool {
        self.runs > 0
    }

    /// Records one run and replaces the latest-run fields.
    pub fn record(&mut self, run: TestRun) {
        self.runs += 1;
        self.last_command = Some(run.command);
        self.last_outcome = Some(run.outcome);
        self.failing_tests = run.failing_tests;
        self.excerpt = run.excerpt;
    }

    /// A compact block for the compaction summary and the post-compaction view.
    /// `None` before any test command.
    pub fn working_memory(&self) -> Option<String> {
        if self.runs == 0 {
            return None;
        }
        let mut lines = vec![format!(
            "Visible test status (tracked by the harness): {} run(s); last {}",
            self.runs,
            self.last_outcome
                .map(TestOutcome::as_str)
                .unwrap_or("unknown")
        )];
        if let Some(command) = &self.last_command {
            lines.push(format!("- last command: `{command}`"));
        }
        if !self.failing_tests.is_empty() {
            lines.push(format!(
                "- failing tests: {}",
                self.failing_tests.join(", ")
            ));
        }
        if let Some(excerpt) = &self.excerpt {
            lines.push(format!("- last failure excerpt: {excerpt}"));
        }
        Some(lines.join("\n"))
    }
}

/// The one pre-final message. It asks for the three things a benchmark trace
/// needs and, when appropriate, names the missing or failing test.
pub fn verification_prompt(status: &TestStatus) -> String {
    let mut text = String::from(
        "Before you finish: state what you changed, what you ran, and what is \
         still unverified.",
    );
    if !status.tests_run() {
        text.push_str(
            "\nYou have not run a visible test in this workspace. Run the \
             narrowest test that exercises your change, then the module it \
             belongs to, and the full suite only if time allows.",
        );
    } else if status.last_outcome == Some(TestOutcome::Failed) {
        text.push_str(
            "\nYour last visible test run failed. Fix the cause and run that \
             test again, or say plainly why it cannot pass.",
        );
    }
    text.push_str(
        "\nDo not change or delete tests to make them pass. After the test \
         result, give the final summary.",
    );
    text
}

/// The `command` argument of a `bash` or `terminal` call when it looks like a
/// test command.
pub fn test_command(arguments: &Value) -> Option<String> {
    let command = arguments.get("command")?.as_str()?;
    is_test_command(command).then(|| command.to_owned())
}

/// Whether a shell command invokes a recognisable test runner.
///
/// The check is deliberately narrow: a `grep pytest` or `cat pytest.ini` is not
/// a test run. Wrappers such as `uv run`, `poetry run` and `npx` are followed.
pub fn is_test_command(command: &str) -> bool {
    for segment in command.split(['&', '|', ';', '\n']) {
        if segment_is_test_command(segment) {
            return true;
        }
    }
    false
}

fn segment_is_test_command(segment: &str) -> bool {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let mut index = 0;
    // Skip environment assignments, `sudo`, `time`, `command` and the timeout
    // value that belongs to `timeout`.
    while index < tokens.len() {
        let token = tokens[index];
        if (token.contains('=') && !token.starts_with('-'))
            || matches!(token, "sudo" | "time" | "command" | "exec" | "builtin")
        {
            index += 1;
        } else if token == "env" {
            index += 1;
            while index < tokens.len()
                && (tokens[index].starts_with('-') || tokens[index].contains('='))
            {
                index += 1;
            }
        } else if token == "timeout" {
            index += 1;
            while index < tokens.len() && tokens[index].starts_with('-') {
                index += 1;
            }
            if index < tokens.len() && tokens[index].chars().all(|c| c.is_ascii_digit()) {
                index += 1;
            }
        } else if matches!(token, "cd" | "pushd" | "popd") {
            return false;
        } else {
            break;
        }
    }
    if index >= tokens.len() {
        return false;
    }
    let first = tokens[index];
    let name = std::path::Path::new(first)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(first);
    let rest = &tokens[index + 1..];

    if is_runner_name(name) {
        return true;
    }
    if matches!(
        name,
        "python" | "python3" | "python3.11" | "python3.12" | "python3.13"
    ) || name.starts_with("python3.")
    {
        return python_runs_tests(rest);
    }
    if let Some(wrapped) = wrapper_remainder(name, rest) {
        return segment_is_test_command(&wrapped);
    }
    match name {
        "cargo" => rest.first() == Some(&"test") || rest.first() == Some(&"nextest"),
        "go" => rest.first() == Some(&"test"),
        "npm" | "pnpm" | "yarn" | "bun" => {
            rest.first() == Some(&"test")
                || (rest.first() == Some(&"run") && rest.get(1) == Some(&"test"))
        }
        "make" => rest
            .iter()
            .any(|token| matches!(*token, "test" | "tests" | "check")),
        "mvn" | "mvnw" => rest.iter().any(|token| matches!(*token, "test" | "verify")),
        "gradle" | "gradlew" => rest.iter().any(|token| matches!(*token, "test" | "check")),
        "dotnet" | "swift" | "mix" => rest.first() == Some(&"test"),
        _ => is_test_script(name),
    }
}

fn is_runner_name(name: &str) -> bool {
    matches!(
        name,
        "pytest"
            | "py.test"
            | "tox"
            | "nox"
            | "ctest"
            | "rspec"
            | "phpunit"
            | "jest"
            | "vitest"
            | "mocha"
    )
}

fn python_runs_tests(rest: &[&str]) -> bool {
    if let Some(position) = rest.iter().position(|token| *token == "-m") {
        return matches!(
            rest.get(position + 1),
            Some(&"pytest") | Some(&"unittest") | Some(&"tox") | Some(&"nox")
        );
    }
    false
}

fn wrapper_remainder<'a>(name: &str, rest: &'a [&'a str]) -> Option<String> {
    let wrapped = match name {
        "uv" | "poetry" | "pdm" | "hatch" | "pipenv" | "rye" if rest.first() == Some(&"run") => {
            &rest[1..]
        }
        "bundle" if rest.first() == Some(&"exec") => &rest[1..],
        "npx" | "pnpx" => rest,
        _ => return None,
    };
    (!wrapped.is_empty()).then(|| wrapped.join(" "))
}

fn is_test_script(name: &str) -> bool {
    matches!(
        name,
        "test" | "tests" | "run_tests" | "run-tests" | "test.sh" | "run_tests.sh"
    )
}

/// Parses one test command's already-truncated output.
pub fn parse_test_output(command: &str, output: &str, is_error: bool) -> TestRun {
    let failed_marker = output.lines().any(|line| {
        let line = line.trim();
        line.starts_with("test result: FAILED")
            || line.starts_with("FAILED (")
            || line.starts_with("FAILURES!")
            || line == "FAILED"
    });
    let failed = is_error || failed_marker;
    if !failed {
        return TestRun {
            command: command.to_owned(),
            outcome: TestOutcome::Passed,
            failing_tests: Vec::new(),
            excerpt: None,
        };
    }

    let (failing_tests, excerpt) = failure_details(output);
    TestRun {
        command: command.to_owned(),
        outcome: TestOutcome::Failed,
        failing_tests,
        excerpt,
    }
}

fn failure_details(output: &str) -> (Vec<String>, Option<String>) {
    let mut names: Vec<String> = Vec::new();
    let mut excerpt_lines: Vec<String> = Vec::new();
    let mut in_cargo_failure_list = false;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            in_cargo_failure_list = false;
            continue;
        }
        if trimmed.eq_ignore_ascii_case("failures:") {
            // Cargo prints a detail block first and a plain name list second.
            // Only the second block's entries look like bare test paths.
            in_cargo_failure_list = true;
            continue;
        }
        if in_cargo_failure_list {
            if trimmed.starts_with("test result:") {
                in_cargo_failure_list = false;
            } else if is_cargo_failure_name(trimmed) {
                push_name(&mut names, trimmed);
                continue;
            } else {
                in_cargo_failure_list = false;
            }
        }

        if let Some(rest) = trimmed.strip_prefix("FAILED ") {
            push_name(&mut names, failure_name(rest));
        } else if let Some(rest) = trimmed.strip_prefix("ERROR ") {
            push_name(&mut names, failure_name(rest));
        } else if let Some(rest) = trimmed.strip_prefix("FAIL: ") {
            push_name(&mut names, rest.split(" (").next().unwrap_or(rest).trim());
        } else if let Some(rest) = trimmed.strip_prefix("test ") {
            if let Some((name, tail)) = rest.split_once(" ... ") {
                if tail.trim_start().starts_with("FAILED") {
                    push_name(&mut names, name.trim());
                }
            }
        }

        if let Some(rest) = trimmed.strip_prefix("E   ") {
            push_excerpt(&mut excerpt_lines, rest.trim());
        } else if trimmed.starts_with("E ") {
            push_excerpt(&mut excerpt_lines, trimmed.trim_start_matches("E ").trim());
        } else if trimmed.contains("AssertionError")
            || trimmed.starts_with("assert ")
            || trimmed.starts_with("panic")
            || trimmed.starts_with("thread '")
        {
            push_excerpt(&mut excerpt_lines, trimmed);
        }
    }

    if excerpt_lines.is_empty() {
        let tail: Vec<String> = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(str::to_owned)
            .collect();
        excerpt_lines = tail;
    }

    let excerpt = (!excerpt_lines.is_empty()).then(|| {
        let joined = excerpt_lines
            .into_iter()
            .take(5)
            .collect::<Vec<_>>()
            .join("\n");
        truncate_excerpt(&joined)
    });
    (names, excerpt)
}

fn is_cargo_failure_name(line: &str) -> bool {
    if line.contains(char::is_whitespace)
        || line.starts_with("----")
        || line.contains("panicked")
        || line.contains("assertion")
        || line.contains('=')
    {
        return false;
    }
    // Module paths contain `::`; a single colon is detail text, not a name.
    !line.contains(':') || line.contains("::")
}

fn failure_name(rest: &str) -> &str {
    rest.split(" - ").next().unwrap_or(rest).trim()
}

fn push_name(names: &mut Vec<String>, name: &str) {
    let name = name.trim();
    if name.is_empty() || name.starts_with('(') || names.iter().any(|existing| existing == name) {
        return;
    }
    if names.len() < MAX_FAILING_TESTS {
        names.push(name.to_owned());
    }
}

fn push_excerpt(lines: &mut Vec<String>, line: &str) {
    let line = line.trim();
    if line.is_empty() || lines.iter().any(|existing| existing == line) {
        return;
    }
    if lines.len() < 5 {
        lines.push(line.to_owned());
    }
}

fn truncate_excerpt(text: &str) -> String {
    if text.len() <= MAX_EXCERPT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_EXCERPT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PYTEST_FAILURE: &str = "\
============================= test session starts ==============================
collected 2 items

tests/test_app.py F.                                                       [100%]

=================================== FAILURES ===================================
________________________________ test_value ____________________________________

    def test_value():
>       assert load() == 2
E       assert 1 == 2

tests/test_app.py:5: AssertionError
=========================== short test summary info ============================
FAILED tests/test_app.py::test_value - assert 1 == 2
========================= 1 failed, 1 passed in 0.04s ==========================
";

    const CARGO_FAILURE: &str = "\
running 2 tests
test tests::loads ... ok
test tests::saves ... FAILED

failures:

---- tests::saves stdout ----
thread 'tests::saves' panicked at src/lib.rs:10:5:
assertion `left == right` failed
  left: 1
 right: 2

failures:
    tests::saves

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
";

    #[test]
    fn pytest_failure_keeps_the_name_and_an_assertion_excerpt() {
        let run = parse_test_output("python -m pytest tests/test_app.py", PYTEST_FAILURE, true);
        assert_eq!(run.outcome, TestOutcome::Failed);
        assert_eq!(run.failing_tests, vec!["tests/test_app.py::test_value"]);
        let excerpt = run.excerpt.expect("a failure excerpt");
        assert!(excerpt.contains("assert 1 == 2"), "{excerpt}");
    }

    #[test]
    fn cargo_failure_keeps_the_failing_test_name() {
        let run = parse_test_output("cargo test", CARGO_FAILURE, true);
        assert_eq!(run.outcome, TestOutcome::Failed);
        assert_eq!(run.failing_tests, vec!["tests::saves"]);
        assert!(run.excerpt.is_some());
    }

    #[test]
    fn a_passing_run_clears_the_failure_details() {
        let run = parse_test_output(
            "python -m pytest",
            "================ 2 passed in 0.01s ================\n",
            false,
        );
        assert_eq!(run.outcome, TestOutcome::Passed);
        assert!(run.failing_tests.is_empty());
        assert!(run.excerpt.is_none());
    }

    #[test]
    fn a_failure_without_a_known_shape_still_records_the_output_tail() {
        let run = parse_test_output("make test", "not found\nexit: 127", true);
        assert_eq!(run.outcome, TestOutcome::Failed);
        assert!(run.failing_tests.is_empty());
        assert!(run.excerpt.unwrap().contains("exit: 127"));
    }

    #[test]
    fn unittest_failures_are_named() {
        let run = parse_test_output(
            "python3 -m unittest",
            "FAIL: test_value (tests.test_app.TestApp.test_value)\n\
             FAILED (failures=1)\n",
            true,
        );
        assert_eq!(run.failing_tests, vec!["test_value"]);
    }

    #[test]
    fn test_commands_are_recognised_through_wrappers() {
        for command in [
            "pytest -q",
            "python -m pytest tests/test_app.py",
            "uv run pytest",
            "poetry run python -m pytest",
            "cargo test --workspace",
            "cargo nextest run",
            "go test ./...",
            "npm test",
            "npm run test -- --watch=false",
            "pnpm run test",
            "make test",
            "make -C repo check",
            "mvn test",
            "./gradlew test",
            "ctest",
            "dotnet test",
            "tox -e py311",
            "npx vitest run",
            "./tests/run_tests.sh",
            "timeout 60 python3 -m unittest discover",
            "cd repo && pytest",
            "FOO=1 pytest",
        ] {
            assert!(is_test_command(command), "expected test command: {command}");
        }
    }

    #[test]
    fn ordinary_commands_are_not_test_commands() {
        for command in [
            "grep -rn pytest src",
            "cat pytest.ini",
            "python -m pip install pytest",
            "cargo build",
            "make build",
            "echo cargo test",
            "npm install",
            "ls tests",
        ] {
            assert!(!is_test_command(command), "not a test command: {command}");
        }
    }

    #[test]
    fn the_command_argument_is_the_only_source() {
        assert_eq!(
            test_command(&json!({"command": "pytest -q"})),
            Some("pytest -q".to_owned())
        );
        assert_eq!(test_command(&json!({"path": "pytest"})), None);
    }

    #[test]
    fn working_memory_names_the_latest_failure() {
        let mut status = TestStatus::default();
        status.record(parse_test_output("pytest", PYTEST_FAILURE, true));
        let memory = status.working_memory().expect("a test status block");
        assert!(memory.contains("1 run(s)"));
        assert!(memory.contains("tests/test_app.py::test_value"));
        assert!(memory.contains("assert 1 == 2"));
    }

    #[test]
    fn the_verification_prompt_asks_for_the_missing_test() {
        let status = TestStatus::default();
        let prompt = verification_prompt(&status);
        assert!(prompt.starts_with(VERIFY_PROMPT_PREFIX));
        assert!(prompt.contains("what you changed"));
        assert!(prompt.contains("have not run a visible test"));
    }
}
