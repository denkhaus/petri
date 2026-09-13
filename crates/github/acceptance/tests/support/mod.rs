//! The GHA end-to-end harness: lower with the real frontend, run on the
//! standard runtime.

#![allow(
    dead_code,
    reason = "each test binary uses only the harness helpers its own battery needs"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::env::{self, consts};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fs, mem};

use execution::host::{self, HostRun};
use execution::{
    CoordinatorEvent, CoordinatorRecord, ExecutionId, ExecutionObserver, InvocationId,
};
use frontend_gha::load;
use github_actions::GitActionSource;
use runtime::driver::{ExecutionReport, RunGuard};
use runtime::executor::{MapSecrets, Retention};
use runtime::frontend::{FileSource, MapFiles, NoFiles};
use runtime::ir::Graph;
use runtime::{RunOptions, Runtime, engine, ir};
use serde_json::json;
use tokio::time;

pub(crate) fn files(pairs: &[(&str, &str)]) -> MapFiles {
    MapFiles(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>(),
    )
}

pub(crate) fn lower_ok(text: &str) -> Graph {
    lower_ok_with(text, &NoFiles).graph
}

#[expect(
    clippy::print_stderr,
    reason = "the harness echoes lowering diagnostics so a failing battery shows why"
)]
pub(crate) fn lower_ok_with(text: &str, files: &dyn FileSource) -> HostRun {
    let lowered = load(".github/workflows/test.yml", text, files);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    HostRun::new(lowered.graph.expect("expected a graph")).with_children(lowered.children)
}

/// Run parameters a real host would supply.
pub(crate) fn with_params(graph: Graph) -> Graph {
    let mut graph = graph;
    graph.params.entry("github".into()).or_insert(json!({
        "sha": "0123456789abcdef", "ref": "refs/heads/main", "ref_name": "main",
        "repository": "example/repo", "actor": "tester", "event_name": "push",
        "run_id": "1", "run_number": "1", "server_url": "https://github.com",
    }));
    graph.params.entry("runner".into()).or_insert(json!({
        "os": consts::OS, "arch": consts::ARCH, "name": "local",
    }));
    graph.params.entry("vars".into()).or_insert(json!({}));
    graph
}

fn run_dir(label: &str) -> PathBuf {
    // Canonical, or macOS's `/var` → `/private/var` symlink makes toolkit
    // actions compute relative archive paths that resolve nowhere.
    let temp = env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| env::temp_dir());
    let dir = temp
        .join("petri-gha")
        .join(format!("{label}-{}", process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// Whether `program --version` answers on this machine, with the skip note the
/// batteries share (`node` for JavaScript actions, `git` for fixtures).
#[expect(
    clippy::print_stderr,
    reason = "the skip note tells whoever runs the batteries why a test did nothing"
)]
pub(crate) fn is_tool_ready(program: &str) -> bool {
    let found = Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !found {
        eprintln!("skipping: no `{program}` on PATH");
    }
    found
}

/// A `gh` on `PATH` that records what it was asked and answers `cache list`, so
/// a corpus workflow runs for real without reaching GitHub's API. Returns the
/// bin dir to prepend to `PATH`; invocations append to the file named by
/// `GH_STUB_LOG`.
pub(crate) fn install_gh_stub(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).expect("the run dir is fresh, so it accepts a bin directory");
    let stub = bin.join("gh");
    fs::write(
        &stub,
        r#"#!/bin/sh
echo "gh $*" >> "$GH_STUB_LOG"
case "$1 $2" in
  "cache list") echo 101; echo 202 ;;
esac
exit 0
"#,
    )
    .expect("the bin directory was just created, so the stub is writable");
    #[cfg(unix)]
    {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stub, Permissions::from_mode(0o755))
            .expect("the stub was just written, so its mode is ours to set");
    }
    bin
}

/// The corpus's shared action cache: the acceptance batteries and the sweep
/// pull the same pinned trees once.
pub(crate) fn corpus_action_source() -> Arc<GitActionSource> {
    let cache = Path::new(env!("CARGO_MANIFEST_DIR")).join("../corpus/.actions-cache");
    Arc::new(GitActionSource::new(cache))
}

/// [`lower_ok`], with remote `uses:` resolved through `source`.
#[expect(
    clippy::print_stderr,
    reason = "the harness echoes lowering diagnostics so a failing battery shows why"
)]
pub(crate) fn lower_with_actions(text: &str, source: &Arc<GitActionSource>) -> Graph {
    let actions: Arc<dyn github_actions::ActionSource> = source.clone() as _;
    let lowered = frontend_gha::load_with(
        ".github/workflows/test.yml",
        text,
        &NoFiles,
        Some(actions.as_ref()),
    );
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered.graph.expect("the workflow lowers")
}

/// The distribution's run guard for the ObjectService, mirrored here: teardown
/// is the service's explicit async shutdown, so joining its thread never
/// blocks a Tokio worker.
struct ObjectServiceGuard(github_objects::ObjectService);

