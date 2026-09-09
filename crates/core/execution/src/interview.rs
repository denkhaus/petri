//! Questions from a run, answers from a host: the interview boundary.
//!
//! A step that needs a person emits a [`Question`] on its progress channel
//! and waits on its control channel. Which person, and how they are reached,
//! is the host's business: a terminal prompt, a scripted fixture, a product
//! inbox. The host supplies an [`Interviewer`]; the [`InterviewDispatcher`]
//! owns everything around it that is the same for every host.
//!
//! The dispatcher subscribes to the coordinator as an [`ExecutionObserver`],
//! correlates each question with its invocation, execution, firing, node
//! instance and occurrence, hands the interviewer one [`InterviewRequest`] per
//! question on its own task, registers a sensitive answer as a secret before
//! anything else sees it, delivers the answer through the
//! [`CoordinatorHandle`], refuses late and duplicate answers, and on
//! [`InterviewDispatcher::shutdown`] ends every pending wait and writes the
//! [`InterviewReceipt`] a host persists beside the run.
//!
//! # Concurrency
//!
//! Questions from parallel stages reach the interviewer concurrently, one
//! [`Interviewer::reply`] call per question, each on its own task. An
//! interviewer that must serialize (a terminal has one keyboard) does so
//! itself. The observer callback never blocks: it records the question and
//! spawns.
//!
//! # Cancellation
//!
//! The `cancel` token an interviewer receives fires when the question's
//! firing finishes without the answer (the step timed out, the run was
//! cancelled, a sibling failed the branch) and when the dispatcher shuts
//! down. An interviewer returns promptly once it fires; a reply that arrives
//! anyway is recorded as late and not delivered.
//!
//! # Late and duplicate answers
//!
//! A reply for a firing that has already finished is late: recorded as an
//! error, never delivered. A question event that repeats the id of a question
//! whose reply is still pending is a duplicate: ignored. A question event that
//! repeats an id after its answer was delivered is the step asking again (the
//! answer it got named no choice); the interviewer sees it as a new request
//! with the same `occurrence` and a higher `ask`, so a scripted fixture can
//! tell a re-ask from a new question.
//!
//! # Wait accounting
//!
//! The dispatcher decides nothing about time. The driver records the wait
//! itself: a question on a firing's progress channel starts an interaction
//! wait for that firing and attempt, the delivered answer ends it, and an
//! `ExecutorEnforced` attempt budget stops counting in between
//! ([`ir::TimeoutPolicy`]). The watchdog
//! ([`StallWatchdog`](crate::watchdog::StallWatchdog)) parks on the same
//! facts. A `HandlerManaged` step (a human gate) owns its own answer deadline
//! and expires the question itself; the dispatcher then sees the firing
//! finish and ends the interviewer's wait as cancelled.
//!
//! # Answer shapes
//!
//! A single choice is `Answer::choice(key)`. A `multi_select` answer is
//! `Answer::choices(keys)`: `choices` carries every selected key, and
//! `choice` repeats the first so a step that routes on one choice still
//! routes. This is the shape the pinned Fabro reference sends over its API
//! as `{"kind": "multi_selected", "option_keys": [...]}`; `choices` is
//! `option_keys`. Free text is `Answer::text`; a refusal is the negative
//! choice; `Answer::cancelled()` is the host's "no answer".
//!
//! # Errors and refusal
//!
//! [`InterviewReply::Failed`] means the interviewer itself could not answer:
//! a closed terminal, a fixture with no matching entry. The dispatcher records
//! the error, delivers [`Answer::cancelled`] so the step fails closed, and
//! the host surfaces the receipt's errors. [`InterviewReply::Cancelled`]
//! delivers the same. Refusing is not an error: it is an ordinary
//! [`InterviewReply::Answered`] naming the negative choice.
//!
//! # Receipt order
//!
//! Reply tasks finish in whatever order the interviewer answers, so the
//! dispatcher orders the receipt's `questions` itself when it produces the
//! receipt: by invocation, execution, firing, occurrence, then ask. That is
//! the order the run asked the questions. Every record has its own key (a
//! re-ask keeps the occurrence and takes the next ask), so the order is
//! total: two runs that ask the same questions write the same receipt order
//! whatever the answer timing. `petri inspect` passes the receipt through
//! as the host wrote it.

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use engine::{EngineState, Event, EventRecord};
use executor::SecretProvider;
use ir::{Attempt, Control, FiringId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use smol_str::SmolStr;
use steps::{Answer, Question};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{
    CoordinatorEvent, CoordinatorHandle, CoordinatorRecord, ExecutionId, ExecutionObserver,
    InvocationId,
};

/// The receipt format this module writes. Bump when a field changes meaning.
pub const RECEIPT_VERSION: u32 = 1;

/// The receipt's file name under a standalone run dir.
pub const RECEIPT_FILE: &str = "interviews.json";

/// How long `shutdown` waits for reply tasks that ignore their cancel token.
const SHUTDOWN_PATIENCE: Duration = Duration::from_secs(5);

/// One question, with everything a host needs to tell it apart from every
/// other question this run asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterviewRequest {
    pub invocation:      InvocationId,
    /// The logical path of the invocation: `/` for the root, then one `/`
    /// segment per nested call slot (`/manager/1`).
    pub invocation_path: String,
    pub execution:       ExecutionId,
    pub firing:          FiringId,
    pub attempt:         Attempt,
    /// The node instance name.
    pub node:            SmolStr,
    /// Which distinct question this node instance is asking within its
    /// invocation, 1-based. A node that fires again in a loop asks a new
    /// question and the occurrence advances.
    pub occurrence:      u32,
    /// How many times this exact question id has been asked, 1-based. Greater
    /// than one only when the step re-asked after rejecting an answer.
    pub ask:             u32,
    pub question:        Question,
}

