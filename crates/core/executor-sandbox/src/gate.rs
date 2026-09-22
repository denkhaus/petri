//! A run's admission to its sandbox capabilities.
//!
//! An environment and a one-shot runner hold their sandbox directly, so a
//! step or a host that keeps one after the run finishes could still reach
//! the sandbox. A plugin's transport closing used to be what stopped that; a
//! provider linked into this process has no transport to close. The router's
//! gate is the explicit boundary instead: every operation is admitted
//! through it, and [`RunGate::close`] stops new admissions, then waits a
//! bounded time for admitted work to settle.
//!
//! The gate stops access, not sandboxes. Lease release still ends the
//! sandboxes by retention before the run closes its gate.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use executor::EnvError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;

use crate::BACKEND;

/// Far more concurrent operations than one run makes; draining takes them
/// all, so it completes exactly when no admitted operation remains.
const PERMITS: u32 = 1 << 20;

/// How long closing waits for admitted operations before giving up on them.
pub(crate) const DRAIN_BUDGET: Duration = Duration::from_secs(10);

/// Admission to one run's sandboxes. Clones share the gate.
#[derive(Clone)]
pub(crate) struct RunGate {
    inner: Arc<Inner>,
}

struct Inner {
    closed:  AtomicBool,
    permits: Arc<Semaphore>,
}

/// One admitted operation. The gate cannot finish closing while it lives.
pub(crate) struct Admission {
    _permit: OwnedSemaphorePermit,
}

impl Default for RunGate {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                closed:  AtomicBool::new(false),
                permits: Arc::new(Semaphore::new(PERMITS as usize)),
            }),
        }
    }
}

impl RunGate {
    /// Admits one operation, or refuses once the run has closed its gate.
    pub(crate) async fn admit(&self, operation: &str) -> Result<Admission, EnvError> {
        let refused = || {
            EnvError::backend(
                BACKEND,
                operation,
                "the run has finished; its sandbox is no longer reachable",
            )
        };
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(refused());
        }
        let permit = Arc::clone(&self.inner.permits)
            .acquire_owned()
            .await
            .map_err(|_| refused())?;
        // Closing may have begun while this call waited; it must not start
        // work the drain has stopped waiting for.
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(refused());
        }
        Ok(Admission { _permit: permit })
    }

    /// Stops new admissions and waits up to `budget` for admitted
    /// operations to settle. Returns `false` when some were still running
    /// at the deadline: they finish on their own, and no new one starts. A
    /// second close returns `true` at once.
    pub(crate) async fn close(&self, budget: Duration) -> bool {
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return true;
        }
        let drained = timeout(
            budget,
            Arc::clone(&self.inner.permits).acquire_many_owned(PERMITS),
        )
        .await;
        self.inner.permits.close();
        matches!(drained, Ok(Ok(_)))
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::sleep;

    use super::*;

    #[tokio::test]
    async fn a_closed_gate_refuses_new_work() {
        let gate = RunGate::default();
        drop(gate.admit("read").await.expect("open gate admits"));
        assert!(gate.close(DRAIN_BUDGET).await);
        let error = gate.admit("read").await.err().expect("closed gate refuses");
        assert!(error.to_string().contains("finished"), "{error}");
        assert!(gate.close(DRAIN_BUDGET).await, "closing twice is safe");
    }

    #[tokio::test]
    async fn closing_waits_for_admitted_work() {
        let gate = RunGate::default();
        let admitted = gate.admit("exec").await.expect("admitted");
        let closing = tokio::spawn({
            let gate = gate.clone();
            async move { gate.close(DRAIN_BUDGET).await }
        });
        sleep(Duration::from_millis(20)).await;
        assert!(
            !closing.is_finished(),
            "an admitted operation holds the drain"
        );
        assert!(
            gate.admit("exec").await.is_err(),
            "no new admissions while draining"
        );
        drop(admitted);
        assert!(closing.await.expect("close task"));
    }

    #[tokio::test]
    async fn closing_gives_up_on_work_past_its_budget() {
        let gate = RunGate::default();
        let _stuck = gate.admit("exec").await.expect("admitted");
        assert!(!gate.close(Duration::from_millis(10)).await);
        assert!(gate.admit("exec").await.is_err());
    }
}
