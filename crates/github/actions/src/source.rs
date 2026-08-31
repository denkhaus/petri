//! Actions from git: a bare repository per `owner/repo` under a cache
//! directory.
//!
//! Resolution asks the remote (`git ls-remote`), so a moving tag such as `v4`
//! resolves to whatever it points at now — as GitHub does at the start of a run
//! — and the graph pins that commit. Fetching is by the reference as written,
//! one commit deep. Trees are extracted once per commit with `git archive`.
//!
//! Everything shells out to `git`, which every machine that runs workflows has.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, Weak};
use std::{env, fs};

use frontend_gha::ActionSource;
use frontend_gha::action::{ActionRef, ActionSourceError, PinnedAction};
use smol_str::SmolStr;
use tracing::Span;
use tracing::field::Empty;

use crate::ActionTreeSource;

pub struct GitActionSource {
    cache:       PathBuf,
    /// `https://github.com` — or a `file://` directory of repositories in tests.
    remote_base: String,
    /// Fetches and extractions serialize per repository. Independent actions do
    /// not block each other, while two steps cannot write one cache entry at
    /// once.
    locks:       Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>,
}

impl GitActionSource {
    pub fn new(cache: impl Into<PathBuf>) -> Self {
        Self {
            cache:       cache.into(),
            remote_base: "https://github.com".into(),
            locks:       Mutex::new(HashMap::new()),
        }
    }

    /// Where `owner/repo` is found: `<base>/owner/repo`.
    #[must_use]
    pub fn with_remote_base(mut self, base: impl Into<String>) -> Self {
        self.remote_base = base.into();
        self
    }

    fn url(&self, reference: &ActionRef) -> String {
        format!(
            "{}/{}/{}",
            self.remote_base.trim_end_matches('/'),
            reference.owner(),
            reference.repo()
        )
    }

    fn bare_dir(&self, reference: &ActionRef) -> PathBuf {
        self.cache
            .join("repos")
            .join(reference.owner())
            .join(format!("{}.git", reference.repo()))
    }

    fn tree_entry_dir(&self, pinned: &PinnedAction) -> PathBuf {
        self.cache
            .join("trees")
            .join(pinned.reference().owner())
            .join(pinned.reference().repo())
            .join(pinned.sha())
            .join("root")
    }

    fn repo_lock(&self, reference: &ActionRef) -> Arc<Mutex<()>> {
        let key = self.bare_dir(reference);
        let mut locks = self.locks.lock().expect("lock map is not poisoned");
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    fn ensure_bare(&self, reference: &ActionRef) -> Result<PathBuf, ActionSourceError> {
        let dir = self.bare_dir(reference);
        if !dir.join("HEAD").is_file() {
            fs::create_dir_all(&dir).map_err(|e| fetch_error(reference, e.to_string()))?;
            git(&["init", "--bare", "-q"], Some(&dir)).map_err(|e| fetch_error(reference, e))?;
        }
        Ok(dir)
    }

    fn has_commit(bare: &Path, sha: &str) -> bool {
        git(
            &["cat-file", "-e", &format!("{sha}^{{commit}}")],
            Some(bare),
        )
        .is_ok()
    }

    /// Make sure the pinned commit is in the bare repository.
    #[tracing::instrument(
        name = "github.action_fetch",
        level = "debug",
        skip_all,
        fields(
            owner = %pinned.reference().owner(),
            repo = %pinned.reference().repo(),
            git_ref = %pinned.reference().git_ref(),
            sha = %pinned.sha(),
            cached = Empty,
        )
    )]
    fn fetch(&self, pinned: &PinnedAction) -> Result<PathBuf, ActionSourceError> {
        let reference = pinned.reference();
        let bare = self.ensure_bare(reference)?;
        if Self::has_commit(&bare, pinned.sha()) {
            Span::current().record("cached", true);
            return Ok(bare);
        }
        Span::current().record("cached", false);
        // By the reference as written: a tag or branch fetch works on every
        // transport, and GitHub also serves a commit id directly.
        let url = self.url(reference);
        git(
            &["fetch", "-q", "--depth", "1", &url, reference.git_ref()],
            Some(&bare),
        )
        .map_err(|e| fetch_error(reference, e))?;
        if !Self::has_commit(&bare, pinned.sha()) {
            return Err(fetch_error(
                reference,
                format!(
                    "fetched `{}` but did not receive commit {} (the reference moved?)",
                    reference.git_ref(),
                    pinned.sha()
                ),
            ));
        }
        Ok(bare)
    }
}