/// What an interviewer decided.
#[derive(Debug)]
pub enum InterviewReply {
    /// An answer to bind to the question and deliver. A refusal is an
    /// `Answered` naming the negative choice.
    Answered(Answer),
    /// The interviewer stopped waiting: the token fired, or a script said to
    /// cancel this interview.
    Cancelled,
    /// The interviewer itself failed. The gate fails closed and the error
    /// reaches the receipt.
    Failed(InterviewError),
}

/// A failure of the interviewer, not a negative answer.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct InterviewError {
    message: String,
    #[source]
    source:  Option<Box<dyn StdError + Send + Sync + 'static>>,
    /// What the interviewer wants in the receipt's `script` section even
    /// though it failed: a scripted interviewer's per-entry counts when a
    /// required entry went unused.
    report:  Option<Value>,
}

impl InterviewError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source:  None,
            report:  None,
        }
    }

    pub fn with_source(
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source:  Some(Box::new(source)),
            report:  None,
        }
    }

    /// Attach the receipt section a failed `finish` still has to report.
    #[must_use]
    pub fn with_report(mut self, report: Value) -> Self {
        self.report = Some(report);
        self
    }

    pub fn report(&self) -> Option<&Value> {
        self.report.as_ref()
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Where a run's questions go.
///
/// Implementations choose answers and nothing else: correlation, secret
/// registration, delivery and the receipt belong to the
/// [`InterviewDispatcher`]. See the module documentation for the concurrency,
/// cancellation and error contract an implementation relies on.
#[async_trait::async_trait]
pub trait Interviewer: Send + Sync {
    /// Answer one question. Return promptly once `cancel` fires; the reply is
    /// then recorded as cancelled or late and never delivered.
    async fn reply(&self, request: InterviewRequest, cancel: CancellationToken) -> InterviewReply;

    /// Called once after the run finished and every reply task ended. An
    /// implementation with expectations of its own (a script with required
    /// entries) reports what was left unmet as the error, and may return a
    /// summary the host writes into the receipt under `script`. The default
    /// reports nothing.
    async fn finish(&self) -> Result<Option<Value>, InterviewError> {
        Ok(None)
    }
}

/// How an answer left the dispatcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// The control reached the firing's channel.
    Delivered,
    /// The firing was no longer live when the control was sent.
    NotLive,
    /// The firing finished before the interviewer replied; nothing was sent.
    Late,
    /// The dispatcher shut down while the reply was pending; nothing was sent.
    Shutdown,
    /// A sensitive answer could not be registered as a secret; its plaintext
    /// was withheld and the gate cancelled instead.
    Withheld,
}

/// What the interviewer replied, as the receipt records it. A sensitive text
/// answer appears only as its `$secret` reference.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplyRecord {
    Answered {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choice:  Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        choices: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text:    Option<Value>,
    },
    Cancelled,
    Failed {
        error: String,
    },
}

