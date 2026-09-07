//! Where a native agent's skills come from, as Fabro orders them.
//!
//! Fabro searches three directories, later ones overriding earlier names:
//! the configured skills directory (`$FABRO_HOME/skills`, else
//! `$HOME/.fabro/skills`), then `<root>/.fabro/skills`, then
//! `<root>/skills`, where the root is the Git root above the scope's working
//! directory, or the working directory itself outside a repository. Petri
//! resolves that list here, appends the directories a workflow names
//! (`[run.agent] skills`, a Petri extension) and hands it to Pebble
//! (`CodingAgentOptions::with_skill_dirs`). Pebble owns discovery, parsing,
//! name precedence, the prompt section, `/name` expansion and the skill
//! tool; its `SkillsDiscovered` and `SkillActivated` events reach the run
//! log in the `pebble` envelope, attributed to the node, firing and attempt.
//!
//! Fabro skips a `SKILL.md` it cannot read or parse without a word. Pebble
//! skips it too, but reports what it skipped on `SkillsDiscovered`
//! ([`skipped`]), so Petri records a [`WARNING_EVENT`] and a stderr line for
//! each and a broken skill is never a silent absence. A directory the
//! workflow named that does not exist is Petri's own diagnostic
//! ([`missing_directories`]): Pebble, like Fabro, says nothing about a
//! directory that is not there.
//!
//! Skill loading is separate from the fidelity preamble
//! (`crate::fidelity`) and from project document selection
//! (`crate::memory`): the three share nothing but the Git root probe.

use std::env;
use std::error::Error as _;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use executor::ExecEnv;
use ir::{Attempt, FiringId, StepEvent};
use pebble_coding_agent::Error as PebbleError;
use pebble_coding_agent::events::{CodingAgentEvent, CodingEvent, SkippedSkillReason};
use serde::{Deserialize, Serialize};
use serde_json::json;
use smol_str::SmolStr;
use steps::StepCtx;
use tokio::sync::mpsc;

use crate::agent::AgentConfig;
use crate::memory;

/// The `kind` of the `StepEvent::Custom` payload that records the resolved
/// directories: `{ kind, node, firing, attempt, scope, dirs: [{ path,
/// source }] }`.
pub const RESOLVED_EVENT: &str = "fabro.skills";

/// The `kind` of the `StepEvent::Custom` payload that records a skill file
/// or directory Pebble will skip: `{ kind, node, firing, attempt, reason,
/// path, message }`.
pub const WARNING_EVENT: &str = "fabro.skills.warning";

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

/// Fabro's order, then the workflow's own: lowest precedence first.
#[must_use]
pub fn order(
    configured: Option<&Path>,
    root: &str,
    workspace: &str,
    workflow: &[String],
) -> Vec<SkillDir> {
    let mut dirs = Vec::with_capacity(3 + workflow.len());
    if let Some(configured) = configured {
        dirs.push(SkillDir {
            path:   configured.to_string_lossy().into_owned(),
            source: Source::Configured,
        });
    }
    dirs.push(SkillDir {
        path:   format!("{root}/.fabro/skills"),
        source: Source::ProjectFabro,
    });
    dirs.push(SkillDir {
        path:   format!("{root}/skills"),
        source: Source::Project,
    });
    for dir in workflow {
        let path = if Path::new(dir).is_absolute() {
            dir.clone()
        } else {
            format!("{workspace}/{dir}")
        };
        dirs.push(SkillDir {
            path,
            source: Source::Workflow,
        });
    }
    dirs
}

