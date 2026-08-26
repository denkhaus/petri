//! The interface between the driver and the environments that run steps.
//!
//! An [`Executor`] materializes the environment for a scope instance and hands out an
//! [`ExecEnv`] capability. Step kinds are written once against that capability and
//! never mention where a process actually runs. The executors themselves live in
//! their own crates — `executor-host` and `executor-docker` — and this crate knows
//! nothing about either: a new kind of environment is a new crate, not an edit here.
//!
//! Everything in this crate signals **process groups**, never individual pids. A
//! `run:` script that backgrounds children has to die as a unit.
//!
//! [`lines`] is support for implementing the interface rather than part of it: the
//! line-capped output pump every executor needs, kept here so it is written once.

pub mod env;
pub mod error;
pub mod lines;
pub mod scope;
pub mod secrets;

pub use env::{ExecEnv, ExitStatus, LineStream, LogLine, ProcessHandle, ProcessSpec, Sig};
pub use error::{EnvError, ReleaseReport};
pub use lines::LINE_CAP;
pub use scope::{DEFAULT_GRACE, EnvHandle, Executor, Retention, ScopeOutcome, ScopeSpec, Teardown};
pub use secrets::{MASK, MIN_MASK_LENGTH, MapSecrets, Masker, SecretError, SecretProvider};
