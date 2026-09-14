//! `[run.agent] skills`: the skill directories a workflow names, a Petri
//! extension.
//!
//! The pinned Fabro refuses the key (its `[run.agent]` denies unknown
//! fields) and takes skills from the Fabro home, the repository's
//! `.fabro/skills` and its `skills` directory alone. The standalone runner
//! accepts an ordered list of extra directories (read by the Fabro frontend,
//! which warns `fabro.petri_extension` so a workflow that uses it knows Fabro
//! would not load it). The list reaches agent nodes as `skill_dirs`; the native
//! backend searches it after Fabro's three directories, so a workflow's own
//! skill overrides a repository skill of the same name
//! (`attractor_steps::skills`).

use serde_json::{Map, Value, json};

/// The agent config key the list travels under.
pub(super) const CONFIG_KEY: &str = "skill_dirs";

/// What `[run.agent] skills` asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillSettings {
    /// Directories as written, lowest precedence first. A relative path is
    /// resolved against the scope's working directory at run time.
    pub dirs: Vec<String>,
}

/// Put the list on an agent node's config, when there is one.
pub(super) fn write(settings: &SkillSettings, config: &mut Map<String, Value>) {
    if !settings.dirs.is_empty() {
        config.insert(CONFIG_KEY.into(), json!(settings.dirs));
    }
}
