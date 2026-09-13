//! The inverse of the projection: the durable records a public stream
//! derives from, and the proof that the round trip loses nothing.
//!
//! [`invert`] reads the first event derived from each record (`index` 0)
//! and rebuilds the record. The core's own records (routed tokens, applied
//! routes, splices, cascading cancels) derive events too, but replay
//! regenerates them from the external records, so inversion skips them by
//! their [`RecordOrigin`] and yields the external records alone.
//! [`verify_lossless`] proves the round trip over a run dir: project the run,
//! invert the stream, replay the inverted records through the engine, and
//! compare the regenerated logs and the re-projected stream with the
//! originals.

use std::collections::BTreeMap;
use std::path::Path;

use driver::lifecycle::{BUDGET_PAUSED_KIND, BUDGET_RESUMED_KIND, BudgetNote, Note};
use engine::{
    DecisionId, EngineStart, EngineState, Event, EventLog, EventOrigin, EventRecord, GroupDecision,
};
use ir::{Attempt, Control, FiringId, Graph, StepEvent, Value};
use serde::Serialize;
use steps::QuestionExpired;

use super::{
    AgentActivity, BACKEND_EVENT_KIND_KEY, DeliveredControl, EventBody, EventId, EventSource,
    Projection, RecordOrigin, ReplayError, RunEvent, load_run, project_loaded, replay_execution,
};
use crate::hooks::{HOOK_ACTIVITY_NOTE_KIND, HookActivity};
use crate::{CoordinatorEvent, CoordinatorRecord, ExecutionId, GraphDigest, ParentCallKey};

/// An external engine record rebuilt from the events it derived, with the
/// time it was recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct InvertedRecord {
    pub record:      EventRecord,
    /// Absent for a record replay regenerated that never reached a log.
    pub recorded_at: Option<u64>,
}

/// The records a stream derives from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InvertedRun {
    /// The coordinator records, in seq order.
    pub coordinator: Vec<CoordinatorRecord>,
    /// Each execution's external engine records, in seq order.
    pub executions:  BTreeMap<ExecutionId, Vec<InvertedRecord>>,
}

/// Why an event could not be turned back into its record.
#[derive(Debug, thiserror::Error)]
pub enum InvertError {
    #[error("event {id:?} names no subject, so its firing is unknown")]
    MissingSubject { id: EventId },
    #[error("event {id:?} names no firing")]
    MissingFiring { id: EventId },
    #[error("event {id:?} names no attempt")]
    MissingAttempt { id: EventId },
    #[error("coordinator event {id:?} carries no recording time")]
    MissingRecordedAt { id: EventId },
    #[error("event {id:?} cancels neither a scope nor a group")]
    CancelWithoutTarget { id: EventId },
    #[error("event {id:?} names an invalid graph digest `{digest}`")]
    BadDigest { id: EventId, digest: String },
    #[error("event {id:?} is not the first event of a record of its log")]
    UnexpectedBody { id: EventId },
    #[error("event {id:?} cannot be encoded as a record")]
    Encode {
        id:     EventId,
        #[source]
        source: serde_json::Error,
    },
}

/// Rebuild the records a stream derives from: every coordinator record, and
/// every execution's external engine records. Only the first event of each
/// record (`index` 0) is read; the rest restate it. An event of a core record
/// is skipped by its origin. Events are deduplicated by [`EventId`], so an
/// at-least-once delivery inverts the same.
pub fn invert(events: &[RunEvent]) -> Result<InvertedRun, InvertError> {
    let mut run = InvertedRun::default();
    for event in events.iter().filter(|event| event.id.index == 0) {
        match event.id.source {
            EventSource::Coordinator => run.coordinator.push(coordinator_record(event)?),
            EventSource::Execution { .. } if event.origin == RecordOrigin::Core => {}
            EventSource::Execution { execution } => {
                run.executions
                    .entry(execution)
                    .or_default()
                    .push(InvertedRecord {
                        record:      EventRecord {
                            seq:    event.id.seq,
                            origin: EventOrigin::External,
                            event:  engine_event(event)?,
                        },
                        recorded_at: event.recorded_at,
                    });
            }
        }
    }
    run.coordinator.sort_by_key(|record| record.seq);
    run.coordinator.dedup_by_key(|record| record.seq);
    for records in run.executions.values_mut() {
        records.sort_by_key(|record| record.record.seq);
        records.dedup_by_key(|record| record.record.seq);
    }
    Ok(run)
}

