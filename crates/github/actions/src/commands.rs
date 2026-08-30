//! Workflow commands: `::name key=value,key2=value2::message` on stdout.
//!
//! [`CommandSink`] sits between the process step and the driver. Every log
//! event the process step emits passes through it; a line that is a command is
//! applied and swallowed, everything else is forwarded unchanged. Both streams
//! are read: the runner attaches its command-aware output manager to stdout and
//! stderr alike (`ScriptHandler.cs`), so a `::error::` echoed to stderr is a
//! command there too. `::add-mask::` registers with the run's masker before the
//! next line is forwarded, so the value is masked from then on.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use executor::Masker;
use ir::{LogStream, StepEvent, Value};
use serde_json::Map;
use tokio::sync::mpsc;

use crate::session::set_env_blocked;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowCommand {
    pub name:       String,
    pub properties: BTreeMap<String, String>,
    pub message:    String,
}

/// Parse one output line. `None` when it is not a command. Leading whitespace
/// is ignored, as the runner's `TryParseV2` trims it before looking for `::`.
pub fn parse(line: &str) -> Option<WorkflowCommand> {
    let rest = line.trim_start().strip_prefix("::")?;
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

/// The toolkit escapes `%`, `\r`, `\n` in a message, in that order; undo in
/// reverse.
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
    /// `::set-output name=X::value` (deprecated; still emitted by older
    /// actions).
    pub outputs: Map<String, Value>,
    /// `::save-state name=X::value` (deprecated in favour of `GITHUB_STATE`).
    pub state:   Map<String, Value>,
    /// `::set-env name=X::value`, only when unsecure commands are allowed.
    pub env:     Map<String, Value>,
    /// `::add-path::dir`, only when unsecure commands are allowed.
    pub path:    Vec<String>,
    /// The names of commands the sink refused — `set-env`/`add-path` without
    /// the opt-in. The runner's extension throws, `TryProcessCommand` records
    /// `CommandResult = Failed`, and the step fails once its process ends,
    /// whatever the exit code; the fold applies the same verdict.
    pub refused: Vec<String>,
}

/// The sink one step's log events pass through.
pub struct CommandSink {
    out:            mpsc::Sender<StepEvent>,
    masker:         Masker,
    /// `ACTIONS_ALLOW_UNSECURE_COMMANDS`: whether `set-env` and `add-path`
    /// apply.
    allow_unsecure: bool,
    /// `::stop-commands::token` is in effect until `::token::`.
    stopped:        Option<String>,
    /// `::echo::on`: commands are also forwarded as lines.
    echo:           bool,
    effects:        Arc<Mutex<CommandEffects>>,
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

    /// The effects so far, shared: readable even if the process left a
    /// straggler holding stdout open and the sink never sees the end of the
    /// stream.
    pub fn effects(&self) -> Arc<Mutex<CommandEffects>> {
        self.effects.clone()
    }

    /// Consume events until the sender side is gone.
    pub async fn run(mut self, mut rx: mpsc::Receiver<StepEvent>) {
        while let Some(event) = rx.recv().await {
            match event {
                StepEvent::Log { stream, line } => self.on_line(stream, line).await,
                other => {
                    let _ = self.out.send(other).await;
                }
            }
        }
    }

    async fn forward(&self, stream: LogStream, line: String) {
        let _ = self.out.send(StepEvent::Log { stream, line }).await;
    }

