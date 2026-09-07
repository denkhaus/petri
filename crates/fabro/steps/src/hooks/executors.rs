//! The four hook executors: a command in the sandbox or on the host, an
//! HTTP POST, one model turn, an agent with the coding tools.
//!
//! Each returns [`Executed`]: a decision, a fail-open note (HTTP, prompt and
//! agent errors and timeouts), or a reason the hook could not run at all (a
//! sandbox hook with no environment, a model hook with no client). Command
//! hooks never fail open: Fabro turns their exit code into a decision, and a
//! timeout is exit code -1, a block.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use executor::{ExecEnv, OutputMode, ProcessSpec, Sig, StdinMode};
use frontend_fabro::hooks::{
    DEFAULT_MAX_TOOL_ROUNDS, DEFAULT_MODEL, HookDefinition, HookKind, TlsMode,
};
use lithos_llm::types::{Message, Request, ResponseFormat, Role};
use pebble_coding_agent::events::PermissionLevel;
use pebble_coding_agent::{CodingAgent, ShutdownReason};
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt as _;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::{Context, Decision};
use crate::pebble::PebbleClient;
use crate::pebble::environment::PebbleEnvironment;

/// Fabro's prompt for its hook evaluator.
const EVALUATOR_SYSTEM: &str = "You are a hook evaluator for a workflow engine. Given context \
                                about a workflow event, evaluate the condition.";

/// The response a prompt or agent hook returns: `{"ok": bool, "reason"}`.
#[derive(Debug, Deserialize)]
struct Verdict {
    ok:     bool,
    #[serde(default)]
    reason: Option<String>,
}

/// How one hook ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Executed {
    Decided(Decision),
    FailedOpen(String),
    Unsupported(String),
}

/// One `reqwest` client per TLS mode, built on first use.
#[derive(Default)]
pub struct HttpClients {
    clients: Mutex<BTreeMap<u8, reqwest::Client>>,
}

impl HttpClients {
    fn client(&self, tls: TlsMode) -> Result<reqwest::Client, reqwest::Error> {
        let key = match tls {
            TlsMode::Verify => 0,
            TlsMode::NoVerify => 1,
            TlsMode::Off => 2,
        };
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(tls != TlsMode::Verify)
            .build()?;
        clients.insert(key, client.clone());
        Ok(client)
    }
}

/// Run one hook.
pub async fn execute(
    hook: &HookDefinition,
    context: &Context,
    env: Option<&Arc<dyn ExecEnv>>,
    client: Option<&PebbleClient>,
    http: &HttpClients,
) -> Executed {
    match &hook.kind {
        HookKind::Command { command } => command_hook(hook, command, context, env).await,
        HookKind::Http { url, headers, tls } => {
            http_hook(hook, url, headers, *tls, context, http).await
        }
        HookKind::Prompt { prompt, model } => {
            prompt_hook(hook, prompt, model.as_deref(), context, client).await
        }
        HookKind::Agent {
            prompt,
            model,
            max_tool_rounds,
        } => {
            agent_hook(
                hook,
                prompt,
                model.as_deref(),
                max_tool_rounds.unwrap_or(DEFAULT_MAX_TOOL_ROUNDS),
                context,
                env,
                client,
            )
            .await
        }
    }
}

/// Fabro's exit-code rule.
fn parse_decision(code: i32, stdout: &str) -> Decision {
    let json = serde_json::from_str::<Decision>(stdout.trim()).ok();
    match (code, json) {
        (0, Some(decision)) => decision,
        (0, None) => Decision::Proceed,
        (2, Some(decision)) => decision,
        (code, _) => Decision::Block {
            reason: Some(format!("hook exited with code {code}")),
        },
    }
}

fn env_vars(context: &Context) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    vars.insert("FABRO_EVENT".into(), context.event.as_str().to_owned());
    vars.insert("FABRO_RUN_ID".into(), context.run_id.clone());
    vars.insert("FABRO_WORKFLOW".into(), context.workflow_name.clone());
    if let Some(node) = &context.node_id {
        vars.insert("FABRO_NODE_ID".into(), node.clone());
    }
    vars
}

