//! The command line's interviewers: who answers a run's questions.
//!
//! Three implementations of [`Interviewer`], one per `petri run` option:
//! [`TerminalInterviewer`] for `--interactive`, [`AutoApproveInterviewer`]
//! for `--auto-approve`, and [`ScriptedInterviewer`] for
//! `--interview-script <file>`. Correlation, secret registration, delivery and
//! the receipt are the [`InterviewDispatcher`]'s; these decide answers only.
//!
//! # The interview script
//!
//! A JSON document, version 1:
//!
//! ```json
//! {
//!   "version": 1,
//!   "entries": [
//!     {
//!       "id": "approve-plan",
//!       "match": { "node": "gate", "kind": "yes_no", "options": ["Y", "N"] },
//!       "count": 1,
//!       "action": { "kind": "choice", "value": "Y" }
//!     }
//!   ]
//! }
//! ```
//!
//! Every `match` field is optional and every present field must hold:
//! `node` (the node instance name), `invocation_path` (`/` for the root,
//! `/<slot>` per nested call), `occurrence` (which distinct question of that
//! node, 1-based), `ask` (which time this exact question was asked, 1-based;
//! greater than one only after the step rejected an answer), `kind` (the
//! format's question type), `text` (exact), `text_contains`, `options` (the
//! offered keys, in order), `default`, `freeform`, `sensitive`,
//! `reference_url_contains` (the question carries a review reference whose
//! URL contains the text). A question that matches no entry, or more than
//! one, fails the interview; so does a matching entry that has already
//! answered `count` times.
//!
//! Actions: `choice` (`value`: a key or label), `choices` (`values`: keys or
//! labels, for `multi_select`), `text` (`value`), `negative` (the option
//! keyed `N`/`NO` or labelled `no`), `invalid` (`value`: text sent as a choice
//! the step must reject; the re-ask needs its own entry with `"ask": 2`),
//! `cancel` (the interview is cancelled; the gate fails closed), `withhold`
//! (no reply until the question is cancelled). `delay_ms` on an entry waits
//! that long before the action, or until the question is cancelled.
//!
//! `count` defaults to 1. `required` defaults to true: an entry that answered
//! fewer than `count` times when the run ends fails the interview. Reservation
//! and consumption are one step under one lock, so concurrent questions cannot
//! consume the same use of an entry.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{self, BufRead as _, IsTerminal as _, Write as _};
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use std::{fs, thread};

use execution::{InterviewError, InterviewReply, InterviewRequest, Interviewer};
use runtime::steps::{Answer, Question, QuestionOption};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// The script format this module reads.
pub const SCRIPT_VERSION: u32 = 1;

/// How many times the terminal re-prompts on input that names no choice.
const TERMINAL_ATTEMPTS: u32 = 3;

/// The option a key or label names, case-insensitively, without a `[K] `
/// accelerator prefix on the label.
fn option_for<'a>(question: &'a Question, text: &str) -> Option<&'a QuestionOption> {
    let wanted = text.trim();
    if wanted.is_empty() {
        return None;
    }
    question.options.iter().find(|option| {
        option.key.eq_ignore_ascii_case(wanted)
            || option.label.eq_ignore_ascii_case(wanted)
            || plain_label(&option.label).eq_ignore_ascii_case(wanted)
    })
}

/// A label without its `[K] ` accelerator.
fn plain_label(label: &str) -> &str {
    let trimmed = label.trim();
    if trimmed.starts_with('[')
        && let Some(end) = trimmed.find(']')
    {
        return trimmed[end + 1..].trim();
    }
    trimmed
}

