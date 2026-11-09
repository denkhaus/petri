//! The environment image build (ADR-0042, the fabro plan
//! 2026-11-09-run-environment-image-build): an environment may declare
//! a Dockerfile; the engine builds that image at scope acquisition,
//! BEFORE the scope's sandbox is created, and the scope runs on the
//! built tag.
//!
//! Why a holder sandbox: the docker provider's image build is a
//! ONE-SHOT capability — it reads the build context out of an existing
//! sandbox's filesystem (the daemon archive API) and builds daemon-side
//! (`sandbox-driver-docker/src/one_shot.rs`). A scope's own image
//! cannot be built inside itself, so the build rides a THROWAWAY
//! holder: create (busybox), upload the packed context, extract it
//! with a one-shot, build with a one-shot (`reuse` short-circuits on a
//! tag hit), then stop and delete the holder. Everything uses driver
//! capabilities that already exist; no protocol change rides this.
//!
//! The tag is the cache: `petri-env-<hash12>` over the Dockerfile text
//! and the packed context, so an unchanged environment costs one
//! holder lifecycle and a tag-presence check, and the daemon's layer
//! cache makes even a changed context cheap where layers did not move.
//!
//! Known v1 residuals (accepted in the plan): the context upload
//! happens before the `reuse` check can see the tag, so a cache hit
//! still packs and uploads once per acquire; and the build one-shot
//! runs a trivial `/bin/sh -c 'exit 0'` from the freshly built tag, so
//! an environment image without a shell cannot be built this way.

use std::path::Path;
use std::process::Command as StdCommand;
use std::sync::Arc;

use sandbox_driver::{
    Error as DriverError, ExecControls, OneShotImage, OneShotSpec, Sandbox, SandboxProvider,
    SandboxSource, SandboxSpec,
};
use sha2::{Digest as _, Sha256};

use crate::EnvError;

/// The holder's base: the smallest image that can run `tar` and `sh`.
/// The daemon pulls it once; it exists only to hold the context.
const HOLDER_IMAGE: &str = "busybox:1.36";
/// Where the packed context lands inside the holder, and where it is
/// extracted to. Both live UNDER THE HOLDER'S WORKSPACE: a one-shot
/// container sees the holder's filesystem only through the workspace
/// mount, so the extract one-shot finds the tar exactly there, and the
/// build's context read (which resolves against the holder) sees the
/// extracted tree at the same path.
const CONTEXT_TAR: &str = "/workspace/petri-env-context.tar";
const CONTEXT_DIR: &str = "/workspace/petri-env-context";
/// The Dockerfile's name inside the context (the build reads
/// `context/Dockerfile`).
const DOCKERFILE_NAME: &str = "Dockerfile";
/// The label that marks the holder as build machinery, never a run
/// workspace a lease reconcile could mistake for a lost sandbox.
const ROLE_LABEL: &str = "petri.role";
const ROLE_VALUE: &str = "env-build";

/// What an environment's build declaration carries: the inline
/// Dockerfile text and the host-side context root (the run's bound
/// repository).
#[derive(Clone, Debug)]
pub(crate) struct EnvironmentBuild {
    pub dockerfile: String,
    pub context:    String,
}

/// Ensures the environment image exists on the daemon and returns the
/// tag the scope must run on. A failure fails the scope's acquisition
/// (a routable error; the run records `scope.failed`).
pub(crate) async fn ensure_environment_image(
    provider: &Arc<dyn SandboxProvider>,
    build: &EnvironmentBuild,
    holder_name: &str,
) -> Result<String, EnvError> {
    let dockerfile = build.dockerfile.clone();
    let context = build.context.clone();
    let context_tar = tokio::task::spawn_blocking(move || pack_context(Path::new(&context)))
        .await
        .map_err(|error| backend_error(format!("the context pack task failed: {error}")))?
        .map_err(|message| backend_error(format!("packing the build context failed: {message}")))?;

    let mut hash = Sha256::new();
    hash.update(dockerfile.as_bytes());
    hash.update(&context_tar);
    let digest = hash.finalize();
    let hex: String = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let tag = format!("petri-env-{hex}");

    let holder = create_holder(provider, holder_name)
        .await
        .map_err(|error| backend_error(format!("creating the build holder failed: {error}")))?;
    let outcome = build_on_holder(&holder, &dockerfile, &context_tar, &tag).await;
    // The holder is machinery: it never outlives the attempt, whatever
    // the attempt's result.
    if let Err(problem) = holder.stop().await {
        tracing::warn!(%tag, %problem, "env image build: stopping the holder failed");
    }
    if let Err(problem) = holder.delete().await {
        tracing::warn!(%tag, %problem, "env image build: deleting the holder failed");
    }
    outcome?;
    Ok(tag)
}

/// Creates the holder sandbox. Its labels mark it as build machinery;
/// nothing about it matches a run workspace label, so lease
/// reconciliation never adopts it.
async fn create_holder(
    provider: &Arc<dyn SandboxProvider>,
    name: &str,
) -> Result<Arc<dyn Sandbox>, DriverError> {
    let mut spec = SandboxSpec::new(SandboxSource::Image {
        reference: HOLDER_IMAGE.to_owned(),
    })
    .name(name.to_owned())
    .working_directory("/workspace".to_owned());
    spec.labels
        .insert(ROLE_LABEL.to_owned(), ROLE_VALUE.to_owned());
    let holder = provider.create(&spec, None).await?;
    if holder.one_shot().is_none() {
        // Free the just-created sandbox before refusing: an unusable
        // holder must not leak as a running container.
        let _ = holder.stop().await;
        let _ = holder.delete().await;
        return Err(DriverError::unsupported(
            sandbox_driver::Capability::OneShot,
        ));
    }
    Ok(holder)
}

