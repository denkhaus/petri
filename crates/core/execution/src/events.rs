//! The public event contract: one versioned stream a host projects a run
//! from, built only from durable records.
//!
//! Every [`RunEvent`] is derived from a coordinator record or an engine
//! record with the post-apply engine state beside it. The same derivation
//! runs live (as an [`ExecutionObserver`]) and over a finished run dir
//! ([`replay_run`]), so a host that lost its live subscription rebuilds the
//! same events, in the same order, with the same identities, from the files
//! alone. Nothing here reads a clock inside the state machine: `recorded_at`
//! is the time the record was appended to its log, read at that boundary and
//! persisted beside the record, so it is the same live and on replay;
//! `observed_at` is stamped by the projector when it sees a record live and is
//! absent on replay.
//!
//! # Identity and ordering
//!
//! [`EventId`] is `(source, seq, index)`: the log the record came from (the
//! coordinator log or one execution's engine log), the record's `seq` in
//! that log, and the ordinal of this event among the events one record
//! produced. Within one source the order is total. Across sources the
//! `execution` and `invocation` fields plus [`ParentLink`] tie an execution's
//! events to the invocation that declared it and to the parent firing that
//! called it.
//!
//! # Delivery
//!
//! [`EventProjector`] is the lossless path: the observer callback projects
//! synchronously and queues; a pump task hands each event to the host's
//! [`RunEventSink`] and awaits it, so a slow sink delays delivery and never
//! drops. A sink failure stops the pump; every later event is counted as
//! undelivered and [`EventProjector::shutdown`] reports the failure. A host
//! that needs completeness after such a failure calls [`replay_run`].
//!
//! Across a resume, the driver replays the regenerated suffix to observers
//! before dispatching pending work, so events for records the crash kept off
//! disk arrive again with the same identities: delivery is at-least-once,
//! deduplicated by [`EventId`]. Records before the loaded prefix are not
//! re-delivered live; [`replay_run`] covers them. A projector attached at
//! resume is built with [`EventProjector::primed`], which folds the prefix
//! into its state without delivering it, so the suffix derives the same
//! events it would have derived live.
//!
//! # Durability
//!
//! Every event here is derived from a durable record, log lines included:
//! the engine log persists `StepProgress`, and every record carries the time
//! it was recorded, so run, invocation, visit, attempt, interview and command
//! start and completion times are recovered from the logs as `recorded_at`.
//! `observed_at` is the one live-only field. Agent activity
//! ([`EventBody::AgentActivity`]) carries the backend's own envelope as
//! recorded; a backend's live stream chunks that never reached the step's
//! progress channel are not in the contract.
//!
//! # Secrets
//!
//! Records are masked by the driver before they are appended, so every value
//! here is post-mask: a secret reference stays `{"$secret": ...}` and a
//! masked value stays `***`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, io};

use driver::lifecycle::{BUDGET_PAUSED_KIND, BUDGET_RESUMED_KIND, BudgetNote, Note};
use driver::{BranchMap, BranchRef, BranchRole};
use engine::{
    Admission, DecisionId, EngineExit, EngineState, EntryPoint, Event, EventRecord, GroupDecision,
    Intervention, RouteApplied, RouteDecision,
};
use ir::placeholder::is_placeholder_item;
use ir::{
    Attempt, CancelScopeId, Control, EdgeId, EdgeTransition, FiringId, Generation, Graph,
    LogStream, Metrics, NodeId, Outcome, RunStatus, Status, StepEvent, Token, Value,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use steps::{ANSWER_KEY, Question};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::host::EVENTS_FILE;
use crate::store::execution_relative_dir;
use crate::{
    COORDINATOR_FILE, CancelReason, CoordinatorEvent, CoordinatorRecord, CoordinatorState,
    EngineLogError, ExecutionId, ExecutionObserver, GRAPHS_DIR, InvocationId, InvocationResult,
    ParentCallKey, SandboxBinding, StateError, StoreError, decode_coordinator_log, read_engine_log,
};

/// The version of this contract. Bump when an existing field changes meaning
/// or a variant is removed; adding a variant or an optional field does not.
pub const EVENT_CONTRACT_VERSION: u32 = 1;

/// The `StepEvent::Custom` key under which a backend's own event envelope
/// rides, with `kind` naming the backend.
pub const BACKEND_EVENT_KIND_KEY: &str = "kind";

/// Which durable log an event was derived from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "log", rename_all = "snake_case")]
pub enum EventSource {
    Coordinator,
    Execution { execution: ExecutionId },
}

/// A stable identity for deduplication.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventId {
    pub source: EventSource,
    /// The record's position in its log.
    pub seq:    u64,
    /// Which of the events derived from that one record this is.
    pub index:  u32,
}

/// The firing that called a nested invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentLink {
    pub execution: ExecutionId,
    pub firing:    FiringId,
    pub attempt:   Attempt,
    pub slot:      SmolStr,
}

impl From<&ParentCallKey> for ParentLink {
    fn from(key: &ParentCallKey) -> Self {
        Self {
            execution: key.parent,
            firing:    key.firing,
            attempt:   key.attempt,
            slot:      key.slot.clone(),
        }
    }
}

/// A node, with the frontend's metadata so a host can tell a logical stage
/// from a synthetic lowering node without knowing the frontend.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeRef {
    pub id:   NodeId,
    /// The instance name (`build`, or `build#2` for an expansion clone).
    pub name: SmolStr,
    /// The step kind the node runs.
    pub kind: SmolStr,
    /// `Node::meta` verbatim. A frontend marks a node it invented with
    /// `"synthetic": true` and names its role under `"kind"`.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta: Value,
}

/// What an event is about: one firing of one node, or the node alone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subject {
    pub node:       NodeRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firing:     Option<FiringId>,
    /// Which firing of the node this is within its execution, 1-based.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visit:      Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt:    Option<Attempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<Generation>,
    pub branch:     BranchRole,
}

/// One event of the public stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    pub id:          EventId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation:  Option<InvocationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution:   Option<ExecutionId>,
    /// The parent call, for an event of a nested invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent:      Option<ParentLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject:     Option<Subject>,
    /// Milliseconds since the Unix epoch when the projector saw the record
    /// live. Absent on replay: a replayed event is not observed again, and
    /// replay time is never passed off as execution time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<u64>,
    /// Milliseconds since the Unix epoch when the record this event derives
    /// from was appended to its log: the driver's clock for an engine record,
    /// the coordinator store's for a coordinator record, read at the append
    /// and persisted beside the record. The same live and on replay; the
    /// time an event happened, as opposed to when it was seen. Absent only
    /// for a record replay regenerated that never reached a log (a crash's
    /// lost tail, projected before any resume rewrote it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<u64>,
    pub body:        EventBody,
}

/// Where a firing stands between records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitState {
    /// The host is deciding whether the attempt may start.
    AwaitingAdmission,
    Running,
    /// A question is out and no answer has arrived.
    AwaitingAnswer,
    /// Between attempts, waiting out the backoff.
    AwaitingRetry,
    /// Told to stop; the outcome is on its way.
    Cancelling,
}

