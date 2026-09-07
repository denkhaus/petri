//! Loading `[run.agent.mcps]` from the settings layers a workflow file can
//! see, and carrying the merged list on every agent node's step config.
//!
//! The same three layers the hook loader reads (`lower::hooks`): the host's
//! `~/.fabro/settings.toml` text under [`SETTINGS_HOOKS_VAR`],
//! `.fabro/project.toml` at the repository root, then `workflow.toml` beside
//! the workflow.

use frontend::{CompileInputs, Diagnostics, FileSource};
use ir::Value;

use crate::hooks::{PROJECT_FILE, SETTINGS_HOOKS_VAR};
use crate::mcps::{self, McpServer};
use crate::template::Context;

/// Read and merge every layer. `workflow_toml` is the already-read
/// `(path, text)` of the workflow's own `workflow.toml`, when it exists.
pub(super) fn load(
    files: &dyn FileSource,
    inputs: &CompileInputs,
    workflow_toml: Option<&(String, String)>,
    template: &Context,
    diags: &mut Diagnostics,
) -> Vec<McpServer> {
    let mut layers = Vec::with_capacity(3);
    if let Some(Value::String(text)) = inputs.vars.get(SETTINGS_HOOKS_VAR) {
        layers.push(mcps::read_layer(text, "settings.toml", template, diags));
    }
    if let Some(text) = files.read(PROJECT_FILE) {
        layers.push(mcps::read_layer(&text, PROJECT_FILE, template, diags));
    }
    if let Some((path, text)) = workflow_toml {
        layers.push(mcps::read_layer(text, path, template, diags));
    }
    mcps::merge(layers)
}

/// The merged list as an agent node's `mcps` config value.
pub(super) fn param(servers: &[McpServer]) -> Value {
    serde_json::to_value(servers).unwrap_or(Value::Array(Vec::new()))
}
