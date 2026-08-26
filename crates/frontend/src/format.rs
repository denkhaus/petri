//! What every workflow format implements.
//!
//! The CLI and the corpus harness hold a list of [`Frontend`]s and ask each one
//! whether it claims a path, instead of matching on an enum. Adding a format is a
//! new crate that implements this trait, and one entry in that list.

use std::path::Path;

use crate::diag::Lowered;
use crate::files::FileSource;

/// A workflow format: text in, graph and diagnostics out.
pub trait Frontend: Send + Sync {
    /// Short, stable, lower-case and unique: `gha`, `native`. What `--format` takes.
    fn name(&self) -> &str;

    /// Whether files at `path` are, by convention, in this format.
    ///
    /// Frontends are asked in registration order, so a format that claims every path
    /// goes last.
    fn claims(&self, path: &Path) -> bool;

    /// Parse and lower one file. `file` is the name spans carry — usually the
    /// repository-relative path — and `files` resolves local includes.
    fn load(&self, file: &str, text: &str, files: &dyn FileSource) -> Lowered;
}

/// The frontend called `name`, if any.
pub fn by_name<'a>(frontends: &[&'a dyn Frontend], name: &str) -> Option<&'a dyn Frontend> {
    frontends.iter().copied().find(|f| f.name() == name)
}

/// The first frontend that claims `path`.
pub fn detect<'a>(frontends: &[&'a dyn Frontend], path: &Path) -> Option<&'a dyn Frontend> {
    frontends.iter().copied().find(|f| f.claims(path))
}
