use std::collections::BTreeMap;

use ir::{Control, Value};
use smol_str::SmolStr;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    CallSite, GraphDigest, InvocationId, InvocationResult, InvocationStatus, SandboxMode,
    SecretBindings,
};

#[derive(Clone, Debug, PartialEq)]
pub struct InvocationRequest {
    pub site:    CallSite,
    pub graph:   GraphDigest,
    pub context: BTreeMap<SmolStr, Value>,
    pub secrets: SecretBindings,
    pub sandbox: SandboxMode,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvokeError {
    #[error("unknown invocation graph {0}")]
    UnknownGraph(GraphDigest),
    #[error("the invocation limit has been reached")]
    InvocationLimit,
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
    cancel: mpsc::UnboundedSender<InvocationId>,
}

impl InvocationHandle {
    /// A handle over a status channel and a cancel channel. The coordinator
    /// builds these; a host or a test that implements [`InvocationClient`]
    /// itself builds them the same way.
    pub fn new(
        id: InvocationId,
        status: watch::Receiver<InvocationStatus>,
        cancel: mpsc::UnboundedSender<InvocationId>,
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
    /// deliveries do not complete the call. `None` means cancellation was
    /// requested; the coordinator still owns the child's shutdown.
    pub async fn result_with_control(
        &mut self,
        control: &mut mpsc::Receiver<Control>,
    ) -> Option<InvocationResult> {
        loop {
            tokio::select! {
                result = self.result() => return Some(result),
                message = control.recv() => {
                    if !matches!(message, Some(Control::Deliver(_))) {
                        self.cancel().await;
                        return None;
                    }
                },
            }
        }
    }

    #[expect(
        clippy::unused_async,
        reason = "the public handle contract keeps cancellation awaitable across implementations"
    )]
    pub async fn cancel(&self) {
        let _ = self.cancel.send(self.id);
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
