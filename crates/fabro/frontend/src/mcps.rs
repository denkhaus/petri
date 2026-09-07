//! Fabro's `[run.agent.mcps.<name>]` entries: the MCP servers a native agent
//! node connects to, read from the settings layers the way Fabro reads them
//! and carried on every agent node's step config.
//!
//! The layers, lowest first: `~/.fabro/settings.toml` (the host's
//! [`SETTINGS_HOOKS_VAR`] variable), `.fabro/project.toml`, `workflow.toml`.
//! Entries are keyed by name; a higher layer's entry replaces a lower one with
//! the same name whole (Fabro's `StickyMap`), and an entry with
//! `enabled = false` in any layer removes the name. Fabro's own parser is
//! `fabro-config/src/layers/run.rs` (`McpEntryLayer`) and
//! `resolve/run.rs` (`resolve_mcp_entry`) at the pinned revision; this
//! module keeps its shape, its field rules and its defaults.
//!
//! `{{ inputs.* }}`, `{{ vars.* }}` and `{{ goal }}` substitute at load.
//! `{{ secrets.NAME }}` may stand alone as an `env` or `headers` value and
//! becomes a [`McpValue::Secret`] the step resolves when it launches the
//! server. `{{ env.* }}` is refused, as Fabro refuses it before launch.
//!
//! Tool names: a server's tool `t` is exposed to the model as
//! `mcp__<server>__<t>` ([`qualified_tool_name`]), with every character that
//! is not alphanumeric or `_` replaced by `_`, as Fabro names them.

use std::collections::BTreeMap;

use frontend::{Diagnostics, Span};
use serde::{Deserialize, Serialize};

use crate::lower::{InterpolationError, interpolate};
use crate::model::parse_duration;
use crate::template::Context;

/// The default handshake timeout, Fabro's `startup_timeout`.
pub const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;
/// The default per-call timeout, Fabro's `tool_timeout`.
pub const DEFAULT_TOOL_TIMEOUT_MS: u64 = 60_000;

/// The separator in a qualified tool name.
const TOOL_NAME_DELIMITER: &str = "__";
/// The prefix of every MCP tool name.
const TOOL_NAME_PREFIX: &str = "mcp";

/// A transport string: literal text, or a secret the run resolves when it
/// launches the server. Serialized as the string or as `{"$secret": name}`,
/// so a secret never appears in a persisted graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpValue {
    Secret {
        #[serde(rename = "$secret")]
        name: String,
    },
    Literal(String),
}

/// How the runner reaches a server: Fabro's three transports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransport {
    /// A child process on Petri's host, spoken to over stdin and stdout.
    /// `command` is the argv; a `script` entry is `["sh", "-c", script]`.
    Stdio {
        command: Vec<String>,
        #[serde(default)]
        env:     BTreeMap<String, McpValue>,
    },
    /// A streamable HTTP endpoint the host connects to.
    Http {
        url:     String,
        #[serde(default)]
        headers: BTreeMap<String, McpValue>,
    },
    /// A command launched inside the scope's execution environment that
    /// listens on `port`; the host connects to it over streamable HTTP. A
    /// `script` entry is `["bash", "-c", script]`.
    Sandbox {
        command: Vec<String>,
        port:    u16,
        #[serde(default)]
        env:     BTreeMap<String, McpValue>,
    },
}

impl McpTransport {
    /// The transport's name in events and messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
            Self::Sandbox { .. } => "sandbox",
        }
    }
}

/// One configured server, as the agent step receives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    /// The table key, and the `<server>` part of every qualified tool name.
    pub name:               String,
    pub transport:          McpTransport,
    #[serde(default = "default_startup_timeout")]
    pub startup_timeout_ms: u64,
    #[serde(default = "default_tool_timeout")]
    pub tool_timeout_ms:    u64,
    /// The settings file the entry came from, for messages.
    #[serde(default)]
    pub source:             String,
}

fn default_startup_timeout() -> u64 {
    DEFAULT_STARTUP_TIMEOUT_MS
}

fn default_tool_timeout() -> u64 {
    DEFAULT_TOOL_TIMEOUT_MS
}

