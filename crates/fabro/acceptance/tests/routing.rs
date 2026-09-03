//! Acceptance §7 items 3 and 5, the run half: the shared scripted cases
//! drive every routing tier, failure policy, goal gate, fan-out and
//! `loop_restart` through a real run under stubs — and each result is
//! compared with the committed Fabro oracle fixture for the same case
//! (`crates/fabro/oracle/expected/<case>.json`), generated against the pinned
//! Fabro by `scripts/oracle-regenerate.sh`. A case whose fixture records a
//! deliberate departure is compared with the departure recorded.

use std::path::{Path, PathBuf};
use std::{env, fs};

use fabro_acceptance::runs::{Case, RunResult, result_json, run};
use serde_json::Value;

fn oracle_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../oracle")
}

fn cases() -> Vec<Case> {
    let mut paths: Vec<PathBuf> = fs::read_dir(oracle_dir().join("cases"))
        .expect("the cases directory")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    paths.iter().map(|p| Case::load(p)).collect()
}

async fn run_case(case: &Case) -> RunResult {
    run(case.graph(), &case.name).await
}

fn assert_path(result: &RunResult, status: &str, path: &[&str]) {
    assert_eq!(result.status, status, "{result:?}");
    assert_eq!(result.nodes(), path, "{result:?}");
}

async fn run_named(name: &str) -> RunResult {
    let case = Case::load(&oracle_dir().join("cases").join(format!("{name}.json")));
    run_case(&case).await
}

#[tokio::test]
async fn tier_one_conditions_pick_by_weight_then_lexical_target() {
    let r = run_named("tier1_condition_picks_highest_weight_then_lexical").await;
    assert_path(&r, "success", &["start", "a", "c", "exit"]);
    let r = run_named("tier1_condition_lexical_tiebreak").await;
    assert_path(&r, "success", &["start", "a", "alpha", "exit"]);
}

#[tokio::test]
async fn tier_two_preferred_label_beats_suggested_ids() {
    let r = run_named("tier2_preferred_label_beats_suggested_and_fallback").await;
    assert_path(&r, "success", &["start", "a", "fix", "exit"]);
}

#[tokio::test]
async fn tier_three_suggested_ids_in_order() {
    let r = run_named("tier3_suggested_ids_lowest_index_wins").await;
    assert_path(&r, "success", &["start", "a", "p2", "exit"]);
}

#[tokio::test]
async fn tier_four_fallback_follows_the_failure_policy() {
    let r = run_named("tier4_fallback_route_on_failure").await;
    assert_path(&r, "success", &["start", "a", "b", "exit"]);
    assert_eq!(r.path[1].outcome, "failed");
    let r = run_named("tier4_exit_policy_ends_the_run_failed").await;
    assert_path(&r, "failed", &["start", "a"]);
    let r = run_named("tier4_exit_policy_still_takes_an_explicit_failure_edge").await;
    assert_path(&r, "success", &["start", "a", "recover", "exit"]);
}

#[tokio::test]
async fn context_signals_route_conditionals() {
    let r = run_named("conditional_node_routes_on_previous_outcome_via_context").await;
    assert_path(&r, "success", &["start", "work", "gate", "exit"]);
    let r = run_named("custom_routing_signal_rides_context").await;
    assert_path(&r, "success", &["start", "a", "fast", "exit"]);
    assert_eq!(r.context.get("mode"), Some(&Value::String("fast".into())));
}

#[tokio::test]
async fn human_gates_route_on_the_answer_and_never_fall_through_on_failure() {
    let r = run_named("human_gate_answer_selects_its_edge").await;
    assert_path(&r, "success", &["start", "gate", "no", "exit"]);
    let r = run_named("human_gate_failure_does_not_fall_through").await;
    assert_path(&r, "failed", &["start", "gate"]);
}

#[tokio::test]
async fn retries_and_exhaustion_follow_fabro() {
    let r = run_named("retry_requested_then_success").await;
    assert_path(&r, "success", &["start", "a", "exit"]);
    assert_eq!(r.path[1].outcome, "succeeded");
    let r = run_named("retries_exhausted_allow_partial").await;
    assert_path(&r, "success", &["start", "a", "b", "exit"]);
    assert_eq!(r.path[1].outcome, "partially_succeeded");
    let r = run_named("retries_exhausted_stays_failed").await;
    assert_path(&r, "success", &["start", "a", "recover", "exit"]);
    let r = run_named("allow_partial_at_one_attempt").await;
    assert_path(&r, "success", &["start", "a", "exit"]);
    assert_eq!(r.path[1].outcome, "partially_succeeded");
}

