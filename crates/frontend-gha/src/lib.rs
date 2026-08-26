//! GitHub Actions workflow YAML → HIR.
//!
//! Pure: text in, `Graph` and diagnostics out. The only IO is reading the workflow's
//! local composite actions, and that goes through [`FileSource`] so the caller decides
//! what "the repository" is — a directory, or a map in a test.
//!
//! # What a job becomes
//!
//! ```text
//!   J/start ──► J/step-1 ──► J/step-2 ──► … ──► J/last ──► J/done ──► (each dependent's start)
//!   gate         steps, chained by plain `next:`           collector
//! ```
//!
//! `J/start` is a noop whose precondition is the job's gate: `success()` over its
//! `needs` (unless its `if:` names a status function), and its `if:`. `J/done` is a
//! noop that summarizes the job — `{ result, outputs }` — from the payload the last
//! step's edge carries, and fans out to every job that needs this one. A matrix job
//! expands the region `J/start ..= J/last` per combination; `J/done` sits outside and
//! folds the legs. Every `needs.J.*` and `success()` downstream reads `J/done`'s
//! record, never a template edge.
//!
//! Step preconditions carry GitHub's job-status semantics explicitly: `success()` in a
//! step means "no earlier step of this job failed", built as an expression over the
//! earlier steps' run-context records, and every step is additionally gated on the
//! job having started. That is how a false job `if:` skips every step, and how
//! `continue-on-error` (a `PartialSuccess`) does not fail the job.
//!
//! # Two things the engine cannot do today
//!
//! Both are reported as spec findings rather than worked around:
//!
//! - **`cancelled()` steps.** GitHub runs `if: cancelled()` and `if: always()` steps
//!   after a cancellation. The engine drops a cancelled firing's tokens, so nothing
//!   downstream of a cancelled step fires. The lowering is faithful and the step can
//!   never run.
//! - **`runs-on: ${{ matrix.os }}`.** Each matrix leg would need its own environment,
//!   and the IR has one scope per job. Rejected as `unsupported.runs_on.expression`.

pub mod composite;
pub mod expr_lower;
pub mod exprs;
pub mod lower;
pub mod model;

use frontend::{Diagnostics, Lowered};

/// Where the frontend reads a repository file from, by repository-relative path.
pub trait FileSource {
    fn read(&self, path: &str) -> Option<String>;
}

/// A repository with no readable files: every local action is missing.
pub struct NoFiles;

impl FileSource for NoFiles {
    fn read(&self, _path: &str) -> Option<String> {
        None
    }
}

/// A repository rooted at a directory.
pub struct DirFiles {
    pub root: std::path::PathBuf,
}

impl FileSource for DirFiles {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(path)).ok()
    }
}

/// An in-memory repository, for tests.
pub struct MapFiles(pub std::collections::BTreeMap<String, String>);

impl FileSource for MapFiles {
    fn read(&self, path: &str) -> Option<String> {
        self.0.get(path.trim_start_matches("./")).cloned()
    }
}

/// Parse and lower a workflow file.
pub fn load(file: &str, text: &str, files: &dyn FileSource) -> Lowered {
    let mut diags = Diagnostics::new();
    let Some(doc) = frontend::yaml::Document::parse(file, text, &mut diags) else {
        return Lowered::rejected(diags);
    };
    let Some(workflow) = model::read(&doc, &mut diags) else {
        return Lowered::rejected(diags);
    };
    lower::lower(&workflow, files, diags)
}