/// The name the model sees for `tool` on `server`: `mcp__<server>__<tool>`,
/// each part sanitized as Fabro sanitizes it.
pub fn qualified_tool_name(server: &str, tool: &str) -> String {
    format!(
        "{TOOL_NAME_PREFIX}{TOOL_NAME_DELIMITER}{}{TOOL_NAME_DELIMITER}{}",
        sanitize_name(server),
        sanitize_name(tool)
    )
}

/// The `(server, tool)` parts of a qualified name, or `None` when the name is
/// not one. Both parts come back sanitized, as they were written.
pub fn parse_qualified_name(qualified: &str) -> Option<(String, String)> {
    let rest = qualified
        .strip_prefix(TOOL_NAME_PREFIX)?
        .strip_prefix(TOOL_NAME_DELIMITER)?;
    let index = rest.find(TOOL_NAME_DELIMITER)?;
    let server = &rest[..index];
    let tool = &rest[index + TOOL_NAME_DELIMITER.len()..];
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server.to_owned(), tool.to_owned()))
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// One layer's entries: the name and the server, or `None` for an entry the
/// layer disabled (which removes the name from lower layers too).
pub type LayerEntry = (String, Option<McpServer>);

/// Read one settings layer's `[run.agent.mcps]` table. `source` names the
/// file in messages. Every problem is a diagnostic on the file; a file that
/// names servers and cannot be read is an error, never a silent skip.
pub fn read_layer(
    text: &str,
    source: &str,
    template: &Context,
    diags: &mut Diagnostics,
) -> Vec<LayerEntry> {
    let span = Span::file(source);
    let table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(error) => {
            if text.contains("run.agent.mcps") {
                diags.error(
                    "fabro.mcps.toml",
                    span,
                    format!("`{source}` configures MCP servers but is not valid TOML: {error}"),
                );
            }
            return Vec::new();
        }
    };
    let Some(mcps) = table
        .get("run")
        .and_then(toml::Value::as_table)
        .and_then(|run| run.get("agent"))
        .and_then(toml::Value::as_table)
        .and_then(|agent| agent.get("mcps"))
    else {
        return Vec::new();
    };
    let Some(entries) = mcps.as_table() else {
        diags.error(
            "fabro.mcps.shape",
            span,
            format!("`run.agent.mcps` in `{source}` must be a table of servers keyed by name"),
        );
        return Vec::new();
    };
    let mut reader = EntryReader {
        source,
        span,
        template,
        diags,
    };
    let mut names: Vec<&String> = entries.keys().collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|name| match reader.entry(name, &entries[name]) {
            Read::Server(server) => Some((name.clone(), Some(server))),
            Read::Disabled => Some((name.clone(), None)),
            Read::Rejected => None,
        })
        .collect()
}

/// What one entry read as.
enum Read {
    Server(McpServer),
    /// `enabled = false`: the name is removed from every layer below.
    Disabled,
    /// Errors were reported, or the runner cannot serve the entry.
    Rejected,
}

/// Merge the layers, lowest first: a later entry replaces an earlier one with
/// the same name; a disabled entry removes the name. Sorted by name.
pub fn merge(layers: Vec<Vec<LayerEntry>>) -> Vec<McpServer> {
    let mut merged: BTreeMap<String, Option<McpServer>> = BTreeMap::new();
    for layer in layers {
        for (name, server) in layer {
            merged.insert(name, server);
        }
    }
    merged.into_values().flatten().collect()
}

struct EntryReader<'a> {
    source:   &'a str,
    span:     Span,
    template: &'a Context,
    diags:    &'a mut Diagnostics,
}

/// What a `script` entry runs through, per transport: `sh` on the host for
/// stdio, the scope's Bash for sandbox, as Fabro resolves them.
#[derive(Clone, Copy)]
enum Interpreter {
    HostShell,
    SandboxBash,
}

