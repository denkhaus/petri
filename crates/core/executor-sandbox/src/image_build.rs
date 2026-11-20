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
//! and a context MANIFEST (work#1440: path/size/mtime per file, not
//! the packed tar — the tar is packed only when the cook must build),
//! so an unchanged environment costs one manifest walk, and the
//! daemon's layer cache makes even a changed context cheap where
//! layers did not move. Concurrent first cooks of the same tag
//! single-flight through a process-wide guard (work#1440): the first
//! caller builds, the rest block and reuse; and because the cooks run
//! one process PER GEAR (work#1463), the post-ledger-miss path
//! re-checks the daemon itself for the tag and reuses when another
//! process already cooked it. After every successful cook a mint-time
//! eviction caps the `petri-env` population at the newest few images,
//! so concurrent gears no longer mint one full layer set each and
//! fill the Docker disk.
//!
//! Known v2 residuals (accepted): the build one-shot runs a trivial
//! `/bin/sh -c 'exit 0'` from the freshly built tag, so an environment
//! image without a shell cannot be built this way; and the in-process
//! fast path trusts the ledger — an out-of-band `docker rmi` between
//! cook and use surfaces as an image-missing sandbox create, not as a
//! rebuild (the daemon-side sweep keeps its evictions in sync through
//! `forget`).

use std::path::Path;
use std::process::{Command as StdCommand, id};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, io};

use sandbox_driver::{
    Error as DriverError, ExecControls, OneShotImage, OneShotSpec, Sandbox, SandboxProvider,
    SandboxSource, SandboxSpec,
};
use tokio::task::spawn_blocking;

use crate::{EnvError, image_flight};

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
///
/// Single-flight per tag (work#1440): concurrent first cooks of the
/// same tag yield exactly one full build — the rest block on the
/// process-wide guard and reuse the built image.
pub(crate) async fn ensure_environment_image(
    provider: &Arc<dyn SandboxProvider>,
    build: &EnvironmentBuild,
    holder_name: &str,
) -> Result<String, EnvError> {
    let dockerfile = build.dockerfile.clone();
    let context = build.context.clone();
    // The digest walks the tree, not the tar: cheap enough to pay on
    // every cook, and the tar is packed only on a manifest miss.
    let digest_context = context.clone();
    let digest_dockerfile = dockerfile.clone();
    let hex = spawn_blocking(move || {
        image_flight::manifest_digest(&digest_dockerfile, Path::new(&digest_context))
    })
    .await
    .map_err(|error| backend_error(format!("the manifest digest task failed: {error}")))?
    .map_err(|message| backend_error(format!("hashing the build context failed: {message}")))?;
    let tag = format!("{}{hex}", image_flight::ENV_TAG_PREFIX);

    // The single-flight seam: the first caller of this tag holds the
    // guard through its build; every concurrent caller blocks here and
    // proceeds once the image exists. Process-LOCAL (work#1463): the
    // cooks run one process per gear, so the guard and the ledger
    // below only serialize THIS process's cooks — cross-process
    // dedup rides the daemon presence probe after the ledger miss.
    let guard = image_flight::guard_for(&tag);
    let _single_flight = guard.lock().await;
    // A tag THIS process already cooked needs no holder, no pack, no
    // build one-shot — the ledger dropped it if the sweep evicted it.
    if image_flight::is_built(&tag) {
        tracing::debug!(%tag, "env image cook: in-process fast path");
        return Ok(tag);
    }

    let holder = create_holder(provider, holder_name)
        .await
        .map_err(|error| backend_error(format!("creating the build holder failed: {error}")))?;
    // Cross-process presence re-check (work#1463): another gear's
    // process may already have cooked this tag — the daemon is the
    // shared truth. A hit skips the pack and the build entirely.
    #[cfg(feature = "env-image-evict")]
    {
        let one_shot = holder
            .one_shot()
            .ok_or_else(|| backend_error("the holder lost its one-shot capability".to_owned()))?;
        if cook_path_after_ledger_miss(one_shot, &tag).await == CookPath::Reuse {
            retire_holder(&holder, &tag).await;
            return Ok(tag);
        }
    }
    // Feature-off builds (upstream sandbox-driver, no image_present)
    // and probe failures fall through to the build path.

    let context_tar = spawn_blocking(move || pack_context(Path::new(&context)))
        .await
        .map_err(|error| backend_error(format!("the context pack task failed: {error}")))?
        .map_err(|message| backend_error(format!("packing the build context failed: {message}")))?;

    let outcome = build_on_holder(&holder, &dockerfile, &context_tar, &tag).await;
    if outcome.is_ok() {
        image_flight::note_built(&tag);
        // Mint-time eviction (work#1440): rides the SAME holder — it is
        // the only driver seam the cook holds — so it must run before
        // the holder goes away. Gated on the fork feature: upstream
        // sandbox-driver has no `evict_lru_images`.
        #[cfg(feature = "env-image-evict")]
        evict_beyond_ceiling(&holder).await;
    }
    // The holder is machinery: it never outlives the attempt, whatever
    // the attempt's result.
    retire_holder(&holder, &tag).await;
    outcome?;
    Ok(tag)
}