/// One routing group's resolution, with the edge target resolved to a node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteChoice {
    pub group:    u32,
    pub decision: RouteDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target:   Option<NodeRef>,
    /// Middleware and host interventions, outermost first. An override or
    /// jump here means the host changed the engine's proposal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trace:    Vec<Intervention>,
    /// Whether the engine drew a random number for a weighted tier.
    pub weighted: bool,
}

/// One branch's result: the record of the last node that ran on the branch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchResult {
    pub branch:  BranchRef,
    /// The last node on the branch: the one whose token reached the join, or
    /// the branch's last record when the fork was cancelled or killed.
    pub node:    NodeRef,
    pub firing:  FiringId,
    pub status:  Status,
    /// The token payload the branch handed to the join. Absent when the join
    /// never fired (a cancelled or killed fork).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
}

/// One occurrence of a fork: the firing of the fork node that opened it, in
/// its execution. Every event about the fork's branches carries it, so a
/// host keys a repeated visit of one fork, a fork inside a branch, or two
/// branches with the same target on this reference and never on the most
/// recent fork it saw.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkOccurrence {
    pub execution:  ExecutionId,
    pub fork:       NodeId,
    pub firing:     FiringId,
    /// Which firing of the fork node this is within the execution, 1-based.
    pub visit:      u32,
    pub generation: Generation,
}

/// How a fork's branches were closed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkDisposition {
    /// Every branch's token reached the join and the join fired.
    #[default]
    Joined,
    /// The join completed without running: its scope was cancelled, or every
    /// branch reached it cancelled. The results are the branches' last
    /// records.
    Cancelled,
    /// The fork's scope was killed and the join never fired. The results are
    /// the branches' last records; a branch that never recorded one is
    /// absent.
    Killed,
}

/// A backend's own event, attributed. `envelope` is the backend's envelope
/// as the step recorded it (for the native agent backend: Pebble's
/// `CodingAgentEvent`, with `seq`, `stream_id`, `session_id`,
/// `parent_session_id`, `tool_call_id`, `timestamp`, `event`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentActivity {
    /// The backend name, from the custom event's `kind`.
    pub backend:        SmolStr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session:        Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call:      Option<String>,
    /// The backend's own stream identity and sequence, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream:         Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_seq:     Option<u64>,
    pub envelope:       Value,
}

/// What happened.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventBody {
    // ── Run and invocations (coordinator log) ───────────────────────────────
    RunStarted {
        root:             InvocationId,
        middleware_chain: Vec<engine::MiddlewareKey>,
    },
    RunFinished {
        status: RunStatus,
    },
    InvocationDeclared {
        invocation: InvocationId,
        /// Absent for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call:       Option<ParentLink>,
        graph:      String,
        sandbox:    SandboxBinding,
        context:    BTreeMap<SmolStr, Value>,
    },
    InvocationFinished {
        invocation: InvocationId,
        result:     InvocationResult,
    },
    InvocationCancelRequested {
        invocation: InvocationId,
        /// Why, when the requester said (the watchdog, an interrupt, a
        /// control).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason:     Option<CancelReason>,
    },
    /// The stall watchdog cancelled the run: no execution activity for the
    /// budget. Derived from the cancel record that carries the reason.
    StallTimeout {
        stall_timeout_ms: u64,
        idle_ms:          u64,
    },
    /// The run is paused: attempts not yet admitted are held. From the
    /// coordinator's `RunPaused` record, so replay carries it and a resume
    /// starts paused when it is the last control recorded.
    RunPaused,
    /// The run is unpaused: held attempts proceed. From the coordinator's
    /// `RunUnpaused` record.
    RunUnpaused,
    /// An executor-enforced attempt budget stopped counting: the attempt asked
    /// a question. `remaining_ms` is the active-work time left.
    BudgetPaused {
        remaining_ms:      u64,
        pending_questions: u32,
    },
    /// The attempt budget counts again: its last pending question was
    /// answered.
    BudgetResumed {
        remaining_ms: u64,
    },
    ExecutionDeclared {
        execution:       ExecutionId,
        invocation:      InvocationId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        predecessor:     Option<ExecutionId>,
        execution_index: u32,
        entry:           EntryPoint,
    },
    ExecutionFinished {
        execution: ExecutionId,
        exit:      EngineExit,
    },

    // ── Execution (engine log) ──────────────────────────────────────────────
    ExecutionStarted {
        entry:           EntryPoint,
        execution_index: u32,
        context:         BTreeMap<SmolStr, Value>,
    },
    /// The execution's own admission decision, before any node fires.
    ExecutionAdmitted {
        admitted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason:   Option<SmolStr>,
    },
    /// A node's join was satisfied and a firing exists, awaiting admission.
    VisitStarted {
        inputs: Vec<Token>,
    },
    /// The host or middleware decided on an attempt.
    AttemptAdmitted {
        decision: Admission,
        trace:    Vec<engine::MiddlewareKey>,
    },
    /// The attempt was dispatched to its step.
    AttemptStarted,
    /// An attempt returned. `final` is whether the engine recorded it as the
    /// firing's outcome; a non-final attempt is followed by a retry.
    AttemptFinished {
        outcome:   Outcome,
        #[serde(rename = "final")]
        is_final:  bool,
        /// The retry policy allowed no further attempt and the status was
        /// retryable: the firing exhausted its retries.
        exhausted: bool,
    },
    RetryScheduled {
        next_attempt: Attempt,
        base_delay:   Duration,
    },
    RetryElapsed {
        next_attempt: Attempt,
    },
    /// A firing's final record exists: the node completed this visit.
    /// `executed` is false for a completion the engine synthesized (a false
    /// precondition, a cancelled scope, a blocked admission).
    VisitCompleted {
        outcome:  Outcome,
        executed: bool,
        attempts: u32,
    },
    RoutesResolved {
        choices: Vec<RouteChoice>,
    },
    RouteApplied {
        route: AppliedRoute,
    },
    /// The routes of a fork node applied: its branches are starting.
    ForkStarted {
        occurrence: ForkOccurrence,
        branches:   Vec<BranchRef>,
    },
    /// A branch reached its end: its final token reached the join, or the
    /// fork was cancelled or killed and this is the branch's last record.
    BranchCompleted {
        occurrence: ForkOccurrence,
        result:     BranchResult,
    },
    /// The fork's branches are all accounted for, in branch order:
    /// `disposition` says whether the join fired or the fork was stopped.
    ForkCompleted {
        occurrence:  ForkOccurrence,
        fork:        NodeRef,
        results:     Vec<BranchResult>,
        #[serde(default)]
        disposition: ForkDisposition,
    },
    /// A `for_each` expansion spliced clones in.
    NodeExpanded {
        clones:       Vec<CloneRef>,
        max_parallel: Option<u32>,
        fail_fast:    bool,
    },
    /// A firing asked the host a question.
    QuestionAsked {
        question: Question,
    },
    /// The host delivered a control into a firing. `deliverable` is whether
    /// the firing could receive it; a late answer is recorded but not
    /// deliverable.
    ControlDelivered {
        control:     DeliveredControl,
        deliverable: bool,
    },
    /// A firing's wait state changed.
    WaitStateChanged {
        state: WaitState,
    },
    /// A polite cancel. `scope` names the cancelled scope for a scope
    /// cancel; `group` names the anchor node for a group cancel.
    CancelRequested {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<CancelScopeId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<NodeRef>,
    },
    KillRequested {
        scope: CancelScopeId,
    },
    OutputLine {
        stream: LogStream,
        line:   String,
    },
    ArtifactRecorded {
        name: SmolStr,
        uri:  String,
    },
    AgentActivity(AgentActivity),
    /// A host extension recorded a fact (`driver::lifecycle::Note`). Kinds
    /// the driver writes: `result_prepared`, `transition`. Kinds the hook
    /// adapter writes: `hook`.
    HostNote {
        kind:    SmolStr,
        payload: Value,
    },
    /// A step-defined progress payload this contract does not interpret.
    StepCustom {
        value: Value,
    },
}

