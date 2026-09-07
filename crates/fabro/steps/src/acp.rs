//! A minimal Agent Client Protocol client over a step's process handle.
//!
//! The agent is a subprocess in the scope environment speaking ACP 0.11
//! JSON-RPC over stdio: `initialize`, `session/new`, `session/prompt`, with
//! `session/update` notifications streaming the agent's text back and
//! `session/request_permission` requests answered with the most permissive
//! option, as Fabro's client does. Written against the wire protocol rather
//! than the `agent-client-protocol` crate: the subset a turn needs is small,
//! and the transport is Petri's [`ProcessHandle`] rather than a socket the
//! crate owns.
//!
//! Tool hooks are best effort here. ACP exposes one tool boundary Petri can
//! act on: `session/request_permission`, which the agent sends only for
//! calls it chooses to ask about. A `pre_tool_use` hook runs there, and a
//! block answers the request with the rejecting option, so the effect does
//! not happen for that call. Tool calls the agent never asks about, and
//! every post-tool result, cross only as `session/update` notifications the
//! agent may or may not send, so `post_tool_use` and `post_tool_use_failure`
//! hooks cannot run against real results. The client says so, once per
//! configured hook, on the step's progress channel and in a warning line,
//! and never records an unenforceable block as enforced.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use execution::hooks::{HookDecision, HookPoint, HookRequest, HookService};
use executor::{LineStream, ProcessHandle, ProcessSpec, Sig, StdinMode, StdinWriter};
use frontend_fabro::hooks::HookEvent;
use ir::{Attempt, Control, FiringId, LogStream, StepEvent, Value};
use runtime::driver::FiringView;
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Answer, Steer};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;
use tokio::time;

use crate::hooks::{ToolPayload, WARNING_EVENT, report_event, warning_event};

/// The backend name in hook warnings.
pub const BACKEND: &str = "acp";

/// The hook service bound to one ACP node, and what it has already warned
/// about.
pub struct AcpHooks {
    service: Arc<dyn HookService>,
    view:    Arc<FiringView>,
    node:    SmolStr,
    firing:  FiringId,
    attempt: Attempt,
    /// Configured tool hooks by event, so a boundary gap is reported once per
    /// hook.
    pre:     Vec<String>,
    post:    Vec<String>,
    warned:  Mutex<BTreeSet<String>>,
}

impl AcpHooks {
    pub fn new(
        service: Arc<dyn HookService>,
        view: Arc<FiringView>,
        node: SmolStr,
        firing: FiringId,
        attempt: Attempt,
        pre: Vec<String>,
        post: Vec<String>,
    ) -> Self {
        Self {
            service,
            view,
            node,
            firing,
            attempt,
            pre,
            post,
            warned: Mutex::new(BTreeSet::new()),
        }
    }

    /// Whether any tool hook is configured.
    pub fn has_tool_hooks(&self) -> bool {
        !self.pre.is_empty() || !self.post.is_empty()
    }

    /// The warnings to emit before the agent starts: what this backend
    /// cannot enforce for each configured tool hook.
    pub fn known_gaps(&self) -> Vec<StepEvent> {
        let mut out = Vec::new();
        for hook in &self.pre {
            out.push(warning_event(
                &self.node,
                self.firing,
                self.attempt,
                BACKEND,
                hook,
                HookEvent::PreToolUse,
                "session/request_permission",
                "the ACP agent decides which tool calls ask for permission; this hook runs only \
                 for those, and a tool call the agent does not ask about is not intercepted",
            ));
        }
        for hook in &self.post {
            out.push(warning_event(
                &self.node,
                self.firing,
                self.attempt,
                BACKEND,
                hook,
                HookEvent::PostToolUse,
                "none",
                "ACP exposes no post-tool result boundary; this hook does not run for this node",
            ));
        }
        out
    }

