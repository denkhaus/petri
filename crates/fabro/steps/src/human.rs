//! `fabro/human`: a human gate. The question is built from the node's
//! outgoing edges, emitted as the core `Question` event, and answered through
//! `Control::Deliver`. The answer becomes the reported label (so the
//! preferred-label tier routes) and the suggested target. A cancel fails
//! closed, and no answer ever falls through to an unconditional edge — the
//! lowering guards the fallback tier for human gates.
//!
//! The gate owns its answer deadline (`TimeoutPolicy::HandlerManaged`): with
//! a `timeout`, an unanswered question expires here. The gate reports the
//! expiry on its progress channel first (`QuestionExpired`, naming the
//! question and the default it takes), so the interview record and the
//! public event stream carry the timeout as the gate's own fact.
//! `human.default_choice` then routes to the named choice; without one the
//! gate fails with Fabro's retry outcome (class `retry_requested`), so
//! `max_retries` asks again and `on_retries_exhausted` decides after the last
//! attempt, with the explicit routes checked first as Fabro's executor checks
//! them. The driver arms no timer around a human gate, so its 30 day
//! structural budget is not the answer deadline.
//!
//! A `review_target=true` gate reads `review_target` from the run context
//! (`{label, url, kind}`), validates it as Fabro does, and asks Fabro's review
//! question with the reference attached, so a host shows the URL beside the
//! question. A missing or invalid target fails the gate before anyone is
//! asked, and the URL itself never appears in the failure.

use std::future::pending;
use std::time::Duration;

use frontend_fabro::Policy;
use frontend_fabro::kinds::{HUMAN_KIND, RETRY_REQUESTED_CLASS, StageOutcome};
use frontend_fabro::labels::strip_accelerator;
use ir::{Control, LogStream, Outcome, StepKindId, Value};
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Answer, Question, QuestionExpired, QuestionOption, QuestionReference, Step, StepCtx};
use tokio::time;

use crate::outcome::{ExplicitRoutes, Stage};

pub const KIND: StepKindId = HUMAN_KIND;

/// The context key a review gate reads its target from, as Fabro names it.
pub const REVIEW_TARGET_KEY: &str = "review_target";

