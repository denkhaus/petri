//! Launching, verifying, and supervising sandbox-driver plugins.
//!
//! Petri reaches every sandbox provider through sandbox-driver's JSON-RPC
//! plugin protocol, its own first-party providers included: a third-party
//! provider can only ever be a plugin, so Petri's own take the same path
//! rather than a privileged in-process one. One [`PluginSettings`] per
//! provider kind says where the executable is, what its checksum must be,
//! and what a sandbox uses to reach services on this machine; one
//! [`PluginSupervisor`] per kind owns the running process.
//!
//! # Resolution and trust
//!
//! The executable is `PETRI_SANDBOX_<KIND>_PLUGIN` when set, else
//! `sandbox-driver-<kind>` beside Petri's own executable (a release bundle),
//! else `sandbox-driver-<kind>` on `PATH`. Its SHA-256 must match the
//! target-matched pin compiled into this build, or
//! `PETRI_SANDBOX_<KIND>_SHA256` when that overrides it. An unpinned executable
//! launches only in dev mode — `PETRI_SANDBOX_PLUGIN_DEV=1`, or the CLI's
//! `--sandbox-plugin-dev` — which a debug build turns on by default so
//! development works before a first release. Dev mode is logged clearly when it
//! lets an unverified plugin run.
//!
//! The plugin starts from an empty environment. Only named variables are
//! forwarded: `PATH` and `HOME`, the Docker daemon selection and its TLS
//! companions, and Daytona's credentials. The values are captured with the
//! settings and reused at each launch. Credentials are not persisted or
//! included in the settings' debug output.
//!
//! # Supervision
//!
//! A supervisor owns the plugin process and a generation number. When the
//! transport closes, every in-flight call fails routably through the
//! protocol client, and no call is ever replayed: a mutating call whose
//! outcome is unknown is reconciled by the lease manager from durable
//! records and provider labels, not by trying again. The next provider call
//! launches one new process, single-flight, with the same kind, path,
//! checksum, and environment, and hands back a new generation; the lease
//! manager fences every sandbox once before a holder resumes on it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, fmt};

use executor::EnvError;
use sandbox_driver::{HealthStatus, ProviderKind, SandboxProvider};
use sandbox_driver_protocol::PluginProvider;
use sandbox_driver_protocol::discovery::{PluginConfig, launch_plugin};
use tokio::sync::Mutex;

use crate::DOCKER_HOST_ALIAS;

/// The naming prefix plugin discovery searches `PATH` for:
/// `sandbox-driver-<kind>`.
pub const PLUGIN_PREFIX: &str = "sandbox-driver";
/// Turns unpinned plugins on for every kind.
pub const DEV_MODE_VAR: &str = "PETRI_SANDBOX_PLUGIN_DEV";

// Generated from the release bundle's plugin binaries. Ordinary development
// builds have no pins and use the explicit override or development policy.
include!(concat!(env!("OUT_DIR"), "/plugin_pins.rs"));

/// What the plugin process inherits, per kind. Everything else is scrubbed.
fn forwarded_env(kind: &str) -> Vec<&'static str> {
    // `RUST_LOG` too: the plugin's stderr is the host's, and an operator
    // turning logging up expects to see the plugin's side of a call.
    let mut vars = vec!["PATH", "HOME", "RUST_LOG"];
    match kind {
        "docker" => vars.extend([
            "DOCKER_HOST",
            "DOCKER_TLS_VERIFY",
            "DOCKER_CERT_PATH",
            "DOCKER_API_VERSION",
            "DOCKER_CONFIG",
        ]),
        "daytona" => vars.extend([
            "DAYTONA_API_KEY",
            "DAYTONA_JWT_TOKEN",
            "DAYTONA_ORGANIZATION_ID",
            "DAYTONA_API_URL",
            "DAYTONA_TARGET",
        ]),
        _ => {}
    }
    vars
}

