//! The evidence side of the readiness gate: a scenario run through the
//! shipped binary leaves a machine-readable record with every pin, its
//! launch inputs, the twin scenarios it consumed, its observations, final
//! context, assertions, and cleanup; a failed scenario keeps its whole case
//! directory; the coverage report (`scripts/fabro-coverage-report.py`)
//! counts only passed cells; the pin check (`scripts/check-pins.py`) rejects
//! a record that cites another revision; and a required asset fails instead
//! of skipping when CI asks for it.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::{env, fs, panic, process};

use serde_json::{Value, json};
use support::fabro::evidence::{Backend, Recorder, outcomes};
use support::fabro::launch::Case;
use support::fabro::require;
use support::fabro::twins::{Provider, Twin, model, scenario, text};

const TEST_FILE: &str = "fabro_evidence_blackbox";

fn workspace_root() -> PathBuf {
    require::workspace_root()
}

fn script(name: &str, args: &[&str]) -> Output {
    Command::new("python3")
        .arg(workspace_root().join("scripts").join(name))
        .args(args)
        .env_remove("PETRI_EVIDENCE_DIR")
        .output()
        .expect("python3 runs the script")
}

fn read_json(path: &Path) -> Value {
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).expect("JSON")
}

/// A workflow with one command and one native agent answered by the twin.
fn prepare_and_answer(provider: Provider) -> String {
    format!(
        r#"digraph Evidence {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    prepare [shape=parallelogram, script="printf 'draft\n' > notes.txt && echo prepared"]
    agent [prompt="Say EVIDENCE_DONE and nothing else.", model="{model}", provider="{provider}", on_failure="exit"]
    start -> prepare -> agent -> exit
}}"#,
        model = model(provider),
        provider = provider.id(),
    )
}

#[tokio::test]
async fn a_host_scenario_writes_a_complete_evidence_record() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("evidence-host");
    let evidence = case.root.join("evidence");
    let twin = Twin::start(provider, &case.root.join("twins"), vec![scenario(
        provider,
        &case.credential,
        "answer",
        model(provider),
        "EVIDENCE_DONE",
        text("EVIDENCE_DONE"),
    )])
    .await;
    case.redirect(&twin);
    let workflow = case.workflow(&prepare_and_answer(provider), None);
    let test = format!("{TEST_FILE}::a_host_scenario_writes_a_complete_evidence_record");
    let mut record = Recorder::start_in(&evidence, "evidence-smoke", Backend::Host, &test);
    record.scenario_meta("evidence", None);
    record.launch(&workflow, &[], &[]);

    let finished = case.run(&workflow, &[]).await;
    record.finished(&finished);
    record.twin(&twin);
    assert!(
        record.check("exit code 0", finished.code == Some(0), &finished.stderr),
        "{}",
        finished.stderr
    );
    assert!(record.check(
        "run: success",
        finished.status_line() == Some("success"),
        &finished.stderr
    ));
    assert!(record.check(
        "the twin's scripted answer was consumed once",
        twin.consumed() == ["answer"] && twin.unmatched() == 0,
        format!("{:?}", twin.consumed())
    ));
    record.artifact(&case.workspace().join("notes.txt"), "the command's file");
    record.decision(
        "none",
        "note",
        "crates/fabro/acceptance/CONTRACT.md#accepted-differences",
        "no accepted difference is exercised by this scenario",
    );
    record.library_link("pebble", "pebble-cli::coding_sessions");
    finished.assert_no_leaked_processes().await;
    record.cleanup("clean", "no process launched for the case is still alive");
    let path = record.finish();

    // The record, read back the way the coverage report reads it.
    let record = read_json(&path);
    assert_eq!(record["schema_version"], json!(1));
    assert_eq!(record["outcome"], json!("passed"));
    assert_eq!(record["scenario"]["id"], json!("evidence-smoke"));
    assert_eq!(record["scenario"]["backend"], json!("host"));
    assert_eq!(record["scenario"]["test"], json!(test));
    assert_eq!(record["scenario"]["family"], json!("evidence"));
    assert_eq!(record["launch"]["workflow"], json!(workflow));
    assert!(
        record["launch"]["workflow_text"]
            .as_str()
            .is_some_and(|t| t.contains("EVIDENCE_DONE")),
        "{}",
        record["launch"]
    );
    assert_eq!(record["services"][0]["provider"], json!("openai"));
    assert_eq!(
        record["services"][0]["scenarios_consumed"],
        json!(["answer"])
    );
    assert_eq!(record["services"][0]["unmatched_requests"], json!(0));
    assert_eq!(record["observations"]["raw"]["exit_code"], json!(0));
    assert_eq!(
        record["observations"]["normalized"]["status"],
        json!("success")
    );
    assert_eq!(
        record["final_context"]["response.agent"],
        json!("EVIDENCE_DONE")
    );
    assert_eq!(record["artifacts"][0]["bytes"], json!("draft\n".len()));
    assert_eq!(record["assertions"].as_array().map(Vec::len), Some(3));
    assert!(
        record["assertions"]
            .as_array()
            .expect("assertions")
            .iter()
            .all(|a| a["outcome"] == "passed"),
        "{}",
        record["assertions"]
    );
    assert_eq!(record["cleanup"]["result"], json!("clean"));
    assert_eq!(record["library_links"][0]["library"], json!("pebble"));
    assert_eq!(
        record["library_links"][0]["revision"],
        record["pins"]["pebble"]
    );
    for pin in [
        "pebble",
        "lithos_llm",
        "sandbox_driver",
        "twins",
        "fabro_reference",
    ] {
        let value = record["pins"][pin].as_str().expect(pin);
        assert!(value.len() >= 7 && value != "unpinned", "{pin}: {value}");
    }
    assert!(record["pins"]["petri"]["commit"].is_string());

    // The bundle: process output, the inspect document, the twin's log, and
    // no case copy because the scenario passed.
    let bundle = PathBuf::from(record["bundle"]["dir"].as_str().expect("bundle dir"));
    for file in [
        "stdout.txt",
        "stderr.txt",
        "inspect.json",
        "openai-requests.jsonl",
    ] {
        assert!(
            bundle.join(file).is_file(),
            "{file} in {}",
            bundle.display()
        );
    }
    assert!(!bundle.join("case").exists());
    assert!(record["bundle"]["case"].is_null());

    // The pin check accepts the record; the coverage report passes the run.
    let pins = script("check-pins.py", &[
        "--evidence",
        evidence.to_str().expect("utf-8"),
    ]);
    assert!(
        pins.status.success(),
        "{}",
        String::from_utf8_lossy(&pins.stderr)
    );
    let coverage = script("fabro-coverage-report.py", &[
        "--evidence",
        evidence.to_str().expect("utf-8"),
        "--strict",
    ]);
    assert!(
        coverage.status.success(),
        "{}",
        String::from_utf8_lossy(&coverage.stderr)
    );
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(report["totals"]["required"], json!(1));
    assert_eq!(report["totals"]["passed"], json!(1));
    assert_eq!(report["ok"], json!(true));
    twin.stop();
}