const REVIEW_TARGET_LABEL_MAX_CHARS: usize = 200;
const REVIEW_TARGET_URL_MAX_CHARS: usize = 2048;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Choice {
    pub key:   String,
    pub label: String,
    pub to:    String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanConfig {
    pub label:                String,
    pub node:                 String,
    #[serde(default)]
    pub goal:                 String,
    #[serde(default)]
    pub choices:              Vec<Choice>,
    #[serde(default)]
    pub freeform_target:      Option<String>,
    #[serde(default)]
    pub question_type:        Option<String>,
    #[serde(default)]
    pub sensitive:            Option<bool>,
    /// Read `review_target` from the context and ask about it.
    #[serde(default)]
    pub review_target:        Option<bool>,
    /// The choice (by target, else by key) an expired question takes.
    #[serde(default)]
    pub default_choice:       Option<String>,
    #[serde(default)]
    pub on_failure:           Option<Policy>,
    #[serde(default)]
    pub on_retries_exhausted: Option<Policy>,
    /// The node's explicit routes, for failure promotion.
    #[serde(default, rename = "routes")]
    pub explicit_routes:      Option<ExplicitRoutes>,
    /// The answer deadline. Absent: wait until answered or cancelled.
    #[serde(default)]
    pub timeout_ms:           Option<u64>,
    #[serde(default)]
    pub kv:                   Value,
}

pub struct HumanStep;

/// Why a review target could not be shown. The messages are Fabro's, and
/// none of them repeats the value it rejects.
#[derive(Debug, PartialEq, Eq)]
enum ReviewTargetError {
    Missing,
    NotAnObject,
    EmptyLabel,
    LabelTooLong,
    LabelContainsControl,
    EmptyUrl,
    UrlTooLong,
    UrlContainsUnsafeCharacters,
    InvalidUrl,
    UnsupportedUrlScheme,
    MissingUrlHost,
    UrlContainsCredentials,
}

impl ReviewTargetError {
    fn message(&self, node: &str) -> String {
        match self {
            Self::Missing => format!(
                "Human gate \"{node}\" has review_target=true but context.review_target is missing"
            ),
            other => format!(
                "Human gate \"{node}\" has invalid context.review_target: {}",
                match other {
                    Self::Missing => unreachable!("handled above"),
                    Self::NotAnObject => "review target must be an object with label, url and kind",
                    Self::EmptyLabel => "review target label must not be empty",
                    Self::LabelTooLong => "review target label must be at most 200 characters",
                    Self::LabelContainsControl =>
                        "review target label must not contain control characters",
                    Self::EmptyUrl => "review target URL must not be empty",
                    Self::UrlTooLong => "review target URL must be at most 2048 characters",
                    Self::UrlContainsUnsafeCharacters =>
                        "review target URL must not contain control characters or link delimiters",
                    Self::InvalidUrl => "review target URL must be a valid absolute URL",
                    Self::UnsupportedUrlScheme => "review target URL must use http or https",
                    Self::MissingUrlHost => "review target URL must include a host",
                    Self::UrlContainsCredentials =>
                        "review target URL must not include username or password credentials",
                }
            ),
        }
    }
}

/// Validate a review target the way Fabro's `ReviewTarget::new` does. The URL
/// is parsed only to check its shape; it is never fetched.
fn review_target(value: Option<&Value>) -> Result<QuestionReference, ReviewTargetError> {
    let value = value.ok_or(ReviewTargetError::Missing)?;
    let object = value.as_object().ok_or(ReviewTargetError::NotAnObject)?;
    let label = object
        .get("label")
        .and_then(Value::as_str)
        .ok_or(ReviewTargetError::EmptyLabel)?
        .trim();
    let url = object
        .get("url")
        .and_then(Value::as_str)
        .ok_or(ReviewTargetError::EmptyUrl)?
        .trim();
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("document");
    if label.is_empty() {
        return Err(ReviewTargetError::EmptyLabel);
    }
    if label.chars().count() > REVIEW_TARGET_LABEL_MAX_CHARS {
        return Err(ReviewTargetError::LabelTooLong);
    }
    if label.chars().any(char::is_control) {
        return Err(ReviewTargetError::LabelContainsControl);
    }
    if url.is_empty() {
        return Err(ReviewTargetError::EmptyUrl);
    }
    if url.chars().count() > REVIEW_TARGET_URL_MAX_CHARS {
        return Err(ReviewTargetError::UrlTooLong);
    }
    if url
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | '|'))
    {
        return Err(ReviewTargetError::UrlContainsUnsafeCharacters);
    }
    let (scheme, rest) = url.split_once("://").ok_or(ReviewTargetError::InvalidUrl)?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return Err(ReviewTargetError::InvalidUrl);
    }
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return Err(ReviewTargetError::UnsupportedUrlScheme);
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(ReviewTargetError::UrlContainsCredentials);
    }
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) {
                host
            } else {
                authority
            }
        });
    if host.is_empty() {
        return Err(ReviewTargetError::MissingUrlHost);
    }
    if host.chars().any(char::is_whitespace) {
        return Err(ReviewTargetError::InvalidUrl);
    }
    Ok(QuestionReference {
        label: label.to_owned(),
        url:   url.to_owned(),
        kind:  Some(kind.to_owned()),
    })
}