/// One applied route.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppliedRoute {
    Edge {
        group:      u32,
        edge:       EdgeId,
        target:     NodeRef,
        transition: EdgeTransition,
        back:       bool,
    },
    Jump {
        target: NodeRef,
    },
    None {
        group: u32,
    },
}

/// One expansion clone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CloneRef {
    pub index: u32,
    pub entry: NodeRef,
    pub item:  Value,
}

/// A control as delivered, decoded when it is an answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeliveredControl {
    Answer { answer: steps::Answer },
    Deliver { value: Value },
    Cancel,
    Kill,
}

// ── Projection ─────────────────────────────────────────────────────────────

/// Per-execution bookkeeping the derivation needs beyond the engine state.
#[derive(Default)]
struct ExecutionTrack {
    invocation: Option<InvocationId>,
    parent:     Option<ParentLink>,
    /// Firings seen live, so a new one is a visit start.
    firings:    BTreeSet<FiringId>,
    /// Firings whose attempt was dispatched at least once.
    started:    BTreeSet<FiringId>,
    /// Firings with a question out.
    asking:     BTreeSet<FiringId>,
    history:    usize,
    branches:   BranchMap,
    /// Fork firings whose `ForkStarted` was emitted.
    announced:  BTreeSet<FiringId>,
    /// Forks whose `ForkCompleted` is still to come, by the fork's firing.
    open:       BTreeMap<FiringId, OpenFork>,
}

/// A fork between its `ForkStarted` and its `ForkCompleted`. The generation
/// ties the branches and the join to this occurrence of the fork: the engine
/// fires one `(node, generation)` at most once, and the tokens a fork routes
/// to its branches and on to the join keep the fork firing's generation.
struct OpenFork {
    occurrence: ForkOccurrence,
    branches:   Vec<BranchRef>,
}

impl OpenFork {
    fn covers(&self, fork: NodeId, generation: Generation) -> bool {
        self.occurrence.fork == fork && self.occurrence.generation == generation
    }
}

impl ExecutionTrack {
    /// Remember an announced fork until its join closes it.
    fn open_fork(&mut self, occurrence: ForkOccurrence, branches: &[BranchRef]) {
        self.open.insert(occurrence.firing, OpenFork {
            occurrence,
            branches: branches.to_vec(),
        });
    }

    /// Take the open fork a join of `fork` in `generation` closes, if any.
    fn close_fork(&mut self, fork: NodeId, generation: Option<Generation>) -> Option<OpenFork> {
        let generation = generation?;
        let firing = self
            .open
            .iter()
            .find(|(_, open)| open.covers(fork, generation))
            .map(|(firing, _)| *firing)?;
        self.open.remove(&firing)
    }
}

/// The stateless-by-record derivation, with the little state it needs across
/// records. One per run; fed both logs.
#[derive(Default)]
pub struct Projection {
    executions:  BTreeMap<ExecutionId, ExecutionTrack>,
    invocations: BTreeMap<InvocationId, Option<ParentLink>>,
}

