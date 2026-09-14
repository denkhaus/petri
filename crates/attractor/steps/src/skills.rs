//! Where a native agent's skills come from, as Fabro orders them.
//!
//! Fabro searches three directories, later ones overriding earlier names:
//! the configured skills directory (`$FABRO_HOME/skills`, else
//! `$HOME/.fabro/skills`), then `<root>/.fabro/skills`, then
//! `<root>/skills`, where the root is the Git root above the scope's working
//! directory, or the working directory itself outside a repository. Petri
//! states that order as a Pebble [`SkillDiscovery`], appends the directories
//! a workflow names (`[run.agent] skills`, a Petri extension) as required
//! searches, and Pebble does the walk: the Git root probe, the paths, the
//! discovery, parsing, name precedence, the prompt section, `/name`
//! expansion and the skill tool. Its `SkillsDiscovered` and `SkillActivated`
//! events reach the run log in the `pebble` envelope, attributed to the
//! node, firing and attempt.
//!
//! What Petri records is its own: [`RESOLVED_EVENT`] names the directories
//! Pebble searched with the convention that put each on the list, read
//! back from `SkillsDiscovered` ([`labeled`]); and every file, directory or
//! required directory Pebble skipped ([`skipped`]) is a [`WARNING_EVENT`]
//! and a stderr line, so a broken skill or a directory the workflow named in
//! vain is never a silent absence. A conventional directory that is absent
//! is ordinary and silent, as in Fabro.
//!
//! Skill loading is separate from the fidelity preamble
//! (`crate::fidelity`) and from project document selection, which Pebble's
//! `MemoryDiscovery` does for the same session.

use std::env;
use std::error::Error as _;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use ir::{Attempt, FiringId, StepEvent};
use pebble_coding_agent::events::{CodingAgentEvent, CodingEvent, SkippedSkillReason};
use pebble_coding_agent::{Error as PebbleError, SkillDiscovery};
use serde::{Deserialize, Serialize};
use serde_json::json;
use smol_str::SmolStr;
use steps::{ProgressSender, StepCtx};

use crate::agent::AgentConfig;

/// The `kind` of the `StepEvent::Custom` payload that records the resolved
/// directories: `{ kind, node, firing, attempt, scope, dirs: [{ path,
/// source }] }`.
pub const RESOLVED_EVENT: &str = "attractor.skills";

/// The `kind` of the `StepEvent::Custom` payload that records a skill file
/// or directory Pebble will skip: `{ kind, node, firing, attempt, reason,
/// path, message }`.
pub const WARNING_EVENT: &str = "attractor.skills.warning";

/// The failure class of a prompt that names a skill the session did not
/// discover (`/name` with no such skill).
pub const MISSING_CLASS: &str = "skill_missing";

/// The failure class of every other prompt failure.
const PROMPT_CLASS: &str = "pebble_prompt";

/// The environment variable naming the Fabro home, as Fabro reads it.
pub const HOME_ENV: &str = "FABRO_HOME";

/// The Fabro home directory: where the configured skills directory lives.
///
/// A host registers one as a capability to name the home explicitly; without
/// one the step reads the process environment as Fabro does (`FABRO_HOME`,
/// else `$HOME/.fabro`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FabroHome(pub PathBuf);

impl FabroHome {
    /// The home the process environment names, if any.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::from_lookup(|name| env::var_os(name))
    }

    /// Fabro's rule: `FABRO_HOME` when set, else `$HOME/.fabro`.
    #[must_use]
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Option<Self> {
        if let Some(root) = lookup(HOME_ENV) {
            return Some(Self(PathBuf::from(root)));
        }
        lookup("HOME").map(|home| Self(PathBuf::from(home).join(".fabro")))
    }

    /// The configured skills directory, `<home>/skills`.
    #[must_use]
    pub fn skills_dir(&self) -> PathBuf {
        self.0.join("skills")
    }
}

/// Which convention put a directory on the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The configured skills directory under the Fabro home.
    Configured,
    /// `<root>/.fabro/skills`.
    ProjectFabro,
    /// `<root>/skills`.
    Project,
    /// A directory the workflow named (`[run.agent] skills`).
    Workflow,
}

/// One directory Pebble searches, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDir {
    pub path:   String,
    pub source: Source,
}

/// The searches for one native session, in Fabro's order then the
/// workflow's own: Pebble resolves the Git root and the paths, and reports
/// a directory the workflow named that is not there.
#[must_use]
pub fn discovery(configured: Option<&Path>, workflow: &[String]) -> SkillDiscovery {
    let mut discovery = SkillDiscovery::new();
    if let Some(configured) = configured {
        discovery = discovery.search(configured.to_string_lossy().into_owned());
    }
    discovery = discovery
        .search_under_git_root(".fabro/skills")
        .search_under_git_root("skills");
    for dir in workflow {
        discovery = discovery.require(dir.clone());
    }
    discovery
}

