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
//! Petri's own. An `http` server is reached from the host, over streamable
//! HTTP or, with `protocol = "sse"`, the older SSE transport. A `sandbox`
//! server is launched in the scope's execution environment and reached over
//! the same two protocols through the environment's route to its port
//! (`ExecEnv::preview_url`, handed to Pebble as sandbox-driver's
//! `PreviewUrls` facet by [`super::environment::PortRoutes`]): the host's own
//! loopback, the Docker plugin's forward into the container, or Daytona's
//! preview link with its token header. A streamable HTTP server is reached
//! at the route itself; an SSE server serves its event stream at `/sse`
//! under it ([`SSE_PATH`]), where Fabro has always reached one. Pebble
//! releases the route when the server stops. Secrets in `env` and `headers`
//! are resolved here, when the servers are named, and never written down.
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
//! `ToolCallCompleted` events by the session's sink, each sent acknowledged
//! so that when Pebble's own event is confirmed recorded, Petri's derived one
//! is too. A retained session's
//! successor node names the same servers again, so Pebble starts its own and
//! registers the same tool names, and the conversation's earlier tool calls
//! stay valid (`crate::sessions`).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use executor::{ExecEnv, Masker, SecretProvider};
use frontend_fabro::mcps::{
    McpHttpProtocol, McpServer, McpTransport, McpValue, parse_qualified_name,
};
use ir::{Attempt, FiringId, LogStream, StepEvent, Value};
use pebble_coding_agent::events::{CodingEvent, ToolErrorKind};
use pebble_coding_agent::mcp::{
    McpHttpProtocol as PebbleProtocol, McpPlacement, McpServer as PebbleServer,
};
use serde_json::json;
use smol_str::SmolStr;
use steps::{ProgressError, ProgressSender, StepCtx};

use super::environment::elapsed_ms;

/// Where a sandbox-hosted SSE server serves its event stream, under the
/// environment's route to its port: the path Fabro reaches one at.
pub const SSE_PATH: &str = "/sse";

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
        McpTransport::Http {
            protocol,
            url,
            headers,
        } => McpPlacement::Http {
            url:      url.clone(),
            headers:  resolve_values(headers, secrets, "headers")?,
            protocol: pebble_protocol(*protocol),
        },
        McpTransport::Sandbox {
            protocol,
            command,
            port,
            env: vars,
        } => McpPlacement::Environment {
            command:  command.clone(),
            port:     *port,
            env:      resolve_values(vars, secrets, "env")?,
            protocol: pebble_protocol(*protocol),
            path:     matches!(protocol, McpHttpProtocol::Sse).then(|| SSE_PATH.to_owned()),
        },
    };
    Ok(PebbleServer::new(server.name.clone(), placement)
        .with_startup_timeout(Duration::from_millis(server.startup_timeout_ms))
        .with_tool_timeout(Duration::from_millis(server.tool_timeout_ms)))
}