impl HumanConfig {
    /// The question this gate asks. The id names the firing, so a re-asked
    /// question after a resume is a new question with a new secret name.
    /// A review gate's text is Fabro's review sentence and carries the
    /// validated reference.
    pub fn question(&self, ctx: &StepCtx) -> Question {
        let mut question = Question::new(
            format!("{}#{}", self.node, ctx.firing.raw()),
            self.label.clone(),
        );
        question.options = self
            .choices
            .iter()
            .map(|c| QuestionOption {
                key:   c.key.clone(),
                label: c.label.clone(),
            })
            .collect();
        question.default = self.choices.first().map(|c| c.key.clone());
        question.freeform = self.freeform_target.is_some();
        question.sensitive = self.sensitive.unwrap_or(false);
        question.kind.clone_from(&self.question_type);
        question.timeout_ms = self.timeout_ms;
        question
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

    /// Whether the gate is a `yes_no` or `confirmation` question, whose
    /// answer Fabro records as the word `yes` or `no`.
    fn is_yes_no(&self) -> bool {
        matches!(
            self.question_type.as_deref(),
            Some("yes_no" | "confirmation")
        )
    }

    /// Whether a choice is the affirmative one of a yes/no gate, as Fabro
    /// matches a `yes` answer: key `y` or `yes`, or label `yes`.
    fn is_affirmative(choice: &Choice) -> bool {
        choice.key.eq_ignore_ascii_case("y")
            || choice.key.eq_ignore_ascii_case("yes")
            || strip_accelerator(&choice.label).eq_ignore_ascii_case("yes")
    }

    /// The choice `human.default_choice` names: a target node first, as
    /// Fabro reads it, else a key.
    fn default_choice(&self) -> Option<Choice> {
        let wanted = self.default_choice.as_deref()?;
        self.choices
            .iter()
            .find(|c| c.to == wanted)
            .or_else(|| self.choices.iter().find(|c| c.key == wanted))
            .cloned()
            .or_else(|| {
                Some(Choice {
                    key:   wanted.to_owned(),
                    label: wanted.to_owned(),
                    to:    wanted.to_owned(),
                })
            })
    }

    /// The stage's outcome under the node's failure policies, with the
    /// explicit routes and the attempt's position among the node's attempts
    /// in hand.
    fn finish(&self, stage: Stage, ctx: &StepCtx) -> Outcome {
        stage
            .with_routing(self.explicit_routes.clone(), self.kv.clone())
            .with_retries(self.on_retries_exhausted, ctx.is_final_attempt())
            .into_outcome(&self.node)
    }

    fn interrupted(&self, ctx: &StepCtx) -> Outcome {
        self.finish(
            Stage::failed(
                "human interaction interrupted before an answer was provided",
                "interrupted",
                self.on_failure,
            ),
            ctx,
        )
    }

    /// The outcome for one or more selected choices: the first routes; every
    /// selected key and label is recorded, as Fabro records them.
    fn selected(
        &self,
        selected: &[&Choice],
        question: &str,
        answer_text: &str,
        ctx: &StepCtx,
    ) -> Outcome {
        let mut stage = Stage::new(StageOutcome::Succeeded, self.on_failure);
        let first = selected[0];
        let label = strip_accelerator(&first.label).to_string();
        stage.output.insert("preferred_label".into(), json!(label));
        stage
            .output
            .insert("suggested_next_ids".into(), json!([first.to]));
        let keys = selected
            .iter()
            .map(|c| c.key.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let labels = selected
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        stage.output.insert("choice".into(), json!(keys));
        stage
            .context_updates
            .insert(SmolStr::new("human.gate.selected"), json!(keys));
        stage
            .context_updates
            .insert(SmolStr::new("human.gate.label"), json!(labels));
        self.answer_context(&mut stage, question, answer_text, Some(&labels));
        self.finish(stage, ctx)
    }

    /// Fabro's per-gate record of what was asked and answered.
    fn answer_context(&self, stage: &mut Stage, question: &str, answer: &str, label: Option<&str>) {
        stage.context_updates.insert(
            SmolStr::new(format!("human.gate.{}.question", self.node)),
            json!(question),
        );
        stage.context_updates.insert(
            SmolStr::new(format!("human.gate.{}.answer", self.node)),
            json!(answer),
        );
        if let Some(label) = label {
            stage.context_updates.insert(
                SmolStr::new(format!("human.gate.{}.label", self.node)),
                json!(label),
            );
        }
    }
}

#[async_trait::async_trait]
impl Step for HumanStep {
    const NAME: &'static str = "fabro/human";
    type Config = HumanConfig;

    async fn run(&self, config: HumanConfig, mut ctx: StepCtx) -> Outcome {
        let mut question = config.question(&ctx);
        if config.choices.is_empty() && config.freeform_target.is_none() {
            return config.finish(
                Stage::failed(
                    format!(
                        "human gate `{}` has no outgoing edges to offer",
                        config.node
                    ),
                    "bad_config",
                    config.on_failure,
                ),
                &ctx,
            );
        }
        if config.review_target.unwrap_or(false) {
            match review_target(config.kv.get(REVIEW_TARGET_KEY)) {
                Ok(reference) => {
                    question.text = format!(
                        "Review the {} {}, then choose the next action.",
                        reference.label,
                        reference.kind.as_deref().unwrap_or("document")
                    );
                    question.reference = Some(reference);
                }
                Err(error) => {
                    return config.finish(
                        Stage::failed(
                            error.message(&config.node),
                            "review_target",
                            config.on_failure,
                        ),
                        &ctx,
                    );
                }
            }
        }
        let _ = ctx.logs.send(question.to_event()).await;
        ctx.log(
            LogStream::Stdout,
            format!("waiting for an answer: {}", question.text),
        )
        .await;
        if let Some(reference) = &question.reference {
            ctx.log(
                LogStream::Stdout,
                format!("review: {} <{}>", reference.label, reference.url),
            )
            .await;
        }
        // The answer deadline, when the gate has one. `pending` sleeps forever
        // otherwise, so the gate waits for the answer or a cancel.
        let deadline = async {
            match config.timeout_ms {
                Some(ms) => time::sleep(Duration::from_millis(ms)).await,
                None => pending::<()>().await,
            }
        };
        tokio::pin!(deadline);
        loop {
            let value = tokio::select! {
                control = ctx.control.recv() => match control {
                    Some(Control::Deliver(value)) => value,
                    // Fail closed: an interrupted gate never routes.
                    Some(Control::Cancel | Control::Kill) | None => return config.interrupted(&ctx),
                    Some(_) => continue,
                },
                () = &mut deadline => {
                    let waited = config.timeout_ms.unwrap_or_default();
                    ctx.log(
                        LogStream::Stderr,
                        format!("no answer within {waited}ms; the question expired"),
                    )
                    .await;
                    // The expiry is the gate's own fact: reported before the
                    // gate acts on it, with the default it takes, so the
                    // host's interview record and the public stream never
                    // infer a timeout from how the firing ended.
                    let choice = config.default_choice();
                    let expired = QuestionExpired {
                        question:  question.id.clone(),
                        waited_ms: waited,
                        default:   choice.as_ref().map(|c| c.key.clone()),
                    };
                    let _ = ctx.logs.send(expired.to_event()).await;
                    if let Some(choice) = choice {
                        ctx.log(
                            LogStream::Stdout,
                            format!("taking the default choice `{}`", choice.to),
                        )
                        .await;
                        return config.selected(&[&choice], &question.text, "timeout", &ctx);
                    }
                    // Fabro's retry outcome. With attempts left the engine
                    // asks again; on the last one `on_retries_exhausted`
                    // decides, explicit routes first.
                    return config.finish(
                        Stage::failed(
                            "human gate timeout, no default",
                            RETRY_REQUESTED_CLASS,
                            config.on_failure,
                        ),
                        &ctx,
                    );
                }
            };
            let Some(answer) = Answer::from_value(&value) else {
                // A steer or any other control payload is not an answer; the
                // question stays open.
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
            if answer.cancelled {
                // The host ended the interview: fail closed, as a cancel does.
                return config.interrupted(&ctx);
            }
            // A multi-select answer names several choices; as Fabro does, the
            // first routes and every selected key and label is recorded.
            let selected: Vec<&Choice> = if answer.choices.is_empty() {
                answer
                    .choice
                    .as_deref()
                    .and_then(|c| config.choice_for(c))
                    .or_else(|| {
                        answer
                            .text
                            .as_ref()
                            .and_then(Value::as_str)
                            .and_then(|t| config.choice_for(t))
                    })
                    .into_iter()
                    .collect()
            } else {
                answer
                    .choices
                    .iter()
                    .filter_map(|c| config.choice_for(c))
                    .collect()
            };
            if !selected.is_empty() {
                // Fabro records the answer word for a yes/no or confirmation
                // gate (`yes`, `no`), the selected key for one choice, and
                // the keys joined for a multi-select.
                let answered = if !answer.choices.is_empty() {
                    answer.choices.join(", ")
                } else if config.is_yes_no() {
                    if HumanConfig::is_affirmative(selected[0]) {
                        "yes".to_owned()
                    } else {
                        "no".to_owned()
                    }
                } else {
                    selected[0].key.clone()
                };
                return config.selected(&selected, &question.text, &answered, &ctx);
            }
            if let (Some(target), Some(text)) = (&config.freeform_target, &answer.text) {
                // Free text: the value (or its `$secret` reference) as
                // written, never resolved here.
                let mut stage = Stage::new(StageOutcome::Succeeded, config.on_failure);
                stage
                    .output
                    .insert("suggested_next_ids".into(), json!([target]));
                stage.output.insert("text".into(), text.clone());
                stage
                    .context_updates
                    .insert(SmolStr::new("human.gate.selected"), json!("freeform"));
                // Fabro records the free text as the label too.
                stage
                    .context_updates
                    .insert(SmolStr::new("human.gate.label"), text.clone());
                stage
                    .context_updates
                    .insert(SmolStr::new("human.gate.text"), text.clone());
                let shown = match text {
                    Value::String(plain) => plain.clone(),
                    other => other.to_string(),
                };
                config.answer_context(&mut stage, &question.text, &shown, None);
                return config.finish(stage, &ctx);
            }
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(label: &str, url: &str) -> Value {
        json!({ "label": label, "url": url, "kind": "document" })
    }

    #[test]
    fn a_review_target_is_validated_as_fabro_validates_it() {
        let ok = review_target(Some(&target(
            "Quarry review exercise",
            "https://quarry.lithos.computer/tmp/0123456789abcdef0123456789abcdef",
        )))
        .expect("valid");
        assert_eq!(ok.label, "Quarry review exercise");
        assert_eq!(ok.kind.as_deref(), Some("document"));
        assert_eq!(review_target(None), Err(ReviewTargetError::Missing));
        assert_eq!(
            review_target(Some(&target("Unsafe", "javascript:alert(1)"))),
            Err(ReviewTargetError::InvalidUrl)
        );
        assert_eq!(
            review_target(Some(&target("Ftp", "ftp://example.com/x"))),
            Err(ReviewTargetError::UnsupportedUrlScheme)
        );
        assert_eq!(
            review_target(Some(&target("Creds", "https://user:pw@example.com/"))),
            Err(ReviewTargetError::UrlContainsCredentials)
        );
        assert_eq!(
            review_target(Some(&target("No host", "https:///path"))),
            Err(ReviewTargetError::MissingUrlHost)
        );
        assert_eq!(
            review_target(Some(&target("", "https://example.com"))),
            Err(ReviewTargetError::EmptyLabel)
        );
        assert_eq!(
            review_target(Some(&target("Pipe", "https://example.com/a|b"))),
            Err(ReviewTargetError::UrlContainsUnsafeCharacters)
        );
        let message = ReviewTargetError::UnsupportedUrlScheme.message("gate");
        assert!(message.contains("must use http or https"), "{message}");
        assert!(!message.contains("javascript"), "{message}");
    }
}