async fn command_hook(
    hook: &HookDefinition,
    command: &str,
    context: &Context,
    env: Option<&Arc<dyn ExecEnv>>,
) -> Executed {
    let payload = serde_json::to_vec(context).unwrap_or_default();
    let vars = env_vars(context);
    if hook.runs_in_sandbox() {
        let Some(env) = env else {
            return Executed::Unsupported(
                "the hook runs in the sandbox, and no sandbox environment is available at this \
                 point (set `sandbox = false` to run it on the host)"
                    .into(),
            );
        };
        return sandbox_command(hook, command, &payload, vars, env.as_ref()).await;
    }
    host_command(hook, command, &payload, vars, env.map(Arc::as_ref)).await
}

/// In the sandbox: `bash -c`, the context on stdin and in a file the command
/// removes when it ends, so `FABRO_HOOK_CONTEXT` works as Fabro's scripts
/// expect and the workspace is left as it was.
async fn sandbox_command(
    hook: &HookDefinition,
    command: &str,
    payload: &[u8],
    mut vars: BTreeMap<String, String>,
    env: &dyn ExecEnv,
) -> Executed {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let relative = format!(".fabro-hook-context-{nanos}.json");
    let context_path = if env
        .write_file(std::path::Path::new(&relative), payload)
        .await
        .is_ok()
    {
        let absolute = format!("{}/{relative}", env.workspace_path().trim_end_matches('/'));
        vars.insert("FABRO_HOOK_CONTEXT".into(), absolute.clone());
        Some(absolute)
    } else {
        None
    };
    let script = match &context_path {
        Some(path) => format!(
            "{command}\n__fabro_status=$?\nrm -f -- '{}'\nexit $__fabro_status",
            path.replace('\'', "'\\''")
        ),
        None => command.to_owned(),
    };
    let spec = ProcessSpec::new("bash", &["-c", &script])
        .with_output(OutputMode::Bytes)
        .with_stdin(StdinMode::Piped)
        .with_env(
            vars.iter()
                .map(|(k, v)| (k.as_str().into(), v.as_str().into()))
                .collect(),
        );
    let mut handle = match env.spawn(spec).await {
        Ok(handle) => handle,
        Err(error) => {
            return Executed::Decided(Decision::Block {
                reason: Some(format!("sandbox exec failed: {error}")),
            });
        }
    };
    if let Some(mut stdin) = handle.stdin() {
        let bytes = payload.to_vec();
        tokio::spawn(async move {
            let _ = stdin.write_all(&bytes).await;
            let _ = stdin.shutdown().await;
        });
    }
    let Some(mut bytes) = handle.bytes() else {
        let _ = handle.signal(Sig::Kill).await;
        let _ = handle.wait().await;
        return Executed::Decided(Decision::Block {
            reason: Some("sandbox exec failed: no output stream".into()),
        });
    };
    let drain = tokio::spawn(async move {
        let mut out = Vec::new();
        while let Some(chunk) = bytes.recv().await {
            if chunk.stream == ir::LogStream::Stdout {
                out.extend(chunk.bytes);
            }
        }
        out
    });
    let status = match timeout(hook.timeout(), handle.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            return Executed::Decided(Decision::Block {
                reason: Some(format!("sandbox exec failed: {error}")),
            });
        }
        Err(_) => {
            let _ = handle.signal(Sig::Kill).await;
            let _ = handle.wait().await;
            drain.abort();
            return Executed::Decided(parse_decision(-1, ""));
        }
    };
    let stdout = drain.await.unwrap_or_default();
    let code = status.code.unwrap_or(-1);
    Executed::Decided(parse_decision(code, &String::from_utf8_lossy(&stdout)))
}