impl Projection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Derive the events of one coordinator record.
    pub fn lifecycle(&mut self, record: &CoordinatorRecord) -> Vec<RunEvent> {
        let id = EventId {
            source: EventSource::Coordinator,
            seq:    record.seq,
            index:  0,
        };
        let (invocation, execution, body) = match &record.event {
            CoordinatorEvent::RunStarted {
                root,
                middleware_chain,
                ..
            } => (Some(*root), None, EventBody::RunStarted {
                root:             *root,
                middleware_chain: middleware_chain.clone(),
            }),
            CoordinatorEvent::GraphRegistered { .. } => return Vec::new(),
            CoordinatorEvent::InvocationDeclared {
                invocation,
                call,
                graph,
                context,
                sandbox,
                ..
            } => {
                let link = call.as_ref().map(ParentLink::from);
                self.invocations.insert(*invocation, link.clone());
                (Some(*invocation), None, EventBody::InvocationDeclared {
                    invocation: *invocation,
                    call:       link,
                    graph:      graph.to_hex(),
                    sandbox:    *sandbox,
                    context:    context.clone(),
                })
            }
            CoordinatorEvent::ExecutionDeclared {
                execution,
                invocation,
                predecessor,
                start,
                ..
            } => {
                let track = self.executions.entry(*execution).or_default();
                track.invocation = Some(*invocation);
                track.parent = self.invocations.get(invocation).cloned().flatten();
                (
                    Some(*invocation),
                    Some(*execution),
                    EventBody::ExecutionDeclared {
                        execution:       *execution,
                        invocation:      *invocation,
                        predecessor:     *predecessor,
                        execution_index: start.execution_index,
                        entry:           start.entry,
                    },
                )
            }
            CoordinatorEvent::ExecutionFinished { execution, exit } => (
                self.executions
                    .get(execution)
                    .and_then(|track| track.invocation),
                Some(*execution),
                EventBody::ExecutionFinished {
                    execution: *execution,
                    exit:      exit.clone(),
                },
            ),
            CoordinatorEvent::InvocationFinished { invocation, result } => (
                Some(*invocation),
                Some(result.final_execution),
                EventBody::InvocationFinished {
                    invocation: *invocation,
                    result:     result.clone(),
                },
            ),
            CoordinatorEvent::InvocationCancelRequested { invocation, reason } => {
                let mut events = vec![RunEvent {
                    id,
                    invocation: Some(*invocation),
                    execution: None,
                    parent: self.invocations.get(invocation).cloned().flatten(),
                    subject: None,
                    observed_at: None,
                    recorded_at: Some(record.recorded_at),
                    body: EventBody::InvocationCancelRequested {
                        invocation: *invocation,
                        reason:     reason.clone(),
                    },
                }];
                if let Some(CancelReason::StallTimeout {
                    stall_timeout_ms,
                    idle_ms,
                }) = reason
                {
                    events.push(RunEvent {
                        id:          EventId { index: 1, ..id },
                        invocation:  Some(*invocation),
                        execution:   None,
                        parent:      None,
                        subject:     None,
                        observed_at: None,
                        recorded_at: Some(record.recorded_at),
                        body:        EventBody::StallTimeout {
                            stall_timeout_ms: *stall_timeout_ms,
                            idle_ms:          *idle_ms,
                        },
                    });
                }
                return events;
            }
            CoordinatorEvent::RunPaused => (None, None, EventBody::RunPaused),
            CoordinatorEvent::RunUnpaused => (None, None, EventBody::RunUnpaused),
            // A run-level hook report: the same `host_note` a firing's hook
            // report is, with no subject, since no firing owns it.
            CoordinatorEvent::RunNote {
                execution,
                kind,
                payload,
            } => (None, *execution, EventBody::HostNote {
                kind:    kind.clone(),
                payload: payload.clone(),
            }),
            CoordinatorEvent::RunFinished { status } => {
                (None, None, EventBody::RunFinished { status: *status })
            }
        };
        let parent = invocation
            .and_then(|invocation| self.invocations.get(&invocation).cloned())
            .flatten();
        vec![RunEvent {
            id,
            invocation,
            execution,
            parent,
            subject: None,
            observed_at: None,
            recorded_at: Some(record.recorded_at),
            body,
        }]
    }

    /// Derive the events of one engine record, given its recording time (when
    /// the record reached a log; `None` for a regenerated record that never
    /// did) and the post-apply state.
    pub fn engine(
        &mut self,
        execution: ExecutionId,
        record: &EventRecord,
        recorded_at: Option<u64>,
        state: &EngineState,
    ) -> Vec<RunEvent> {
        let track = self.executions.entry(execution).or_default();
        if !track.branches.covers(state.graph()) {
            track.branches = BranchMap::of(state.graph()).with_expansions(state);
        }
        let mut out = Vec::new();
        let mut emit = |subject: Option<Subject>, body: EventBody| {
            out.push((subject, body));
        };

        match &record.event {
            Event::ExecutionStarted(start) => emit(None, EventBody::ExecutionStarted {
                entry:           start.entry,
                execution_index: start.execution_index,
                context:         start.context.clone(),
            }),
            Event::TokenEmitted(_) => {}
            Event::StepStarted { firing, .. } => {
                track.started.insert(*firing);
                emit(subject_of(state, track, *firing), EventBody::AttemptStarted);
                emit(
                    subject_of(state, track, *firing),
                    EventBody::WaitStateChanged {
                        state: WaitState::Running,
                    },
                );
            }
            Event::StepProgress { firing, ev } => {
                let subject = subject_of(state, track, *firing);
                match ev {
                    StepEvent::Log { stream, line } => emit(subject, EventBody::OutputLine {
                        stream: *stream,
                        line:   line.clone(),
                    }),
                    StepEvent::Artifact { name, uri } => {
                        emit(subject, EventBody::ArtifactRecorded {
                            name: name.clone(),
                            uri:  uri.clone(),
                        });
                    }
                    StepEvent::Custom(value) => {
                        if let Some(question) = Question::from_event(ev) {
                            track.asking.insert(*firing);
                            emit(subject.clone(), EventBody::QuestionAsked { question });
                            emit(subject, EventBody::WaitStateChanged {
                                state: WaitState::AwaitingAnswer,
                            });
                        } else if let Some(note) = Note::from_step_event(ev) {
                            emit(subject, note_body(note));
                        } else if let Some(activity) = agent_activity(value) {
                            emit(subject, EventBody::AgentActivity(activity));
                        } else {
                            emit(subject, EventBody::StepCustom {
                                value: value.clone(),
                            });
                        }
                    }
                }
            }
            Event::StepFinished {
                firing,
                attempt,
                outcome,
            } => {
                let is_final = state
                    .history()
                    .iter()
                    .rev()
                    .any(|entry| entry.firing == *firing && entry.attempt == *attempt);
                let node = state
                    .firing_node(*firing)
                    .and_then(|id| state.graph().node(id));
                let exhausted = node.is_some_and(|node| {
                    node.retry.should_retry(&outcome.status)
                        && !node.retry.has_attempt_after(*attempt)
                });
                track.asking.remove(firing);
                let subject = subject_of(state, track, *firing);
                emit(subject.clone(), EventBody::AttemptFinished {
                    outcome: outcome.clone(),
                    is_final,
                    exhausted,
                });
                if !is_final && let Some(node) = node {
                    emit(subject.clone(), EventBody::RetryScheduled {
                        next_attempt: attempt.next(),
                        base_delay:   node.retry.base_delay(*attempt),
                    });
                    emit(subject, EventBody::WaitStateChanged {
                        state: WaitState::AwaitingRetry,
                    });
                }
            }
            Event::Admitted {
                decision_id,
                decision,
                trace,
            } => match decision_id {
                DecisionId::ExecutionStart => {
                    let (admitted, reason) = match decision {
                        Admission::Block { reason } => (false, Some(reason.clone())),
                        _ => (true, None),
                    };
                    emit(None, EventBody::ExecutionAdmitted { admitted, reason });
                }
                DecisionId::AttemptStart { firing, .. } => {
                    emit(
                        subject_of(state, track, *firing),
                        EventBody::AttemptAdmitted {
                            decision: decision.clone(),
                            trace:    trace.clone(),
                        },
                    );
                }
                DecisionId::Route { .. } => {}
            },
            Event::RoutingResolved {
                decision_id,
                groups,
            } => {
                if let DecisionId::Route { firing, .. } = decision_id {
                    let choices = groups
                        .iter()
                        .map(|group| route_choice(state, group))
                        .collect();
                    emit(
                        subject_of(state, track, *firing),
                        EventBody::RoutesResolved { choices },
                    );
                }
            }
            Event::RouteApplied(applied) => {
                let firing = applied.firing();
                let subject = subject_of(state, track, firing);
                let route = applied_route(state, applied);
                emit(subject.clone(), EventBody::RouteApplied { route });
                if let (Some(subject), RouteApplied::Edge { .. }) = (&subject, applied)
                    && let BranchRole::Fork { branches } = subject.branch
                    && track.announced.insert(firing)
                {
                    // The fork's routing applies group by group; the first
                    // applied route announces the fork once.
                    let fork = subject.node.id;
                    let branches: Vec<BranchRef> = (0..branches)
                        .map(|index| BranchRef { fork, index })
                        .collect();
                    if let Some(occurrence) = occurrence_of(execution, subject) {
                        track.open_fork(occurrence.clone(), &branches);
                        emit(Some(subject.clone()), EventBody::ForkStarted {
                            occurrence,
                            branches,
                        });
                    }
                }
            }
            Event::RetryElapsed {
                firing,
                next_attempt,
            } => emit(subject_of(state, track, *firing), EventBody::RetryElapsed {
                next_attempt: *next_attempt,
            }),
            Event::NodeExpanded { node, splice } => {
                emit(node_subject(state, track, *node), EventBody::NodeExpanded {
                    clones:       splice
                        .clones
                        .iter()
                        .map(|clone| CloneRef {
                            index: clone.index,
                            entry: clone
                                .nodes
                                .iter()
                                .find(|n| n.id == clone.entry)
                                .map_or_else(
                                    || NodeRef {
                                        id:   clone.entry,
                                        name: SmolStr::new(""),
                                        kind: SmolStr::new(""),
                                        meta: Value::Null,
                                    },
                                    |n| NodeRef {
                                        id:   n.id,
                                        name: n.name.clone(),
                                        kind: SmolStr::new(n.step.kind.as_str()),
                                        meta: n.meta.clone(),
                                    },
                                ),
                            item:  clone.item.clone(),
                        })
                        .collect(),
                    max_parallel: splice.max_parallel,
                    fail_fast:    splice.fail_fast,
                });
                // An expansion is a fork: its clones are the branches, in
                // item order, and the node that fanned out into the template
                // (the branch map's fork for it) announces them once. The
                // records of one engine turn are derived against the state
                // after the whole turn, so the fork's own `route_applied`
                // may already see its role and announce it above; the guard
                // is the same firing set, so whichever record comes first
                // announces and the other stays quiet. The join derives
                // `branch_completed` and `fork_completed` from the same roles
                // the static path uses.
                if let Some(fork) = track.branches.expansion_fork(*node) {
                    // The fork's own firing: the record of the fork node in
                    // the expansion's generation, which the template's token
                    // carried from it.
                    let firing = state
                        .history()
                        .iter()
                        .rev()
                        .find(|record| {
                            record.node == fork && record.generation == splice.generation
                        })
                        .map(|record| record.firing);
                    let announced = firing.is_some_and(|firing| !track.announced.insert(firing));
                    let subject = firing.and_then(|firing| subject_of(state, track, firing));
                    // The placeholder clone an empty list expands to is no
                    // branch: the fork starts and closes with none.
                    let mut branches: Vec<BranchRef> = splice
                        .clones
                        .iter()
                        .filter(|clone| !is_placeholder_item(&clone.item))
                        .map(|clone| BranchRef {
                            fork,
                            index: clone.index,
                        })
                        .collect();
                    branches.sort_by_key(|branch| branch.index);
                    if !announced
                        && let Some(subject) = subject
                        && let Some(occurrence) = occurrence_of(execution, &subject)
                    {
                        track.open_fork(occurrence.clone(), &branches);
                        emit(Some(subject), EventBody::ForkStarted {
                            occurrence,
                            branches,
                        });
                    }
                }
            }
            Event::CancelRequested { scope } => emit(None, EventBody::CancelRequested {
                scope: Some(*scope),
                group: None,
            }),
            Event::CancelGroupRequested { node } => {
                let group = state.graph().node(*node).map(node_ref);
                emit(None, EventBody::CancelRequested { scope: None, group });
            }
            Event::KillRequested { scope } => {
                emit(None, EventBody::KillRequested { scope: *scope });
            }
            Event::ControlRequested { firing, ctl } => {
                let deliverable = state
                    .firing(*firing)
                    .is_some_and(|f| !f.cancelling && !f.awaiting_retry)
                    && !state.is_awaiting_admission(*firing);
                let control = match ctl {
                    Control::Cancel => DeliveredControl::Cancel,
                    Control::Kill => DeliveredControl::Kill,
                    Control::Deliver(value) => match value.get(ANSWER_KEY) {
                        Some(_) => steps::Answer::from_value(value).map_or_else(
                            || DeliveredControl::Deliver {
                                value: value.clone(),
                            },
                            |answer| DeliveredControl::Answer { answer },
                        ),
                        None => DeliveredControl::Deliver {
                            value: value.clone(),
                        },
                    },
                    _ => DeliveredControl::Deliver { value: Value::Null },
                };
                let answered = matches!(control, DeliveredControl::Answer { .. })
                    && deliverable
                    && track.asking.remove(firing);
                let subject = subject_of(state, track, *firing);
                emit(subject.clone(), EventBody::ControlDelivered {
                    control,
                    deliverable,
                });
                if answered {
                    emit(subject, EventBody::WaitStateChanged {
                        state: WaitState::Running,
                    });
                }
            }
        }

        // State-derived facts every record may carry: new firings (visits
        // starting) and new final records (visits completing).
        let live: Vec<FiringId> = state.live_firings().map(|f| f.id).collect();
        for firing in live {
            if track.firings.insert(firing) {
                let inputs = state
                    .firing(firing)
                    .map(|f| f.inputs.clone())
                    .unwrap_or_default();
                let subject = subject_of(state, track, firing);
                if let Some(subject) = &subject
                    && let BranchRole::Join { fork } = subject.branch
                {
                    // The join fired: every branch is in, and its inputs are
                    // the branches' final tokens.
                    let results = branch_results(state, track, fork, &inputs);
                    let occurrence = track
                        .close_fork(fork, subject.generation)
                        .map(|open| open.occurrence)
                        .or_else(|| {
                            recover_occurrence(state, track, execution, fork, subject.generation)
                        });
                    if let Some(occurrence) = occurrence {
                        close_fork(
                            &mut emit,
                            state,
                            track,
                            subject,
                            occurrence,
                            results,
                            ForkDisposition::Joined,
                        );
                    }
                }
                emit(subject.clone(), EventBody::VisitStarted { inputs });
                emit(subject, EventBody::WaitStateChanged {
                    state: WaitState::AwaitingAdmission,
                });
            }
        }
        let cancelling: Vec<FiringId> = state
            .live_firings()
            .filter(|f| f.cancelling)
            .map(|f| f.id)
            .collect();
        if matches!(
            &record.event,
            Event::CancelRequested { .. }
                | Event::CancelGroupRequested { .. }
                | Event::KillRequested { .. }
        ) {
            for firing in cancelling {
                emit(
                    subject_of(state, track, firing),
                    EventBody::WaitStateChanged {
                        state: WaitState::Cancelling,
                    },
                );
            }
        }
        let history = state.history();
        if history.len() > track.history {
            for entry in &history[track.history..] {
                let executed = track.started.contains(&entry.firing);
                let attempts = entry.attempt.raw();
                let subject = state.graph().node(entry.node).map(|node| Subject {
                    node:       node_ref(node),
                    firing:     Some(entry.firing),
                    visit:      Some(state.firing_count(entry.node)),
                    attempt:    Some(entry.attempt),
                    generation: Some(entry.generation),
                    branch:     track.branches.role(entry.node),
                });
                // A join that completed without ever being live was
                // synthesized: the fork's scope was cancelled, or every
                // branch reached it cancelled. Its record closes the fork
                // from the branches' own final records.
                if let Some(subject) = &subject
                    && let BranchRole::Join { fork } = subject.branch
                    && !track.firings.contains(&entry.firing)
                    && let Some(open) = track.close_fork(fork, Some(entry.generation))
                {
                    let results = member_results(state, track, &open);
                    let disposition = if matches!(entry.outcome.status, Status::Cancelled) {
                        ForkDisposition::Cancelled
                    } else {
                        ForkDisposition::Joined
                    };
                    close_fork(
                        &mut emit,
                        state,
                        track,
                        subject,
                        open.occurrence,
                        results,
                        disposition,
                    );
                }
                emit(subject, EventBody::VisitCompleted {
                    outcome: entry.outcome.clone(),
                    executed,
                    attempts,
                });
            }
            track.history = history.len();
        }
        // A killed fork's join never fires: its tokens were dropped. Once no
        // branch of the fork has a live firing left, the fork is closed from
        // the branches' final records.
        let killed: Vec<FiringId> = track
            .open
            .iter()
            .filter(|(_, open)| {
                state.is_node_killed(open.occurrence.fork)
                    && !state.live_firings().any(|firing| {
                        firing.generation == open.occurrence.generation
                            && matches!(
                                track.branches.role(firing.node),
                                BranchRole::Member(branch) if branch.fork == open.occurrence.fork
                            )
                    })
            })
            .map(|(firing, _)| *firing)
            .collect();
        for firing in killed {
            let Some(open) = track.open.remove(&firing) else {
                continue;
            };
            let Some(subject) = subject_of(state, track, firing) else {
                continue;
            };
            let results = member_results(state, track, &open);
            close_fork(
                &mut emit,
                state,
                track,
                &subject,
                open.occurrence,
                results,
                ForkDisposition::Killed,
            );
        }

        let invocation = track.invocation;
        let parent = track.parent.clone();
        out.into_iter()
            .enumerate()
            .map(|(index, (subject, body))| RunEvent {
                id: EventId {
                    source: EventSource::Execution { execution },
                    seq:    record.seq,
                    index:  u32::try_from(index).unwrap_or(u32::MAX),
                },
                invocation,
                execution: Some(execution),
                parent: parent.clone(),
                subject,
                observed_at: None,
                recorded_at,
                body,
            })
            .collect()
    }
}