#[async_trait::async_trait]
impl RunGuard for ObjectServiceGuard {
    async fn teardown(self: Box<Self>) {
        self.0.shutdown().await;
    }
}

/// The distribution's per-run ObjectService wiring, assembled here because a
/// component's tests may not depend on the distribution: the service starts
/// beside the run dir and its capability reaches the steps. `cache_store` is
/// the host-scoped half; `None` keeps it per run, under the run dir.
#[expect(
    clippy::print_stderr,
    reason = "the harness warns when the results service will not start; the run goes on"
)]
pub(crate) fn with_object_service(rt: Runtime, cache_store: Option<PathBuf>) -> Runtime {
    rt.run_services(move |run_dir, caps| {
        let cache = cache_store.clone().unwrap_or_else(|| run_dir.join("cache"));
        match github_objects::ObjectService::start(run_dir.join("artifacts"), cache) {
            Ok(service) => {
                let cap = github_actions::ResultsServiceCap {
                    port:  service.port(),
                    token: service.token().into(),
                };
                (
                    caps.provide(cap),
                    Some(Box::new(ObjectServiceGuard(service)) as _),
                )
            }
            Err(error) => {
                eprintln!("warning: no results service: {error}");
                (caps, None)
            }
        }
    })
}

/// Run `git -C <dir>` under the batteries' fixed fixture identity, asserting
/// success.
pub(crate) fn git_in(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "petri")
        .env("GIT_AUTHOR_EMAIL", "petri@test")
        .env("GIT_COMMITTER_NAME", "petri")
        .env("GIT_COMMITTER_EMAIL", "petri@test")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `init -b main`, `add .`, `commit`: the committed half of a fixture tree.
pub(crate) fn commit_fixture(dir: &Path) {
    git_in(dir, &["init", "--quiet", "-b", "main"]);
    git_in(dir, &["add", "."]);
    git_in(dir, &["commit", "--quiet", "-m", "fixture"]);
}

/// The standard runtime plus the GitHub step kinds the frontend lowers to —
/// what the distribution registers, assembled here because a component's tests
/// may not depend on the distribution.
pub(crate) fn runtime(dir: &Path) -> Runtime {
    let mut options = RunOptions::new(dir);
    options.grace = Duration::from_secs(1);
    options.retention = Retention::Never;
    github_actions::register(Runtime::standard().options(options))
        // The distribution's token-less stance, so batteries test the shipped
        // semantics: `github.token` resolves to the empty string — the toolkit
        // treats it as "no auth" and reads anonymously — never a missing
        // secret. A bare runtime has no secrets at all, and a setup-* action's
        // `token: ${{ github.token }}` default then fails `secret_unavailable`
        // where the shipped product proceeds.
        .secrets(MapSecrets::from_pairs(&[("GITHUB_TOKEN", "")]))
}

/// Run on the standard runtime, which verifies replay itself.
pub(crate) async fn run_host(graph: impl Into<HostRun>, label: &str) -> RunReportPlus {
    run_host_with_secrets(graph, label, &[]).await
}

/// [`run_host`], with the runtime customized before the run — an extra
/// capability, an option — for batteries that probe host wiring.
pub(crate) async fn run_host_with(
    graph: impl Into<HostRun>,
    label: &str,
    customize: impl FnOnce(Runtime) -> Runtime,
) -> RunReportPlus {
    let mut run = graph.into();
    run.graph = with_params(run.graph);
    let dir = run_dir(label);
    let events = Arc::new(WorkflowEvents::default());
    let report = host::run_configured(
        &customize(runtime(&dir)),
        run.observe(events.clone()),
        |_, _| {},
    )
    .await
    .expect("replay is byte-identical");
    let _ = fs::remove_dir_all(&dir);
    let mut report = RunReportPlus::from(report);
    report.child_logs = mem::take(&mut *events.child_logs.lock().expect("workflow logs lock"));
    report.lifecycle = mem::take(&mut *events.lifecycle.lock().expect("workflow lifecycle lock"));
    report
}

/// [`run_host`], with named secrets configured for the run.
pub(crate) async fn run_host_with_secrets(
    graph: impl Into<HostRun>,
    label: &str,
    secrets: &[(&str, &str)],
) -> RunReportPlus {
    // `.secrets()` replaces the provider, so the default empty GITHUB_TOKEN
    // rides along unless the test names its own.
    let mut pairs: Vec<(&str, &str)> = secrets.to_vec();
    if !pairs.iter().any(|(k, _)| *k == "GITHUB_TOKEN") {
        pairs.push(("GITHUB_TOKEN", ""));
    }
    run_host_with(graph, label, |runtime| {
        runtime.secrets(MapSecrets::from_pairs(&pairs))
    })
    .await
}

#[derive(Default)]
struct WorkflowEvents {
    root_executions: Mutex<BTreeSet<ExecutionId>>,
    child_logs:      Mutex<Vec<String>>,
    lifecycle:       Mutex<Vec<CoordinatorRecord>>,
}

