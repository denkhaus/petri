//! Black box scenarios for Fabro workflows on the shipped `petri` binary.
//!
//! The first scenario is the parallel-result regression from the readiness
//! assessment: two command branches write distinct findings under the same
//! context key, a fan-in joins them, and the pinned Conveyor `code_review.py`
//! merges them. Petri `9cea20d` emits `{id, status, output}` per branch, so the
//! helper sees no `context_updates` and reports zero findings.
//!
//! Two tests pin today's loss (`current_*`); they pass now and must be deleted
//! by task 6. Two tests state the contract in
//! `crates/fabro/acceptance/scenarios/parallel-results/CONTRACT.md`
//! (`contract_*`); they are `#[should_panic]` on the exact contract marker, so
//! they turn red the moment the fix lands and task 6 removes the attribute.
//! An infrastructure failure panics with a different message and fails either
//! way.

mod support;

use std::fs;

use serde_json::{Value, json};
use support::fabro::{BranchEnvelope, Petri, RunObservation, RunOutput, Scenario};

const SCENARIO: &str = "parallel-results";
const FINDERS: [&str; 2] = ["finder_a", "finder_b"];
const CONTRACT: &str = "parallel result contract (task 6):";

const FINDING_A: &str = "page_count drops the final partial page";
const FINDING_B: &str = "render escapes the title twice";

/// The report the pinned helper writes when both findings survive, as Fabro
/// produced it (`fabro-reference/raw/report.md`).
fn expected_report() -> String {
    fs::read_to_string(Scenario::source_file(
        SCENARIO,
        "fabro-reference/raw/report.md",
    ))
    .expect("the Fabro reference report is tracked")
}

/// The branch envelopes Fabro produced for the finder fan-out, from the
/// normalized capture, without `command.output` (Petri's command output ends
/// with a newline; the value is compared on its own).
fn fabro_finder_envelopes() -> Vec<BranchEnvelope> {
    let text = fs::read_to_string(Scenario::source_file(
        SCENARIO,
        "fabro-reference/normalized.json",
    ))
    .expect("the normalized Fabro capture is tracked");
    let capture: Value = serde_json::from_str(&text).expect("normalized.json is JSON");
    let group = capture["parallel_groups"]
        .as_array()
        .and_then(|groups| groups.iter().find(|g| g["node"] == "find"))
        .expect("the capture has the `find` group");
    group["results_from_dump"]
        .as_array()
        .expect("the group has dumped results")
        .iter()
        .map(|value| {
            let mut value = value.clone();
            if let Some(updates) = value["context_updates"].as_object_mut() {
                updates.remove("command.output");
            }
            BranchEnvelope::from_value(&value)
        })
        .collect()
}

fn run_scenario(label: &str) -> (Scenario, RunOutput) {
    let scenario = Scenario::stage(SCENARIO);
    let helper = scenario.file("helper/code_review.py");
    let output = Petri::run_workflow(&scenario.file("workflow.fabro"), &scenario.run_dir(label))
        .input("helper", helper.to_string_lossy())
        .input("level", "high")
        .input("target", "review-fixture")
        .run();
    assert!(
        output.success(),
        "the scenario runs to completion today:\n{}",
        output.stderr()
    );
    assert_eq!(output.run_status().as_deref(), Some("success"));
    (scenario, output)
}

fn finder_envelopes(observation: &RunObservation) -> Vec<BranchEnvelope> {
    observation
        .fan_in_output(&FINDERS)
        .expect("the finder fan-in produced one envelope per branch, in branch order")
}

/// Strip `command.output` so an envelope compares on the finding it carries.
fn without_command_output(mut envelope: BranchEnvelope) -> BranchEnvelope {
    if let Some(updates) = envelope
        .context_updates
        .as_mut()
        .and_then(Value::as_object_mut)
    {
        updates.remove("command.output");
    }
    envelope
}

#[test]
fn current_branch_envelopes_keep_both_branches_but_drop_their_context() {
    let (_scenario, output) = run_scenario("current-envelopes");
    let observation = RunObservation::load(&output.run_dir);
    let envelopes = finder_envelopes(&observation);

    // Both branches are preserved, in edge order, and each branch's own
    // output still holds its finding.
    assert_eq!(envelopes.len(), 2);
    assert!(
        envelopes[0]
            .output
            .as_ref()
            .is_some_and(|o| o["stdout"].as_str().is_some_and(|s| s.contains(FINDING_A))),
        "{:?}",
        envelopes[0]
    );
    assert!(
        envelopes[1]
            .output
            .as_ref()
            .is_some_and(|o| o["stdout"].as_str().is_some_and(|s| s.contains(FINDING_B))),
        "{:?}",
        envelopes[1]
    );
    // Today's shape: Petri's status tag, no index, no context_updates. Task 6
    // deletes this test when it makes the contract tests pass.
    for envelope in &envelopes {
        assert_eq!(envelope.status.as_deref(), Some("success"));
        assert_eq!(envelope.index, None);
        assert_eq!(envelope.context_updates, None);
    }
}

