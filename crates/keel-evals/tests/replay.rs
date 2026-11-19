//! Offline replay for every task, plus the cassette manifest gate.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use keel_evals::fixture::cassettes_dir;
use keel_evals::replay::replay_task;
use keel_evals::{TASKS, TaskStatus, task_by_id};
use keel_testkit::Cassette;

#[derive(Debug, serde::Deserialize)]
struct Manifest {
    scenario: Vec<Scenario>,
}

#[derive(Debug, serde::Deserialize)]
struct Scenario {
    task: String,
    file: String,
    protocol: String,
    provider: String,
    model: String,
    covers: String,
}

fn manifest() -> Manifest {
    let path = cassettes_dir().join("scenarios.toml");
    toml::from_str(&fs::read_to_string(&path).expect("the eval scenario manifest exists"))
        .expect("the eval scenario manifest parses")
}

#[test]
fn every_eval_cassette_is_covered_by_the_manifest() {
    let manifest = manifest();
    assert!(!manifest.scenario.is_empty(), "the manifest is empty");

    let mut listed = BTreeSet::new();
    for scenario in &manifest.scenario {
        let task =
            task_by_id(&scenario.task).unwrap_or_else(|| panic!("{}: no such task", scenario.task));
        assert!(
            listed.insert(scenario.file.clone()),
            "{} is listed twice",
            scenario.file
        );
        assert!(
            !scenario.covers.is_empty(),
            "{} has no description",
            scenario.task
        );
        assert_eq!(
            task.cassette,
            format!("evals/{}", scenario.task),
            "{}: cassette name follows the task id",
            scenario.task
        );

        let path = cassettes_dir().join(&scenario.file);
        let cassette =
            Cassette::load(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(cassette.name, task.cassette);
        assert_eq!(cassette.protocol, scenario.protocol);
        assert_eq!(cassette.provider, scenario.provider);
        assert_eq!(cassette.model, scenario.model);
        assert!(
            !cassette.source.is_empty(),
            "{} has no provenance",
            scenario.task
        );
        assert!(
            !cassette.interactions.is_empty(),
            "{} has no interactions",
            scenario.task
        );

        for interaction in &cassette.interactions {
            assert!(
                serde_json::from_str::<serde_json::Value>(&interaction.request.body).is_ok(),
                "{}: a request body is not JSON",
                scenario.task
            );
            assert!(
                !interaction.response.body.is_empty(),
                "{}: an interaction has an empty response",
                scenario.task
            );
            for (name, value) in &interaction.request.headers {
                if is_sensitive(name) {
                    assert_eq!(
                        value, "REDACTED",
                        "{}: {name} is not redacted",
                        scenario.task
                    );
                }
            }
        }
    }

    for path in json_files(&cassettes_dir().join("evals")) {
        let relative = path
            .strip_prefix(cassettes_dir())
            .expect("inside the cassette tree")
            .to_string_lossy()
            .replace('\\', "/");
        assert!(
            listed.contains(&relative),
            "{relative} is not in scenarios.toml"
        );
    }

    for task in TASKS {
        let file = task.cassette_file();
        assert!(
            manifest
                .scenario
                .iter()
                .any(|scenario| scenario.file == file),
            "task {:?} has no manifest scenario",
            task.id
        );
    }
}

fn is_sensitive(name: &str) -> bool {
    matches!(
        name,
        "authorization" | "proxy-authorization" | "x-api-key" | "api-key" | "cookie" | "set-cookie"
    )
}

fn json_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                found.push(path);
            }
        }
    }
    found
}

async fn assert_replays(id: &str) {
    let task = task_by_id(id).unwrap_or_else(|| panic!("{id}: no such task"));
    let report = replay_task(task)
        .await
        .unwrap_or_else(|error| panic!("{id}: {error}"));
    assert_eq!(
        report.status,
        TaskStatus::Passed,
        "{id}: {}",
        report.detail()
    );
}

macro_rules! replay_tests {
    ($($name:ident => $id:literal),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() {
                assert_replays($id).await;
            }
        )*
    };
}

replay_tests!(
    replays_truncate => "truncate",
    replays_slugify => "slugify",
    replays_parse_int => "parse-int",
    replays_mean => "mean",
    replays_clamp => "clamp",
    replays_unique => "unique",
    replays_chunk => "chunk",
    replays_format_table => "format-table",
    replays_refactor_whitespace => "refactor-whitespace",
    replays_rename_format_row => "rename-format-row",
    replays_readme_slugify => "readme-slugify",
    replays_package_exports => "package-exports",
);
