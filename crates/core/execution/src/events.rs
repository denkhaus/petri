//! The public event contract: one versioned stream a host projects a run
//! from, built only from durable records.
//!
//! # The rule
//!
//! A public event carries its record, unchanged, plus what Petri derived
//! beside it. Every [`RunEvent`] is either a record's own event or a view
//! event attached to one:
//!
//! - A record's own event is named after the record (`step.finished`,
//!   `execution.declared`): the `body.event` tag of the stored line. It carries
//!   the stored line itself under [`RunEvent::record`], exactly as the
//!   coordinator log or the execution's engine log holds it (`seq`, `origin`,
//!   `recorded_at`, `body`), serialized by the same types. Nothing is renamed,
//!   re-nested, dropped or lifted out of it. What Petri derived from the record
//!   and the post-apply state lives apart from it under [`RunEvent::derived`]:
//!   whether an attempt was final, the node a route resolved to, the answer a
//!   delivered control decoded to, Petri's reading of a step protocol it owns
//!   (`parsed`).
//! - A view event ([`ViewEvent`]: `visit.started`, `wait.state.changed`,
//!   `fork.completed` and the rest) has no record. It is derived from the state
//!   alone, marked [`RecordOrigin::Derived`], and follows the record whose
//!   apply produced it at `index` 1 and up.
//!
//! The same derivation runs live (as an [`ExecutionObserver`]) and over a
//! run's stored logs ([`replay_run`]), so a host that lost its live
//! subscription rebuilds the same events, in the same order, with the same
//! identities, from the records alone. Nothing here reads a clock inside the
//! state machine: `recorded_at` is the time the record was appended to its
//! log, read at that boundary and persisted beside the record, so it is the
//! same live and on replay; `observed_at` is stamped by the projector when
//! it sees a record live and is absent on replay.
//!
//! # Identity and ordering
//!
//! [`EventId`] is `(log, seq, index)`: the log the record came from (the
//! coordinator log or one execution's engine log), the record's `seq` in
//! that log, and the ordinal of this event among the events one record
//! produced (`0` is the record's own event). Within one log the order is
//! total. Across logs the [`Context`] plus [`ParentLink`] tie an execution's
//! events to the invocation that declared it and to the parent firing that
//! called it.
//!
//! # Delivery
//!
//! [`EventProjector`] is the live path: the observer callback projects
//! synchronously and queues without waiting; a pump task hands each event to
//! the host's [`RunEventSink`] and awaits it, in order. The queue is bounded
//! ([`ProjectorOptions::capacity`], 1024 events by default), which is the
//! most the projector holds in memory: a slow sink delays delivery and never
//! slows the driver, and an event projected while the queue is full is not
//! queued. It is counted as `overflowed` in the [`ProjectionReceipt`], live
//! delivery goes on with the next event that finds room (so the sink sees
//! each log in order, with gaps), and the durable log keeps it. A sink
//! error stops the pump; every later event is counted as undelivered. A
//! `deliver` or `finish` that outlasts [`ProjectorOptions::stall_timeout`]
//! (30 seconds by default) is dropped and counts as a failure that names the
//! event, so [`EventProjector::shutdown`] completes within about one stall
//! budget plus the drain of the queue. None of this fails the run. A host
//! that needs completeness after an overflow, a failure or a stall calls
//! [`replay_run`] and deduplicates by [`EventId`]. A host that follows a
//! run through its store alone keeps a [`RunReplay`] and advances it: each
//! advance reads and folds only the records past the ones it consumed.
//!
//! # Crash recovery
//!
//! Read-only projection publishes what is stored. A crash can leave an
//! engine log short of the core records its last external record produced;
//! [`replay_run`] regenerates them to reach the right state but publishes no
//! event attached to a record that never reached the log. Resume writes those
//! records through the normal storage path, with normal recording times,
//! before its observers see them: the driver hands the regenerated suffix to
//! the log writer first and to every other observer after it, before it
//! dispatches pending work, so events for records the crash kept off disk
//! arrive with the same identities. Delivery is at-least-once, deduplicated
//! by [`EventId`]. Records before the loaded prefix are not re-delivered
//! live; [`replay_run`] covers them. A projector attached at resume is built
//! with [`EventProjector::primed`], which folds the stored prefix into its
//! state without delivering it, so the suffix derives the same events it
//! would have derived live.
//!
//! # Export
//!
//! Because a record's own event carries the stored line, a host that stores
//! `record` values stores the logs. [`verify_export`] proves it over a run
//! dir: the exported records equal the stored ones as JSON values, reload
//! through the log readers, and replay to the stored logs (a complete log
//! exactly, a crash prefix as a prefix). The standalone host runs it at the
//! end of every run under `verify_replay`.
//!
//! # Secrets
//!
//! Records are masked by the driver before they are appended, so every value
//! here is post-mask: a secret reference stays `{"$secret": ...}` and a
//! masked value stays `***`.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::mem;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use driver::lifecycle::{BUDGET_PAUSED_KIND, BUDGET_RESUMED_KIND, BudgetNote, Note};
use driver::{BranchMap, BranchRef, BranchRole};
use engine::{
    CancelTarget, DecisionId, EngineState, Event, EventOrigin, EventRecord, GroupDecision,
    RouteApplied, RouteDecision,
};
use ir::placeholder::is_placeholder_item;
use ir::{
    Attempt, Control, EdgeTransition, FiringId, Generation, Graph, Metrics, NodeId, Outcome,
    Status, StepEvent, Token, Value,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use steps::{ANSWER_KEY, Answer, Question, QuestionExpired};
use store::{Access, LogId, RunLogs};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::hooks::{HOOK_ACTIVITY_NOTE_KIND, HookActivity};
use crate::store::{decode_coordinator_record, decode_graph, graph_bytes, read_coordinator_log};
use crate::{
    CancelReason, CoordinatorEvent, CoordinatorRecord, CoordinatorState, DecodedEngineLog,
    EngineLogDecodeError, EngineLogError, ExecutionId, ExecutionObserver, GraphDigest,
    InvocationId, ParentCallKey, StateError, StoreError, StoredEngineRecord, open_run_dir,
    read_execution_log,
};

pub mod export;

pub use export::{ExportError, verify_export, verify_export_run_dir};

/// The version of this contract. Bump when an existing field changes meaning
/// or a variant is removed; adding a variant or an optional field does not.
///
/// Version 2 made the stream lossless for replay: every event carried the
/// `origin` of its record and the first event of a record carried everything
/// the record did, under presentation names, so a reverse mapping could
/// rebuild the records.
///
/// Version 3 is one vocabulary for records and events: a public event is
/// named after its record (`step.finished`, `routing.resolved`) and carries
/// the stored record unchanged under `record`, with what Petri derived
/// beside it under `derived`. The presentation names of version 2
/// (`attempt_finished`, `routes_resolved`, `output_line` and the rest) are
/// gone, and so is the reverse mapping: export reads `record`.
///
/// Version 4 adds the scope records: `scope.acquired` and `scope.failed` in
/// an execution's log (where a scope's environment runs, or why it could not
/// be acquired) and `scope.released` in the coordinator log (a lease's
/// sandbox released by retention). The records are additive to the stream;
/// the version moves with the engine log (v11) and run format (6) that
/// carry them, so a host reading version 3 streams cannot mistake a run
/// with no scope records for one that had none to record. Within version 4,
/// `run.started` gained the optional `forked_from` (run format 7): a forked
/// run's stream names its source before any copied record (`FORK.md`).
pub const EVENT_CONTRACT_VERSION: u32 = 4;

/// Which durable log an event was derived from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "log", rename_all = "snake_case")]
pub enum EventSource {
    Coordinator,
    Execution { execution: ExecutionId },
}

/// Who appended the record an event derives from, copied from the record;
/// or that the event has no record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordOrigin {
    /// The host fed the record: every coordinator record, and an engine
    /// record the driver applied (an execution's start, an admission, a
    /// step's start, progress and result, a routing decision, an elapsed
    /// retry, a host's cancel, kill or control). Replay consumes these.
    #[default]
    External,
    /// The core produced the record while draining: a routed token, an
    /// applied route, a splice, a cascading cancel. Replay regenerates these
    /// from the external records.
    Core,
    /// A view event: derived from the state alone, with no record of its
    /// own. Never stored; replay recomputes it.
    Derived,
}