fn coordinator_record(event: &RunEvent) -> Result<CoordinatorRecord, InvertError> {
    let id = event.id;
    let recorded_at = event
        .recorded_at
        .ok_or(InvertError::MissingRecordedAt { id })?;
    let coordinator = match &event.body {
        EventBody::RunStarted {
            format_version,
            root,
            middleware_chain,
        } => CoordinatorEvent::RunStarted {
            format_version:   *format_version,
            root:             *root,
            middleware_chain: middleware_chain.clone(),
        },
        EventBody::GraphRegistered { digest } => CoordinatorEvent::GraphRegistered {
            digest: parse_digest(id, digest)?,
        },
        EventBody::InvocationDeclared {
            invocation,
            call,
            graph,
            sandbox,
            context,
            secret_bindings,
            admission,
        } => CoordinatorEvent::InvocationDeclared {
            invocation:      *invocation,
            call:            call.as_ref().map(|link| ParentCallKey {
                parent:  link.execution,
                firing:  link.firing,
                attempt: link.attempt,
                slot:    link.slot.clone(),
            }),
            graph:           parse_digest(id, graph)?,
            context:         context.clone(),
            secret_bindings: secret_bindings.clone(),
            sandbox:         *sandbox,
            admission:       admission.clone(),
        },
        EventBody::ExecutionDeclared {
            execution,
            invocation,
            predecessor,
            execution_index,
            entry,
            context,
            prior_firings,
            max_executions,
            middleware_state,
        } => CoordinatorEvent::ExecutionDeclared {
            execution:        *execution,
            invocation:       *invocation,
            predecessor:      *predecessor,
            start:            EngineStart {
                entry:           *entry,
                context:         context.clone(),
                prior_firings:   prior_firings.clone(),
                execution_index: *execution_index,
                max_executions:  *max_executions,
            },
            middleware_state: middleware_state.clone(),
        },
        EventBody::ExecutionFinished { execution, exit } => CoordinatorEvent::ExecutionFinished {
            execution: *execution,
            exit:      exit.clone(),
        },
        EventBody::InvocationFinished { invocation, result } => {
            CoordinatorEvent::InvocationFinished {
                invocation: *invocation,
                result:     result.clone(),
            }
        }
        EventBody::InvocationCancelRequested { invocation, reason } => {
            CoordinatorEvent::InvocationCancelRequested {
                invocation: *invocation,
                reason:     reason.clone(),
            }
        }
        EventBody::RunPaused => CoordinatorEvent::RunPaused,
        EventBody::RunUnpaused => CoordinatorEvent::RunUnpaused,
        EventBody::RunFinished { status } => CoordinatorEvent::RunFinished { status: *status },
        body => {
            let note = note_of(id, body, None)?.ok_or(InvertError::UnexpectedBody { id })?;
            CoordinatorEvent::RunNoteRecorded {
                execution: event.execution,
                kind:      note.kind,
                payload:   note.payload,
            }
        }
    };
    Ok(CoordinatorRecord::external(
        id.seq,
        recorded_at,
        coordinator,
    ))
}

