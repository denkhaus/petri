//! What every workflow format implements.
//!
//! The CLI and the corpus harness hold a list of [`Frontend`]s and ask each one
//! whether it claims a path, instead of matching on an enum. Adding a format is
//! a new crate that implements this trait, and one entry in that list.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use smol_str::SmolStr;

use crate::diag::Lowered;
use crate::files::FileSource;

/// What a host supplies *before* lowering: the run inputs and variables a
/// format renders into its file at compile time (Fabro's `{{ inputs.* }}`
/// and `{{ vars.* }}`). Distinct from [`Frontend::default_params`], which
/// fills `Graph.params` *after* lowering and never changes the graph's
/// shape. A format that renders nothing ignores it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompileInputs {
    pub inputs:             BTreeMap<SmolStr, Value>,
    pub vars:               BTreeMap<SmolStr, Value>,
    /// Whether a template that reads an input no one supplied is a warning
    /// that leaves the text unrendered, instead of an error. `petri check`
    /// sets this when it was given no inputs at all, so a workflow validates
    /// before its inputs exist. A run never sets it.
    pub unbound_is_warning: bool,
}

impl CompileInputs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Report an unbound input as a warning and leave its text unrendered.
    #[must_use]
    pub fn with_unbound_as_warning(mut self) -> Self {
        self.unbound_is_warning = true;
        self
    }

    #[must_use]
    pub fn with_input(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.inputs.insert(SmolStr::new(name), value.into());
        self
    }

    #[must_use]
    pub fn with_var(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.vars.insert(SmolStr::new(name), value.into());
        self
    }

    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty() && self.vars.is_empty()
    }
}

/// A workflow format: text in, graph and diagnostics out.
pub trait Frontend: Send + Sync {
    /// Short, stable, lower-case and unique: `gha`, `native`. What `--format`
    /// takes.
    fn name(&self) -> &str;

    /// Whether files at `path` are, by convention, in this format.
    ///
    /// Frontends are asked in registration order, so a format that claims every
    /// path goes last.
    fn claims(&self, path: &Path) -> bool;

    /// Parse and lower one file. `file` is the name spans carry — usually the
    /// repository-relative path — `files` resolves local includes, and
    /// `inputs` carries what the host supplies before lowering.
    fn load(
        &self,
        file: &str,
        text: &str,
        files: &dyn FileSource,
        inputs: &CompileInputs,
    ) -> Lowered;

    /// Run parameters a host owes this format when it has nothing better: fixed
    /// values, so lowering the same file twice yields the identical graph and a
    /// saved log replays against it. `repo` is the repository root. Default:
    /// none.
    fn default_params(&self, _repo: &Path) -> Vec<(SmolStr, Value)> {
        Vec::new()
    }

    /// Where the repository root is above `file`, when the host was not told. A
    /// format whose files sit at a fixed place in a repository walks up to it;
    /// the default is the file's own directory.
    ///
    /// It is the root local includes resolve against, and the prefix stripped
    /// from the name spans carry.
    fn repo_root(&self, file: &Path) -> PathBuf {
        file.parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    }
}

/// The frontend called `name`, if any.
pub fn by_name<'a>(frontends: &[&'a dyn Frontend], name: &str) -> Option<&'a dyn Frontend> {
    frontends.iter().copied().find(|f| f.name() == name)
}

/// The first frontend that claims `path`.
pub fn detect<'a>(frontends: &[&'a dyn Frontend], path: &Path) -> Option<&'a dyn Frontend> {
    frontends.iter().copied().find(|f| f.claims(path))
}
