//! The MCP servers a native agent node connects to: Petri's application
//! service for `[run.agent.mcps]`.
//!
//! [`McpServers`] owns every server process and connection for one agent
//! node's session. It starts them before the Pebble agent is built, registers
//! each discovered tool with Pebble as a [`RegisteredTool`] whose source is
//! [`ToolSource::Mcp`] (so Pebble's middleware, history, output policy,
//! cancellation and agent events apply to MCP tools as to any other), and
//! shuts the servers down when the session ends, which is before the node
//! returns and so before the scope's environment is released. The MCP
//! protocol lives in the `rmcp` client library ([`client`]); Pebble's agent
//! loop sees only tools.
//!
//! Placement follows Fabro: a `stdio` server is a child process of Petri's
//! host (Fabro's run worker), never of the sandbox; its working directory is
//! the scope's workspace when the scope shares the host filesystem, else
//! Petri's own. An `http` server is reached from the host. A `sandbox` server
//! is launched in the scope's execution environment and reached over HTTP
//! through the environment's route to its port (`ExecEnv::preview_url`): the
//! host's own loopback, the Docker plugin's forward into the container, or
//! Daytona's preview link with its token header. The route is released when
//! the server stops.
//!
//! Failure behavior follows Fabro: a server that does not start (a spawn
//! error, a handshake timeout, a protocol error) is reported and skipped, and
//! the session proceeds with the tools of the servers that started. A tool
//! result the server marks `isError` reaches the model as the tool's error
//! text; a transport failure, a timeout or a cancellation reaches it as a
//! failed call with a reason.
//!
//! Every fact is a `StepEvent::Custom` payload attributed to the node,
//! firing and attempt: [`SERVER_EVENT`] for the server lifecycle and
//! [`TOOL_EVENT`] for every proxied call. A retained session's successor node
//! starts its own servers again and registers the same tool names, so the
//! conversation's earlier tool calls stay valid (`crate::sessions`).

mod client;

use std::sync::Arc;
use std::time::{Duration, Instant};

use client::{CallOutcome, Connection, DiscoveredTool};
use executor::{ExecEnv, Masker, SecretProvider};
use frontend_fabro::mcps::{McpServer, McpTransport, qualified_tool_name};
use ir::{Attempt, FiringId, LogStream, StepEvent, Value};
use lithos_llm::types::ToolDefinition;
use pebble_coding_agent::tools::{RegisteredTool, ToolContext, ToolError, ToolSource};
use serde_json::json;
use smol_str::SmolStr;
use steps::StepCtx;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The `kind` of the `StepEvent::Custom` payload for a server's lifecycle:
/// `{ kind, node, firing, attempt, server, transport, placement, phase,
/// tools?, tool_count?, error?, duration_ms? }`. `phase` is `starting`,
/// `ready` (with `tools` as `[{ name, original_name }]` sorted by `name` and
/// `tool_count`), `failed` (with `error`), `disconnected` (with `error`), or
/// `stopped`. `placement` is `host` (stdio), `remote` (http) or `scope`
/// (sandbox).
pub const SERVER_EVENT: &str = "fabro.mcp.server";

/// The `kind` of the `StepEvent::Custom` payload for one proxied tool call:
/// `{ kind, node, firing, attempt, server, name, tool, tool_call_id, status,
/// duration_ms, error? }`. `name` is the qualified name the model called,
/// `tool` the server's own name. `status` is `ok` (a result), `error` (a
/// result the server marked as an error; the model sees its text), `failed`
/// (a transport or protocol failure), `timeout` or `cancelled`.
pub const TOOL_EVENT: &str = "fabro.mcp.tool";

/// How long a server gets to close after the session ends, as Fabro bounds
/// it.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// The attribution every MCP event carries, cloned out of the step context
/// so the tool executors need no borrow of it.
#[derive(Clone)]
pub(crate) struct Attribution {
    logs:    mpsc::Sender<StepEvent>,
    node:    SmolStr,
    firing:  FiringId,
    attempt: Attempt,
    masker:  Masker,
}