/// The external engine record the first event of one derives from.
fn engine_event(event: &RunEvent) -> Result<Event, InvertError> {
    let id = event.id;
    let engine = match &event.body {
        EventBody::ExecutionStarted {
            entry,
            execution_index,
            context,
            prior_firings,
            max_executions,
        } => Event::ExecutionStarted {
            start: EngineStart {
                entry:           *entry,
                context:         context.clone(),
                prior_firings:   prior_firings.clone(),
                execution_index: *execution_index,
                max_executions:  *max_executions,
            },
        },
        EventBody::ExecutionAdmitted {
            decision, trace, ..
        } => Event::AdmissionDecided {
            decision_id: DecisionId::ExecutionStart,
            decision:    decision.clone(),
            trace:       trace.clone(),
        },
        EventBody::AttemptAdmitted { decision, trace } => Event::AdmissionDecided {
            decision_id: DecisionId::attempt_start(firing_of(event)?, attempt_of(event)?),
            decision:    decision.clone(),
            trace:       trace.clone(),
        },
        EventBody::AttemptStarted => Event::StepStarted {
            firing:  firing_of(event)?,
            attempt: attempt_of(event)?,
        },
        EventBody::AttemptFinished { outcome, .. } => Event::StepFinished {
            firing:  firing_of(event)?,
            attempt: attempt_of(event)?,
            outcome: outcome.clone(),
        },
        EventBody::RetryElapsed { next_attempt } => Event::RetryElapsed {
            firing:       firing_of(event)?,
            next_attempt: *next_attempt,
        },
        EventBody::RoutesResolved { choices } => Event::RoutingResolved {
            decision_id: DecisionId::route(firing_of(event)?, attempt_of(event)?),
            groups:      choices
                .iter()
                .map(|choice| GroupDecision {
                    group:    choice.group,
                    draw:     choice.draw.clone(),
                    trace:    choice.trace.clone(),
                    decision: choice.decision.clone(),
                })
                .collect(),
        },
        EventBody::CancelRequested {
            scope: Some(scope), ..
        } => Event::cancel_scope(*scope),
        EventBody::CancelRequested {
            scope: None,
            group: Some(group),
        } => Event::cancel_group(group.id),
        EventBody::CancelRequested {
            scope: None,
            group: None,
        } => return Err(InvertError::CancelWithoutTarget { id }),
        EventBody::KillRequested { scope } => Event::KillRequested { scope: *scope },
        EventBody::ControlDelivered { control, .. } => Event::ControlRequested {
            firing: firing_of(event)?,
            ctl:    match control {
                DeliveredControl::Answer { value, .. } | DeliveredControl::Deliver { value } => {
                    Control::Deliver(value.clone())
                }
                DeliveredControl::Cancel => Control::Cancel,
                DeliveredControl::Kill => Control::Kill,
            },
        },
        EventBody::OutputLine { stream, line } => progress(event, StepEvent::Log {
            stream: *stream,
            line:   line.clone(),
        })?,
        EventBody::ArtifactRecorded { name, uri } => progress(event, StepEvent::Artifact {
            name: name.clone(),
            uri:  uri.clone(),
        })?,
        EventBody::QuestionAsked { question } => progress(event, question.to_event())?,
        EventBody::QuestionExpired {
            question,
            waited_ms,
            default,
        } => progress(
            event,
            QuestionExpired {
                question:  question.clone(),
                waited_ms: *waited_ms,
                default:   default.clone(),
            }
            .to_event(),
        )?,
        EventBody::AgentActivity(activity) => {
            progress(event, StepEvent::Custom(backend_payload(activity)))?
        }
        EventBody::StepCustom { value } => progress(event, StepEvent::Custom(value.clone()))?,
        body @ (EventBody::HostNote { .. }
        | EventBody::HookActivity { .. }
        | EventBody::BudgetPaused { .. }
        | EventBody::BudgetResumed { .. }) => {
            let attempt = event.subject.as_ref().and_then(|subject| subject.attempt);
            let note = note_of(id, body, attempt)?.ok_or(InvertError::UnexpectedBody { id })?;
            progress(event, note.to_step_event())?
        }
        // The first event of an external engine record is never one of
        // these: they derive from the core's own records, from the state
        // after a record, or from a coordinator record.
        EventBody::VisitStarted { .. }
        | EventBody::VisitCompleted { .. }
        | EventBody::RouteApplied { .. }
        | EventBody::ForkStarted { .. }
        | EventBody::BranchCompleted { .. }
        | EventBody::ForkCompleted { .. }
        | EventBody::NodeExpanded { .. }
        | EventBody::WaitStateChanged { .. }
        | EventBody::RetryScheduled { .. }
        | EventBody::RunStarted { .. }
        | EventBody::RunFinished { .. }
        | EventBody::GraphRegistered { .. }
        | EventBody::InvocationDeclared { .. }
        | EventBody::InvocationFinished { .. }
        | EventBody::InvocationCancelRequested { .. }
        | EventBody::StallTimeout { .. }
        | EventBody::RunPaused
        | EventBody::RunUnpaused
        | EventBody::ExecutionDeclared { .. }
        | EventBody::ExecutionFinished { .. } => return Err(InvertError::UnexpectedBody { id }),
    };
    Ok(engine)
}

