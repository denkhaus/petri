//! The checkout's identity: what `github.repository` means here.
//!
//! The one honest source is the checkout's own git configuration — the
//! `origin` remote names the repository the way GitHub would — read through
//! the same [`FileSource`] the rest of the lowering uses, so a corpus
//! checkout or a test map simply has none and the repository stays unnamed.
//! *Lowering* deliberately never reads HEAD: a commit must not change what a
//! file lowers to, so the placement statics keep the fixed `ref` and `sha`
//! defaults and only the stable slug is read. A placement guard
//! (`github.repository == 'owner/repo' && pool || fallback`) then branches the
//! way GitHub would in this checkout — and takes its fallback everywhere the
//! identity is unknown.
//!
//! *Run* identity is the one deliberate exception ([`head_identity`]):
//! `graph.params` are host-filled at run start and recorded in the graph, so
//! honest `sha`/`ref` values there change no lowering and break no replay —
//! and they are what lets `actions/checkout` fetch a commit that exists.

use std::fs;
use std::path::{Path, PathBuf};

use frontend::FileSource;
use serde_json::{Value, json};

/// The `owner/repo` slug the checkout's `origin` remote names, from
/// `.git/config`. `None` when there is no checkout, no origin, or no
/// recognizable URL.
pub fn repository_slug(files: &dyn FileSource) -> Option<String> {
    slug_from_config(&files.read(".git/config")?)
}

/// The slug out of a git config's `[remote "origin"]` url.
pub fn slug_from_config(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line.replace(' ', "") == "[remote\"origin\"]";
            continue;
        }
        if in_origin
            && let Some(rest) = line.strip_prefix("url")
            && let Some(url) = rest.trim_start().strip_prefix('=')
        {
            return slug_from_url(url.trim());
        }
    }
    None
}

/// `owner/repo` from a remote URL, whatever the scheme:
/// `git@github.com:owner/repo.git`, `https://host/owner/repo`, `ssh://…`.
fn slug_from_url(url: &str) -> Option<String> {
    let path = url.split_once("://").map_or(url, |(_, rest)| rest);
    // scp-like `git@host:owner/repo` puts the path after the colon.
    let path = path.rsplit_once(':').map_or(path, |(_, p)| p);
    let trimmed = path.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = trimmed.rsplit('/');
    let repo = parts.next()?;
    let owner = parts.next()?;
    let owner = owner.rsplit_once('@').map_or(owner, |(_, o)| o);
    // A GitHub owner never contains a dot; a host always does, so a URL too
    // short to carry an owner does not produce one.
    if repo.is_empty() || owner.is_empty() || owner.contains('.') {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// What the checkout's HEAD names, for run identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadIdentity {
    /// The commit HEAD resolves to, 40 hex characters.
    pub sha:       String,
    /// The branch HEAD is on (`refs/heads/<name>`); a detached HEAD has none.
    pub reference: Option<String>,
}

/// Read `<repo>/.git`'s HEAD: the branch and the commit it points at, through
/// loose refs, `packed-refs`, and a worktree's `gitdir:` indirection. `None`
/// without a checkout, or when any link in the chain is missing or malformed —
/// the caller keeps its fixed values, never a partial identity.
///
/// Direct file reads, like the slug from `.git/config` above: run parameters
/// are host business, outside the [`FileSource`] the lowering sees.
pub fn head_identity(repo: &Path) -> Option<HeadIdentity> {
    let (git_dir, common_dir) = git_dirs(repo)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        let reference = reference.trim().to_string();
        let sha = resolve_ref(&common_dir, &reference)?;
        return Some(HeadIdentity {
            sha,
            reference: Some(reference),
        });
    }
    is_full_sha(head).then(|| HeadIdentity {
        sha:       head.to_string(),
        reference: None,
    })
}

/// The repository's git directory and its common directory. `.git` is usually
/// the directory itself; in a linked worktree it is a file naming the real one
/// (`gitdir: …`), whose `commondir` file names where shared refs live.
fn git_dirs(repo: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot_git = repo.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let link = fs::read_to_string(&dot_git).ok()?;
        let target = link.strip_prefix("gitdir:")?.trim();
        let target = PathBuf::from(target);
        if target.is_absolute() {
            target
        } else {
            repo.join(target)
        }
    };
    let common_dir = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(common) => {
            let common = PathBuf::from(common.trim());
            if common.is_absolute() {
                common
            } else {
                git_dir.join(common)
            }
        }
        Err(_) => git_dir.clone(),
    };
    Some((git_dir, common_dir))
}

/// A ref's commit: the loose file when it exists, else its `packed-refs` line.
fn resolve_ref(common_dir: &Path, reference: &str) -> Option<String> {
    if let Ok(sha) = fs::read_to_string(common_dir.join(reference)) {
        let sha = sha.trim().to_string();
        return is_full_sha(&sha).then_some(sha);
    }
    let packed = fs::read_to_string(common_dir.join("packed-refs")).ok()?;
    for line in packed.lines() {
        // Comment header lines and `^` peel lines are not refs.
        if line.starts_with(['#', '^']) {
            continue;
        }
        if let Some((sha, name)) = line.split_once(' ')
            && name.trim() == reference
            && is_full_sha(sha)
        {
            return Some(sha.to_string());
        }
    }
    None
}

