//! Keeps the eval cassettes' request halves in sync with the built-in tools.
//!
//! An eval cassette stores the whole live exchange, including the tools keel
//! advertised at the time. A tool-description change therefore has to refresh
//! the recorded request bodies, or the next replay is a cassette miss. The
//! response halves are never touched.
//!
//! Run with `UPDATE_EVAL_CASSETTE=1 cargo test -p keel-evals --test tool_text`
//! after an intentional tool-text change, then review the diff. Without the
//! variable the test asserts that every recorded request already advertises the
//! current specs.

use keel_evals::TASKS;
use keel_evals::fixture::cassettes_dir;
use keel_testkit::Cassette;
use keel_tools::builtin;
use serde_json::{Value, json};

const BASH_FAILED_HINT: &str =
    "command failed; read stderr and fix the command or the code, then retry";

#[test]
fn eval_cassettes_advertise_the_current_tool_specs() {
    let update = std::env::var_os("UPDATE_EVAL_CASSETTE").is_some();
    let tools = current_tools();

    for task in TASKS {
        let path = cassettes_dir().join(task.cassette_file());
        let mut cassette =
            Cassette::load(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let mut changed = false;

        for (index, interaction) in cassette.interactions.iter_mut().enumerate() {
            let mut body: Value = serde_json::from_str(&interaction.request.body)
                .unwrap_or_else(|error| panic!("{}: interaction {index}: {error}", path.display()));
            assert!(
                body.get("tools").is_some_and(Value::is_array),
                "{}: interaction {index} has no tools array",
                path.display()
            );

            let mut body_changed = false;
            if body["tools"] != tools {
                body["tools"] = tools.clone();
                body_changed = true;
            }
            if refresh_recorded_results(&mut body) {
                body_changed = true;
            }
            if !body_changed {
                continue;
            }

            let rewritten = serde_json::to_string(&body).expect("the body serializes");
            set_content_length(&mut interaction.request.headers, rewritten.len());
            interaction.request.body = rewritten;
            changed = true;
        }

        if update {
            if changed {
                cassette.save(&path).expect("write the refreshed cassette");
            }
        } else {
            assert!(
                !changed,
                "{}: recorded tool text is stale; run \
                 UPDATE_EVAL_CASSETTE=1 cargo test -p keel-evals --test tool_text",
                path.display()
            );
        }
    }
}

fn current_tools() -> Value {
    let specs = builtin::registry().specs();
    Value::Array(
        specs
            .into_iter()
            .map(|spec| {
                json!({
                    "type": "function",
                    "function": {
                        "name": spec.name,
                        "description": spec.description,
                        "parameters": spec.input_schema,
                    }
                })
            })
            .collect(),
    )
}

/// Rewrites the two recorded results whose text changed with M2, so the
/// recorded transcript still matches what the tools produce now.
fn refresh_recorded_results(body: &mut Value) -> bool {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(content) = message.get("content").and_then(Value::as_str) else {
            continue;
        };
        let Some(updated) = refreshed_result_text(content) else {
            continue;
        };
        message["content"] = Value::String(updated);
        changed = true;
    }
    changed
}

fn refreshed_result_text(content: &str) -> Option<String> {
    if let Some(pattern) = content
        .strip_prefix("no matches for `")
        .and_then(|rest| rest.strip_suffix('`'))
    {
        return Some(format!(
            "no matches for `{pattern}`; check the pattern, `path`, and `glob` filter, \
             or use `glob` to find the file first"
        ));
    }
    if content.starts_with("ERROR: exit: ") && !content.contains(BASH_FAILED_HINT) {
        return Some(format!("{content}\n{BASH_FAILED_HINT}"));
    }
    None
}

fn set_content_length(headers: &mut [(String, String)], length: usize) {
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-length") {
            *value = length.to_string();
        }
    }
}

#[test]
fn the_recorded_result_rewrite_is_idempotent() {
    let old = "no matches for `format_row`";
    let new = refreshed_result_text(old).expect("the old no-match text is refreshed");
    assert!(new.starts_with("no matches for `format_row`;"));
    assert_eq!(refreshed_result_text(&new), None);

    let old = "ERROR: exit: 1\nstdout:\nFAIL";
    let new = refreshed_result_text(old).expect("the old failed-command text is refreshed");
    assert!(new.ends_with(BASH_FAILED_HINT));
    assert_eq!(refreshed_result_text(&new), None);
}
