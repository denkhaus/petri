//! One-shot containers, run in a scope's world.
//!
//! A [`ContainerRunner`] is bound to one scope at acquisition and rides the
//! acquisition return ([`EnvHandle`]): a step can only ever run containers in
//! the scope it belongs to — correct network attachment, the scope's workspace
//! mounted, the run's image cache and cleanup fence — and a step whose executor
//! provided no runner fails with one clear error instead of finding a daemon by
//! other means. The consumer (a Docker-action step, say) assembles *what* to
//! run and never learns how.
//!
//! [`EnvHandle`]: crate::EnvHandle

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use smol_str::SmolStr;

use crate::env::ProcessHandle;
use crate::error::EnvError;

/// The failure class when a step needs a one-shot container and its scope's
/// executor provided no runner, or the container runtime is unreachable.
/// Routable, like `capability_unavailable`: the step fails its node, never the
/// run machinery.
pub const CONTAINER_RUNTIME_CLASS: ir::FailureClass =
    ir::FailureClass::new_static("container_runtime");

/// Where a one-shot container's image comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerImage {
    /// A registry image, pulled when absent.
    Registry { image: SmolStr },
    /// Built from a Dockerfile under `context` (workspace-relative).
    Build {
        context:    PathBuf,
        /// The Dockerfile within the context; `None` is the context's own
        /// `Dockerfile`.
        dockerfile: Option<SmolStr>,
        /// The image tag the build produces — the cache key.
        tag:        SmolStr,
        /// Whether an image already carrying `tag` may be reused without
        /// building. True for content-addressed tags (a pinned commit); false
        /// when the context can change under the same tag (a local action's
        /// first build of the run).
        reuse:      bool,
    },
}

/// What one one-shot container runs. The runner supplies the scope's world —
/// workspace mount, network, naming — so nothing here names a host path.
///
/// `Debug` is hand-written for the same reason [`crate::ProcessSpec`]'s is:
/// `env` holds the plaintext a step's `{"$secret": ...}` references resolved
/// to, and `args` can carry a secret a caller substituted into them, so the
/// shape prints and the values do not.
#[derive(Clone, PartialEq, Eq)]
pub struct OneShotContainer {
    pub image:      ContainerImage,
    /// Override the image's entrypoint (one executable; arguments go in
    /// `args`).
    pub entrypoint: Option<SmolStr>,
    /// Arguments to the entrypoint.
    pub args:       Vec<SmolStr>,
    pub env:        BTreeMap<SmolStr, SmolStr>,
    /// Working directory inside the container, absolute. `None` means the
    /// workspace mount ([`ContainerRunner::workspace_path`]).
    pub workdir:    Option<SmolStr>,
}

impl OneShotContainer {
    pub fn registry(image: &str) -> Self {
        Self {
            image:      ContainerImage::Registry {
                image: SmolStr::new(image),
            },
            entrypoint: None,
            args:       Vec::new(),
            env:        BTreeMap::new(),
            workdir:    None,
        }
    }

    pub fn build(context: impl Into<PathBuf>, tag: &str) -> Self {
        Self {
            image:      ContainerImage::Build {
                context:    context.into(),
                dockerfile: None,
                tag:        SmolStr::new(tag),
                reuse:      true,
            },
            entrypoint: None,
            args:       Vec::new(),
            env:        BTreeMap::new(),
            workdir:    None,
        }
    }

    #[must_use]
    pub fn with_entrypoint(mut self, entrypoint: &str) -> Self {
        self.entrypoint = Some(SmolStr::new(entrypoint));
        self
    }

    #[must_use]
    pub fn with_args(mut self, args: &[&str]) -> Self {
        self.args = args.iter().map(|a| SmolStr::new(*a)).collect();
        self
    }

    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<SmolStr, SmolStr>) -> Self {
        self.env = env;
        self
    }

    #[must_use]
    pub fn with_workdir(mut self, workdir: &str) -> Self {
        self.workdir = Some(SmolStr::new(workdir));
        self
    }
}

impl fmt::Debug for OneShotContainer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OneShotContainer")
            .field("image", &self.image)
            .field("entrypoint", &self.entrypoint)
            .field("args", &self.args.len())
            .field("env", &self.env.keys())
            .field("workdir", &self.workdir)
            .finish()
    }
}

/// Run one-shot containers in one scope's world. Bound at acquisition; the
/// handle that comes back is the same [`ProcessHandle`] every process gets, so
/// waiting, output capture and the cancel ladder are written once.
#[async_trait]
pub trait ContainerRunner: Send + Sync {
    /// The scope's workspace root as a one-shot container sees it (its mount
    /// point), for building paths to hand to the container.
    fn workspace_path(&self) -> &str;

    /// How a one-shot container reaches the driver's machine — the container
    /// side of [`crate::ExecEnv::host_address`]. The provider guarantees the
    /// answer resolves inside the containers it runs (`--add-host` at create).
    fn host_address(&self) -> &str;

    /// Pull or build the image as needed, then run the container. The exit
    /// status is the container's own.
    async fn run(&self, spec: OneShotContainer) -> Result<Box<dyn ProcessHandle>, EnvError>;
}
