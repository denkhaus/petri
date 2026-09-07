//! The host-facing hook service: one interface a local hook executor and an
//! embedding host both implement, and one adapter that calls it from the
//! driver's awaited extension points, so a configured hook runs exactly once
//! whoever serves it.
//!
//! The split is deliberate. A [`HookService`] answers "run the hooks
//! configured for this point, on this subject, and tell me the decision".
//! Which hooks are configured, how a command hook's exit code becomes a
//! decision, and what a timeout means are the service's business: the local
//! implementation reads the workflow's configuration; a platform host reads
//! its own. The [`HookAdapter`] is the one caller: it implements
//! [`ExecutionHooks`] and asks the service at each point, records the
//! service's report as a `hook` note, and turns the decision into the
//! driver's admission, result, or transition answer. Nothing else in Petri
//! calls a hook service for a workflow point, so installing one service
//! means each hook executes once.
//!
//! Tool-boundary points (`BeforeToolUse`, `AfterToolUse`, `AfterToolFailure`)
//! are not driven by the adapter: the agent backend's tool middleware reaches
//! the same service through the [`HookServiceHandle`] capability and asks at
//! the actual tool boundary. The service stays the single owner of the
//! decision either way.

use std::sync::Arc;

use driver::FiringView;
use driver::lifecycle::{
    AdmitAttempt, AttemptDecision, ExecutionHooks, Note, PrepareError, PrepareResult, Prepared,
    Recorded, RouteOverride, RunFinished, ScopeReleased, Transition, TransitionError,
    TransitionReport,
};
use engine::{Admission, RouteDecision};
use ir::{EdgeId, Outcome, Status, Value};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

/// The note kind the adapter records for every service report.
pub const HOOK_NOTE_KIND: &str = "hook";

/// Where in a run a hook may be configured. Names describe the point, not
/// any one workflow format's spelling of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPoint {
    /// Before the first attempt of a visit is dispatched.
    BeforeVisit,
    /// Before every attempt, retries included.
    BeforeAttempt,
    /// After an attempt returned, before its record; the decision may adjust
    /// the effective result.
    AfterAttempt,
    /// After a visit's final record, before routing.
    AfterVisit,
    /// After routes were selected, before advancement; the decision may
    /// override or block.
    RouteSelected,
    /// Before a retry attempt is scheduled.
    Retrying,
    /// A fork's branches are about to start.
    ForkStarted,
    /// A fork's branches have all completed.
    ForkCompleted,
    /// The run ended: its status is final, no environment is released yet.
    /// A run-level point: the request carries no firing view; its payload is
    /// [`RunFinishedPayload`].
    RunFinished,
    /// A scope's environment is about to be released. A run-level point: no
    /// firing view; the payload is [`ScopeReleasedPayload`].
    ScopeReleased,
    /// An agent tool is about to run. Served at the tool boundary.
    BeforeToolUse,
    /// An agent tool returned. Served at the tool boundary.
    AfterToolUse,
    /// An agent tool failed. Served at the tool boundary.
    AfterToolFailure,
}

/// What a hook decided, in the vocabulary the driver can act on. A service
/// returns only the variants its point consumes; the adapter ignores a
/// decision a point cannot take and records that it did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum HookDecision {
    Proceed,
    /// Skip the visit with this status (`BeforeVisit`, `BeforeAttempt`).
    Skip {
        status: Status,
    },
    /// Block: the attempt fails (`BeforeVisit`, `BeforeAttempt`) or the
    /// transition is refused (`RouteSelected`).
    Block {
        reason: String,
    },
    /// Replace a routing group's selected edge (`RouteSelected`).
    Override {
        group: u32,
        edge:  EdgeId,
    },
    /// Make this status the effective result (`AfterAttempt`).
    Adjust {
        status: Status,
        reason: String,
    },
}

/// One hook the service ran, or could not run, for the record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookRun {
    pub name:        String,
    /// `executed`, `skipped`, `failed_open`, `unsupported`.
    pub state:       SmolStr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message:     Option<String>,
}

