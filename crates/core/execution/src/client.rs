use std::collections::BTreeMap;

use ir::{Control, Value};
use smol_str::SmolStr;
use tokio::sync::{mpsc, oneshot, watch};

use crate::event::CancelRequest;
use crate::{
    AttemptAdmission, CallSite, ExecutionId, GraphDigest, InvocationId, InvocationResult,
    InvocationStatus, SandboxMode, SecretBindings,
};

#[derive(Clone, Debug, PartialEq)]
pub struct InvocationRequest {
    pub site:      CallSite,
    pub graph:     GraphDigest,
    pub context:   BTreeMap<SmolStr, Value>,
    pub secrets:   SecretBindings,
    pub sandbox:   SandboxMode,
    /// Bounded attempt concurrency for the child's steps, shared with every
    /// sibling the same parent execution declares under the same gate name.
    /// `None` leaves the child's attempts unbounded.
    pub admission: Option<AttemptAdmission>,
}

impl InvocationRequest {
    /// A request with no attempt admission bound.
    pub fn new(
        site: CallSite,
        graph: GraphDigest,
        context: BTreeMap<SmolStr, Value>,
        secrets: SecretBindings,
        sandbox: SandboxMode,
    ) -> Self {
        Self {
            site,
            graph,
            context,
            secrets,
            sandbox,
            admission: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvokeError {
    #[error("unknown invocation graph {0}")]
    UnknownGraph(GraphDigest),
    /// The run declared as many invocations as it may. `total` counts every
    /// invocation ever declared in the run, finished ones included; `parent`
    /// and `slot` name the call that was refused.
    #[error(
        "the run-wide invocation limit is reached: {total} of {limit} invocations are declared, \
         so execution {parent} firing {firing} cannot start `{slot}`"
    )]
    InvocationLimit {
        total:  u64,
        limit:  u32,
        parent: ExecutionId,
        firing: ir::FiringId,
        slot:   SmolStr,
    },
    #[error("the call site is already attached to a different invocation request")]
    RequestMismatch,
    #[error("the calling firing has no inheritable sandbox")]
    NoInheritableSandbox,
    /// The child graph declares a container for one of its scopes that is
    /// not the inherited sandbox's. An inherited child runs in its caller's
    /// sandbox; a child that needs a different image uses an isolated
    /// binding.
    #[error(
        "the invocation inherits its caller's sandbox but declares a different container for \
         scope {scope}"
    )]
    InheritedContainerMismatch { scope: ir::ScopeId },
    #[error("the invocation coordinator is unavailable")]
    CoordinatorUnavailable,
    #[error("invocation failed before it could return a result: {0}")]
    Coordinator(SmolStr),
}

#[async_trait::async_trait]
pub trait InvocationClient: Send + Sync {
    async fn start_or_attach(
        &self,
        request: InvocationRequest,
    ) -> Result<InvocationHandle, InvokeError>;
}

pub struct InvocationHandle {
    id:     InvocationId,
    status: watch::Receiver<InvocationStatus>,
    cancel: mpsc::UnboundedSender<CancelRequest>,
}

impl InvocationHandle {
    /// A handle over a status channel and a cancel channel. The coordinator
    /// builds these; a host or a test that implements [`InvocationClient`]
    /// itself builds them the same way.
    pub fn new(
        id: InvocationId,
        status: watch::Receiver<InvocationStatus>,
        cancel: mpsc::UnboundedSender<CancelRequest>,
    ) -> Self {
        Self { id, status, cancel }
    }

    pub fn id(&self) -> InvocationId {
        self.id
    }

    pub async fn result(&mut self) -> InvocationResult {
        loop {
            if let InvocationStatus::Finished(result) = self.status.borrow().clone() {
                return result;
            }
            self.status
                .changed()
                .await
                .expect("the coordinator keeps invocation status open until Finished");
        }
    }

    /// Wait for the child, forwarding a parent cancellation or kill. Steering
    /// deliveries do not complete the call. `None` means the parent was told
    /// to stop; the coordinator still owns the child's shutdown.
    pub async fn result_with_control(
        &mut self,
        control: &mut mpsc::Receiver<Control>,
    ) -> Option<InvocationResult> {
        self.settled_with_control(control).await.ok()
    }

