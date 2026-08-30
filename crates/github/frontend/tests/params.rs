//! Run identity: what `default_params` hands a host. The slug is the stable
//! `origin` remote; `sha`/`ref`/`ref_name` are the checkout's honest HEAD —
//! run parameters, never lowering inputs — and every read failure falls back to
//! the fixed values, so a run without a checkout still has a full identity.

use std::path::PathBuf;
use std::{env, fs, process};

use frontend::Frontend;
use frontend_gha::GitHubActions;

const SHA: &str = "89abcdef0123456789abcdef0123456789abcdef";

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let dir = env::temp_dir()
            .join("petri-params")
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

fn github_param(repo: &Scratch) -> serde_json::Value {
    GitHubActions::new()
        .default_params(&repo.0)
        .into_iter()
        .find(|(key, _)| key == "github")
        .expect("a github param")
        .1
}

#[test]
fn a_checkout_yields_its_slug_and_honest_head() {
    let repo = Scratch::new("checkout");
    repo.write(
        ".git/config",
        "[remote \"origin\"]\n\turl = git@github.com:octo/widget.git\n",
    );
    repo.write(".git/HEAD", "ref: refs/heads/topic\n");
    repo.write(".git/refs/heads/topic", &format!("{SHA}\n"));

    let github = github_param(&repo);
    assert_eq!(github["repository"], "octo/widget");
    assert_eq!(github["sha"], SHA);
    assert_eq!(github["ref"], "refs/heads/topic");
    assert_eq!(github["ref_name"], "topic");
}

#[test]
fn without_a_checkout_the_fixed_identity_stands() {
    let repo = Scratch::new("bare");
    let github = github_param(&repo);
    assert_eq!(github["sha"], "0000000000000000000000000000000000000000");
    assert_eq!(github["ref"], "refs/heads/main");
    assert_eq!(github["ref_name"], "main");
}

#[test]
fn a_detached_head_updates_the_sha_and_keeps_the_fixed_ref() {
    let repo = Scratch::new("detached");
    repo.write(".git/HEAD", &format!("{SHA}\n"));
    let github = github_param(&repo);
    assert_eq!(github["sha"], SHA);
    assert_eq!(github["ref"], "refs/heads/main");
}
