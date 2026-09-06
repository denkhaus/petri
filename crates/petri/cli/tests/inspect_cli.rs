//! `petri inspect` over run directories the shipped binary wrote: after the
//! process exits, twice over, with no provider reachable, after the source
//! workflow is gone, after a restart with a nested child, after a failure and
//! a cancellation, over damaged logs, and with a sensitive answer masked.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};
use std::{env, fs, io, thread};

use serde_json::Value;
use testkit::RunDir;

fn petri() -> Command {
    Command::new(env!("CARGO_BIN_EXE_petri"))
}

fn run_workflow(run_dir: &Path, workflow: &Path, extra: &[&str]) -> Output {
    petri()
        .args(["run", "--quiet"])
        .args(extra)
        .arg("--run-dir")
        .arg(run_dir)
        .arg(workflow)
        .output()
        .expect("petri runs")
}

/// `petri inspect --json` under an environment with no provider reachable
/// and no sandbox plugin named: the command needs neither.
fn inspect(run_dir: &Path) -> (Output, Value) {
    let output = petri()
        .args(["inspect", "--json", "--run-dir"])
        .arg(run_dir)
        .env("ANTHROPIC_API_KEY", "bogus")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .env("OPENAI_API_KEY", "bogus")
        .env("OPENAI_BASE_URL", "http://127.0.0.1:9")
        .env_remove("PETRI_SANDBOX_HOST_PLUGIN")
        .env_remove("PETRI_SANDBOX_DOCKER_PLUGIN")
        .output()
        .expect("petri inspects");
    let document = if output.stdout.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout is not JSON: {error}\n{}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    };
    (output, document)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Every file under `root`: its bytes and modification time.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (SystemTime, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, (SystemTime, Vec<u8>)>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                walk(&path, out)?;
            } else {
                let modified = entry.metadata()?.modified()?;
                out.insert(path.clone(), (modified, fs::read(&path)?));
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out).expect("the run dir walks");
    out
}

fn root_events(run_dir: &Path) -> PathBuf {
    run_dir.join("invocations/0000000000000000/executions/0000000000000000/events.jsonl")
}

/// A restart loop around a nested child. `work` reports `done=true` on its
/// second visit through a counter file, so the root restarts once and the
/// child runs in each execution.
fn restart_with_child(counter: &Path) -> String {
    format!(
        r#"digraph T {{
            start [shape=Mdiamond]
            exit [shape=Msquare]
            work [shape=parallelogram, output_schema="routing", max_visits=3, script="f={counter}; n=$(cat \"$f\" 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" > \"$f\"; if [ \"$n\" -ge 2 ]; then echo '{{\"context_updates\": {{\"done\": \"true\", \"visits\": \"'$n'\"}}}}'; else echo '{{\"context_updates\": {{\"done\": \"false\", \"visits\": \"'$n'\"}}}}'; fi"]
            child [shape=house, stack.child_dot_source="digraph C {{ start [shape=Mdiamond] exit [shape=Msquare] inner [shape=parallelogram, output_schema=\"routing\", script=\"echo '{{\\\"context_updates\\\": {{\\\"inner\\\": \\\"ran\\\"}}}}'\"] start -> inner -> exit }}"]
            check [shape=diamond]
            start -> work -> child -> check
            check -> exit [condition="context.done=true"]
            check -> work [loop_restart=true]
        }}"#,
        counter = counter.display()
    )
}

fn executions_of(document: &Value, invocation: u64) -> Vec<&Value> {
    document["executions"]
        .as_array()
        .expect("executions")
        .iter()
        .filter(|execution| execution["invocation"] == invocation)
        .collect()
}