    /// Wait for the child's result, forwarding a parent stop. `Err` carries
    /// the stop the parent was told (`Cancel` or `Kill`), already forwarded
    /// to the child; the coordinator still owns the child's shutdown, and a
    /// later wait returns the child's result once it settles. Steering
    /// deliveries do not end the wait.
    pub async fn settled_with_control(
        &mut self,
        control: &mut mpsc::Receiver<Control>,
    ) -> Result<InvocationResult, Control> {
        loop {
            // A child that has finished is finished: its result wins over a
            // stop that arrived beside it.
            tokio::select! {
                biased;
                result = self.result() => return Ok(result),
                message = control.recv() => {
                    if let Some(stop) = stop_of(message) {
                        self.forward(&stop).await;
                        return Err(stop);
                    }
                },
            }
        }
    }

    /// Wait until the child's engine has started (it holds a slot under its
    /// gate and its execution is declared) or the child finished, forwarding
    /// a parent stop as [`Self::settled_with_control`] does. A child whose
    /// start the parent never saw before a stop never started from the
    /// parent's point of view, even though the coordinator still runs its
    /// cancelled execution to record the cancellation.
    pub async fn started_with_control(
        &mut self,
        control: &mut mpsc::Receiver<Control>,
    ) -> ChildStart {
        loop {
            match self.status.borrow().clone() {
                InvocationStatus::Running { .. } => return ChildStart::Started,
                InvocationStatus::Finished(result) => return ChildStart::Finished(result),
                InvocationStatus::Declared => {}
            }
            tokio::select! {
                biased;
                changed = self.status.changed() => {
                    changed.expect("the coordinator keeps invocation status open until Finished");
                }
                message = control.recv() => {
                    if let Some(stop) = stop_of(message) {
                        self.forward(&stop).await;
                        return ChildStart::Stopped(stop);
                    }
                },
            }
        }
    }

    /// Forward a stop the parent was told: a polite cancel as a cancel, a
    /// kill as an escalation.
    async fn forward(&self, stop: &Control) {
        if matches!(stop, Control::Kill) {
            self.kill().await;
        } else {
            self.cancel().await;
        }
    }

    /// Politely cancel the child. A child the coordinator has already
    /// cancelled (its cascade reaches every descendant of a cancelled
    /// invocation) is left alone, so a parent forwarding its own cancel never
    /// escalates the child to a kill.
    #[expect(
        clippy::unused_async,
        reason = "the public handle contract keeps cancellation awaitable across implementations"
    )]
    pub async fn cancel(&self) {
        self.request_stop(false);
    }

    /// Escalate the child to the kill tier: the request reaches the child's
    /// driver even when the child is already cancelled, which is what makes
    /// the driver kill. A child not yet cancelled is cancelled by it.
    #[expect(
        clippy::unused_async,
        reason = "the public handle contract keeps cancellation awaitable across implementations"
    )]
    pub async fn kill(&self) {
        self.request_stop(true);
    }

    fn request_stop(&self, escalate: bool) {
        let _ = self.cancel.send(CancelRequest {
            invocation: self.id,
            reason: None,
            escalate,
        });
    }
}

/// How a wait for a child's start ended.
#[derive(Debug)]
pub enum ChildStart {
    /// The child's engine started.
    Started,
    /// The child finished before its start was observed.
    Finished(InvocationResult),
    /// The parent was told to stop first (`Cancel` or `Kill`); the stop was
    /// forwarded to the child.
    Stopped(Control),
}

/// The stop a control message carries, if any: a steering delivery is not a
/// stop, and a closed control channel is treated as a polite cancel.
fn stop_of(message: Option<Control>) -> Option<Control> {
    match message {
        Some(Control::Deliver(_)) => None,
        Some(control) => Some(control),
        None => Some(Control::Cancel),
    }
}

pub(crate) struct StartRequest {
    pub parent:  crate::ExecutionId,
    pub request: InvocationRequest,
    pub reply:   oneshot::Sender<Result<InvocationHandle, InvokeError>>,
}

#[derive(Clone)]
pub struct CoordinatorInvocationClient {
    parent: crate::ExecutionId,
    start:  mpsc::Sender<StartRequest>,
}

impl CoordinatorInvocationClient {
    pub(crate) fn new(parent: crate::ExecutionId, start: mpsc::Sender<StartRequest>) -> Self {
        Self { parent, start }
    }
}

#[async_trait::async_trait]
impl InvocationClient for CoordinatorInvocationClient {
    async fn start_or_attach(
        &self,
        request: InvocationRequest,
    ) -> Result<InvocationHandle, InvokeError> {
        let (reply, result) = oneshot::channel();
        self.start
            .send(StartRequest {
                parent: self.parent,
                request,
                reply,
            })
            .await
            .map_err(|_| InvokeError::CoordinatorUnavailable)?;
        result
            .await
            .map_err(|_| InvokeError::CoordinatorUnavailable)?
    }
}
