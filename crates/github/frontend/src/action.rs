//! Actions from other repositories: how a `uses: owner/repo@ref` is named,
//! pinned and fetched.
//!
//! The frontend resolves a reference to a commit while lowering, so the graph
//! carries what will run ([`PinnedAction`]) and the same file lowers to the
//! same graph. It reads the action's manifest the same way. Fetching the tree
//! is the step's business at run time, through the same [`ActionSource`].

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

/// The step kind a `run:` step lowers to: the core process step plus GitHub's
/// env-file protocol. Defined by the `github_actions` crate; named here so the
/// lowering and the step agree on one string.
pub const RUN_KIND: &str = "github/run";

/// The step kind a JavaScript action lowers to.
pub const ACTION_KIND: &str = "github/action";

/// The step kind a Docker container action lowers to: one container per phase
/// invocation, run against the daemon through the scope's container runner.
pub const DOCKER_ACTION_KIND: &str = "github/docker_action";

/// The step kind a supportable `actions/checkout` call substitutes to: the
/// workspace materializes from the run's own local repository — offline,
/// token-less, the tree you have. A lowering-visible substitution, never a
/// silent runtime intercept; anything the substitution cannot honor falls
/// through to the real action.
pub const CHECKOUT_KIND: &str = "github/checkout";

/// The run-parameter context and key naming the run's repository root:
/// the host fills `petri.repo`, and a [`CHECKOUT_KIND`] node's config reads
/// it back at firing. One spelling, shared by writer and reader.
pub const REPO_PARAM_CONTEXT: &str = "petri";
pub const REPO_PARAM_KEY: &str = "repo";

/// The key under which an action node's output carries the state its `pre` or
/// `main` saved (`GITHUB_STATE`, `::save-state::`), for the phases after it.
pub const STATE_OUTPUT_KEY: &str = "github.state";

/// Which of an action's entry points a `github/action` node runs. Defined here
/// so the lowering and the step agree on one wire value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Pre,
    Main,
    Post,
}

/// Where an action's files are, as a `github/action` config carries it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ActionLocation {
    /// Fetched from its repository at a commit, through the action source.
    Pinned(PinnedAction),
    /// `uses: ./path`: a directory of the checked-out repository, resolved
    /// against `GITHUB_WORKSPACE` at run time as GitHub does.
    Local { local: String },
}

impl ActionLocation {
    /// The action's directory, relative to the fetched repository root (pinned)
    /// or the checkout root (local). Empty for the root itself.
    pub fn directory(&self) -> &str {
        match self {
            Self::Pinned(pinned) => pinned.reference.path.as_deref().unwrap_or(""),
            Self::Local { local } => local,
        }
    }
}

/// `owner/repo[/path]@ref`, as written in `uses:`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActionRef {
    pub owner:   SmolStr,
    pub repo:    SmolStr,
    /// A subdirectory of the repository holding the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path:    Option<SmolStr>,
    /// The tag, branch or commit as written.
    #[serde(rename = "ref")]
    pub git_ref: SmolStr,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RefError {
    #[error("a `uses:` reference needs an `@ref`")]
    NoRef,
    #[error("a `uses:` reference is `owner/repo[/path]@ref`")]
    Shape,
    #[error(transparent)]
    UnsafePath(#[from] ActionPathError),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unsafe action path `{path}`: {reason}")]
pub struct ActionPathError {
    path:   String,
    reason: &'static str,
}

fn path_error(path: &str, reason: &'static str) -> ActionPathError {
    ActionPathError {
        path: path.to_string(),
        reason,
    }
}

fn validate_repository_component(value: &str) -> Result<(), ActionPathError> {
    if value.is_empty() || value == "." || value == ".." {
        return Err(path_error(value, "expected one repository-name component"));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(path_error(
            value,
            "repository names use letters, digits, `-`, `_`, or `.`",
        ));
    }
    Ok(())
}

#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "git's own rule is a literal, case-sensitive `.lock` suffix on a reference \
              component; this is a reference, not a filename, and an ASCII-insensitive \
              match would reject references git accepts"
)]
fn validate_git_ref(value: &str) -> Result<(), ActionPathError> {
    let invalid_char = |c: char| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c);
    if value.is_empty()
        || value.starts_with('-')
        || value.starts_with('/')
        || value.ends_with('/')
        || value.ends_with('.')
        || value.contains("..")
        || value.contains("@{")
        || value.chars().any(invalid_char)
        || value
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
    {
        return Err(path_error(value, "the git reference is not safe or valid"));
    }
    Ok(())
}