#[test]
fn inspect_reconstructs_a_restarted_run_with_children_after_the_process_exits() {
    let dir = RunDir::new("inspect-cli-restart");
    let workflow = dir.path().join("loop.fabro");
    fs::write(&workflow, restart_with_child(&dir.path().join("counter")))
        .expect("write the workflow");
    let run_dir = dir.path().join("run");
    let ran = run_workflow(&run_dir, &workflow, &[]);
    assert!(ran.status.success(), "{}", stderr(&ran));

    // The source workflow is gone: only the run dir speaks.
    fs::remove_file(&workflow).expect("remove the workflow");

    let before = snapshot(&run_dir);
    let (first, document) = inspect(&run_dir);
    assert!(first.status.success(), "{}", stderr(&first));
    let (second, again) = inspect(&run_dir);
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(first.stdout, second.stdout, "inspection is deterministic");
    assert_eq!(document, again);
    assert_eq!(snapshot(&run_dir), before, "inspection changed the run dir");

    assert_eq!(document["inspect_format_version"], Value::from(1));
    assert_eq!(document["complete"], Value::Bool(true));
    assert_eq!(document["status"], Value::from("success"));

    // The root restarted once: two executions, the second is final.
    let root_executions = executions_of(&document, 0);
    assert_eq!(root_executions.len(), 2, "{document:#}");
    assert_eq!(root_executions[0]["status"], Value::from("restarted"));
    assert_eq!(root_executions[1]["status"], Value::from("finished"));
    assert_eq!(
        document["root"]["final_execution"],
        root_executions[1]["execution"]
    );
    assert_eq!(root_executions[1]["entry_node"], Value::from("work"));
    assert_eq!(
        root_executions[0]["exit"]["kind"],
        Value::from("restart"),
        "{:#}",
        root_executions[0]
    );
    assert_eq!(
        root_executions[0]["exit"]["target_name"],
        Value::from("work")
    );

    // Each execution keeps its own context: the first saw `done=false`,
    // the final saw `done=true`.
    let first_kv = &root_executions[0]["engine"]["context"]["kv"];
    let final_kv = &root_executions[1]["engine"]["context"]["kv"];
    assert_eq!(first_kv["done"], Value::from("false"), "{first_kv:#}");
    assert_eq!(final_kv["done"], Value::from("true"), "{final_kv:#}");
    assert_eq!(first_kv["visits"], Value::from("1"));
    assert_eq!(final_kv["visits"], Value::from("2"));
    let final_nodes = &root_executions[1]["engine"]["context"]["nodes"];
    assert_eq!(final_nodes["work"]["status"], Value::from("success"));
    assert_eq!(final_nodes["exit"]["status"], Value::from("success"));
    assert_eq!(final_nodes["work"]["attempts"], Value::from(1));
    assert!(
        final_nodes.get("start").is_none(),
        "the successor starts at the restart target: {final_nodes:#}"
    );

    // Routing across the repeated visit is in the history and routes.
    let first_routes = root_executions[0]["engine"]["routes"]
        .as_array()
        .expect("routes");
    assert!(
        first_routes
            .iter()
            .any(|route| route["node"] == "check" && route["target"] == "work"),
        "{first_routes:#?}"
    );

    // One child invocation per root execution, each with its own context.
    let invocations = document["invocations"].as_array().expect("invocations");
    assert_eq!(invocations.len(), 3, "{invocations:#?}");
    let root = &invocations[0];
    assert_eq!(root["invocation"], Value::from(0));
    assert_eq!(root["children"].as_array().map(Vec::len), Some(2));
    for (child, root_execution) in invocations[1..].iter().zip(&root_executions) {
        assert_eq!(child["status"], Value::from("finished"), "{child:#}");
        assert_eq!(child["parent"]["invocation"], Value::from(0));
        assert_eq!(child["parent"]["execution"], root_execution["execution"]);
        assert_eq!(child["sandbox"], Value::from("inherited"));
        assert_eq!(child["secrets"]["mode"], Value::from("inherit"));
        assert_eq!(child["result"]["status"], Value::from("success"));
        assert_eq!(child["result"]["context"]["inner"], Value::from("ran"));
        let child_id = child["invocation"].as_u64().expect("id");
        let child_executions = executions_of(&document, child_id);
        assert_eq!(child_executions.len(), 1);
        let nodes = &child_executions[0]["engine"]["context"]["nodes"];
        assert_eq!(nodes["inner"]["status"], Value::from("success"));
        assert!(
            nodes.get("work").is_none(),
            "the child's context is its own"
        );
    }
    assert_eq!(
        invocations[1]["context"]["done"],
        Value::from("false"),
        "the first child was declared with the first visit's context"
    );
    assert_eq!(invocations[2]["context"]["done"], Value::from("true"));
}

#[test]
fn inspect_reports_a_failed_run_as_complete_and_failed() {
    let dir = RunDir::new("inspect-cli-failed");
    let workflow = dir.path().join("fail.fabro");
    fs::write(
        &workflow,
        r#"digraph F {
            start [shape=Mdiamond]
            exit [shape=Msquare]
            boom [shape=parallelogram, script="echo about to fail; exit 2", on_failure="exit"]
            start -> boom -> exit
        }"#,
    )
    .expect("write the workflow");
    let run_dir = dir.path().join("run");
    let ran = run_workflow(&run_dir, &workflow, &[]);
    assert!(!ran.status.success(), "the run fails:\n{}", stderr(&ran));

    let (output, document) = inspect(&run_dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(document["complete"], Value::Bool(true));
    assert_eq!(document["status"], Value::from("failed"));
    let root = &document["invocations"][0];
    assert_eq!(root["result"]["status"], Value::from("failed"));
    assert!(
        root["result"]["failure"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("status 2")),
        "{root:#}"
    );
    let nodes = &document["executions"][0]["engine"]["context"]["nodes"];
    assert_eq!(nodes["boom"]["status"], Value::from("failure"));
    assert_eq!(nodes["boom"]["success_like"], Value::Bool(false));
    assert!(nodes.get("exit").is_none(), "{nodes:#}");
    assert_eq!(
        document["executions"][0]["engine"]["folded_status"],
        Value::from("failed")
    );
}

