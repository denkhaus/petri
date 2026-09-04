//! `fabro/human`: a human gate. The question is built from the node's
//! outgoing edges, emitted as the core `Question` event, and answered through
//! `Control::Deliver`. The answer becomes the reported label (so the
//! preferred-label tier routes) and the suggested target. A cancel fails
//! closed, and no answer ever falls through to an unconditional edge — the
//! lowering guards the fallback tier for human gates.

use frontend_fabro::kinds::HUMAN_KIND;
use frontend_fabro::labels::strip_accelerator;
use ir::{Control, LogStream, Outcome, StepKindId, Value};
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Answer, Question, QuestionOption, Step, StepCtx};

use crate::outcome::Stage;

pub const KIND: StepKindId = HUMAN_KIND;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Choice {
    pub key:   String,
    pub label: String,
    pub to:    String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanConfig {
    pub label:           String,
    pub node:            String,
    #[serde(default)]
    pub goal:            String,
    #[serde(default)]
    pub choices:         Vec<Choice>,
    #[serde(default)]
    pub freeform_target: Option<String>,
    #[serde(default)]
    pub question_type:   Option<String>,
    #[serde(default)]
    pub review_target:   Option<bool>,
    #[serde(default)]
    pub sensitive:       Option<bool>,
    #[serde(default)]
    pub on_failure:      Option<String>,
    #[serde(default)]
    pub timeout_ms:      Option<u64>,
    #[serde(default)]
    pub kv:              Value,
}

pub struct HumanStep;

impl HumanConfig {
    /// The question this gate asks. The id names the firing, so a re-asked
    /// question after a resume is a new question with a new secret name.
    pub fn question(&self, ctx: &StepCtx) -> Question {
        Question {
            id:        format!("{}#{}", self.node, ctx.firing.raw()),
            text:      self.label.clone(),
            options:   self
                .choices
                .iter()
                .map(|c| QuestionOption {
                    key:   c.key.clone(),
                    label: c.label.clone(),
                })
                .collect(),
            default:   self.choices.first().map(|c| c.key.clone()),
            freeform:  self.freeform_target.is_some(),
            sensitive: self.sensitive.unwrap_or(false),
            kind:      self.question_type.clone(),
        }
    }

    /// The choice an answer names, by key or by label (accelerator-free,
    /// case-insensitive).
    fn choice_for(&self, text: &str) -> Option<&Choice> {
        let wanted = strip_accelerator(text).to_lowercase();
        self.choices.iter().find(|c| {
            c.key.eq_ignore_ascii_case(text.trim())
                || strip_accelerator(&c.label).to_lowercase() == wanted
        })
    }
}

#[async_trait::async_trait]
impl Step for HumanStep {
    const NAME: &'static str = "fabro/human";
    type Config = HumanConfig;

    async fn run(&self, config: HumanConfig, mut ctx: StepCtx) -> Outcome {
        let question = config.question(&ctx);
        if config.choices.is_empty() && config.freeform_target.is_none() {
            return Stage::failed(
                format!(
                    "human gate `{}` has no outgoing edges to offer",
                    config.node
                ),
                "bad_config",
                config.on_failure.as_deref(),
            )
            .into_outcome(&config.node);
        }
        let _ = ctx.logs.send(question.to_event()).await;
        ctx.log(
            LogStream::Stdout,
            format!("waiting for an answer: {}", question.text),
        )
        .await;
        loop {
            let value = match ctx.control.recv().await {
                Some(Control::Deliver(value)) => value,
                Some(Control::Cancel | Control::Kill) | None => {
                    // Fail closed: an interrupted gate never routes.
                    return Stage::failed(
                        "human interaction interrupted before an answer was provided",
                        "interrupted",
                        config.on_failure.as_deref(),
                    )
                    .into_outcome(&config.node);
                }
                Some(_) => continue,
            };
            let Some(answer) = Answer::from_value(&value) else {
                ctx.log(
                    LogStream::Stderr,
                    "ignoring a delivery that is not an answer",
                )
                .await;
                continue;
            };
            if answer.question.as_deref().is_some_and(|q| q != question.id) {
                ctx.log(LogStream::Stderr, "ignoring an answer to another question")
                    .await;
                continue;
            }
            let mut stage = Stage::new("succeeded", config.on_failure.as_deref());
            let chosen = answer
                .choice
                .as_deref()
                .and_then(|c| config.choice_for(c))
                .or_else(|| {
                    answer
                        .text
                        .as_ref()
                        .and_then(Value::as_str)
                        .and_then(|t| config.choice_for(t))
                });
            if let Some(choice) = chosen {
                let label = strip_accelerator(&choice.label).to_string();
                stage.output.insert("preferred_label".into(), json!(label));
                stage
                    .output
                    .insert("suggested_next_ids".into(), json!([choice.to]));
                stage.output.insert("choice".into(), json!(choice.key));
                stage
                    .context_updates
                    .insert(SmolStr::new("human.gate.selected"), json!(choice.key));
                stage
                    .context_updates
                    .insert(SmolStr::new("human.gate.label"), json!(choice.label));
            } else if let (Some(target), Some(text)) = (&config.freeform_target, &answer.text) {
                // Free text: the value (or its `$secret` reference) as
                // written, never resolved here.
                stage
                    .output
                    .insert("suggested_next_ids".into(), json!([target]));
                stage.output.insert("text".into(), text.clone());
                stage
                    .context_updates
                    .insert(SmolStr::new("human.gate.selected"), json!("freeform"));
                stage
                    .context_updates
                    .insert(SmolStr::new("human.gate.text"), text.clone());
            } else {
                ctx.log(
                    LogStream::Stderr,
                    format!(
                        "the answer names no choice; the choices are {}",
                        config
                            .choices
                            .iter()
                            .map(|c| format!("[{}] {}", c.key, strip_accelerator(&c.label)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
                .await;
                let _ = ctx.logs.send(question.to_event()).await;
                continue;
            }
            return stage.into_outcome(&config.node);
        }
    }
}