fn firing_of(event: &RunEvent) -> Result<FiringId, InvertError> {
    let id = event.id;
    event
        .subject
        .as_ref()
        .ok_or(InvertError::MissingSubject { id })?
        .firing
        .ok_or(InvertError::MissingFiring { id })
}

fn attempt_of(event: &RunEvent) -> Result<Attempt, InvertError> {
    let id = event.id;
    event
        .subject
        .as_ref()
        .and_then(|subject| subject.attempt)
        .ok_or(InvertError::MissingAttempt { id })
}

fn progress(event: &RunEvent, ev: StepEvent) -> Result<Event, InvertError> {
    Ok(Event::StepProgressRecorded {
        firing: firing_of(event)?,
        ev,
    })
}

/// The note a note-derived body restates, if the body is one. `attempt` is
/// the subject's, which a budget note records.
fn note_of(
    id: EventId,
    body: &EventBody,
    attempt: Option<Attempt>,
) -> Result<Option<Note>, InvertError> {
    Ok(Some(match body {
        EventBody::HostNote { kind, payload } => Note::new(kind.clone(), payload.clone()),
        EventBody::HookActivity { hook, activity } => Note::new(
            HOOK_ACTIVITY_NOTE_KIND,
            encode(id, &HookActivity {
                hook:     hook.clone(),
                backend:  activity.backend.clone(),
                envelope: activity.envelope.clone(),
            })?,
        ),
        EventBody::BudgetPaused {
            remaining_ms,
            pending_questions,
        } => budget_note(
            id,
            BUDGET_PAUSED_KIND,
            attempt,
            *remaining_ms,
            *pending_questions,
        )?,
        EventBody::BudgetResumed { remaining_ms } => {
            budget_note(id, BUDGET_RESUMED_KIND, attempt, *remaining_ms, 0)?
        }
        _ => return Ok(None),
    }))
}

fn budget_note(
    id: EventId,
    kind: &str,
    attempt: Option<Attempt>,
    remaining_ms: u64,
    pending_questions: u32,
) -> Result<Note, InvertError> {
    let attempt = attempt.ok_or(InvertError::MissingAttempt { id })?;
    Ok(Note::new(
        kind,
        encode(id, &BudgetNote {
            attempt,
            remaining_ms,
            pending_questions,
        })?,
    ))
}

/// The custom payload a backend's activity was read from: `kind`, the step's
/// own attributes in their order, then the envelope under `event`.
fn backend_payload(activity: &AgentActivity) -> Value {
    let mut payload = serde_json::Map::new();
    payload.insert(
        BACKEND_EVENT_KIND_KEY.to_owned(),
        Value::String(activity.backend.to_string()),
    );
    for (key, value) in &activity.attributes {
        payload.insert(key.clone(), value.clone());
    }
    payload.insert("event".to_owned(), activity.envelope.clone());
    Value::Object(payload)
}

fn parse_digest(id: EventId, digest: &str) -> Result<GraphDigest, InvertError> {
    digest.parse().map_err(|_| InvertError::BadDigest {
        id,
        digest: digest.to_owned(),
    })
}

fn encode<T: Serialize>(id: EventId, value: &T) -> Result<Value, InvertError> {
    serde_json::to_value(value).map_err(|source| InvertError::Encode { id, source })
}

// ── Verification ───────────────────────────────────────────────────────────

/// Why a run dir's stream did not round-trip to its logs.
#[derive(Debug, thiserror::Error)]
pub enum LosslessError {
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error(transparent)]
    Invert(#[from] InvertError),
    #[error(
        "the inverted coordinator log differs from the stored one{}",
        at_seq(*seq)
    )]
    Coordinator { seq: Option<u64> },
    #[error(
        "execution {execution}: the inverted external records differ from the stored ones{}",
        at_seq(*seq)
    )]
    External {
        execution: ExecutionId,
        seq:       Option<u64>,
    },
    #[error(
        "execution {execution}: replaying the inverted records regenerated a log that differs \
         from the stored one{}",
        at_seq(*seq)
    )]
    Regenerated {
        execution: ExecutionId,
        seq:       Option<u64>,
    },
    #[error(
        "re-projecting the inverted run derived a stream that differs from the original{}",
        id.map(|id| format!(" at {id:?}")).unwrap_or_default()
    )]
    Reprojected { id: Option<EventId> },
}