/// A deadline in the largest whole unit that fits: `90s`, `15m`, `2h`.
fn render_deadline(ms: u64) -> String {
    let seconds = ms.div_ceil(1000);
    if seconds >= 3600 && seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// `--auto-approve`: the question's default choice, else the empty text.
pub struct AutoApproveInterviewer;

impl AutoApproveInterviewer {
    pub fn answer(question: &Question) -> Answer {
        match &question.default {
            Some(key) => Answer::choice(key),
            None => Answer::text(""),
        }
    }
}

#[async_trait::async_trait]
impl Interviewer for AutoApproveInterviewer {
    async fn reply(&self, request: InterviewRequest, _cancel: CancellationToken) -> InterviewReply {
        InterviewReply::Answered(Self::answer(&request.question))
    }
}

/// `--interactive`: print the question on stderr, read one line from stdin.
///
/// One question at a time: a second question waits for the first answer, so
/// a line typed for one gate never lands on another. A dedicated thread owns
/// stdin and forwards lines; a wait ends when the line arrives or the
/// question is cancelled. EOF fails the interview: with no terminal input
/// left there is nothing to wait for. Input that names no offered choice on a
/// question without free text is re-prompted, three times, then fails.
pub struct TerminalInterviewer {
    lines:    AsyncMutex<mpsc::UnboundedReceiver<io::Result<String>>>,
    terminal: bool,
}

impl TerminalInterviewer {
    /// Start the stdin thread. When stdin is not a terminal the lines are read
    /// all the same (piped input is how a test drives this), with a note that
    /// nobody is watching the prompt.
    pub fn start() -> io::Result<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let terminal = io::stdin().is_terminal();
        thread::Builder::new()
            .name("petri-terminal-input".into())
            .spawn(move || {
                let stdin = io::stdin();
                let mut lock = stdin.lock();
                loop {
                    let mut line = String::new();
                    let result = match lock.read_line(&mut line) {
                        Ok(0) => Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "standard input reached EOF",
                        )),
                        Ok(_) => Ok(line.trim_end_matches(['\n', '\r']).to_owned()),
                        Err(error) => Err(error),
                    };
                    let stop = result.is_err();
                    if sender.send(result).is_err() || stop {
                        return;
                    }
                }
            })?;
        Ok(Self {
            lines: AsyncMutex::new(receiver),
            terminal,
        })
    }

    /// The prompt for a question, in Fabro's five presentations plus the
    /// native default.
    fn render(request: &InterviewRequest) -> String {
        let question = &request.question;
        let mut out = String::new();
        let _ = writeln!(out);
        let where_ = if request.invocation_path == "/" {
            request.node.to_string()
        } else {
            format!("{}/{}", request.invocation_path, request.node)
        };
        let _ = writeln!(out, "question [{where_}]: {}", question.text);
        if let Some(reference) = &question.reference {
            let _ = writeln!(out, "  review: {} <{}>", reference.label, reference.url);
        }
        if let Some(ms) = question.timeout_ms {
            let _ = writeln!(out, "  (answer within {})", render_deadline(ms));
        }
        match question.kind.as_deref() {
            Some("yes_no" | "confirmation") => {
                let keys = question
                    .options
                    .iter()
                    .map(|option| format!("[{}] {}", option.key, plain_label(&option.label)))
                    .collect::<Vec<_>>()
                    .join("  ");
                let _ = writeln!(out, "  {keys}");
                if let Some(default) = &question.default {
                    let _ = writeln!(out, "  (Enter takes [{default}])");
                }
            }
            Some("multi_select") => {
                for option in &question.options {
                    let _ = writeln!(out, "  [{}] {}", option.key, plain_label(&option.label));
                }
                let _ = writeln!(out, "  (several keys, separated by commas)");
            }
            Some("freeform") => {
                for option in &question.options {
                    let _ = writeln!(out, "  [{}] {}", option.key, plain_label(&option.label));
                }
                let _ = writeln!(out, "  (type an answer)");
            }
            _ => {
                for option in &question.options {
                    let _ = writeln!(out, "  [{}] {}", option.key, plain_label(&option.label));
                }
                if question.freeform {
                    let _ = writeln!(out, "  (or type an answer)");
                }
            }
        }
        if question.sensitive {
            let _ = writeln!(
                out,
                "  (sensitive: the answer is stored as a secret; the terminal still echoes it)"
            );
        }
        let _ = write!(out, "answer> ");
        out
    }

    /// The answer a typed line means, or why it means nothing.
    fn parse(question: &Question, line: &str) -> Result<Answer, String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let Some(default) = &question.default
                && matches!(question.kind.as_deref(), Some("yes_no" | "confirmation"))
            {
                return Ok(Answer::choice(default));
            }
            if question.freeform && question.options.is_empty() {
                return Ok(Answer::text(""));
            }
            return Err("an empty answer names no choice".to_owned());
        }
        if question.kind.as_deref() == Some("multi_select") && trimmed.contains(',') {
            let mut keys = Vec::new();
            for part in trimmed.split(',') {
                match option_for(question, part) {
                    Some(option) => keys.push(option.key.clone()),
                    None => return Err(format!("`{}` names no choice", part.trim())),
                }
            }
            return Ok(Answer::choices(keys));
        }
        if let Some(option) = option_for(question, trimmed) {
            return Ok(Answer::choice(&option.key));
        }
        if question.freeform {
            return Ok(Answer::text(trimmed));
        }
        Err(format!(
            "`{trimmed}` names no choice; the choices are {}",
            question
                .options
                .iter()
                .map(|option| format!("[{}] {}", option.key, plain_label(&option.label)))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

#[async_trait::async_trait]
impl Interviewer for TerminalInterviewer {
    #[expect(
        clippy::print_stderr,
        reason = "the question is for the user, on stderr where the CLI's other messages go"
    )]
    async fn reply(&self, request: InterviewRequest, cancel: CancellationToken) -> InterviewReply {
        // One keyboard: the lock serializes questions from parallel stages.
        let mut lines = tokio::select! {
            lines = self.lines.lock() => lines,
            () = cancel.cancelled() => return InterviewReply::Cancelled,
        };
        if !self.terminal {
            tracing::warn!(
                question = %request.question.id,
                "standard input is not a terminal; reading the answer from it anyway"
            );
        }
        let mut attempts = 0;
        loop {
            eprint!("{}", Self::render(&request));
            let _ = io::stderr().flush();
            let line = tokio::select! {
                line = lines.recv() => line,
                () = cancel.cancelled() => return InterviewReply::Cancelled,
            };
            let line = match line {
                Some(Ok(line)) => line,
                Some(Err(error)) => {
                    return InterviewReply::Failed(InterviewError::with_source(
                        "could not read the answer from standard input",
                        error,
                    ));
                }
                None => {
                    return InterviewReply::Failed(InterviewError::new(
                        "standard input closed before an answer was read",
                    ));
                }
            };
            match Self::parse(&request.question, &line) {
                Ok(answer) => return InterviewReply::Answered(answer),
                Err(problem) => {
                    attempts += 1;
                    eprintln!("{problem}");
                    if attempts >= TERMINAL_ATTEMPTS {
                        return InterviewReply::Failed(InterviewError::new(format!(
                            "no valid answer after {TERMINAL_ATTEMPTS} attempts"
                        )));
                    }
                }
            }
        }
    }
}

