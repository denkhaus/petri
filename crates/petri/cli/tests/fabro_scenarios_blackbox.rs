//! The complete-workflow scenarios of the Fabro black box battery (phase 4):
//! every required cell of `crates/fabro/acceptance/scenarios/matrix.json`,
//! each a pinned bundle run through the shipped `petri` binary from a
//! scenario file (`SCHEMA.md` there) with provider twins, a fixture
//! repository with real local Git remotes, a scripted interviewer, and the
//! observations the file declared before the run.

mod support;

use support::fabro::runner::run_cell;
use support::fabro::scenario::{Agent, Backend, CellStatus, Matrix, Scenario, matches};

/// One matrix cell as one test: `cell!(test_name, "family/name", host,
/// openai)`.
macro_rules! cell {
    ($name:ident, $id:literal, $backend:ident, $agent:ident) => {
        #[tokio::test]
        async fn $name() {
            let backend = cell!(@backend $backend);
            let agent = cell!(@agent $agent);
            let cell = format!(
                "{}@{}/{}",
                $id,
                cell!(@backend_name $backend),
                cell!(@agent_name $agent)
            );
            run_cell(&cell, $id, backend, agent).await;
        }
    };
    (@backend host) => { Backend::Host };
    (@backend docker) => { Backend::Docker };
    (@backend_name host) => { "host" };
    (@backend_name docker) => { "docker" };
    (@agent openai) => { Agent::OpenAi };
    (@agent anthropic) => { Agent::Anthropic };
    (@agent openrouter) => { Agent::OpenRouter };
    (@agent none) => { Agent::None };
    (@agent_name openai) => { "openai" };
    (@agent_name anthropic) => { "anthropic" };
    (@agent_name openrouter) => { "openrouter" };
    (@agent_name none) => { "none" };
}

// ── Interview gates ─────────────────────────────────────────────────────────

cell!(
    interview_scripted_choice_refusal_freeform,
    "interview/scripted-choice-refusal-freeform",
    host,
    openai
);

cell!(
    interview_repeated_and_concurrent_questions,
    "interview/repeated-and-concurrent-questions",
    host,
    none
);
cell!(
    interview_child_interviews,
    "interview/child-interviews",
    host,
    openai
);
cell!(
    interview_delayed_withheld_reply,
    "interview/delayed-withheld-reply",
    host,
    openai
);
cell!(
    interview_invalid_answer_reasked,
    "interview/invalid-answer-reasked",
    host,
    openai
);
cell!(
    interview_unexpected_question,
    "interview/unexpected-question",
    host,
    openai
);
cell!(
    interview_unused_answer,
    "interview/unused-answer",
    host,
    openai
);
cell!(
    interview_timeout_cancel,
    "interview/timeout-cancel",
    host,
    openai
);
cell!(interview_gate_timeout, "interview/gate-timeout", host, none);
cell!(
    interview_terminal_eof,
    "interview/terminal-eof",
    host,
    openai
);

/// Every tracked scenario file loads under the strict schema, its bundle
/// hash equals the lock, and every planned matrix cell names a scenario
/// that exists. A scenario that fails here never runs, so a typo in an
/// assertion cannot pass silently.
#[test]
fn every_scenario_file_loads_and_the_matrix_is_consistent() {
    let scenarios = Scenario::all().unwrap_or_else(|error| panic!("{error}"));
    assert!(!scenarios.is_empty(), "no scenario files under scenarios/");
    let matrix = Matrix::load().unwrap_or_else(|error| panic!("{error}"));
    for cell in &matrix.cells {
        let Some(id) = &cell.scenario else {
            assert!(
                cell.reason.is_some(),
                "cell `{}` has no scenario and no reason",
                cell.name
            );
            continue;
        };
        // A planned cell whose scenario file does not exist yet is not an
        // error here: the coverage report lists it as `missing`, which
        // counts as failed, so the gap stays visible without hiding the
        // rest of the matrix.
        assert!(
            id.split('/').count() == 2,
            "cell `{}` names a malformed scenario id `{id}`",
            cell.name
        );
        if cell.status == CellStatus::Planned {
            assert!(
                cell.test.is_some(),
                "planned cell `{}` names no test",
                cell.name
            );
        }
        let backend = match cell.backend {
            support::fabro::scenario::Backend::Host => "host",
            support::fabro::scenario::Backend::Docker => "docker",
        };
        assert!(
            cell.name.contains(&format!("@{backend}/")),
            "cell `{}` does not name its backend",
            cell.name
        );
    }
    for (path, scenario) in &scenarios {
        let listed = matrix
            .cells
            .iter()
            .any(|cell| cell.scenario.as_deref() == Some(scenario.id.as_str()));
        assert!(listed, "{} is not in matrix.json", path.display());
    }
}

/// The matchers `SCHEMA.md` documents behave as documented.
#[test]
fn matchers_compare_as_documented() {
    use serde_json::json;
    assert!(matches(&json!({"$any": true}), Some(&json!(null))).is_ok());
    assert!(matches(&json!({"$any": true}), None).is_err());
    assert!(
        matches(
            &json!({"$regex": "^CODE-REVIEW-\\d{8}-\\d{6}(-\\d+)?$"}),
            Some(&json!("CODE-REVIEW-20260907-051200"))
        )
        .is_ok()
    );
    assert!(matches(&json!({"$regex": "^a$"}), Some(&json!("b"))).is_err());
    assert!(matches(&json!({"$contains": "b"}), Some(&json!("abc"))).is_ok());
    assert!(matches(&json!({"$contains": 2}), Some(&json!([1, 2]))).is_ok());
    assert!(matches(&json!({"$len": 2}), Some(&json!([1, 2]))).is_ok());
    assert!(matches(&json!({"$set": ["b", "a"]}), Some(&json!(["a", "b"]))).is_ok());
    assert!(matches(&json!({"$set": ["a"]}), Some(&json!(["a", "b"]))).is_err());
    assert!(
        matches(
            &json!({"$subset": {"a": 1}}),
            Some(&json!({"a": 1, "b": 2}))
        )
        .is_ok()
    );
    assert!(matches(&json!({"a": 1}), Some(&json!({"a": 1, "b": 2}))).is_err());
    assert!(matches(&json!({"$type": "number"}), Some(&json!(1))).is_ok());
    assert!(matches(&json!([1, {"$any": true}]), Some(&json!([1, "x"]))).is_ok());
    assert!(
        matches(&json!("1"), Some(&json!(1))).is_err(),
        "strings and numbers differ"
    );
    assert!(
        matches(&json!(null), None).is_err(),
        "missing and null differ"
    );
}