/// One question the run asked, and what became of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterviewRecord {
    pub invocation:      InvocationId,
    pub invocation_path: String,
    pub execution:       ExecutionId,
    pub firing:          FiringId,
    pub attempt:         Attempt,
    pub node:            SmolStr,
    pub occurrence:      u32,
    pub ask:             u32,
    pub question:        String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind:            Option<String>,
    pub text:            String,
    pub options:         Vec<String>,
    pub sensitive:       bool,
    /// What the question asked the person to review, when it named
    /// something (a `review_target` gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference:       Option<steps::QuestionReference>,
    /// The step's answer deadline, when it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms:      Option<u64>,
    pub reply:           ReplyRecord,
    pub delivery:        Delivery,
}

/// The machine-readable record of a run's interviews. The standalone host
/// writes it as JSON to `<run_dir>/interviews.json` ([`RECEIPT_FILE`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterviewReceipt {
    pub version:   u32,
    /// Every question the run asked, in the receipt order: by invocation,
    /// execution, firing, occurrence, then ask ([`Self::sort_questions`]).
    pub questions: Vec<InterviewRecord>,
    /// Interviewer failures, late replies, withheld plaintext, pending tasks
    /// at shutdown, and whatever `finish` reported. Non-empty means the run's
    /// interviews did not go as intended, whatever the engine status says.
    pub errors:    Vec<String>,
    /// What [`Interviewer::finish`] returned: a scripted interviewer's
    /// per-entry consumption, for instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script:    Option<Value>,
}

impl InterviewReceipt {
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }

    /// Put `questions` in the receipt order: by invocation, execution,
    /// firing, occurrence, then ask, which is the order the run asked them.
    /// The dispatcher applies it when it produces the receipt; a host that
    /// assembles a receipt from its own records applies it before writing.
    pub fn sort_questions(&mut self) {
        self.questions.sort_by_key(|record| {
            (
                record.invocation,
                record.execution,
                record.firing,
                record.occurrence,
                record.ask,
            )
        });
    }
}

struct Wiring {
    handle:  CoordinatorHandle,
    secrets: Arc<dyn SecretProvider>,
}

struct LiveQuestion {
    firing: FiringId,
    cancel: CancellationToken,
}

#[derive(Default)]
struct State {
    /// Which invocation each execution belongs to, from the lifecycle log.
    executions:  BTreeMap<ExecutionId, InvocationId>,
    /// Each invocation's logical path.
    paths:       BTreeMap<InvocationId, String>,
    /// Distinct questions per (invocation, node instance).
    occurrences: BTreeMap<(InvocationId, SmolStr), u32>,
    /// Asks per (execution, question id).
    asks:        BTreeMap<(ExecutionId, String), u32>,
    /// Questions whose reply is pending.
    live:        BTreeMap<(ExecutionId, String), LiveQuestion>,
    records:     Vec<InterviewRecord>,
    errors:      Vec<String>,
    tasks:       Vec<JoinHandle<()>>,
    /// Set once `shutdown` starts; later questions are refused.
    closed:      bool,
}

struct Inner {
    interviewer: Arc<dyn Interviewer>,
    wiring:      OnceLock<Wiring>,
    shutdown:    CancellationToken,
    state:       Mutex<State>,
}

/// The host side of the interview boundary. Construct before the run,
/// [`observe`](crate::host::HostRun::observe) a clone, [`wire`](Self::wire) it
/// once the coordinator exists, and [`shutdown`](Self::shutdown) it after the
/// run for the receipt. Clones share one dispatcher.
#[derive(Clone)]
pub struct InterviewDispatcher {
    inner: Arc<Inner>,
}

impl InterviewDispatcher {
    pub fn new(interviewer: Arc<dyn Interviewer>) -> Self {
        let mut state = State::default();
        state.paths.insert(InvocationId::ROOT, "/".to_owned());
        Self {
            inner: Arc::new(Inner {
                interviewer,
                wiring: OnceLock::new(),
                shutdown: CancellationToken::new(),
                state: Mutex::new(state),
            }),
        }
    }

    /// Connect to the running coordinator. `run_configured` hands both over
    /// before the first record; a question that arrives unwired is refused
    /// and recorded as an error.
    pub fn wire(&self, handle: CoordinatorHandle, secrets: Arc<dyn SecretProvider>) {
        let _ = self.inner.wiring.set(Wiring { handle, secrets });
    }