/// The document `--interview-script` reads.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InterviewScript {
    pub version: u32,
    pub entries: Vec<ScriptEntry>,
}

/// One scripted reply.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptEntry {
    /// Stable, unique within the script.
    pub id:       String,
    #[serde(default, rename = "match")]
    pub matcher:  Matcher,
    /// How many questions this entry answers.
    #[serde(default = "one")]
    pub count:    u32,
    /// Whether answering fewer than `count` times fails the interview.
    #[serde(default = "yes")]
    pub required: bool,
    /// Wait this long before acting, or until the question is cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    pub action:   Action,
}

fn one() -> u32 {
    1
}

fn yes() -> bool {
    true
}

/// Which questions an entry answers. Every present field must hold.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Matcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node:                   Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_path:        Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence:             Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask:                    Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind:                   Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text:                   Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_contains:          Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options:                Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default:                Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freeform:               Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitive:              Option<bool>,
    /// The question carries a review reference whose URL contains this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_url_contains: Option<String>,
}

impl Matcher {
    fn matches(&self, request: &InterviewRequest) -> bool {
        let q = &request.question;
        self.node.as_deref().is_none_or(|n| n == request.node)
            && self
                .invocation_path
                .as_deref()
                .is_none_or(|p| p == request.invocation_path)
            && self.occurrence.is_none_or(|o| o == request.occurrence)
            && self.ask.is_none_or(|a| a == request.ask)
            && self
                .kind
                .as_ref()
                .is_none_or(|k| Some(k) == q.kind.as_ref())
            && self.text.as_deref().is_none_or(|t| t == q.text)
            && self
                .text_contains
                .as_deref()
                .is_none_or(|t| q.text.contains(t))
            && self.options.as_ref().is_none_or(|keys| {
                keys.len() == q.options.len()
                    && keys.iter().zip(&q.options).all(|(k, o)| *k == o.key)
            })
            && self
                .default
                .as_ref()
                .is_none_or(|d| Some(d) == q.default.as_ref())
            && self.freeform.is_none_or(|f| f == q.freeform)
            && self.sensitive.is_none_or(|s| s == q.sensitive)
            && self.reference_url_contains.as_deref().is_none_or(|needle| {
                q.reference
                    .as_ref()
                    .is_some_and(|reference| reference.url.contains(needle))
            })
    }
}

