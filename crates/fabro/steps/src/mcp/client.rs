//! One server's process and connection, through the `rmcp` client.
//!
//! [`Connection::start`] resolves the transport's secrets, launches or
//! reaches the server, performs the MCP handshake within the startup timeout
//! and lists its tools. [`Connection::call`] forwards one call with the
//! server's tool timeout and the caller's cancellation, sending the protocol's
//! `notifications/cancelled` when either ends the wait. [`Connection::close`]
//! ends the session, stops the process the connection owns and releases the
//! route it opened to a `sandbox` server's port.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use executor::{ExecEnv, OutputMode, PreviewUrl, ProcessHandle, ProcessSpec, SecretProvider, Sig};
use frontend_fabro::mcps::{McpServer, McpTransport, McpValue};
use ir::Value;
use reqwest::header::{CONNECTION, HeaderMap, HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotification,
    CancelledNotificationParam, ClientCapabilities, ClientInfo, ClientRequest, Implementation,
    ProtocolVersion, RawContent, RequestId, ServerResult,
};
use rmcp::service::{
    Peer, PeerRequestOptions, RequestHandle, RoleClient, RunningService, ServiceError,
    serve_client_with_ct,
};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::Map;
use smol_str::SmolStr;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

/// How much of a server's own error output is kept for a failure message.
const STDERR_TAIL_BYTES: usize = 4096;
/// How often a `sandbox` server's port is probed while it starts.
const PORT_POLL: Duration = Duration::from_millis(100);
/// How long one readiness probe of a `sandbox` server's route may take. A
/// forward into a container accepts the connection before anything listens
/// inside, so readiness is an HTTP answer, not an accepted connection.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a stopped process gets after `SIGKILL`.
const KILL_WAIT: Duration = Duration::from_secs(5);

/// One tool the server advertised.
pub(super) struct DiscoveredTool {
    pub(super) name:         String,
    pub(super) description:  String,
    pub(super) input_schema: Value,
}

/// How one forwarded call ended.
pub(super) enum CallOutcome {
    /// The server answered; the text the model reads.
    Ok(String),
    /// The server answered with `isError`; the text the model reads as the
    /// tool's error.
    ToolError(String),
    /// The call did not reach the server or came back malformed.
    Failed(String),
    /// The server did not answer within the tool timeout.
    Timeout(Duration),
    /// The caller cancelled the wait.
    Cancelled,
}

/// Why a server did not start.
#[derive(Debug, thiserror::Error)]
pub(super) enum StartError {
    #[error("secret `{name}` for `{field}` is unavailable: {reason}")]
    Secret {
        name:   String,
        field:  String,
        reason: String,
    },
    #[error("could not launch `{program}`: {reason}")]
    Launch { program: String, reason: String },
    #[error("the server did not complete the MCP handshake within {}s{tail}", timeout.as_secs())]
    HandshakeTimeout { timeout: Duration, tail: String },
    #[error("the MCP handshake failed: {reason}{tail}")]
    Handshake { reason: String, tail: String },
    #[error("listing the server's tools failed: {reason}")]
    ListTools { reason: String },
    #[error("no route from Petri to port {port} in the scope's execution environment: {reason}")]
    Route { port: u16, reason: String },
    #[error("{0}")]
    Unsupported(String),
    #[error("the server exited while starting (status {status}){tail}")]
    Exited { status: String, tail: String },
    #[error("the session was cancelled while the server started")]
    Cancelled,
}

/// The last bytes a server wrote to its error output.
#[derive(Default)]
struct Tail(VecDeque<u8>);

impl Tail {
    fn push(&mut self, bytes: &[u8]) {
        self.0.extend(bytes);
        let excess = self.0.len().saturating_sub(STDERR_TAIL_BYTES);
        self.0.drain(..excess);
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.iter().copied().collect::<Vec<u8>>()).into_owned()
    }
}

type SharedTail = Arc<StdMutex<Tail>>;

fn tail_suffix(tail: Option<&SharedTail>) -> String {
    let text = tail
        .map(|tail| tail.lock().unwrap_or_else(PoisonError::into_inner).text())
        .unwrap_or_default();
    let text = text.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!("; the server wrote: {text}")
    }
}

