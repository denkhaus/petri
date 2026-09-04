//! The in-process answer surface for a step's question.
//!
//! A step that needs a person emits a [`Question`]; this observer sees the
//! record, decides the answer — `--auto-approve` takes the question's default,
//! `--interactive` prints it and reads a line from stdin — and delivers it
//! through the coordinator handle into the live firing. A sensitive answer is
//! registered on the run's secret provider under `answer:<id>` and crosses as
//! a `$secret` reference, so its value never enters the log.

use std::io::{self, BufRead as _, Write as _};
use std::sync::{Arc, OnceLock};

use execution::{CoordinatorHandle, CoordinatorRecord, ExecutionId, ExecutionObserver};
use runtime::engine::{EngineState, Event, EventRecord};
use runtime::executor::SecretProvider;
use runtime::ir::FiringId;
use runtime::steps::{Answer, Question};
use serde_json::json;
use tokio::task::spawn_blocking;

/// How the CLI answers questions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Print the question and read the answer from stdin.
    Interactive,
    /// Take the question's default; free text is the empty string.
    AutoApprove,
}

struct Wiring {
    handle:  CoordinatorHandle,
    secrets: Arc<dyn SecretProvider>,
}

/// The observer: constructed before the run, wired to the coordinator once it
/// exists (`run_configured` hands the handle over before the first record).
pub(crate) struct Answerer {
    mode:   Mode,
    wiring: OnceLock<Wiring>,
}

impl Answerer {
    pub(crate) fn new(mode: Mode) -> Self {
        Self {
            mode,
            wiring: OnceLock::new(),
        }
    }

    /// Connect the answerer to a running coordinator.
    pub(crate) fn wire(&self, handle: CoordinatorHandle, secrets: Arc<dyn SecretProvider>) {
        let _ = self.wiring.set(Wiring { handle, secrets });
    }

    #[expect(
        clippy::print_stderr,
        reason = "the question is for the user, on stderr where the CLI's other messages go"
    )]
    fn decide(mode: Mode, question: &Question) -> Answer {
        match mode {
            Mode::AutoApprove => match &question.default {
                Some(key) => Answer::choice(key).for_question(&question.id),
                None => Answer::text("").for_question(&question.id),
            },
            Mode::Interactive => {
                eprintln!();
                eprintln!("question: {}", question.text);
                for option in &question.options {
                    eprintln!("  [{}] {}", option.key, option.label);
                }
                if question.freeform {
                    eprintln!("  (or type an answer)");
                }
                eprint!("answer> ");
                let _ = io::stderr().flush();
                let mut line = String::new();
                let _ = io::stdin().lock().read_line(&mut line);
                let line = line.trim_end_matches(['\n', '\r']).to_string();
                let is_choice = question
                    .options
                    .iter()
                    .any(|o| o.key.eq_ignore_ascii_case(line.trim()) || o.label == line.trim());
                if is_choice {
                    Answer::choice(line.trim()).for_question(&question.id)
                } else {
                    Answer::text(line).for_question(&question.id)
                }
            }
        }
    }
}

impl ExecutionObserver for Answerer {
    fn on_engine_record(&self, execution: ExecutionId, record: &EventRecord, _state: &EngineState) {
        let Event::StepProgress { firing, ev } = &record.event else {
            return;
        };
        let Some(question) = Question::from_event(ev) else {
            return;
        };
        let Some(wiring) = self.wiring.get() else {
            tracing::warn!(question = %question.id, "a question arrived before the answerer was wired");
            return;
        };
        let handle = wiring.handle.clone();
        let secrets = wiring.secrets.clone();
        let mode = self.mode;
        let firing: FiringId = *firing;
        tokio::spawn(async move {
            let sensitive = question.sensitive;
            let secret_name = question.secret_name();
            let mut answer = spawn_blocking(move || Self::decide(mode, &question))
                .await
                .unwrap_or_else(|_| Answer::text(""));
            if sensitive
                && answer.choice.is_none()
                && let Some(serde_json::Value::String(text)) = answer.text.clone()
            {
                // Registered first, then referenced: the log sees the name only.
                if let Err(error) = secrets.register(&secret_name, &text) {
                    tracing::warn!(error = %error, "could not register the answer as a secret");
                }
                answer.text = Some(json!({ "$secret": secret_name }));
            }
            let disposition = handle.deliver(execution, firing, answer.to_control()).await;
            tracing::debug!(?disposition, "answer delivered");
        });
    }

    fn on_lifecycle(&self, _record: &CoordinatorRecord) {}
}