    /// Ask the `pre_tool_use` hooks about a permission request.
    /// `Some(reason)` blocks.
    async fn pre_tool(&self, params: &Value, logs: &mpsc::Sender<StepEvent>) -> Option<String> {
        let call = params.get("toolCall").cloned().unwrap_or(Value::Null);
        let tool_name = call
            .get("title")
            .or_else(|| call.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_owned();
        let payload = ToolPayload {
            tool_name,
            tool_call_id: call
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            tool_input: call.get("rawInput").cloned(),
            ..ToolPayload::default()
        };
        let report = self
            .service
            .run(HookRequest {
                point:   HookPoint::BeforeToolUse,
                view:    self.view.clone(),
                outcome: None,
                routes:  Vec::new(),
                payload: serde_json::to_value(&payload).unwrap_or(Value::Null),
            })
            .await;
        if !report.is_silent() {
            let _ = logs
                .send(report_event(
                    &self.node,
                    self.firing,
                    self.attempt,
                    HookEvent::PreToolUse,
                    &report,
                ))
                .await;
        }
        match report.decision {
            HookDecision::Block { reason } => Some(reason),
            _ => None,
        }
    }

    /// A tool call the agent reported without asking permission: the
    /// configured `pre_tool_use` hooks could not run for it. Warn once per
    /// hook and tool.
    async fn unintercepted(&self, tool: &str, logs: &mpsc::Sender<StepEvent>) {
        for hook in &self.pre {
            let key = format!("{hook}:{tool}");
            let first = self
                .warned
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key);
            if !first {
                continue;
            }
            let _ = logs
                .send(warning_event(
                    &self.node,
                    self.firing,
                    self.attempt,
                    BACKEND,
                    hook,
                    HookEvent::PreToolUse,
                    "session/update",
                    &format!(
                        "the agent ran `{tool}` without a permission request; the hook did not \
                         run for it"
                    ),
                ))
                .await;
        }
    }
}

/// How an agent is launched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCommand {
    pub program: String,
    pub args:    Vec<String>,
    pub env:     BTreeMap<String, String>,
}

impl AgentCommand {
    /// `acp.command`: one shell-quoted command line.
    pub fn from_command_line(line: &str) -> Result<Self, String> {
        let words = shlex::split(line.trim()).ok_or_else(|| "unbalanced quotes".to_string())?;
        let mut words = words.into_iter();
        let program = words
            .next()
            .ok_or_else(|| "the command line is empty".to_string())?;
        Ok(Self {
            program,
            args: words.collect(),
            env: BTreeMap::new(),
        })
    }

    /// `acp.config`: the JSON stdio server shape `{command, args, env}`.
    pub fn from_config(config: &Value) -> Result<Self, String> {
        let config: StdioConfig = serde_json::from_value(config.clone())
            .map_err(|error| format!("invalid `acp.config`: {error}"))?;
        if config.command.is_empty() {
            return Err("`acp.config` needs a non-empty `command`".into());
        }
        let env = match config.env {
            None => BTreeMap::new(),
            Some(ConfigEnv::Map(env)) => env,
            Some(ConfigEnv::Pairs(pairs)) => pairs
                .into_iter()
                .map(|pair| (pair.name, pair.value))
                .collect(),
        };
        Ok(Self {
            program: config.command,
            args: config.args,
            env,
        })
    }

