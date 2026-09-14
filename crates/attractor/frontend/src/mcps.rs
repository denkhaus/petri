//! The MCP servers a native agent node connects to, as resolved definitions
//! carried on every agent node's step config.
//!
//! Where the definitions come from is the settings layer's business: the
//! Fabro frontend reads `[run.agent.mcps.<name>]` from its settings files,
//! interpolates the values, and merges the layers into the list on
//! [`RunSettings::mcps`](crate::RunSettings). A `{{ secrets.NAME }}` value
//! arrives as a [`McpValue::Secret`] the step resolves when it launches the
//! server; a persisted graph never holds the secret.
//!
//! Tool names: a server's tool `t` is exposed to the model as
//! `mcp__<server>__<t>` ([`qualified_tool_name`]), with every character that
//! is not alphanumeric or `_` replaced by `_`, as Fabro names them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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

/// Which HTTP transport an `http` or `sandbox` server speaks: Fabro's
/// `protocol` field, `streamable_http` unless the entry says `sse`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpHttpProtocol {
    /// The current transport: JSON-RPC over POST, with optional server
    /// streams.
    #[default]
    StreamableHttp,
    /// The older transport: one server-sent event stream that names the
    /// endpoint messages are posted to.
    Sse,
}

impl McpHttpProtocol {
    /// The protocol's name in the entry and in messages.
    pub fn kind(self) -> &'static str {
        match self {
            Self::StreamableHttp => "streamable_http",
            Self::Sse => "sse",
        }
    }
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
    /// An HTTP endpoint the host connects to, over `protocol`.
    Http {
        #[serde(default)]
        protocol: McpHttpProtocol,
        url:      String,
        #[serde(default)]
        headers:  BTreeMap<String, McpValue>,
    },
    /// A command launched inside the scope's execution environment that
    /// listens on `port`; the host connects to it over `protocol` through
    /// the environment's route to the port. A `script` entry is
    /// `["bash", "-c", script]`.
    Sandbox {
        #[serde(default)]
        protocol: McpHttpProtocol,
        command:  Vec<String>,
        port:     u16,
        #[serde(default)]
        env:      BTreeMap<String, McpValue>,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