fn fetch_error(reference: &ActionRef, message: String) -> ActionSourceError {
    let upstream_refusal = is_upstream_refusal(&message);
    tracing::error!(
        owner = %reference.owner(),
        repo = %reference.repo(),
        git_ref = %reference.git_ref(),
        upstream_refusal,
        "action source could not serve the reference"
    );
    if upstream_refusal {
        return ActionSourceError::Unavailable {
            reference: reference.to_string(),
            reason:    Some(message),
        };
    }
    ActionSourceError::Fetch {
        action: reference.to_string(),
        message,
    }
}

/// Whether git's stderr is a terminal upstream refusal — the repository is
/// private or removed, and no anonymous fetch will ever serve it — as opposed
/// to a transient or local failure. GitHub answers `Repository not found` for
/// both private and missing repositories; a credential prompt with prompts
/// disabled is the same refusal seen from the auth side. Classified as
/// [`ActionSourceError::Unavailable`] with the reason, so the lowering rejects
/// it as `unsupported.action.upstream_gone` — the same code the snapshot
/// source reports from its recorded refresh failures, keeping the two sources'
/// verdicts on one reference identical.
fn is_upstream_refusal(stderr: &str) -> bool {
    [
        "Repository not found",
        "could not read Username",
        "Authentication failed",
    ]
    .iter()
    .any(|needle| stderr.contains(needle))
}

/// Run git and return its stdout, or its stderr as the error.
fn git(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut command = Command::new("git");
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    // Never prompt: a private repository fails fast instead of hanging.
    command.env("GIT_TERMINAL_PROMPT", "0");
    let output = command
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

impl ActionSource for GitActionSource {
    #[tracing::instrument(
        name = "github.action_resolve",
        level = "debug",
        skip_all,
        fields(
            owner = %reference.owner(),
            repo = %reference.repo(),
            git_ref = %reference.git_ref(),
            sha = Empty,
        )
    )]
    fn resolve(&self, reference: &ActionRef) -> Result<PinnedAction, ActionSourceError> {
        if reference.is_commit() {
            let sha = SmolStr::new(reference.git_ref());
            return Ok(PinnedAction::try_new(reference.clone(), sha)
                .expect("a commit reference is a full hexadecimal id"));
        }
        let key = reference.to_string();
        let url = self.url(reference);
        let listing = git(
            &["ls-remote", "--tags", "--heads", &url, reference.git_ref()],
            None,
        )
        .map_err(|message| {
            if is_upstream_refusal(&message) {
                ActionSourceError::Unavailable {
                    reference: key.clone(),
                    reason:    Some(message),
                }
            } else {
                ActionSourceError::Unresolvable {
                    reference: key.clone(),
                    message,
                }
            }
        })?;
        // Prefer the peeled tag (the commit an annotated tag points at), then the
        // tag itself, then a branch.
        let want = [
            format!("refs/tags/{}^{{}}", reference.git_ref()),
            format!("refs/tags/{}", reference.git_ref()),
            format!("refs/heads/{}", reference.git_ref()),
        ];
        let mut found: Option<SmolStr> = None;
        for candidate in &want {
            if let Some(sha) = listing.lines().find_map(|line| {
                let (sha, name) = line.split_once('\t')?;
                (name.trim() == candidate).then(|| SmolStr::new(sha.trim()))
            }) {
                found = Some(sha);
                break;
            }
        }
        let sha = found.ok_or_else(|| ActionSourceError::Unresolvable {
            reference: key.clone(),
            message:   format!("no tag or branch `{}` at {url}", reference.git_ref()),
        })?;
        Span::current().record("sha", sha.as_str());
        PinnedAction::try_new(reference.clone(), sha).map_err(|e| ActionSourceError::Unresolvable {
            reference: key,
            message:   e.to_string(),
        })
    }

    fn manifest(&self, pinned: &PinnedAction) -> Result<String, ActionSourceError> {
        let lock = self.repo_lock(pinned.reference());
        let _guard = lock.lock().expect("repository lock is not poisoned");
        let bare = self.fetch(pinned)?;
        let prefix = pinned
            .reference()
            .path()
            .map(|p| format!("{}/", p.trim_matches('/')))
            .unwrap_or_default();
        for name in ["action.yml", "action.yaml"] {
            let spec = format!("{}:{prefix}{name}", pinned.sha());
            if let Ok(text) = git(&["show", &spec], Some(&bare)) {
                return Ok(text);
            }
        }
        Err(ActionSourceError::NoManifest(
            pinned.reference().to_string(),
        ))
    }

    /// The one file the reference's path names, at the pinned commit — a
    /// remote called workflow's YAML.
    fn file(&self, pinned: &PinnedAction) -> Result<String, ActionSourceError> {
        let Some(path) = pinned.reference().path() else {
            return Err(ActionSourceError::Fetch {
                action:  pinned.reference().to_string(),
                message: "the reference names no file path".into(),
            });
        };
        let lock = self.repo_lock(pinned.reference());
        let _guard = lock.lock().expect("repository lock is not poisoned");
        let bare = self.fetch(pinned)?;
        let spec = format!("{}:{path}", pinned.sha());
        git(&["show", &spec], Some(&bare)).map_err(|message| ActionSourceError::Fetch {
            action: pinned.reference().to_string(),
            message,
        })
    }
}