#[test]
fn a_failed_scenario_keeps_its_full_bundle_and_never_counts_as_passed() {
    let root = env::temp_dir().join(format!(
        "petri-evidence-failure-{}-{}",
        process::id(),
        testkit::unique_id()
    ));
    let case = root.join("case");
    fs::create_dir_all(case.join("run")).expect("case dir");
    fs::write(case.join("run").join("marker.txt"), "kept\n").expect("marker");
    let evidence = root.join("evidence");

    let mut record = Recorder::start_in(
        &evidence,
        "evidence-failure",
        Backend::Docker,
        &format!("{TEST_FILE}::a_failed_scenario_keeps_its_full_bundle"),
    );
    record.case_root(&case);
    assert!(record.check("holds", true, ""));
    assert!(!record.check("the file says shipped", false, "it says held"));
    let path = record.finish();

    let record = read_json(&path);
    assert_eq!(record["outcome"], json!("failed"));
    assert_eq!(record["scenario"]["backend"], json!("docker"));
    let copy = PathBuf::from(record["bundle"]["case"].as_str().expect("case copy"));
    assert_eq!(
        fs::read_to_string(copy.join("run").join("marker.txt")).expect("copied marker"),
        "kept\n"
    );
    assert_eq!(record["assertions"][1]["outcome"], json!("failed"));

    let coverage = script("fabro-coverage-report.py", &[
        "--evidence",
        evidence.to_str().expect("utf-8"),
        "--strict",
    ]);
    assert_eq!(coverage.status.code(), Some(1));
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(report["totals"]["failed"], json!(1));
    assert_eq!(report["totals"]["passed"], json!(0));
    assert_eq!(report["ok"], json!(false));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_panic_before_finish_still_writes_a_failed_record() {
    let evidence = env::temp_dir().join(format!(
        "petri-evidence-panic-{}-{}",
        process::id(),
        testkit::unique_id()
    ));
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let mut record = Recorder::start_in(
            &evidence,
            "evidence-panic",
            Backend::Host,
            &format!("{TEST_FILE}::a_panic_before_finish"),
        );
        record.check("first", true, "");
        panic!("the scenario blew up");
    }));
    assert!(result.is_err());
    let recorded = outcomes(&evidence);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    let (id, outcome) = recorded.iter().next().expect("one record");
    assert!(id.starts_with("evidence-panic--host--"), "{id}");
    assert_eq!(outcome, "failed");
    let record = read_json(&evidence.join("records").join(format!("{id}.json")));
    assert!(
        record["assertions"]
            .as_array()
            .expect("assertions")
            .iter()
            .any(|a| a["name"] == "record"
                && a["detail"].as_str().is_some_and(|d| d.contains("panicked"))),
        "{}",
        record["assertions"]
    );
    let _ = fs::remove_dir_all(&evidence);
}