/// What names the directories Pebble searched: the configured directory,
/// the workflow's own, and the working directory they resolve against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Labels {
    pub configured: Option<String>,
    pub workflow:   Vec<String>,
    pub workspace:  String,
}

impl Labels {
    /// The labels for one node's configuration.
    #[must_use]
    pub fn new(configured: Option<&Path>, workflow: &[String], workspace: &str) -> Self {
        let workspace = workspace.trim_end_matches('/').to_owned();
        Self {
            configured: configured.map(|path| path.to_string_lossy().into_owned()),
            workflow: workflow
                .iter()
                .map(|dir| {
                    if Path::new(dir).is_absolute() {
                        dir.clone()
                    } else {
                        format!("{workspace}/{}", dir.trim_start_matches("./"))
                    }
                })
                .collect(),
            workspace,
        }
    }

    /// Which convention put `path` on the list.
    #[must_use]
    pub fn source_of(&self, path: &str) -> Source {
        if self.configured.as_deref() == Some(path) {
            Source::Configured
        } else if self.workflow.iter().any(|dir| dir == path) {
            Source::Workflow
        } else if path.ends_with("/.fabro/skills") {
            Source::ProjectFabro
        } else if path.ends_with("/skills") {
            Source::Project
        } else {
            Source::Workflow
        }
    }
}

/// The directories Pebble searched, each with the convention that named it,
/// as `SkillsDiscovered` lists them: the root session's report alone, since
/// a child re-discovers the same directories.
#[must_use]
pub fn labeled(event: &CodingAgentEvent, labels: &Labels) -> Option<Vec<SkillDir>> {
    let CodingEvent::SkillsDiscovered { source_dirs, .. } = &event.event else {
        return None;
    };
    if event.parent_session_id.is_some() {
        return None;
    }
    Some(
        source_dirs
            .iter()
            .map(|path| SkillDir {
                path:   path.clone(),
                source: labels.source_of(path),
            })
            .collect(),
    )
}

/// Why Pebble will skip a file or directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// A directory the workflow named does not exist.
    MissingDirectory,
    /// A `SKILL.md` that cannot be read.
    Unreadable,
    /// A directory Pebble could not search for skills.
    Unsearchable,
    /// A `SKILL.md` Pebble's parser rejects.
    Malformed,
}

/// One skipped file or directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub reason:  Reason,
    pub path:    String,
    pub message: String,
}

/// What discovery skipped, as Pebble reported it on `SkillsDiscovered`, in
/// Petri's own words. A child session re-discovers the same directories, so
/// only the root session's report is taken and the stage says each thing once.
#[must_use]
pub fn skipped(event: &CodingAgentEvent) -> Vec<Problem> {
    let CodingEvent::SkillsDiscovered { skipped, .. } = &event.event else {
        return Vec::new();
    };
    if event.parent_session_id.is_some() {
        return Vec::new();
    }
    skipped
        .iter()
        .map(|skill| {
            let (reason, what) = match skill.reason {
                SkippedSkillReason::Malformed => {
                    (Reason::Malformed, "not a skill, so it is skipped")
                }
                SkippedSkillReason::UnreadableFile => {
                    (Reason::Unreadable, "cannot be read, so it is skipped")
                }
                SkippedSkillReason::UnsearchableDirectory => (
                    Reason::Unsearchable,
                    "cannot be searched, so its skills are skipped",
                ),
                SkippedSkillReason::MissingDirectory => (
                    Reason::MissingDirectory,
                    "the workflow names a skills directory that does not exist",
                ),
                _ => (Reason::Unreadable, "skipped by discovery"),
            };
            Problem {
                reason,
                path: skill.path.clone(),
                message: format!("{what}: {}", skill.message),
            }
        })
        .collect()
}

/// Where a warning belongs: the node, firing and attempt that own the
/// session. The step and the Pebble event sink both report through it.
#[derive(Clone, Debug)]
pub struct Attribution {
    pub node:    SmolStr,
    pub firing:  FiringId,
    pub attempt: Attempt,
}

impl Attribution {
    /// The step's own identity.
    #[must_use]
    pub fn of(ctx: &StepCtx) -> Self {
        Self {
            node:    ctx.node.clone(),
            firing:  ctx.firing,
            attempt: ctx.attempt,
        }
    }
}