fn node_ref(node: &ir::Node) -> NodeRef {
    NodeRef {
        id:   node.id,
        name: node.name.clone(),
        kind: SmolStr::new(node.step.kind.as_str()),
        meta: node.meta.clone(),
    }
}

fn node_subject(state: &EngineState, track: &ExecutionTrack, node: NodeId) -> Option<Subject> {
    state.graph().node(node).map(|n| Subject {
        node:       node_ref(n),
        firing:     None,
        visit:      Some(state.firing_count(node)),
        attempt:    None,
        generation: None,
        branch:     track.branches.role(node),
    })
}

/// The subject for a firing, live or retired.
fn subject_of(state: &EngineState, track: &ExecutionTrack, firing: FiringId) -> Option<Subject> {
    if let Some(live) = state.firing(firing) {
        let node = state.graph().node(live.node)?;
        return Some(Subject {
            node:       node_ref(node),
            firing:     Some(firing),
            visit:      Some(state.firing_count(live.node)),
            attempt:    Some(live.attempt),
            generation: Some(live.generation),
            branch:     track.branches.role(live.node),
        });
    }
    let record = state.history().iter().rev().find(|r| r.firing == firing)?;
    let node = state.graph().node(record.node)?;
    Some(Subject {
        node:       node_ref(node),
        firing:     Some(firing),
        visit:      Some(state.firing_count(record.node)),
        attempt:    Some(record.attempt),
        generation: Some(record.generation),
        branch:     track.branches.role(record.node),
    })
}