/// Stops and deletes the holder — machinery that never outlives the
/// cook attempt, whatever the attempt's result.
async fn retire_holder(holder: &Arc<dyn Sandbox>, tag: &str) {
    if let Err(problem) = holder.stop().await {
        tracing::warn!(%tag, %problem, "env image build: stopping the holder failed");
    }
    if let Err(problem) = holder.delete().await {
        tracing::warn!(%tag, %problem, "env image build: deleting the holder failed");
    }
}

/// The path a post-ledger-miss cook takes after the daemon presence
/// probe (work#1463).
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(
    not(feature = "env-image-evict"),
    allow(
        dead_code,
        reason = "driven only through the fork probe; the pins exercise it in tests"
    )
)]
pub(crate) enum CookPath {
    /// The daemon already holds the tag — reuse it, no build.
    Reuse,
    /// Build the image.
    Build,
}

/// The cross-process single-flight DECISION (work#1463), pure over the
/// probe's outcome so the fork-only pins can drive it without a
/// daemon: after the in-process ledger misses, the daemon is
/// re-checked for the tag. A presence hit means another process's
/// cook already minted it — the tag joins the fast-path ledger and
/// the cook reuses it with no pack and no build. A probe failure
/// (including providers without the facet) fails OPEN to `Build`: the
/// worst case is a duplicate build whose layers the daemon dedupes,
/// never a stuck cook.
#[cfg_attr(
    not(feature = "env-image-evict"),
    allow(
        dead_code,
        reason = "driven only through the fork probe; the pins exercise it in tests"
    )
)]
pub(crate) fn cook_path(probe: Result<bool, DriverError>, tag: &str) -> CookPath {
    match probe {
        Ok(true) => {
            tracing::info!(%tag, "env image cook: the daemon already holds the tag — reusing");
            image_flight::note_built(tag);
            CookPath::Reuse
        }
        Ok(false) => CookPath::Build,
        Err(problem) => {
            tracing::warn!(
                %tag,
                %problem,
                "env image cook: daemon presence probe unavailable — building"
            );
            CookPath::Build
        }
    }
}

/// Runs the daemon presence probe and maps its outcome through
/// [`cook_path`]. Gated on the fork feature exactly like the eviction
/// sweep: upstream sandbox-driver has no `OneShot::image_present`.
#[cfg(feature = "env-image-evict")]
async fn cook_path_after_ledger_miss(
    one_shot: &dyn sandbox_driver::OneShot,
    tag: &str,
) -> CookPath {
    cook_path(one_shot.image_present(tag).await, tag)
}

/// Caps the daemon's `petri-env` population at the newest
/// [`image_flight::ENV_IMAGE_CEILING`] images after a successful cook.
/// Eviction is a backstop the cook must survive: unsupported providers
/// and daemon errors are warnings, and the evicted references leave
/// the in-process fast path so a later cook re-adjudicates presence.
#[cfg(feature = "env-image-evict")]
async fn evict_beyond_ceiling(holder: &Arc<dyn Sandbox>) {
    let Some(one_shot) = holder.one_shot() else {
        return;
    };
    match one_shot
        .evict_lru_images(
            image_flight::ENV_TAG_PREFIX,
            image_flight::ENV_IMAGE_CEILING,
        )
        .await
    {
        Ok(evicted) if evicted.is_empty() => {}
        Ok(evicted) => {
            tracing::info!(
                count = evicted.len(),
                ?evicted,
                "env image cook: mint-time eviction"
            );
            image_flight::forget(&evicted);
        }
        Err(problem) => tracing::warn!(
            %problem,
            "env image cook: mint-time eviction unavailable"
        ),
    }
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
        result.result.exit_code.map_or_else(
            || "no exit observed".to_owned(),
            |code| format!("exit {code}")
        ),
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
    let temp = env::temp_dir().join(format!(
        "petri-env-ctx-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
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
        let _ = fs::remove_dir_all(&temp);
        return Err("git clone of the context failed".to_owned());
    }

    let mut archive = tar::Builder::new(Vec::new());
    append_tree(&mut archive, &clone, "")
        .map_err(|error| format!("packing the context failed: {error}"))?;
    let bytes = archive
        .into_inner()
        .map_err(|error| format!("finishing the context tar failed: {error}"))?;
    let _ = fs::remove_dir_all(&temp);
    Ok(bytes)
}

/// Appends `dir`'s contents under `prefix`, skipping `.git` at every
/// level and `.dockerignore` at the root: the packed context is a plain
/// source tree the DAEMON builds, and the repository's own
/// `.dockerignore` (tuned for the runtime image's staged-binary
/// context) would exclude everything from an environment build — the
/// daemon applies it when it unpacks the context tar.
fn append_tree(archive: &mut tar::Builder<Vec<u8>>, dir: &Path, prefix: &str) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
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
        let metadata = fs::metadata(&path)?;
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