/// The route the scope's execution environment opened from Petri to a
/// `sandbox` server's port, released when the connection closes.
struct Route {
    env:  Arc<dyn ExecEnv>,
    port: u16,
}

impl Route {
    /// Open the route and answer with its address. `Ok(None)` from the
    /// environment is a named refusal: this environment offers no way to
    /// reach its ports.
    async fn open(
        env: &Arc<dyn ExecEnv>,
        port: u16,
        server: &str,
    ) -> Result<(Self, PreviewUrl), StartError> {
        let preview = match env.preview_url(port).await {
            Ok(Some(preview)) => preview,
            Ok(None) => {
                return Err(StartError::Route {
                    port,
                    reason: format!(
                        "the sandbox provider offers no preview URL, so `{server}` cannot be \
                         reached from Petri"
                    ),
                });
            }
            Err(error) => {
                return Err(StartError::Route {
                    port,
                    reason: error.to_string(),
                });
            }
        };
        Ok((
            Self {
                env: Arc::clone(env),
                port,
            },
            preview,
        ))
    }

    /// End the route. Best effort: the scope's release closes it anyway, so
    /// a failure is logged and nothing else.
    async fn release(&self) {
        if let Err(error) = self.env.release_preview_url(self.port).await {
            tracing::warn!(port = self.port, error = %error, "releasing the MCP server's route failed");
        }
    }
}

async fn release_route(route: Option<&Route>) {
    if let Some(route) = route {
        route.release().await;
    }
}

/// The live connection to one server.
pub(super) struct Connection {
    peer:         Peer<RoleClient>,
    service:      Mutex<Option<RunningService<RoleClient, ClientInfo>>>,
    tool_timeout: Duration,
    /// The process a `sandbox` server runs in, launched in the scope.
    process:      Mutex<Option<Box<dyn ProcessHandle>>>,
    /// The route to a `sandbox` server's port.
    route:        Option<Route>,
    grace:        Duration,
    disconnected: AtomicBool,
    reported:     AtomicBool,
}

