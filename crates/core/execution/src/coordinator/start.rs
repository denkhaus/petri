//! What a start request from a live driver attaches to: an invocation it
//! already declared, a superseded attempt to wait out, or a new child.

use std::fmt;

use smol_str::SmolStr;
use tokio::sync::watch;

use super::{Coordinator, CoordinatorError};
use crate::client::StartRequest;
use crate::{
    CancelRequest, CoordinatorEvent, InvocationHandle, InvocationId, InvocationStatus, InvokeError,
    ParentCallKey, SandboxBinding, SandboxMode,
};

/// What a start request resolved to.
enum StartOutcome {
    /// Wait for a superseded prior attempt to settle, then requeue the
    /// request.
    Requeue { previous: InvocationId },
    /// Attach to this invocation — newly declared, or found by its call key.
    Attach {
        invocation: InvocationId,
        is_new:     bool,
    },
}

/// A coordinator failure rendered for the invoking step.
pub(super) fn invoke_error(error: impl fmt::Display) -> InvokeError {
    InvokeError::Coordinator(SmolStr::new(error.to_string()))
}

impl Coordinator {
    pub(super) async fn handle_start(
        &mut self,
        request: StartRequest,
    ) -> Result<Option<InvocationId>, CoordinatorError> {
        // A completed driver cannot own a new child. Requests whose callers
        // disappeared can still be queued when that driver's report arrives.
        if request.reply.is_closed() || !self.live.is_running(request.parent) {
            let _ = request.reply.send(Err(InvokeError::CoordinatorUnavailable));
            return Ok(None);
        }

        let key = ParentCallKey {
            parent:  request.parent,
            firing:  request.request.site.firing,
            attempt: request.request.site.attempt,
            slot:    request.request.site.slot.clone(),
        };
        match self.resolve_start(&request, &key).await {
            Err(error) => {
                let _ = request.reply.send(Err(error));
                Ok(None)
            }
            Ok(StartOutcome::Requeue { previous }) => {
                // The request re-enters the queue once the superseded attempt
                // settles; the reply travels with it.
                let sender = self
                    .statuses
                    .entry(previous)
                    .or_insert_with(|| watch::channel(InvocationStatus::Declared).0);
                let mut status = sender.subscribe();
                let starts = self.start_tx.clone();
                tokio::spawn(async move {
                    loop {
                        if matches!(*status.borrow(), InvocationStatus::Finished(_)) {
                            let _ = starts.send(request).await;
                            break;
                        }
                        if status.changed().await.is_err() {
                            break;
                        }
                    }
                });
                Ok((!self.live.contains(previous)).then_some(previous))
            }
            Ok(StartOutcome::Attach { invocation, is_new }) => {
                let sender = self.statuses.entry(invocation).or_insert_with(|| {
                    let status = self.store.state().invocations[&invocation]
                        .result
                        .clone()
                        .map_or(InvocationStatus::Declared, InvocationStatus::Finished);
                    watch::channel(status).0
                });
                let handle =
                    InvocationHandle::new(invocation, sender.subscribe(), self.cancel_tx.clone());
                let reply_delivered = request.reply.send(Ok(handle)).is_ok();
                let incomplete = self.store.state().invocations[&invocation].result.is_none();
                let parent = self.store.state().executions[&request.parent]
                    .declaration
                    .invocation;
                if incomplete
                    && !self.store.state().invocations[&invocation].cancelled
                    && (!reply_delivered || self.store.state().invocations[&parent].cancelled)
                {
                    self.cancel_invocations(vec![(invocation, false)], invocation, None, false)
                        .await?;
                }
                Ok(
                    (incomplete && !self.live.contains(invocation) && (is_new || reply_delivered))
                        .then_some(invocation),
                )
            }
        }
    }

    /// Decide what a start request attaches to. Every rejection comes back as
    /// the error; `handle_start` owns the one reply send.
    async fn resolve_start(
        &mut self,
        request: &StartRequest,
        key: &ParentCallKey,
    ) -> Result<StartOutcome, InvokeError> {
        if !self.store.state().graphs.contains(&request.request.graph) {
            return Err(InvokeError::UnknownGraph(request.request.graph));
        }
        self.refuse_secret(&request.request.context)
            .map_err(invoke_error)?;

        if let Some(invocation) = self.store.state().calls.get(key).copied() {
            let declaration = &self.store.state().invocations[&invocation].declaration;
            // A kind-match only: when the parent invocation is itself
            // inherited, the child reuses the parent's lease, whose scope
            // belongs to an ancestor's graph rather than the caller's.
            let sandbox_matches = matches!(
                (request.request.sandbox, declaration.sandbox),
                (SandboxMode::Isolated, SandboxBinding::Isolated)
                    | (
                        SandboxMode::Inherit { .. },
                        SandboxBinding::Inherited { .. }
                    )
            );
            if declaration.graph != request.request.graph
                || declaration.context != request.request.context
                || declaration.secret_bindings != request.request.secrets
                || declaration.admission != request.request.admission
                || !sandbox_matches
            {
                return Err(InvokeError::RequestMismatch);
            }
            return Ok(StartOutcome::Attach {
                invocation,
                is_new: false,
            });
        }

        let previous = self
            .store
            .state()
            .calls
            .iter()
            .filter(|(candidate, _)| {
                candidate.parent == key.parent
                    && candidate.firing == key.firing
                    && candidate.slot == key.slot
            })
            .max_by_key(|(candidate, _)| candidate.attempt)
            .map(|(candidate, invocation)| (candidate.clone(), *invocation));
        if let Some((previous_key, previous)) = previous {
            if previous_key.attempt > key.attempt {
                return Err(InvokeError::RequestMismatch);
            }
            if self.store.state().invocations[&previous].result.is_none() {
                self.handle_cancel(CancelRequest {
                    invocation: previous,
                    reason:     None,
                    escalate:   false,
                })
                .await
                .map_err(invoke_error)?;
                return Ok(StartOutcome::Requeue { previous });
            }
        }
        let total = self.store.state().invocations.len() as u64;
        if total >= u64::from(self.options.max_invocations) {
            return Err(InvokeError::InvocationLimit {
                total,
                limit: self.options.max_invocations,
                parent: key.parent,
                firing: key.firing,
                slot: key.slot.clone(),
            });
        }
        let invocation = self.store.state().next_invocation_id();
        let sandbox = match request.request.sandbox {
            SandboxMode::Isolated => SandboxBinding::Isolated,
            SandboxMode::Inherit { scope } => match self.inherited_binding(key, scope).await {
                Ok(binding) => binding,
                Err(CoordinatorError::NoInheritableSandbox) => {
                    return Err(InvokeError::NoInheritableSandbox);
                }
                Err(error) => return Err(invoke_error(error)),
            },
        };
        if let SandboxBinding::Inherited { lease } = sandbox {
            self.check_inherited_container(lease, request.request.graph)
                .await?;
        }
        self.append(CoordinatorEvent::InvocationDeclared {
            invocation,
            call: Some(key.clone()),
            graph: request.request.graph,
            context: request.request.context.clone(),
            secret_bindings: request.request.secrets.clone(),
            sandbox,
            admission: request.request.admission.clone(),
        })
        .await
        .map_err(invoke_error)?;
        Ok(StartOutcome::Attach {
            invocation,
            is_new: true,
        })
    }
}