    /// End every pending wait, join the reply tasks, ask the interviewer to
    /// finish, and produce the receipt. Call once, after the run.
    pub async fn shutdown(&self) -> InterviewReceipt {
        let inner = &self.inner;
        inner.shutdown.cancel();
        let tasks = {
            let mut state = inner.state();
            state.closed = true;
            mem::take(&mut state.tasks)
        };
        for task in tasks {
            match timeout(SHUTDOWN_PATIENCE, task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => inner
                    .state()
                    .errors
                    .push(format!("a reply task failed: {error}")),
                Err(_) => inner
                    .state()
                    .errors
                    .push("a reply task was still pending after shutdown".to_owned()),
            }
        }
        let script = match inner.interviewer.finish().await {
            Ok(script) => script,
            Err(error) => {
                inner.state().errors.push(error.to_string());
                // A failed verification still reports what it counted.
                error.report().cloned()
            }
        };
        let mut receipt = {
            let mut state = inner.state();
            InterviewReceipt {
                version: RECEIPT_VERSION,
                questions: mem::take(&mut state.records),
                errors: mem::take(&mut state.errors),
                script,
            }
        };
        // The records arrived in reply order; the receipt is in ask order.
        receipt.sort_questions();
        receipt
    }
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Correlate a question and start its reply task. Never blocks.
    fn ask(
        self: &Arc<Self>,
        execution: ExecutionId,
        firing: FiringId,
        question: Question,
        engine: &EngineState,
    ) {
        let Some(wiring) = self.wiring.get() else {
            self.state().errors.push(format!(
                "question `{}` arrived before the dispatcher was wired",
                question.id
            ));
            return;
        };
        let attempt = engine
            .firing(firing)
            .map_or(Attempt::new(1), |live| live.attempt);
        let node = engine
            .firing_node(firing)
            .and_then(|id| engine.graph().node(id))
            .map_or_else(|| SmolStr::new("?"), |node| node.name.clone());
        let (request, cancel) = {
            let mut state = self.state();
            if state.closed {
                state
                    .errors
                    .push(format!("question `{}` arrived after shutdown", question.id));
                return;
            }
            let key = (execution, question.id.clone());
            if state.live.contains_key(&key) {
                tracing::debug!(question = %question.id, "duplicate question while a reply is pending");
                return;
            }
            let invocation = state
                .executions
                .get(&execution)
                .copied()
                .unwrap_or(InvocationId::ROOT);
            let invocation_path = state
                .paths
                .get(&invocation)
                .cloned()
                .unwrap_or_else(|| "/".to_owned());
            let ask = state.asks.entry(key.clone()).or_insert(0);
            *ask += 1;
            let ask = *ask;
            let occurrence = if ask == 1 {
                let count = state
                    .occurrences
                    .entry((invocation, node.clone()))
                    .or_insert(0);
                *count += 1;
                *count
            } else {
                state
                    .occurrences
                    .get(&(invocation, node.clone()))
                    .copied()
                    .unwrap_or(1)
            };
            let cancel = self.shutdown.child_token();
            state.live.insert(key, LiveQuestion {
                firing,
                cancel: cancel.clone(),
            });
            (
                InterviewRequest {
                    invocation,
                    invocation_path,
                    execution,
                    firing,
                    attempt,
                    node,
                    occurrence,
                    ask,
                    question,
                },
                cancel,
            )
        };
        let handle = wiring.handle.clone();
        let secrets = wiring.secrets.clone();
        let this = self.clone();
        let task = tokio::spawn(async move {
            this.serve(request, cancel, handle, secrets).await;
        });
        self.state().tasks.push(task);
    }

