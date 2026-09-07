//! Loading `[[run.hooks]]` from the settings layers a workflow file can see,
//! and carrying the merged list in `Graph.params`.
//!
//! The standalone runner reads two layers through the `FileSource`:
//! `.fabro/project.toml` at the repository root, then `workflow.toml` beside
//! the workflow. A user-level `~/.fabro/settings.toml` is outside the
//! repository; the host supplies it with [`CompileInputs`] variables under
//! [`SETTINGS_HOOKS_VAR`] when it wants one, so lowering stays a pure
//! function of its inputs.

use frontend::{CompileInputs, Diagnostics, FileSource, Span};
use ir::Value;

pub(super) use crate::hooks::PARAM;
use crate::hooks::{self, HookDefinition, HookEvent, PROJECT_FILE, SETTINGS_HOOKS_VAR};

/// Read and merge every hook layer. `workflow_toml` is the already-read
/// `(path, text)` of the workflow's own `workflow.toml`, when it exists.
pub(super) fn load(
    files: &dyn FileSource,
    inputs: &CompileInputs,
    workflow_toml: Option<&(String, String)>,
    diags: &mut Diagnostics,
) -> Vec<HookDefinition> {
    let mut layers = Vec::with_capacity(3);
    if let Some(Value::String(text)) = inputs.vars.get(SETTINGS_HOOKS_VAR) {
        layers.push(hooks::read_layer(text, "settings.toml", diags));
    }
    if let Some(text) = files.read(PROJECT_FILE) {
        layers.push(hooks::read_layer(&text, PROJECT_FILE, diags));
    }
    if let Some((path, text)) = workflow_toml {
        layers.push(hooks::read_layer(text, path, diags));
    }
    let merged = hooks::merge(layers);
    if merged
        .iter()
        .any(|hook| hook.event == HookEvent::CheckpointSaved)
    {
        // The per-entry warning names the file; this one states the policy
        // once for the run.
        diags.warning(
            "fabro.hooks.checkpoint_saved",
            Span::file("hooks"),
            "a `checkpoint_saved` hook is configured; Petri does not write checkpoints, so it \
             is recorded as unsupported and never runs",
        );
    }
    merged
}

/// The merged list as a `Graph.params` value.
pub(super) fn param(hooks: &[HookDefinition]) -> Value {
    serde_json::to_value(hooks).unwrap_or(Value::Array(Vec::new()))
}