/// Why a plugin could not be configured or launched.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("`{0}` is not a provider kind")]
    Kind(String),
    #[error(
        "the {kind} plugin has no pinned checksum for this build; set \
         PETRI_SANDBOX_{upper}_SHA256, or allow an unverified plugin with \
         PETRI_SANDBOX_PLUGIN_DEV=1 (--sandbox-plugin-dev)"
    )]
    Unpinned { kind: String, upper: String },
    #[error(
        "the docker daemon at `{docker_host}` is remote, so a sandbox cannot reach this \
         machine by inference; set PETRI_SANDBOX_DOCKER_HOST_ADDRESS to the host name or \
         address the daemon's containers can reach Petri at"
    )]
    RemoteDaemonNeedsHostAddress { docker_host: String },
    #[error("launching the {kind} plugin failed")]
    Launch {
        kind:   String,
        #[source]
        source: Box<sandbox_driver::Error>,
    },
    #[error("the {kind} plugin reports its backend {status}: {message}")]
    Unhealthy {
        kind:    String,
        status:  &'static str,
        message: String,
    },
}

impl PluginError {
    /// The routable form: every plugin failure surfaces at acquire.
    pub fn into_env_error(self) -> EnvError {
        EnvError::backend("sandbox", "plugin", self.to_string())
    }
}

/// Where a plugin is and how it is trusted, for one provider kind.
#[derive(Clone)]
pub struct PluginSettings {
    kind:         ProviderKind,
    path:         Option<PathBuf>,
    sha256:       Option<String>,
    dev:          bool,
    host_address: Option<String>,
    env:          BTreeMap<String, String>,
    /// A non-secret description of the backend the plugin will drive, from
    /// the same environment it inherits: the effective daemon endpoint,
    /// account, and target. Recorded on every lease and checked before a
    /// recorded sandbox is touched again.
    fingerprint:  String,
}

impl fmt::Debug for PluginSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginSettings")
            .field("kind", &self.kind)
            .field("path", &self.path)
            .field("sha256", &self.sha256)
            .field("dev", &self.dev)
            .field("host_address", &self.host_address)
            .field("env_keys", &self.env.keys())
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

impl PluginSettings {
    /// The settings for `kind` from this process's environment: the
    /// resolution order, pin, dev flag, and host address documented at the
    /// module level. `dev_override` is the CLI flag; `None` reads the
    /// environment and the build profile.
    pub fn from_env(kind: &str, dev_override: Option<bool>) -> Result<Self, PluginError> {
        Self::from_lookup(kind, dev_override, |name| env::var_os(name))
    }

    fn from_lookup(
        kind: &str,
        dev_override: Option<bool>,
        read: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self, PluginError> {
        let text = |name: &str| read(name).and_then(|value| value.into_string().ok());
        let provider_kind =
            ProviderKind::try_new(kind).map_err(|_| PluginError::Kind(kind.to_owned()))?;
        let upper = kind.to_ascii_uppercase().replace('-', "_");
        let path = read(&format!("PETRI_SANDBOX_{upper}_PLUGIN")).map(PathBuf::from);
        let sha256 = text(&format!("PETRI_SANDBOX_{upper}_SHA256"))
            .filter(|value| !value.trim().is_empty())
            .or_else(|| pinned_sha256(kind).map(str::to_owned));
        let dev = dev_override.unwrap_or_else(|| {
            text(DEV_MODE_VAR).is_some_and(|value| !value.is_empty() && value != "0")
                || cfg!(debug_assertions)
        });
        let env = forwarded_env(kind)
            .into_iter()
            .filter_map(|name| text(name).map(|value| (name.to_owned(), value)))
            .collect();
        let host_address = text(&format!("PETRI_SANDBOX_{upper}_HOST_ADDRESS"))
            .filter(|value| !value.trim().is_empty());
        let fingerprint = fingerprint_for(kind, &env);
        Ok(Self {
            kind: provider_kind,
            path,
            sha256,
            dev,
            host_address,
            env,
            fingerprint,
        })
    }

    /// Settings for a plugin at an explicit path, trusted as given: tests
    /// and hosts that ship their own plugin.
    pub fn at_path(kind: &str, path: impl Into<PathBuf>) -> Result<Self, PluginError> {
        let mut settings = Self::from_env(kind, Some(true))?;
        settings.path = Some(path.into());
        settings
            .env
            .insert("RUST_LOG".to_owned(), "warn".to_owned());
        Ok(settings)
    }

    pub fn kind(&self) -> &ProviderKind {
        &self.kind
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The host name or address a sandbox of this provider uses to reach
    /// services Petri runs on this machine. A local Docker daemon has a
    /// known alias; a remote one needs the operator to say.
    pub fn host_address(&self) -> Result<Option<String>, PluginError> {
        infer_host_address(
            self.kind.as_str(),
            self.host_address.as_deref(),
            self.env.get("DOCKER_HOST").map_or("", String::as_str),
        )
    }

    /// The executable, in resolution order: explicit path, the release
    /// bundle sibling, `PATH`.
    fn config(&self) -> Result<PluginConfig, PluginError> {
        let mut config = PluginConfig::new(self.kind.clone());
        if let Some(path) = &self.path {
            config.path = Some(path.clone());
        } else if let Some(sibling) = bundled_sibling(self.kind.as_str()) {
            config.path = Some(sibling);
        }
        config.sha256.clone_from(&self.sha256);
        if config.sha256.is_none() {
            if !self.dev {
                return Err(PluginError::Unpinned {
                    kind:  self.kind.to_string(),
                    upper: self.kind.as_str().to_ascii_uppercase().replace('-', "_"),
                });
            }
            config.dev = true;
        }
        config.env.clone_from(&self.env);
        Ok(config)
    }
}

fn pinned_sha256(kind: &str) -> Option<&'static str> {
    let target = env!("PETRI_TARGET_TRIPLE");
    PINNED_PLUGINS
        .iter()
        .find(|(pinned_kind, pinned_target, _)| *pinned_kind == kind && *pinned_target == target)
        .map(|(_, _, sha256)| *sha256)
}

/// `sandbox-driver-<kind>` beside this executable, when it exists.
fn bundled_sibling(kind: &str) -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let sibling = exe.parent()?.join(format!("{PLUGIN_PREFIX}-{kind}"));
    sibling.is_file().then_some(sibling)
}