    async fn on_line(&mut self, stream: LogStream, line: String) {
        if let Some(token) = &self.stopped {
            let resume = line
                .trim()
                .strip_prefix("::")
                .and_then(|rest| rest.strip_suffix("::"));
            if resume == Some(token.as_str()) {
                self.stopped = None;
            }
            // The resume line itself is output — the runner's
            // `TryProcessCommand` writes it before resuming.
            return self.forward(stream, line).await;
        }
        let Some(cmd) = parse(&line) else {
            return self.forward(stream, line).await;
        };
        // The runner's `OmitEcho` set: the value-bearing mask, and the commands
        // that already render into the log.
        let omit_echo = matches!(
            cmd.name.as_str(),
            "add-mask" | "debug" | "notice" | "warning" | "error"
        );
        if self.echo && !omit_echo {
            self.forward(stream, line.clone()).await;
        }
        let name_prop = cmd.properties.get("name").cloned();
        // Apply under the lock, forward after it: the guard must not live across
        // the send.
        let forward: Vec<String> = {
            let mut effects = self.effects.lock().expect("effects are not poisoned");
            match cmd.name.as_str() {
                "add-mask" => {
                    self.masker.register(&cmd.message);
                    tracing::debug!(
                        mask_count = self.masker.len(),
                        "workflow command masked a value"
                    );
                    vec![]
                }
                "save-state" => {
                    if let Some(name) = name_prop {
                        effects.state.insert(name, Value::String(cmd.message));
                    }
                    vec![]
                }
                "set-output" => {
                    if let Some(name) = name_prop {
                        effects.outputs.insert(name, Value::String(cmd.message));
                    }
                    vec![]
                }
                // With the opt-in, the runner's block list still holds: a
                // blocked name gets an error issue and is not applied — no
                // throw, so the step's result is untouched. The message
                // carries the list's spelling, not the command's.
                "set-env" if self.allow_unsecure => {
                    if let Some(name) = name_prop {
                        if let Some(blocked) = set_env_blocked(&name) {
                            vec![format!(
                                "Error: Can't update {blocked} environment variable \
                                 using ::set-env:: command."
                            )]
                        } else {
                            effects.env.insert(name, Value::String(cmd.message));
                            vec![]
                        }
                    } else {
                        vec![]
                    }
                }
                "add-path" if self.allow_unsecure => {
                    effects.path.push(cmd.message);
                    vec![]
                }
                // The runner's `SetEnvCommandExtension` / `AddPathCommandExtension`
                // throw when the opt-in is absent, and `TryProcessCommand` logs
                // the two errors below (the step then fails).
                "set-env" | "add-path" => {
                    tracing::warn!(command = %cmd.name, "unsecure workflow command refused");
                    effects.refused.push(cmd.name.clone());
                    vec![
                        format!("Error: Unable to process command '{line}' successfully."),
                        format!(
                            "Error: The `{}` command is disabled. Please upgrade to using \
                         Environment Files or opt into unsecure command execution by \
                         setting the `ACTIONS_ALLOW_UNSECURE_COMMANDS` environment \
                         variable to `true`. For more information see: \
                         https://github.blog/changelog/2020-10-01-github-actions-deprecating-set-env-and-add-path-commands/",
                            cmd.name
                        ),
                    ]
                }
                "debug" | "add-matcher" | "remove-matcher" | "endgroup" => vec![],
                "group" => vec![format!("▶ {}", cmd.message)],
                level @ ("notice" | "warning" | "error") => {
                    let label = match level {
                        "notice" => "Notice",
                        "warning" => "Warning",
                        _ => "Error",
                    };
                    vec![format!("{label}: {}", cmd.message)]
                }
                "echo" => {
                    self.echo = cmd.message.trim().eq_ignore_ascii_case("on");
                    vec![]
                }
                "stop-commands" => {
                    self.stopped = Some(cmd.message);
                    vec![]
                }
                _ => vec![line],
            }
        };
        for text in forward {
            self.forward(stream, text).await;
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
        assert_eq!(parse("prefix ::warning::x"), None);
    }

    #[test]
    fn leading_whitespace_is_trimmed() {
        let cmd = parse("   ::warning::indented").unwrap();
        assert_eq!(cmd.name, "warning");
        assert_eq!(cmd.message, "indented");
    }

    async fn drive(
        allow_unsecure: bool,
        lines: &[(LogStream, &str)],
    ) -> (Vec<(LogStream, String)>, Arc<Mutex<CommandEffects>>, Masker) {
        let (out_tx, mut out_rx) = mpsc::channel(64);
        let (tx, rx) = mpsc::channel(64);
        let masker = Masker::new();
        let sink = CommandSink::new(out_tx, masker.clone(), allow_unsecure);
        let effects = sink.effects();
        let task = tokio::spawn(sink.run(rx));
        for (stream, line) in lines {
            tx.send(StepEvent::Log {
                stream: *stream,
                line:   (*line).into(),
            })
            .await
            .unwrap();
        }
        drop(tx);
        task.await.unwrap();
        let mut out = Vec::new();
        while let Ok(ev) = out_rx.try_recv() {
            if let StepEvent::Log { stream, line } = ev {
                out.push((stream, line));
            }
        }
        (out, effects, masker)
    }

    /// Commands on stderr are commands too, forwarded on their own stream;
    /// the echo set omits what the runner omits; the resume token is output.
    #[tokio::test]
    async fn both_streams_carry_commands_and_echo_omits_the_runners_set() {
        let (out, _, _) = drive(false, &[
            (LogStream::Stderr, "::error::from stderr"),
            (LogStream::Stderr, "plain stderr"),
            (LogStream::Stdout, "::echo::on"),
            (LogStream::Stdout, "::set-output name=o::v"),
            (LogStream::Stdout, "::add-mask::hidden-value"),
            (LogStream::Stdout, "::debug::quiet"),
            (LogStream::Stdout, "::warning::loud"),
            (LogStream::Stdout, "::group::g"),
            (LogStream::Stdout, "::echo::off"),
            (LogStream::Stdout, "::set-output name=p::w"),
            (LogStream::Stdout, "::stop-commands::tok"),
            (LogStream::Stdout, "::error::inert"),
            (LogStream::Stdout, "::tok::"),
            (LogStream::Stdout, "::error::live"),
        ])
        .await;
        assert_eq!(out, vec![
            (LogStream::Stderr, "Error: from stderr".to_string()),
            (LogStream::Stderr, "plain stderr".to_string()),
            (LogStream::Stdout, "::set-output name=o::v".to_string()),
            (LogStream::Stdout, "Warning: loud".to_string()),
            (LogStream::Stdout, "::group::g".to_string()),
            (LogStream::Stdout, "▶ g".to_string()),
            // `::echo::off` is still echoed: echo is on when it is checked.
            (LogStream::Stdout, "::echo::off".to_string()),
            (LogStream::Stdout, "::error::inert".to_string()),
            (LogStream::Stdout, "::tok::".to_string()),
            (LogStream::Stdout, "Error: live".to_string()),
        ]);
    }

    /// Even with the unsecure opt-in, the runner's block list refuses
    /// `NODE_OPTIONS` in any casing: the error names the list's spelling, the
    /// variable is not applied, and — the runner `AddIssue`s rather than
    /// throwing — nothing lands in `refused`, so the step's result stands.
    #[tokio::test]
    async fn set_env_blocks_node_options_even_when_unsecure_commands_are_allowed() {
        let (out, effects, _) = drive(true, &[
            (
                LogStream::Stdout,
                "::set-env name=NODE_OPTIONS::--require=/e.js",
            ),
            (
                LogStream::Stdout,
                "::set-env name=node_options::--require=/e.js",
            ),
            (LogStream::Stdout, "::set-env name=GOOD::applies"),
        ])
        .await;
        let blocked =
            "Error: Can't update NODE_OPTIONS environment variable using ::set-env:: command.";
        assert_eq!(out, vec![
            (LogStream::Stdout, blocked.to_string()),
            (LogStream::Stdout, blocked.to_string()),
        ]);
        let effects = effects.lock().unwrap();
        assert!(!effects.env.contains_key("NODE_OPTIONS"));
        assert!(!effects.env.contains_key("node_options"));
        assert_eq!(effects.env["GOOD"], "applies");
        assert!(effects.refused.is_empty(), "an annotation, not a refusal");
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
                line:   line.into(),
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
        assert_eq!(lines, vec![
            "plain",
            "Error: Unable to process command '::set-env name=E::1' successfully.",
            "Error: The `set-env` command is disabled. Please upgrade to using \
                 Environment Files or opt into unsecure command execution by setting \
                 the `ACTIONS_ALLOW_UNSECURE_COMMANDS` environment variable to `true`. \
                 For more information see: \
                 https://github.blog/changelog/2020-10-01-github-actions-deprecating-set-env-and-add-path-commands/",
            "▶ title",
            "Error: bad",
            "::save-state name=ignored::x",
            "::tok::",
            "after",
        ]);
        let effects = effects.lock().unwrap();
        assert_eq!(effects.state["k"], "v");
        assert_eq!(effects.outputs["o"], "out");
        assert!(effects.env.is_empty(), "set-env is disabled by default");
        assert_eq!(masker.mask("got hunter2-long"), "got ***");
    }
}