#[async_trait::async_trait]
impl ExecutionObserver for WorkflowEvents {
    fn on_engine_record(
        &self,
        execution: ExecutionId,
        record: &engine::EventRecord,
        _recorded_at: u64,
        _state: &engine::EngineState,
    ) {
        if !self
            .root_executions
            .lock()
            .expect("root executions lock")
            .contains(&execution)
            && let engine::Event::StepProgressRecorded {
                ev: ir::StepEvent::Log { line, .. },
                ..
            } = &record.event
        {
            self.child_logs
                .lock()
                .expect("workflow logs lock")
                .push(line.clone());
        }
    }

    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        if let CoordinatorEvent::ExecutionDeclared {
            execution,
            invocation: InvocationId::ROOT,
            ..
        } = record.body
        {
            self.root_executions
                .lock()
                .expect("root executions lock")
                .insert(execution);
        }
        self.lifecycle
            .lock()
            .expect("workflow lifecycle lock")
            .push(record.clone());
    }
}

/// Start a run, cancel it once `node` has started, and return the report.
///
/// The step must print something once it is under way (`echo ready && sleep
/// 30`): the cancel is triggered by its log file appearing. Replay
/// byte-identity is verified on the way out, cancellation included.
pub(crate) async fn run_host_then_cancel(
    graph: Graph,
    label: &str,
    node: &str,
) -> (RunReportPlus, ()) {
    let graph = with_params(graph);
    let original = graph.clone();
    let dir = run_dir(label);
    let driver = runtime(&dir).driver(graph);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    // Wait for the named step's log file to appear, then cancel.
    let deadline = Instant::now() + Duration::from_secs(20);
    let logs = dir.join("logs");
    let want = node.replace('/', "_");
    loop {
        let seen = fs::read_dir(&logs).is_ok_and(|rd| {
            rd.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(&want))
        });
        if seen || Instant::now() > deadline {
            break;
        }
        time::sleep(Duration::from_millis(50)).await;
    }
    time::sleep(Duration::from_millis(200)).await;
    handle.cancel(ir::CancelScopeId::ROOT).await;
    let report = run.await.expect("run finished");
    let _ = fs::remove_dir_all(&dir);
    testkit::assert_replay_identical(&original, &report);
    (RunReportPlus::from(report), ())
}

/// Assert the run succeeded — and when it did not, say everything a step
/// failure has to say. `state.errors()` is empty for an ordinary failed step
/// (engine errors are a different thing), so a bare status assertion shows a
/// failed run as `[]`: the records carry the failure's class and message, and
/// the log carries what the step printed. One dump, every battery.
#[allow(
    dead_code,
    reason = "every test binary compiles this module but uses only some helpers"
)]
pub(crate) fn assert_success(report: &RunReportPlus) {
    if report.status == ir::RunStatus::Success {
        return;
    }
    let records: Vec<String> = report
        .state
        .history()
        .iter()
        .map(|r| format!("{} → {:?}", r.name, r.outcome.status))
        .collect();
    panic!(
        "run ended {:?}\nengine errors: {:?}\nrecords: {records:#?}\nlog: {:#?}",
        report.status,
        report.state.errors(),
        log_lines(report)
    );
}

/// A report in the shape the tests read.
pub(crate) struct RunReportPlus {
    pub status:    ir::RunStatus,
    pub state:     engine::EngineState,
    pub commands:  Vec<engine::Command>,
    pub lifecycle: Vec<CoordinatorRecord>,
    child_logs:    Vec<String>,
}

impl From<ExecutionReport> for RunReportPlus {
    fn from(r: ExecutionReport) -> Self {
        Self {
            status:     r.status,
            state:      r.state,
            commands:   Vec::new(),
            lifecycle:  Vec::new(),
            child_logs: Vec::new(),
        }
    }
}

/// The nodes that actually ran their work. Every step node fires and evaluates
/// its gate inside the step kind, so `StepStarted` no longer separates ran from
/// self-skipped: the recorded status does. A step cancelled mid-run is not
/// listed either; a test that cares reads its status directly.
pub(crate) fn started(report: &RunReportPlus) -> Vec<String> {
    report
        .state
        .history()
        .iter()
        .filter(|h| !matches!(h.outcome.status.tag(), "skipped" | "cancelled"))
        .map(|h| h.name.to_string())
        .collect()
}

pub(crate) fn status_of(report: &RunReportPlus, name: &str) -> Option<String> {
    report
        .state
        .history()
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.outcome.status.tag().to_string())
}

pub(crate) fn log_lines(report: &RunReportPlus) -> Vec<String> {
    report
        .state
        .log
        .events()
        .filter_map(|e| match e {
            engine::Event::StepProgressRecorded {
                ev: ir::StepEvent::Log { line, .. },
                ..
            } => Some(line.clone()),
            _ => None,
        })
        .chain(report.child_logs.iter().cloned())
        .collect()
}
