//! Sidecar service containers, realized with their scope.
//!
//! One network per scope, named like the scope's job container; one container
//! per service, reachable by its name on that network, its ports published to
//! the host when the scope itself runs on the host. Acquire returns only when
//! every service is healthy — Docker-reported health, from the image's
//! `HEALTHCHECK` or a `--health-cmd` in the service's options; a service with
//! no health check is ready when running. Everything is named under the
//! scope's deterministic prefix, so the acquire fence and scope release sweep
//! a crashed run's services the same way they sweep its containers.

use std::time::Duration;

use executor::{AcquireContext, EnvError, Progress, ScopeSpec};
use smol_str::SmolStr;
use tokio::task::JoinSet;
use tokio::time::{self, Instant};

use crate::{PullPolicy, prepare_image, run_docker, sweep_containers};

/// The backstop for a health check that never leaves `starting`: Docker's own
/// retry budget bounds the common case, this bounds a misconfigured one.
pub const SERVICE_HEALTH_WAIT: Duration = Duration::from_mins(5);

const HEALTH_POLL: Duration = Duration::from_millis(250);

/// The container-name prefix of a scope's services, under its base name.
pub(crate) fn service_prefix(base: &str) -> String {
    format!("{base}-svc-")
}

/// Remove everything a scope's service world may have left: containers by
/// prefix, then the network (which shares the base name). Best effort and
/// idempotent — the fence and release both come through here.
pub(crate) async fn sweep(base: &str) {
    sweep_containers(&service_prefix(base)).await;
    let _ = run_docker(&["network", "rm", base]).await;
}

/// Realize a scope's services. Returns the scope network's name when there are
/// services to attach to; on any failure the partial world is torn down before
/// the error returns, so a failed acquire leaks nothing.
pub(crate) async fn realize(
    scope: &ScopeSpec,
    base: &str,
    publish_ports: bool,
    pull: PullPolicy,
    ctx: &AcquireContext,
) -> Result<Option<String>, EnvError> {
    if scope.services.is_empty() {
        return Ok(None);
    }
    match realize_inner(scope, base, publish_ports, pull, ctx).await {
        Ok(()) => Ok(Some(base.to_string())),
        Err(error) => {
            sweep(base).await;
            Err(error)
        }
    }
}

async fn realize_inner(
    scope: &ScopeSpec,
    base: &str,
    publish_ports: bool,
    pull: PullPolicy,
    ctx: &AcquireContext,
) -> Result<(), EnvError> {
    run_docker(&["network", "create", base]).await?;
    // The pulls are the long pole of a cold acquire and the services are
    // independent: fetch every image concurrently. Every pull runs to
    // completion before any verdict, so a failing service never leaves another
    // one racing the sweep that follows.
    let mut pulls = JoinSet::new();
    for service in &scope.services {
        let image = service.image.clone();
        let credentials = service.credentials.clone();
        let scope_id = scope.id;
        let ctx = ctx.clone();
        pulls.spawn(async move {
            prepare_image(&image, credentials.as_ref(), pull, scope_id, &ctx).await
        });
    }
    let mut failed = None;
    while let Some(joined) = pulls.join_next().await {
        let result = joined.expect("image pull task does not panic");
        failed = failed.or(result.err());
    }
    if let Some(error) = failed {
        return Err(error);
    }
    for service in &scope.services {
        let name = format!("{}{}", service_prefix(base), service.name);
        let mut create: Vec<String> = vec![
            "create".into(),
            "--name".into(),
            name.clone(),
            "--network".into(),
            base.to_string(),
            "--network-alias".into(),
            service.name.to_string(),
        ];
        for (key, value) in &service.env {
            create.push("-e".into());
            create.push(format!("{key}={value}"));
        }
        if publish_ports {
            for port in &service.ports {
                create.push("-p".into());
                create.push(port.to_string());
            }
        }
        // The service's raw engine flags — health checks ride here.
        create.extend(service.options.iter().map(ToString::to_string));
        create.push(service.image.to_string());
        let refs: Vec<&str> = create.iter().map(String::as_str).collect();
        run_docker(&refs).await?;
        run_docker(&["start", &name]).await?;
        ctx.progress().progress(scope.id, Progress::ServiceStarted {
            name: service.name.clone(),
        });
    }
    // Health after every service has started, so slow checks overlap.
    for service in &scope.services {
        let name = format!("{}{}", service_prefix(base), service.name);
        await_health(&name, &service.name).await?;
        ctx.progress().progress(scope.id, Progress::ServiceHealthy {
            name: service.name.clone(),
        });
    }
    Ok(())
}

/// Wait for Docker to call the container healthy: `healthy` from a check, or
/// plain `running` when the container has none. `unhealthy` and an exited
/// container fail with the container's last log lines, so a bad service names
/// itself.
async fn await_health(container: &str, service: &SmolStr) -> Result<(), EnvError> {
    let deadline = Instant::now() + SERVICE_HEALTH_WAIT;
    loop {
        let state = run_docker(&[
            "inspect",
            "--format",
            "{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}",
            container,
        ])
        .await?;
        let mut parts = state.split_whitespace();
        let status = parts.next().unwrap_or("");
        let health = parts.next().unwrap_or("");
        match (status, health) {
            (_, "healthy") | ("running", "") => return Ok(()),
            (_, "unhealthy") => {
                return Err(service_failure(service, container, "reported unhealthy").await);
            }
            ("exited" | "dead", _) => {
                return Err(
                    service_failure(service, container, "exited before it was ready").await,
                );
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(service_failure(service, container, "never reported healthy").await);
        }
        time::sleep(HEALTH_POLL).await;
    }
}

async fn service_failure(service: &SmolStr, container: &str, what: &str) -> EnvError {
    let logs = run_docker(&["logs", "--tail", "10", container])
        .await
        .unwrap_or_default();
    let tail = if logs.is_empty() {
        String::new()
    } else {
        format!("; last output:\n{logs}")
    };
    EnvError::Backend {
        backend:   SmolStr::new("docker"),
        operation: SmolStr::new("service"),
        message:   format!("service `{service}` {what}{tail}"),
    }
}