/// Uploads the context, then runs the extract and build one-shots on
/// the holder. The build one-shot exits trivially from the built tag —
/// the image build is its side effect, which is why the tag must carry
/// a shell (v1 residual, see the module docs).
async fn build_on_holder(
    holder: &Arc<dyn Sandbox>,
    dockerfile: &str,
    context_tar: &[u8],
    tag: &str,
) -> Result<(), EnvError> {
    let one_shot = holder
        .one_shot()
        .ok_or_else(|| backend_error("the holder lost its one-shot capability".to_owned()))?;

    holder
        .fs()
        .write(CONTEXT_TAR, context_tar)
        .await
        .map_err(|error| backend_error(format!("uploading the build context failed: {error}")))?;

    let extract = OneShotSpec::new(OneShotImage::Registry {
        reference: HOLDER_IMAGE.to_owned(),
    })
    .entrypoint("/bin/sh")
    .args([
        "-c",
        &format!("mkdir -p {CONTEXT_DIR} && tar -xf {CONTEXT_TAR} -C {CONTEXT_DIR}"),
    ]);
    run_one_shot(one_shot, &extract, "extracting the build context").await?;

    holder
        .fs()
        .write(
            &format!("{CONTEXT_DIR}/{DOCKERFILE_NAME}"),
            dockerfile.as_bytes(),
        )
        .await
        .map_err(|error| backend_error(format!("writing the Dockerfile failed: {error}")))?;

    let build = OneShotSpec::new(OneShotImage::Build {
        context:    CONTEXT_DIR.to_owned(),
        dockerfile: Some(DOCKERFILE_NAME.to_owned()),
        tag:        tag.to_owned(),
        reuse:      true,
    })
    .entrypoint("/bin/sh")
    .args(["-c", "exit 0"]);
    run_one_shot(one_shot, &build, "building the environment image").await
}

/// Runs one one-shot to completion and checks it ended by exiting 0.
async fn run_one_shot(
    one_shot: &dyn sandbox_driver::OneShot,
    spec: &OneShotSpec,
    what: &str,
) -> Result<(), EnvError> {
    let result = one_shot
        .run(spec, ExecControls::default())
        .await
        .map_err(|error| backend_error(format!("{what} failed: {error}")))?;
    if result.result.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&result.result.stderr);
    Err(backend_error(format!(
        "{what} failed: {}{}",
        result
            .result
            .exit_code
            .map(|code| format!("exit {code}"))
            .unwrap_or_else(|| "no exit observed".to_owned()),
        if stderr.trim().is_empty() {
            String::new()
        } else {
            format!(" — {}", stderr.trim())
        }
    )))
}

/// Packs the context host-side: a fresh clone of the repository at
/// `root`, tarred WITHOUT `.git` (a build context wants the committed
/// tree, not the history; the checkout seed's `pack_clone` in
/// attractor/steps keeps `.git` because a workspace needs it — same
/// dance, different keep-list). Returns the tar bytes.
fn pack_context(root: &Path) -> Result<Vec<u8>, String> {
    if !root.join(".git").exists() {
        return Err(format!(
            "{} carries no .git — the build context must be a git checkout",
            root.display()
        ));
    }
    let temp = std::env::temp_dir().join(format!(
        "petri-env-ctx-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or_default()
    ));
    let clone = temp.join("clone");
    let status = StdCommand::new("git")
        .args([
            "clone",
            "--quiet",
            "--no-hardlinks",
            &format!("file://{}", root.display()),
            clone.to_string_lossy().as_ref(),
        ])
        .status()
        .map_err(|error| format!("running git clone failed: {error}"))?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&temp);
        return Err("git clone of the context failed".to_owned());
    }

    let mut archive = tar::Builder::new(Vec::new());
    append_tree(&mut archive, &clone, "")
        .map_err(|error| format!("packing the context failed: {error}"))?;
    let bytes = archive
        .into_inner()
        .map_err(|error| format!("finishing the context tar failed: {error}"))?;
    let _ = std::fs::remove_dir_all(&temp);
    Ok(bytes)
}

/// Appends `dir`'s contents under `prefix`, skipping `.git` at every
/// level and `.dockerignore` at the root: the packed context is a plain
/// source tree the DAEMON builds, and the repository's own
/// `.dockerignore` (tuned for the runtime image's staged-binary
/// context) would exclude everything from an environment build — the
/// daemon applies it when it unpacks the context tar.
fn append_tree(
    archive: &mut tar::Builder<Vec<u8>>,
    dir: &Path,
    prefix: &str,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" || (name == ".dockerignore" && prefix.is_empty()) {
            continue;
        }
        let path = entry.path();
        let arc = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let metadata = std::fs::metadata(&path)?;
        if metadata.is_dir() {
            archive.append_dir(&arc, &path)?;
            append_tree(archive, &path, &arc)?;
        } else {
            archive.append_path_with_name(&path, &arc)?;
        }
    }
    Ok(())
}

/// The build's error shape: a backend error under the sandbox backend,
/// staged as `image-build` so a failed acquire names the image build,
/// not an unrelated sandbox create.
fn backend_error(message: String) -> EnvError {
    EnvError::backend(crate::BACKEND, "image-build", message)
}
