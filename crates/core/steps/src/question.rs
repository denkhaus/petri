//! A step asks, a host answers: the one protocol for a human gate.
//!
//! A step that needs a person emits a [`Question`] as
//! [`StepEvent::Custom`] and waits on its control channel for
//! [`Control::Deliver`] carrying an [`Answer`]. The shape is format-agnostic
//! so a host — the CLI's `--interactive` prompt, a product's inbox — can
//! answer any format's gate without knowing which step kind asked. A
//! sensitive answer crosses as a `{"$secret": "answer:<id>"}` reference the
//! answerer registered on the run's secret provider first, so the value is
//! maskable and never enters the log.

use ir::{Control, StepEvent, Value};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The key a question rides under in a `StepEvent::Custom` value.
pub const QUESTION_KEY: &str = "$question";
/// The key an answer rides under in a `Control::Deliver` value.
pub const ANSWER_KEY: &str = "$answer";
/// The `$secret` reference prefix for a sensitive answer's value.
pub const ANSWER_SECRET_PREFIX: &str = "answer:";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    /// The shortcut a person types.
    pub key:   String,
    pub label: String,
}

/// What a step asks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// Unique within the run: what an answer names, and what a sensitive
    /// answer's secret is registered as (`answer:<id>`).
    pub id:        String,
    pub text:      String,
    #[serde(default)]
    pub options:   Vec<QuestionOption>,
    /// The option a host takes when told to auto-approve, by key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default:   Option<String>,
    /// Whether free text is an acceptable answer.
    #[serde(default)]
    pub freeform:  bool,
    /// Whether the answer must cross as a `$secret` reference.
    #[serde(default)]
    pub sensitive: bool,
    /// The asking format's own question type, for a host that renders it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind:      Option<String>,
}

impl Question {
    /// The event a step emits to ask.
    pub fn to_event(&self) -> StepEvent {
        StepEvent::Custom(json!({ QUESTION_KEY: self }))
    }

    /// The question a step event carries, if it is one.
    pub fn from_event(event: &StepEvent) -> Option<Self> {
        let StepEvent::Custom(value) = event else {
            return None;
        };
        serde_json::from_value(value.get(QUESTION_KEY)?.clone()).ok()
    }

    /// The secret name a sensitive answer to this question registers as.
    pub fn secret_name(&self) -> String {
        format!("{ANSWER_SECRET_PREFIX}{}", self.id)
    }
}

/// What a host delivers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    /// The question answered, when the host says which.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// A chosen option, by key or by label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice:   Option<String>,
    /// Several chosen options, by key or by label: a `multi_select`
    /// question's answer. A step that routes on one choice takes the first;
    /// what it records of the rest is its own contract.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices:  Vec<String>,
    /// Free text, or a `{"$secret": "answer:<id>"}` reference to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text:     Option<Value>,
    /// The host ended the interview without an answer: the interviewer
    /// failed, or the wait was cancelled. A step treats it as an interrupted
    /// gate and fails closed. Distinct from a negative choice, which is an
    /// ordinary answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
}

impl Answer {
    pub fn choice(key: &str) -> Self {
        Self {
            question: None,
            choice:   Some(key.to_string()),
            choices:  Vec::new(),
            text:     None,
            cancelled: false,
        }
    }

    /// The interview ended without an answer.
    pub fn cancelled() -> Self {
        Self {
            question:  None,
            choice:    None,
            choices:   Vec::new(),
            text:      None,
            cancelled: true,
        }
    }

    /// Several choices at once, for a `multi_select` question. The first is
    /// also the `choice`, so a step that knows one choice still routes.
    pub fn choices(keys: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let choices: Vec<String> = keys.into_iter().map(Into::into).collect();
        Self {
            question: None,
            choice:   choices.first().cloned(),
            choices,
            text:     None,
            cancelled: false,
        }
    }

    pub fn text(text: impl Into<Value>) -> Self {
        Self {
            question: None,
            choice:   None,
            choices:  Vec::new(),
            text:     Some(text.into()),
            cancelled: false,
        }
    }

    #[must_use]
    pub fn for_question(mut self, id: &str) -> Self {
        self.question = Some(id.to_string());
        self
    }

    /// The control a host delivers.
    pub fn to_control(&self) -> Control {
        Control::Deliver(json!({ ANSWER_KEY: self }))
    }

    /// The answer a delivered value carries. A bare string is a choice or
    /// free text; a bare object without the key is read as the answer itself.
    pub fn from_value(value: &Value) -> Option<Self> {
        if let Some(inner) = value.get(ANSWER_KEY) {
            return serde_json::from_value(inner.clone()).ok();
        }
        match value {
            Value::String(s) => Some(Self {
                question: None,
                choice:   Some(s.clone()),
                choices:  Vec::new(),
                text:     Some(Value::String(s.clone())),
                cancelled: false,
            }),
            Value::Object(_) => serde_json::from_value(value.clone()).ok(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_round_trips_through_its_event() {
        let question = Question {
            id:        "q1".into(),
            text:      "Ship?".into(),
            options:   vec![QuestionOption {
                key:   "Y".into(),
                label: "[Y] Yes".into(),
            }],
            default:   Some("Y".into()),
            freeform:  false,
            sensitive: false,
            kind:      Some("yes_no".into()),
        };
        assert_eq!(
            Question::from_event(&question.to_event()),
            Some(question.clone())
        );
        assert_eq!(question.secret_name(), "answer:q1");
        assert_eq!(
            Question::from_event(&StepEvent::Custom(json!({"other": 1}))),
            None
        );
    }

    #[test]
    fn an_answer_reads_the_shapes_a_host_may_send() {
        let answer = Answer::choice("Y").for_question("q1");
        let Control::Deliver(value) = answer.to_control() else {
            panic!("deliver");
        };
        assert_eq!(Answer::from_value(&value), Some(answer));
        assert_eq!(
            Answer::from_value(&json!("no")),
            Some(Answer {
                question: None,
                choice:   Some("no".into()),
                choices:  Vec::new(),
                text:     Some(json!("no")),
                cancelled: false,
            })
        );
        assert_eq!(
            Answer::from_value(&json!({ "text": "free" })),
            Some(Answer::text("free"))
        );
        assert_eq!(Answer::from_value(&json!(3)), None);
    }
}