/// The service's answer for one point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookReport {
    pub point:    HookPoint,
    pub decision: HookDecision,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks:    Vec<HookRun>,
    /// Problems that fail open: recorded, execution continues.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl HookReport {
    pub fn proceed(point: HookPoint) -> Self {
        Self {
            point,
            decision: HookDecision::Proceed,
            hooks: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Whether anything happened worth a record.
    pub fn is_silent(&self) -> bool {
        self.decision == HookDecision::Proceed && self.hooks.is_empty() && self.warnings.is_empty()
    }
}

/// The payload of a [`HookPoint::RunFinished`] request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFinishedPayload {
    pub status:  ir::RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// The payload of a [`HookPoint::ScopeReleased`] request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeReleasedPayload {
    pub scope:   ir::ScopeId,
    /// `succeeded` or `failed`: the scope outcome retention is decided on.
    pub outcome: SmolStr,
}

/// What a service is asked about.
pub struct HookRequest {
    pub point:   HookPoint,
    /// The firing the point belongs to. `None` for the run-level points
    /// (`RunFinished`, `ScopeReleased`), which have no firing.
    pub view:    Option<Arc<FiringView>>,
    /// The attempt's outcome (`AfterAttempt`, `AfterVisit`, `RouteSelected`).
    pub outcome: Option<Outcome>,
    /// The selected routes (`RouteSelected`), one decision per group.
    pub routes:  Vec<(u32, RouteDecision)>,
    /// Point-specific data a caller at the tool boundary supplies: the tool
    /// name and input, the result. Opaque here.
    pub payload: Value,
}

/// The one interface a hook executor implements. The local implementation
/// runs configured command, HTTP, prompt and agent hooks; an embedding host
/// runs its own. Exactly one service is installed per run.
///
/// Implementations must return within the point's timeout policy, apply
/// their own fail-open rules, and never call back into the run. Cancellation
/// of the run cancels the awaiting caller; the service should stop its own
/// work when the future is dropped.
#[async_trait::async_trait]
pub trait HookService: Send + Sync {
    async fn run(&self, request: HookRequest) -> HookReport;
}

/// The capability type steps and tool middleware look the service up by.
pub struct HookServiceHandle(pub Arc<dyn HookService>);

/// A service with no hooks: every point proceeds. The standalone default.
pub struct NoHooks;

#[async_trait::async_trait]
impl HookService for NoHooks {
    async fn run(&self, request: HookRequest) -> HookReport {
        HookReport::proceed(request.point)
    }
}

/// The one caller of a [`HookService`] for workflow points.
pub struct HookAdapter {
    service: Arc<dyn HookService>,
}

impl HookAdapter {
    pub fn new(service: Arc<dyn HookService>) -> Self {
        Self { service }
    }

    fn note(report: &HookReport) -> Option<Note> {
        if report.is_silent() {
            return None;
        }
        Some(Note::new(
            HOOK_NOTE_KIND,
            serde_json::to_value(report).unwrap_or(Value::Null),
        ))
    }
}

/// Whether a node is a lowering artifact rather than a stage: the frontend
/// marked it `synthetic` (a goal check, a parallel branch's delegate node, a
/// synthetic fan-in). No hook point fires for one; its stage, when it has
/// one, runs elsewhere with hooks of its own.
fn is_synthetic(view: &FiringView) -> bool {
    view.meta().get("synthetic") == Some(&Value::Bool(true))
}

#[async_trait::async_trait]
impl ExecutionHooks for HookAdapter {
    async fn before_attempt(&self, request: AdmitAttempt) -> AttemptDecision {
        if is_synthetic(&request.view) {
            return AttemptDecision::admit();
        }
        let mut notes = Vec::new();
        let mut admission = Admission::Admit;
        let points = if request.view.attempt == ir::Attempt::FIRST {
            vec![HookPoint::BeforeVisit, HookPoint::BeforeAttempt]
        } else {
            vec![HookPoint::Retrying, HookPoint::BeforeAttempt]
        };
        for point in points {
            let report = self
                .service
                .run(HookRequest {
                    point,
                    view: Some(request.view.clone()),
                    outcome: None,
                    routes: Vec::new(),
                    payload: Value::Null,
                })
                .await;
            notes.extend(Self::note(&report));
            match report.decision {
                HookDecision::Skip { status } => {
                    admission = Admission::Skip {
                        outcome: Outcome::new(status, Value::Null),
                    };
                    break;
                }
                HookDecision::Block { reason } => {
                    admission = Admission::Block {
                        reason: SmolStr::new(reason),
                    };
                    break;
                }
                HookDecision::Proceed
                | HookDecision::Override { .. }
                | HookDecision::Adjust { .. } => {}
            }
        }
        AttemptDecision { admission, notes }
    }

