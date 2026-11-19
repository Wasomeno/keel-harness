//! The budget gate and the live-mode opt-in.

use futures_util::future::BoxFuture;
use keel_evals::live::live_allowed;
use keel_evals::{
    Budget, CheckOutcome, Mode, Prices, SuiteExecutor, TASKS, Task, TaskReport, TaskStatus,
    run_suite,
};
use keel_llm_types::Usage;

/// A fake cost source: $1 per million tokens, so one token costs one
/// micro-USD. It lets the test exercise the runner's stop-before-crossing rule
/// without a network or a real provider.
const FAKE_PRICES: Prices = Prices {
    input_micros_per_million: 1_000_000,
    cached_input_micros_per_million: 1_000_000,
    output_micros_per_million: 1_000_000,
};

struct FakeExecutor {
    requests: usize,
    per_request: (u64, u64),
}

impl SuiteExecutor for FakeExecutor {
    fn run_task<'a>(
        &'a mut self,
        task: &'a Task,
        budget: &'a mut Budget,
    ) -> BoxFuture<'a, TaskReport> {
        Box::pin(async move {
            for _ in 0..2 {
                let (input, output) = self.per_request;
                if !budget.allows(input, output) {
                    return TaskReport {
                        id: task.id.to_owned(),
                        status: TaskStatus::StoppedBudget,
                        turns: 1,
                        tool_calls: 0,
                        usage: Usage::default(),
                        stop: None,
                        check: CheckOutcome {
                            passed: false,
                            detail: "not run (budget)".to_owned(),
                        },
                        spend_micros: Some(0),
                        cassette: None,
                        error: None,
                    };
                }
                self.requests += 1;
                budget.charge_usage(&Usage {
                    input_tokens: input,
                    output_tokens: output,
                    ..Usage::default()
                });
            }

            TaskReport {
                id: task.id.to_owned(),
                status: TaskStatus::Passed,
                turns: 2,
                tool_calls: 1,
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 10,
                    ..Usage::default()
                },
                stop: None,
                check: CheckOutcome {
                    passed: true,
                    detail: "pass".to_owned(),
                },
                spend_micros: Some(30),
                cassette: None,
                error: None,
            }
        })
    }
}

#[tokio::test]
async fn the_runner_stops_before_the_request_that_would_cross_the_cap() {
    // 10 input + 5 output = 15 micro-USD per request. Cap 20 lets the first
    // request run and refuses the second (15 + 15 > 20).
    let mut budget = Budget::new(FAKE_PRICES, 20);
    let mut executor = FakeExecutor {
        requests: 0,
        per_request: (10, 5),
    };

    let report = run_suite(&TASKS[..2], &mut executor, &mut budget, Mode::Live).await;

    assert_eq!(executor.requests, 1, "the second request never ran");
    assert_eq!(budget.spent_micros(), 15);
    assert_eq!(report.tasks[0].status, TaskStatus::StoppedBudget);
    assert_eq!(report.tasks[1].status, TaskStatus::NotRunBudget);
    assert_eq!(report.count(TaskStatus::NotRunBudget), 1);
    assert_eq!(report.spent_micros, Some(15));

    let markdown = report.to_markdown();
    assert!(markdown.contains("stopped (budget)"), "{markdown}");
    assert!(markdown.contains("not run (budget)"), "{markdown}");
}

#[test]
fn live_mode_needs_its_opt_in_and_refuses_ci() {
    assert!(live_allowed(false, false).is_err(), "no opt-in");
    assert!(live_allowed(true, true).is_err(), "CI refuses recording");
    assert!(
        live_allowed(false, true).is_err(),
        "CI refuses without opt-in"
    );
    assert!(live_allowed(true, false).is_ok());
}