#[test]
fn current_helper_sees_no_findings() {
    let (_scenario, output) = run_scenario("current-helper");
    let observation = RunObservation::load(&output.run_dir);

    assert!(
        output
            .echoed("merge_find")
            .contains("Pooled 0 candidates into 0 locations"),
        "{}",
        output.stderr()
    );
    let context = observation.final_context();
    assert_eq!(context["candidate_count"], json!(0));
    assert_eq!(context["run_verify"], json!(false));
    assert_eq!(context["reported"], json!(0));
    let report = output.echoed("report");
    assert!(report.contains("## Findings (0)"), "{report}");
    assert!(
        !report.contains(FINDING_A) && !report.contains(FINDING_B),
        "{report}"
    );
}

#[test]
#[should_panic(expected = "parallel result contract (task 6):")]
fn contract_branch_envelopes_carry_index_status_and_context_updates() {
    let (_scenario, output) = run_scenario("contract-envelopes");
    let observation = RunObservation::load(&output.run_dir);
    let envelopes = finder_envelopes(&observation);
    assert_eq!(envelopes.len(), 2, "{CONTRACT} two branches, two envelopes");

    let expected = fabro_finder_envelopes();
    for (position, (actual, fabro)) in envelopes.iter().zip(&expected).enumerate() {
        assert_eq!(actual.id, fabro.id, "{CONTRACT} id at {position}");
        assert_eq!(
            actual.index,
            Some(position as u64),
            "{CONTRACT} index is the edge position at {position}: {actual:?}"
        );
        assert_eq!(
            actual.item_label, None,
            "{CONTRACT} static branches have no item_label: {actual:?}"
        );
        assert_eq!(
            actual.status.as_deref(),
            Some("succeeded"),
            "{CONTRACT} status uses Fabro's vocabulary at {position}: {actual:?}"
        );
        let updates = actual.context_updates.clone().unwrap_or_else(|| {
            panic!("{CONTRACT} context_updates is present at {position}: {actual:?}")
        });
        assert_eq!(
            updates["output.finder"],
            fabro.context_updates.as_ref().expect("fabro has updates")["output.finder"],
            "{CONTRACT} each branch keeps its own output.finder at {position}"
        );
        assert_eq!(
            without_command_output(actual.clone()),
            *fabro,
            "{CONTRACT} envelope {position} matches the Fabro capture"
        );
    }
    // The parent context does not absorb branch-local keys.
    let context = observation.final_context();
    assert!(
        context.get("output.finder").is_none(),
        "{CONTRACT} output.finder stays branch-local: {context}"
    );
}

#[test]
#[should_panic(expected = "parallel result contract (task 6):")]
fn contract_helper_merges_both_findings_into_the_report() {
    let (_scenario, output) = run_scenario("contract-report");
    let observation = RunObservation::load(&output.run_dir);

    let merge_find = output.echoed("merge_find");
    assert!(
        merge_find.contains("Pooled 2 candidates into 2 locations"),
        "{CONTRACT} merge_find pools both candidates:\n{merge_find}"
    );
    let context = observation.final_context();
    assert_eq!(
        context["candidate_count"],
        json!(2),
        "{CONTRACT} candidate_count"
    );
    assert_eq!(context["run_verify"], json!(true), "{CONTRACT} run_verify");
    assert_eq!(
        context["verified_count"],
        json!(2),
        "{CONTRACT} verified_count"
    );
    assert_eq!(context["reported"], json!(2), "{CONTRACT} reported");
    assert_eq!(
        context["parallel.branch_count"],
        json!(2),
        "{CONTRACT} parallel.branch_count is published"
    );
    assert_eq!(
        output.echoed("report"),
        expected_report(),
        "{CONTRACT} the report is byte-identical to Fabro's"
    );
    assert_eq!(observation.final_status(), Some("Success"));
    let history: Vec<String> = output
        .node_history()
        .into_iter()
        .map(|(_, node)| node)
        .collect();
    for node in [
        "verify",
        "verifier_a",
        "verifier_b",
        "verify_join",
        "merge_verify",
    ] {
        assert!(
            history.iter().any(|n| n == node),
            "{CONTRACT} {node} ran: {history:?}"
        );
    }
}
