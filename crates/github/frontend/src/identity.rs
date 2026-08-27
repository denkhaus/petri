//! The checkout's identity: what `github.repository` means here.
//!
//! The one honest source is the checkout's own git configuration — the
//! `origin` remote names the repository the way GitHub would — read through
//! the same [`FileSource`] the rest of the lowering uses, so a corpus
//! checkout or a test map simply has none and the repository stays unnamed.
//! Deliberately never HEAD: a commit must not change what a file lowers to,
//! so `ref` and `sha` keep their fixed defaults and only the stable slug is
//! read. A placement guard (`github.repository == 'owner/repo' && pool ||
//! fallback`) then branches the way GitHub would in this checkout — and takes
//! its fallback everywhere the identity is unknown.

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

/// The `github` context as this runner declares it: fixed values — a local run
/// is not a real GitHub event, and lowering twice must give the same graph —
/// plus the repository slug where one is known. The placement evaluator and
/// [`crate::GitHubActions::default_params`] both build from here, so a guard's
/// branch choice and a step's `github.repository` can never disagree.
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
    use super::*;

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
            ("[remote \"origin\"]\n\turl = https://github.com/only\n", None),
        ] {
            assert_eq!(slug_from_config(config).as_deref(), want, "{config}");
        }
    }
}
