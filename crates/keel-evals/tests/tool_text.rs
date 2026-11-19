//! Keeps the eval cassettes' request halves in sync with the built-in tools
//! and the deterministic prompt cache key.
//!
//! An eval cassette stores the whole live exchange, including the tools keel
//! advertised at the time. A tool-description change or a cache-policy change
//! therefore has to refresh the recorded request bodies, or the next replay is
//! a cassette miss. The response halves are never touched.
//!
//! Run with `UPDATE_EVAL_CASSETTE=1 cargo test -p keel-evals --test tool_text`
//! after an intentional tool-text or cache-key change, then review the diff.
//! Without the variable the test asserts that every recorded request already
//! carries the current specs and key.
//!
//! The result-text migration below covers the two historical texts that M2
//! changed. A future tool-result wording change needs its own migration here;
//! otherwise the replay miss is the signal that the recorded transcript must be
//! migrated (or re-recorded live).

use keel_evals::TASKS;
use keel_evals::fixture::cassettes_dir;
use keel_llm_types::{ProtocolId, ToolSpec, prompt_cache_key};
use keel_testkit::Cassette;
use keel_tools::builtin;
use serde_json::{Value, json};

const BASH_FAILED_HINT: &str =
    "If this exit was not expected, read stderr and fix the command or the code, then retry";
const PREVIOUS_BASH_FAILED_HINT: &str =
    "command failed; read stderr and fix the command or the code, then retry";

#[test]
fn eval_cassettes_match_the_current_request_prefix() {
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
            // Re-deriving the key here only keeps a regenerated cassette in
            // step with the encoder. The replay gate compares the whole body
            // against this cassette, and that comparison is the real check: a
            // wrong key cannot pass it just because this helper agreed.
            if refresh_prompt_cache_key(&mut body) {
                body_changed = true;
            }
            if !body_changed {
                continue;
            }

            let rewritten = serde_json::to_string(&body).expect("the body serializes");
            assert!(
                set_content_length(&mut interaction.request.headers, rewritten.len()),
                "{}: interaction {index} has no content-length header",
                path.display()
            );
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
                "{}: recorded request prefix is stale; run \
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

/// Keeps the recorded request in sync with the deterministic cache key the
/// encoder derives from its prefix. The eval cassettes are OpenAI Chat, so the
/// system text is the first recorded message when it has the system role.
fn refresh_prompt_cache_key(body: &mut Value) -> bool {
    let Some(model) = body.get("model").and_then(Value::as_str) else {
        return false;
    };
    let system = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.first())
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str);
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let function = item.get("function")?;
                    Some(ToolSpec {
                        name: function.get("name")?.as_str()?.to_owned(),
                        description: function
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        input_schema: function.get("parameters").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let key = prompt_cache_key(ProtocolId::OpenAiChat, model, system, &tools);
    if body.get("prompt_cache_key").and_then(Value::as_str) == Some(key.as_str()) {
        return false;
    }
    body["prompt_cache_key"] = Value::String(key);
    true
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
    if content.starts_with("ERROR: exit: ") {
        let mut base = content;
        loop {
            if let Some(stripped) = base.strip_suffix(&format!("\n{BASH_FAILED_HINT}")) {
                base = stripped;
            } else if let Some(stripped) =
                base.strip_suffix(&format!("\n{PREVIOUS_BASH_FAILED_HINT}"))
            {
                base = stripped;
            } else {
                break;
            }
        }
        let updated = format!("{base}\n{BASH_FAILED_HINT}");
        return (updated != content).then_some(updated);
    }
    None
}

fn set_content_length(headers: &mut [(String, String)], length: usize) -> bool {
    let mut found = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-length") {
            *value = length.to_string();
            found = true;
        }
    }
    found
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

    let migrated = format!("{old}\n{PREVIOUS_BASH_FAILED_HINT}");
    let new = refreshed_result_text(&migrated).expect("the previous hint is replaced");
    assert_eq!(new, format!("{old}\n{BASH_FAILED_HINT}"));
}

#[test]
fn a_missing_content_length_is_reported() {
    let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
    assert!(!set_content_length(&mut headers, 1));

    let mut headers = vec![("content-length".to_owned(), "0".to_owned())];
    assert!(set_content_length(&mut headers, 12));
    assert_eq!(headers[0].1, "12");
}
