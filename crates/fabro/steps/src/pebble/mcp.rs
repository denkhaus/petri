//! The run's `[run.agent.mcps]` servers on a native session, through Pebble's
//! `mcp` feature.
//!
//! Pebble owns the servers. The session names them to the builder
//! (`CodingAgentBuilder::mcp_servers`); Pebble starts each one while it builds
//! the agent, registers every tool it advertises as a `RegisteredTool` whose
//! source is `ToolSource::Mcp` under `mcp__<server>__<tool>` (the name Fabro
//! gives it, and the name `frontend_fabro::mcps::qualified_tool_name`
//! computes), runs the calls through its normal tool path (so Pebble's
//! middleware, history, output policy, cancellation and agent events apply to
//! an MCP tool as to any other), and closes the servers when the agent shuts
//! down. What stays Petri's is the mapping from Fabro's entries to Pebble's
//! servers ([`pebble_servers`]) and the event contract ([`Mirror`]).
//!
//! Placement follows Fabro: a `stdio` server is a child process of Petri's
//! host (Fabro's run worker), never of the sandbox; its working directory is
//! the scope's workspace when the scope shares the host filesystem, else
//! Petri's own. An `http` server is reached from the host. A `sandbox` server
//! is launched in the scope's execution environment and reached over HTTP
//! through the environment's route to its port (`ExecEnv::preview_url`, handed
//! to Pebble as sandbox-driver's `PreviewUrls` facet by
//! [`super::environment::PortRoutes`]): the host's own loopback, the Docker
//! plugin's forward into the container, or Daytona's preview link with its
//! token header. Pebble releases the route when the server stops. Secrets in
//! `env` and `headers` are resolved here, when the servers are named, and
//! never written down.
//!
//! Failure behavior follows Fabro: a server that does not start (a spawn
//! error, a handshake timeout, a protocol error, an unavailable secret) is
//! reported and skipped, and the session proceeds with the tools of the
//! servers that started. A tool result the server marks `isError` reaches
//! the model as the tool's error text; a transport failure, a timeout or a
//! cancellation reaches it as a failed call with a reason.
//!
//! Every fact is a `StepEvent::Custom` payload attributed to the node,
//! firing and attempt: [`SERVER_EVENT`] for the server lifecycle and
//! [`TOOL_EVENT`] for every proxied call, mirrored from Pebble's
//! `McpServerReady`, `McpServerFailed`, `ToolCallStarted` and
//! `ToolCallCompleted` events by the session's sink. A retained session's
//! successor node names the same servers again, so Pebble starts its own and
//! registers the same tool names, and the conversation's earlier tool calls
//! stay valid (`crate::sessions`).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use executor::{ExecEnv, Masker, SecretProvider};
use frontend_fabro::mcps::{McpServer, McpTransport, McpValue, parse_qualified_name};
use ir::{Attempt, FiringId, LogStream, StepEvent, Value};
use pebble_coding_agent::events::{CodingEvent, ToolErrorKind};
use pebble_coding_agent::mcp::{McpHttpProtocol, McpPlacement, McpServer as PebbleServer};
use serde_json::json;
use smol_str::SmolStr;
use steps::{ProgressSender, StepCtx};

use super::environment::elapsed_ms;

/// The `kind` of the `StepEvent::Custom` payload for a server's lifecycle:
/// `{ kind, node, firing, attempt, server, transport, placement, phase,
/// tools?, tool_count?, error? }`. `phase` is `starting` (the server is named
/// to the builder; Pebble starts it while the agent is built), `ready` (with
/// `tools` as `[{ name, original_name }]` sorted by `name` and `tool_count`),
/// `failed` (with `error`), or `stopped` (Pebble closed it with the agent).
/// `placement` is `host` (stdio), `remote` (http) or `scope` (sandbox).
pub const SERVER_EVENT: &str = "fabro.mcp.server";

/// The `kind` of the `StepEvent::Custom` payload for one proxied tool call:
/// `{ kind, node, firing, attempt, server, name, tool, tool_call_id, status,
/// duration_ms, error? }`. `name` is the qualified name the model called,
/// `tool` the server's own name. `status` is `ok` (a result), `error` (a
/// result the server marked as an error, or no answer within the tool
/// timeout; the model sees the text in `error`), `failed` (the call did not
/// reach the server, or came back malformed) or `cancelled`. `duration_ms`
/// is the time between Pebble's `ToolCallStarted` and `ToolCallCompleted`.
pub const TOOL_EVENT: &str = "fabro.mcp.tool";