    pub fn spec(&self) -> ProcessSpec {
        let args: Vec<&str> = self.args.iter().map(String::as_str).collect();
        let env: BTreeMap<SmolStr, SmolStr> = self
            .env
            .iter()
            .map(|(k, v)| (SmolStr::new(k), SmolStr::new(v)))
            .collect();
        ProcessSpec::new(&self.program, &args)
            .with_env(env)
            .with_stdin(StdinMode::Piped)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StdioConfig {
    command: String,
    #[serde(default)]
    args:    Vec<String>,
    #[serde(default)]
    env:     Option<ConfigEnv>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ConfigEnv {
    Map(BTreeMap<String, String>),
    Pairs(Vec<EnvPair>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvPair {
    name:  String,
    value: String,
}

/// Why a turn did not complete.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AcpError {
    #[error("the agent process exited before the protocol completed{0}")]
    ProcessExited(String),
    #[error("the agent sent something that is not JSON-RPC: {0}")]
    Protocol(String),
    #[error("the agent answered `{method}` with an error: {message}")]
    Rejected { method: String, message: String },
    #[error("the turn ended with stop reason `{0}`")]
    StopReason(String),
    #[error("cancelled")]
    Cancelled,
}

/// What a completed turn produced.
#[derive(Clone, Debug, Default)]
pub struct Turn {
    pub text: String,
}

/// One live connection to an agent process.
pub struct Client {
    handle:     Box<dyn ProcessHandle>,
    stdin:      StdinWriter,
    lines:      LineStream,
    logs:       mpsc::Sender<StepEvent>,
    next_id:    u64,
    session_id: Option<String>,
    exited:     bool,
    hooks:      Option<Arc<AcpHooks>>,
}

/// What arrived from the agent, sorted by JSON-RPC shape.
enum Incoming {
    Response {
        id:     Value,
        result: Option<Value>,
        error:  Option<Value>,
    },
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id:     Value,
        method: String,
        params: Value,
    },
}

impl Client {
    /// Spawn the agent and take both ends of its stdio.
    pub async fn spawn(
        env: &dyn executor::ExecEnv,
        command: &AgentCommand,
        logs: mpsc::Sender<StepEvent>,
    ) -> Result<Self, AcpError> {
        let mut handle = env.spawn(command.spec()).await.map_err(|e| {
            AcpError::ProcessExited(format!(": could not start `{}`: {e}", command.program))
        })?;
        let stdin = handle
            .stdin()
            .ok_or_else(|| AcpError::Protocol("the executor gave the agent no stdin".into()))?;
        let lines = handle
            .lines()
            .ok_or_else(|| AcpError::Protocol("the executor gave the agent no stdout".into()))?;
        Ok(Self {
            handle,
            stdin,
            lines,
            logs,
            next_id: 1,
            session_id: None,
            exited: false,
            hooks: None,
        })
    }

    /// Bind the tool hooks this node configured. Warns about the boundaries
    /// this backend lacks before the first prompt.
    pub async fn with_hooks(&mut self, hooks: Arc<AcpHooks>) {
        for warning in hooks.known_gaps() {
            if let StepEvent::Custom(value) = &warning
                && value["kind"] == WARNING_EVENT
            {
                let _ = self
                    .logs
                    .send(StepEvent::Log {
                        stream: LogStream::Stderr,
                        line:   format!(
                            "hook warning ({}): hook `{}` on `{}`: {}",
                            BACKEND,
                            value["hook"].as_str().unwrap_or("?"),
                            value["event"].as_str().unwrap_or("?"),
                            value["message"].as_str().unwrap_or("")
                        ),
                    })
                    .await;
            }
            let _ = self.logs.send(warning).await;
        }
        self.hooks = Some(hooks);
    }

    async fn send(&mut self, message: Value) -> Result<(), AcpError> {
        let mut bytes =
            serde_json::to_vec(&message).map_err(|e| AcpError::Protocol(e.to_string()))?;
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .map_err(|e| AcpError::ProcessExited(format!(": stdin closed: {e}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| AcpError::ProcessExited(format!(": stdin closed: {e}")))
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<u64, AcpError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        Ok(id)
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), AcpError> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    /// The next JSON-RPC message from stdout. Stderr lines are forwarded as
    /// step logs; stdout lines that are not JSON are forwarded too, since an
    /// agent that prints prose over its protocol stream is still worth
    /// hearing from.
    async fn receive(&mut self) -> Result<Incoming, AcpError> {
        loop {
            let Some(line) = self.lines.recv().await else {
                self.exited = true;
                return Err(AcpError::ProcessExited(String::new()));
            };
            if line.stream == LogStream::Stderr {
                let _ = self
                    .logs
                    .send(StepEvent::Log {
                        stream: LogStream::Stderr,
                        line:   line.line,
                    })
                    .await;
                continue;
            }
            let text = line.line;
            let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
                let _ = self
                    .logs
                    .send(StepEvent::Log {
                        stream: LogStream::Stdout,
                        line:   text,
                    })
                    .await;
                continue;
            };
            let method = value
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_string);
            let id = value.get("id").cloned();
            return Ok(match (method, id) {
                (Some(method), Some(id)) => Incoming::Request {
                    id,
                    method,
                    params: value.get("params").cloned().unwrap_or(Value::Null),
                },
                (Some(method), None) => Incoming::Notification {
                    method,
                    params: value.get("params").cloned().unwrap_or(Value::Null),
                },
                (None, Some(id)) => Incoming::Response {
                    id,
                    result: value.get("result").cloned(),
                    error: value.get("error").cloned(),
                },
                (None, None) => {
                    return Err(AcpError::Protocol(format!(
                        "neither a request nor a response: {text}"
                    )));
                }
            });
        }
    }

    /// Answer a permission request with the most permissive option offered,
    /// as Fabro's client does: `allow_always`, else `allow_once`, else any
    /// option that is not a rejection.
    async fn answer_permission(&mut self, id: Value, params: &Value) -> Result<(), AcpError> {
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let pick = |kind: &str| {
            options
                .iter()
                .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
        };
        // The one boundary ACP exposes: a blocking `pre_tool_use` hook
        // answers with the rejecting option, so the call does not happen.
        if let Some(hooks) = self.hooks.clone()
            && let Some(reason) = hooks.pre_tool(params, &self.logs).await
        {
            let rejected = pick("reject_once").or_else(|| pick("reject_always"));
            let outcome = match rejected.and_then(|o| o.get("optionId")) {
                Some(option) => json!({ "outcome": "selected", "optionId": option }),
                None => json!({ "outcome": "cancelled" }),
            };
            let _ = self
                .logs
                .send(StepEvent::Log {
                    stream: LogStream::Stderr,
                    line:   format!("pre_tool_use hook blocked a permission request: {reason}"),
                })
                .await;
            return self
                .send(json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": outcome } }))
                .await;
        }
        let chosen = pick("allow_always")
            .or_else(|| pick("allow_once"))
            .or_else(|| {
                options.iter().find(|o| {
                    !matches!(
                        o.get("kind").and_then(Value::as_str),
                        Some("reject_once" | "reject_always")
                    )
                })
            });
        let outcome = match chosen.and_then(|o| o.get("optionId")) {
            Some(option) => json!({ "outcome": "selected", "optionId": option }),
            None => json!({ "outcome": "cancelled" }),
        };
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": outcome } }))
            .await
    }

    /// Wait for the response to request `id`, serving what arrives meanwhile.
    async fn response(
        &mut self,
        id: u64,
        method: &str,
        turn: &mut Turn,
    ) -> Result<Value, AcpError> {
        loop {
            match self.receive().await? {
                Incoming::Response {
                    id: got,
                    result,
                    error,
                } if got == json!(id) => {
                    if let Some(error) = error {
                        return Err(AcpError::Rejected {
                            method:  method.to_string(),
                            message: error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown error")
                                .to_string(),
                        });
                    }
                    return Ok(result.unwrap_or(Value::Null));
                }
                Incoming::Response { .. } => {}
                Incoming::Notification { method, params } => {
                    self.on_notification(&method, params, turn).await;
                }
                Incoming::Request { id, method, params } => {
                    self.on_request(id, &method, &params).await?;
                }
            }
        }
    }

    async fn on_notification(&mut self, method: &str, params: Value, turn: &mut Turn) {
        if method != "session/update" {
            let _ = self
                .logs
                .send(StepEvent::Custom(
                    json!({ "acp": { "method": method, "params": params } }),
                ))
                .await;
            return;
        }
        let update = params.get("update").cloned().unwrap_or(Value::Null);
        let kind = update.get("sessionUpdate").and_then(Value::as_str);
        if kind == Some("agent_message_chunk")
            && let Some(text) = update.pointer("/content/text").and_then(Value::as_str)
            && update.pointer("/content/type").and_then(Value::as_str) == Some("text")
        {
            turn.text.push_str(text);
            return;
        }
        // A tool call the agent reports without a permission request ran
        // past every configured pre-tool hook: say so.
        if kind == Some("tool_call")
            && let Some(hooks) = self.hooks.clone()
        {
            let tool = update
                .get("title")
                .or_else(|| update.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("tool");
            hooks.unintercepted(tool, &self.logs).await;
        }
        // Tool calls, thoughts, plans: observers see them as agent activity.
        let _ = self
            .logs
            .send(StepEvent::Custom(json!({ "acp": update })))
            .await;
    }

    async fn on_request(
        &mut self,
        id: Value,
        method: &str,
        params: &Value,
    ) -> Result<(), AcpError> {
        match method {
            "session/request_permission" => self.answer_permission(id, params).await,
            other => {
                self.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {other}") },
                }))
                .await
            }
        }
    }

    /// `initialize` then `session/new` in `cwd`.
    pub async fn open_session(&mut self, cwd: &str) -> Result<(), AcpError> {
        let mut scratch = Turn::default();
        let id = self
            .request("initialize", json!({
                "protocolVersion": 1,
                "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false } },
            }))
            .await?;
        self.response(id, "initialize", &mut scratch).await?;
        let id = self
            .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await?;
        let result = self.response(id, "session/new", &mut scratch).await?;
        let session = result
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| AcpError::Protocol("`session/new` returned no sessionId".into()))?;
        self.session_id = Some(session.to_string());
        Ok(())
    }

    /// One prompt turn. `control` delivers steering (`Deliver` text is queued
    /// and sent as a follow-up prompt when the turn ends) and cancellation
    /// (`session/cancel`, then the process is stopped).
    pub async fn prompt(
        &mut self,
        text: &str,
        control: &mut mpsc::Receiver<Control>,
        grace: Duration,
    ) -> Result<Turn, AcpError> {
        let session = self
            .session_id
            .clone()
            .ok_or_else(|| AcpError::Protocol("no session".into()))?;
        let mut turn = Turn::default();
        let mut pending = VecDeque::from([text.to_string()]);
        while let Some(prompt) = pending.pop_front() {
            let id = self
                .request(
                    "session/prompt",
                    json!({
                        "sessionId": session,
                        "prompt": [{ "type": "text", "text": prompt }],
                    }),
                )
                .await?;
            let stop_reason = loop {
                tokio::select! {
                    message = self.receive() => match message? {
                        Incoming::Response { id: got, result, error } if got == json!(id) => {
                            if let Some(error) = error {
                                return Err(AcpError::Rejected {
                                    method: "session/prompt".into(),
                                    message: error.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_string(),
                                });
                            }
                            break result
                                .as_ref()
                                .and_then(|r| r.get("stopReason"))
                                .and_then(Value::as_str)
                                .unwrap_or("end_turn")
                                .to_string();
                        }
                        Incoming::Response { .. } => {}
                        Incoming::Notification { method, params } => self.on_notification(&method, params, &mut turn).await,
                        Incoming::Request { id, method, params } => self.on_request(id, &method, &params).await?,
                    },
                    ctl = control.recv() => {
                        if let Some(Control::Deliver(value)) = ctl {
                            if let Some(text) = steer_text(&value) {
                                pending.push_back(text);
                            }
                        } else {
                            self.cancel(&session, grace).await;
                            return Err(AcpError::Cancelled);
                        }
                    }
                }
            };
            match stop_reason.as_str() {
                "end_turn" | "refusal" => {}
                "cancelled" => return Err(AcpError::Cancelled),
                other => return Err(AcpError::StopReason(other.to_string())),
            }
        }
        Ok(turn)
    }

    /// `session/cancel`, a grace period for the agent to wind down, then the
    /// process is stopped: TERM, grace, KILL.
    async fn cancel(&mut self, session: &str, grace: Duration) {
        let _ = self
            .notify("session/cancel", json!({ "sessionId": session }))
            .await;
        let deadline = time::sleep(Duration::from_millis(500));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                message = self.receive() => if message.is_err() { break },
                () = &mut deadline => break,
            }
        }
        self.terminate(grace).await;
    }

