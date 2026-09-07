//! The Fabro frontend: Graphviz DOT workflows → HIR.
//!
//! Text in, `Graph` and diagnostics out, exactly as the GitHub Actions
//! frontend. Fabro renders templates, resolves `@file` references and applies
//! its model stylesheet once at run creation and persists the literal graph;
//! this frontend does the same at load, so what the engine sees is literal.
//!
//! What lowers where:
//!
//! | Fabro                         | Petri                                            |
//! |-------------------------------|--------------------------------------------------|
//! | `Mdiamond` start              | `noop`, the graph entry                          |
//! | `Msquare` exit                | `noop`; `Completion::TerminalNode`               |
//! | `diamond` conditional         | `noop`                                           |
//! | `box` agent, `tab` prompt     | `fabro/agent`                                    |
//! | `parallelogram` command       | `fabro/command`                                  |
//! | `hexagon` human               | `fabro/human`                                    |
//! | `component` parallel          | fan-out groups, or `Expansion::ForEach`          |
//! | `tripleoctagon` fan-in        | `noop`, `join: all`, output = branch results     |
//! | `insulator` wait              | `fabro/wait`                                     |
//! | `house` manager loop          | `fabro/workflow`                                 |
//! | edge selection                | one `Tiered` group per node, four tiers          |
//! | `goal_gate`                   | a `goal_check` noop before exit with back arms   |
//! | `max_visits`, unlimited       | `Budget.max_firings`, capped at 500              |
//! | `loop_restart`                | `EdgeTransition::Restart`                        |
//!
//! Everything else is refused with a specific `unsupported.*` code. See
//! `FORMAT.md` for the dialect as lowered.

pub mod condition;
pub mod dot;
pub mod fidelity;
pub mod hooks;
pub mod kinds;
pub mod labels;
mod lower;
pub mod model;
pub mod stylesheet;
pub mod template;

use std::path::{Component, Path, PathBuf};

use frontend::{
    CompileInputs, Diagnostics, FileSource, Frontend, LaunchSettings, Lowered, NoFiles,
    WorkspaceRetention,
};
pub use lower::policy::{DEFAULT_SIGNATURE_LIMIT, DEFAULT_STALL_TIMEOUT};
pub use lower::{
    BRANCH_META_KIND, DEFAULT_MAX_PARALLEL, ENVIRONMENT_PARAM, EnvValue, Environment,
    FailurePolicy, IMPORT_ERROR, Kind, LAUNCH_PARAM, MAX_CALL_DEPTH, MAX_FIRINGS,
    MAX_FOR_EACH_ITEMS, MAX_INVOCATIONS, ModelDefaults, PREPARE_NODE_PREFIX, Policy, PrepareStep,
    ROUTES_KEY, RunSettings, shape_of, subagents,
};

/// Parse and lower one workflow. `file` is the repository-relative path the
/// spans carry and `@file` references resolve beside; `files` reads them.
pub fn load(file: &str, text: &str, files: &dyn FileSource, inputs: &CompileInputs) -> Lowered {
    let mut diags = Diagnostics::new();
    let dot = match dot::parse(file, text) {
        Ok(dot) => dot,
        Err(diagnostic) => {
            diags.push(diagnostic);
            return Lowered::rejected(diags);
        }
    };
    let workflow = model::build(&dot);
    lower::lower(workflow, file, files, inputs, diags)
}

/// [`load`] with no repository: every `@file` reference is missing.
pub fn load_text(file: &str, text: &str) -> Lowered {
    load(file, text, &NoFiles, &CompileInputs::new())
}

/// Fabro, as a [`Frontend`]: it claims `*.fabro` and `*.dot`.
#[derive(Debug, Default)]
pub struct Fabro;

impl Fabro {
    pub fn new() -> Self {
        Self
    }
}

impl Frontend for Fabro {
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `Frontend` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "fabro"
    }

    fn claims(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("fabro" | "dot")
        )
    }

    fn load(
        &self,
        file: &str,
        text: &str,
        files: &dyn FileSource,
        inputs: &CompileInputs,
    ) -> Lowered {
        load(file, text, files, inputs)
    }

    /// The launch settings `workflow.toml` declared, read back from the
    /// persisted graph's `fabro.launch` parameter.
    fn launch_settings(&self, graph: &ir::Graph) -> LaunchSettings {
        let Some(launch) = graph.params.get(LAUNCH_PARAM) else {
            return LaunchSettings::default();
        };
        LaunchSettings {
            sandbox_backend: launch["sandbox_backend"].as_str().map(str::to_owned),
            dry_run:         launch["dry_run"].as_bool().unwrap_or(false),
            auto_approve:    launch["auto_approve"].as_bool().unwrap_or(false),
        }
    }

    /// A Fabro run's result is the files its stages produced or changed, so a
    /// standalone run keeps its workspace after success, failure and
    /// cancellation; only an explicit `--retain never` deletes it.
    fn default_retention(&self) -> WorkspaceRetention {
        WorkspaceRetention::Always
    }

    /// The nearest ancestor holding a `.fabro` directory — the bundle root
    /// Fabro resolves `fabro/...` paths against — else the file's own
    /// directory. A file inside the bundle itself belongs to the bundle's
    /// parent, never to `.fabro`.
    fn repo_root(&self, file: &Path) -> PathBuf {
        let dir = file
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let mut current = Some(dir.as_path());
        while let Some(candidate) = current {
            if candidate.join(".fabro").is_dir() {
                return candidate.to_path_buf();
            }
            if candidate
                .components()
                .next_back()
                .is_some_and(|c| c == Component::Normal(".fabro".as_ref()))
            {
                return candidate
                    .parent()
                    .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
            }
            current = candidate.parent();
        }
        dir
    }
}