/// The host address for `kind`: the configured one when given; else, for
/// Docker, the local alias when `docker_host` names a daemon on this
/// machine, and a refusal to guess when it does not.
fn infer_host_address(
    kind: &str,
    configured: Option<&str>,
    docker_host: &str,
) -> Result<Option<String>, PluginError> {
    if let Some(address) = configured {
        return Ok(Some(address.to_owned()));
    }
    match kind {
        "host" => Ok(Some("127.0.0.1".to_owned())),
        "daytona" => Ok(None),
        "docker" if docker_host_is_local(docker_host) => Ok(Some(DOCKER_HOST_ALIAS.to_owned())),
        "docker" => Err(PluginError::RemoteDaemonNeedsHostAddress {
            docker_host: docker_host.trim().to_owned(),
        }),
        other => Err(PluginError::RemoteDaemonNeedsHostAddress {
            docker_host: format!("<{other} backend>"),
        }),
    }
}

/// Whether `DOCKER_HOST` names a daemon on this machine: unset, a Unix
/// socket, or a named pipe.
fn docker_host_is_local(docker_host: &str) -> bool {
    let value = docker_host.trim();
    value.is_empty() || value.starts_with("unix://") || value.starts_with("npipe://")
}

/// The non-secret backend identity for `kind`, from the environment.
fn fingerprint_for(kind: &str, env: &BTreeMap<String, String>) -> String {
    let value = |name: &str| env.get(name).map_or("", String::as_str);
    match kind {
        "docker" => {
            let endpoint = match value("DOCKER_HOST").trim() {
                "" => "default",
                configured => configured,
            };
            format!("docker:{endpoint}")
        }
        "daytona" => format!(
            "daytona:{}:{}:{}",
            value("DAYTONA_API_URL"),
            value("DAYTONA_ORGANIZATION_ID"),
            value("DAYTONA_TARGET")
        ),
        other => other.to_owned(),
    }
}

/// One live plugin process and the generation it belongs to.
pub struct PluginGeneration {
    pub provider:   Arc<PluginProvider>,
    pub generation: u64,
    pub path:       PathBuf,
}

impl PluginGeneration {
    pub fn is_closed(&self) -> bool {
        self.provider.is_closed()
    }
}

/// Owns the plugin process for one provider kind.
pub struct PluginSupervisor {
    settings: PluginSettings,
    current:  Mutex<Option<Arc<PluginGeneration>>>,
    next:     AtomicU64,
}

impl PluginSupervisor {
    pub fn new(settings: PluginSettings) -> Self {
        Self {
            settings,
            current: Mutex::new(None),
            next: AtomicU64::new(1),
        }
    }

    pub fn settings(&self) -> &PluginSettings {
        &self.settings
    }

    pub fn kind(&self) -> &ProviderKind {
        &self.settings.kind
    }