fn at_seq(seq: Option<u64>) -> String {
    seq.map(|seq| format!(" at seq {seq}")).unwrap_or_default()
}

/// Prove that a run dir's public stream loses nothing replay needs: project
/// the run, invert the stream, and check that the inverted coordinator log
/// and external engine records equal the stored ones, that replaying the
/// inverted records regenerates each stored engine log whole, and that the
/// regenerated logs project to the same stream. Returns the inverted run.
pub fn verify_lossless(run_dir: &Path) -> Result<InvertedRun, LosslessError> {
    let loaded = load_run(run_dir)?;
    let events = project_loaded(&loaded, &mut Projection::new());
    let inverted = invert(&events)?;

    if let Some(at) = first_difference(&inverted.coordinator, &loaded.coordinator.records) {
        return Err(LosslessError::Coordinator { seq: at.seq() });
    }

    let mut regenerated: Vec<(ExecutionId, Graph, EventLog, Vec<u64>)> = Vec::new();
    for execution in &loaded.executions {
        let stored: Vec<InvertedRecord> = execution
            .log
            .log
            .records()
            .iter()
            .filter(|record| record.origin == EventOrigin::External)
            .map(|record| InvertedRecord {
                record:      record.clone(),
                recorded_at: usize::try_from(record.seq)
                    .ok()
                    .and_then(|seq| execution.log.recorded_at.get(seq).copied()),
            })
            .collect();
        let empty = Vec::new();
        let records = inverted
            .executions
            .get(&execution.execution)
            .unwrap_or(&empty);
        if let Some(at) = first_difference(records, &stored) {
            return Err(LosslessError::External {
                execution: execution.execution,
                seq:       at.seq(),
            });
        }
        let (log, recorded_at) = regenerate(execution.graph.clone(), records);
        if let Some(at) = first_difference(log.records(), execution.log.log.records()) {
            return Err(LosslessError::Regenerated {
                execution: execution.execution,
                seq:       at.seq(),
            });
        }
        regenerated.push((
            execution.execution,
            execution.graph.clone(),
            log,
            recorded_at,
        ));
    }

    let mut projection = Projection::new();
    let mut reprojected = Vec::new();
    for record in &inverted.coordinator {
        reprojected.extend(projection.lifecycle(record));
    }
    for (execution, graph, log, recorded_at) in &regenerated {
        reprojected.extend(replay_execution(
            &mut projection,
            *execution,
            graph.clone(),
            log,
            recorded_at,
        ));
    }
    if reprojected != events {
        let id = reprojected
            .iter()
            .zip(&events)
            .find(|(a, b)| a != b)
            .map(|(a, _)| a.id)
            .or_else(|| events.get(reprojected.len()).map(|event| event.id));
        return Err(LosslessError::Reprojected { id });
    }
    Ok(inverted)
}

/// Replay external records through the engine from the graph the execution
/// started with: the regenerated log, whole, and each record's recording
/// time. The core's records carry the time of the external record whose
/// apply produced them, as the driver stamps them.
fn regenerate(graph: Graph, records: &[InvertedRecord]) -> (EventLog, Vec<u64>) {
    let mut state = EngineState::new(graph);
    let mut recorded_at = Vec::new();
    for record in records {
        let (next, _) = engine::apply(state, record.record.event.clone());
        state = next;
        let at = record.recorded_at.unwrap_or(0);
        recorded_at.resize(state.log.len(), at);
    }
    (state.log, recorded_at)
}

/// Where two record sequences first differ, as the seq an error names:
/// `Some(Some(seq))` at a record, `Some(None)` for a length difference past a
/// common prefix, `None` when they are equal.
fn first_difference<T: PartialEq + Seq>(a: &[T], b: &[T]) -> Option<Difference> {
    for (x, y) in a.iter().zip(b) {
        if x != y {
            return Some(Difference::At(x.seq()));
        }
    }
    (a.len() != b.len()).then_some(Difference::Length)
}