/// On the host: `sh -c`, the context on stdin, in the workspace directory
/// when the sandbox shares the host filesystem.
async fn host_command(
    hook: &HookDefinition,
    command: &str,
    payload: &[u8],
    vars: BTreeMap<String, String>,
    env: Option<&dyn ExecEnv>,
) -> Executed {
    let mut process = tokio::process::Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        .envs(vars)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(env) = env
        && env.shares_host_filesystem()
    {
        process.current_dir(env.workspace_path());
    }
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Executed::Decided(Decision::Block {
                reason: Some(format!("command spawn failed: {error}")),
            });
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = payload.to_vec();
        tokio::spawn(async move {
            let _ = stdin.write_all(&bytes).await;
            let _ = stdin.shutdown().await;
        });
    }
    match timeout(hook.timeout(), child.wait_with_output()).await {
        Ok(Ok(output)) => Executed::Decided(parse_decision(
            output.status.code().unwrap_or(1),
            &String::from_utf8_lossy(&output.stdout),
        )),
        Ok(Err(error)) => Executed::Decided(Decision::Block {
            reason: Some(format!("command wait failed: {error}")),
        }),
        Err(_) => Executed::Decided(parse_decision(-1, "")),
    }
}

async fn http_hook(
    hook: &HookDefinition,
    url: &str,
    headers: &BTreeMap<String, String>,
    tls: TlsMode,
    context: &Context,
    http: &HttpClients,
) -> Executed {
    if tls != TlsMode::Off && !url.starts_with("https://") {
        return Executed::Decided(Decision::Block {
            reason: Some(format!(
                "HTTP hook URL must use https:// (tls mode is {tls:?})"
            )),
        });
    }
    let client = match http.client(tls) {
        Ok(client) => client,
        Err(error) => return Executed::FailedOpen(format!("HTTP client: {error}")),
    };
    let mut request = client.post(url).timeout(hook.timeout()).json(context);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return Executed::FailedOpen(format!("HTTP request failed: {error}")),
    };
    if !response.status().is_success() {
        return Executed::FailedOpen(format!("HTTP hook returned {}", response.status()));
    }
    let body = match response.text().await {
        Ok(body) => body,
        Err(error) => return Executed::FailedOpen(format!("HTTP body: {error}")),
    };
    if body.trim().is_empty() {
        return Executed::Decided(Decision::Proceed);
    }
    match serde_json::from_str::<Decision>(body.trim()) {
        Ok(decision) => Executed::Decided(decision),
        Err(_) => Executed::FailedOpen("HTTP response is not a hook decision".into()),
    }
}

fn evaluator_message(prompt: &str, context: &Context) -> String {
    format!(
        "Hook prompt: {prompt}\n\nEvent context:\n{}",
        serde_json::to_string_pretty(context).unwrap_or_default()
    )
}

/// Fabro's verdict to a decision: `ok` proceeds, anything else blocks.
fn verdict(text: &str) -> Option<Decision> {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();
    let verdict: Verdict = serde_json::from_str(inner).ok()?;
    Some(if verdict.ok {
        Decision::Proceed
    } else {
        Decision::Block {
            reason: verdict.reason,
        }
    })
}

async fn prompt_hook(
    hook: &HookDefinition,
    prompt: &str,
    model: Option<&str>,
    context: &Context,
    client: Option<&PebbleClient>,
) -> Executed {
    let Some(client) = client else {
        return Executed::Unsupported("a prompt hook needs the application's model client".into());
    };
    let model = model.unwrap_or(DEFAULT_MODEL);
    let schema = json!({
        "type": "object",
        "properties": { "ok": { "type": "boolean" }, "reason": { "type": "string" } },
        "required": ["ok"],
        "additionalProperties": false,
    });
    let mut builder = Request::builder()
        .model(model)
        .system(EVALUATOR_SYSTEM)
        .message(Message::text(
            Role::User,
            evaluator_message(prompt, context),
        ))
        .max_output_tokens(1024)
        .timeout(hook.timeout());
    let format = ResponseFormat::JsonSchema {
        name: "hook_verdict".into(),
        schema,
    };
    if let Ok(probe) = Request::builder().model(model).user("probe").build()
        && let Ok(route) = client.0.resolve_route(&probe)
        && !route
            .model()
            .capabilities()
            .response_format(&format)
            .is_unsupported()
    {
        builder = builder.response_format(format);
    }
    let request = match builder.build() {
        Ok(request) => request,
        Err(error) => return Executed::FailedOpen(format!("prompt hook request: {error}")),
    };
    match timeout(hook.timeout(), client.0.complete(request)).await {
        Ok(Ok(response)) => match verdict(&response.text()) {
            Some(decision) => Executed::Decided(decision),
            None => Executed::FailedOpen("the model did not return a hook verdict".into()),
        },
        Ok(Err(error)) => Executed::FailedOpen(format!("prompt hook model call failed: {error}")),
        Err(_) => Executed::FailedOpen(format!(
            "prompt hook timed out after {} ms",
            hook.timeout().as_millis()
        )),
    }
}

