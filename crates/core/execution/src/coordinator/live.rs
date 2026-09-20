//! The invocations between their dispatch and their release: one table, so
//! every step of that lifecycle is a named transition made in one place.

use std::collections::{BTreeMap, BTreeSet};

use super::leases::ExecutionLeases;
use crate::{ExecutionId, InvocationId};

/// Where a live invocation stands. It enters the table when it is queued or
/// dispatched and leaves it when its leases are released, or when it
/// restarts and is started again.
enum LiveInvocation {
    /// Declared under a fork gate and queued for a slot; only its driver
    /// waits, its declaration is durable already.
    Queued,
    /// Its driver runs. `leases` is what its isolated scopes acquire, for a
    /// child that inherits one; an inherited invocation acquires none.
    Running {
        execution: ExecutionId,
        handle:    driver::RunHandle,
        leases:    Option<ExecutionLeases>,
    },
    /// Its driver stopped. It completes once every live descendant has
    /// settled, and then restarts or has its result recorded.
    Stopped,
    /// Its result is durable and its leases are being released. A parent
    /// waits for this too, so a child's sandboxes are gone before the parent
    /// completes.
    Releasing,
}

/// The live invocations by id. The transitions keep the order the drive
/// loop relies on: a stopped driver is forgotten before its report is
/// checked, and an invocation stays live until its release is recorded.
#[derive(Default)]
pub(super) struct LiveInvocations(BTreeMap<InvocationId, LiveInvocation>);

impl LiveInvocations {
    /// No invocation is queued, running, stopped or releasing.
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn contains(&self, invocation: InvocationId) -> bool {
        self.0.contains_key(&invocation)
    }

    /// Every live invocation, in id order.
    pub(super) fn invocations(&self) -> impl Iterator<Item = InvocationId> + '_ {
        self.0.keys().copied()
    }

    /// The invocation waits on its fork gate for a slot.
    pub(super) fn queue(&mut self, invocation: InvocationId) {
        debug_assert!(
            !self.contains(invocation),
            "a queued invocation was not live"
        );
        self.0.insert(invocation, LiveInvocation::Queued);
    }

    /// The invocation's driver starts on `execution`, fresh or from the
    /// gate's queue.
    pub(super) fn run(
        &mut self,
        invocation: InvocationId,
        execution: ExecutionId,
        handle: driver::RunHandle,
        leases: Option<ExecutionLeases>,
    ) {
        debug_assert!(
            matches!(self.0.get(&invocation), None | Some(LiveInvocation::Queued)),
            "a dispatched invocation was queued or not live"
        );
        self.0.insert(invocation, LiveInvocation::Running {
            execution,
            handle,
            leases,
        });
    }

    /// The invocation's driver stopped: its handle and its scopes' leases
    /// are forgotten, and it stays live until it completes.
    pub(super) fn stop(&mut self, invocation: InvocationId, execution: ExecutionId) {
        debug_assert!(
            matches!(
                self.0.get(&invocation),
                Some(LiveInvocation::Running { execution: running, .. }) if *running == execution
            ),
            "a stopped driver was the invocation's running one"
        );
        self.0.insert(invocation, LiveInvocation::Stopped);
    }

    /// The invocation's result is durable; its leases are being released.
    pub(super) fn mark_releasing(&mut self, invocation: InvocationId) {
        debug_assert!(
            matches!(self.0.get(&invocation), Some(LiveInvocation::Stopped)),
            "a releasing invocation had stopped"
        );
        self.0.insert(invocation, LiveInvocation::Releasing);
    }

    /// Forget the invocation: its release is recorded, or it restarts and is
    /// started again.
    pub(super) fn remove(&mut self, invocation: InvocationId) {
        self.0.remove(&invocation);
    }

    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    /// Whether `execution` is the running execution of a live invocation.
    pub(super) fn is_running(&self, execution: ExecutionId) -> bool {
        self.handle_of(execution).is_some()
    }

    /// The driver handle of the invocation running `execution`.
    pub(super) fn handle_of(&self, execution: ExecutionId) -> Option<&driver::RunHandle> {
        self.running(execution).map(|(handle, _)| handle)
    }

    /// The leases the invocation running `execution` acquired for its
    /// isolated scopes.
    pub(super) fn leases_of(&self, execution: ExecutionId) -> Option<&ExecutionLeases> {
        self.running(execution).and_then(|(_, leases)| leases)
    }

    /// The driver handles of the running invocations among `invocations`.
    pub(super) fn handles_of(
        &self,
        invocations: &BTreeSet<InvocationId>,
    ) -> Vec<driver::RunHandle> {
        self.0
            .iter()
            .filter(|(invocation, _)| invocations.contains(invocation))
            .filter_map(|(_, live)| match live {
                LiveInvocation::Running { handle, .. } => Some(handle.clone()),
                LiveInvocation::Queued | LiveInvocation::Stopped | LiveInvocation::Releasing => {
                    None
                }
            })
            .collect()
    }

    fn running(
        &self,
        execution: ExecutionId,
    ) -> Option<(&driver::RunHandle, Option<&ExecutionLeases>)> {
        self.0.values().find_map(|live| match live {
            LiveInvocation::Running {
                execution: running,
                handle,
                leases,
            } if *running == execution => Some((handle, leases.as_ref())),
            LiveInvocation::Running { .. }
            | LiveInvocation::Queued
            | LiveInvocation::Stopped
            | LiveInvocation::Releasing => None,
        })
    }
}