#[test]
fn inspect_reports_a_run_cancelled_by_sigint() {
    let dir = RunDir::new("inspect-cli-cancelled");
    let marker = dir.path().join("started");
    let workflow = dir.path().join("slow.fabro");
    fs::write(
        &workflow,
        format!(
            r#"digraph S {{
                start [shape=Mdiamond]
                exit [shape=Msquare]
                slow [shape=parallelogram, script="echo go > {marker}; sleep 120"]
                start -> slow -> exit
            }}"#,
            marker = marker.display()
        ),
    )
    .expect("write the workflow");
    let run_dir = dir.path().join("run");
    let child = petri()
        .args(["run", "--quiet", "--run-dir"])
        .arg(&run_dir)
        .arg(&workflow)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("petri starts");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "the step never started");
        thread::sleep(Duration::from_millis(50));
    }
    let interrupted = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(interrupted.success());
    let output = child.wait_with_output().expect("petri exits");
    let text = stderr(&output);
    assert!(!output.status.success(), "{text}");
    assert!(text.contains("run: cancelled"), "{text}");

    let (inspected, document) = inspect(&run_dir);
    assert!(inspected.status.success(), "{}", stderr(&inspected));
    assert_eq!(document["complete"], Value::Bool(true));
    assert_eq!(document["status"], Value::from("cancelled"));
    let engine = &document["executions"][0]["engine"];
    assert_eq!(engine["cancelled"], Value::Bool(true));
    assert_eq!(
        engine["context"]["nodes"]["slow"]["status"],
        Value::from("cancelled")
    );
    assert_eq!(engine["live"].as_array().map(Vec::len), Some(0));
}

fn finished_run(label: &str) -> (RunDir, PathBuf) {
    let dir = RunDir::new(label);
    let workflow = dir.path().join("ok.fabro");
    fs::write(
        &workflow,
        r#"digraph G {
            start [shape=Mdiamond]
            exit [shape=Msquare]
            a [shape=parallelogram, script="echo alpha"]
            b [shape=parallelogram, script="echo beta"]
            start -> a -> b -> exit
        }"#,
    )
    .expect("write the workflow");
    let run_dir = dir.path().join("run");
    let ran = run_workflow(&run_dir, &workflow, &[]);
    assert!(ran.status.success(), "{}", stderr(&ran));
    (dir, run_dir)
}

