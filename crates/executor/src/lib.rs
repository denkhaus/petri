//! Environments for running steps.
//!
//! An [`Executor`] materializes the environment for a scope instance — a workspace on
//! this machine, or a container with that workspace bind-mounted. It hands out an
//! [`ExecEnv`] capability, and step kinds are written once against that capability
//! and never mention Docker.
//!
//! Everything in this crate signals **process groups**, never individual pids. A
//! `run:` script that backgrounds children has to die as a unit.

pub mod docker;
pub mod env;
pub mod error;
pub mod host;
pub mod scope;
pub mod secrets;

pub use docker::{CONTAINER_WORKSPACE, DockerExecutor, PullPolicy, list_containers};
pub use env::{ExecEnv, ExitStatus, LineStream, LogLine, ProcessHandle, ProcessSpec, Sig};
pub use error::{EnvError, ReleaseReport};
pub use host::{HostExecutor, LINE_CAP};
pub use scope::{DEFAULT_GRACE, EnvHandle, Executor, Retention, ScopeOutcome, ScopeSpec, Teardown};
pub use secrets::{
    MASK, MIN_MASK_LENGTH, MapSecrets, Masker, SECRET_REF_KEY, SecretError, SecretProvider,
};