    /// The live generation, launching one when there is none or the last
    /// one's transport closed. Single-flight: concurrent callers share one
    /// launch.
    pub async fn current(&self) -> Result<Arc<PluginGeneration>, PluginError> {
        let mut current = self.current.lock().await;
        if let Some(generation) = current.as_ref() {
            if !generation.is_closed() {
                return Ok(Arc::clone(generation));
            }
            tracing::warn!(
                provider_kind = %self.settings.kind,
                generation = generation.generation,
                "sandbox plugin transport closed; relaunching"
            );
        }
        let launched = self.launch().await?;
        *current = Some(Arc::clone(&launched));
        Ok(launched)
    }

    async fn launch(&self) -> Result<Arc<PluginGeneration>, PluginError> {
        let kind = self.settings.kind.to_string();
        let config = self.settings.config()?;
        let launch = launch_plugin(PLUGIN_PREFIX, &config)
            .await
            .map_err(|source| PluginError::Launch {
                kind:   kind.clone(),
                source: Box::new(source),
            })?;
        let generation = self.next.fetch_add(1, Ordering::Relaxed);
        if launch.verified {
            tracing::info!(
                provider_kind = %kind,
                generation,
                path = %launch.path.display(),
                "sandbox plugin launched"
            );
        } else {
            tracing::warn!(
                provider_kind = %kind,
                generation,
                path = %launch.path.display(),
                "sandbox plugin launched UNVERIFIED: dev mode allows an unpinned executable"
            );
        }
        let health = launch
            .provider
            .health()
            .await
            .map_err(|source| PluginError::Launch {
                kind:   kind.clone(),
                source: Box::new(source),
            })?;
        if health.status != HealthStatus::Ok {
            let status = match health.status {
                HealthStatus::Unreachable => "unreachable",
                HealthStatus::Unauthorized => "unauthorized",
                _ => "in an unknown state",
            };
            let message = health
                .message
                .unwrap_or_else(|| "the plugin gave no detail".to_owned());
            let _ = launch.provider.shutdown().await;
            return Err(PluginError::Unhealthy {
                kind,
                status,
                message,
            });
        }
        Ok(Arc::new(PluginGeneration {
            provider: Arc::new(launch.provider),
            generation,
            path: launch.path,
        }))
    }

    /// Asks the live plugin, if any, to exit.
    pub async fn shutdown(&self) {
        let current = self.current.lock().await.take();
        if let Some(generation) = current
            && let Err(error) = generation.provider.shutdown().await
        {
            tracing::debug!(error = %error, "sandbox plugin shutdown failed");
        }
    }
}

/// Where a manager gets its provider from: a supervised plugin in
/// production, a fixed provider in tests.
#[async_trait::async_trait]
pub trait ProviderSource: Send + Sync {
    /// The provider to use now and the generation it belongs to. A
    /// generation change means every handle from before it is dead.
    async fn current(&self) -> Result<(Arc<dyn SandboxProvider>, u64), EnvError>;

    /// The non-secret backend fingerprint every lease records.
    fn fingerprint(&self) -> &str;

    /// The provider kind, as recorded on leases.
    fn kind(&self) -> &str;
}

#[async_trait::async_trait]
impl ProviderSource for PluginSupervisor {
    async fn current(&self) -> Result<(Arc<dyn SandboxProvider>, u64), EnvError> {
        let generation = self.current().await.map_err(PluginError::into_env_error)?;
        let provider: Arc<dyn SandboxProvider> =
            Arc::clone(&generation.provider) as Arc<dyn SandboxProvider>;
        Ok((provider, generation.generation))
    }

    fn fingerprint(&self) -> &str {
        self.settings.fingerprint()
    }

    fn kind(&self) -> &str {
        self.settings.kind.as_str()
    }
}

/// A provider handed in directly, with one fixed generation.
pub struct FixedProvider {
    provider:    Arc<dyn SandboxProvider>,
    kind:        String,
    fingerprint: String,
}

impl FixedProvider {
    pub fn new(provider: Arc<dyn SandboxProvider>) -> Self {
        let kind = provider.kind().to_string();
        Self {
            provider,
            fingerprint: format!("{kind}:fixed"),
            kind,
        }
    }
}