/// What an entry does with a matching question.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Choice { value: String },
    Choices { values: Vec<String> },
    Text { value: String },
    Negative,
    Invalid { value: String },
    Cancel,
    Withhold,
}

#[derive(Debug)]
struct Use {
    consumed:  u32,
    questions: Vec<String>,
}

/// `--interview-script`: pre-canned answers under strict expectations. Never
/// falls back to the terminal or to auto-approval.
pub struct ScriptedInterviewer {
    entries: Vec<ScriptEntry>,
    uses:    Mutex<Vec<Use>>,
}

/// Why a script could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    #[error("could not read `{path}`")]
    Read {
        path:   String,
        #[source]
        source: io::Error,
    },
    #[error("`{path}` is not an interview script")]
    Parse {
        path:   String,
        #[source]
        source: serde_json::Error,
    },
    #[error("`{path}` is interview script version {found}; this petri reads version {expected}")]
    Version {
        path:     String,
        found:    u32,
        expected: u32,
    },
    #[error("`{path}`: {problem}")]
    Invalid { path: String, problem: String },
}

impl ScriptedInterviewer {
    pub fn load(path: &Path) -> Result<Self, ScriptError> {
        let name = path.display().to_string();
        let text = fs::read_to_string(path).map_err(|source| ScriptError::Read {
            path: name.clone(),
            source,
        })?;
        let script: InterviewScript =
            serde_json::from_str(&text).map_err(|source| ScriptError::Parse {
                path: name.clone(),
                source,
            })?;
        Self::new(script).map_err(|problem| ScriptError::Invalid {
            path: name,
            problem,
        })
    }

    pub fn new(script: InterviewScript) -> Result<Self, String> {
        if script.version != SCRIPT_VERSION {
            return Err(format!(
                "interview script version {} is not the supported version {SCRIPT_VERSION}",
                script.version
            ));
        }
        let mut seen = BTreeSet::new();
        for entry in &script.entries {
            if entry.id.trim().is_empty() {
                return Err("an entry has an empty id".to_owned());
            }
            if !seen.insert(entry.id.clone()) {
                return Err(format!("entry id `{}` is used twice", entry.id));
            }
            if entry.count == 0 {
                return Err(format!("entry `{}` has count 0", entry.id));
            }
            if let Action::Choices { values } = &entry.action
                && values.is_empty()
            {
                return Err(format!("entry `{}` chooses nothing", entry.id));
            }
        }
        let uses = script
            .entries
            .iter()
            .map(|_| Use {
                consumed:  0,
                questions: Vec::new(),
            })
            .collect();
        Ok(Self {
            entries: script.entries,
            uses:    Mutex::new(uses),
        })
    }

