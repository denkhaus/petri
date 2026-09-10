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
    (@agent acp) => { Agent::Acp };
    (@agent none) => { Agent::None };
    (@agent_name openai) => { "openai" };
    (@agent_name anthropic) => { "anthropic" };
    (@agent_name openrouter) => { "openrouter" };
    (@agent_name acp) => { "acp" };
    (@agent_name none) => { "none" };
}

// ── ACP agent backend ───────────────────────────────────────────────────────

cell!(acp_turn_and_directive, "acp/turn-and-directive", host, acp);
cell!(
    acp_turn_and_directive_docker,
    "acp/turn-and-directive",
    docker,
    acp
);
cell!(
    acp_exit_before_answer_is_retried,
    "acp/exit-before-answer-is-retried",
    host,
    acp
);
cell!(
    acp_permission_request_and_hook,
    "acp/permission-request-and-hook",
    host,
    acp
);
cell!(acp_cancel_during_turn, "acp/cancel-during-turn", host, acp);

// ── Code review ─────────────────────────────────────────────────────────────

cell!(
    code_review_findings_in_multiple_branches,
    "code-review/findings-in-multiple-branches",
    host,
    openrouter
);
cell!(
    code_review_findings_in_multiple_branches_docker,
    "code-review/findings-in-multiple-branches",
    docker,
    openrouter
);

cell!(
    code_review_empty_diff,
    "code-review/empty-diff",
    host,
    openrouter
);
cell!(
    code_review_no_surviving_findings,
    "code-review/no-surviving-findings",
    host,
    openrouter
);
cell!(
    code_review_invalid_output_repaired,
    "code-review/invalid-output-repaired",
    host,
    openrouter
);
cell!(
    code_review_repair_exhausted,
    "code-review/repair-exhausted",
    host,
    openrouter
);
cell!(
    code_review_one_failed_branch,
    "code-review/one-failed-branch",
    host,
    openrouter
);
cell!(
    code_review_reverse_branch_completion,
    "code-review/reverse-branch-completion",
    host,
    openrouter
);
cell!(
    code_review_multi_level_fan_out,
    "code-review/multi-level-fan-out",
    host,
    openrouter
);

// ── Security review ─────────────────────────────────────────────────────────

cell!(
    security_review_no_vulnerabilities,
    "security-review/no-vulnerabilities",
    host,
    openrouter
);
cell!(
    security_review_several_verified,
    "security-review/several-verified",
    host,
    openrouter
);
cell!(
    security_review_rejected_candidate,
    "security-review/rejected-candidate",
    host,
    openrouter
);
cell!(
    security_review_partial_branch_failure,
    "security-review/partial-branch-failure",
    host,
    openrouter
);
cell!(
    security_review_malformed_response,
    "security-review/malformed-response",
    host,
    openrouter
);
cell!(
    security_review_timeout_cancel,
    "security-review/timeout-cancel",
    host,
    openrouter
);

// ── Cross-cutting provider faults ───────────────────────────────────────────

cell!(
    provider_faults_auth_failure,
    "provider-faults/auth-failure",
    host,
    anthropic
);
cell!(
    provider_faults_rate_limit_then_recovery,
    "provider-faults/rate-limit-then-recovery",
    host,
    anthropic
);
cell!(
    provider_faults_exhausted_retries,
    "provider-faults/exhausted-retries",
    host,
    anthropic
);
cell!(
    provider_faults_hanging_response,
    "provider-faults/hanging-response",
    host,
    anthropic
);
cell!(
    provider_faults_truncated_stream,
    "provider-faults/truncated-stream",
    host,
    anthropic
);
cell!(
    provider_faults_tool_call_continuation,
    "provider-faults/tool-call-continuation",
    host,
    anthropic
);

// ── Routing and configuration ───────────────────────────────────────────────

cell!(
    routing_bundle_defaults_and_overrides,
    "routing/bundle-defaults-and-overrides",
    host,
    openrouter
);
cell!(
    routing_goal_gate_restart_and_visit_limit,
    "routing/goal-gate-restart-and-visit-limit",
    host,
    none
);
cell!(routing_failure_policy, "routing/failure-policy", host, none);

// ── Backend matrix ──────────────────────────────────────────────────────────

cell!(
    backend_file_and_tool_execution,
    "backend/file-and-tool-execution",
    host,
    openai
);
cell!(
    backend_file_and_tool_execution_docker,
    "backend/file-and-tool-execution",
    docker,
    openai
);
cell!(
    backend_dynamic_parallelism,
    "backend/dynamic-parallelism",
    host,
    openai
);
cell!(
    backend_dynamic_parallelism_docker,
    "backend/dynamic-parallelism",
    docker,
    openai
);
cell!(
    backend_nested_workflow_docker,
    "interview/child-interviews",
    docker,
    openai
);
cell!(
    backend_cancellation_docker,
    "provider-faults/hanging-response",
    docker,
    anthropic
);

// ── Implement issue and plan ────────────────────────────────────────────────

cell!(
    implement_child_runs_successfully,
    "implement/child-runs-successfully",
    host,
    openrouter
);
cell!(
    implement_child_runs_successfully_docker,
    "implement/child-runs-successfully",
    docker,
    openrouter
);
cell!(
    implement_input_model_inheritance,
    "implement/input-model-inheritance",
    host,
    openrouter
);
cell!(
    implement_multiple_manager_cycles,
    "implement/multiple-manager-cycles",
    host,
    openrouter
);
cell!(
    implement_stop_condition,
    "implement/stop-condition",
    host,
    openrouter
);
cell!(
    implement_child_failure,
    "implement/child-failure",
    host,
    openrouter
);
cell!(
    implement_parent_cancellation,
    "implement/parent-cancellation",
    host,
    openrouter
);

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
        // rest of the matrix. A cell another suite satisfies (`test` names
        // `<package>::<binary>::<test>`, the differential matrix's cells)
        // names the scenario that suite defines, not a `family/name` file.
        assert!(
            external_suite(cell) || id.split('/').count() == 2,
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
    // Every planned cell names a scenario file that exists, so a coverage
    // report can only be missing a cell because its test did not run.
    for cell in &matrix.cells {
        if cell.status != CellStatus::Planned {
            continue;
        }
        let Some(id) = &cell.scenario else {
            continue;
        };
        if external_suite(cell) {
            continue;
        }
        assert!(
            scenarios.iter().any(|(_, scenario)| &scenario.id == id),
            "planned cell `{}` names no scenario file",
            cell.name
        );
    }
}

/// A cell whose test lives in another suite: `test` is
/// `<package>::<binary>::<test>`, and the scenario is that suite's.
fn external_suite(cell: &support::fabro::scenario::Cell) -> bool {
    cell.test.as_deref().is_some_and(|test| test.contains("::"))
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