    /// Stop the process: TERM, grace, KILL. Idempotent once it has exited.
    pub async fn terminate(&mut self, grace: Duration) {
        if self.exited {
            return;
        }
        let _ = self.handle.signal(Sig::Term).await;
        if time::timeout(grace, self.handle.wait()).await.is_err() {
            let _ = self.handle.signal(Sig::Kill).await;
            let _ = self.handle.wait().await;
        }
        self.exited = true;
    }
}

/// The text a delivered steering value carries: a string, or `{ "text": … }`.
/// The guidance a delivered value carries: a core [`Steer`], or the older
/// answer-shaped spelling (a bare string, or `text`/`choice` fields).
fn steer_text(value: &Value) -> Option<String> {
    if let Some(steer) = Steer::from_value(value) {
        return Some(steer.text);
    }
    let answer = Answer::from_value(value)?;
    answer
        .text
        .and_then(|text| text.as_str().map(str::to_string))
        .or(answer.choice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_split_like_a_shell() {
        let command =
            AgentCommand::from_command_line("python3 -c 'print(1)' --flag").expect("splits");
        assert_eq!(command.program, "python3");
        assert_eq!(command.args, ["-c", "print(1)", "--flag"]);
        assert!(AgentCommand::from_command_line("").is_err());
        assert!(AgentCommand::from_command_line("a 'b").is_err());
    }

    #[test]
    fn configs_carry_args_and_env() {
        let command = AgentCommand::from_config(&json!({
            "command": "agent",
            "args": ["--acp"],
            "env": [{ "name": "K", "value": "v" }],
        }))
        .expect("parses");
        assert_eq!(command.args, ["--acp"]);
        assert_eq!(command.env.get("K").map(String::as_str), Some("v"));
        assert!(AgentCommand::from_config(&json!({ "args": [] })).is_err());
        assert!(AgentCommand::from_config(&json!({ "command": ["agent"], "args": [] })).is_err());
        assert!(
            AgentCommand::from_config(&json!({ "command": "agent", "args": "--acp" })).is_err()
        );
        assert!(
            AgentCommand::from_config(&json!({ "command": "agent", "unknown": true })).is_err()
        );
    }
}