    async fn prepare_result(&self, request: PrepareResult) -> Result<Prepared, PrepareError> {
        if is_synthetic(&request.view) {
            return Ok(Prepared::unchanged());
        }
        let report = self
            .service
            .run(HookRequest {
                point:   HookPoint::AfterAttempt,
                view:    Some(request.view.clone()),
                outcome: Some(request.outcome.clone()),
                routes:  Vec::new(),
                payload: Value::Null,
            })
            .await;
        let mut prepared = Prepared::unchanged();
        prepared.notes.extend(Self::note(&report));
        if let HookDecision::Adjust { status, reason } = report.decision {
            prepared.adjustment.status = Some(status);
            prepared.adjustment.reason = Some(reason);
        }
        Ok(prepared)
    }

    async fn after_record(&self, recorded: Recorded) -> Vec<Note> {
        if is_synthetic(&recorded.view) {
            return Vec::new();
        }
        let report = self
            .service
            .run(HookRequest {
                point:   HookPoint::AfterVisit,
                view:    Some(recorded.view),
                outcome: Some(recorded.outcome),
                routes:  Vec::new(),
                payload: Value::Null,
            })
            .await;
        Self::note(&report).into_iter().collect()
    }

    async fn transition(
        &self,
        transition: Transition,
    ) -> Result<TransitionReport, TransitionError> {
        if is_synthetic(&transition.view) {
            return Ok(TransitionReport::default());
        }
        let report = self
            .service
            .run(HookRequest {
                point:   HookPoint::RouteSelected,
                view:    Some(transition.view),
                outcome: Some(transition.outcome),
                routes:  transition
                    .groups
                    .iter()
                    .map(|group| (group.group, group.decision.clone()))
                    .collect(),
                payload: Value::Null,
            })
            .await;
        let mut out = TransitionReport {
            problems: report.warnings.clone(),
            ..TransitionReport::default()
        };
        out.notes.extend(Self::note(&report));
        match report.decision {
            HookDecision::Block { reason } => Err(TransitionError::new(reason)),
            HookDecision::Override { group, edge } => {
                out.overrides.push(RouteOverride { group, edge });
                Ok(out)
            }
            _ => Ok(out),
        }
    }

    async fn run_finished(&self, finished: RunFinished) {
        let report = self
            .run_level(HookPoint::RunFinished, RunFinishedPayload {
                status:  finished.status,
                failure: finished.failure,
            })
            .await;
        Self::log_run_level(&report);
    }

    async fn scope_released(&self, released: ScopeReleased) {
        let report = self
            .run_level(HookPoint::ScopeReleased, ScopeReleasedPayload {
                scope:   released.scope,
                outcome: SmolStr::new(match released.outcome {
                    executor::ScopeOutcome::Succeeded => "succeeded",
                    executor::ScopeOutcome::Failed => "failed",
                }),
            })
            .await;
        Self::log_run_level(&report);
    }
}

impl HookAdapter {
    /// A point with no firing: the payload is the whole request. No decision
    /// is consumed; the report is logged, not recorded (there is no firing
    /// to record it under).
    async fn run_level(&self, point: HookPoint, payload: impl Serialize) -> HookReport {
        self.service
            .run(HookRequest {
                point,
                view: None,
                outcome: None,
                routes: Vec::new(),
                payload: serde_json::to_value(payload).unwrap_or(Value::Null),
            })
            .await
    }

    fn log_run_level(report: &HookReport) {
        if report.is_silent() {
            return;
        }
        tracing::info!(
            point = ?report.point,
            hooks = report.hooks.len(),
            warnings = ?report.warnings,
            "run-level hooks ran"
        );
    }
}