/// Record one [`WARNING_EVENT`] and one stderr line per problem, so a
/// skipped skill reaches the event log and the terminal.
pub async fn report(logs: &ProgressSender, at: &Attribution, problems: &[Problem]) {
    for problem in problems {
        tracing::warn!(
            node = %at.node,
            path = %problem.path,
            reason = ?problem.reason,
            "skill skipped"
        );
        let _ = logs
            .send(StepEvent::Custom(json!({
                "kind": WARNING_EVENT,
                "node": at.node,
                "firing": at.firing,
                "attempt": at.attempt,
                "reason": problem.reason,
                "path": problem.path,
                "message": problem.message,
            })))
            .await;
        let _ = logs
            .send(StepEvent::Log {
                stream: ir::LogStream::Stderr,
                line:   format!("skills: {} {}", problem.path, problem.message),
            })
            .await;
    }
}

/// The Fabro home for a node: the host's capability, else the process
/// environment as Fabro reads it.
#[must_use]
pub fn home(ctx: &StepCtx) -> Option<FabroHome> {
    ctx.capability::<FabroHome>()
        .map(|home| (*home).clone())
        .or_else(FabroHome::from_env)
}

/// The searches and labels for one native session's configuration.
#[must_use]
pub fn for_node(config: &AgentConfig, ctx: &StepCtx) -> (SkillDiscovery, Labels) {
    let configured = home(ctx).map(|home| home.skills_dir());
    (
        discovery(configured.as_deref(), &config.skill_dirs),
        Labels::new(
            configured.as_deref(),
            &config.skill_dirs,
            ctx.env.workspace_path(),
        ),
    )
}

/// Record the directories Pebble searched as [`RESOLVED_EVENT`], attributed
/// to the node, firing and attempt.
pub async fn record_resolved(
    logs: &ProgressSender,
    at: &Attribution,
    scope: ir::ScopeId,
    dirs: &[SkillDir],
) {
    let _ = logs
        .send(StepEvent::Custom(json!({
            "kind": RESOLVED_EVENT,
            "node": at.node,
            "firing": at.firing,
            "attempt": at.attempt,
            "scope": scope,
            "dirs": dirs,
        })))
        .await;
}

/// The failure class a Pebble prompt error maps to: [`MISSING_CLASS`] when
/// the prompt named a skill the session does not have, `pebble_prompt`
/// otherwise.
#[must_use]
pub fn failure_class(error: &PebbleError) -> &'static str {
    if matches!(error, PebbleError::SkillExpansion(_)) {
        MISSING_CLASS
    } else {
        PROMPT_CLASS
    }
}

/// A Pebble error with its causes, `: `-joined, so a failure reason names
/// the skill (`expanding a skill reference: Unknown skill: /name`).
#[must_use]
pub fn describe(error: &PebbleError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use pebble_coding_agent::SkillSearch;

    use super::*;

    #[test]
    fn fabro_home_follows_fabros_rule() {
        let explicit =
            FabroHome::from_lookup(|name| (name == HOME_ENV).then(|| OsString::from("/explicit")))
                .expect("home");
        assert_eq!(explicit.skills_dir(), PathBuf::from("/explicit/skills"));
        let implicit =
            FabroHome::from_lookup(|name| (name == "HOME").then(|| OsString::from("/home/ada")))
                .expect("home");
        assert_eq!(
            implicit.skills_dir(),
            PathBuf::from("/home/ada/.fabro/skills")
        );
    }

    #[test]
    fn labels_name_the_convention_behind_each_directory() {
        let labels = Labels::new(
            Some(Path::new("/home/ada/.fabro/skills")),
            &["extra/skills".to_owned(), "/abs/skills".to_owned()],
            "/repo/",
        );
        assert_eq!(
            labels.source_of("/home/ada/.fabro/skills"),
            Source::Configured
        );
        assert_eq!(
            labels.source_of("/repo/.fabro/skills"),
            Source::ProjectFabro
        );
        assert_eq!(labels.source_of("/repo/skills"), Source::Project);
        assert_eq!(labels.source_of("/repo/extra/skills"), Source::Workflow);
        assert_eq!(labels.source_of("/abs/skills"), Source::Workflow);
    }

    #[test]
    fn the_discovery_states_fabros_order_then_the_workflows() {
        let discovery = discovery(Some(Path::new("/h/skills")), &["mine".to_owned()]);
        let paths: Vec<&str> = discovery.searches().iter().map(SkillSearch::path).collect();
        assert_eq!(paths, ["/h/skills", ".fabro/skills", "skills", "mine"]);
        assert!(discovery.searches()[3].is_required());
        assert!(!discovery.searches()[2].is_required());
    }
}
