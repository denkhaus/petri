//! Which `runs-on` labels the local executor can place.
//!
//! The built-in set is GitHub's `ubuntu-*` labels ([`KNOWN_RUNS_ON`]) — the
//! environments the local executor emulates on this machine. Configuration adds
//! labels that also describe usable Linux environments this machine can stand
//! in for: a third-party pool (`depot-ubuntu-24.04-8`), a self-hosted set
//! (`self-hosted`, `linux`, `x64`). A job places only when every label of its
//! set resolves — GitHub requires all labels to match, and so does this map —
//! and an unresolved label is an explicit `runs_on.unknown` rejection naming
//! the label, never a silently skipped job.
//!
//! Windows and macOS labels stay specific errors whatever the configuration
//! says: the local executor emulates Linux, and a label claiming otherwise is a
//! contradiction to reject, not to map. Static and expression-derived labels
//! alike pass through this map — both reach the lowering's one label check.

use std::collections::BTreeSet;

/// `runs-on` labels the local executor places out of the box.
///
/// These are GitHub-hosted labels the local executor places on this machine or a
/// Linux container. Third-party and self-hosted labels (`depot-*`,
/// `namespace-profile-*`, `self-hosted`) are rejected per label unless the
/// host's [`RunnerMap`] maps them: whether such a label names a usable Linux
/// environment is the host's call, not a frontend guess.
pub const KNOWN_RUNS_ON: &[&str] = &[
    "ubuntu-latest",
    "ubuntu-slim",
    "ubuntu-26.04",
    "ubuntu-24.04",
    "ubuntu-22.04",
    "ubuntu-20.04",
    "ubuntu-24.04-arm",
    "ubuntu-22.04-arm",
];

/// The label map: built-ins plus whatever the host's configuration added.
#[derive(Clone, Debug, Default)]
pub struct RunnerMap {
    /// Configured labels, lowercased — labels are case-insensitive on GitHub.
    extra: BTreeSet<String>,
}

impl RunnerMap {
    /// The built-in `ubuntu-*` labels and nothing else.
    pub fn builtin() -> Self {
        Self::default()
    }

    /// Add one label that resolves to the local Linux environment.
    pub fn allow(mut self, label: &str) -> Self {
        let label = label.trim();
        if !label.is_empty() {
            self.extra.insert(label.to_lowercase());
        }
        self
    }

    /// Add a written list — labels separated by commas or whitespace, the shape
    /// an environment variable or a flag carries.
    pub fn allow_list(mut self, list: &str) -> Self {
        for label in list.split([',', ' ', '\t', '\n']) {
            self = self.allow(label);
        }
        self
    }

    /// Whether one label resolves, case-insensitively.
    pub fn knows(&self, label: &str) -> bool {
        self.knows_lowered(&label.to_lowercase())
    }

    /// [`Self::knows`], for a caller that already lowercased the label.
    pub(crate) fn knows_lowered(&self, lowered: &str) -> bool {
        KNOWN_RUNS_ON.contains(&lowered) || self.extra.contains(lowered)
    }

    /// Every label that resolves, built-ins first — the rejection hint's list.
    pub fn known(&self) -> Vec<&str> {
        KNOWN_RUNS_ON
            .iter()
            .copied()
            .chain(self.extra.iter().map(String::as_str))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_list_parses_and_labels_are_case_insensitive() {
        let map =
            RunnerMap::builtin().allow_list(" depot-ubuntu-24.04-8, Self-Hosted\nlinux  x64,,");
        for label in ["depot-ubuntu-24.04-8", "self-hosted", "LINUX", "x64"] {
            assert!(map.knows(label), "{label}");
        }
        assert!(map.knows("Ubuntu-Latest"), "built-ins stay");
        assert!(
            !map.knows("namespace-profile-arm"),
            "unconfigured stays unknown"
        );
    }
}