#[test]
fn inspect_reports_torn_logs_as_incomplete_and_corrupt_logs_as_errors() {
    let (_dir, run_dir) = finished_run("inspect-cli-damaged");

    // A torn engine-log tail: incomplete, exit 1, the file left alone.
    let events = root_events(&run_dir);
    let clean = fs::read(&events).expect("reads");
    let mut torn = clean.clone();
    torn.extend_from_slice(b"{\"seq\":");
    fs::write(&events, &torn).expect("writes");
    let (output, document) = inspect(&run_dir);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(document["complete"], Value::Bool(false));
    assert_eq!(document["executions"][0]["log"]["torn"], Value::Bool(true));
    assert!(
        stderr(&output).contains("incomplete:"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        fs::read(&events).expect("reads"),
        torn,
        "the torn tail was rewritten"
    );

    // A complete line that does not decode: an error, exit 2, no document.
    let mut corrupt = clean.clone();
    corrupt.extend_from_slice(b"not-json\n");
    fs::write(&events, &corrupt).expect("writes");
    let (output, document) = inspect(&run_dir);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(document, Value::Null);
    assert!(stderr(&output).contains("error:"), "{}", stderr(&output));
    fs::write(&events, &clean).expect("restores");

    // A coordinator log missing its finish: incomplete, no status.
    let coordinator = run_dir.join("coordinator.jsonl");
    let text = fs::read_to_string(&coordinator).expect("reads");
    let mut lines: Vec<&str> = text.lines().collect();
    assert!(lines.pop().is_some_and(|last| last.contains("RunFinished")));
    fs::write(&coordinator, format!("{}\n", lines.join("\n"))).expect("writes");
    let (output, document) = inspect(&run_dir);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(document["status"], Value::Null);
    assert_eq!(document["complete"], Value::Bool(false));
    fs::write(&coordinator, &text).expect("restores");

    // An unsupported run format: an error.
    let metadata = run_dir.join("run.json");
    let mut value: Value =
        serde_json::from_slice(&fs::read(&metadata).expect("reads")).expect("json");
    value["format_version"] = Value::from(1);
    fs::write(&metadata, serde_json::to_vec(&value).expect("encodes")).expect("writes");
    let (output, _) = inspect(&run_dir);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("unsupported"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn inspect_prints_a_summary_without_json() {
    let (_dir, run_dir) = finished_run("inspect-cli-summary");
    let output = petri()
        .args(["inspect", "--run-dir"])
        .arg(&run_dir)
        .output()
        .expect("petri inspects");
    assert!(output.status.success(), "{}", stderr(&output));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.starts_with("run: success\n"), "{text}");
    assert!(text.contains("  success a gen 0 attempt 1\n"), "{text}");
    assert!(
        !text.contains("interviews:"),
        "a run without an interviewer has no receipt line: {text}"
    );
    let (_, document) = inspect(&run_dir);
    assert_eq!(document["interviews"], Value::Null);
}

#[test]
fn inspect_shows_a_sensitive_answer_as_a_secret_reference_only() {
    let dir = RunDir::new("inspect-cli-sensitive");
    let workflow = dir.path().join("gate.fabro");
    fs::write(
        &workflow,
        r#"digraph Gate {
            start [shape=Mdiamond]
            exit [shape=Msquare]
            gate [shape=hexagon, label="Token?", sensitive=true]
            done [shape=parallelogram, script="echo done"]
            start -> gate
            gate -> done [freeform=true]
            done -> exit
        }"#,
    )
    .expect("write the workflow");
    let run_dir = dir.path().join("run");
    let plaintext = "hunter2-plaintext-token";
    let mut child = petri()
        .args(["run", "--quiet", "--interactive", "--run-dir"])
        .arg(&run_dir)
        .arg(&workflow)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("petri starts");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{plaintext}\n").as_bytes())
        .expect("answer written");
    let output = child.wait_with_output().expect("petri exits");
    let text = stderr(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("success done"), "{text}");

    let (inspected, document) = inspect(&run_dir);
    assert!(inspected.status.success(), "{}", stderr(&inspected));
    let rendered = String::from_utf8_lossy(&inspected.stdout);
    assert!(
        !rendered.contains(plaintext),
        "the plaintext leaked:\n{rendered}"
    );
    let engine = &document["executions"][0]["engine"];
    let gate = &engine["context"]["nodes"]["gate"];
    assert_eq!(gate["status"], Value::from("success"), "{gate:#}");
    // The step's echo of the value was masked before it was appended.
    assert_eq!(gate["output"]["text"], Value::from("***"), "{gate:#}");
    assert_eq!(
        engine["context"]["kv"]["human.gate.text"],
        Value::from("***")
    );
    // The answer itself crossed as a reference, and that is what the log
    // and the document hold.
    let deliveries = engine["deliveries"].as_array().expect("deliveries");
    let answer = deliveries
        .iter()
        .find(|delivery| delivery["kind"] == "deliver")
        .unwrap_or_else(|| panic!("an answer was delivered: {deliveries:#?}"));
    assert_eq!(answer["node"], Value::from("gate"));
    assert!(
        answer["payload"]["$answer"]["text"]["$secret"]
            .as_str()
            .is_some_and(|name| name.starts_with("answer:gate#")),
        "{answer:#}"
    );
    // The interview receipt rides the same document, with the answer as its
    // reference only.
    let receipt = &document["interviews"];
    assert_eq!(receipt["version"], Value::from(1), "{receipt:#}");
    assert_eq!(receipt["errors"], Value::Array(Vec::new()), "{receipt:#}");
    let question = &receipt["questions"][0];
    assert_eq!(question["node"], Value::from("gate"));
    assert_eq!(question["sensitive"], Value::from(true));
    assert_eq!(question["delivery"], Value::from("delivered"));
    assert!(
        question["reply"]["text"]["$secret"]
            .as_str()
            .is_some_and(|name| name.starts_with("answer:gate#")),
        "{question:#}"
    );
    for file in snapshot(&run_dir).values() {
        assert!(
            !String::from_utf8_lossy(&file.1).contains(plaintext),
            "the plaintext is in the run dir"
        );
    }
}
