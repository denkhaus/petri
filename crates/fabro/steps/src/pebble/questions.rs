//! Pebble asks a person; Petri's interviewer answers.
//!
//! Pebble's question tool hands the session's [`HumanInputProvider`] one
//! batch of questions and waits. This bridge turns each into the core
//! [`Question`] a workflow human gate emits, sends it on the step's progress
//! channel, and waits for the [`Answer`] the host's interview dispatcher
//! delivers on the step's control channel. The same interviewer that answers
//! `fabro/human` gates answers agent questions; Pebble never sees Petri's
//! control types, and Petri never reads a terminal inside Pebble.
//!
//! Identity: a question's id is `<node>#<firing>/agent/<session>/<tool
//! call>/<index>`, so the receipt records the Pebble session and tool call
//! beside the workflow node, firing and attempt the dispatcher adds.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock, PoisonError};

use ir::{FiringId, StepEvent, Value};
use pebble_coding_agent::extensions::{
    Answer as PebbleAnswer, AnswerStatus, HumanInputError, HumanInputProvider,
    Question as PebbleQuestion, QuestionKind,
};
use smol_str::SmolStr;
use steps::{Answer, Question, QuestionOption};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// The agent's open questions, shared between the session's control loop
/// and the provider Pebble calls.
pub struct AgentQuestions {
    logs:    mpsc::Sender<StepEvent>,
    node:    SmolStr,
    firing:  FiringId,
    session: OnceLock<String>,
    pending: Mutex<BTreeMap<String, oneshot::Sender<Answer>>>,
}

impl AgentQuestions {
    pub fn new(logs: mpsc::Sender<StepEvent>, node: SmolStr, firing: FiringId) -> Self {
        Self {
            logs,
            node,
            firing,
            session: OnceLock::new(),
            pending: Mutex::new(BTreeMap::new()),
        }
    }

    /// Record the Pebble session id once the agent exists.
    pub fn set_session(&self, session: &str) {
        let _ = self.session.set(session.to_owned());
    }

    /// Route a delivered value to the open question it answers. `false` when
    /// the value is not an answer to one of ours: the caller treats it as
    /// steering.
    pub fn answer(&self, value: &Value) -> bool {
        let Some(answer) = Answer::from_value(value) else {
            return false;
        };
        let Some(id) = answer.question.as_deref() else {
            return false;
        };
        let reply = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        match reply {
            Some(reply) => {
                let _ = reply.send(answer);
                true
            }
            None => false,
        }
    }

    fn question_id(&self, tool_call_id: &str, index: usize) -> String {
        format!(
            "{}#{}/agent/{}/{tool_call_id}/{index}",
            self.node,
            self.firing.raw(),
            self.session.get().map_or("?", String::as_str)
        )
    }
}

/// Pebble's question as the core protocol carries it.
fn translate(id: String, question: &PebbleQuestion) -> Question {
    Question {
        id,
        text: question.text.clone(),
        options: question
            .options
            .iter()
            .map(|option| QuestionOption {
                key:   option.key.clone(),
                label: option.label.clone(),
            })
            .collect(),
        default: question.options.first().map(|option| option.key.clone()),
        freeform: question.allow_freeform,
        sensitive: false,
        kind: Some(
            match question.kind {
                QuestionKind::MultiSelect => "multi_select",
                _ => "multiple_choice",
            }
            .to_owned(),
        ),
        reference: None,
        timeout_ms: None,
    }
}

/// The delivered answer as Pebble reads it: option keys, or free text.
fn translate_back(question: &PebbleQuestion, answer: Answer) -> PebbleAnswer {
    if answer.cancelled {
        return PebbleAnswer::unanswered(question, AnswerStatus::Cancelled);
    }
    let values: Vec<String> = if !answer.choices.is_empty() {
        answer.choices
    } else if let Some(choice) = answer.choice {
        vec![choice]
    } else if let Some(Value::String(text)) = answer.text {
        vec![text]
    } else {
        Vec::new()
    };
    if values.is_empty() {
        PebbleAnswer::unanswered(question, AnswerStatus::Skipped)
    } else {
        PebbleAnswer::answered(question, values)
    }
}

#[async_trait::async_trait]
impl HumanInputProvider for AgentQuestions {
    async fn ask_questions(
        &self,
        tool_call_id: &str,
        questions: Vec<PebbleQuestion>,
        cancel_token: CancellationToken,
    ) -> Result<Vec<PebbleAnswer>, HumanInputError> {
        let mut waits = Vec::with_capacity(questions.len());
        for (index, question) in questions.iter().enumerate() {
            let id = self.question_id(tool_call_id, index);
            let (reply, wait) = oneshot::channel();
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(id.clone(), reply);
            let event = translate(id.clone(), question).to_event();
            if self.logs.send(event).await.is_err() {
                self.pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                return Err(HumanInputError::new(
                    "the step's progress channel closed before the question was asked",
                ));
            }
            waits.push((id, wait));
        }
        let mut answers = Vec::with_capacity(questions.len());
        for ((id, wait), question) in waits.into_iter().zip(&questions) {
            let answer = tokio::select! {
                answer = wait => answer.ok(),
                () = cancel_token.cancelled() => None,
            };
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            answers.push(match answer {
                Some(answer) => translate_back(question, answer),
                None => PebbleAnswer::unanswered(question, AnswerStatus::Cancelled),
            });
        }
        Ok(answers)
    }
}
