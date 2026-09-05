use std::sync::Arc;
use std::time::Duration;

use executor::{AcquireContext, MapSecrets, NoProgress, ScopeSpec, ServiceSpec};
use ir::{ContainerOptions, RegistryCredentials, RuntimeSpec, RuntimeTarget, ScopeId};
use sandbox_driver::{Resources, SandboxKind, SandboxSource};
use sandbox_driver_daytona_config::{DaytonaProviderConfig, DockerExecutionTarget};

use crate::build_daytona_spec;
use crate::snapshots::RunnerSnapshot;

fn snapshot() -> RunnerSnapshot {
    RunnerSnapshot::new(
        "pinned-runner",
        Resources::default(),
        SandboxKind::VirtualMachine,
        None,
    )
}

#[test]
fn container_offering_supports_process_and_nested_container_jobs() {
    let snapshot = RunnerSnapshot::new(
        "pinned-runner",
        Resources::default(),
        SandboxKind::Container,
        None,
    );
    for container_job in [false, true] {
        let mut scope = ScopeSpec::new(ScopeId::new(1), "job");
        if container_job {
            scope.runtime.target = RuntimeTarget::Container {
                image:       "alpine:3.20".into(),
                options:     ContainerOptions::default(),
                credentials: None,
            };
        }
        let spec = build_daytona_spec(&scope, &AcquireContext::bare(), &snapshot).unwrap();
        assert_eq!(spec.sandbox_kind, Some(SandboxKind::Container));
        let config: DaytonaProviderConfig = serde_json::from_value(spec.provider_config).unwrap();
        assert_eq!(
            config.docker.unwrap().target,
            if container_job {
                DockerExecutionTarget::Container
            } else {
                DockerExecutionTarget::VirtualMachine
            }
        );
    }
}

#[test]
fn a_process_target_uses_the_vm_and_disables_automatic_stops_and_deletion() {
    let scope = ScopeSpec::new(ScopeId::new(1), "job");
    let spec = build_daytona_spec(&scope, &AcquireContext::bare(), &snapshot()).unwrap();
    spec.validate().unwrap();
    assert!(matches!(&spec.source, SandboxSource::Snapshot { id } if id == &snapshot().id));
    assert_eq!(spec.sandbox_kind, Some(SandboxKind::VirtualMachine));
    assert_eq!(
        spec.resources,
        Resources::default(),
        "the snapshot owns resource allocation"
    );
    assert_eq!(spec.timers.auto_stop_after_idle, Some(Duration::ZERO));
    assert_eq!(spec.timers.auto_pause_after_idle, Some(Duration::ZERO));
    assert_eq!(spec.timers.auto_delete_after_stop, Some(Duration::ZERO));
    assert_eq!(spec.timers.ttl, Some(Duration::ZERO));
    assert_eq!(spec.public, Some(false));
    let config: DaytonaProviderConfig = serde_json::from_value(spec.provider_config).unwrap();
    let docker = config.docker.unwrap();
    assert_eq!(docker.target, DockerExecutionTarget::VirtualMachine);
    assert!(docker.image.is_empty());
    assert!(docker.options.binds.is_empty());
}

#[test]
fn container_jobs_keep_options_services_and_credentials_inside_the_vm() {
    let mut scope = ScopeSpec::new(ScopeId::new(1), "job");
    scope.env.insert("SCOPE_VALUE".into(), "scope".into());
    scope.runtime = RuntimeSpec {
        target:       RuntimeTarget::Container {
            image:       "private/job:pin".into(),
            options:     ContainerOptions {
                user: Some("1000".into()),
                privileged: true,
                env: vec![("SCOPE_VALUE".into(), "container".into())],
                ..Default::default()
            },
            credentials: Some(RegistryCredentials {
                username:        "user".into(),
                password_secret: "REGISTRY_PASSWORD".into(),
            }),
        },
        requirements: Vec::new(),
    };
    scope.services.push(ServiceSpec::new("db", "postgres:16"));
    let ctx = AcquireContext::new(
        Arc::new(MapSecrets::from_pairs(&[(
            "REGISTRY_PASSWORD",
            "private-password",
        )])),
        Arc::new(NoProgress),
    );
    let spec = build_daytona_spec(&scope, &ctx, &snapshot()).unwrap();
    assert_eq!(spec.env["SCOPE_VALUE"], "container");
    assert_eq!(
        spec.user.as_deref(),
        Some("root"),
        "VM bootstrap keeps its own user"
    );
    assert!(!format!("{spec:?}").contains("private-password"));
    let config: DaytonaProviderConfig = serde_json::from_value(spec.provider_config).unwrap();
    let docker = config.docker.unwrap();
    assert_eq!(docker.target, DockerExecutionTarget::Container);
    assert_eq!(docker.image, "private/job:pin");
    assert_eq!(docker.user.as_deref(), Some("1000"));
    assert!(docker.options.privileged);
    assert!(docker.options.extra_hosts.is_empty());
    assert!(docker.options.binds.is_empty());
    assert_eq!(docker.options.sidecars[0].name, "db");
    assert_eq!(
        docker.options.registry_auth.unwrap().password,
        "private-password"
    );
}

#[test]
fn a_vm_process_target_with_sidecars_fails_before_snapshot_preparation() {
    let scope = ScopeSpec::new(ScopeId::new(1), "job")
        .with_services(vec![ServiceSpec::new("db", "postgres:16")]);
    assert!(build_daytona_spec(&scope, &AcquireContext::bare(), &snapshot()).is_err());
}
