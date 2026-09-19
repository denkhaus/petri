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
//!
//! Two other payloads ride the same control: a [`Steer`], guidance for an
//! agent's session, and an [`Interrupt`], which stops an agent's current
//! model turn. Neither is ever read as an answer.

use ir::{Control, StepEvent, Value};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The key a question rides under in a `StepEvent::Custom` value.
pub const QUESTION_KEY: &str = "$question";
/// The key a question's expiry rides under in a `StepEvent::Custom` value.
pub const EXPIRED_KEY: &str = "$question_expired";
/// The key an answer rides under in a `Control::Deliver` value.
pub const ANSWER_KEY: &str = "$answer";
/// The `$secret` reference prefix for a sensitive answer's value.
pub const ANSWER_SECRET_PREFIX: &str = "answer:";
/// The key a steering message rides under in a `Control::Deliver` value. A
/// steer is never an answer: a step waiting on a question ignores it, and an
/// agent step queues it as guidance for its session.
pub const STEER_KEY: &str = "$steer";
/// The key an interrupt rides under in a `Control::Deliver` value. Like a
/// steer, an interrupt is never an answer.
pub const INTERRUPT_KEY: &str = "$interrupt";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    /// The shortcut a person types.
    pub key:   String,
    pub label: String,
}

/// Something a person should look at before answering: a review document,
/// a pull request. The host shows the label and the URL beside the question;
/// it never fetches the URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionReference {
    pub label: String,
    pub url:   String,
    /// What the reference is, in the asking format's words (`document`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind:  Option<String>,
}

/// What a step asks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// Unique within the run: what an answer names, and what a sensitive
    /// answer's secret is registered as (`answer:<id>`).
    pub id:         String,
    pub text:       String,
    #[serde(default)]
    pub options:    Vec<QuestionOption>,
    /// The option a host takes when told to auto-approve, by key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default:    Option<String>,
    /// Whether free text is an acceptable answer.
    #[serde(default)]
    pub freeform:   bool,
    /// Whether the answer must cross as a `$secret` reference.
    #[serde(default)]
    pub sensitive:  bool,
    /// The asking format's own question type, for a host that renders it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind:       Option<String>,
    /// What to review before answering, when the step names something.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference:  Option<QuestionReference>,
    /// How long the step waits for the answer, when it has a deadline. The
    /// step owns the expiry; a host shows the deadline so a person knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl Question {
    /// A question with the given id and text, no options, and nothing else.
    pub fn new(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id:         id.into(),
            text:       text.into(),
            options:    Vec::new(),
            default:    None,
            freeform:   false,
            sensitive:  false,
            kind:       None,
            reference:  None,
            timeout_ms: None,
        }
    }

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

/// A question's answer deadline passed with no answer. The step that owns
/// the deadline emits this as [`StepEvent::Custom`] before it acts on the
/// expiry (takes its default, fails with its retry outcome), so the host's
/// interview record and the public event stream carry the timeout as a fact
/// the step reported, not an inference from the firing's end.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionExpired {
    /// The id of the question that expired.
    pub question:  String,
    /// How long the step waited: its answer deadline.
    pub waited_ms: u64,
    /// The option the step took on its own, by key, when it had a default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default:   Option<String>,
}

impl QuestionExpired {
    /// The event a step emits to report the expiry.
    pub fn to_event(&self) -> StepEvent {
        StepEvent::Custom(json!({ EXPIRED_KEY: self }))
    }

    /// The expiry a step event carries, if it is one.
    pub fn from_event(event: &StepEvent) -> Option<Self> {
        let StepEvent::Custom(value) = event else {
            return None;
        };
        serde_json::from_value(value.get(EXPIRED_KEY)?.clone()).ok()
    }
}

/// Guidance a host delivers to a running stage: text an agent step queues
/// for its session. Not an answer to anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Steer {
    pub text: String,
}

impl Steer {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// The control a host delivers.
    pub fn to_control(&self) -> Control {
        Control::Deliver(json!({ STEER_KEY: self }))
    }

    /// The steer a delivered value carries, if it is one.
    pub fn from_value(value: &Value) -> Option<Self> {
        serde_json::from_value(value.get(STEER_KEY)?.clone()).ok()
    }
}

/// A host stops a running agent stage's current model turn: the request in
/// flight and the tool calls it is running end, the session stays open, and
/// the stage continues with its next input. `steer` is that input when the
/// host gives it in the same control; without it the stage waits for the
/// next delivered text. Not an answer to anything.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interrupt {
    /// The stage's next input, when the host names it with the interrupt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steer: Option<String>,
}

impl Interrupt {
    /// Stop the turn; the next delivered text is the stage's next input.
    pub fn new() -> Self {
        Self { steer: None }
    }

    /// Stop the turn and make `text` the stage's next input.
    pub fn and_steer(text: impl Into<String>) -> Self {
        Self {
            steer: Some(text.into()),
        }
    }

    /// The control a host delivers.
    pub fn to_control(&self) -> Control {
        Control::Deliver(json!({ INTERRUPT_KEY: self }))
    }

    /// The interrupt a delivered value carries, if it is one.
    pub fn from_value(value: &Value) -> Option<Self> {
        serde_json::from_value(value.get(INTERRUPT_KEY)?.clone()).ok()
    }
}

