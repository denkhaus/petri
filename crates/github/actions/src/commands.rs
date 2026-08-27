//! Workflow commands: `::name key=value,key2=value2::message` on stdout.
//!
//! [`CommandSink`] sits between the process step and the driver. Every log event
//! the process step emits passes through it; a stdout line that is a command is
//! applied and swallowed, everything else is forwarded unchanged. `::add-mask::`
//! registers with the run's masker before the next line is forwarded, so the value
//! is masked from then on.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use executor::Masker;
use ir::{LogStream, StepEvent, Value};
use serde_json::Map;
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowCommand {
    pub name: String,
    pub properties: BTreeMap<String, String>,
    pub message: String,
}

/// Parse one stdout line. `None` when it is not a command.
pub fn parse(line: &str) -> Option<WorkflowCommand> {
    let rest = line.strip_prefix("::")?;
    let (head, message) = rest.split_once("::")?;
    let (name, props) = match head.split_once(' ') {
        Some((n, p)) => (n, Some(p)),
        None => (head, None),
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let mut properties = BTreeMap::new();
    if let Some(props) = props {
        for pair in props.split(',') {
            if let Some((k, v)) = pair.split_once('=') {
                properties.insert(k.trim().to_string(), unescape_property(v));
            }
        }
    }
    Some(WorkflowCommand {
        name: name.to_string(),
        properties,
        message: unescape_data(message),
    })
}

/// The toolkit escapes `%`, `\r`, `\n` in a message, in that order; undo in reverse.
fn unescape_data(s: &str) -> String {
    s.replace("%0A", "\n")
        .replace("%0D", "\r")
        .replace("%25", "%")
}

/// Property values additionally escape `:` and `,`.
fn unescape_property(s: &str) -> String {
    s.replace("%0A", "\n")
        .replace("%0D", "\r")
        .replace("%3A", ":")
        .replace("%2C", ",")
        .replace("%25", "%")
}

/// What the commands asked for, collected while the process ran.
#[derive(Debug, Default)]
pub struct CommandEffects {
    /// `::set-output name=X::value` (deprecated; still emitted by older actions).
    pub outputs: Map<String, Value>,
    /// `::save-state name=X::value` (deprecated in favour of `GITHUB_STATE`).
    pub state: Map<String, Value>,
    /// `::set-env name=X::value`, only when unsecure commands are allowed.
    pub env: Map<String, Value>,
    /// `::add-path::dir`, only when unsecure commands are allowed.
    pub path: Vec<String>,
    /// `::error::`, `::warning::`, `::notice::` messages, by level.
    pub annotations: Vec<(String, String)>,
}

/// The sink one step's log events pass through.
pub struct CommandSink {
    out: mpsc::Sender<StepEvent>,
    masker: Masker,
    /// `ACTIONS_ALLOW_UNSECURE_COMMANDS`: whether `set-env` and `add-path` apply.
    allow_unsecure: bool,
    /// `::stop-commands::token` is in effect until `::token::`.
    stopped: Option<String>,
    /// `::echo::on`: commands are also forwarded as lines.
    echo: bool,
    effects: Arc<Mutex<CommandEffects>>,
}

impl CommandSink {
    pub fn new(out: mpsc::Sender<StepEvent>, masker: Masker, allow_unsecure: bool) -> Self {
        Self {
            out,
            masker,
            allow_unsecure,
            stopped: None,
            echo: false,
            effects: Arc::new(Mutex::new(CommandEffects::default())),
        }
    }

    /// The effects so far, shared: readable even if the process left a straggler
    /// holding stdout open and the sink never sees the end of the stream.
    pub fn effects(&self) -> Arc<Mutex<CommandEffects>> {
        Arc::clone(&self.effects)
    }

    /// Consume events until the sender side is gone.
    pub async fn run(mut self, mut rx: mpsc::Receiver<StepEvent>) {
        while let Some(event) = rx.recv().await {
            match event {
                StepEvent::Log {
                    stream: LogStream::Stdout,
                    line,
                } => self.on_stdout(line).await,
                other => {
                    let _ = self.out.send(other).await;
                }
            }
        }
    }

    async fn forward(&self, line: String) {
        let _ = self
            .out
            .send(StepEvent::Log {
                stream: LogStream::Stdout,
                line,
            })
            .await;
    }

    async fn on_stdout(&mut self, line: String) {
        if let Some(token) = &self.stopped {
            if line.trim() == format!("::{token}::") {
                self.stopped = None;
            } else {
                self.forward(line).await;
            }
            return;
        }
        let Some(cmd) = parse(&line) else {
            return self.forward(line).await;
        };
        if self.echo && cmd.name != "add-mask" {
            self.forward(line.clone()).await;
        }
        let name_prop = cmd.properties.get("name").cloned();
        // Apply under the lock, forward after it: the guard must not live across
        // the send.
        let forward: Option<String> = {
            let mut effects = self.effects.lock().expect("effects are not poisoned");
            match cmd.name.as_str() {
                "add-mask" => {
                    self.masker.register(&cmd.message);
                    None
                }
                "save-state" => {
                    if let Some(name) = name_prop {
                        effects.state.insert(name, Value::String(cmd.message));
                    }
                    None
                }
                "set-output" => {
                    if let Some(name) = name_prop {
                        effects.outputs.insert(name, Value::String(cmd.message));
                    }
                    None
                }
                "set-env" if self.allow_unsecure => {
                    if let Some(name) = name_prop {
                        effects.env.insert(name, Value::String(cmd.message));
                    }
                    None
                }
                "add-path" if self.allow_unsecure => {
                    effects.path.push(cmd.message);
                    None
                }
                "set-env" | "add-path" => Some(format!(
                    "Warning: `::{}::` is disabled; use the `GITHUB_ENV` / `GITHUB_PATH` files \
                     (ACTIONS_ALLOW_UNSECURE_COMMANDS enables the old commands)",
                    cmd.name
                )),
                "debug" | "add-matcher" | "remove-matcher" | "endgroup" => None,
                "group" => Some(format!("▶ {}", cmd.message)),
                level @ ("notice" | "warning" | "error") => {
                    effects
                        .annotations
                        .push((level.to_string(), cmd.message.clone()));
                    let label = match level {
                        "notice" => "Notice",
                        "warning" => "Warning",
                        _ => "Error",
                    };
                    Some(format!("{label}: {}", cmd.message))
                }
                "echo" => {
                    self.echo = cmd.message.trim().eq_ignore_ascii_case("on");
                    None
                }
                "stop-commands" => {
                    self.stopped = Some(cmd.message);
                    None
                }
                _ => Some(line),
            }
        };
        if let Some(text) = forward {
            self.forward(text).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_with_and_without_properties() {
        let cmd = parse("::save-state name=token::abc%0Adef").unwrap();
        assert_eq!(cmd.name, "save-state");
        assert_eq!(cmd.properties["name"], "token");
        assert_eq!(cmd.message, "abc\ndef");

        let cmd = parse("::add-mask::s3cret%25").unwrap();
        assert_eq!(cmd.name, "add-mask");
        assert!(cmd.properties.is_empty());
        assert_eq!(cmd.message, "s3cret%");

        let cmd = parse("::error file=a.rs,line=3,title=Oops%3A%2C::boom").unwrap();
        assert_eq!(cmd.properties["title"], "Oops:,");
        assert_eq!(cmd.properties["line"], "3");
    }

    #[test]
    fn plain_lines_are_not_commands() {
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("::not a command"), None);
        assert_eq!(parse("::::"), None);
        assert_eq!(parse(":: spaced::x"), None);
    }

    #[tokio::test]
    async fn the_sink_applies_commands_and_forwards_the_rest() {
        let (out_tx, mut out_rx) = mpsc::channel(16);
        let (tx, rx) = mpsc::channel(16);
        let masker = Masker::new();
        let sink = CommandSink::new(out_tx, masker.clone(), false);
        let effects = sink.effects();
        let task = tokio::spawn(sink.run(rx));
        for line in [
            "plain",
            "::add-mask::hunter2-long",
            "::save-state name=k::v",
            "::set-output name=o::out",
            "::set-env name=E::1",
            "::group::title",
            "::error::bad",
            "::stop-commands::tok",
            "::save-state name=ignored::x",
            "::tok::",
            "::debug::hidden",
            "after",
        ] {
            tx.send(StepEvent::Log {
                stream: LogStream::Stdout,
                line: line.into(),
            })
            .await
            .unwrap();
        }
        drop(tx);
        task.await.unwrap();

        let mut lines = Vec::new();
        while let Ok(ev) = out_rx.try_recv() {
            if let StepEvent::Log { line, .. } = ev {
                lines.push(line);
            }
        }
        assert_eq!(
            lines,
            vec![
                "plain",
                "Warning: `::set-env::` is disabled; use the `GITHUB_ENV` / `GITHUB_PATH` files \
                 (ACTIONS_ALLOW_UNSECURE_COMMANDS enables the old commands)",
                "▶ title",
                "Error: bad",
                "::save-state name=ignored::x",
                "after",
            ]
        );
        let effects = effects.lock().unwrap();
        assert_eq!(effects.state["k"], "v");
        assert_eq!(effects.outputs["o"], "out");
        assert!(effects.env.is_empty(), "set-env is disabled by default");
        assert_eq!(
            effects.annotations,
            vec![("error".to_string(), "bad".to_string())]
        );
        assert_eq!(masker.mask("got hunter2-long"), "got ***");
    }
}