fn route_choice(state: &EngineState, group: &GroupDecision) -> RouteChoice {
    let target = match &group.decision {
        RouteDecision::Emit(edge) => state
            .graph()
            .edge(*edge)
            .and_then(|edge| state.graph().node(edge.to))
            .map(node_ref),
        RouteDecision::Jump(node) => state.graph().node(*node).map(node_ref),
        RouteDecision::None | RouteDecision::Block { .. } => None,
    };
    RouteChoice {
        group: group.group,
        decision: group.decision.clone(),
        target,
        trace: group.trace.clone(),
        weighted: group.draw.is_some(),
    }
}

fn applied_route(state: &EngineState, applied: &RouteApplied) -> AppliedRoute {
    match applied {
        RouteApplied::Edge { group, edge, .. } => {
            let arm = state.graph().edge(*edge);
            let target = arm.and_then(|arm| state.graph().node(arm.to)).map_or_else(
                || NodeRef {
                    id:   NodeId::new(u32::MAX),
                    name: SmolStr::new(""),
                    kind: SmolStr::new(""),
                    meta: Value::Null,
                },
                node_ref,
            );
            AppliedRoute::Edge {
                group: *group,
                edge: *edge,
                target,
                transition: arm.map_or(EdgeTransition::Continue, |arm| arm.transition),
                back: arm.is_some_and(|arm| arm.back),
            }
        }
        RouteApplied::Jump { target, .. } => AppliedRoute::Jump {
            target: state.graph().node(*target).map_or_else(
                || NodeRef {
                    id:   *target,
                    name: SmolStr::new(""),
                    kind: SmolStr::new(""),
                    meta: Value::Null,
                },
                node_ref,
            ),
        },
        RouteApplied::None { group, .. } => AppliedRoute::None { group: *group },
    }
}

/// The branch results a join's inputs carry, in branch order.
fn branch_results(
    state: &EngineState,
    track: &ExecutionTrack,
    fork: NodeId,
    inputs: &[Token],
) -> Vec<BranchResult> {
    let mut results: Vec<BranchResult> = inputs
        .iter()
        .filter_map(|token| {
            let record = state
                .history()
                .iter()
                .rev()
                .find(|r| r.firing == token.from)?;
            let node = state.graph().node(record.node)?;
            let branch = match track.branches.role(record.node) {
                BranchRole::Member(branch) if branch.fork == fork => branch,
                BranchRole::Fork { .. } if record.node == fork => BranchRef { fork, index: 0 },
                _ => return None,
            };
            Some(BranchResult {
                branch,
                node: node_ref(node),
                firing: token.from,
                status: record.outcome.status.clone(),
                payload: token.payload.clone(),
            })
        })
        .collect();
    results.sort_by_key(|result| result.branch.index);
    results
}

/// The occurrence a fork firing's subject names.
fn occurrence_of(execution: ExecutionId, subject: &Subject) -> Option<ForkOccurrence> {
    Some(ForkOccurrence {
        execution,
        fork: subject.node.id,
        firing: subject.firing?,
        visit: subject.visit?,
        generation: subject.generation?,
    })
}

/// The occurrence of `fork` in `generation` from the fork's own record, for
/// a join whose fork this projection never saw announced (it attached after
/// the fork fired without being primed).
fn recover_occurrence(
    state: &EngineState,
    track: &ExecutionTrack,
    execution: ExecutionId,
    fork: NodeId,
    generation: Option<Generation>,
) -> Option<ForkOccurrence> {
    let record = state.history().iter().rev().find(|record| {
        record.node == fork && generation.is_none_or(|generation| record.generation == generation)
    })?;
    let subject = subject_of(state, track, record.firing)?;
    occurrence_of(execution, &subject)
}

/// Emit `branch_completed` per result, each on its branch's last firing,
/// then `fork_completed` on `subject` (the join's firing when there is one,
/// else the fork's own).
fn close_fork(
    emit: &mut impl FnMut(Option<Subject>, EventBody),
    state: &EngineState,
    track: &ExecutionTrack,
    subject: &Subject,
    occurrence: ForkOccurrence,
    results: Vec<BranchResult>,
    disposition: ForkDisposition,
) {
    for result in &results {
        emit(
            subject_of(state, track, result.firing),
            EventBody::BranchCompleted {
                occurrence: occurrence.clone(),
                result:     result.clone(),
            },
        );
    }
    if let Some(fork_node) = state.graph().node(occurrence.fork).map(node_ref) {
        emit(Some(subject.clone()), EventBody::ForkCompleted {
            occurrence,
            fork: fork_node,
            results,
            disposition,
        });
    }
}