/// Where two record sequences part.
enum Difference {
    /// At the record with this seq.
    At(u64),
    /// Past a common prefix: one is longer.
    Length,
}

impl Difference {
    fn seq(&self) -> Option<u64> {
        match self {
            Self::At(seq) => Some(*seq),
            Self::Length => None,
        }
    }
}

trait Seq {
    fn seq(&self) -> u64;
}

impl Seq for CoordinatorRecord {
    fn seq(&self) -> u64 {
        self.seq
    }
}

impl Seq for EventRecord {
    fn seq(&self) -> u64 {
        self.seq
    }
}

impl Seq for InvertedRecord {
    fn seq(&self) -> u64 {
        self.record.seq
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::slice;

    use engine::{EngineStart, EntryPoint};
    use ir::RunStatus;

    use super::*;
    use crate::{
        COORDINATOR_FORMAT_VERSION, CoordinatorEvent, InvocationId, SandboxBinding, SecretBindings,
    };

    fn record(seq: u64, event: CoordinatorEvent) -> CoordinatorRecord {
        CoordinatorRecord::external(seq, 1_000 + seq, event)
    }

    #[test]
    fn coordinator_records_round_trip_through_their_events() {
        let digest: GraphDigest = "ab".repeat(32).parse().expect("a digest");
        let records = vec![
            record(0, CoordinatorEvent::RunStarted {
                format_version:   COORDINATOR_FORMAT_VERSION,
                root:             InvocationId::ROOT,
                middleware_chain: Vec::new(),
            }),
            record(1, CoordinatorEvent::GraphRegistered { digest }),
            record(2, CoordinatorEvent::InvocationDeclared {
                invocation:      InvocationId::ROOT,
                call:            None,
                graph:           digest,
                context:         BTreeMap::new(),
                secret_bindings: SecretBindings::None,
                sandbox:         SandboxBinding::Isolated,
                admission:       None,
            }),
            record(3, CoordinatorEvent::ExecutionDeclared {
                execution:        ExecutionId::new(1),
                invocation:       InvocationId::ROOT,
                predecessor:      None,
                start:            EngineStart {
                    entry:           EntryPoint::GraphEntries,
                    context:         BTreeMap::new(),
                    prior_firings:   BTreeMap::new(),
                    execution_index: 1,
                    max_executions:  4,
                },
                middleware_state: BTreeMap::new(),
            }),
            record(4, CoordinatorEvent::RunNoteRecorded {
                execution: Some(ExecutionId::new(1)),
                kind:      "hook".into(),
                payload:   serde_json::json!({ "point": "run_finished" }),
            }),
            record(5, CoordinatorEvent::RunPaused),
            record(6, CoordinatorEvent::RunUnpaused),
            record(7, CoordinatorEvent::RunFinished {
                status: RunStatus::Success,
            }),
        ];
        let mut projection = Projection::new();
        let events: Vec<RunEvent> = records
            .iter()
            .flat_map(|record| projection.lifecycle(record))
            .collect();
        assert_eq!(
            events.len(),
            records.len(),
            "every coordinator record derives exactly one event"
        );
        let inverted = invert(&events).expect("the events invert");
        assert_eq!(inverted.coordinator, records);
        assert!(inverted.executions.is_empty());
    }

    #[test]
    fn an_engine_event_without_a_subject_is_refused() {
        let event = RunEvent {
            id:          EventId {
                source: EventSource::Execution {
                    execution: ExecutionId::new(1),
                },
                seq:    3,
                index:  0,
            },
            origin:      RecordOrigin::External,
            invocation:  None,
            execution:   Some(ExecutionId::new(1)),
            parent:      None,
            subject:     None,
            observed_at: None,
            recorded_at: Some(7),
            body:        EventBody::AttemptStarted,
        };
        let error = invert(slice::from_ref(&event)).expect_err("no subject, no firing");
        assert!(matches!(error, InvertError::MissingSubject { id } if id == event.id));
    }
}