impl Connection {
    /// Launch or reach the server, complete the handshake and list its tools.
    pub(super) async fn start(
        server: &McpServer,
        env: &Arc<dyn ExecEnv>,
        secrets: &dyn SecretProvider,
        cancel: &CancellationToken,
    ) -> Result<(Self, Vec<DiscoveredTool>), StartError> {
        let startup = Duration::from_millis(server.startup_timeout_ms);
        let tool_timeout = Duration::from_millis(server.tool_timeout_ms);
        let info = ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("petri", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(ProtocolVersion::V_2025_03_26);
        let mut process: Option<Box<dyn ProcessHandle>> = None;
        let mut route: Option<Route> = None;
        let mut tail: Option<SharedTail> = None;
        let began = Instant::now();
        let handshake = match &server.transport {
            McpTransport::Stdio { command, env: vars } => {
                let vars = resolve_values(vars, secrets, "env")?;
                let (program, args) = command.split_first().ok_or_else(|| StartError::Launch {
                    program: String::new(),
                    reason:  "the command is empty".into(),
                })?;
                let mut cmd = Command::new(program);
                cmd.args(args).envs(vars).kill_on_drop(true);
                if env.shares_host_filesystem() {
                    cmd.current_dir(env.workspace_path());
                }
                #[cfg(unix)]
                cmd.process_group(0);
                let (transport, stderr) = TokioChildProcess::builder(cmd)
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|error| StartError::Launch {
                        program: program.clone(),
                        reason:  error.to_string(),
                    })?;
                let shared: SharedTail = Arc::default();
                if let Some(stderr) = stderr {
                    let shared = Arc::clone(&shared);
                    let name = server.name.clone();
                    // Disposable: the server's own diagnostics, kept for a
                    // failure message and traced; the child owns the pipe.
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            tracing::debug!(server = %name, line = %line, "MCP server stderr");
                            let mut tail = shared.lock().unwrap_or_else(PoisonError::into_inner);
                            tail.push(line.as_bytes());
                            tail.push(b"\n");
                        }
                    });
                }
                tail = Some(shared);
                timeout(
                    startup,
                    serve_client_with_ct(info, transport, cancel.child_token()),
                )
                .await
            }
            McpTransport::Http { url, headers } => {
                let headers = resolve_values(headers, secrets, "headers")?;
                let transport = http_transport(url, &headers)?;
                timeout(
                    startup,
                    serve_client_with_ct(info, transport, cancel.child_token()),
                )
                .await
            }
            McpTransport::Sandbox {
                command,
                port,
                env: vars,
            } => {
                let vars = resolve_values(vars, secrets, "env")?;
                let (program, args) = command.split_first().ok_or_else(|| StartError::Launch {
                    program: String::new(),
                    reason:  "the command is empty".into(),
                })?;
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let spec = ProcessSpec::new(program, &args)
                    .with_env(
                        vars.into_iter()
                            .map(|(key, value)| (SmolStr::new(key), SmolStr::new(value)))
                            .collect(),
                    )
                    .with_output(OutputMode::Lines);
                let mut handle = env.spawn(spec).await.map_err(|error| StartError::Launch {
                    program: program.clone(),
                    reason:  error.to_string(),
                })?;
                let shared: SharedTail = Arc::default();
                if let Some(mut lines) = handle.lines() {
                    let shared = Arc::clone(&shared);
                    let name = server.name.clone();
                    tokio::spawn(async move {
                        while let Some(line) = lines.recv().await {
                            tracing::debug!(server = %name, line = %line.line, "MCP server output");
                            let mut tail = shared.lock().unwrap_or_else(PoisonError::into_inner);
                            tail.push(line.line.as_bytes());
                            tail.push(b"\n");
                        }
                    });
                }
                tail = Some(shared);
                // The address Petri reaches the port at is the environment's
                // route to it: the host's own loopback, the Docker plugin's
                // forward into the container, Daytona's preview link. Fabro
                // polls the port for the startup window and then connects;
                // Petri polls the route with a request, since a forward
                // accepts a connection before the port inside answers.
                let (opened, preview) = match Route::open(env, *port, &server.name).await {
                    Ok(opened) => opened,
                    Err(error) => {
                        stop_process(handle.as_mut(), env.grace()).await;
                        return Err(error);
                    }
                };
                let mut headers: HeaderMap = header_map(&preview.headers)?.into_iter().collect();
                // Each probe closes its connection: a single-threaded server
                // (or a forward's bridge) must be free for the handshake.
                headers.insert(CONNECTION, HeaderValue::from_static("close"));
                let probe = reqwest::Client::builder()
                    .pool_max_idle_per_host(0)
                    .timeout(PROBE_TIMEOUT)
                    .build()
                    .map_err(|error| StartError::Route {
                        port:   *port,
                        reason: format!("building the readiness probe: {error}"),
                    })?;
                let deadline = began + startup;
                loop {
                    if cancel.is_cancelled() {
                        stop_process(handle.as_mut(), env.grace()).await;
                        opened.release().await;
                        return Err(StartError::Cancelled);
                    }
                    if probe
                        .get(&preview.url)
                        .headers(headers.clone())
                        .send()
                        .await
                        .is_ok()
                    {
                        break;
                    }
                    if Instant::now() >= deadline {
                        stop_process(handle.as_mut(), env.grace()).await;
                        opened.release().await;
                        return Err(StartError::HandshakeTimeout {
                            timeout: startup,
                            tail:    tail_suffix(tail.as_ref()),
                        });
                    }
                    if let Ok(status) = timeout(PORT_POLL, handle.wait()).await {
                        let status = status
                            .map_or_else(|error| error.to_string(), |status| format!("{status:?}"));
                        opened.release().await;
                        return Err(StartError::Exited {
                            status,
                            tail: tail_suffix(tail.as_ref()),
                        });
                    }
                }
                drop(probe);
                process = Some(handle);
                let transport = http_transport(&preview.url, &preview.headers)?;
                route = Some(opened);
                let remaining = deadline.saturating_duration_since(Instant::now());
                timeout(
                    remaining,
                    serve_client_with_ct(info, transport, cancel.child_token()),
                )
                .await
            }
        };
        let service = match handshake {
            Ok(Ok(service)) => service,
            Ok(Err(error)) => {
                if let Some(mut handle) = process {
                    stop_process(handle.as_mut(), env.grace()).await;
                }
                release_route(route.as_ref()).await;
                if cancel.is_cancelled() {
                    return Err(StartError::Cancelled);
                }
                return Err(StartError::Handshake {
                    reason: error.to_string(),
                    tail:   tail_suffix(tail.as_ref()),
                });
            }
            Err(_) => {
                if let Some(mut handle) = process {
                    stop_process(handle.as_mut(), env.grace()).await;
                }
                release_route(route.as_ref()).await;
                return Err(StartError::HandshakeTimeout {
                    timeout: startup,
                    tail:    tail_suffix(tail.as_ref()),
                });
            }
        };
        if let Some(peer_info) = service.peer().peer_info() {
            tracing::info!(
                server = %server.name,
                server_name = %peer_info.server_info.name,
                server_version = %peer_info.server_info.version,
                "MCP server initialized"
            );
        }
        let tools = match timeout(tool_timeout, service.list_all_tools()).await {
            Ok(Ok(tools)) => tools,
            Ok(Err(error)) => {
                if let Some(mut handle) = process {
                    stop_process(handle.as_mut(), env.grace()).await;
                }
                release_route(route.as_ref()).await;
                return Err(StartError::ListTools {
                    reason: error.to_string(),
                });
            }
            Err(_) => {
                if let Some(mut handle) = process {
                    stop_process(handle.as_mut(), env.grace()).await;
                }
                release_route(route.as_ref()).await;
                return Err(StartError::ListTools {
                    reason: format!("no answer within {}s", tool_timeout.as_secs()),
                });
            }
        };
        let tools = tools
            .into_iter()
            .map(|tool| DiscoveredTool {
                name:         tool.name.to_string(),
                description:  tool.description.as_deref().unwrap_or("").to_owned(),
                input_schema: serde_json::to_value(&*tool.input_schema).unwrap_or_default(),
            })
            .collect();
        Ok((
            Self {
                peer: service.peer().clone(),
                service: Mutex::new(Some(service)),
                tool_timeout,
                process: Mutex::new(process),
                route,
                grace: env.grace(),
                disconnected: AtomicBool::new(false),
                reported: AtomicBool::new(false),
            },
            tools,
        ))
    }

    /// Forward one call. The server's tool timeout and the caller's
    /// cancellation both end the wait with a `notifications/cancelled` to the
    /// server.
    pub(super) async fn call(
        &self,
        tool: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> CallOutcome {
        if self.disconnected.load(Ordering::Acquire) {
            return CallOutcome::Failed("the server's connection is closed".into());
        }
        let arguments: Option<Map<String, Value>> = match arguments {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                return CallOutcome::Failed(format!(
                    "MCP tool arguments must be a JSON object, got {other}"
                ));
            }
        };
        let mut params = CallToolRequestParams::new(tool.to_owned());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let handle = match self
            .peer
            .send_cancellable_request(request, PeerRequestOptions::no_options())
            .await
        {
            Ok(handle) => handle,
            Err(error) => return self.failed(&error),
        };
        let RequestHandle { rx, id, peer, .. } = handle;
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                notify_cancelled(&peer, id, "cancelled by the agent").await;
                CallOutcome::Cancelled
            }
            () = sleep(self.tool_timeout) => {
                notify_cancelled(&peer, id, RequestHandle::<RoleClient>::REQUEST_TIMEOUT_REASON).await;
                CallOutcome::Timeout(self.tool_timeout)
            }
            response = rx => match response {
                Ok(Ok(ServerResult::CallToolResult(result))) => outcome_of(&result),
                Ok(Ok(_)) => CallOutcome::Failed("the server answered with an unexpected response type".into()),
                Ok(Err(error)) => self.failed(&error),
                Err(_) => self.failed(&ServiceError::TransportClosed),
            }
        }
    }

    fn failed(&self, error: &ServiceError) -> CallOutcome {
        if matches!(
            error,
            ServiceError::TransportClosed | ServiceError::TransportSend(_)
        ) {
            self.disconnected.store(true, Ordering::Release);
        }
        CallOutcome::Failed(error.to_string())
    }

    /// Whether the connection is gone and nobody has reported it yet: true
    /// once, for the `disconnected` event.
    pub(super) fn take_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire) && !self.reported.swap(true, Ordering::AcqRel)
    }

    /// End the session, stop the owned process and release the route to its
    /// port. Bounded by `limit` for the protocol's close and by the scope's
    /// grace for the process.
    pub(super) async fn close(&self, limit: Duration) {
        let service = self.service.lock().await.take();
        if let Some(mut service) = service {
            match timeout(limit + KILL_WAIT, service.close_with_timeout(limit)).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::warn!(error = %error, "MCP client did not close cleanly");
                }
                Err(_) => {
                    tracing::warn!("MCP client did not close in time");
                }
            }
            // Dropping the service drops the transport: a child process is
            // killed on drop when it did not exit on its closed stdin.
            drop(service);
        }
        let process = self.process.lock().await.take();
        if let Some(mut handle) = process {
            stop_process(handle.as_mut(), self.grace).await;
        }
        release_route(self.route.as_ref()).await;
    }
}