    /// Reserve one use of the single entry matching `request`. One lock
    /// covers the search and the consumption.
    fn reserve(&self, request: &InterviewRequest) -> Result<usize, InterviewError> {
        let mut uses = self.uses.lock().unwrap_or_else(PoisonError::into_inner);
        let matching: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.matcher.matches(request))
            .map(|(index, _)| index)
            .collect();
        let describe = || {
            format!(
                "node `{}` at `{}` occurrence {} ask {} (`{}`)",
                request.node,
                request.invocation_path,
                request.occurrence,
                request.ask,
                request.question.text
            )
        };
        match matching.as_slice() {
            [] => Err(InterviewError::new(format!(
                "no script entry matches the question from {}",
                describe()
            ))),
            [index] => {
                let entry = &self.entries[*index];
                let use_ = &mut uses[*index];
                if use_.consumed >= entry.count {
                    return Err(InterviewError::new(format!(
                        "script entry `{}` is exhausted (count {}) at the question from {}",
                        entry.id,
                        entry.count,
                        describe()
                    )));
                }
                use_.consumed += 1;
                use_.questions.push(request.question.id.clone());
                Ok(*index)
            }
            many => Err(InterviewError::new(format!(
                "script entries {} all match the question from {}",
                many.iter()
                    .map(|index| format!("`{}`", self.entries[*index].id))
                    .collect::<Vec<_>>()
                    .join(", "),
                describe()
            ))),
        }
    }

    fn act(entry: &ScriptEntry, question: &Question) -> InterviewReply {
        let invalid = |problem: String| {
            InterviewReply::Failed(InterviewError::new(format!(
                "script entry `{}`: {problem}",
                entry.id
            )))
        };
        match &entry.action {
            Action::Choice { value } => match option_for(question, value) {
                Some(option) => InterviewReply::Answered(Answer::choice(&option.key)),
                None if question.freeform => InterviewReply::Answered(Answer::text(value.as_str())),
                None => invalid(format!("`{value}` names none of the offered choices")),
            },
            Action::Choices { values } => {
                let mut keys = Vec::new();
                for value in values {
                    match option_for(question, value) {
                        Some(option) => keys.push(option.key.clone()),
                        None => {
                            return invalid(format!("`{value}` names none of the offered choices"));
                        }
                    }
                }
                InterviewReply::Answered(Answer::choices(keys))
            }
            Action::Text { value } => {
                if !question.freeform {
                    return invalid("the question takes no free text".to_owned());
                }
                InterviewReply::Answered(Answer::text(value.as_str()))
            }
            Action::Negative => {
                let negative = question.options.iter().find(|option| {
                    option.key.eq_ignore_ascii_case("n")
                        || option.key.eq_ignore_ascii_case("no")
                        || plain_label(&option.label).eq_ignore_ascii_case("no")
                });
                match negative {
                    Some(option) => InterviewReply::Answered(Answer::choice(&option.key)),
                    None => invalid("the question offers no negative choice".to_owned()),
                }
            }
            Action::Invalid { value } => InterviewReply::Answered(Answer::choice(value)),
            // `Withhold` waits for the cancel token in `reply`; by the time it
            // acts, the interview is over.
            Action::Cancel | Action::Withhold => InterviewReply::Cancelled,
        }
    }
}

#[async_trait::async_trait]
impl Interviewer for ScriptedInterviewer {
    async fn reply(&self, request: InterviewRequest, cancel: CancellationToken) -> InterviewReply {
        let index = match self.reserve(&request) {
            Ok(index) => index,
            Err(error) => return InterviewReply::Failed(error),
        };
        let entry = &self.entries[index];
        if let Some(delay) = entry.delay_ms {
            tokio::select! {
                () = sleep(Duration::from_millis(delay)) => {},
                () = cancel.cancelled() => return InterviewReply::Cancelled,
            }
        }
        if matches!(entry.action, Action::Withhold) {
            cancel.cancelled().await;
            return InterviewReply::Cancelled;
        }
        Self::act(entry, &request.question)
    }

