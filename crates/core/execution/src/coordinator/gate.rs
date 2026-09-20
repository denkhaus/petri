//! Fork gates: the slots a fork's children share, and the queue of declared
//! children waiting for one before their driver starts.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use smol_str::SmolStr;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::{AbortHandle, JoinSet};

use super::{CompletedExecution, Coordinator, CoordinatorError};
use crate::{ExecutionId, InvocationId};

/// A fork gate's identity: the parent execution and the gate name its
/// children were declared under.
pub(super) type GateKey = (ExecutionId, SmolStr);

/// One fork's admission: the slots its children share, and the declared
/// children waiting for a slot before their driver starts, in declaration
/// order. A child holds its slot from dispatch to the end of its driver,
/// except across a retry backoff, so a fork never has more live children
/// than slots, plus the children waiting out a backoff.
pub(super) struct ForkGate {
    slots:  Arc<Semaphore>,
    queued: VecDeque<InvocationId>,
    /// The task waiting for the next free slot while `queued` is not empty.
    waiter: Option<AbortHandle>,
}

impl ForkGate {
    fn new(max_parallel: u32) -> Self {
        let limit = usize::try_from(max_parallel.max(1)).unwrap_or(usize::MAX);
        Self {
            slots:  Arc::new(Semaphore::new(limit)),
            queued: VecDeque::new(),
            waiter: None,
        }
    }

    /// Wait for the next free slot while children are queued; at most one
    /// wait at a time. The slot comes back on the coordinator's admission
    /// channel, for the child at the head of the queue.
    fn wait_for_slot(&mut self, key: GateKey, admit: mpsc::UnboundedSender<AdmittedSlot>) {
        if self.waiter.is_some() || self.queued.is_empty() {
            return;
        }
        let slots = self.slots.clone();
        let waiter = tokio::spawn(async move {
            let Ok(permit) = slots.acquire_owned().await else {
                return;
            };
            let _ = admit.send(AdmittedSlot { gate: key, permit });
        });
        self.waiter = Some(waiter.abort_handle());
    }
}

impl Drop for ForkGate {
    fn drop(&mut self) {
        if let Some(waiter) = &self.waiter {
            waiter.abort();
        }
    }
}

/// A free slot of one fork gate, taken for the next queued child.
pub(super) struct AdmittedSlot {
    gate:   GateKey,
    permit: OwnedSemaphorePermit,
}

impl Coordinator {
    /// Queue a fork's child for a slot of its gate, creating the gate with
    /// the first child that names it; the gate waits for the next free slot
    /// unless it is waiting already.
    pub(super) fn queue_for_slot(
        &mut self,
        invocation: InvocationId,
        key: GateKey,
        max_parallel: u32,
    ) {
        let gate = self
            .gates
            .entry(key.clone())
            .or_insert_with(|| ForkGate::new(max_parallel));
        gate.queued.push_back(invocation);
        gate.wait_for_slot(key, self.admit_tx.clone());
    }

    /// A fork gate's waiter took a slot: the child at the head of the queue
    /// starts on it, and the gate waits again for the next one.
    pub(super) async fn dispatch_admitted(
        &mut self,
        admitted: AdmittedSlot,
        running: &mut JoinSet<CompletedExecution>,
    ) -> Result<(), CoordinatorError> {
        let AdmittedSlot { gate: key, permit } = admitted;
        let Some(gate) = self.gates.get_mut(&key) else {
            return Ok(());
        };
        gate.waiter = None;
        let Some(invocation) = gate.queued.pop_front() else {
            return Ok(());
        };
        gate.wait_for_slot(key, self.admit_tx.clone());
        self.dispatch_invocation(invocation, Some(permit), running)
            .await
    }

    /// The fork gate an invocation was declared under, and its slot count,
    /// when its declaration bounds concurrency.
    pub(super) fn fork_admission(&self, invocation: InvocationId) -> Option<(GateKey, u32)> {
        let declaration = &self.store.state().invocations[&invocation].declaration;
        let admission = declaration.admission.as_ref()?;
        let parent = declaration.call.as_ref()?.parent;
        Some(((parent, admission.gate.clone()), admission.max_parallel))
    }

    /// The slots an invocation's driver shares with its fork's other
    /// children: one semaphore per parent execution and gate name, created
    /// with the first child that names it.
    pub(super) fn attempt_slots(&mut self, invocation: InvocationId) -> Option<Arc<Semaphore>> {
        let (key, max_parallel) = self.fork_admission(invocation)?;
        Some(
            self.gates
                .entry(key)
                .or_insert_with(|| ForkGate::new(max_parallel))
                .slots
                .clone(),
        )
    }

    /// Drop every fork gate whose parent execution is no longer live and
    /// that has no child queued or live. A gate still in use keeps its
    /// slots, so the bound holds for children that settle after their
    /// parent, and a child that restarts rejoins the same gate.
    pub(super) fn drop_idle_gates(&mut self) {
        let in_use: BTreeSet<GateKey> = self
            .live
            .invocations()
            .filter_map(|invocation| self.fork_admission(invocation).map(|(key, _)| key))
            .collect();
        let live = &self.live;
        self.gates.retain(|key, gate| {
            live.is_running(key.0) || !gate.queued.is_empty() || in_use.contains(key)
        });
    }
}
