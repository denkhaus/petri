//! `[run.model]` from the layers below `workflow.toml`: the host's user
//! settings (`~/.fabro/settings.toml`, passed as the `fabro.settings_toml`
//! variable) and `.fabro/project.toml` at the repository root. Fabro's
//! settings layer carries a `[run]` table like the other two, and its
//! `combine` merges them settings, then project, then workflow, a higher
//! layer's value replacing a lower one's. Only the keys `workflow.toml` did
//! not set are filled here, so the workflow's own values stay in charge.
//!
//! This is how a bundle that declares no model, such as the pinned
//! interview workflow, gets one: the operator's settings name it, as they
//! do for a Fabro server.

use frontend::{CompileInputs, Diagnostics, FileSource, Span};
use serde_json::Value;

use super::workflow_toml::ModelDefaults;
use crate::hooks::{PROJECT_FILE, SETTINGS_HOOKS_VAR};

/// Fill `model`'s unset keys from the project layer, then the settings
/// layer.
pub(super) fn apply(
    files: &dyn FileSource,
    inputs: &CompileInputs,
    model: &mut ModelDefaults,
    diags: &mut Diagnostics,
) {
    if let Some(text) = files.read(PROJECT_FILE) {
        fill(&text, PROJECT_FILE, model, diags);
    }
    if let Some(Value::String(text)) = inputs.vars.get(SETTINGS_HOOKS_VAR) {
        fill(text, "settings.toml", model, diags);
    }
}

fn fill(text: &str, path: &str, model: &mut ModelDefaults, diags: &mut Diagnostics) {
    let table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(error) => {
            diags.warning(
                "fabro.workflow_toml",
                Span::file(path),
                format!("`{path}` is not valid TOML; its `[run.model]` is ignored: {error}"),
            );
            return;
        }
    };
    let Some(section) = table
        .get("run")
        .and_then(toml::Value::as_table)
        .and_then(|run| run.get("model"))
        .and_then(toml::Value::as_table)
    else {
        return;
    };
    let text_of = |key: &str| {
        section
            .get(key)
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    };
    if model.provider.is_none() {
        model.provider = text_of("provider");
    }
    if model.name.is_none() {
        model.name = text_of("name");
    }
    let controls = section.get("controls").and_then(toml::Value::as_table);
    let control_of = |key: &str| {
        controls
            .and_then(|c| c.get(key))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    };
    if model.reasoning_effort.is_none() {
        model.reasoning_effort = control_of("reasoning_effort");
    }
    if model.speed.is_none() {
        model.speed = control_of("speed");
    }
    if model.fallbacks.is_empty()
        && let Some(fallbacks) = section.get("fallbacks")
    {
        model.fallbacks = super::fallbacks::read(path, diags, fallbacks);
    }
}