impl EntryReader<'_> {
    fn error(&mut self, code: &str, message: String) {
        self.diags.error(code, self.span.clone(), message);
    }

    fn unsupported(&mut self, feature: &str, message: String, hint: &str) {
        self.diags
            .unsupported(feature, self.span.clone(), message, hint);
    }

    fn entry(&mut self, name: &str, value: &toml::Value) -> Read {
        match self.read_entry(name, value) {
            Some(Some(server)) => Read::Server(server),
            Some(None) => Read::Disabled,
            None => Read::Rejected,
        }
    }

    /// `Some(None)` is a disabled entry, `None` an entry with errors (already
    /// reported) or a reference the runner cannot serve.
    #[expect(
        clippy::option_option,
        reason = "the inner `None` is the disabled case and `?` propagates the rejected one; \
                  the public shape is `Read`"
    )]
    fn read_entry(&mut self, name: &str, value: &toml::Value) -> Option<Option<McpServer>> {
        let source = self.source;
        let Some(table) = value.as_table() else {
            self.error(
                "fabro.mcps.entry",
                format!("`run.agent.mcps.{name}` in `{source}` must be a table"),
            );
            return None;
        };
        let has_reference = table.contains_key("id");
        let has_inline = [
            "type",
            "script",
            "command",
            "url",
            "headers",
            "port",
            "env",
            "startup_timeout",
            "tool_timeout",
        ]
        .iter()
        .any(|key| table.contains_key(*key));
        if has_reference && has_inline {
            self.error(
                "fabro.mcps.entry",
                format!(
                    "`run.agent.mcps.{name}` in `{source}` cannot mix catalog reference fields \
                     (`id`, `enabled`) with inline server fields"
                ),
            );
            return None;
        }
        let enabled = match table.get("enabled") {
            None => true,
            Some(toml::Value::Boolean(flag)) => *flag,
            Some(_) => {
                self.error(
                    "fabro.mcps.entry",
                    format!("`run.agent.mcps.{name}.enabled` in `{source}` must be a boolean"),
                );
                return None;
            }
        };
        if has_reference {
            if let Some(key) = table
                .keys()
                .find(|key| !matches!(key.as_str(), "id" | "enabled"))
            {
                self.error(
                    "fabro.mcps.entry",
                    format!(
                        "`run.agent.mcps.{name}.{key}` in `{source}` is not a field of a catalog \
                         reference (`id`, `enabled`)"
                    ),
                );
                return None;
            }
            if !enabled {
                return Some(None);
            }
            let id = table.get("id").and_then(toml::Value::as_str).unwrap_or("");
            self.unsupported(
                "workflow_toml.run.agent.mcps.reference",
                format!(
                    "`run.agent.mcps.{name}` in `{source}` references the server-managed MCP \
                     catalog entry `{id}`; the standalone runner has no Fabro server catalog"
                ),
                "write the server inline with `type = \"stdio\"`, `\"http\"` or `\"sandbox\"`",
            );
            return None;
        }
        if !enabled {
            return Some(None);
        }
        let Some(kind) = table.get("type").and_then(toml::Value::as_str) else {
            self.error(
                "fabro.mcps.type",
                format!(
                    "`run.agent.mcps.{name}` in `{source}` needs `type = \"stdio\"`, `\"http\"` \
                     or `\"sandbox\"`"
                ),
            );
            return None;
        };
        let allowed: &[&str] = match kind {
            "http" => &[
                "enabled",
                "type",
                "protocol",
                "url",
                "headers",
                "startup_timeout",
                "tool_timeout",
            ],
            "stdio" => &[
                "enabled",
                "type",
                "script",
                "command",
                "env",
                "startup_timeout",
                "tool_timeout",
            ],
            "sandbox" => &[
                "enabled",
                "type",
                "protocol",
                "script",
                "command",
                "port",
                "env",
                "startup_timeout",
                "tool_timeout",
            ],
            other => {
                self.error(
                    "fabro.mcps.type",
                    format!(
                        "`run.agent.mcps.{name}.type = \"{other}\"` in `{source}` is not a \
                         transport Fabro accepts (`stdio`, `http`, `sandbox`)"
                    ),
                );
                return None;
            }
        };
        let mut ok = true;
        for key in table.keys() {
            if !allowed.contains(&key.as_str()) {
                self.error(
                    "fabro.mcps.entry",
                    format!(
                        "`run.agent.mcps.{name}.{key}` in `{source}` is not a field of a \
                         `{kind}` server"
                    ),
                );
                ok = false;
            }
        }
        if !ok {
            return None;
        }
        if !self.protocol_supported(name, table) {
            return None;
        }
        let startup_timeout_ms =
            self.duration(name, table, "startup_timeout", DEFAULT_STARTUP_TIMEOUT_MS)?;
        let tool_timeout_ms =
            self.duration(name, table, "tool_timeout", DEFAULT_TOOL_TIMEOUT_MS)?;
        let transport = match kind {
            "http" => {
                let url = self.text(name, table, "url", true)?;
                let headers = self.values(name, table, "headers")?;
                McpTransport::Http { url, headers }
            }
            "stdio" => McpTransport::Stdio {
                command: self.command(name, table, Interpreter::HostShell)?,
                env:     self.values(name, table, "env")?,
            },
            _ => {
                let port = match table.get("port") {
                    Some(toml::Value::Integer(port)) if (1..=65_535).contains(port) => {
                        u16::try_from(*port).unwrap_or(u16::MAX)
                    }
                    _ => {
                        self.error(
                            "fabro.mcps.entry",
                            format!(
                                "`run.agent.mcps.{name}.port` in `{source}` must be an integer \
                                 from 1 to 65535"
                            ),
                        );
                        return None;
                    }
                };
                McpTransport::Sandbox {
                    command: self.command(name, table, Interpreter::SandboxBash)?,
                    port,
                    env: self.values(name, table, "env")?,
                }
            }
        };
        Some(Some(McpServer {
            name: name.to_owned(),
            transport,
            startup_timeout_ms,
            tool_timeout_ms,
            source: source.to_owned(),
        }))
    }

    /// `protocol`: `streamable_http` (the default) is served; the legacy
    /// `sse` protocol is not.
    fn protocol_supported(&mut self, name: &str, table: &toml::Table) -> bool {
        let source = self.source;
        match table.get("protocol").and_then(toml::Value::as_str) {
            None | Some("streamable_http") => true,
            Some("sse") => {
                self.unsupported(
                    "workflow_toml.run.agent.mcps.protocol",
                    format!(
                        "`run.agent.mcps.{name}` in `{source}` asks for the legacy `sse` MCP \
                         protocol, which the standalone runner does not speak"
                    ),
                    "use a server that speaks streamable HTTP (`protocol = \"streamable_http\"`)",
                );
                false
            }
            Some(other) => {
                self.error(
                    "fabro.mcps.entry",
                    format!(
                        "`run.agent.mcps.{name}.protocol = \"{other}\"` in `{source}` must be \
                         `streamable_http` or `sse`"
                    ),
                );
                false
            }
        }
    }

    fn duration(
        &mut self,
        name: &str,
        table: &toml::Table,
        key: &str,
        default: u64,
    ) -> Option<u64> {
        let source = self.source;
        match table.get(key) {
            None => Some(default),
            Some(toml::Value::String(text)) => {
                if let Some(duration) = parse_duration(text) {
                    Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
                } else {
                    self.error(
                        "fabro.mcps.entry",
                        format!(
                            "`run.agent.mcps.{name}.{key} = \"{text}\"` in `{source}` is not a \
                             duration (`10s`, `500ms`, `2m`)"
                        ),
                    );
                    None
                }
            }
            Some(_) => {
                self.error(
                    "fabro.mcps.entry",
                    format!(
                        "`run.agent.mcps.{name}.{key}` in `{source}` must be a duration string \
                         (`10s`, `500ms`, `2m`)"
                    ),
                );
                None
            }
        }
    }

    /// A required or optional string field, interpolated. A secret token is
    /// refused: only an `env` or `headers` value may carry one.
    fn text(
        &mut self,
        name: &str,
        table: &toml::Table,
        key: &str,
        required: bool,
    ) -> Option<String> {
        let source = self.source;
        match table.get(key) {
            None if required => {
                self.error(
                    "fabro.mcps.entry",
                    format!("`run.agent.mcps.{name}` in `{source}` needs `{key}`"),
                );
                None
            }
            None => Some(String::new()),
            Some(toml::Value::String(text)) => self
                .interpolate(&format!("run.agent.mcps.{name}.{key}"), text)
                .map(|value| match value {
                    McpValue::Literal(text) => text,
                    McpValue::Secret { .. } => String::new(),
                }),
            Some(_) => {
                self.error(
                    "fabro.mcps.entry",
                    format!("`run.agent.mcps.{name}.{key}` in `{source}` must be a string"),
                );
                None
            }
        }
    }

    /// Exactly one of `script` and `command`, as the argv the transport runs.
    fn command(
        &mut self,
        name: &str,
        table: &toml::Table,
        interpreter: Interpreter,
    ) -> Option<Vec<String>> {
        let source = self.source;
        match (table.get("script"), table.get("command")) {
            (Some(_), Some(_)) | (None, None) => {
                self.error(
                    "fabro.mcps.entry",
                    format!(
                        "`run.agent.mcps.{name}` in `{source}` needs exactly one of `script` and \
                         `command`"
                    ),
                );
                None
            }
            (Some(_), None) => {
                let script = self.text(name, table, "script", true)?;
                let shell = match interpreter {
                    Interpreter::HostShell => "sh",
                    Interpreter::SandboxBash => "bash",
                };
                Some(vec![shell.to_owned(), "-c".to_owned(), script])
            }
            (None, Some(toml::Value::Array(items))) => {
                let mut command = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    let Some(text) = item.as_str() else {
                        self.error(
                            "fabro.mcps.entry",
                            format!(
                                "`run.agent.mcps.{name}.command[{index}]` in `{source}` must be \
                                 a string"
                            ),
                        );
                        return None;
                    };
                    match self
                        .interpolate(&format!("run.agent.mcps.{name}.command[{index}]"), text)?
                    {
                        McpValue::Literal(text) => command.push(text),
                        McpValue::Secret { .. } => return None,
                    }
                }
                if command.is_empty() {
                    self.error(
                        "fabro.mcps.entry",
                        format!("`run.agent.mcps.{name}.command` in `{source}` must not be empty"),
                    );
                    return None;
                }
                Some(command)
            }
            (None, Some(_)) => {
                self.error(
                    "fabro.mcps.entry",
                    format!(
                        "`run.agent.mcps.{name}.command` in `{source}` must be an array of \
                         strings"
                    ),
                );
                None
            }
        }
    }

    /// An `env` or `headers` table: each value literal text or exactly one
    /// `{{ secrets.NAME }}` token.
    fn values(
        &mut self,
        name: &str,
        table: &toml::Table,
        key: &str,
    ) -> Option<BTreeMap<String, McpValue>> {
        let source = self.source;
        let mut out = BTreeMap::new();
        let Some(values) = table.get(key) else {
            return Some(out);
        };
        let Some(values) = values.as_table() else {
            self.error(
                "fabro.mcps.entry",
                format!("`run.agent.mcps.{name}.{key}` in `{source}` must be a table of strings"),
            );
            return None;
        };
        for (field, value) in values {
            let Some(text) = value.as_str() else {
                self.error(
                    "fabro.mcps.entry",
                    format!("`run.agent.mcps.{name}.{key}.{field}` in `{source}` must be a string"),
                );
                return None;
            };
            let what = format!("run.agent.mcps.{name}.{key}.{field}");
            out.insert(field.clone(), self.interpolate_value(&what, text)?);
        }
        Some(out)
    }

    /// Interpolate a transport string where a secret may not appear.
    fn interpolate(&mut self, what: &str, text: &str) -> Option<McpValue> {
        match interpolate(text, self.template, false) {
            Ok(value) => Some(McpValue::Literal(value.text)),
            Err(problem) => {
                self.report(what, problem);
                None
            }
        }
    }

    /// Interpolate a value that may be exactly one secret token.
    fn interpolate_value(&mut self, what: &str, text: &str) -> Option<McpValue> {
        match interpolate(text, self.template, true) {
            Ok(value) => Some(match value.secret {
                Some(name) => McpValue::Secret { name },
                None => McpValue::Literal(value.text),
            }),
            Err(problem) => {
                self.report(what, problem);
                None
            }
        }
    }

    fn report(&mut self, what: &str, problem: InterpolationError) {
        let source = self.source;
        match problem {
            InterpolationError::SecretNotAllowed { name } => self.unsupported(
                "workflow_toml.run.agent.mcps.secret",
                format!(
                    "`{what}` in `{source}` reads `{{{{ secrets.{name} }}}}`; the standalone \
                     runner resolves a secret only as a whole `env` or `headers` value"
                ),
                "write `KEY = \"{{ secrets.NAME }}\"` under `env` or `headers` on its own",
            ),
            InterpolationError::Env { name } => self.error(
                "fabro.mcps.env_token",
                format!(
                    "`{what}` in `{source}` reads `{{{{ env.{name} }}}}`, which Fabro refuses \
                     before launching an MCP server"
                ),
            ),
            InterpolationError::Unbound { name } => self.error(
                "fabro.mcps.unbound",
                format!("`{what}` in `{source}` reads `{{{{ {name} }}}}`, which no input binds"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use frontend::CompileInputs;

    use super::*;

    fn context() -> Context {
        Context::new(&CompileInputs::new().with_input("root", "/srv"))
    }

    fn layer(text: &str) -> (Vec<LayerEntry>, Vec<String>) {
        let mut diags = Diagnostics::new();
        let entries = read_layer(text, "workflow.toml", &context(), &mut diags);
        let codes = diags.iter().map(|d| d.code.to_string()).collect();
        (entries, codes)
    }

    #[test]
    fn qualified_names_follow_fabro() {
        assert_eq!(
            qualified_tool_name("filesystem", "read_file"),
            "mcp__filesystem__read_file"
        );
        assert_eq!(
            qualified_tool_name("my-server", "read.file"),
            "mcp__my_server__read_file"
        );
        assert_eq!(
            parse_qualified_name("mcp__my_server__read_file"),
            Some(("my_server".into(), "read_file".into()))
        );
        assert_eq!(parse_qualified_name("not_mcp__server__tool"), None);
        assert_eq!(parse_qualified_name("mcp__serveronly"), None);
        assert_eq!(parse_qualified_name("mcp____tool"), None);
    }

    #[test]
    fn every_transport_reads_with_fabros_fields_and_defaults() {
        let (entries, codes) = layer(
            r#"
[run.agent.mcps.files]
type = "stdio"
command = ["npx", "server", "{{ inputs.root }}"]
env = { TOKEN = "{{ secrets.FILES_TOKEN }}", MODE = "prod" }
startup_timeout = "15s"
tool_timeout = "2m"

[run.agent.mcps.shell]
type = "stdio"
script = "exec ./server --root {{ inputs.root }}"

[run.agent.mcps.remote]
type = "http"
url = "https://mcp.example/{{ inputs.root }}"
headers = { Authorization = "{{ secrets.REMOTE }}" }

[run.agent.mcps.browser]
type = "sandbox"
command = ["npx", "playwright-mcp", "--port", "3100"]
port = 3100
"#,
        );
        assert!(codes.is_empty(), "{codes:?}");
        let servers = merge(vec![entries]);
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["browser", "files", "remote", "shell"]);
        let files = &servers[1];
        assert_eq!(files.startup_timeout_ms, 15_000);
        assert_eq!(files.tool_timeout_ms, 120_000);
        assert_eq!(files.source, "workflow.toml");
        assert_eq!(files.transport, McpTransport::Stdio {
            command: vec!["npx".into(), "server".into(), "/srv".into()],
            env:     BTreeMap::from([
                ("MODE".to_owned(), McpValue::Literal("prod".into())),
                ("TOKEN".to_owned(), McpValue::Secret {
                    name: "FILES_TOKEN".into(),
                }),
            ]),
        });
        assert_eq!(servers[3].transport, McpTransport::Stdio {
            command: vec!["sh".into(), "-c".into(), "exec ./server --root /srv".into()],
            env:     BTreeMap::new(),
        });
        assert_eq!(servers[2].transport, McpTransport::Http {
            url:     "https://mcp.example//srv".into(),
            headers: BTreeMap::from([("Authorization".to_owned(), McpValue::Secret {
                name: "REMOTE".into(),
            })]),
        });
        assert_eq!(servers[2].startup_timeout_ms, DEFAULT_STARTUP_TIMEOUT_MS);
        assert_eq!(servers[2].tool_timeout_ms, DEFAULT_TOOL_TIMEOUT_MS);
        assert!(matches!(
            &servers[0].transport,
            McpTransport::Sandbox { command, port: 3100, .. }
                if command[0] == "npx"
        ));
        // The step config round-trips through JSON.
        let json = serde_json::to_value(&servers).expect("json");
        assert_eq!(
            json[1]["transport"]["env"]["TOKEN"]["$secret"],
            "FILES_TOKEN"
        );
        let back: Vec<McpServer> = serde_json::from_value(json).expect("back");
        assert_eq!(back, servers);
    }

    #[test]
    fn fabros_shape_rules_are_errors_and_disabled_entries_vanish() {
        let cases = [
            ("[run.agent.mcps.a]\ncommand = [\"x\"]\n", "fabro.mcps.type"),
            (
                "[run.agent.mcps.a]\ntype = \"grpc\"\ncommand = [\"x\"]\n",
                "fabro.mcps.type",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"x\"]\nurl = \"u\"\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\nscript = \"s\"\ncommand = [\"x\"]\n",
                "fabro.mcps.entry",
            ),
            ("[run.agent.mcps.a]\ntype = \"stdio\"\n", "fabro.mcps.entry"),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = []\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = \"x\"\n",
                "fabro.mcps.entry",
            ),
            ("[run.agent.mcps.a]\ntype = \"http\"\n", "fabro.mcps.entry"),
            (
                "[run.agent.mcps.a]\ntype = \"sandbox\"\ncommand = [\"x\"]\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"sandbox\"\ncommand = [\"x\"]\nport = 70000\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"x\"]\ntool_timeout = 5\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"x\"]\ntool_timeout = \"soon\"\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\nid = \"cat\"\ntype = \"stdio\"\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\nid = \"cat\"\nname = \"x\"\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\nid = \"cat\"\n",
                "unsupported.workflow_toml.run.agent.mcps.reference",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"http\"\nurl = \"u\"\nprotocol = \"sse\"\n",
                "unsupported.workflow_toml.run.agent.mcps.protocol",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"x\", \"{{ secrets.T }}\"]\n",
                "unsupported.workflow_toml.run.agent.mcps.secret",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"x\"]\nenv = { A = \"{{ env.HOME }}\" }\n",
                "fabro.mcps.env_token",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"{{ inputs.nope }}\"]\n",
                "fabro.mcps.unbound",
            ),
            ("[run.agent]\nmcps = 3\n", "fabro.mcps.shape"),
            ("[run.agent.mcps]\na = 3\n", "fabro.mcps.entry"),
            ("[run.agent.mcps.a\ntype = \"stdio\"\n", "fabro.mcps.toml"),
        ];
        for (text, code) in cases {
            let (entries, codes) = layer(text);
            assert_eq!(codes, [code], "{text}");
            assert!(entries.is_empty(), "{text}");
        }
        let (entries, codes) = layer(
            "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"x\"]\nenabled = false\n[run.agent.mcps.b]\nid = \"cat\"\nenabled = false\n",
        );
        assert!(codes.is_empty(), "{codes:?}");
        assert_eq!(entries, vec![
            ("a".to_owned(), None),
            ("b".to_owned(), None)
        ]);
        assert!(layer("[run]\ngoal = \"x\"\n").0.is_empty());
        assert!(layer("[run.agent.mcps]\n").0.is_empty());
    }

    #[test]
    fn later_layers_replace_by_name_and_can_disable() {
        let (settings, _) = layer(
            "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"low\"]\n[run.agent.mcps.b]\ntype = \"stdio\"\ncommand = [\"b\"]\n",
        );
        let (project, _) = layer("[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"high\"]\n");
        let (workflow, _) = layer(
            "[run.agent.mcps.b]\ntype = \"stdio\"\ncommand = [\"b\"]\nenabled = false\n[run.agent.mcps.c]\ntype = \"http\"\nurl = \"u\"\n",
        );
        let merged = merge(vec![settings, project, workflow]);
        let summary: Vec<(String, String)> = merged
            .iter()
            .map(|server| {
                (server.name.clone(), match &server.transport {
                    McpTransport::Stdio { command, .. } => command[0].clone(),
                    McpTransport::Http { url, .. } => url.clone(),
                    McpTransport::Sandbox { .. } => "sandbox".into(),
                })
            })
            .collect();
        assert_eq!(summary, [
            ("a".to_owned(), "high".to_owned()),
            ("c".to_owned(), "u".to_owned()),
        ]);
    }
}