/// Fabro's entries as Pebble's servers, in configuration order, their
/// secrets resolved.
pub(super) struct Prepared {
    /// The servers Pebble starts.
    pub(super) servers:     Vec<PebbleServer>,
    /// The servers whose secret is unavailable, with the reason: reported
    /// `failed` and never named to Pebble.
    pub(super) unavailable: Vec<(String, String)>,
}

/// Map every configured server onto Pebble's, resolving `{"$secret": name}`
/// values through the run's secret provider now and nowhere else; the
/// resolved values go into the process or the request and are never written
/// down.
pub(super) fn pebble_servers(
    servers: &[McpServer],
    env: &dyn ExecEnv,
    secrets: &dyn SecretProvider,
) -> Prepared {
    let mut prepared = Prepared {
        servers:     Vec::with_capacity(servers.len()),
        unavailable: Vec::new(),
    };
    for server in servers {
        match pebble_server(server, env, secrets) {
            Ok(server) => prepared.servers.push(server),
            Err(error) => prepared
                .unavailable
                .push((server.name.clone(), error.to_string())),
        }
    }
    prepared
}

/// A secret a transport names that the run cannot supply.
#[derive(Debug, thiserror::Error)]
#[error("secret `{name}` for `{field}` is unavailable: {reason}")]
struct SecretUnavailable {
    name:   String,
    field:  String,
    reason: String,
}

fn pebble_server(
    server: &McpServer,
    env: &dyn ExecEnv,
    secrets: &dyn SecretProvider,
) -> Result<PebbleServer, SecretUnavailable> {
    let placement = match &server.transport {
        McpTransport::Stdio { command, env: vars } => McpPlacement::Stdio {
            command:     command.clone(),
            env:         resolve_values(vars, secrets, "env")?,
            current_dir: env
                .shares_host_filesystem()
                .then(|| PathBuf::from(env.workspace_path())),
            clear_env:   false,
        },
        McpTransport::Http { url, headers } => McpPlacement::Http {
            url:      url.clone(),
            headers:  resolve_values(headers, secrets, "headers")?,
            protocol: McpHttpProtocol::StreamableHttp,
        },
        McpTransport::Sandbox {
            command,
            port,
            env: vars,
        } => McpPlacement::Environment {
            command:  command.clone(),
            port:     *port,
            env:      resolve_values(vars, secrets, "env")?,
            protocol: McpHttpProtocol::StreamableHttp,
            path:     None,
        },
    };
    Ok(PebbleServer::new(server.name.clone(), placement)
        .with_startup_timeout(Duration::from_millis(server.startup_timeout_ms))
        .with_tool_timeout(Duration::from_millis(server.tool_timeout_ms)))
}

fn resolve_values(
    values: &BTreeMap<String, McpValue>,
    secrets: &dyn SecretProvider,
    field: &str,
) -> Result<BTreeMap<String, String>, SecretUnavailable> {
    let mut out = BTreeMap::new();
    for (key, value) in values {
        let text = match value {
            McpValue::Literal(text) => text.clone(),
            McpValue::Secret { name } => secrets
                .resolve(name)
                .map_err(|error| SecretUnavailable {
                    name:   name.clone(),
                    field:  format!("{field}.{key}"),
                    reason: error.to_string(),
                })?
                .expose()
                .to_string(),
        };
        out.insert(key.clone(), text);
    }
    Ok(out)
}

fn placement(transport: &McpTransport) -> &'static str {
    match transport {
        McpTransport::Stdio { .. } => "host",
        McpTransport::Http { .. } => "remote",
        McpTransport::Sandbox { .. } => "scope",
    }
}

/// Petri's record of the servers' lifecycle and calls, from Pebble's events.
/// The session's sink hands every event to [`Mirror::observe`]; the session
/// itself says when the servers were named ([`Mirror::starting`]) and when
/// Pebble closed them ([`Mirror::stopped`]).
pub(super) struct Mirror {
    logs:    ProgressSender,
    masker:  Masker,
    node:    SmolStr,
    firing:  FiringId,
    attempt: Attempt,
    /// The configured servers by name, for `transport` and `placement`.
    servers: BTreeMap<String, McpTransport>,
    /// The server and its own tool name behind each registered name, from
    /// the `ready` events.
    tools:   Mutex<HashMap<String, (String, String)>>,
    /// When each MCP call in flight started, by call id.
    calls:   Mutex<HashMap<String, Instant>>,
}