impl Attribution {
    pub(crate) fn of(ctx: &StepCtx) -> Self {
        Self {
            logs:    ctx.logs.clone(),
            node:    ctx.node.clone(),
            firing:  ctx.firing,
            attempt: ctx.attempt,
            masker:  ctx.secrets.masker(),
        }
    }

    async fn server(&self, server: &McpServer, phase: &str, extra: Value) {
        let mut payload = json!({
            "kind": SERVER_EVENT,
            "node": self.node,
            "firing": self.firing,
            "attempt": self.attempt,
            "server": server.name,
            "transport": server.transport.kind(),
            "placement": placement(&server.transport),
            "phase": phase,
        });
        if let (Value::Object(map), Value::Object(extra)) = (&mut payload, extra) {
            map.extend(extra);
        }
        let _ = self
            .logs
            .send(StepEvent::Custom(self.masker.mask_value(&payload)))
            .await;
    }

    async fn tool(&self, payload: Value) {
        let _ = self
            .logs
            .send(StepEvent::Custom(self.masker.mask_value(&payload)))
            .await;
    }

    async fn line(&self, line: String) {
        let _ = self
            .logs
            .send(StepEvent::Log {
                stream: LogStream::Stderr,
                line:   self.masker.mask(&line),
            })
            .await;
    }
}

fn placement(transport: &McpTransport) -> &'static str {
    match transport {
        McpTransport::Stdio { .. } => "host",
        McpTransport::Http { .. } => "remote",
        McpTransport::Sandbox { .. } => "scope",
    }
}

/// One node's MCP servers: started, their tools registered, and shut down
/// with the session.
pub struct McpServers {
    connections: Vec<(McpServer, Arc<Connection>)>,
    tools:       Vec<RegisteredTool>,
    attribution: Attribution,
}

impl McpServers {
    /// Start every configured server in order, as Fabro does, and discover
    /// its tools. A server that fails is reported and skipped. `cancel` stops
    /// the remaining starts (the servers already started are still owned and
    /// shut down by the returned value).
    pub(crate) async fn start(
        servers: &[McpServer],
        env: Arc<dyn ExecEnv>,
        secrets: Arc<dyn SecretProvider>,
        attribution: Attribution,
        cancel: &CancellationToken,
    ) -> Self {
        let mut started = Self {
            connections: Vec::with_capacity(servers.len()),
            tools: Vec::new(),
            attribution,
        };
        for server in servers {
            if cancel.is_cancelled() {
                break;
            }
            let began = Instant::now();
            started
                .attribution
                .server(server, "starting", json!({}))
                .await;
            // Boxed: the start future carries the readiness probe and the
            // route, and it would otherwise weigh on every session future
            // above it.
            match Box::pin(Connection::start(server, &env, secrets.as_ref(), cancel)).await {
                Ok((connection, tools)) => {
                    let connection = Arc::new(connection);
                    let mut summaries: Vec<Value> = tools
                        .iter()
                        .map(|tool| {
                            json!({
                                "name": qualified_tool_name(&server.name, &tool.name),
                                "original_name": tool.name,
                            })
                        })
                        .collect();
                    summaries.sort_by_key(|summary| summary["name"].as_str().map(str::to_owned));
                    for tool in tools {
                        started.tools.push(registered_tool(
                            &connection,
                            server,
                            tool,
                            started.attribution.clone(),
                        ));
                    }
                    started
                        .attribution
                        .server(
                            server,
                            "ready",
                            json!({
                                "tool_count": summaries.len(),
                                "tools": summaries,
                                "duration_ms": elapsed_ms(began),
                            }),
                        )
                        .await;
                    started.connections.push((server.clone(), connection));
                }
                Err(error) => {
                    let message = error.to_string();
                    tracing::error!(server = %server.name, error = %message, "MCP server failed to start");
                    started
                        .attribution
                        .line(format!(
                            "mcp server `{}` failed to start: {message}",
                            server.name
                        ))
                        .await;
                    started
                        .attribution
                        .server(
                            server,
                            "failed",
                            json!({ "error": message, "duration_ms": elapsed_ms(began) }),
                        )
                        .await;
                }
            }
        }
        started
    }

