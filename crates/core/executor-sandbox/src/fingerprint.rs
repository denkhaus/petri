//! The non-secret backend identity every lease records.
//!
//! A fingerprint names the resource namespace a provider drives: the Host
//! registry, the Docker daemon endpoint, or the Daytona account and target.
//! It is recorded on every lease and checked before a recorded sandbox is
//! touched again, so a resume or prune against a different backend refuses
//! instead of acting on the wrong resources.
//!
//! The seed is the same whichever way the provider is reached, a plugin or
//! an in-process factory, so a lease recorded one way is recoverable the
//! other. The first health report's identity, when the provider gives one,
//! is appended to the seed ([`verified`]).

use std::path::Path;

use sandbox_driver::ProviderHealth;

/// `host:<registry>`: the canonical Host registry directory.
pub fn host(registry: &Path) -> String {
    format!("host:{}", registry.to_string_lossy())
}

/// `docker:<endpoint>`: the daemon endpoint, or `default` when none is
/// configured (the local socket).
pub fn docker(docker_host: Option<&str>) -> String {
    let endpoint = match docker_host.map(str::trim) {
        None | Some("") => "default",
        Some(configured) => configured,
    };
    format!("docker:{endpoint}")
}

/// `daytona:<api url>:<organization>:<target>`, each empty when unset.
pub fn daytona(
    api_url: Option<&str>,
    organization_id: Option<&str>,
    target: Option<&str>,
) -> String {
    format!(
        "daytona:{}:{}:{}",
        api_url.unwrap_or_default(),
        organization_id.unwrap_or_default(),
        target.unwrap_or_default()
    )
}

/// Why a provider's health report cannot confirm its namespace.
#[derive(Debug, thiserror::Error)]
#[error(
    "the {kind} provider reports no verified resource identity; its effective \
     organization must be known before sandbox resources can be created or recovered"
)]
pub struct MissingIdentity {
    pub kind: String,
}

/// The fingerprint a provider serves under: `seed`, plus `:<identity>` when
/// its health report names one. Daytona must name one, because its seed
/// alone cannot tell two organizations behind the same key apart.
pub fn verified(
    kind: &str,
    seed: &str,
    health: &ProviderHealth,
) -> Result<String, MissingIdentity> {
    let identity = health
        .identity
        .as_deref()
        .filter(|identity| !identity.trim().is_empty());
    if kind == "daytona" && identity.is_none() {
        return Err(MissingIdentity {
            kind: kind.to_owned(),
        });
    }
    Ok(match identity {
        Some(identity) => format!("{seed}:{identity}"),
        None => seed.to_owned(),
    })
}
