//! Run-level placement. Workflow targets describe a process or container;
//! these options choose the system that provides it.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use executor::EnvError;
use ir::RuntimeSpec;
use sandbox_driver::{Resources, SandboxKind};

const RUNNER_PIN: &str = "df708f910111";
const DEFAULT_LABEL: &str = "ubuntu-24.04";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxBackend {
    /// Local processes, with Docker for explicit container targets.
    #[default]
    Host,
    /// Docker runner images for process targets and job images for containers.
    Docker,
    /// A Daytona sandbox, with a nested job container when requested.
    Daytona,
}

impl fmt::Display for SandboxBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Host => "host",
            Self::Docker => "docker",
            Self::Daytona => "daytona",
        })
    }
}

impl FromStr for SandboxBackend {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "host" => Ok(Self::Host),
            "docker" => Ok(Self::Docker),
            "daytona" => Ok(Self::Daytona),
            _ => Err(format!(
                "unknown backend {value:?}; expected host, docker, or daytona"
            )),
        }
    }
}

/// Configuration for the built-in sandbox router. Credentials come from
/// provider environment variables and never belong in this structure.
#[derive(Clone, Debug, Default)]
pub struct SandboxOptions {
    pub backend:           SandboxBackend,
    /// Label-to-image overrides. Daytona images must include Docker,
    /// `start-docker`, and Python 3. Empty requirements use `ubuntu-24.04`.
    pub runner_images:     BTreeMap<String, String>,
    pub daytona_resources: DaytonaResources,
    /// The outer Daytona sandbox. Workflow `container:` selects a nested job.
    pub daytona_kind:      DaytonaSandboxKind,
    /// `None` leaves development mode to the environment and build profile.
    pub plugin_dev:        Option<bool>,
}

/// The Daytona offering that hosts a workflow runner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DaytonaSandboxKind {
    Container,
    #[default]
    VirtualMachine,
}

impl DaytonaSandboxKind {
    pub(crate) fn sandbox_kind(self) -> SandboxKind {
        match self {
            Self::Container => SandboxKind::Container,
            Self::VirtualMachine => SandboxKind::VirtualMachine,
        }
    }
}

impl FromStr for DaytonaSandboxKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "container" => Ok(Self::Container),
            "vm" => Ok(Self::VirtualMachine),
            _ => Err(format!(
                "unknown Daytona sandbox kind {value:?}; expected container or vm"
            )),
        }
    }
}

/// Snapshot allocation. Include these values in the snapshot identity:
/// Daytona fixes a sandbox's resources when its snapshot is created.
#[derive(Clone, Copy, Debug)]
pub struct DaytonaResources {
    pub cpu_cores: u32,
    pub memory_mb: u64,
    pub disk_mb:   u64,
}

impl Default for DaytonaResources {
    fn default() -> Self {
        Self {
            cpu_cores: 2,
            memory_mb: 4096,
            disk_mb:   20 * 1024,
        }
    }
}

impl DaytonaResources {
    pub(crate) fn validated(self) -> Result<Resources, EnvError> {
        if self.cpu_cores < 2 || self.memory_mb < 4096 || self.disk_mb < 4096 {
            return Err(EnvError::backend(
                "daytona",
                "configure",
                "nested Docker needs at least 2 CPUs, 4096 MiB of memory, and 4096 MiB of disk",
            ));
        }
        let mut resources = Resources::default();
        resources.cpu_cores = Some(self.cpu_cores);
        resources.memory_mb = Some(self.memory_mb);
        resources.disk_mb = Some(self.disk_mb);
        Ok(resources)
    }
}

impl SandboxOptions {
    pub(crate) fn runner_image(&self, runtime: &RuntimeSpec) -> Result<String, EnvError> {
        let mut selected = None;
        let labels = runtime
            .requirements
            .iter()
            .map(smol_str::SmolStr::as_str)
            .chain(runtime.requirements.is_empty().then_some(DEFAULT_LABEL));
        for label in labels {
            let image = self.runner_images.get(label).cloned().or_else(|| self.default_image(label))
                .filter(|image| !image.trim().is_empty())
                .ok_or_else(|| EnvError::backend(&self.backend.to_string(), "configure",
                    format!("no runner image for label {label:?}; configure --runner-image {label}=IMAGE")))?;
            if selected.as_ref().is_some_and(|previous| previous != &image) {
                return Err(EnvError::backend(
                    &self.backend.to_string(),
                    "configure",
                    "placement labels select different runner images",
                ));
            }
            selected = Some(image);
        }
        Ok(selected.expect("the default supplies a label for empty requirements"))
    }

    fn default_image(&self, label: &str) -> Option<String> {
        let version = match label {
            "ubuntu-latest" | "ubuntu-24.04" => "24.04",
            "ubuntu-22.04" if self.backend != SandboxBackend::Daytona => "22.04",
            "ubuntu-26.04" if self.backend != SandboxBackend::Daytona => "26.04",
            _ => return None,
        };
        let flavor = if self.backend == SandboxBackend::Daytona {
            "dind"
        } else {
            "slim"
        };
        Some(format!(
            "ghcr.io/lithoscomputer/ubuntu-{version}:{flavor}-{RUNNER_PIN}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use ir::RuntimeTarget;

    use super::*;

    fn runtime(labels: &[&str]) -> RuntimeSpec {
        RuntimeSpec {
            target:       RuntimeTarget::HostProcess,
            requirements: labels.iter().map(|label| (*label).into()).collect(),
        }
    }

    #[test]
    fn daytona_uses_only_available_pinned_dind_images_unless_overridden() {
        let mut options = SandboxOptions {
            backend: SandboxBackend::Daytona,
            ..Default::default()
        };
        assert_eq!(
            options.runner_image(&runtime(&[])).unwrap(),
            format!("ghcr.io/lithoscomputer/ubuntu-24.04:dind-{RUNNER_PIN}")
        );
        assert!(options.runner_image(&runtime(&["ubuntu-22.04"])).is_err());
        options.runner_images.insert(
            "ubuntu-22.04".to_owned(),
            "custom:22-dind-pinned".to_owned(),
        );
        assert_eq!(
            options.runner_image(&runtime(&["ubuntu-22.04"])).unwrap(),
            "custom:22-dind-pinned"
        );
        assert!(
            options
                .runner_image(&runtime(&["ubuntu-22.04", "ubuntu-24.04"]))
                .is_err()
        );
    }

    #[test]
    fn docker_maps_supported_labels_and_rejects_unknown_placement() {
        let options = SandboxOptions {
            backend: SandboxBackend::Docker,
            ..Default::default()
        };
        assert_eq!(
            options.runner_image(&runtime(&["ubuntu-22.04"])).unwrap(),
            format!("ghcr.io/lithoscomputer/ubuntu-22.04:slim-{RUNNER_PIN}")
        );
        assert!(options.runner_image(&runtime(&["custom-runner"])).is_err());
    }

    #[test]
    fn daytona_rejects_resources_below_the_dind_minimum() {
        assert!(DaytonaResources::default().validated().is_ok());
        assert!(
            DaytonaResources {
                cpu_cores: 1,
                ..Default::default()
            }
            .validated()
            .is_err()
        );
        assert!(
            DaytonaResources {
                memory_mb: 2048,
                ..Default::default()
            }
            .validated()
            .is_err()
        );
    }
}