impl Mirror {
    pub(super) fn new(ctx: &StepCtx, servers: &[McpServer]) -> Self {
        Self {
            logs:    ctx.logs.clone(),
            masker:  ctx.secrets.masker(),
            node:    ctx.node.clone(),
            firing:  ctx.firing,
            attempt: ctx.attempt,
            servers: servers
                .iter()
                .map(|server| (server.name.clone(), server.transport.clone()))
                .collect(),
            tools:   Mutex::default(),
            calls:   Mutex::default(),
        }
    }

    /// The server is named to the builder; Pebble starts it next.
    pub(super) async fn starting(&self, server: &str) {
        self.server(server, "starting", json!({})).await;
    }

    /// The server did not start: the event, and the line on the node's
    /// stderr.
    pub(super) async fn failed(&self, server: &str, error: &str) {
        tracing::error!(server, error, "MCP server failed to start");
        let line = format!("mcp server `{server}` failed to start: {error}");
        let _ = self
            .logs
            .send(StepEvent::Log {
                stream: LogStream::Stderr,
                line:   self.masker.mask(&line),
            })
            .await;
        self.server(server, "failed", json!({ "error": error }))
            .await;
    }

    /// Pebble closed the server with the agent.
    pub(super) async fn stopped(&self, server: &str) {
        self.server(server, "stopped", json!({})).await;
    }

    /// What `event` says about the servers and their calls, as Petri's
    /// events.
    pub(super) async fn observe(&self, event: &CodingEvent) {
        match event {
            CodingEvent::McpServerReady { server, tools } => {
                {
                    let mut known = self.tools.lock().unwrap_or_else(PoisonError::into_inner);
                    for tool in tools {
                        known.insert(
                            tool.name.clone(),
                            (server.clone(), tool.original_name.clone()),
                        );
                    }
                }
                self.server(
                    server,
                    "ready",
                    json!({ "tool_count": tools.len(), "tools": tools }),
                )
                .await;
            }
            CodingEvent::McpServerFailed { server, error } => self.failed(server, error).await,
            CodingEvent::ToolCallStarted {
                tool_name,
                tool_call_id,
                ..
            } if parse_qualified_name(tool_name).is_some() => {
                self.calls
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(tool_call_id.clone(), Instant::now());
            }
            CodingEvent::ToolCallCompleted {
                tool_name,
                tool_call_id,
                output,
                is_error,
                error_kind,
                ..
            } => {
                let Some(parsed) = parse_qualified_name(tool_name) else {
                    return;
                };
                let began = self
                    .calls
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(tool_call_id);
                let status = match (*is_error, error_kind) {
                    (false, _) => "ok",
                    (true, Some(ToolErrorKind::Cancelled)) => "cancelled",
                    (true, Some(ToolErrorKind::Unavailable)) => "failed",
                    // Refused before it reached the server: a hook's block, or
                    // arguments Pebble rejected. Not a proxied call.
                    (true, Some(ToolErrorKind::Denied | ToolErrorKind::InvalidArguments)) => {
                        return;
                    }
                    (true, _) => "error",
                };
                let (server, tool) = self
                    .tools
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(tool_name)
                    .cloned()
                    .unwrap_or(parsed);
                let error = is_error.then(|| match output {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                });
                self.send(json!({
                    "kind": TOOL_EVENT,
                    "node": self.node,
                    "firing": self.firing,
                    "attempt": self.attempt,
                    "server": server,
                    "name": tool_name,
                    "tool": tool,
                    "tool_call_id": tool_call_id,
                    "status": status,
                    "duration_ms": began.map(|began| elapsed_ms(began.elapsed())),
                    "error": error,
                }))
                .await;
            }
            _ => {}
        }
    }

    async fn server(&self, server: &str, phase: &str, extra: Value) {
        let transport = self.servers.get(server);
        let mut payload = json!({
            "kind": SERVER_EVENT,
            "node": self.node,
            "firing": self.firing,
            "attempt": self.attempt,
            "server": server,
            "transport": transport.map(McpTransport::kind),
            "placement": transport.map(placement),
            "phase": phase,
        });
        if let (Value::Object(map), Value::Object(extra)) = (&mut payload, extra) {
            map.extend(extra);
        }
        self.send(payload).await;
    }

    async fn send(&self, payload: Value) {
        let _ = self
            .logs
            .send(StepEvent::Custom(self.masker.mask_value(&payload)))
            .await;
    }
}