/// `SIGTERM`, the grace period, then `SIGKILL`.
async fn stop_process(process: &mut dyn ProcessHandle, grace: Duration) {
    let _ = process.signal(Sig::Term).await;
    if timeout(grace, process.wait()).await.is_ok() {
        return;
    }
    let _ = process.signal(Sig::Kill).await;
    let _ = timeout(KILL_WAIT, process.wait()).await;
}

async fn notify_cancelled(peer: &Peer<RoleClient>, id: RequestId, reason: &str) {
    let notification = CancelledNotification::new(CancelledNotificationParam {
        request_id: id,
        reason:     Some(reason.to_owned()),
    });
    let _ = peer.send_notification(notification.into()).await;
}

/// Fabro's `call_result_to_string`: the text parts joined by newlines, a
/// placeholder for every other part; `isError` makes it the tool's error.
fn outcome_of(result: &CallToolResult) -> CallOutcome {
    let text = result
        .content
        .iter()
        .map(|part| match &part.raw {
            RawContent::Text(text) => text.text.clone(),
            RawContent::Image(_) => "[image content]".to_owned(),
            RawContent::Audio(_) => "[audio content]".to_owned(),
            RawContent::Resource(_) | RawContent::ResourceLink(_) => {
                "[resource content]".to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if result.is_error.unwrap_or(false) {
        CallOutcome::ToolError(text)
    } else {
        CallOutcome::Ok(text)
    }
}

/// Resolve `{"$secret": name}` values through the run's secret provider, at
/// launch and nowhere else; the resolved values go into the process or the
/// request and are never written down.
fn resolve_values(
    values: &BTreeMap<String, McpValue>,
    secrets: &dyn SecretProvider,
    field: &str,
) -> Result<BTreeMap<String, String>, StartError> {
    let mut out = BTreeMap::new();
    for (key, value) in values {
        let text = match value {
            McpValue::Literal(text) => text.clone(),
            McpValue::Secret { name } => secrets
                .resolve(name)
                .map_err(|error| StartError::Secret {
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

/// The configured (or route-provided) headers, parsed, in the shape the
/// `rmcp` transport config takes.
fn header_map(
    headers: &BTreeMap<String, String>,
) -> Result<HashMap<HeaderName, HeaderValue>, StartError> {
    let mut map = HashMap::new();
    for (key, value) in headers {
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(|error| {
            StartError::Unsupported(format!("invalid header name `{key}`: {error}"))
        })?;
        let value = HeaderValue::from_str(value).map_err(|error| {
            StartError::Unsupported(format!("invalid header value for `{key}`: {error}"))
        })?;
        map.insert(name, value);
    }
    Ok(map)
}

fn http_transport(
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<StreamableHttpClientTransport<reqwest::Client>, StartError> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_owned());
    config.custom_headers = header_map(headers)?;
    Ok(StreamableHttpClientTransport::with_client(
        reqwest::Client::new(),
        config,
    ))
}
