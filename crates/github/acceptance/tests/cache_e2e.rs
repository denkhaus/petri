//! The cache round-trip, end to end: the real `actions/cache` (pinned) against
//! the per-run ObjectService's cache façade over a host-scoped store. Two
//! *runs* share the store — save in the first (the action's `post` phase),
//! restore in the second — which is the whole point of a cache: it outlives
//! the run that wrote it.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use github_actions::{
    ActionSource, ActionSourceCap, ActionTreeSource, GitActionSource, ResultsServiceCap,
};
use github_objects::ObjectService;
use runtime::executor::Retention;
use runtime::frontend::NoFiles;
use runtime::ir::Graph;
use runtime::{RunOptions, Runtime};
use support::*;

const CACHE: &str = "actions/cache@0057852bfaa89a56745cba8c7296529d2fc39830";

fn cache_workflow() -> String {
    format!(
        "on: push\n\
         jobs:\n\
         \x20 build:\n\
         \x20   runs-on: ubuntu-latest\n\
         \x20   steps:\n\
         \x20     - id: depot\n\
         \x20       uses: {CACHE}\n\
         \x20       with:\n\
         \x20         path: depot\n\
         \x20         key: probe-fixed-key\n\
         \x20     - run: test -f depot/seed.txt && echo cache-was-warm || echo cache-was-cold\n\
         \x20     - run: mkdir -p depot && echo seeded > depot/seed.txt\n"
    )
}

fn action_source() -> Arc<GitActionSource> {
    let cache = Path::new(env!("CARGO_MANIFEST_DIR")).join("../corpus/.actions-cache");
    Arc::new(GitActionSource::new(cache))
}

/// One run against a given cache-store dir; the artifact store stays per run.
async fn run_once(label: &str, store: &Path) -> Vec<String> {
    let source = action_source();
    let actions: Arc<dyn ActionSource> = source.clone() as _;
    let trees: Arc<dyn ActionTreeSource> = source;
    let lowered = frontend_gha::load_with(
        ".github/workflows/test.yml",
        &cache_workflow(),
        &NoFiles,
        Some(actions.as_ref()),
    );
    let graph: Graph = lowered.graph.expect("the workflow lowers");
    let graph = with_params(graph);

    // Canonical, or macOS's `/var` → `/private/var` symlink makes the cache
    // action compute a relative archive path that resolves nowhere.
    let dir = canonical_temp()
        .join("petri-cache-e2e")
        .join(format!("{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut options = RunOptions::new(&dir);
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    let cache_store = PathBuf::from(store);
    let report = Runtime::standard()
        .options(options)
        .step(github_actions::RunStep)
        .step(github_actions::ActionStep)
        .step(github_actions::DockerActionStep)
        .step(github_actions::CheckoutStep)
        .capability(ActionSourceCap(trees))
        .run_services(move |run_dir, caps| {
            match ObjectService::start(run_dir.join("artifacts"), cache_store.clone()) {
                Ok(service) => {
                    let cap = ResultsServiceCap {
                        port: service.port(),
                        token: service.token().into(),
                    };
                    (caps.provide(cap), Some(Box::new(service) as _))
                }
                Err(error) => {
                    eprintln!("warning: no results service: {error}");
                    (caps, None)
                }
            }
        })
        .run(graph)
        .await
        .expect("replay is byte-identical");
    let report = RunReportPlus::from(report);
    assert_eq!(
        report.status,
        runtime::ir::RunStatus::Success,
        "statuses: {:?}\nlog: {:?}",
        report
            .state
            .history()
            .iter()
            .map(|r| (r.name.to_string(), r.outcome.status.tag()))
            .collect::<Vec<_>>(),
        log_lines(&report)
    );
    let lines = log_lines(&report);
    let _ = std::fs::remove_dir_all(&dir);
    lines
}

/// Save in one run, restore in the next: the second run's workspace has the
/// first run's `depot/seed.txt` before any step recreated it.
#[tokio::test(flavor = "multi_thread")]
async fn a_cache_saved_by_one_run_restores_in_the_next() {
    if !node_ready() {
        return;
    }
    let store = canonical_temp()
        .join("petri-cache-e2e")
        .join(format!("store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store);

    let first = run_once("first", &store).await;
    assert!(
        first.iter().any(|l| l == "cache-was-cold"),
        "a fresh store misses: {first:?}"
    );

    let second = run_once("second", &store).await;
    assert!(
        second.iter().any(|l| l == "cache-was-warm"),
        "the second run restores what the first saved: {second:?}"
    );
    assert!(
        second
            .iter()
            .any(|l| l.contains("Cache restored from key: probe-fixed-key")),
        "the action reports the hit: {second:?}"
    );
    let _ = std::fs::remove_dir_all(&store);
}

fn canonical_temp() -> PathBuf {
    std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir())
}

fn node_ready() -> bool {
    let found = std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !found {
        eprintln!("skipping: no `node` on PATH");
    }
    found
}