    async fn finish(&self) -> Result<Option<Value>, InterviewError> {
        let uses = self.uses.lock().unwrap_or_else(PoisonError::into_inner);
        let summary: Vec<Value> = self
            .entries
            .iter()
            .zip(uses.iter())
            .map(|(entry, use_)| {
                json!({
                    "id": entry.id,
                    "count": entry.count,
                    "consumed": use_.consumed,
                    "remaining": entry.count.saturating_sub(use_.consumed),
                    "required": entry.required,
                    "questions": use_.questions,
                })
            })
            .collect();
        let unused: Vec<String> = self
            .entries
            .iter()
            .zip(uses.iter())
            .filter(|(entry, use_)| entry.required && use_.consumed < entry.count)
            .map(|(entry, use_)| {
                format!(
                    "`{}` answered {} of {} expected question(s)",
                    entry.id, use_.consumed, entry.count
                )
            })
            .collect();
        if unused.is_empty() {
            Ok(Some(json!({ "entries": summary })))
        } else {
            Err(InterviewError::new(format!(
                "unused required interview script entries: {}",
                unused.join("; ")
            ))
            .with_report(json!({ "entries": summary })))
        }
    }
}

#[cfg(test)]
mod tests {
    use execution::{ExecutionId, InvocationId};
    use runtime::ir::{Attempt, FiringId};
    use runtime::steps::QuestionReference;
    use tokio::time::timeout;

    use super::*;

    fn request(node: &str, kind: Option<&str>, options: &[(&str, &str)]) -> InterviewRequest {
        InterviewRequest {
            invocation:      InvocationId::ROOT,
            invocation_path: "/".into(),
            execution:       ExecutionId::new(0),
            firing:          FiringId::new(1),
            attempt:         Attempt::new(1),
            node:            node.into(),
            occurrence:      1,
            ask:             1,
            question:        Question {
                id:         format!("{node}#1"),
                text:       "Ship it?".into(),
                options:    options
                    .iter()
                    .map(|(key, label)| QuestionOption::new(*key, *label))
                    .collect(),
                default:    options.first().map(|(key, _)| (*key).into()),
                freeform:   false,
                sensitive:  false,
                kind:       kind.map(Into::into),
                reference:  None,
                timeout_ms: None,
                context:    None,
            },
        }
    }

    fn script(entries: &Value) -> ScriptedInterviewer {
        let script: InterviewScript =
            serde_json::from_value(json!({ "version": 1, "entries": entries })).unwrap();
        ScriptedInterviewer::new(script).unwrap()
    }

    #[tokio::test]
    async fn a_scripted_choice_answers_by_key_or_label() {
        let interviewer = script(&json!([
            { "id": "gate", "match": { "node": "gate" }, "action": { "kind": "choice", "value": "no" } }
        ]));
        let reply = interviewer
            .reply(
                request("gate", Some("yes_no"), &[("Y", "[Y] Yes"), ("N", "[N] No")]),
                CancellationToken::new(),
            )
            .await;
        let InterviewReply::Answered(answer) = reply else {
            panic!("answered");
        };
        assert_eq!(answer.choice.as_deref(), Some("N"));
        assert!(interviewer.finish().await.is_ok());
    }

    #[tokio::test]
    async fn unexpected_ambiguous_and_exhausted_questions_fail() {
        let interviewer = script(&json!([
            { "id": "a", "match": { "node": "gate" }, "action": { "kind": "choice", "value": "Y" } },
            { "id": "b", "match": { "kind": "yes_no" }, "action": { "kind": "choice", "value": "Y" } }
        ]));
        let ambiguous = interviewer
            .reply(
                request("gate", Some("yes_no"), &[("Y", "Yes")]),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(ambiguous, InterviewReply::Failed(_)));
        let unexpected = interviewer
            .reply(
                request("other", None, &[("Y", "Yes")]),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(unexpected, InterviewReply::Failed(_)));
        let once = script(&json!([
            { "id": "a", "match": { "node": "gate" }, "action": { "kind": "choice", "value": "Y" } }
        ]));
        let first = once
            .reply(
                request("gate", None, &[("Y", "Yes")]),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(first, InterviewReply::Answered(_)));
        let second = once
            .reply(
                request("gate", None, &[("Y", "Yes")]),
                CancellationToken::new(),
            )
            .await;
        let InterviewReply::Failed(error) = second else {
            panic!("exhausted");
        };
        assert!(error.message().contains("exhausted"), "{error}");
    }