/// Validate a path before it is joined to an action cache or workspace root.
/// GitHub action paths use `/` on every host.
pub fn validate_relative_action_path(path: &str, allow_empty: bool) -> Result<(), ActionPathError> {
    if path.is_empty() {
        return if allow_empty {
            Ok(())
        } else {
            Err(path_error(path, "the path is empty"))
        };
    }
    if path.starts_with('/') || path.starts_with('\\') || path.contains('\0') {
        return Err(path_error(path, "the path must be relative"));
    }
    if path
        .split(['/', '\\'])
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(path_error(
            path,
            "every path component must be a normal name",
        ));
    }
    Ok(())
}

/// Resolve a path a manifest names relative to its action directory —
/// `runs.main`, `runs.image` — the way the runner joins it: `./` is normal,
/// and `..` may climb only as far as the fetched repository root (the checkout
/// root for a local action). A subpath action's file may live beside or above
/// its directory (`github/codeql-action/init` runs `../lib/init-entry.js`;
/// oss-fuzz's `infra/cifuzz` actions build
/// `../../../build_fuzzers.Dockerfile`), and the whole repository is what
/// stages. The invariant: a fetched manifest can name any file of its own
/// pinned repository and nothing outside it — stricter than GitHub, which only
/// checks that the joined file exists. The result is the file's path from that
/// root with every `.` and `..` resolved away, so the executor receives a fully
/// normalized path, never `..`.
///
/// `directory` must already be strictly validated
/// ([`validate_relative_action_path`]) — the climb budget it grants is only
/// as trustworthy as its own components.
pub fn resolve_manifest_path(directory: &str, path: &str) -> Result<String, ActionPathError> {
    if path.is_empty() {
        return Err(path_error(path, "the path is empty"));
    }
    if path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return Err(path_error(path, "the path must be relative"));
    }
    let mut resolved: Vec<&str> = directory
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if resolved.pop().is_none() {
                    return Err(path_error(path, "it escapes the repository root"));
                }
            }
            _ => resolved.push(part),
        }
    }
    if resolved.is_empty() {
        return Err(path_error(path, "it names the repository root, not a file"));
    }
    Ok(resolved.join("/"))
}

impl ActionRef {
    pub fn parse(uses: &str) -> Result<Self, RefError> {
        let (name, git_ref) = uses.split_once('@').ok_or(RefError::NoRef)?;
        if git_ref.is_empty() || git_ref.contains(char::is_whitespace) {
            return Err(RefError::NoRef);
        }
        validate_git_ref(git_ref)?;
        let mut parts = name.splitn(3, '/');
        let owner = parts
            .next()
            .filter(|s| !s.is_empty())
            .ok_or(RefError::Shape)?;
        let repo = parts
            .next()
            .filter(|s| !s.is_empty())
            .ok_or(RefError::Shape)?;
        validate_repository_component(owner)?;
        validate_repository_component(repo)?;
        let path = parts
            .next()
            .map(|p| p.trim_matches('/'))
            .filter(|p| !p.is_empty())
            .map(SmolStr::new);
        if let Some(path) = &path {
            validate_relative_action_path(path, false)?;
        }
        Ok(Self {
            owner: SmolStr::new(owner),
            repo: SmolStr::new(repo),
            path,
            git_ref: SmolStr::new(git_ref),
        })
    }

    /// `owner/repo`.
    pub fn repository(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// Whether the reference is already a full commit id.
    pub fn is_commit(&self) -> bool {
        self.git_ref.len() == 40 && self.git_ref.chars().all(|c| c.is_ascii_hexdigit())
    }

    pub fn validate(&self) -> Result<(), ActionPathError> {
        validate_repository_component(&self.owner)?;
        validate_repository_component(&self.repo)?;
        validate_git_ref(&self.git_ref)?;
        if let Some(path) = &self.path {
            validate_relative_action_path(path, false)?;
        }
        Ok(())
    }
}

impl FromStr for ActionRef {
    type Err = RefError;