#[tokio::test]
async fn goal_gates_jump_to_their_retry_target_or_fail_the_run() {
    let r = run_named("goal_gate_unsatisfied_jumps_to_retry_target_then_passes").await;
    assert_path(&r, "success", &[
        "start", "work", "verify", "work", "verify", "exit",
    ]);
    let r = run_named("goal_gate_unsatisfied_without_target_fails").await;
    assert_path(&r, "failed", &["start", "work", "verify"]);
}

#[tokio::test]
async fn static_fan_out_joins_every_branch() {
    let r = run_named("static_fan_out_joins_all_branches").await;
    assert_eq!(r.status, "success", "{r:?}");
    let nodes = r.nodes();
    assert_eq!(&nodes[..2], &["start", "fork"]);
    assert_eq!(&nodes[4..], &["merge", "report", "exit"]);
    assert!(nodes[2..4].contains(&"a") && nodes[2..4].contains(&"b"));
}

#[tokio::test]
async fn loop_restart_produces_a_successor_execution() {
    let r = run_named("loop_restart_starts_a_successor_execution").await;
    assert_eq!(r.status, "success", "{r:?}");
    assert_eq!(r.executions, 2);
    assert_eq!(
        r.nodes(),
        ["work", "check", "exit"],
        "the successor's own history"
    );
}

#[tokio::test]
async fn skipped_and_partial_outcomes_route_as_fabro_documents() {
    let r = run_named("skipped_outcome_routes_like_success").await;
    assert_path(&r, "success", &["start", "a", "b", "exit"]);
    let r = run_named("partially_succeed_policy_classifies_before_routing").await;
    assert_path(&r, "success", &["start", "a", "b", "exit"]);
    assert_eq!(r.path[1].outcome, "partially_succeeded");
}

/// Every shared case against its committed Fabro oracle fixture. The fixture
/// holds Fabro's result; a case with a recorded departure holds Petri's
/// expected result beside Fabro's, and the test checks Petri against that.
/// With `PETRI_ORACLE_RECORD=1` (what `scripts/oracle-regenerate.sh` sets)
/// the test writes Petri's result into the fixture of every departure case
/// instead of checking it.
#[tokio::test]
#[expect(
    clippy::print_stderr,
    reason = "a missing fixture is reported on stderr with the command that generates it"
)]
async fn every_case_matches_the_fabro_oracle() {
    let expected_dir = oracle_dir().join("expected");
    let record = env::var("PETRI_ORACLE_RECORD").is_ok_and(|v| !v.is_empty());
    let mut missing = Vec::new();
    let mut mismatches = Vec::new();
    for case in cases() {
        let path = expected_dir.join(format!("{}.json", case.name));
        let Ok(text) = fs::read_to_string(&path) else {
            missing.push(case.name.clone());
            continue;
        };
        let mut fixture: Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let actual = result_json(&run_case(&case).await);
        if case.departure.is_some() && record {
            fixture["petri"] = actual;
            fs::write(
                &path,
                format!(
                    "{}\n",
                    serde_json::to_string_pretty(&fixture).expect("json")
                ),
            )
            .expect("write the fixture");
            continue;
        }
        let expected = if case.departure.is_some() {
            fixture.get("petri").cloned().unwrap_or(Value::Null)
        } else {
            fixture["fabro"].clone()
        };
        if actual != expected {
            mismatches.push(format!(
                "{}:\n  expected {}\n  actual   {}",
                case.name,
                serde_json::to_string(&expected).expect("json"),
                serde_json::to_string(&actual).expect("json")
            ));
        }
    }
    if !missing.is_empty() {
        eprintln!(
            "no oracle fixture for: {}; run scripts/oracle-regenerate.sh",
            missing.join(", ")
        );
    }
    assert!(
        missing.is_empty(),
        "every case has a committed Fabro oracle fixture"
    );
    assert!(
        mismatches.is_empty(),
        "oracle mismatches:\n{}",
        mismatches.join("\n")
    );
}
