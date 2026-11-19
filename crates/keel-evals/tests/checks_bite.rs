//! Proof that every mechanical check fails on the untouched fixture.

use std::collections::BTreeSet;
use std::process::Command;

use keel_evals::{Fixture, TASKS, run_checks};

#[test]
fn every_check_fails_on_the_untouched_fixture() {
    for task in TASKS {
        let fixture = Fixture::prepare(task).expect("fixture copies");
        let outcome = run_checks(task, fixture.root());
        assert!(
            !outcome.passed,
            "the check for {:?} passed on the untouched fixture: {}",
            task.id, outcome.detail
        );

        // The agent runs the same script from the workspace. It must also fail
        // there, not only in the grader's copy.
        let output = Command::new("python3")
            .arg("tests/check.py")
            .current_dir(fixture.root())
            .output()
            .expect("python3 runs the workspace check");
        assert!(
            !output.status.success(),
            "tests/check.py for {:?} passed on the untouched fixture",
            task.id
        );
    }
}

#[test]
fn task_ids_cassettes_and_check_scripts_are_unique() {
    let mut ids = BTreeSet::new();
    let mut cassettes = BTreeSet::new();
    for task in TASKS {
        assert!(ids.insert(task.id), "duplicate task id {:?}", task.id);
        assert!(
            cassettes.insert(task.cassette),
            "duplicate cassette {:?}",
            task.cassette
        );
        assert_eq!(
            task.cassette,
            format!("evals/{}", task.id),
            "cassette names follow the task id"
        );
        assert!(
            keel_evals::fixture::checks_dir()
                .join(task.check_script)
                .is_file(),
            "missing check script for {:?}",
            task.id
        );
    }
    assert_eq!(TASKS.len(), 12, "the baseline is 12 tasks");
}
