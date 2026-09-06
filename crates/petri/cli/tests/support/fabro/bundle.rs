//! Scenario staging: copy a scenario directory from the Fabro acceptance tree
//! into a fresh, self-deleting directory so a run can never write into the
//! source tree.

use std::path::{Path, PathBuf};
use std::{fs, io};

use testkit::RunDir;

/// Where the tracked scenarios live, relative to this crate's manifest.
const SCENARIOS: &str = "../../fabro/acceptance/scenarios";

/// One staged scenario: its files copied under a run directory that is
/// removed when the value drops.
pub(crate) struct Scenario {
    dir:  RunDir,
    root: PathBuf,
}

impl Scenario {
    /// Stage `crates/fabro/acceptance/scenarios/<name>`.
    ///
    /// # Panics
    ///
    /// Panics when the scenario directory is missing or cannot be copied;
    /// both mean the checkout is broken, not that the scenario failed.
    pub(crate) fn stage(name: &str) -> Self {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(SCENARIOS)
            .join(name);
        assert!(
            source.is_dir(),
            "scenario `{name}` is not at {}",
            source.display()
        );
        let dir = RunDir::new(&format!("fabro-blackbox-{name}"));
        let root = dir.path().join("bundle");
        copy_tree(&source, &root)
            .unwrap_or_else(|e| panic!("could not stage scenario `{name}`: {e}"));
        Self { dir, root }
    }

    /// A file inside the staged copy.
    pub(crate) fn file(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// The tracked source file, for fixtures that must be read rather than
    /// handed to a run.
    pub(crate) fn source_file(name: &str, relative: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(SCENARIOS)
            .join(name)
            .join(relative)
    }

    /// A fresh run directory beside the bundle, for `petri run --run-dir`.
    pub(crate) fn run_dir(&self, label: &str) -> PathBuf {
        self.dir.path().join(format!("run-{label}"))
    }
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
