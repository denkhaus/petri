//! Where a frontend reads a repository file from.
//!
//! Frontends are pure, but some formats have local includes — a GitHub
//! composite action under `./.github/actions/x`, say — and the frontend has to
//! read them. It does so through [`FileSource`], so the caller decides what
//! "the repository" is: a directory on disk, or a map in a test.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// Where the frontend reads a repository file from, by repository-relative
/// path.
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
    pub root: PathBuf,
}

impl FileSource for DirFiles {
    fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.root.join(path)).ok()
    }
}

/// An in-memory repository: file text keyed by repository-relative path with
/// `/` separators (`flows/workflow.toml`, `.fabro/project.toml`). A host that
/// holds a workflow bundle in memory hands one to `Runtime::check_source`; a
/// leading `./` on a requested path is ignored.
pub struct MapFiles(pub BTreeMap<String, String>);

impl FileSource for MapFiles {
    fn read(&self, path: &str) -> Option<String> {
        self.0.get(path.trim_start_matches("./")).cloned()
    }
}