fn pebble_protocol(protocol: McpHttpProtocol) -> PebbleProtocol {
    match protocol {
        McpHttpProtocol::StreamableHttp => PebbleProtocol::StreamableHttp,
        McpHttpProtocol::Sse => PebbleProtocol::Sse,
    }
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
/// Pebble closed them ([`Mirror::stopped`]). Every event is sent
/// acknowledged: a method returns once the record is durable, and an error
/// says the driver stopped taking progress or a store failed.
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
    pub(super) async fn starting(&self, server: &str) -> Result<(), ProgressError> {
        self.server(server, "starting", json!({})).await
    }

    /// The server did not start: the event, and the line on the node's
    /// stderr.
    pub(super) async fn failed(&self, server: &str, error: &str) -> Result<(), ProgressError> {
        tracing::error!(server, error, "MCP server failed to start");
        let line = format!("mcp server `{server}` failed to start: {error}");
        self.logs
            .send_acked(StepEvent::Log {
                stream: LogStream::Stderr,
                line:   self.masker.mask(&line),
            })
            .await?;
        self.server(server, "failed", json!({ "error": error }))
            .await
    }

    /// Pebble closed the server with the agent.
    pub(super) async fn stopped(&self, server: &str) -> Result<(), ProgressError> {
        self.server(server, "stopped", json!({})).await
    }

    /// What `event` says about the servers and their calls, as Petri's
    /// events.
    pub(super) async fn observe(&self, event: &CodingEvent) -> Result<(), ProgressError> {
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
                .await
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
                Ok(())
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
                    return Ok(());
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
                        return Ok(());
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
                .await
            }
            _ => Ok(()),
        }
    }

    async fn server(&self, server: &str, phase: &str, extra: Value) -> Result<(), ProgressError> {
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
        self.send(payload).await
    }

    async fn send(&self, payload: Value) -> Result<(), ProgressError> {
        self.logs
            .send_acked(StepEvent::Custom(self.masker.mask_value(&payload)))
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::slice;

    use executor::{EnvError, MapSecrets, ProcessHandle, ProcessSpec};

    use super::*;

    /// An environment that runs nothing and shares no filesystem: the
    /// mapping needs only its workspace answers.
    #[derive(Debug)]
    struct NoEnv;

    #[async_trait::async_trait]
    impl ExecEnv for NoEnv {
        async fn spawn(&self, _spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
            Err(EnvError::backend("none", "spawn", "runs nothing"))
        }

        fn workspace_path(&self) -> &'static str {
            "/work"
        }

        async fn read_file(&self, _relative: &Path) -> Result<Option<Vec<u8>>, EnvError> {
            Ok(None)
        }

        async fn write_file(&self, _relative: &Path, _: &[u8]) -> Result<(), EnvError> {
            Ok(())
        }

        fn grace(&self) -> Duration {
            Duration::from_secs(1)
        }
    }

    fn server(name: &str, transport: McpTransport) -> McpServer {
        McpServer {
            name: name.to_owned(),
            transport,
            startup_timeout_ms: 1_000,
            tool_timeout_ms: 2_000,
            source: "workflow.toml".to_owned(),
        }
    }

    fn placement_of(server: &McpServer) -> McpPlacement {
        let prepared = pebble_servers(slice::from_ref(server), &NoEnv, &MapSecrets::empty());
        assert!(
            prepared.unavailable.is_empty(),
            "{:?}",
            prepared.unavailable
        );
        assert_eq!(prepared.servers.len(), 1);
        assert_eq!(prepared.servers[0].name(), server.name);
        assert_eq!(
            prepared.servers[0].startup_timeout(),
            Duration::from_secs(1)
        );
        assert_eq!(prepared.servers[0].tool_timeout(), Duration::from_secs(2));
        prepared.servers[0].placement().clone()
    }

    /// An `http` server keeps its protocol: SSE is Pebble's `Sse` at the
    /// configured URL, with no path of Petri's added.
    #[test]
    fn an_http_server_maps_to_pebbles_protocol_at_its_url() {
        let sse = server("legacy", McpTransport::Http {
            protocol: McpHttpProtocol::Sse,
            url:      "http://127.0.0.1:1/sse".into(),
            headers:  BTreeMap::from([("X-Case".to_owned(), McpValue::Literal("sse".into()))]),
        });
        match placement_of(&sse) {
            McpPlacement::Http {
                url,
                headers,
                protocol,
            } => {
                assert_eq!(url, "http://127.0.0.1:1/sse");
                assert_eq!(
                    headers,
                    BTreeMap::from([("X-Case".to_owned(), "sse".to_owned())])
                );
                assert_eq!(protocol, PebbleProtocol::Sse);
            }
            other => panic!("expected an http placement, got {other:?}"),
        }
        let current = server("current", McpTransport::Http {
            protocol: McpHttpProtocol::StreamableHttp,
            url:      "http://127.0.0.1:1/mcp".into(),
            headers:  BTreeMap::new(),
        });
        match placement_of(&current) {
            McpPlacement::Http { protocol, .. } => {
                assert_eq!(protocol, PebbleProtocol::StreamableHttp);
            }
            other => panic!("expected an http placement, got {other:?}"),
        }
    }

    /// A `sandbox` server is an environment placement; an SSE one serves its
    /// stream at `/sse` under the route to its port, as Fabro reaches it, and
    /// a streamable HTTP one is reached at the route itself.
    #[test]
    fn a_sandbox_server_maps_to_an_environment_placement_with_fabros_sse_path() {
        let sse = server("browser", McpTransport::Sandbox {
            protocol: McpHttpProtocol::Sse,
            command:  vec!["npx".into(), "@playwright/mcp".into()],
            port:     3100,
            env:      BTreeMap::from([("MODE".to_owned(), McpValue::Literal("test".into()))]),
        });
        match placement_of(&sse) {
            McpPlacement::Environment {
                command,
                port,
                env,
                protocol,
                path,
            } => {
                assert_eq!(command, ["npx", "@playwright/mcp"]);
                assert_eq!(port, 3100);
                assert_eq!(
                    env,
                    BTreeMap::from([("MODE".to_owned(), "test".to_owned())])
                );
                assert_eq!(protocol, PebbleProtocol::Sse);
                assert_eq!(path.as_deref(), Some(SSE_PATH));
            }
            other => panic!("expected an environment placement, got {other:?}"),
        }
        let current = server("plain", McpTransport::Sandbox {
            protocol: McpHttpProtocol::StreamableHttp,
            command:  vec!["server".into()],
            port:     3200,
            env:      BTreeMap::new(),
        });
        match placement_of(&current) {
            McpPlacement::Environment { protocol, path, .. } => {
                assert_eq!(protocol, PebbleProtocol::StreamableHttp);
                assert!(path.is_none(), "reached at the route itself: {path:?}");
            }
            other => panic!("expected an environment placement, got {other:?}"),
        }
    }
}
