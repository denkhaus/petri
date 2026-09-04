//! `fabro/command`: Fabro's command node. A bash (or python) script in the
//! scope environment, its combined output captured, an optional stdin from
//! the run context, and — with `output_schema="routing"` — the last JSON
//! object of the output read as a routing directive.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use executor::{ProcessSpec, StdinMode};
use frontend_fabro::Policy;
use frontend_fabro::kinds::{COMMAND_KIND, StageOutcome};
use ir::{FailureClass, LogStream, Outcome, StepEvent, StepKindId, Value};
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Ending, Step, StepCtx, StepFailure, ladder};
use tokio::io::AsyncWriteExt as _;
use tokio::time;

use crate::directive;
use crate::outcome::Stage;

/// The step kind id.
pub const KIND: StepKindId = COMMAND_KIND;

/// The process could not be started.
pub const SPAWN_CLASS: FailureClass = FailureClass::new_static("spawn_failed");

/// How much captured output a stage keeps in its output and context. Fabro
/// offloads larger output to a blob; a later host capability does the same
/// here, and until then the tail is kept with a truncation marker.
pub const OUTPUT_CAP: usize = 64 * 1024;

const TRUNCATED: &str = "\n… [output truncated]\n";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandConfig {
    pub label:         String,
    pub node:          String,
    #[serde(default)]
    pub goal:          String,
    pub script:        String,
    #[serde(default = "default_language")]
    pub language:      String,
    #[serde(default)]
    pub stdin:         Value,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub on_failure:    Option<Policy>,
    #[serde(default)]
    pub timeout_ms:    Option<u64>,
    /// The run context at spawn, resolved by the engine.
    #[serde(default)]
    pub kv:            Value,
}

fn default_language() -> String {
    "shell".into()
}

pub struct CommandStep;

#[async_trait::async_trait]
impl Step for CommandStep {
    const NAME: &'static str = "fabro/command";
    type Config = CommandConfig;

    async fn run(&self, config: CommandConfig, ctx: StepCtx) -> Outcome {
        match execute(config, ctx).await {
            Ok(outcome) => outcome,
            Err(failure) => failure.into(),
        }
    }
}

/// The text a context value feeds to stdin: strings as they are, everything
/// else as JSON.
fn stdin_text(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone().into_bytes()),
        other => Some(other.to_string().into_bytes()),
    }
}

#[derive(Default)]
struct OutputTail {
    bytes:     VecDeque<u8>,
    truncated: bool,
}

impl OutputTail {
    fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= OUTPUT_CAP {
            self.bytes.clear();
            self.bytes.extend(&bytes[bytes.len() - OUTPUT_CAP..]);
            self.truncated = true;
            return;
        }
        self.bytes.extend(bytes);
        if self.bytes.len() > OUTPUT_CAP {
            self.truncated = true;
            self.bytes.drain(..self.bytes.len() - OUTPUT_CAP);
        }
    }

    fn push_line(&mut self, line: &str) {
        self.push(line.as_bytes());
        self.push(b"\n");
    }

    fn render(&self) -> String {
        let bytes: Vec<u8> = self.bytes.iter().copied().collect();
        let start = bytes
            .iter()
            .position(|byte| byte & 0b1100_0000 != 0b1000_0000)
            .unwrap_or(bytes.len());
        let tail = String::from_utf8(bytes[start..].to_vec())
            .expect("the captured output was valid UTF-8 before its prefix was truncated");
        if self.truncated || start > 0 {
            format!("{TRUNCATED}{tail}")
        } else {
            tail
        }
    }
}

async fn execute(config: CommandConfig, mut ctx: StepCtx) -> Result<Outcome, StepFailure> {
    let (program, args) = match config.language.as_str() {
        "python" => ("python3", vec!["-c".to_string(), config.script.clone()]),
        _ => ("bash", vec![
            "-c".to_string(),
            format!("exec 2>&1\n{}", config.script),
        ]),
    };
    let stdin = stdin_text(&config.stdin);
    let spec = ProcessSpec::new(
        program,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .with_stdin(if stdin.is_some() {
        StdinMode::Piped
    } else {
        StdinMode::Null
    });
    let mut handle = ctx.env.spawn(spec).await.map_err(|e| StepFailure {
        class:   SPAWN_CLASS,
        message: e.to_string(),
    })?;
    if let (Some(bytes), Some(mut writer)) = (stdin, handle.stdin()) {
        tokio::spawn(async move {
            let _ = writer.write_all(&bytes).await;
            let _ = writer.shutdown().await;
        });
    }
    let captured = Arc::new(Mutex::new(OutputTail::default()));
    let mut drain = None;
    if let Some(mut lines) = handle.lines() {
        let logs = ctx.logs.clone();
        let captured = captured.clone();
        drain = Some(tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                captured
                    .lock()
                    .expect("the output tail lock is not poisoned")
                    .push_line(&line.line);
                let _ = logs
                    .send(StepEvent::Log {
                        stream: LogStream::Stdout,
                        line:   line.line,
                    })
                    .await;
            }
        }));
    }
    let grace = ctx.env.grace();
    let ending = ladder(&mut *handle, &mut ctx.control, grace).await;
    if let Some(mut drain) = drain
        && time::timeout(time::Duration::from_secs(5), &mut drain)
            .await
            .is_err()
    {
        drain.abort();
        let _ = drain.await;
    }
    let output = captured
        .lock()
        .expect("the output tail lock is not poisoned")
        .render();

    let mut stage = match ending {
        Ending::Natural(status) if status.is_success() => {
            Stage::new(StageOutcome::Succeeded, config.on_failure)
        }
        Ending::Natural(status) => {
            let (reason, class) = match (status.code, status.signal) {
                (_, Some(signal)) => (
                    format!("script was killed by signal {signal}"),
                    FailureClass::signal(signal),
                ),
                (code, None) => {
                    let code = code.unwrap_or(-1);
                    (
                        format!("script exited with status {code}"),
                        FailureClass::exit_status(code),
                    )
                }
            };
            let mut reason = reason;
            if !output.trim().is_empty() {
                let tail: String = output
                    .chars()
                    .rev()
                    .take(2000)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                reason.push('\n');
                reason.push_str(tail.trim_end());
            }
            Stage::failed(reason, class.as_str(), config.on_failure)
        }
        Ending::Signalled { .. } => {
            let mut out = serde_json::Map::new();
            out.insert("stdout".into(), json!(output));
            return Ok(Outcome::new(ir::Status::Cancelled, Value::Object(out)));
        }
    };
    if let Ending::Natural(status) = &ending {
        stage
            .output
            .insert("exit_status".into(), json!(status.code));
    }
    stage.output.insert("stdout".into(), json!(output));
    stage
        .context_updates
        .insert(SmolStr::new("command.output"), json!(output));

    if stage.outcome == StageOutcome::Succeeded
        && config
            .output_schema
            .as_ref()
            .is_some_and(|s| s == "routing")
    {
        match directive::parse(&output) {
            Ok(directive) => {
                directive.apply_to(&mut stage);
            }
            Err(error) => {
                stage = Stage::failed(
                    format!("the script's output is not a routing directive: {error}"),
                    "bad_output",
                    config.on_failure,
                );
                stage.output.insert("stdout".into(), json!(output));
            }
        }
    }
    Ok(stage.into_outcome(&config.node))
}