/// The branches' final records for a fork that was cancelled or killed: the
/// latest record of a member of each branch in the fork's generation, in
/// branch order. A branch with no record yet is absent. The token payloads
/// are gone with the join that never ran, so none is carried.
fn member_results(
    state: &EngineState,
    track: &ExecutionTrack,
    open: &OpenFork,
) -> Vec<BranchResult> {
    let mut results: BTreeMap<u32, BranchResult> = BTreeMap::new();
    for record in state.history().iter().rev() {
        if record.generation != open.occurrence.generation {
            continue;
        }
        let BranchRole::Member(branch) = track.branches.role(record.node) else {
            continue;
        };
        if !open.branches.contains(&branch) || results.contains_key(&branch.index) {
            continue;
        }
        let Some(node) = state.graph().node(record.node) else {
            continue;
        };
        results.insert(branch.index, BranchResult {
            branch,
            node: node_ref(node),
            firing: record.firing,
            status: record.outcome.status.clone(),
            payload: Value::Null,
        });
    }
    results.into_values().collect()
}

/// Read a backend's envelope out of a `StepEvent::Custom` value: an object
/// with a string `kind` and an `event` object is a backend event; its
/// identities are read from the conventional envelope fields when present.
fn agent_activity(value: &Value) -> Option<AgentActivity> {
    let object = value.as_object()?;
    let backend = object.get(BACKEND_EVENT_KIND_KEY)?.as_str()?;
    // A step's own payload may carry a string `event` (a hook report names
    // its hook event); only an object is a backend envelope.
    let envelope = object.get("event").filter(|event| event.is_object())?;
    let text = |key: &str| envelope.get(key).and_then(Value::as_str).map(str::to_owned);
    Some(AgentActivity {
        backend:        SmolStr::new(backend),
        session:        text("session_id"),
        parent_session: text("parent_session_id"),
        tool_call:      text("tool_call_id"),
        stream:         text("stream_id"),
        stream_seq:     envelope.get("seq").and_then(Value::as_u64),
        envelope:       envelope.clone(),
    })
}

// ── Live delivery ──────────────────────────────────────────────────────────

/// Where projected events go. `deliver` is awaited per event, in order: a
/// slow sink applies backpressure to the queue behind it, never to the
/// driver. An error stops the pump.
#[async_trait::async_trait]
pub trait RunEventSink: Send + Sync {
    async fn deliver(&self, event: RunEvent) -> Result<(), SinkError>;

    /// Called once after the last event, before the receipt.
    async fn finish(&self) -> Result<(), SinkError> {
        Ok(())
    }
}

/// A sink refused or lost an event.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SinkError {
    pub message: String,
}

impl SinkError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// What the projector did over a run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionReceipt {
    pub version:     u32,
    pub projected:   u64,
    pub delivered:   u64,
    /// Events the sink never received because it had failed.
    pub undelivered: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure:     Option<String>,
}

impl ProjectionReceipt {
    pub fn is_clean(&self) -> bool {
        self.failure.is_none() && self.undelivered == 0
    }
}

enum PumpMessage {
    Event(Box<RunEvent>),
    Finish,
}

struct PumpState {
    projected: u64,
}

/// The live, lossless consumption path: an [`ExecutionObserver`] that
/// projects each record and queues the result for a pump task, which awaits
/// the sink per event.
pub struct EventProjector {
    projection: Mutex<Projection>,
    tx:         mpsc::UnboundedSender<PumpMessage>,
    pump:       Mutex<Option<JoinHandle<ProjectionReceipt>>>,
    counts:     Mutex<PumpState>,
}

impl EventProjector {
    /// A projector for a fresh run.
    pub fn new(sink: Arc<dyn RunEventSink>) -> Arc<Self> {
        Self::with_projection(sink, Projection::new())
    }

    /// A projector for a run being resumed from `run_dir`: the records on
    /// disk are folded into its state first, and nothing is delivered for
    /// them. The resumed driver then delivers the regenerated suffix and
    /// every new record with the identities a fresh run would have given
    /// them.
    ///
    /// # Errors
    ///
    /// The run dir's logs do not decode or replay.
    pub fn primed(sink: Arc<dyn RunEventSink>, run_dir: &Path) -> Result<Arc<Self>, ReplayError> {
        let mut projection = Projection::new();
        project_run(run_dir, &mut projection)?;
        Ok(Self::with_projection(sink, projection))
    }

    fn with_projection(sink: Arc<dyn RunEventSink>, projection: Projection) -> Arc<Self> {
        let (tx, mut rx) = mpsc::unbounded_channel::<PumpMessage>();
        let pump = tokio::spawn(async move {
            let mut receipt = ProjectionReceipt {
                version: EVENT_CONTRACT_VERSION,
                ..ProjectionReceipt::default()
            };
            while let Some(message) = rx.recv().await {
                match message {
                    PumpMessage::Event(event) => {
                        receipt.projected += 1;
                        if receipt.failure.is_some() {
                            receipt.undelivered += 1;
                            continue;
                        }
                        match sink.deliver(*event).await {
                            Ok(()) => receipt.delivered += 1,
                            Err(error) => {
                                receipt.undelivered += 1;
                                receipt.failure = Some(error.message);
                            }
                        }
                    }
                    PumpMessage::Finish => break,
                }
            }
            if receipt.failure.is_none()
                && let Err(error) = sink.finish().await
            {
                receipt.failure = Some(error.message);
            }
            receipt
        });
        Arc::new(Self {
            projection: Mutex::new(projection),
            tx,
            pump: Mutex::new(Some(pump)),
            counts: Mutex::new(PumpState { projected: 0 }),
        })
    }

    fn projection(&self) -> MutexGuard<'_, Projection> {
        self.projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, events: Vec<RunEvent>) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_millis()).ok());
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        for mut event in events {
            event.observed_at = now;
            counts.projected += 1;
            let _ = self.tx.send(PumpMessage::Event(Box::new(event)));
        }
    }

    /// End the stream, await the sink's last delivery and `finish`, and
    /// report. Call once, after the run.
    pub async fn shutdown(&self) -> ProjectionReceipt {
        let _ = self.tx.send(PumpMessage::Finish);
        let pump = self
            .pump
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match pump {
            Some(pump) => pump.await.unwrap_or_else(|error| ProjectionReceipt {
                version: EVENT_CONTRACT_VERSION,
                failure: Some(format!("the event pump failed: {error}")),
                ..ProjectionReceipt::default()
            }),
            None => ProjectionReceipt {
                version: EVENT_CONTRACT_VERSION,
                failure: Some("shutdown was called twice".to_owned()),
                ..ProjectionReceipt::default()
            },
        }
    }
}