/// What a host delivers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    /// The question answered, when the host says which.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question:  Option<String>,
    /// A chosen option, by key or by label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice:    Option<String>,
    /// Several chosen options, by key or by label: a `multi_select`
    /// question's answer. A step that routes on one choice takes the first;
    /// what it records of the rest is its own contract.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices:   Vec<String>,
    /// Free text, or a `{"$secret": "answer:<id>"}` reference to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text:      Option<Value>,
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
            question:  None,
            choice:    Some(key.to_string()),
            choices:   Vec::new(),
            text:      None,
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
            choice: choices.first().cloned(),
            choices,
            text: None,
            cancelled: false,
        }
    }

    pub fn text(text: impl Into<Value>) -> Self {
        Self {
            question:  None,
            choice:    None,
            choices:   Vec::new(),
            text:      Some(text.into()),
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
        if value.get(STEER_KEY).is_some() || value.get(INTERRUPT_KEY).is_some() {
            return None;
        }
        match value {
            Value::String(s) => Some(Self {
                question:  None,
                choice:    Some(s.clone()),
                choices:   Vec::new(),
                text:      Some(Value::String(s.clone())),
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
            id:         "q1".into(),
            text:       "Ship?".into(),
            options:    vec![QuestionOption {
                key:   "Y".into(),
                label: "[Y] Yes".into(),
            }],
            default:    Some("Y".into()),
            freeform:   false,
            sensitive:  false,
            kind:       Some("yes_no".into()),
            reference:  None,
            timeout_ms: None,
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
    fn an_expiry_round_trips_through_its_event_and_is_not_a_question() {
        let expired = QuestionExpired {
            question:  "gate#3".into(),
            waited_ms: 1000,
            default:   Some("N".into()),
        };
        let event = expired.to_event();
        assert_eq!(QuestionExpired::from_event(&event), Some(expired));
        assert_eq!(Question::from_event(&event), None);
        let without_default = QuestionExpired {
            question:  "gate#3".into(),
            waited_ms: 500,
            default:   None,
        };
        let StepEvent::Custom(value) = without_default.to_event() else {
            panic!("custom");
        };
        assert_eq!(
            value,
            json!({ EXPIRED_KEY: { "question": "gate#3", "waited_ms": 500 } })
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
                question:  None,
                choice:    Some("no".into()),
                choices:   Vec::new(),
                text:      Some(json!("no")),
                cancelled: false,
            })
        );
        assert_eq!(
            Answer::from_value(&json!({ "text": "free" })),
            Some(Answer::text("free"))
        );
        assert_eq!(Answer::from_value(&json!(3)), None);
    }

    /// The pinned Fabro reference answers a `multi_select` question over
    /// its API as `{"kind": "multi_selected", "option_keys": [...]}`. Petri's
    /// `choices` is that list; the first key doubles as `choice` so a step
    /// that routes on one choice routes on the first, as Fabro's gate does.
    #[test]
    fn a_multi_select_answer_carries_every_key_and_routes_on_the_first() {
        let fabro_wire = json!({ "kind": "multi_selected", "option_keys": ["approve", "notify"] });
        let keys: Vec<String> = serde_json::from_value(fabro_wire["option_keys"].clone()).unwrap();
        let answer = Answer::choices(keys.clone());
        assert_eq!(answer.choices, keys);
        assert_eq!(answer.choice.as_deref(), Some("approve"));
        let Control::Deliver(value) = answer.to_control() else {
            panic!("deliver");
        };
        assert_eq!(
            value[ANSWER_KEY]["choices"],
            json!(["approve", "notify"]),
            "the wire carries the whole selection"
        );
        assert_eq!(Answer::from_value(&value), Some(answer));
        // One key selected is still a multi-select answer, not a bare choice.
        let one = Answer::choices(["approve"]);
        assert_eq!(one.choices, vec!["approve".to_owned()]);
    }

    #[test]
    fn a_steer_is_never_read_as_an_answer() {
        let steer = Steer::new("check the edge cases");
        let Control::Deliver(value) = steer.to_control() else {
            panic!("deliver");
        };
        assert_eq!(Steer::from_value(&value), Some(steer));
        assert_eq!(Answer::from_value(&value), None);
        assert_eq!(Steer::from_value(&json!({ "text": "plain" })), None);
    }

    #[test]
    fn an_interrupt_is_neither_an_answer_nor_a_steer() {
        let plain = Interrupt::new();
        let Control::Deliver(value) = plain.to_control() else {
            panic!("deliver");
        };
        assert_eq!(value, json!({ INTERRUPT_KEY: {} }));
        assert_eq!(Interrupt::from_value(&value), Some(plain));
        assert_eq!(Answer::from_value(&value), None);
        assert_eq!(Steer::from_value(&value), None);

        let with_text = Interrupt::and_steer("stop and summarize");
        let Control::Deliver(value) = with_text.to_control() else {
            panic!("deliver");
        };
        assert_eq!(
            value,
            json!({ INTERRUPT_KEY: { "steer": "stop and summarize" } })
        );
        assert_eq!(Interrupt::from_value(&value), Some(with_text));
        assert_eq!(Answer::from_value(&value), None);
        assert_eq!(
            Interrupt::from_value(&json!({ "$steer": { "text": "x" } })),
            None
        );
    }
}