    async fn serve(
        &self,
        request: InterviewRequest,
        cancel: CancellationToken,
        handle: CoordinatorHandle,
        secrets: Arc<dyn SecretProvider>,
    ) {
        let key = (request.execution, request.question.id.clone());
        let firing = request.firing;
        let mut record = InterviewRecord {
            invocation:      request.invocation,
            invocation_path: request.invocation_path.clone(),
            execution:       request.execution,
            firing:          request.firing,
            attempt:         request.attempt,
            node:            request.node.clone(),
            occurrence:      request.occurrence,
            ask:             request.ask,
            question:        request.question.id.clone(),
            kind:            request.question.kind.clone(),
            text:            request.question.text.clone(),
            options:         request
                .question
                .options
                .iter()
                .map(|option| option.key.clone())
                .collect(),
            sensitive:       request.question.sensitive,
            reference:       request.question.reference.clone(),
            timeout_ms:      request.question.timeout_ms,
            reply:           ReplyRecord::Cancelled,
            delivery:        Delivery::Shutdown,
        };
        let question = request.question.clone();
        let reply = tokio::select! {
            reply = self.interviewer.reply(request, cancel.clone()) => Some(reply),
            () = cancel.cancelled() => None,
        };
        // Whatever the reply, this question is no longer pending. Whether it
        // is still deliverable depends on whether the token fired: the
        // firing finished, or the dispatcher shut down.
        let was_live = self.state().live.remove(&key).is_some();
        let closed = self.shutdown.is_cancelled();
        if cancel.is_cancelled() || !was_live || closed {
            record.delivery = if closed {
                Delivery::Shutdown
            } else {
                Delivery::Late
            };
            match reply {
                // The interviewer stopped when told to: the ordinary end of
                // a question nobody could answer any more.
                None | Some(InterviewReply::Cancelled) => {
                    record.reply = ReplyRecord::Cancelled;
                }
                // An answer, or a failure, after the firing finished: not
                // delivered, and worth a line in the receipt's errors.
                Some(reply) => {
                    record.reply = describe(&reply);
                    self.state().errors.push(format!(
                        "a reply to `{}` arrived after its firing finished",
                        key.1
                    ));
                }
            }
            self.state().records.push(record);
            return;
        }
        let Some(reply) = reply else {
            record.delivery = Delivery::Late;
            self.state().records.push(record);
            return;
        };
        let control = match reply {
            InterviewReply::Answered(answer) => {
                let mut answer = answer.for_question(&question.id);
                if question.sensitive
                    && answer.choice.is_none()
                    && answer.choices.is_empty()
                    && let Some(Value::String(text)) = answer.text.clone()
                {
                    let name = question.secret_name();
                    // Registered first, then referenced: the log sees the
                    // name only. A registration failure withholds the value.
                    if let Err(error) = secrets.register(&name, &text) {
                        self.state().errors.push(format!(
                            "could not register the answer to `{}` as a secret: {error}",
                            question.id
                        ));
                        record.reply = ReplyRecord::Answered {
                            choice:  None,
                            choices: Vec::new(),
                            text:    Some(json!({ "$secret": name })),
                        };
                        record.delivery = Delivery::Withheld;
                        let _ = handle.deliver(key.0, firing, cancelled(&question.id)).await;
                        self.state().records.push(record);
                        return;
                    }
                    answer.text = Some(json!({ "$secret": name }));
                }
                record.reply = ReplyRecord::Answered {
                    choice:  answer.choice.clone(),
                    choices: answer.choices.clone(),
                    text:    answer.text.clone(),
                };
                answer.to_control()
            }
            InterviewReply::Cancelled => {
                record.reply = ReplyRecord::Cancelled;
                cancelled(&question.id)
            }
            InterviewReply::Failed(error) => {
                let rendered = chain(&error);
                self.state().errors.push(format!(
                    "the interviewer failed on `{}`: {rendered}",
                    question.id
                ));
                record.reply = ReplyRecord::Failed { error: rendered };
                cancelled(&question.id)
            }
        };
        let disposition = handle.deliver(key.0, firing, control).await;
        record.delivery = match disposition {
            driver::DeliverDisposition::Delivered => Delivery::Delivered,
            driver::DeliverDisposition::NotLive => Delivery::NotLive,
        };
        self.state().records.push(record);
    }
}

/// The control that ends a question without an answer. The engine forwards
/// only `Control::Deliver` from a host, so the "no answer" is itself an
/// answer, marked cancelled; the step fails closed on it.
fn cancelled(question: &str) -> Control {
    Answer::cancelled().for_question(question).to_control()
}

fn describe(reply: &InterviewReply) -> ReplyRecord {
    match reply {
        InterviewReply::Answered(answer) => ReplyRecord::Answered {
            choice:  answer.choice.clone(),
            choices: answer.choices.clone(),
            text:    answer.text.clone(),
        },
        InterviewReply::Cancelled => ReplyRecord::Cancelled,
        InterviewReply::Failed(error) => ReplyRecord::Failed {
            error: chain(error),
        },
    }
}

/// An error and its source chain on one line.
fn chain(error: &dyn StdError) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