impl ActionTreeSource for GitActionSource {
    /// The whole repository tree at the pinned commit — never just an action's
    /// subdirectory. A subpath action's manifest may reach beside it
    /// (`github/codeql-action/init` runs `../lib/init-entry.js`), so the tree
    /// an action executes from is the repository, exactly as on GitHub's
    /// runners. Extracted once per commit, whichever subpath asked first.
    #[tracing::instrument(
        name = "github.action_tree",
        level = "debug",
        skip_all,
        fields(
            owner = %pinned.reference().owner(),
            repo = %pinned.reference().repo(),
            sha = %pinned.sha(),
            extracted = Empty,
        )
    )]
    fn tree(&self, pinned: &PinnedAction) -> Result<PathBuf, ActionSourceError> {
        let lock = self.repo_lock(pinned.reference());
        let _guard = lock.lock().expect("repository lock is not poisoned");
        let bare = self.fetch(pinned)?;
        let dir = self.tree_entry_dir(pinned);
        let complete = dir.with_extension("complete");
        let needs_extract = !complete.is_file() || !dir.is_dir();
        Span::current().record("extracted", needs_extract);
        if needs_extract {
            let _ = fs::remove_file(&complete);
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).map_err(|e| fetch_error(pinned.reference(), e.to_string()))?;
            extract(&bare, pinned.sha(), &dir).map_err(|e| fetch_error(pinned.reference(), e))?;
            fs::write(&complete, b"")
                .map_err(|e| fetch_error(pinned.reference(), e.to_string()))?;
        }
        Ok(dir)
    }
}

/// `git archive <sha> | tar -x -C <dir>`.
fn extract(bare: &Path, sha: &str, dir: &Path) -> Result<(), String> {
    let mut command = Command::new("git");
    command.args(["archive", "--format=tar", sha]);
    let mut archive = command
        .current_dir(bare)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run git archive: {e}"))?;
    let stdout = archive
        .stdout
        .take()
        .ok_or_else(|| "git archive has no stdout".to_string())?;
    let tar = Command::new("tar")
        .args(["-x", "-C"])
        .arg(dir)
        .stdin(stdout)
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not run tar: {e}"))?;
    let archive = archive
        .wait_with_output()
        .map_err(|e| format!("git archive did not finish: {e}"))?;
    if !archive.status.success() {
        return Err(format!(
            "git archive failed: {}",
            String::from_utf8_lossy(&archive.stderr).trim()
        ));
    }
    if !tar.status.success() {
        return Err(format!(
            "tar failed: {}",
            String::from_utf8_lossy(&tar.stderr).trim()
        ));
    }
    Ok(())
}

/// Where fetched actions live between runs: `$PETRI_CACHE_DIR`, else
/// `$XDG_CACHE_HOME/petri`, else `~/.cache/petri`, else under the temp dir.
pub fn default_cache_dir() -> PathBuf {
    if let Some(dir) = env::var_os("PETRI_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("petri");
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".cache").join("petri");
    }
    env::temp_dir().join("petri-cache")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact stderr shapes seen in the wild: the sweep's anonymous fetch of
    /// an org-private repository, and the snapshot refresh's recorded refusal.
    /// Both must classify as `Unavailable` (→ `upstream_gone`), so the git
    /// source and the snapshot source give one reference the same verdict. A
    /// transient failure stays `Fetch`.
    #[test]
    fn terminal_upstream_refusals_are_unavailable() {
        let reference =
            ActionRef::parse("vercel/gh-sts-action@c30f0b7a16e0766c4ffbc0d210b54d0e75053fd2")
                .expect("a valid reference");
        for refusal in [
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
            "remote: Repository not found.\nfatal: repository 'https://github.com/x/y/' not found",
            "fatal: Authentication failed for 'https://github.com/x/y/'",
        ] {
            match fetch_error(&reference, refusal.to_string()) {
                ActionSourceError::Unavailable { reason, .. } => {
                    assert_eq!(reason.as_deref(), Some(refusal));
                }
                other => panic!("{refusal:?} should be Unavailable, got {other:?}"),
            }
        }
        assert!(matches!(
            fetch_error(&reference, "error: RPC failed; curl 56".to_string()),
            ActionSourceError::Fetch { .. }
        ));
    }
}