async fn agent_hook(
    hook: &HookDefinition,
    prompt: &str,
    model: Option<&str>,
    max_tool_rounds: u32,
    context: &Context,
    env: Option<&Arc<dyn ExecEnv>>,
    client: Option<&PebbleClient>,
) -> Executed {
    let Some(client) = client else {
        return Executed::Unsupported("an agent hook needs the application's model client".into());
    };
    let Some(env) = env else {
        return Executed::Unsupported(
            "an agent hook runs its tools in the sandbox, and no sandbox environment is available \
             at this point"
                .into(),
        );
    };
    let model = model.unwrap_or(DEFAULT_MODEL);
    let cancel = CancellationToken::new();
    let kill = CancellationToken::new();
    let work = async {
        let environment = PebbleEnvironment::prepare(env.clone(), cancel.clone(), kill.clone())
            .await
            .map_err(|e| format!("agent hook environment: {e}"))?;
        let mut agent = CodingAgent::builder(client.0.clone(), Arc::new(environment))
            .model(model)
            .permission_level(PermissionLevel::Full)
            .build()
            .await
            .map_err(|e| format!("agent hook could not start: {e}"))?;
        let instructions = format!(
            "{EVALUATOR_SYSTEM}\n\n{}\n\nWhen you have decided, reply with only a JSON object: \
             {{\"ok\": true}} or {{\"ok\": false, \"reason\": \"...\"}}. You may use at most \
             {max_tool_rounds} tool calls.",
            evaluator_message(prompt, context)
        );
        let report = agent.prompt_with_cancellation(&instructions, &cancel).await;
        let text = report
            .result
            .map(|output| output.text.unwrap_or_default())
            .map_err(|e| format!("agent hook failed: {e}"));
        let _ = agent.shutdown(ShutdownReason::Completed).await;
        text
    };
    match timeout(hook.timeout(), work).await {
        Ok(Ok(text)) => match verdict(&text) {
            Some(decision) => Executed::Decided(decision),
            None => Executed::FailedOpen("the agent did not return a hook verdict".into()),
        },
        Ok(Err(message)) => Executed::FailedOpen(message),
        Err(_) => {
            cancel.cancel();
            Executed::FailedOpen(format!(
                "agent hook timed out after {} ms",
                hook.timeout().as_millis()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_decide_as_fabro_decides() {
        assert_eq!(parse_decision(0, ""), Decision::Proceed);
        assert_eq!(
            parse_decision(0, r#"{"decision":"skip","reason":"ci"}"#),
            Decision::Skip {
                reason: Some("ci".into()),
            }
        );
        assert_eq!(parse_decision(2, ""), Decision::Block {
            reason: Some("hook exited with code 2".into()),
        });
        assert_eq!(
            parse_decision(2, r#"{"decision":"proceed"}"#),
            Decision::Proceed
        );
        assert_eq!(
            parse_decision(3, r#"{"decision":"proceed"}"#),
            Decision::Block {
                reason: Some("hook exited with code 3".into()),
            }
        );
        assert_eq!(parse_decision(-1, ""), Decision::Block {
            reason: Some("hook exited with code -1".into()),
        });
    }

    #[test]
    fn verdicts_parse_with_or_without_fences() {
        assert_eq!(verdict(r#"{"ok": true}"#), Some(Decision::Proceed));
        assert_eq!(
            verdict("```json\n{\"ok\": false, \"reason\": \"tests fail\"}\n```"),
            Some(Decision::Block {
                reason: Some("tests fail".into()),
            })
        );
        assert_eq!(verdict("maybe"), None);
    }
}