#[test]
fn the_coverage_report_counts_only_passed_cells() {
    let root = env::temp_dir().join(format!(
        "petri-evidence-coverage-{}-{}",
        process::id(),
        testkit::unique_id()
    ));
    let evidence = root.join("evidence");
    Recorder::start_in(&evidence, "alpha", Backend::Host, "t::alpha_host").finish();
    Recorder::start_in(&evidence, "alpha", Backend::Docker, "t::alpha_docker")
        .skipped("no Docker daemon");
    Recorder::start_in(&evidence, "delta", Backend::Host, "t::delta").blocked("fixture missing");
    let manifest = root.join("manifest.json");
    fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "scenarios": [
                { "id": "alpha", "status": "required", "backends": ["host", "docker"], "tests": ["t::alpha_host", "t::alpha_docker"] },
                { "id": "beta", "status": "required", "backends": ["host"], "tests": ["t::beta"] },
                { "id": "gamma", "status": "excluded", "backends": ["host"], "reason": "not in the replacement set" },
                { "id": "delta", "status": "blocked", "backends": ["host"], "reason": "needs a fixture repository" },
            ]
        }))
        .expect("manifest"),
    )
    .expect("write manifest");

    // Not strict: exit 0 and the counts.
    let output = script("fabro-coverage-report.py", &[
        "--evidence",
        evidence.to_str().expect("utf-8"),
        "--manifest",
        manifest.to_str().expect("utf-8"),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(
        report["totals"]["required"],
        json!(3),
        "{}",
        report["totals"]
    );
    assert_eq!(report["totals"]["passed"], json!(1));
    assert_eq!(report["totals"]["skipped"], json!(1));
    assert_eq!(report["totals"]["missing"], json!(1));
    assert_eq!(report["totals"]["blocked"], json!(1));
    assert_eq!(report["totals"]["excluded"], json!(1));
    assert_eq!(report["totals"]["failed"], json!(0));
    assert_eq!(report["ok"], json!(false));
    let markdown = fs::read_to_string(evidence.join("coverage.md")).expect("coverage.md");
    assert!(
        markdown.contains("| alpha | docker | required | skipped |"),
        "{markdown}"
    );
    assert!(
        markdown.contains("| beta | host | required | missing |"),
        "{markdown}"
    );
    assert!(markdown.contains("Gate: NOT passed"), "{markdown}");

    // Strict: the same run is a failure.
    let strict = script("fabro-coverage-report.py", &[
        "--evidence",
        evidence.to_str().expect("utf-8"),
        "--manifest",
        manifest.to_str().expect("utf-8"),
        "--strict",
    ]);
    assert_eq!(strict.status.code(), Some(1));

    // A JUnit failure overrides a passed record: two engines agreeing on a
    // wrong report still fail when the runner said so.
    let junit = root.join("junit.xml");
    fs::write(
        &junit,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites><testsuite name="petri-cli::t"><testcase classname="petri-cli::t" name="alpha_host"><failure message="assertion"/></testcase></testsuite></testsuites>"#,
    )
    .expect("junit");
    let output = script("fabro-coverage-report.py", &[
        "--evidence",
        evidence.to_str().expect("utf-8"),
        "--manifest",
        manifest.to_str().expect("utf-8"),
        "--junit",
        junit.to_str().expect("utf-8"),
    ]);
    assert!(output.status.success());
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(report["totals"]["passed"], json!(0));
    assert_eq!(report["totals"]["failed"], json!(1));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_pin_check_rejects_a_record_citing_another_revision() {
    let evidence = env::temp_dir().join(format!(
        "petri-evidence-pins-{}-{}",
        process::id(),
        testkit::unique_id()
    ));
    let path = Recorder::start_in(&evidence, "pins", Backend::Host, "t::pins").finish();
    let mut record = read_json(&path);
    record["pins"]["pebble"] = json!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
    fs::write(&path, serde_json::to_vec_pretty(&record).expect("record")).expect("rewrite");
    let output = script("check-pins.py", &[
        "--evidence",
        evidence.to_str().expect("utf-8"),
    ]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pebble cites deadbeef"), "{stderr}");
    let _ = fs::remove_dir_all(&evidence);
}

#[test]
fn a_required_asset_fails_instead_of_skipping_when_ci_asks() {
    assert_eq!(
        require::decide(true, Some("1"), "PETRI_REQUIRE_FABRO_BUNDLES", "absent"),
        Ok(true)
    );
    assert_eq!(
        require::decide(false, None, "PETRI_REQUIRE_FABRO_BUNDLES", "absent"),
        Ok(false)
    );
    assert_eq!(
        require::decide(false, Some(""), "PETRI_REQUIRE_FABRO_BUNDLES", "absent"),
        Ok(false)
    );
    assert_eq!(
        require::decide(
            false,
            Some("1"),
            "PETRI_REQUIRE_FABRO_BINARY",
            "the pinned fabro binary is absent"
        ),
        Err("PETRI_REQUIRE_FABRO_BINARY is set, but the pinned fabro binary is absent".to_owned())
    );
}

#[test]
fn the_materialized_bundles_are_found_or_skipped_visibly() {
    // The bundles are fetched data. When they are present the helper names
    // the bundle directory; when absent it skips (or fails under the
    // require variable, which CI sets after fetching).
    if let Some(dir) = require::bundle("interview") {
        assert!(dir.join("MANIFEST.txt").is_file() || dir.is_dir());
    }
}
