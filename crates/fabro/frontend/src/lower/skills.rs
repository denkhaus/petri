//! `[run.agent] skills`: the skill directories a workflow names, a Petri
//! extension.
//!
//! The pinned Fabro refuses the key (its `[run.agent]` denies unknown
//! fields) and takes skills from the Fabro home, the repository's
//! `.fabro/skills` and its `skills` directory alone. The standalone runner
//! accepts an ordered list of extra directories here and warns
//! `fabro.petri_extension`, so a workflow that uses it knows Fabro would not
//! load it. The list reaches agent nodes as `skill_dirs`; the native
//! backend searches it after Fabro's three directories, so a workflow's own
//! skill overrides a repository skill of the same name
//! (`fabro_steps::skills`).

use frontend::{Diagnostics, Span};
use serde_json::{Map, Value, json};

/// The agent config key the list travels under.
pub(super) const CONFIG_KEY: &str = "skill_dirs";

/// The `unsupported.*` feature of a malformed `skills` value.
pub(super) const UNSUPPORTED: &str = "workflow_toml.run.agent.skills";

/// The warning that marks the key as a Petri extension.
pub(super) const EXTENSION_WARNING: &str = "fabro.petri_extension";

/// What `[run.agent] skills` asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillSettings {
    /// Directories as written, lowest precedence first. A relative path is
    /// resolved against the scope's working directory at run time.
    pub dirs: Vec<String>,
}

/// Read the `skills` value of `[run.agent]`. A value that is not a list of
/// non-empty strings is refused; a non-empty list warns that Fabro would
/// refuse the key.
pub(super) fn read(
    value: &toml::Value,
    path: &str,
    span: &Span,
    diags: &mut Diagnostics,
) -> SkillSettings {
    let Some(entries) = value.as_array() else {
        diags.unsupported(
            UNSUPPORTED,
            span.clone(),
            format!("`run.agent.skills` in `{path}` must be a list of directory paths"),
            "write `skills = [\"path/to/skills\"]`",
        );
        return SkillSettings::default();
    };
    let mut dirs = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(dir) = entry.as_str().map(str::trim).filter(|dir| !dir.is_empty()) else {
            diags.unsupported(
                UNSUPPORTED,
                span.clone(),
                format!(
                    "every `run.agent.skills` entry in `{path}` must be a non-empty directory path"
                ),
                "write `skills = [\"path/to/skills\"]`",
            );
            return SkillSettings::default();
        };
        dirs.push(dir.to_owned());
    }
    if !dirs.is_empty() {
        diags.warning(
            EXTENSION_WARNING,
            span.clone(),
            format!(
                "`run.agent.skills` in `{path}` is a Petri extension: Fabro's `[run.agent]` \
                 refuses the key; the listed directories are searched after Fabro's own"
            ),
        );
    }
    SkillSettings { dirs }
}

/// Put the list on an agent node's config, when there is one.
pub(super) fn write(settings: &SkillSettings, config: &mut Map<String, Value>) {
    if !settings.dirs.is_empty() {
        config.insert(CONFIG_KEY.into(), json!(settings.dirs));
    }
}