/// The directories for the scope: the Git root is probed once in the scope,
/// the working directory stands in outside a repository.
pub async fn resolve(
    env: &dyn ExecEnv,
    home: Option<&FabroHome>,
    workflow: &[String],
) -> Vec<SkillDir> {
    let root = memory::git_root(env)
        .await
        .unwrap_or_else(|| env.workspace_path().to_owned());
    let configured = home.map(FabroHome::skills_dir);
    order(configured.as_deref(), &root, env.workspace_path(), workflow)
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

/// The directories a workflow named that do not exist, found with one shell
/// probe. A conventional directory that is absent is ordinary and silent, as
/// in Fabro; a directory the workflow asked for is a mistake worth a word.
pub async fn missing_directories(env: &dyn ExecEnv, dirs: &[SkillDir]) -> Vec<Problem> {
    let named: Vec<&SkillDir> = dirs
        .iter()
        .filter(|dir| dir.source == Source::Workflow)
        .collect();
    if named.is_empty() {
        return Vec::new();
    }
    let script = named
        .iter()
        .map(|dir| {
            let d = memory::quote(&dir.path);
            format!("if [ ! -d {d} ]; then printf 'missing\\t%s\\n' {d}; fi")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let Some(output) = memory::run(env, &script).await else {
        return Vec::new();
    };
    output
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(kind, _)| *kind == "missing")
        .map(|(_, path)| Problem {
            reason:  Reason::MissingDirectory,
            path:    path.to_owned(),
            message: "the workflow names a skills directory that does not exist".to_owned(),
        })
        .collect()
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

/// The directories a session gets, and the ones the workflow named in vain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    pub dirs:     Vec<SkillDir>,
    pub problems: Vec<Problem>,
}

impl Prepared {
    /// The paths in search order, for `with_skill_dirs`.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        self.dirs.iter().map(|dir| dir.path.clone()).collect()
    }
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
pub async fn report(logs: &mpsc::Sender<StepEvent>, at: &Attribution, problems: &[Problem]) {
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

/// Resolve the directories for one native session and record them on the
/// step's progress channel as [`RESOLVED_EVENT`], with a warning for each
/// directory the workflow named that does not exist. Pebble reports the
/// files it skipped once it has searched them ([`skipped`]).
pub async fn prepare(config: &AgentConfig, ctx: &StepCtx) -> Prepared {
    let home = ctx
        .capability::<FabroHome>()
        .map(|home| (*home).clone())
        .or_else(FabroHome::from_env);
    let dirs = resolve(ctx.env.as_ref(), home.as_ref(), &config.skill_dirs).await;
    let problems = missing_directories(ctx.env.as_ref(), &dirs).await;
    let _ = ctx
        .logs
        .send(StepEvent::Custom(json!({
            "kind": RESOLVED_EVENT,
            "node": ctx.node,
            "firing": ctx.firing,
            "attempt": ctx.attempt,
            "scope": ctx.scope,
            "dirs": dirs,
        })))
        .await;
    report(&ctx.logs, &Attribution::of(ctx), &problems).await;
    Prepared { dirs, problems }
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
    use pebble_coding_agent::SkillExpansionError;

    use super::*;

    #[test]
    fn home_follows_fabro_home_then_home() {
        let lookup = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = vars
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            move |name: &str| {
                vars.iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        assert_eq!(
            FabroHome::from_lookup(lookup(&[("FABRO_HOME", "/f"), ("HOME", "/h")])),
            Some(FabroHome(PathBuf::from("/f")))
        );
        assert_eq!(
            FabroHome::from_lookup(lookup(&[("HOME", "/h")])),
            Some(FabroHome(PathBuf::from("/h/.fabro")))
        );
        assert_eq!(FabroHome::from_lookup(lookup(&[])), None);
        assert_eq!(
            FabroHome(PathBuf::from("/f")).skills_dir(),
            PathBuf::from("/f/skills")
        );
    }

    #[test]
    fn order_is_configured_then_root_conventions_then_workflow() {
        let dirs = order(
            Some(Path::new("/home/me/.fabro/skills")),
            "/repo",
            "/repo/work",
            &["own/skills".to_owned(), "/abs/skills".to_owned()],
        );
        let paths: Vec<(&str, Source)> = dirs
            .iter()
            .map(|dir| (dir.path.as_str(), dir.source))
            .collect();
        assert_eq!(paths, [
            ("/home/me/.fabro/skills", Source::Configured),
            ("/repo/.fabro/skills", Source::ProjectFabro),
            ("/repo/skills", Source::Project),
            ("/repo/work/own/skills", Source::Workflow),
            ("/abs/skills", Source::Workflow),
        ]);
        let without_home = order(None, "/repo", "/repo", &[]);
        assert_eq!(without_home.len(), 2);
        assert_eq!(without_home[0].source, Source::ProjectFabro);
    }

    #[test]
    fn failure_class_names_a_missing_skill() {
        let error = PebbleError::SkillExpansion(SkillExpansionError::UnknownSkill {
            name: "nope".to_owned(),
        });
        assert_eq!(failure_class(&error), MISSING_CLASS);
        assert!(
            describe(&error).ends_with("Unknown skill: /nope"),
            "{}",
            describe(&error)
        );
        assert_eq!(failure_class(&PebbleError::SessionClosed), PROMPT_CLASS);
    }
}
