//! Actions from git: a bare repository per `owner/repo` under a cache directory.
//!
//! Resolution asks the remote (`git ls-remote`), so a moving tag such as `v4`
//! resolves to whatever it points at now — as GitHub does at the start of a run —
//! and the graph pins that commit. Fetching is by the reference as written, one
//! commit deep. Trees are extracted once per commit with `git archive`.
//!
//! Everything shells out to `git`, which every machine that runs workflows has.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, Weak};

use frontend_gha::action::{ActionRef, ActionSource, ActionSourceError, PinnedAction};
use smol_str::SmolStr;

use crate::ActionTreeSource;

pub struct GitActionSource {
    cache: PathBuf,
    /// `https://github.com` — or a `file://` directory of repositories in tests.
    remote_base: String,
    /// Fetches and extractions serialize per repository. Independent actions do
    /// not block each other, while two steps cannot write one cache entry at once.
    locks: Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>,
}

impl GitActionSource {
    pub fn new(cache: impl Into<PathBuf>) -> Self {
        Self {
            cache: cache.into(),
            remote_base: "https://github.com".into(),
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Where `owner/repo` is found: `<base>/owner/repo`.
    pub fn with_remote_base(mut self, base: impl Into<String>) -> Self {
        self.remote_base = base.into();
        self
    }

    fn url(&self, reference: &ActionRef) -> String {
        format!(
            "{}/{}/{}",
            self.remote_base.trim_end_matches('/'),
            reference.owner,
            reference.repo
        )
    }

    fn bare_dir(&self, reference: &ActionRef) -> PathBuf {
        self.cache
            .join("repos")
            .join(reference.owner.as_str())
            .join(format!("{}.git", reference.repo))
    }

    fn tree_entry_dir(&self, pinned: &PinnedAction) -> PathBuf {
        let root = self
            .cache
            .join("trees")
            .join(pinned.reference.owner.as_str())
            .join(pinned.reference.repo.as_str())
            .join(pinned.sha.as_str());
        match &pinned.reference.path {
            Some(path) => root.join("path").join(path.as_str()),
            None => root.join("root"),
        }
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
        reference
            .validate()
            .map_err(|e| fetch_error(reference, e.to_string()))?;
        let dir = self.bare_dir(reference);
        if !dir.join("HEAD").is_file() {
            std::fs::create_dir_all(&dir).map_err(|e| fetch_error(reference, e.to_string()))?;
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
    fn fetch(&self, pinned: &PinnedAction) -> Result<PathBuf, ActionSourceError> {
        pinned
            .validate()
            .map_err(|e| fetch_error(&pinned.reference, e.to_string()))?;
        let reference = &pinned.reference;
        let bare = self.ensure_bare(reference)?;
        if Self::has_commit(&bare, &pinned.sha) {
            return Ok(bare);
        }
        // By the reference as written: a tag or branch fetch works on every
        // transport, and GitHub also serves a commit id directly.
        let url = self.url(reference);
        git(
            &[
                "fetch",
                "-q",
                "--depth",
                "1",
                &url,
                reference.git_ref.as_str(),
            ],
            Some(&bare),
        )
        .map_err(|e| fetch_error(reference, e))?;
        if !Self::has_commit(&bare, &pinned.sha) {
            return Err(fetch_error(
                reference,
                format!(
                    "fetched `{}` but did not receive commit {} (the reference moved?)",
                    reference.git_ref, pinned.sha
                ),
            ));
        }
        Ok(bare)
    }
}

fn fetch_error(reference: &ActionRef, message: String) -> ActionSourceError {
    ActionSourceError::Fetch {
        action: reference.to_string(),
        message,
    }
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
    fn resolve(&self, reference: &ActionRef) -> Result<PinnedAction, ActionSourceError> {
        reference
            .validate()
            .map_err(|e| ActionSourceError::Unresolvable {
                reference: reference.to_string(),
                message: e.to_string(),
            })?;
        if reference.is_commit() {
            return Ok(PinnedAction {
                reference: reference.clone(),
                sha: reference.git_ref.clone(),
            });
        }
        let key = reference.to_string();
        let url = self.url(reference);
        let listing = git(
            &[
                "ls-remote",
                "--tags",
                "--heads",
                &url,
                reference.git_ref.as_str(),
            ],
            None,
        )
        .map_err(|message| ActionSourceError::Unresolvable {
            reference: key.clone(),
            message,
        })?;
        // Prefer the peeled tag (the commit an annotated tag points at), then the
        // tag itself, then a branch.
        let want = [
            format!("refs/tags/{}^{{}}", reference.git_ref),
            format!("refs/tags/{}", reference.git_ref),
            format!("refs/heads/{}", reference.git_ref),
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
            message: format!("no tag or branch `{}` at {url}", reference.git_ref),
        })?;
        let pinned = PinnedAction {
            reference: reference.clone(),
            sha,
        };
        Ok(pinned)
    }

    fn manifest(&self, pinned: &PinnedAction) -> Result<String, ActionSourceError> {
        let lock = self.repo_lock(&pinned.reference);
        let _guard = lock.lock().expect("repository lock is not poisoned");
        let bare = self.fetch(pinned)?;
        let prefix = pinned
            .reference
            .path
            .as_ref()
            .map(|p| format!("{}/", p.trim_matches('/')))
            .unwrap_or_default();
        for name in ["action.yml", "action.yaml"] {
            let spec = format!("{}:{prefix}{name}", pinned.sha);
            if let Ok(text) = git(&["show", &spec], Some(&bare)) {
                return Ok(text);
            }
        }
        Err(ActionSourceError::NoManifest(pinned.reference.to_string()))
    }

    /// The one file the reference's path names, at the pinned commit — a
    /// remote called workflow's YAML.
    fn file(&self, pinned: &PinnedAction) -> Result<String, ActionSourceError> {
        let Some(path) = pinned.reference.path.as_deref() else {
            return Err(ActionSourceError::Fetch {
                action: pinned.reference.to_string(),
                message: "the reference names no file path".into(),
            });
        };
        let lock = self.repo_lock(&pinned.reference);
        let _guard = lock.lock().expect("repository lock is not poisoned");
        let bare = self.fetch(pinned)?;
        let spec = format!("{}:{path}", pinned.sha);
        git(&["show", &spec], Some(&bare)).map_err(|message| ActionSourceError::Fetch {
            action: pinned.reference.to_string(),
            message,
        })
    }
}

impl ActionTreeSource for GitActionSource {
    fn tree(&self, pinned: &PinnedAction) -> Result<PathBuf, ActionSourceError> {
        let lock = self.repo_lock(&pinned.reference);
        let _guard = lock.lock().expect("repository lock is not poisoned");
        let bare = self.fetch(pinned)?;
        let dir = self.tree_entry_dir(pinned);
        let action_dir = match &pinned.reference.path {
            Some(path) => dir.join(path.trim_matches('/')),
            None => dir.clone(),
        };
        let complete = match &pinned.reference.path {
            Some(_) => dir.join(".complete"),
            None => dir.with_extension("complete"),
        };
        if !complete.is_file() || !action_dir.is_dir() {
            let _ = std::fs::remove_file(&complete);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)
                .map_err(|e| fetch_error(&pinned.reference, e.to_string()))?;
            extract(
                &bare,
                pinned.sha.as_str(),
                pinned.reference.path.as_deref(),
                &dir,
            )
            .map_err(|e| fetch_error(&pinned.reference, e))?;
            std::fs::write(&complete, b"")
                .map_err(|e| fetch_error(&pinned.reference, e.to_string()))?;
        }
        Ok(action_dir)
    }
}

/// `git archive <sha> | tar -x -C <dir>`.
fn extract(bare: &Path, sha: &str, path: Option<&str>, dir: &Path) -> Result<(), String> {
    let mut command = Command::new("git");
    command.args(["archive", "--format=tar", sha]);
    if let Some(path) = path {
        command.args(["--", path]);
    }
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
    if let Some(dir) = std::env::var_os("PETRI_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("petri");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".cache").join("petri");
    }
    std::env::temp_dir().join("petri-cache")
}