    fn from_str(uses: &str) -> Result<Self, Self::Err> {
        Self::parse(uses)
    }
}

impl fmt::Display for ActionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)?;
        if let Some(path) = &self.path {
            write!(f, "/{path}")?;
        }
        write!(f, "@{}", self.git_ref)
    }
}

/// A reference resolved to a commit: what the graph carries. The reference
/// stays alongside for messages and for `GITHUB_ACTION_REF`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PinnedAction {
    pub reference: ActionRef,
    pub sha:       SmolStr,
}

impl fmt::Display for PinnedAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.reference, self.sha)
    }
}

impl PinnedAction {
    pub fn validate(&self) -> Result<(), ActionPathError> {
        self.reference.validate()?;
        if self.sha.len() != 40 || !self.sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(path_error(
                &self.sha,
                "a pinned action needs a 40-digit hexadecimal commit",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ActionSourceError {
    /// The reference names nothing the source can find.
    #[error("cannot resolve `{reference}`: {message}")]
    Unresolvable {
        reference: String,
        message:   String,
    },
    /// The source does not serve this reference — and says why, when it knows.
    /// With no reason, an offline or partial source (a snapshot, say) simply
    /// does not cover the reference, and refreshing it may. With one, the
    /// source met a terminal answer upstream — the repository is private or
    /// removed, recorded at refresh time — and refreshing will not help.
    /// Either way the lowering rejects the step as
    /// `unsupported.action.remote`, exactly as it would with no source at
    /// all, rather than as an error; a *resolved* action whose manifest is
    /// missing or malformed stays a load error instead.
    #[error("`{reference}` is not available from this action source{}",
            .reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default())]
    Unavailable {
        reference: String,
        reason:    Option<String>,
    },
    #[error("`{0}` has no `action.yml` or `action.yaml`")]
    NoManifest(String),
    #[error("cannot fetch `{action}`: {message}")]
    Fetch { action: String, message: String },
}

/// The hint for an [`ActionSourceError::Unavailable`] rejection. Two different
/// situations, one code: with no reason the source simply does not cover the
/// reference (refreshing it may help); with one, upstream already said no (a
/// private or removed repository — refreshing will not).
pub(crate) fn unavailable_hint(reason: Option<String>) -> String {
    match reason {
        Some(reason) => format!(
            "the action source cannot serve it: {} — the repository is unavailable \
             upstream (private or removed), so refreshing the source will not help",
            reason.lines().collect::<Vec<_>>().join(" ")
        ),
        None => "the configured action source does not serve this reference; \
                 refreshing it (for a snapshot, the refresh test) may add it"
            .to_string(),
    }
}

/// Where actions come from.
///
/// Synchronous and blocking by design: `Frontend::load` is synchronous.
pub trait ActionSource: Send + Sync {
    /// Resolve a tag, branch or commit to a commit.
    fn resolve(&self, reference: &ActionRef) -> Result<PinnedAction, ActionSourceError>;

    /// The text of the action's `action.yml` (or `action.yaml`).
    fn manifest(&self, pinned: &PinnedAction) -> Result<String, ActionSourceError>;

    /// The text of the one file `reference.path` names at the pinned commit —
    /// a remote called workflow's YAML
    /// (`uses: owner/repo/.github/workflows/x.yml@ref` puts the file in the
    /// path). Sources that cannot serve files decline as unavailable.
    fn file(&self, pinned: &PinnedAction) -> Result<String, ActionSourceError> {
        Err(ActionSourceError::Unavailable {
            reference: pinned.reference.to_string(),
            reason:    None,
        })
    }
}

/// An in-memory source for tests: manifests by reference, no trees.
#[derive(Default)]
pub struct MapActionSource {
    manifests: Mutex<BTreeMap<String, (String, String)>>,
}

impl MapActionSource {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `uses` (as written, `owner/repo@ref`) as resolving to `sha`
    /// with this manifest text.
    #[must_use]
    pub fn with(self, uses: &str, sha: &str, manifest: &str) -> Self {
        self.manifests
            .lock()
            .expect("map is not poisoned")
            .insert(uses.to_string(), (sha.to_string(), manifest.to_string()));
        self
    }
}

impl ActionSource for MapActionSource {
    fn resolve(&self, reference: &ActionRef) -> Result<PinnedAction, ActionSourceError> {
        let key = reference.to_string();
        let manifests = self.manifests.lock().expect("map is not poisoned");
        match manifests.get(&key) {
            Some((sha, _)) => Ok(PinnedAction {
                reference: reference.clone(),
                sha:       SmolStr::new(sha),
            }),
            None => Err(ActionSourceError::Unresolvable {
                reference: key,
                message:   "not in the map".into(),
            }),
        }
    }

    fn manifest(&self, pinned: &PinnedAction) -> Result<String, ActionSourceError> {
        let key = pinned.reference.to_string();
        let manifests = self.manifests.lock().expect("map is not poisoned");
        manifests
            .get(&key)
            .map(|(_, text)| text.clone())
            .ok_or(ActionSourceError::NoManifest(key))
    }

    /// For tests, a registered "manifest" doubles as the file at the path.
    fn file(&self, pinned: &PinnedAction) -> Result<String, ActionSourceError> {
        self.manifest(pinned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_reference_shapes() {
        let r = ActionRef::parse("actions/checkout@v4").unwrap();
        assert_eq!((r.owner.as_str(), r.repo.as_str()), ("actions", "checkout"));
        assert_eq!(r.path, None);
        assert_eq!(r.git_ref, "v4");
        assert_eq!(r.to_string(), "actions/checkout@v4");

        let r = ActionRef::parse("owner/repo/sub/dir@main").unwrap();
        assert_eq!(r.path.as_deref(), Some("sub/dir"));
        assert_eq!(r.to_string(), "owner/repo/sub/dir@main");

        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert!(ActionRef::parse(&format!("a/b@{sha}")).unwrap().is_commit());
        assert!(!r.is_commit());
    }

    #[test]
    fn manifest_paths_resolve_within_the_repository_root() {
        assert_eq!(
            resolve_manifest_path("", "Dockerfile").unwrap(),
            "Dockerfile"
        );
        assert_eq!(
            resolve_manifest_path("sub/dir", "Dockerfile").unwrap(),
            "sub/dir/Dockerfile"
        );
        assert_eq!(
            resolve_manifest_path("sub/dir", "./images/Dockerfile").unwrap(),
            "sub/dir/images/Dockerfile",
            "a leading ./ is normal, and a subdirectory joins"
        );
        assert_eq!(
            resolve_manifest_path(
                "infra/cifuzz/actions/build_fuzzers",
                "../../../build_fuzzers.Dockerfile"
            )
            .unwrap(),
            "infra/build_fuzzers.Dockerfile",
            "a manifest may name a file above its directory, within its repository"
        );
        assert_eq!(
            resolve_manifest_path(
                ".github/actions/next-stats-action",
                "../../next-stats-action.Dockerfile"
            )
            .unwrap(),
            ".github/next-stats-action.Dockerfile",
            "a local action is bounded by the checkout root the same way"
        );
        assert!(resolve_manifest_path("", "../escape").is_err());
        assert!(resolve_manifest_path("sub", "../../escape").is_err());
        assert!(
            resolve_manifest_path("a", "c/../../../escape").is_err(),
            "descending first buys no extra climb"
        );
        assert_eq!(
            resolve_manifest_path("a/b", "c/../../../at-root").unwrap(),
            "at-root",
            "climbing exactly to the root is within the repository"
        );
        assert!(resolve_manifest_path("a", "/abs").is_err());
        assert!(resolve_manifest_path("a", "a\\b").is_err());
        assert!(resolve_manifest_path("a", "").is_err());
        assert!(
            resolve_manifest_path("a", "..").is_err(),
            "the root itself is not a file"
        );
    }

    #[test]
    fn rejects_the_malformed() {
        assert_eq!(ActionRef::parse("actions/checkout"), Err(RefError::NoRef));
        assert_eq!(ActionRef::parse("actions/checkout@"), Err(RefError::NoRef));
        assert_eq!(ActionRef::parse("checkout@v4"), Err(RefError::Shape));
        assert_eq!(ActionRef::parse("/checkout@v4"), Err(RefError::Shape));
        assert!(ActionRef::parse("owner/repo/../../outside@v1").is_err());
        assert!(ActionRef::parse("owner/repo/path//child@v1").is_err());
        assert!(ActionRef::parse("owner/repo@--upload-pack=evil").is_err());
    }
}