impl From<EventOrigin> for RecordOrigin {
    fn from(origin: EventOrigin) -> Self {
        match origin {
            EventOrigin::External => Self::External,
            EventOrigin::Core => Self::Core,
        }
    }
}

/// A stable identity for deduplication: the log, the record's `seq` in it,
/// and which of the events derived from that record this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventId {
    #[serde(flatten)]
    pub source: EventSource,
    /// The record's position in its log.
    pub seq:    u64,
    /// `0` is the record's own event; the view events attached to the record
    /// follow at `1` and up.
    pub index:  u32,
}

/// The firing that called a nested invocation: the driver's type, so the
/// hook context and the event context name a parent the same way.
pub use driver::ParentLink;

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

/// Where an event sits in the run: its invocation and execution, and the
/// parent call for an event of a nested invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation: Option<InvocationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution:  Option<ExecutionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent:     Option<ParentLink>,
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

/// The stored line a record's own event carries: a coordinator log line or
/// an engine log line, by the same types that write the logs. The log it
/// came from is the event's [`EventId::source`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Record {
    Coordinator(CoordinatorRecord),
    Engine(StoredEngineRecord),
}

impl Record {
    /// The engine event, for an engine record.
    pub fn engine(&self) -> Option<&Event> {
        match self {
            Self::Engine(record) => Some(&record.body),
            Self::Coordinator(_) => None,
        }
    }

    /// The coordinator event, for a coordinator record.
    pub fn coordinator(&self) -> Option<&CoordinatorEvent> {
        match self {
            Self::Coordinator(record) => Some(&record.body),
            Self::Engine(_) => None,
        }
    }
}

/// One event of the public stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    pub id:          EventId,
    /// Copied from the record: who appended it. `derived` for a view event,
    /// which has no record.
    #[serde(default)]
    pub origin:      RecordOrigin,
    #[serde(default)]
    pub context:     Context,
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
    /// time an event happened, as opposed to when it was seen. On a record's
    /// own event it repeats `record.recorded_at`; a view event carries the
    /// time of the record it is attached to.
    pub recorded_at: u64,
    /// The stored line, unchanged, on a record's own event (`index` 0).
    /// Absent on a view event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record:      Option<Record>,
    /// What Petri derived beside the record, or the view event itself.
    /// Absent when a record's event derives nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived:     Option<Derived>,
}

impl RunEvent {
    /// The engine event of an engine record's own event.
    pub fn engine(&self) -> Option<&Event> {
        self.record.as_ref().and_then(Record::engine)
    }

    /// The coordinator event of a coordinator record's own event.
    pub fn coordinator(&self) -> Option<&CoordinatorEvent> {
        self.record.as_ref().and_then(Record::coordinator)
    }

    /// The view event, for an event that is one.
    pub fn view(&self) -> Option<&ViewEvent> {
        match &self.derived {
            Some(Derived::View(view)) => Some(view),
            _ => None,
        }
    }

    /// Petri's reading of a step protocol it owns, on a
    /// `step.progress.recorded` or `run.note.recorded` event.
    pub fn parsed(&self) -> Option<&Parsed> {
        match &self.derived {
            Some(Derived::Parsed { parsed }) => Some(parsed),
            _ => None,
        }
    }

    /// The step-defined payload of a `step.progress.recorded` event, as
    /// recorded: a backend's own event, a step's report, a Petri protocol.
    pub fn custom(&self) -> Option<&Value> {
        match self.engine() {
            Some(Event::StepProgressRecorded {
                ev: StepEvent::Custom(value),
                ..
            }) => Some(value),
            _ => None,
        }
    }

    /// The note a `step.progress.recorded` or `run.note.recorded` event
    /// carries, when it is one.
    pub fn note(&self) -> Option<&Note> {
        match self.parsed() {
            Some(Parsed::Note { note, .. }) => Some(note),
            _ => None,
        }
    }
}

/// What Petri derived beside a record, or the view event an event is. Each
/// record kind that derives anything has its own shape; the view events
/// are tagged by `event`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Derived {
    /// A view event, on an event with no record.
    View(ViewEvent),
    /// `step.finished`: `final` is whether the engine recorded the attempt
    /// as the firing's outcome (a non-final attempt is followed by a
    /// retry); `exhausted` is whether the retry policy allowed no further
    /// attempt while the status was retryable.
    StepFinished {
        #[serde(rename = "final")]
        is_final:  bool,
        exhausted: bool,
    },
    /// `routing.resolved`: the node each group's decision resolved to.
    RoutingResolved { groups: Vec<GroupTarget> },
    /// `route.applied`: the node the route leads to, and for an edge its
    /// transition and whether it is a `back` edge. Absent for a route that
    /// applied nothing.
    RouteApplied {
        target:     NodeRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transition: Option<EdgeTransition>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        back:       Option<bool>,
    },
    /// `node.expanded`: each clone's entry node.
    NodeExpanded { clones: Vec<CloneRef> },
    /// `control.requested`: `deliverable` is whether the firing could
    /// receive the control (a late answer is recorded but not deliverable);
    /// `answer` is the decoding of a delivered value that reads as one.
    ControlRequested {
        deliverable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer:      Option<Answer>,
    },
    /// `step.progress.recorded` and `run.note.recorded`: Petri's reading of
    /// a step protocol it owns. Absent when the payload is a log line, an
    /// artifact, or a payload Petri does not own (a backend's own event is
    /// forwarded as recorded, `kind` naming the backend).
    Parsed { parsed: Parsed },
}

/// One routing group's resolved target.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupTarget {
    pub group:  u32,
    /// The node an `emit` or `jump` decision leads to. Absent for `none`
    /// and `block`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<NodeRef>,
}

/// One expansion clone's entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CloneRef {
    pub index: u32,
    pub entry: NodeRef,
}

/// Petri's reading of a step protocol it owns, from a `$question`,
/// `$question_expired` or `$note` payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Parsed {
    /// The step asked the host a question.
    Question { question: Question },
    /// The step's own answer deadline passed with no answer, as the step
    /// reported it. The attempt's outcome follows as `step.finished`;
    /// expiry is never inferred from that outcome.
    QuestionExpired { expired: QuestionExpired },
    /// A host extension recorded a fact (`driver::lifecycle::Note`). Kinds
    /// the driver writes: `result_prepared`, `transition`, `budget_paused`,
    /// `budget_resumed`. Kinds the hook adapter writes: `hook` (a hook
    /// service report) and `hook.activity`. Two kinds get a reading beside
    /// the note: `hook_activity` for a hook's own agent event, `budget` for
    /// an attempt budget's pause or resume.
    Note {
        note:          Note,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hook_activity: Option<HookActivity>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget:        Option<BudgetReading>,
    },
}

