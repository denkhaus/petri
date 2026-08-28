//! The per-run service seam: a provisioner runs once per driver with the run
//! directory, its capability reaches the run's steps, and its guard — the
//! running service — is dropped when the run is over. Teardown is drop, so
//! "the service dies with the run" is structural, not a callback anyone can
//! forget.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use runtime::ir::{GraphBuilder, RunStatus, ScopeId};
use runtime::{RunOptions, Runtime};

/// The capability a provisioned service hands the run.
struct ProbeCap;

/// The "service": alive until the driver drops it.
struct ProbeGuard(Arc<AtomicBool>);

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn provision_runs_per_driver_and_the_guard_dies_with_the_run() {
    let dir = std::env::temp_dir().join(format!("petri-run-services-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let provisions = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let seen_dir = dir.clone();
    let counter = Arc::clone(&provisions);
    let flag = Arc::clone(&dropped);

    let rt = Runtime::standard()
        .options(RunOptions::new(&dir))
        .run_services(move |run_dir, caps| {
            assert_eq!(run_dir, seen_dir, "the provisioner sees the run dir");
            counter.fetch_add(1, Ordering::SeqCst);
            (
                caps.provide(ProbeCap),
                Some(Box::new(ProbeGuard(Arc::clone(&flag))) as _),
            )
        });

    let mut b = GraphBuilder::bare();
    let scope = b.add_scope(runtime::ir::Scope::new(ScopeId::new(0)));
    b.add_step("only", scope, "noop");
    let graph = b.build();

    let report = rt.run(graph).await.expect("replay is byte-identical");
    assert_eq!(report.status, RunStatus::Success);
    assert!(
        provisions.load(Ordering::SeqCst) >= 1,
        "the provisioner ran"
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "the guard dropped when the run ended"
    );
    // The capability type itself is exercised end to end by the GitHub
    // component's batteries; here the seam's mechanics are the subject.
    let _ = ProbeCap;
    let _ = std::fs::remove_dir_all(&dir);
}