impl ExecutionObserver for EventProjector {
    fn on_engine_record(
        &self,
        execution: ExecutionId,
        record: &EventRecord,
        recorded_at: u64,
        state: &EngineState,
    ) {
        let events = self
            .projection()
            .engine(execution, record, Some(recorded_at), state);
        self.push(events);
    }

    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        let events = self.projection().lifecycle(record);
        self.push(events);
    }
}

/// The body a driver or host note projects to: the driver's budget notes
/// have bodies of their own, every other note is a `host_note`.
fn note_body(note: Note) -> EventBody {
    let budget = || serde_json::from_value::<BudgetNote>(note.payload.clone()).ok();
    match note.kind.as_str() {
        BUDGET_PAUSED_KIND => match budget() {
            Some(budget) => EventBody::BudgetPaused {
                remaining_ms:      budget.remaining_ms,
                pending_questions: budget.pending_questions,
            },
            None => EventBody::HostNote {
                kind:    note.kind,
                payload: note.payload,
            },
        },
        BUDGET_RESUMED_KIND => match budget() {
            Some(budget) => EventBody::BudgetResumed {
                remaining_ms: budget.remaining_ms,
            },
            None => EventBody::HostNote {
                kind:    note.kind,
                payload: note.payload,
            },
        },
        _ => EventBody::HostNote {
            kind:    note.kind,
            payload: note.payload,
        },
    }
}

/// Collects every event it is given, for tests and small hosts.
#[derive(Default)]
pub struct CollectingSink {
    events: Mutex<Vec<RunEvent>>,
}

impl CollectingSink {
    pub fn events(&self) -> Vec<RunEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl RunEventSink for CollectingSink {
    async fn deliver(&self, event: RunEvent) -> Result<(), SinkError> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
        Ok(())
    }
}

// ── Replay ─────────────────────────────────────────────────────────────────

/// Why a run dir could not be projected.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    EngineLog(#[from] EngineLogError),
    #[error("could not read `{}`", path.display())]
    Io {
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("graph {digest} does not decode: {source}")]
    Graph {
        digest: String,
        #[source]
        source: serde_json::Error,
    },
}

/// Project every event of a run from its run dir: the coordinator log first,
/// then each execution's engine log in declaration order, each replayed
/// external event by external event so the derivation sees the same
/// post-apply states the live observer saw. Identities equal the live ones.
pub fn replay_run(run_dir: &Path) -> Result<Vec<RunEvent>, ReplayError> {
    let mut projection = Projection::new();
    project_run(run_dir, &mut projection)
}

/// [`replay_run`] through a caller's projection state.
fn project_run(run_dir: &Path, projection: &mut Projection) -> Result<Vec<RunEvent>, ReplayError> {
    let coordinator = run_dir.join(COORDINATOR_FILE);
    let bytes = fs::read(&coordinator).map_err(|source| ReplayError::Io {
        path: coordinator.clone(),
        source,
    })?;
    let decoded = decode_coordinator_log(&coordinator, &bytes)?;
    let state = CoordinatorState::replay(&decoded.records)?;
    let mut events = Vec::new();
    for record in &decoded.records {
        events.extend(projection.lifecycle(record));
    }
    for (execution, declared) in &state.executions {
        let invocation = declared.declaration.invocation;
        let digest = state.invocations[&invocation].declaration.graph;
        let graph_path = run_dir.join(GRAPHS_DIR).join(format!("{digest}.json"));
        let graph_bytes = fs::read(&graph_path).map_err(|source| ReplayError::Io {
            path: graph_path.clone(),
            source,
        })?;
        let graph: Graph =
            serde_json::from_slice(&graph_bytes).map_err(|source| ReplayError::Graph {
                digest: digest.to_hex(),
                source,
            })?;
        let log_path = run_dir
            .join(execution_relative_dir(invocation, *execution))
            .join(EVENTS_FILE);
        if !log_path.exists() {
            continue;
        }
        let decoded = read_engine_log(&log_path)?;
        events.extend(replay_execution(
            projection,
            *execution,
            graph,
            &decoded.log,
            &decoded.recorded_at,
        ));
    }
    Ok(events)
}

/// Project one execution's log through `projection`, external event by
/// external event. `recorded_at` is the log's recording time per seq; a
/// regenerated record past its end (a lost tail) carries none.
pub fn replay_execution(
    projection: &mut Projection,
    execution: ExecutionId,
    graph: Graph,
    log: &engine::EventLog,
    recorded_at: &[u64],
) -> Vec<RunEvent> {
    let mut state = EngineState::new(graph);
    let mut events = Vec::new();
    for event in log.external_events() {
        let before = state.log.len();
        let (next, _) = engine::apply(state, event.clone());
        state = next;
        for record in &state.log.records()[before..] {
            let at = usize::try_from(record.seq)
                .ok()
                .and_then(|seq| recorded_at.get(seq).copied());
            events.extend(projection.engine(execution, record, at, &state));
        }
    }
    events
}

/// Metrics helpers a projection consumer commonly wants.
pub fn duration_of(metrics: &Metrics) -> Option<Duration> {
    metrics.duration_ms.map(Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn only_an_event_object_is_a_backend_envelope() {
        let envelope = json!({
            "kind": "pebble",
            "event": { "session_id": "ses_1", "seq": 3, "event": { "TurnStarted": {} } },
        });
        let activity = agent_activity(&envelope).expect("a backend envelope");
        assert_eq!(activity.backend, "pebble");
        assert_eq!(activity.session.as_deref(), Some("ses_1"));
        assert_eq!(activity.stream_seq, Some(3));
        // A hook report names its hook event in a string `event`; it is the
        // step's own payload, a `step_custom`, not agent activity.
        let report = json!({
            "kind": "fabro.hook",
            "node": "write",
            "event": "pre_tool_use",
            "report": { "decision": { "decision": "block" }, "hooks": [] },
        });
        assert!(agent_activity(&report).is_none());
        assert!(agent_activity(&json!({ "kind": "fabro.skills", "dirs": [] })).is_none());
    }

    #[test]
    fn budget_notes_project_to_their_own_bodies_and_other_notes_stay_host_notes() {
        let paused = Note::new(
            BUDGET_PAUSED_KIND,
            json!({ "attempt": 1, "remaining_ms": 4000, "pending_questions": 1 }),
        );
        assert_eq!(note_body(paused), EventBody::BudgetPaused {
            remaining_ms:      4000,
            pending_questions: 1,
        });
        let resumed = Note::new(
            BUDGET_RESUMED_KIND,
            json!({ "attempt": 1, "remaining_ms": 4000, "pending_questions": 0 }),
        );
        assert_eq!(note_body(resumed), EventBody::BudgetResumed {
            remaining_ms: 4000,
        });
        let hook = Note::new("hook", json!({ "point": "before_attempt" }));
        assert_eq!(note_body(hook), EventBody::HostNote {
            kind:    "hook".into(),
            payload: json!({ "point": "before_attempt" }),
        });
        let malformed = Note::new(BUDGET_PAUSED_KIND, json!("not a budget"));
        assert!(matches!(note_body(malformed), EventBody::HostNote { .. }));
    }
}