    #[tokio::test]
    async fn unused_required_entries_fail_at_finish() {
        let interviewer = script(&json!([
            { "id": "a", "match": { "node": "gate" }, "action": { "kind": "choice", "value": "Y" } },
            { "id": "b", "match": { "node": "other" }, "required": false, "action": { "kind": "cancel" } }
        ]));
        let error = interviewer.finish().await.unwrap_err();
        assert!(error.message().contains("`a`"), "{error}");
        assert!(!error.message().contains("`b`"), "{error}");
    }

    #[tokio::test]
    async fn withhold_waits_for_cancellation() {
        let interviewer = script(&json!([
            { "id": "a", "match": { "node": "gate" }, "action": { "kind": "withhold" } }
        ]));
        let cancel = CancellationToken::new();
        let pending = interviewer.reply(request("gate", None, &[("Y", "Yes")]), cancel.clone());
        tokio::pin!(pending);
        assert!(
            timeout(Duration::from_millis(50), &mut pending)
                .await
                .is_err()
        );
        cancel.cancel();
        assert!(matches!(pending.await, InterviewReply::Cancelled));
    }

    #[test]
    fn terminal_input_parses_each_presentation() {
        let yes_no = request("g", Some("yes_no"), &[("Y", "[Y] Yes"), ("N", "[N] No")]);
        assert_eq!(
            TerminalInterviewer::parse(&yes_no.question, "")
                .unwrap()
                .choice,
            Some("Y".into())
        );
        assert_eq!(
            TerminalInterviewer::parse(&yes_no.question, "no")
                .unwrap()
                .choice,
            Some("N".into())
        );
        assert!(TerminalInterviewer::parse(&yes_no.question, "maybe").is_err());
        let multi = request("g", Some("multi_select"), &[
            ("A", "Apples"),
            ("B", "Bread"),
        ]);
        let picked = TerminalInterviewer::parse(&multi.question, "a, bread").unwrap();
        assert_eq!(picked.choices, vec!["A".to_owned(), "B".to_owned()]);
        assert_eq!(picked.choice.as_deref(), Some("A"));
        let mut free = request("g", Some("freeform"), &[]);
        free.question.freeform = true;
        assert_eq!(
            TerminalInterviewer::parse(&free.question, "ship it")
                .unwrap()
                .text,
            Some(json!("ship it"))
        );
    }

    #[test]
    fn a_review_reference_and_a_deadline_are_shown_with_the_question() {
        let mut request = request("gate", Some("yes_no"), &[("Y", "[Y] Yes"), ("N", "[N] No")]);
        request.question.reference = Some(QuestionReference {
            label: "the plan".into(),
            url:   "https://example.com/plan".into(),
            kind:  Some("document".into()),
        });
        request.question.timeout_ms = Some(90_000);
        let rendered = TerminalInterviewer::render(&request);
        assert!(
            rendered.contains("review: the plan <https://example.com/plan>"),
            "{rendered}"
        );
        assert!(rendered.contains("(answer within 90s)"), "{rendered}");
        assert_eq!(render_deadline(3_600_000), "1h");
        assert_eq!(render_deadline(900_000), "15m");
        let matcher: Matcher =
            serde_json::from_value(json!({ "reference_url_contains": "example.com" })).unwrap();
        assert!(matcher.matches(&request));
        let other: Matcher =
            serde_json::from_value(json!({ "reference_url_contains": "elsewhere" })).unwrap();
        assert!(!other.matches(&request));
    }

    #[test]
    fn a_script_with_the_wrong_version_is_refused() {
        let script: InterviewScript =
            serde_json::from_value(json!({ "version": 2, "entries": [] })).unwrap();
        assert!(ScriptedInterviewer::new(script).is_err());
    }
}
