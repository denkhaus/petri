//! The cache round-trip, end to end: the real `actions/cache` (pinned) against
//! the per-run ObjectService's cache façade over a host-scoped store. Two
//! *runs* share the store — save in the first (the action's `post` phase),
//! restore in the second — which is the whole point of a cache: it outlives
//! the run that wrote it.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{env, fs, process};

use github_actions::{ActionSourceCap, ActionTreeSource};
use runtime::ir::RunStatus;
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

/// One run against a given cache-store dir; the artifact store stays per run.
async fn run_once(label: &str, store: &Path) -> Vec<String> {
    let source = corpus_action_source();
    let graph = lower_with_actions(&cache_workflow(), &source);
    let cache_store = PathBuf::from(store);
    let report = run_host_with(graph, &format!("cache-{label}"), |rt| {
        let trees: Arc<dyn ActionTreeSource> = source;
        with_object_service(rt.capability(ActionSourceCap(trees)), Some(cache_store))
    })
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "statuses: {:?}\nlog: {:?}",
        report
            .state
            .history()
            .iter()
            .map(|r| (r.name.to_string(), r.outcome.status.tag()))
            .collect::<Vec<_>>(),
        log_lines(&report)
    );
    log_lines(&report)
}

/// Save in one run, restore in the next: the second run's workspace has the
/// first run's `depot/seed.txt` before any step recreated it.
#[tokio::test(flavor = "multi_thread")]
async fn a_cache_saved_by_one_run_restores_in_the_next() {
    if !tool_ready("node") {
        return;
    }
    let store = canonical_temp()
        .join("petri-cache-e2e")
        .join(format!("store-{}", process::id()));
    let _ = fs::remove_dir_all(&store);

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
    let _ = fs::remove_dir_all(&store);
}

/// The store rides the canonical temp path for the same reason the run dir
/// does: macOS's `/var` → `/private/var` symlink breaks the cache action's
/// relative-path math.
fn canonical_temp() -> PathBuf {
    env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| env::temp_dir())
}