/// An attempt budget's pause or resume: an executor-enforced budget stopped
/// counting because the attempt asked a question, or counts again because
/// its last pending question was answered. `remaining_ms` is the active-work
/// time left.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetReading {
    pub state: BudgetState,
    #[serde(flatten)]
    pub note:  BudgetNote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetState {
    Paused,
    Resumed,
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

/// The view events: the projection's incremental reading of the state,
/// never stored. Each is attached to the record whose apply produced it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum ViewEvent {
    /// A node's join was satisfied and a firing exists, awaiting admission.
    #[serde(rename = "visit.started")]
    VisitStarted { inputs: Vec<Token> },
    /// A firing's final record exists: the node completed this visit.
    /// `executed` is false for a completion the engine synthesized (a false
    /// precondition, a cancelled scope, a blocked admission).
    #[serde(rename = "visit.completed")]
    VisitCompleted {
        outcome:  Outcome,
        executed: bool,
        attempts: u32,
    },
    /// A non-final attempt returned and the next one is waiting out the
    /// backoff.
    #[serde(rename = "retry.scheduled")]
    RetryScheduled {
        next_attempt: Attempt,
        base_delay:   Duration,
    },
    /// A firing's wait state changed.
    #[serde(rename = "wait.state.changed")]
    WaitStateChanged { state: WaitState },
    /// The routes of a fork node applied: its branches are starting.
    #[serde(rename = "fork.started")]
    ForkStarted {
        occurrence: ForkOccurrence,
        branches:   Vec<BranchRef>,
    },
    /// A branch reached its end: its final token reached the join, or the
    /// fork was cancelled or killed and this is the branch's last record.
    #[serde(rename = "branch.completed")]
    BranchCompleted {
        occurrence: ForkOccurrence,
        result:     BranchResult,
    },
    /// The fork's branches are all accounted for, in branch order:
    /// `disposition` says whether the join fired or the fork was stopped.
    #[serde(rename = "fork.completed")]
    ForkCompleted {
        occurrence:  ForkOccurrence,
        fork:        NodeRef,
        results:     Vec<BranchResult>,
        #[serde(default)]
        disposition: ForkDisposition,
    },
    /// The stall watchdog cancelled the run: no execution activity for the
    /// budget. Attached to the `invocation.cancel.requested` record that
    /// carries the reason.
    #[serde(rename = "run.stalled")]
    RunStalled {
        stall_timeout_ms: u64,
        idle_ms:          u64,
    },
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
    /// Fork firings whose `fork.started` was emitted.
    announced:  BTreeSet<FiringId>,
    /// Forks whose `fork.completed` is still to come, by the fork's firing.
    open:       BTreeMap<FiringId, OpenFork>,
}

/// A fork between its `fork.started` and its `fork.completed`. The
/// generation ties the branches and the join to this occurrence of the
/// fork: the engine fires one `(node, generation)` at most once, and the
/// tokens a fork routes to its branches and on to the join keep the fork
/// firing's generation.
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

/// A view event with the subject it is about, before it is given an id.
type View = (Option<Subject>, ViewEvent);

/// The stateless-by-record derivation, with the little state it needs across
/// records. One per run; fed both logs.
///
/// The fold is pure: it reads records and the post-apply engine state, does
/// no I/O, and hands back owned events. A record's own event carries the
/// record unchanged under `record`:
///
/// ```
/// use engine::{EngineState, Event, EventOrigin, EventRecord};
/// use execution::ExecutionId;
/// use execution::events::{Projection, Record};
/// use ir::{CancelScopeId, Graph};
///
/// let record = EventRecord {
///     seq:    0,
///     origin: EventOrigin::External,
///     event:  Event::cancel_scope(CancelScopeId::ROOT),
/// };
/// let state = EngineState::new(Graph::new());
/// let events = Projection::new().engine(ExecutionId::new(0), &record, 1_000, &state);
/// let Some(Record::Engine(stored)) = &events[0].record else {
///     panic!("a record's own event carries the record");
/// };
/// assert_eq!(stored.seq, 0);
/// assert_eq!(stored.recorded_at, 1_000);
/// assert_eq!(stored.body, record.event);
/// ```
#[derive(Default)]
pub struct Projection {
    executions:  BTreeMap<ExecutionId, ExecutionTrack>,
    invocations: BTreeMap<InvocationId, Option<ParentLink>>,
}