    /// The tools to register with the agent, one per discovered tool.
    pub(crate) fn tools(&self) -> Vec<RegisteredTool> {
        self.tools.clone()
    }

    /// The names of the servers that started, in order.
    pub fn servers(&self) -> Vec<String> {
        self.connections
            .iter()
            .map(|(server, _)| server.name.clone())
            .collect()
    }

    /// Close every connection and stop every owned process, then report
    /// each server `stopped`. Runs after the agent shut down, so no call is
    /// in flight, and before the node returns, so before the scope's
    /// environment is released.
    pub(crate) async fn shutdown(&mut self) {
        for (server, connection) in self.connections.drain(..) {
            let began = Instant::now();
            connection.close(SHUTDOWN_TIMEOUT).await;
            self.attribution
                .server(
                    &server,
                    "stopped",
                    json!({ "duration_ms": elapsed_ms(began) }),
                )
                .await;
        }
    }
}

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The Pebble tool that forwards one MCP tool to its server.
fn registered_tool(
    connection: &Arc<Connection>,
    server: &McpServer,
    tool: DiscoveredTool,
    attribution: Attribution,
) -> RegisteredTool {
    let qualified = qualified_tool_name(&server.name, &tool.name);
    let definition =
        ToolDefinition::function(qualified.clone(), tool.description, tool.input_schema);
    let connection = Arc::clone(connection);
    let source = ToolSource::Mcp {
        server_name:   server.name.clone(),
        original_name: tool.name.clone(),
    };
    let server = server.clone();
    let server_name = server.name.clone();
    let original = tool.name.clone();
    let executor = Arc::new(move |arguments: Value, context: ToolContext| {
        let connection = Arc::clone(&connection);
        let attribution = attribution.clone();
        let server = server.clone();
        let server_name = server_name.clone();
        let qualified = qualified.clone();
        let original = original.clone();
        let call_id = context.tool_call_id().map(str::to_owned);
        Box::pin(async move {
            let began = Instant::now();
            let outcome = connection
                .call(&original, arguments, context.cancel())
                .await;
            if let CallOutcome::Failed(message) = &outcome
                && connection.take_disconnected()
            {
                attribution
                    .server(&server, "disconnected", json!({ "error": message }))
                    .await;
            }
            let (status, error) = match &outcome {
                CallOutcome::Ok(_) => ("ok", None),
                CallOutcome::ToolError(text) => ("error", Some(text.clone())),
                CallOutcome::Failed(message) => ("failed", Some(message.clone())),
                CallOutcome::Timeout(_) => ("timeout", None),
                CallOutcome::Cancelled => ("cancelled", None),
            };
            attribution
                .tool(json!({
                    "kind": TOOL_EVENT,
                    "node": attribution.node,
                    "firing": attribution.firing,
                    "attempt": attribution.attempt,
                    "server": server_name,
                    "name": qualified,
                    "tool": original,
                    "tool_call_id": call_id,
                    "status": status,
                    "duration_ms": elapsed_ms(began),
                    "error": error,
                }))
                .await;
            match outcome {
                CallOutcome::Ok(text) => Ok(text),
                CallOutcome::ToolError(text) => Err(ToolError::execution(text)),
                CallOutcome::Failed(message) => Err(ToolError::unavailable(format!(
                    "MCP server `{server_name}` failed the call to `{original}`: {message}"
                ))),
                CallOutcome::Timeout(timeout) => Err(ToolError::execution(format!(
                    "MCP tool `{original}` on server `{server_name}` did not answer within {}s",
                    timeout.as_secs()
                ))),
                CallOutcome::Cancelled => Err(ToolError::cancelled(format!(
                    "MCP tool `{original}` on server `{server_name}` was cancelled"
                ))),
            }
        }) as _
    });
    RegisteredTool::new(definition, executor)
        .with_source(source)
        .allow_in_subagents()
}