impl ExecutionObserver for InterviewDispatcher {
    fn on_engine_record(&self, execution: ExecutionId, record: &EventRecord, state: &EngineState) {
        match &record.event {
            Event::StepProgress { firing, ev } => {
                if let Some(question) = Question::from_event(ev) {
                    self.inner.ask(execution, *firing, question, state);
                }
            }
            Event::StepFinished { firing, .. } => {
                // The firing is gone: every pending question of its is
                // unanswerable now. Ending the wait tells the interviewer to
                // stop, and marks whatever arrives afterwards as late. Firing
                // ids are per execution, so the execution is part of the key:
                // a parallel branch's child execution numbers its firings
                // from one like every other.
                let state = self.inner.state();
                for live in state
                    .live
                    .iter()
                    .filter(|((owner, _), live)| *owner == execution && live.firing == *firing)
                    .map(|(_, live)| live)
                {
                    live.cancel.cancel();
                }
            }
            _ => {}
        }
    }

    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        let mut state = self.inner.state();
        match &record.event {
            CoordinatorEvent::ExecutionDeclared {
                execution,
                invocation,
                ..
            } => {
                state.executions.insert(*execution, *invocation);
            }
            CoordinatorEvent::InvocationDeclared {
                invocation,
                call: Some(call),
                ..
            } => {
                let parent = state
                    .executions
                    .get(&call.parent)
                    .and_then(|parent| state.paths.get(parent))
                    .cloned()
                    .unwrap_or_else(|| "/".to_owned());
                let path = if parent == "/" {
                    format!("/{}", call.slot)
                } else {
                    format!("{parent}/{}", call.slot)
                };
                state.paths.insert(*invocation, path);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        invocation: u64,
        execution: u64,
        firing: u64,
        occurrence: u32,
        ask: u32,
    ) -> InterviewRecord {
        InterviewRecord {
            invocation: InvocationId::new(invocation),
            invocation_path: "/".to_owned(),
            execution: ExecutionId::new(execution),
            firing: FiringId::new(firing),
            attempt: Attempt::FIRST,
            node: SmolStr::new("gate"),
            occurrence,
            ask,
            question: format!("gate#{firing}"),
            kind: None,
            text: String::new(),
            options: Vec::new(),
            sensitive: false,
            reference: None,
            timeout_ms: None,
            reply: ReplyRecord::Cancelled,
            delivery: Delivery::Delivered,
        }
    }

    fn keys(receipt: &InterviewReceipt) -> Vec<(u64, u64, u64, u32, u32)> {
        receipt
            .questions
            .iter()
            .map(|record| {
                (
                    record.invocation.raw(),
                    record.execution.raw(),
                    record.firing.raw(),
                    record.occurrence,
                    record.ask,
                )
            })
            .collect()
    }

    /// Records land in reply order; the receipt is in ask order, whatever
    /// the interviewer's timing was.
    #[test]
    fn the_receipt_orders_questions_as_the_run_asked_them() {
        let mut receipt = InterviewReceipt {
            version:   RECEIPT_VERSION,
            questions: vec![
                // The re-ask of a gate, answered before the original ask.
                record(0, 0, 3, 1, 2),
                // A nested invocation's gate, answered first of all.
                record(1, 1, 1, 1, 1),
                // The same node's next firing in a loop.
                record(0, 0, 5, 2, 1),
                // The original ask.
                record(0, 0, 3, 1, 1),
                // A second question in the same firing.
                record(0, 0, 3, 3, 1),
                // The root's second execution after a restart.
                record(0, 2, 1, 1, 1),
            ],
            errors:    Vec::new(),
            script:    None,
        };
        receipt.sort_questions();
        assert_eq!(keys(&receipt), vec![
            (0, 0, 3, 1, 1),
            (0, 0, 3, 1, 2),
            (0, 0, 3, 3, 1),
            (0, 0, 5, 2, 1),
            (0, 2, 1, 1, 1),
            (1, 1, 1, 1, 1),
        ]);
    }

    /// Sorting an ordered receipt changes nothing.
    #[test]
    fn an_ordered_receipt_stays_as_it_is() {
        let questions = vec![record(0, 0, 2, 1, 1), record(0, 0, 2, 1, 2)];
        let mut receipt = InterviewReceipt {
            version:   RECEIPT_VERSION,
            questions: questions.clone(),
            errors:    Vec::new(),
            script:    None,
        };
        receipt.sort_questions();
        assert_eq!(receipt.questions, questions);
    }
}