#[async_trait::async_trait]
impl ProviderSource for FixedProvider {
    async fn current(&self) -> Result<(Arc<dyn SandboxProvider>, u64), EnvError> {
        Ok((Arc::clone(&self.provider), 1))
    }

    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn kind(&self) -> &str {
        &self.kind
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn docker_fingerprints_distinguish_daemon_endpoints() {
        let endpoints = [
            "",
            "unix:///var/run/docker.sock",
            "unix:///run/user/1000/docker.sock",
            "npipe:////./pipe/docker_engine",
            "npipe:////./pipe/another_engine",
            "tcp://10.0.0.5:2376",
        ];
        let fingerprints: BTreeSet<_> = endpoints
            .into_iter()
            .map(|endpoint| {
                fingerprint_for(
                    "docker",
                    &BTreeMap::from([("DOCKER_HOST".to_owned(), endpoint.to_owned())]),
                )
            })
            .collect();
        assert_eq!(fingerprints.len(), endpoints.len());
    }

    #[test]
    fn settings_keep_the_environment_used_for_their_fingerprint() {
        let original = "tcp://10.0.0.5:2376";
        let mut environment = BTreeMap::from([
            ("DOCKER_HOST", OsString::from(original)),
            ("UNRELATED_SECRET", OsString::from("do not forward")),
        ]);
        let settings = PluginSettings::from_lookup("docker", Some(true), |name| {
            environment.get(name).cloned()
        })
        .expect("settings");
        environment.insert("DOCKER_HOST", OsString::from("unix:///another.sock"));

        let config = settings.config().expect("dev configuration");
        assert_eq!(
            config.env.get("DOCKER_HOST").map(String::as_str),
            Some(original)
        );
        assert!(!config.env.contains_key("UNRELATED_SECRET"));
        assert!(config.inherit_env.is_empty());
        assert_eq!(settings.fingerprint(), format!("docker:{original}"));
        assert!(matches!(
            settings.host_address(),
            Err(PluginError::RemoteDaemonNeedsHostAddress { docker_host }) if docker_host == original
        ));
    }

    #[test]
    fn settings_debug_does_not_expose_forwarded_credentials() {
        let secret = "private-daytona-token";
        let settings = PluginSettings::from_lookup("daytona", Some(true), |name| {
            (name == "DAYTONA_API_KEY").then(|| OsString::from(secret))
        })
        .expect("settings");
        assert_eq!(
            settings
                .config()
                .expect("dev configuration")
                .env
                .get("DAYTONA_API_KEY")
                .map(String::as_str),
            Some(secret)
        );
        let debug = format!("{settings:?}");
        assert!(debug.contains("DAYTONA_API_KEY"));
        assert!(!debug.contains(secret));
    }

    #[test]
    fn a_local_docker_host_is_recognized() {
        assert!(docker_host_is_local(""));
        assert!(docker_host_is_local("unix:///var/run/docker.sock"));
        assert!(docker_host_is_local("npipe:////./pipe/docker_engine"));
        assert!(!docker_host_is_local("tcp://10.0.0.5:2376"));
        assert!(!docker_host_is_local("ssh://build@remote"));
    }

    #[test]
    fn a_local_daemon_infers_the_docker_alias() {
        for local in [
            "",
            "unix:///var/run/docker.sock",
            "npipe:////./pipe/docker_engine",
        ] {
            let address = infer_host_address("docker", None, local).expect("inferred");
            assert_eq!(address.as_deref(), Some(DOCKER_HOST_ALIAS), "for `{local}`");
        }
    }

    #[test]
    fn a_remote_daemon_needs_an_explicit_address() {
        let error = infer_host_address("docker", None, "tcp://10.0.0.5:2376")
            .expect_err("no inference for a remote daemon");
        assert!(
            matches!(&error, PluginError::RemoteDaemonNeedsHostAddress { docker_host } if docker_host == "tcp://10.0.0.5:2376"),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains("PETRI_SANDBOX_DOCKER_HOST_ADDRESS"),
            "the error names the setting: {error}"
        );
    }

    #[test]
    fn a_configured_address_wins_everywhere() {
        let address = infer_host_address("docker", Some("petri.internal"), "tcp://10.0.0.5:2376")
            .expect("configured");
        assert_eq!(address.as_deref(), Some("petri.internal"));
        let address = infer_host_address("daytona", Some("203.0.113.7"), "").expect("configured");
        assert_eq!(address.as_deref(), Some("203.0.113.7"));
    }

    #[test]
    fn a_remote_only_provider_never_guesses() {
        assert_eq!(infer_host_address("daytona", None, "").unwrap(), None);
    }
}