impl Projection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Derive the events of one coordinator record: the record's own event,
    /// then any view event attached to it.
    pub fn lifecycle(&mut self, record: &CoordinatorRecord) -> Vec<RunEvent> {
        let mut views: Vec<ViewEvent> = Vec::new();
        let (invocation, execution, derived) = match &record.body {
            CoordinatorEvent::RunStarted { root, .. } => (Some(*root), None, None),
            CoordinatorEvent::GraphRegistered { .. }
            | CoordinatorEvent::RunPaused
            | CoordinatorEvent::RunUnpaused
            | CoordinatorEvent::RunFinished { .. } => (None, None, None),
            CoordinatorEvent::InvocationDeclared {
                invocation, call, ..
            } => {
                let link = call.as_ref().map(ParentLink::from);
                self.invocations.insert(*invocation, link);
                (Some(*invocation), None, None)
            }
            CoordinatorEvent::ExecutionDeclared {
                execution,
                invocation,
                ..
            } => {
                let track = self.executions.entry(*execution).or_default();
                track.invocation = Some(*invocation);
                track.parent = self.invocations.get(invocation).cloned().flatten();
                (Some(*invocation), Some(*execution), None)
            }
            CoordinatorEvent::ExecutionFinished { execution, .. } => (
                self.executions
                    .get(execution)
                    .and_then(|track| track.invocation),
                Some(*execution),
                None,
            ),
            CoordinatorEvent::InvocationFinished { invocation, result } => {
                (Some(*invocation), Some(result.final_execution), None)
            }
            // The lease's owner; the execution that acquired it is the
            // `scope.acquired` record's.
            CoordinatorEvent::ScopeReleased { invocation, .. } => (Some(*invocation), None, None),
            CoordinatorEvent::InvocationCancelRequested { invocation, reason } => {
                if let Some(CancelReason::StallTimeout {
                    stall_timeout_ms,
                    idle_ms,
                }) = reason
                {
                    views.push(ViewEvent::RunStalled {
                        stall_timeout_ms: *stall_timeout_ms,
                        idle_ms:          *idle_ms,
                    });
                }
                (Some(*invocation), None, None)
            }
            // A run-level note reads the way a firing's note does, with no
            // subject, since no firing owns it.
            CoordinatorEvent::RunNoteRecorded {
                execution,
                kind,
                payload,
            } => (
                None,
                *execution,
                Some(Derived::Parsed {
                    parsed: note_parsed(Note::new(kind.clone(), payload.clone())),
                }),
            ),
        };
        let context = Context {
            invocation,
            execution,
            parent: invocation
                .and_then(|invocation| self.invocations.get(&invocation).cloned())
                .flatten(),
        };
        let id = |index: u32| EventId {
            source: EventSource::Coordinator,
            seq: record.seq,
            index,
        };
        let mut out = vec![RunEvent {
            id: id(0),
            origin: record.origin.into(),
            context: context.clone(),
            subject: None,
            observed_at: None,
            recorded_at: record.recorded_at,
            record: Some(Record::Coordinator(record.clone())),
            derived,
        }];
        out.extend(views.into_iter().enumerate().map(|(index, view)| RunEvent {
            id:          id(u32::try_from(index + 1).unwrap_or(u32::MAX)),
            origin:      RecordOrigin::Derived,
            context:     context.clone(),
            subject:     None,
            observed_at: None,
            recorded_at: record.recorded_at,
            record:      None,
            derived:     Some(Derived::View(view)),
        }));
        out
    }

    /// Derive the events of one engine record, given its recording time
    /// (when the record reached its log) and the post-apply state: the
    /// record's own event, then the view events attached to it.
    pub fn engine(
        &mut self,
        execution: ExecutionId,
        record: &EventRecord,
        recorded_at: u64,
        state: &EngineState,
    ) -> Vec<RunEvent> {
        let track = self.executions.entry(execution).or_default();
        if !track.branches.covers(state.graph()) {
            track.branches = BranchMap::of(state.graph()).with_expansions(state);
        }
        let mut views: Vec<View> = Vec::new();
        let mut emit = |subject: Option<Subject>, view: ViewEvent| {
            views.push((subject, view));
        };

        let (subject, derived): (Option<Subject>, Option<Derived>) = match &record.event {
            // A scope is not a node: its records carry their scope and have
            // no subject.
            Event::ExecutionStarted { .. }
            | Event::KillRequested { .. }
            | Event::ScopeAcquired { .. }
            | Event::ScopeFailed { .. }
            | Event::CancelRequested {
                target: CancelTarget::Scope(_),
            } => (None, None),
            Event::CancelRequested {
                target: CancelTarget::Group(node),
            } => (node_subject(state, track, *node), None),
            Event::TokenEmitted { token } => (subject_of(state, track, token.from), None),
            Event::StepStarted { firing, .. } => {
                track.started.insert(*firing);
                let subject = subject_of(state, track, *firing);
                emit(subject.clone(), ViewEvent::WaitStateChanged {
                    state: WaitState::Running,
                });
                (subject, None)
            }
            Event::StepProgressRecorded { firing, ev } => {
                let subject = subject_of(state, track, *firing);
                let parsed = parse_progress(ev);
                match &parsed {
                    Some(Parsed::Question { .. }) => {
                        track.asking.insert(*firing);
                        emit(subject.clone(), ViewEvent::WaitStateChanged {
                            state: WaitState::AwaitingAnswer,
                        });
                    }
                    Some(Parsed::QuestionExpired { .. }) => {
                        // The step ended its own wait: the question is no
                        // longer out, as after an answer.
                        if track.asking.remove(firing) {
                            emit(subject.clone(), ViewEvent::WaitStateChanged {
                                state: WaitState::Running,
                            });
                        }
                    }
                    Some(Parsed::Note { .. }) | None => {}
                }
                (subject, parsed.map(|parsed| Derived::Parsed { parsed }))
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
                if !is_final && let Some(node) = node {
                    emit(subject.clone(), ViewEvent::RetryScheduled {
                        next_attempt: attempt.next(),
                        base_delay:   node.retry.base_delay(*attempt),
                    });
                    emit(subject.clone(), ViewEvent::WaitStateChanged {
                        state: WaitState::AwaitingRetry,
                    });
                }
                (
                    subject,
                    Some(Derived::StepFinished {
                        is_final,
                        exhausted,
                    }),
                )
            }
            Event::AdmissionDecided { decision_id, .. } => match decision_id {
                DecisionId::AttemptStart { firing, .. } => {
                    (subject_of(state, track, *firing), None)
                }
                DecisionId::ExecutionStart | DecisionId::Route { .. } => (None, None),
            },
            Event::RoutingResolved {
                decision_id,
                groups,
            } => match decision_id {
                DecisionId::Route { firing, .. } => (
                    subject_of(state, track, *firing),
                    Some(Derived::RoutingResolved {
                        groups: groups
                            .iter()
                            .map(|group| group_target(state, group))
                            .collect(),
                    }),
                ),
                DecisionId::ExecutionStart | DecisionId::AttemptStart { .. } => (None, None),
            },
            Event::RouteApplied { applied } => {
                let firing = applied.firing();
                let subject = subject_of(state, track, firing);
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
                        emit(Some(subject.clone()), ViewEvent::ForkStarted {
                            occurrence,
                            branches,
                        });
                    }
                }
                (subject, applied_target(state, applied))
            }
            Event::RetryElapsed { firing, .. } => (subject_of(state, track, *firing), None),
            Event::NodeExpanded { node, splice } => {
                let derived = Derived::NodeExpanded {
                    clones: splice
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
                                    node_ref,
                                ),
                        })
                        .collect(),
                };
                // An expansion is a fork: its clones are the branches, in
                // item order, and the node that fanned out into the template
                // (the branch map's fork for it) announces them once. The
                // records of one engine turn are derived against the state
                // after the whole turn, so the fork's own `route.applied`
                // may already see its role and announce it above; the guard
                // is the same firing set, so whichever record comes first
                // announces and the other stays quiet. The join derives
                // `branch.completed` and `fork.completed` from the same roles
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
                        emit(Some(subject), ViewEvent::ForkStarted {
                            occurrence,
                            branches,
                        });
                    }
                }
                (node_subject(state, track, *node), Some(derived))
            }
            Event::ControlRequested { firing, ctl } => {
                let deliverable = state
                    .firing(*firing)
                    .is_some_and(|f| !f.cancelling && !f.awaiting_retry)
                    && !state.is_awaiting_admission(*firing);
                let answer = match ctl {
                    Control::Deliver(value) if value.get(ANSWER_KEY).is_some() => {
                        Answer::from_value(value)
                    }
                    _ => None,
                };
                let answered = answer.is_some() && deliverable && track.asking.remove(firing);
                let subject = subject_of(state, track, *firing);
                if answered {
                    emit(subject.clone(), ViewEvent::WaitStateChanged {
                        state: WaitState::Running,
                    });
                }
                (
                    subject,
                    Some(Derived::ControlRequested {
                        deliverable,
                        answer,
                    }),
                )
            }
        };

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
                emit(subject.clone(), ViewEvent::VisitStarted { inputs });
                emit(subject, ViewEvent::WaitStateChanged {
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
            Event::CancelRequested { .. } | Event::KillRequested { .. }
        ) {
            for firing in cancelling {
                emit(
                    subject_of(state, track, firing),
                    ViewEvent::WaitStateChanged {
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
                emit(subject, ViewEvent::VisitCompleted {
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

        let context = Context {
            invocation: track.invocation,
            execution:  Some(execution),
            parent:     track.parent.clone(),
        };
        let id = |index: u32| EventId {
            source: EventSource::Execution { execution },
            seq: record.seq,
            index,
        };
        let mut out = Vec::with_capacity(1 + views.len());
        out.push(RunEvent {
            id: id(0),
            origin: record.origin.into(),
            context: context.clone(),
            subject,
            observed_at: None,
            recorded_at,
            record: Some(Record::Engine(StoredEngineRecord::new(record, recorded_at))),
            derived,
        });
        out.extend(
            views
                .into_iter()
                .enumerate()
                .map(|(index, (subject, view))| RunEvent {
                    id: id(u32::try_from(index + 1).unwrap_or(u32::MAX)),
                    origin: RecordOrigin::Derived,
                    context: context.clone(),
                    subject,
                    observed_at: None,
                    recorded_at,
                    record: None,
                    derived: Some(Derived::View(view)),
                }),
        );
        out
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

/// The node one routing group's decision leads to.
fn group_target(state: &EngineState, group: &GroupDecision) -> GroupTarget {
    let target = match &group.decision {
        RouteDecision::Emit(edge) => state
            .graph()
            .edge(*edge)
            .and_then(|edge| state.graph().node(edge.to))
            .map(node_ref),
        RouteDecision::Jump(node) => state.graph().node(*node).map(node_ref),
        RouteDecision::None | RouteDecision::Block { .. } => None,
    };
    GroupTarget {
        group: group.group,
        target,
    }
}

/// What an applied route derives: the node it leads to, and for an edge the
/// edge's transition and `back`. Nothing for a route that applied nothing
/// or one whose target the graph no longer names.
fn applied_target(state: &EngineState, applied: &RouteApplied) -> Option<Derived> {
    match applied {
        RouteApplied::Edge { edge, .. } => {
            let arm = state.graph().edge(*edge)?;
            let target = state.graph().node(arm.to).map(node_ref)?;
            Some(Derived::RouteApplied {
                target,
                transition: Some(arm.transition),
                back: Some(arm.back),
            })
        }
        RouteApplied::Jump { target, .. } => Some(Derived::RouteApplied {
            target:     state.graph().node(*target).map(node_ref)?,
            transition: None,
            back:       None,
        }),
        RouteApplied::None { .. } => None,
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

/// Emit `branch.completed` per result, each on its branch's last firing,
/// then `fork.completed` on `subject` (the join's firing when there is one,
/// else the fork's own).
fn close_fork(
    emit: &mut impl FnMut(Option<Subject>, ViewEvent),
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
            ViewEvent::BranchCompleted {
                occurrence: occurrence.clone(),
                result:     result.clone(),
            },
        );
    }
    if let Some(fork_node) = state.graph().node(occurrence.fork).map(node_ref) {
        emit(Some(subject.clone()), ViewEvent::ForkCompleted {
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

/// Petri's reading of a progress payload, when it is a protocol Petri owns.
fn parse_progress(ev: &StepEvent) -> Option<Parsed> {
    if let Some(question) = Question::from_event(ev) {
        return Some(Parsed::Question { question });
    }
    if let Some(expired) = QuestionExpired::from_event(ev) {
        return Some(Parsed::QuestionExpired { expired });
    }
    Note::from_step_event(ev).map(note_parsed)
}

/// A note, with the reading two kinds get beside it: a hook's own agent
/// event, and an attempt budget's pause or resume. A note whose payload
/// does not read as its kind says stays a bare note.
fn note_parsed(note: Note) -> Parsed {
    let budget = |state: BudgetState| {
        serde_json::from_value::<BudgetNote>(note.payload.clone())
            .ok()
            .map(|note| BudgetReading { state, note })
    };
    let (hook_activity, budget) = match note.kind.as_str() {
        HOOK_ACTIVITY_NOTE_KIND => (
            serde_json::from_value::<HookActivity>(note.payload.clone()).ok(),
            None,
        ),
        BUDGET_PAUSED_KIND => (None, budget(BudgetState::Paused)),
        BUDGET_RESUMED_KIND => (None, budget(BudgetState::Resumed)),
        _ => (None, None),
    };
    Parsed::Note {
        note,
        hook_activity,
        budget,
    }
}

// ── Live delivery ──────────────────────────────────────────────────────────

/// Where projected events go. `deliver` is awaited per event, in order: a
/// slow sink applies backpressure to the bounded queue behind it, never to
/// the driver. An error stops the pump. A call that outlasts the projector's
/// stall budget ([`ProjectorOptions::stall_timeout`]) is dropped and counts
/// as a failure, so an implementation tolerates a cancelled `deliver`.
#[async_trait::async_trait]
pub trait RunEventSink: Send + Sync {
    async fn deliver(&self, event: RunEvent) -> Result<(), SinkError>;

    /// Called once after the last event, before the receipt.
    async fn finish(&self) -> Result<(), SinkError> {
        Ok(())
    }
}

/// The queue capacity of an [`EventProjector`] unless [`ProjectorOptions`]
/// says otherwise.
pub const DEFAULT_QUEUE_CAPACITY: usize = 1024;

/// How long an [`EventProjector`] waits for one sink call unless
/// [`ProjectorOptions`] says otherwise.
pub const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How an [`EventProjector`] bounds the memory and the time a host's sink can
/// cost it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectorOptions {
    /// Events the queue holds between the observer callback and the pump:
    /// the most the projector keeps in memory. The callback never waits for
    /// room; an event projected while the queue is full is left to the
    /// durable log and counted as `overflowed` in the receipt.
    pub capacity:      usize,
    /// How long one `deliver` (or `finish`) may take. A call that outlasts it
    /// is dropped, the sink counts as failed from then on, and the receipt
    /// names the event it stalled on.
    pub stall_timeout: Duration,
}

impl Default for ProjectorOptions {
    fn default() -> Self {
        Self {
            capacity:      DEFAULT_QUEUE_CAPACITY,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
        }
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

/// What the projector did over a run. `projected` is every event derived;
/// `delivered + undelivered` equals it. A receipt that is not clean means
/// the sink does not hold the whole stream and `replay_run` completes it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionReceipt {
    pub version:     u32,
    pub projected:   u64,
    pub delivered:   u64,
    /// Events the sink never received: they found the queue full, or the
    /// sink had failed or stalled before their turn.
    pub undelivered: u64,
    /// Of `undelivered`, the events that found the queue full and were left
    /// to the durable log. Live delivery went on with the next event that
    /// found room, so the sink saw each source in order, with gaps.
    #[serde(default)]
    pub overflowed:  u64,
    /// Why delivery stopped, when it did: the sink's error, or the stall the
    /// pump gave up on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure:     Option<String>,
}

impl ProjectionReceipt {
    pub fn is_clean(&self) -> bool {
        self.failure.is_none() && self.undelivered == 0
    }
}

/// The sender side of the pump's queue and what never got onto it.
struct Queue {
    /// `None` once `shutdown` closed the stream.
    tx:          Option<mpsc::Sender<Box<RunEvent>>>,
    /// Events that found the queue full.
    overflowed:  u64,
    /// Events projected after the stream was closed.
    after_close: u64,
}

/// The live consumption path: an [`ExecutionObserver`] that projects each
/// record and queues the result, without waiting, for a pump task that
/// awaits the sink per event. The queue is bounded ([`ProjectorOptions`]),
/// so the projector holds at most `capacity` events in memory and the run
/// keeps its pace whatever the sink does; an event that finds no room, and
/// every event after the sink fails or stalls, stays in the durable log for
/// [`replay_run`]. The receipt says exactly what the sink did not get.
pub struct EventProjector {
    projection: Mutex<Projection>,
    queue:      Mutex<Queue>,
    pump:       Mutex<Option<JoinHandle<ProjectionReceipt>>>,
}

impl EventProjector {
    /// A projector for a fresh run, with the default options.
    pub fn new(sink: Arc<dyn RunEventSink>) -> Arc<Self> {
        Self::with_options(sink, ProjectorOptions::default())
    }

    /// A projector for a fresh run, with the given queue capacity and stall
    /// budget.
    pub fn with_options(sink: Arc<dyn RunEventSink>, options: ProjectorOptions) -> Arc<Self> {
        Self::with_projection(sink, Projection::new(), options)
    }

    /// A projector for a run being resumed: the stored records are folded
    /// into its state first, and nothing is delivered for them. The resumed
    /// driver then delivers the regenerated suffix and every new record
    /// with the identities a fresh run would have given them.
    ///
    /// # Errors
    ///
    /// The run's logs do not decode or replay.
    pub async fn primed(
        sink: Arc<dyn RunEventSink>,
        logs: &dyn RunLogs,
    ) -> Result<Arc<Self>, ReplayError> {
        Self::primed_with_options(sink, logs, ProjectorOptions::default()).await
    }

    /// [`Self::primed`] over the run directory at `run_dir`.
    pub async fn primed_run_dir(
        sink: Arc<dyn RunEventSink>,
        run_dir: &Path,
    ) -> Result<Arc<Self>, ReplayError> {
        let logs = open_run_dir(run_dir, Access::Read).await?;
        Self::primed(sink, &*logs).await
    }

    /// [`Self::primed`] with the given queue capacity and stall budget.
    ///
    /// # Errors
    ///
    /// The run's logs do not decode or replay.
    pub async fn primed_with_options(
        sink: Arc<dyn RunEventSink>,
        logs: &dyn RunLogs,
        options: ProjectorOptions,
    ) -> Result<Arc<Self>, ReplayError> {
        let mut projection = Projection::new();
        project_run(logs, &mut projection).await?;
        Ok(Self::with_projection(sink, projection, options))
    }

    fn with_projection(
        sink: Arc<dyn RunEventSink>,
        projection: Projection,
        options: ProjectorOptions,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<Box<RunEvent>>(options.capacity.max(1));
        let pump = tokio::spawn(pump(sink, rx, options.stall_timeout));
        Arc::new(Self {
            projection: Mutex::new(projection),
            queue:      Mutex::new(Queue {
                tx:          Some(tx),
                overflowed:  0,
                after_close: 0,
            }),
            pump:       Mutex::new(Some(pump)),
        })
    }

    fn projection(&self) -> MutexGuard<'_, Projection> {
        self.projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue what one record derived. Never waits: an event that finds the
    /// queue full is left to the durable log and counted.
    fn push(&self, events: Vec<RunEvent>) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_millis()).ok());
        let mut queue = self.queue();
        for mut event in events {
            event.observed_at = now;
            let Some(tx) = queue.tx.as_ref() else {
                queue.after_close += 1;
                continue;
            };
            match tx.try_send(Box::new(event)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(event)) => {
                    queue.overflowed += 1;
                    if queue.overflowed == 1 {
                        tracing::warn!(
                            event = %event_label(&event.id),
                            "the event sink fell behind; events that find the queue full are left to the log for replay"
                        );
                    }
                }
                // The pump is gone (it panicked); the receipt says so.
                Err(mpsc::error::TrySendError::Closed(_)) => queue.after_close += 1,
            }
        }
    }

    /// End the stream, await the sink's remaining deliveries and `finish`,
    /// and report. Call once, after the run. Bounded: the pump waits at most
    /// one stall budget for any sink call, and delivers nothing more once a
    /// call stalled or failed.
    pub async fn shutdown(&self) -> ProjectionReceipt {
        // Closing the stream: the pump drains what is queued, then finishes.
        let tx = self.queue().tx.take();
        drop(tx);
        let pump = self
            .pump
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let mut receipt = match pump {
            Some(pump) => pump.await.unwrap_or_else(|error| ProjectionReceipt {
                version: EVENT_CONTRACT_VERSION,
                failure: Some(format!("the event pump failed: {error}")),
                ..ProjectionReceipt::default()
            }),
            None => {
                return ProjectionReceipt {
                    version: EVENT_CONTRACT_VERSION,
                    failure: Some("shutdown was called twice".to_owned()),
                    ..ProjectionReceipt::default()
                };
            }
        };
        let (overflowed, after_close) = {
            let queue = self.queue();
            (queue.overflowed, queue.after_close)
        };
        receipt.projected += overflowed + after_close;
        receipt.undelivered += overflowed + after_close;
        receipt.overflowed = overflowed;
        receipt
    }
}

/// The pump: deliver each queued event to the sink, in order, each call
/// bounded by `stall`; after a failure or a stall, count the rest as
/// undelivered; once the queue closes, `finish` the sink.
async fn pump(
    sink: Arc<dyn RunEventSink>,
    mut rx: mpsc::Receiver<Box<RunEvent>>,
    stall: Duration,
) -> ProjectionReceipt {
    let mut receipt = ProjectionReceipt {
        version: EVENT_CONTRACT_VERSION,
        ..ProjectionReceipt::default()
    };
    while let Some(event) = rx.recv().await {
        receipt.projected += 1;
        if receipt.failure.is_some() {
            receipt.undelivered += 1;
            continue;
        }
        let id = event.id;
        match timeout(stall, sink.deliver(*event)).await {
            Ok(Ok(())) => receipt.delivered += 1,
            Ok(Err(error)) => {
                receipt.undelivered += 1;
                receipt.failure = Some(error.message);
            }
            Err(_) => {
                receipt.undelivered += 1;
                let message = format!(
                    "the sink stalled: {} was not accepted within {}ms, and delivery stopped there",
                    event_label(&id),
                    stall.as_millis()
                );
                tracing::warn!("{message}");
                receipt.failure = Some(message);
            }
        }
    }
    if receipt.failure.is_none() {
        match timeout(stall, sink.finish()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => receipt.failure = Some(error.message),
            Err(_) => {
                receipt.failure = Some(format!(
                    "the sink stalled: `finish` did not return within {}ms",
                    stall.as_millis()
                ));
            }
        }
    }
    receipt
}

/// An event identity as a receipt or a log line names it.
fn event_label(id: &EventId) -> String {
    match id.source {
        EventSource::Coordinator => format!("coordinator record {} event {}", id.seq, id.index),
        EventSource::Execution { execution } => format!(
            "execution {} record {} event {}",
            execution.raw(),
            id.seq,
            id.index
        ),
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
            .engine(execution, record, recorded_at, state);
        self.push(events);
    }

    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        let events = self.projection().lifecycle(record);
        self.push(events);
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

/// Why a run could not be projected from its store.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    EngineLog(#[from] EngineLogError),
}

impl From<store::StoreError> for ReplayError {
    fn from(error: store::StoreError) -> Self {
        Self::Store(error.into())
    }
}

/// Project every event of a run from its store: the coordinator log first,
/// then each execution's engine log in declaration order, each replayed
/// external event by external event so the derivation sees the same
/// post-apply states the live observer saw. Identities equal the live ones.
pub async fn replay_run(logs: &dyn RunLogs) -> Result<Vec<RunEvent>, ReplayError> {
    let mut projection = Projection::new();
    project_run(logs, &mut projection).await
}

/// [`replay_run`] over the run directory at `run_dir`.
pub async fn replay_run_dir(run_dir: &Path) -> Result<Vec<RunEvent>, ReplayError> {
    let logs = open_run_dir(run_dir, Access::Read).await?;
    replay_run(&*logs).await
}

/// The events after a set of per-log positions: the incremental form of
/// [`replay_run`], for a consumer that already holds a prefix of the stream
/// and asks for the rest. `held` names the last [`EventId`] the consumer
/// has from each log; a log it names nothing for is replayed whole. The
/// fold still runs over the whole run, since a suffix cannot be derived
/// without the state the prefix built; only the delivery is trimmed.
pub async fn replay_since(
    logs: &dyn RunLogs,
    held: &BTreeMap<EventSource, EventId>,
) -> Result<Vec<RunEvent>, ReplayError> {
    let events = replay_run(logs).await?;
    Ok(events_after(events, held))
}

/// Keep the events past each log's held position.
pub(crate) fn events_after(
    events: Vec<RunEvent>,
    held: &BTreeMap<EventSource, EventId>,
) -> Vec<RunEvent> {
    events
        .into_iter()
        .filter(|event| {
            held.get(&event.id.source)
                .is_none_or(|last| event.id > *last)
        })
        .collect()
}

/// [`replay_run`] through a caller's projection state.
async fn project_run(
    logs: &dyn RunLogs,
    projection: &mut Projection,
) -> Result<Vec<RunEvent>, ReplayError> {
    Ok(project_loaded(&load_run(logs).await?, projection))
}

/// Project a loaded run: the coordinator log first, then each execution's
/// engine log in declaration order.
pub(crate) fn project_loaded(loaded: &LoadedRun, projection: &mut Projection) -> Vec<RunEvent> {
    let mut events = Vec::new();
    for record in &loaded.coordinator {
        events.extend(projection.lifecycle(record));
    }
    for execution in &loaded.executions {
        events.extend(replay_execution(
            projection,
            execution.execution,
            execution.graph.clone(),
            &execution.log.log,
            &execution.log.recorded_at,
        ));
    }
    events
}

/// One run's durable logs as its store holds them: the coordinator log and,
/// for each execution in declaration order that started, its graph and its
/// engine log.
pub(crate) struct LoadedRun {
    pub(crate) coordinator: Vec<CoordinatorRecord>,
    pub(crate) executions:  Vec<LoadedExecution>,
}

pub(crate) struct LoadedExecution {
    pub(crate) execution: ExecutionId,
    /// The graph the execution started from, before any splice.
    pub(crate) graph:     Graph,
    pub(crate) log:       DecodedEngineLog,
    /// Whether the coordinator recorded the execution's exit: a finished
    /// execution's log is complete, or the crash that cut it is the error.
    pub(crate) finished:  bool,
}

/// Read a run's logs and the graphs its executions ran.
pub(crate) async fn load_run(logs: &dyn RunLogs) -> Result<LoadedRun, ReplayError> {
    let coordinator = read_coordinator_log(logs).await?;
    let state = CoordinatorState::replay(&coordinator)?;
    let mut graphs: BTreeMap<crate::GraphDigest, Graph> = BTreeMap::new();
    let mut executions = Vec::new();
    for (execution, declared) in &state.executions {
        let invocation = declared.declaration.invocation;
        let digest = state.invocations[&invocation].declaration.graph;
        if let Entry::Vacant(entry) = graphs.entry(digest) {
            let bytes = graph_bytes(logs, digest).await?;
            entry.insert(decode_graph(digest, &bytes)?);
        }
        let log = read_execution_log(logs, *execution).await?;
        if log.log.is_empty() {
            // Declared, and nothing stored yet: the execution never started.
            continue;
        }
        executions.push(LoadedExecution {
            execution: *execution,
            graph: graphs[&digest].clone(),
            log,
            finished: declared.exit.is_some(),
        });
    }
    Ok(LoadedRun {
        coordinator,
        executions,
    })
}

/// Project one execution's log through `projection`, external event by
/// external event. `recorded_at` is the log's recording time per seq. Only
/// the stored prefix is published: a regenerated record past the log's end
/// (a crash's lost tail) is applied, so the state is right, and derives no
/// event until a resume stores it.
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
            let stored = usize::try_from(record.seq)
                .ok()
                .filter(|seq| *seq < log.len())
                .and_then(|seq| recorded_at.get(seq).copied());
            if let Some(at) = stored {
                events.extend(projection.engine(execution, record, at, &state));
            }
        }
    }
    events
}

// ── Incremental replay ─────────────────────────────────────────────────

/// A run's replay kept between reads: the form of [`replay_run`] for a
/// consumer that follows a live run through its store. Each
/// [`advance`](Self::advance) reads what the store holds past the records
/// the replay consumed, folds them through the same derivation, and hands
/// back the events of the new records alone, with the identities the full
/// replay gives them. What it keeps is what the derivation needs and the
/// full replay rebuilds on every call: the coordinator state, each
/// execution's engine state with its log, and the [`Projection`].
///
/// Per log, the events of a replay advanced over a run in any number of
/// reads are the events of [`replay_run`], in the same order; across logs
/// only the interleaving differs, since an advance hands back each log's
/// new events together (the coordinator log first, then each execution in
/// declaration order, as the full replay does). A record a crash left an
/// execution's log short of is applied, so the state is right, and derives
/// no event until a later advance finds it stored, as under the full
/// replay; the records of an execution the coordinator log has not
/// declared yet wait for the declaration.
///
/// The state is a cache of the records: a fresh replay rebuilds it, and
/// it holds every engine record of the run in memory, so a consumer drops
/// it when the run ends or goes idle. An advance reads everything before
/// it folds anything, so an advance that fails leaves the replay as it
/// was, to be retried once the store is whole.
#[derive(Default)]
pub struct RunReplay {
    projection:  Projection,
    coordinator: CoordinatorState,
    /// Coordinator records consumed: the seq the next read starts at.
    consumed:    usize,
    /// The graphs read so far, so a second execution of one graph reads no
    /// blob.
    graphs:      BTreeMap<GraphDigest, Graph>,
    executions:  BTreeMap<ExecutionId, ExecutionReplay>,
}

/// One execution's replay: its engine state, and how far into its stored
/// log the events were published.
struct ExecutionReplay {
    state:     EngineState,
    /// Stored records whose events were published: every seq below it. The
    /// state's log may run past it, by the records the last external apply
    /// regenerated before the log held them.
    published: usize,
}

/// What one advance read of an execution's log, before the fold.
struct ExecutionTail {
    execution: ExecutionId,
    /// The graph the execution started from, read for an execution seen
    /// for the first time.
    start:     Option<(GraphDigest, Graph)>,
    records:   Vec<(EventRecord, u64)>,
}

impl RunReplay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the records past the ones consumed, fold them, and hand back
    /// their events: the coordinator log's first, then each execution's in
    /// declaration order. Empty when the store holds nothing new.
    ///
    /// # Errors
    ///
    /// What [`replay_run`] fails with over the same store: a log that does
    /// not read or decode, a coordinator record the state rejects, a gap in
    /// a log (a torn tail). The replay is unchanged by a failed advance.
    pub async fn advance(&mut self, logs: &dyn RunLogs) -> Result<Vec<RunEvent>, ReplayError> {
        // Every read and decode happens here, before the fold below changes
        // anything, so a failure leaves the replay where it stood.
        let coordinator = self.read_coordinator(logs).await?;
        let mut state = self.coordinator.clone();
        for (offset, record) in coordinator.iter().enumerate() {
            let index = self.consumed + offset;
            if record.seq != index as u64 {
                return Err(StateError::Sequence {
                    index,
                    found: record.seq,
                }
                .into());
            }
            state.apply(&record.body)?;
        }
        if state.root.is_none() {
            return Err(StateError::MissingRunStart.into());
        }
        let mut tails = Vec::new();
        for (execution, declared) in &state.executions {
            let replay = self.executions.get(execution);
            let published = replay.map_or(0, |replay| replay.published);
            let stored = logs
                .read_from(&LogId::Execution(*execution), published as u64)
                .await?;
            if stored.is_empty() {
                continue;
            }
            let records = decode_engine_tail(*execution, published, &stored)?;
            let start = if replay.is_some() {
                None
            } else {
                let digest = state.invocations[&declared.declaration.invocation]
                    .declaration
                    .graph;
                let graph = match self.graphs.get(&digest) {
                    Some(graph) => graph.clone(),
                    None => decode_graph(digest, &graph_bytes(logs, digest).await?)?,
                };
                Some((digest, graph))
            };
            tails.push(ExecutionTail {
                execution: *execution,
                start,
                records,
            });
        }

        let mut events = Vec::new();
        for record in &coordinator {
            events.extend(self.projection.lifecycle(record));
        }
        self.coordinator = state;
        self.consumed += coordinator.len();
        for tail in tails {
            let replay = match self.executions.entry(tail.execution) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let Some((digest, graph)) = tail.start else {
                        continue;
                    };
                    self.graphs.entry(digest).or_insert_with(|| graph.clone());
                    entry.insert(ExecutionReplay {
                        state:     EngineState::new(graph),
                        published: 0,
                    })
                }
            };
            events.extend(replay.advance(&mut self.projection, tail.execution, tail.records));
        }
        Ok(events)
    }

    /// The coordinator records past the consumed ones, decoded under the
    /// rules of [`read_coordinator_log`]: the format check reads the log's
    /// first record, so the first read takes the log whole.
    async fn read_coordinator(
        &self,
        logs: &dyn RunLogs,
    ) -> Result<Vec<CoordinatorRecord>, ReplayError> {
        if self.consumed == 0 {
            return read_coordinator_log(logs).await.map_err(ReplayError::from);
        }
        let stored = logs
            .read_from(&LogId::Coordinator, self.consumed as u64)
            .await?;
        stored
            .iter()
            .map(|record| decode_coordinator_record(record).map_err(ReplayError::from))
            .collect()
    }
}

impl ExecutionReplay {
    /// Fold a tail of the execution's stored log, contiguous from
    /// `published`, and hand back the events of the records it holds, as
    /// [`replay_execution`] derives them: external event by external event
    /// with the post-apply state, each record published once the log holds
    /// it, with the recording time the log gives it.
    fn advance(
        &mut self,
        projection: &mut Projection,
        execution: ExecutionId,
        tail: Vec<(EventRecord, u64)>,
    ) -> Vec<RunEvent> {
        let published = self.published;
        let times: Vec<u64> = tail.iter().map(|(_, at)| *at).collect();
        let stored_time = |seq: u64| {
            usize::try_from(seq)
                .ok()
                .and_then(|seq| seq.checked_sub(published))
                .and_then(|index| times.get(index).copied())
        };
        let mut events = Vec::new();
        // The next seq to publish: an apply publishes the records it
        // produced as far as the log holds them, and the tail's own copies
        // of those records are then passed over.
        let mut next = published;
        for (record, recorded_at) in tail {
            let seq = usize::try_from(record.seq).unwrap_or(usize::MAX);
            if seq < next {
                continue;
            }
            if let Some(held) = self.state.log.records().get(seq) {
                // Regenerated by the last external apply before the log held
                // it: published now, with that apply's state, which no later
                // external event has moved.
                events.extend(projection.engine(execution, held, recorded_at, &self.state));
                next = seq + 1;
                continue;
            }
            if record.origin != EventOrigin::External {
                continue;
            }
            let before = self.state.log.len();
            let state = mem::replace(&mut self.state, EngineState::new(Graph::new()));
            let (applied, _) = engine::apply(state, record.event);
            self.state = applied;
            for produced in &self.state.log.records()[before..] {
                // A record past the stored log derives no event until a
                // later advance finds it stored.
                let Some(at) = stored_time(produced.seq) else {
                    continue;
                };
                events.extend(projection.engine(execution, produced, at, &self.state));
                next = usize::try_from(produced.seq).map_or(next, |seq| seq + 1);
            }
        }
        self.published = published + times.len();
        events
    }
}

/// Decode the stored tail of an execution's log, contiguous from `from`,
/// into engine records with their recording times, under the rules of
/// [`decode_engine_records`](crate::decode_engine_records): a line that
/// does not decode and a gap in the seqs are the errors the full decode
/// reports, at the same line numbers.
fn decode_engine_tail(
    execution: ExecutionId,
    from: usize,
    stored: &[store::Record],
) -> Result<Vec<(EventRecord, u64)>, ReplayError> {
    let decode_error = |source| EngineLogError::Decode { execution, source };
    let mut records = Vec::with_capacity(stored.len());
    for (offset, line) in stored.iter().enumerate() {
        let index = from + offset;
        let stored: StoredEngineRecord = line.decode().map_err(|source| {
            decode_error(EngineLogDecodeError::BadRecord {
                line: index + 1,
                source,
            })
        })?;
        if stored.seq != index as u64 {
            return Err(decode_error(
                engine::InvalidRecords::SeqMismatch {
                    index,
                    found: stored.seq,
                }
                .into(),
            )
            .into());
        }
        records.push(stored.into_parts());
    }
    Ok(records)
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
    fn budget_notes_and_hook_activity_read_beside_the_note_and_other_notes_stay_bare() {
        let paused = Note::new(
            BUDGET_PAUSED_KIND,
            json!({ "attempt": 1, "remaining_ms": 4000, "pending_questions": 1 }),
        );
        let Parsed::Note { budget, .. } = note_parsed(paused.clone()) else {
            panic!("a note");
        };
        assert_eq!(
            budget,
            Some(BudgetReading {
                state: BudgetState::Paused,
                note:  BudgetNote {
                    attempt:           Attempt::new(1),
                    remaining_ms:      4000,
                    pending_questions: 1,
                },
            })
        );
        let resumed = Note::new(
            BUDGET_RESUMED_KIND,
            json!({ "attempt": 1, "remaining_ms": 4000, "pending_questions": 0 }),
        );
        let Parsed::Note { budget, .. } = note_parsed(resumed) else {
            panic!("a note");
        };
        assert_eq!(
            budget.map(|budget| budget.state),
            Some(BudgetState::Resumed)
        );
        let hook = Note::new("hook", json!({ "point": "before_attempt" }));
        assert_eq!(note_parsed(hook.clone()), Parsed::Note {
            note:          hook,
            hook_activity: None,
            budget:        None,
        });
        let malformed = Note::new(BUDGET_PAUSED_KIND, json!("not a budget"));
        assert!(matches!(note_parsed(malformed), Parsed::Note {
            budget: None,
            ..
        }));
    }

    /// The step protocols Petri owns parse; a backend's own event, a log
    /// line and an artifact do not: they are forwarded as recorded.
    #[test]
    fn only_petri_s_own_protocols_parse() {
        let backend = StepEvent::Custom(json!({
            "kind": "pebble",
            "event": { "session_id": "ses_1", "seq": 3, "event": { "TurnStarted": {} } },
        }));
        assert!(parse_progress(&backend).is_none());
        assert!(
            parse_progress(&StepEvent::Log {
                stream: ir::LogStream::Stdout,
                line:   "x".into(),
            })
            .is_none()
        );
        let note = StepEvent::Custom(json!({ "$note": { "kind": "transition", "payload": 1 } }));
        assert!(matches!(parse_progress(&note), Some(Parsed::Note { .. })));
    }

    /// A record's own event carries the stored line verbatim, the derived
    /// values apart from it, and the envelope repeats the record's identity.
    #[test]
    fn a_record_event_carries_its_line_and_derived_values_apart() {
        let record = CoordinatorRecord::external(3, 1_789, CoordinatorEvent::RunNoteRecorded {
            execution: Some(ExecutionId::new(1)),
            kind:      "hook".into(),
            payload:   json!({ "point": "run_finished" }),
        });
        let events = Projection::new().lifecycle(&record);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.origin, RecordOrigin::External);
        assert_eq!(event.recorded_at, 1_789);
        assert_eq!(event.context.execution, Some(ExecutionId::new(1)));
        let json = serde_json::to_value(event).expect("encodes");
        assert_eq!(
            json["id"],
            json!({ "log": "coordinator", "seq": 3, "index": 0 })
        );
        assert_eq!(
            json["record"],
            serde_json::to_value(&record).expect("encodes"),
            "the record is the stored line"
        );
        assert_eq!(json["record"]["body"]["event"], json!("run.note.recorded"));
        assert_eq!(json["derived"]["parsed"]["kind"], json!("note"));
        let back: RunEvent = serde_json::from_value(json).expect("decodes");
        assert_eq!(&back, event);
    }
}
