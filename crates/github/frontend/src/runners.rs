//! Which `runs-on` labels the local executor can place.
//!
//! A runner label's own text is the only information anyone has about it, and
//! pools are named so humans can read the platform: `ubuntu-24.04-arm`,
//! `depot-ubuntu-22.04-16`, `namespace-profile-macos-15`. [`classify`] reads it
//! the same way, by whole tokens: a label naming Ubuntu or Linux places on the
//! local executor — the same claim `ubuntu-latest` has always made, an
//! environment this machine stands in for — and a label naming Windows or
//! macOS is that platform's specific out-of-scope rejection wherever the token
//! appears, not just as a prefix. Labels that say nothing (`self-hosted`,
//! `gpu`, `codspeed-macro`) answer to the host's [`RunnerMap`]: whether an
//! opaque pool is a usable Linux environment is the host's call, not a
//! frontend guess, and an unmapped one is an explicit `runs_on.unknown`
//! rejection naming the label — never a silently skipped job.
//!
//! A job places only when every label of its set resolves — GitHub requires
//! all labels to match, and so does this policy. Static and expression-derived
//! labels alike pass through the one label check.

use std::collections::BTreeSet;

/// What a label's own text says about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelClass {
    /// Names an Ubuntu or Linux environment: the local executor places it.
    Linux,
    /// Names a Windows runner.
    Windows,
    /// Names a macOS runner.
    MacOs,
    /// Says nothing about its platform: the runner map decides.
    Opaque,
}

/// Classify a label by its whole tokens (split on `-`, `_`, `.`), so
/// `codspeed-macro` does not read as a Mac and `depot-ubuntu-24.04-16` reads
/// as the Ubuntu pool it is. Windows and macOS win over a stray Linux token:
/// a contradiction is rejected, not mapped.
pub fn classify(label: &str) -> LabelClass {
    let mut class = LabelClass::Opaque;
    for token in label.to_lowercase().split(['-', '_', '.']) {
        match token {
            "windows" => return LabelClass::Windows,
            "macos" | "osx" => return LabelClass::MacOs,
            "ubuntu" | "linux" => class = LabelClass::Linux,
            _ => {}
        }
    }
    class
}

/// The host's map for opaque labels: which of them name a usable Linux
/// environment on this machine.
#[derive(Clone, Debug, Default)]
pub struct RunnerMap {
    /// Configured labels, lowercased — labels are case-insensitive on GitHub.
    extra: BTreeSet<String>,
}

impl RunnerMap {
    /// The naming rule and nothing configured.
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

    /// Whether one label resolves, case-insensitively: it names Linux, or the
    /// host mapped it.
    pub fn knows(&self, label: &str) -> bool {
        classify(label) == LabelClass::Linux || self.extra.contains(&label.to_lowercase())
    }

    /// The configured labels, for the rejection hint.
    pub fn configured(&self) -> Vec<&str> {
        self.extra.iter().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_classify_by_their_own_tokens() {
        for label in [
            "ubuntu-latest",
            "Ubuntu-26.04",
            "ubuntu-24.04-arm",
            "ubuntu-24.04-xl",
            "depot-ubuntu-22.04-16",
            "depot-ubuntu-24.04-arm-8",
            "self-hosted-linux-x64",
        ] {
            assert_eq!(classify(label), LabelClass::Linux, "{label}");
        }
        for label in [
            "windows-latest",
            "windows-11-arm",
            "namespace-profile-windows-2022-x86-64-4",
        ] {
            assert_eq!(classify(label), LabelClass::Windows, "{label}");
        }
        for label in ["macos-14", "macos-15-intel", "namespace-profile-macos-15"] {
            assert_eq!(classify(label), LabelClass::MacOs, "{label}");
        }
        for label in [
            "self-hosted",
            "gpu",
            "codspeed-macro",
            "namespace-profile-default",
        ] {
            assert_eq!(classify(label), LabelClass::Opaque, "{label}");
        }
    }

    #[test]
    fn the_map_covers_opaque_labels_and_parses_lists() {
        let map = RunnerMap::builtin().allow_list(" codspeed-macro, Self-Hosted\nx64,,");
        for label in ["codspeed-macro", "self-hosted", "X64"] {
            assert!(map.knows(label), "{label}");
        }
        assert!(
            map.knows("Ubuntu-Latest"),
            "the naming rule needs no config"
        );
        assert!(
            !map.knows("namespace-profile-default"),
            "opaque stays unknown"
        );
    }
}