fn is_full_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The `github` context as this runner declares it: fixed values — a local run
/// is not a real GitHub event, and lowering twice must give the same graph —
/// plus the repository slug where one is known. The placement evaluator and
/// [`crate::GitHubActions::default_params`] both build from here, so a guard's
/// branch choice and a step's `github.repository` can never disagree;
/// `default_params` then layers the checkout's honest HEAD over the fixed
/// `sha`/`ref`, which is run identity and never enters lowering.
pub fn github_context(repository: Option<&str>) -> Value {
    let mut github = json!({
        "event_name": "workflow_dispatch",
        "actor": "petri",
        "ref": "refs/heads/main",
        "ref_name": "main",
        "sha": "0000000000000000000000000000000000000000",
        "run_id": "1",
        "run_number": "1",
        "run_attempt": "1",
        "server_url": "https://github.com",
        "api_url": "https://api.github.com",
        "graphql_url": "https://api.github.com/graphql",
    });
    if let Some(slug) = repository {
        github["repository"] = json!(slug);
        if let Some((owner, _)) = slug.split_once('/') {
            github["repository_owner"] = json!(owner);
        }
    }
    github
}

#[cfg(test)]
mod tests {
    use std::{env, fs, process};

    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A scratch directory removed on drop, so a failing test leaves nothing.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let dir = env::temp_dir()
                .join("petri-identity")
                .join(format!("{label}-{}", process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create the scratch dir");
            Self(dir)
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().expect("a parent")).expect("create parents");
            fs::write(path, contents).expect("write the fixture");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn head_resolves_through_loose_and_packed_refs() {
        let repo = Scratch::new("loose");
        repo.write(".git/HEAD", "ref: refs/heads/work\n");
        repo.write(".git/refs/heads/work", &format!("{SHA}\n"));
        assert_eq!(
            head_identity(&repo.0),
            Some(HeadIdentity {
                sha:       SHA.to_string(),
                reference: Some("refs/heads/work".to_string()),
            })
        );

        let repo = Scratch::new("packed");
        repo.write(".git/HEAD", "ref: refs/heads/main\n");
        repo.write(
            ".git/packed-refs",
            &format!(
                "# pack-refs with: peeled fully-peeled sorted\n\
                 {SHA} refs/heads/main\n\
                 ^aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
            ),
        );
        assert_eq!(head_identity(&repo.0).expect("resolves").sha, SHA);
    }

    #[test]
    fn a_detached_head_has_a_sha_and_no_reference() {
        let repo = Scratch::new("detached");
        repo.write(".git/HEAD", &format!("{SHA}\n"));
        assert_eq!(
            head_identity(&repo.0),
            Some(HeadIdentity {
                sha:       SHA.to_string(),
                reference: None,
            })
        );
    }

    #[test]
    fn a_worktree_resolves_through_gitdir_and_commondir() {
        let root = Scratch::new("worktree");
        root.write("main/.git/refs/heads/topic", &format!("{SHA}\n"));
        root.write("main/.git/worktrees/wt/HEAD", "ref: refs/heads/topic\n");
        root.write("main/.git/worktrees/wt/commondir", "../..\n");
        root.write("wt/.git", "gitdir: ../main/.git/worktrees/wt\n");
        assert_eq!(
            head_identity(&root.0.join("wt")),
            Some(HeadIdentity {
                sha:       SHA.to_string(),
                reference: Some("refs/heads/topic".to_string()),
            })
        );
    }

    #[test]
    fn a_missing_or_broken_chain_yields_no_identity() {
        let repo = Scratch::new("broken");
        assert_eq!(head_identity(&repo.0), None, "no .git at all");
        repo.write(".git/HEAD", "ref: refs/heads/gone\n");
        assert_eq!(head_identity(&repo.0), None, "the ref resolves nowhere");
        repo.write(".git/refs/heads/gone", "not-a-sha\n");
        assert_eq!(head_identity(&repo.0), None, "a malformed sha is refused");
    }

    #[test]
    fn slugs_parse_from_every_common_url_shape() {
        for (config, want) in [
            (
                "[remote \"origin\"]\n\turl = git@github.com:astral-sh/ruff.git\n",
                Some("astral-sh/ruff"),
            ),
            (
                "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = https://github.com/vercel/next.js\n",
                Some("vercel/next.js"),
            ),
            (
                "[remote \"origin\"]\n\turl = ssh://git@github.com/owner/repo.git\n",
                Some("owner/repo"),
            ),
            // The upstream remote is not origin.
            (
                "[remote \"upstream\"]\n\turl = git@github.com:a/b.git\n",
                None,
            ),
            // Too short to carry an owner.
            (
                "[remote \"origin\"]\n\turl = https://github.com/only\n",
                None,
            ),
        ] {
            assert_eq!(slug_from_config(config).as_deref(), want, "{config}");
        }
    }
}
