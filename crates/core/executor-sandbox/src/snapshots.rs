//! Shared runner snapshots. Snapshot preparation runs only for a new lease;
//! resuming an existing VM does not depend on the snapshot service.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use executor::EnvError;
use sandbox_driver::{
    Error, Resources, SandboxKind, SandboxSource, SandboxSpec, SnapshotId, SnapshotProvider,
    SnapshotSource, SnapshotSpec, SnapshotState, SnapshotStatus,
};
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;
use tokio::time;

const PREPARE_TIMEOUT: Duration = Duration::from_secs(900);

pub(crate) struct RunnerSnapshot {
    pub(crate) id: SnapshotId,
    spec:          SnapshotSpec,
}

impl RunnerSnapshot {
    pub(crate) fn new(
        image: &str,
        resources: Resources,
        kind: SandboxKind,
        region: Option<&str>,
    ) -> Self {
        let identity = serde_json::to_vec(&(image, resources, kind, region))
            .expect("runner snapshot identity is serializable");
        let digest = format!("{:x}", Sha256::digest(identity));
        let name = format!("petri-runner-{}", &digest[..48]);
        let mut spec = SnapshotSpec::new(SnapshotSource::Image {
            reference: image.to_owned(),
        });
        spec.name = Some(name.clone());
        spec.resources = resources;
        spec.sandbox_kind = Some(kind);
        spec.region = region.map(str::to_owned);
        Self {
            id: SnapshotId::try_new(name).expect("generated snapshot name is valid"),
            spec,
        }
    }

    pub(crate) fn sandbox_spec(&self) -> SandboxSpec {
        let mut spec = SandboxSpec::new(SandboxSource::Snapshot {
            id: self.id.clone(),
        })
        .sandbox_kind(
            self.spec
                .sandbox_kind
                .expect("runner snapshots have a kind"),
        );
        spec.region.clone_from(&self.spec.region);
        spec
    }

    fn validate_status(&self, status: &SnapshotStatus) -> Result<(), EnvError> {
        let mut expected = self.spec.resources;
        if expected.disk_mb.is_none() {
            // An omitted request delegates disk sizing to Daytona. Accept
            // its positive allocation without imposing the minimum for an
            // explicitly sized runner; other requested resources still
            // have to match exactly.
            let disk = status.resources.and_then(|resources| resources.disk_mb);
            if disk.is_none_or(|disk| disk == 0) {
                return Err(EnvError::backend(
                    "daytona",
                    "snapshot",
                    "the runner snapshot must report a positive disk allocation",
                ));
            }
            expected.disk_mb = disk;
        }
        if status.sandbox_kind != self.spec.sandbox_kind
            || status.resources != Some(expected)
            || self
                .spec
                .region
                .as_ref()
                .is_some_and(|region| !status.regions.contains(region))
        {
            return Err(EnvError::backend(
                "daytona",
                "snapshot",
                "the named runner snapshot has a different kind, resource allocation, or region",
            ));
        }
        Ok(())
    }

    async fn prepare(&self, snapshots: &dyn SnapshotProvider) -> Result<(), EnvError> {
        let initial = match snapshots.get(&self.id).await {
            Ok(status) => status,
            Err(Error::NotFound { .. }) => {
                if let Err(error) = snapshots.create(&self.spec, None).await {
                    // Another run may have created the same named snapshot.
                    // Re-read once; never replay an uncertain create.
                    match snapshots.get(&self.id).await {
                        Ok(status) => return self.wait_ready(snapshots, status).await,
                        Err(_) => return Err(snapshot_error(&error)),
                    }
                }
                snapshots
                    .get(&self.id)
                    .await
                    .map_err(|error| snapshot_error(&error))?
            }
            Err(error) => return Err(snapshot_error(&error)),
        };
        self.wait_ready(snapshots, initial).await
    }

    async fn wait_ready(
        &self,
        snapshots: &dyn SnapshotProvider,
        mut status: SnapshotStatus,
    ) -> Result<(), EnvError> {
        loop {
            match status.state {
                SnapshotState::Active => return self.validate_status(&status),
                SnapshotState::Inactive => {
                    self.validate_status(&status)?;
                    snapshots
                        .activate(&self.id, None)
                        .await
                        .map_err(|error| snapshot_error(&error))?;
                }
                SnapshotState::Building => time::sleep(Duration::from_secs(2)).await,
                _ => {
                    return Err(EnvError::backend(
                        "daytona",
                        "snapshot",
                        format!("runner snapshot is in state {:?}", status.state),
                    ));
                }
            }
            status = snapshots
                .get(&self.id)
                .await
                .map_err(|error| snapshot_error(&error))?;
        }
    }
}

#[derive(Default)]
pub(crate) struct RunnerSnapshots {
    ready: Mutex<BTreeMap<SnapshotId, Arc<OnceCell<()>>>>,
}

impl RunnerSnapshots {
    pub(crate) async fn ensure(
        &self,
        snapshots: &dyn SnapshotProvider,
        snapshot: &RunnerSnapshot,
    ) -> Result<(), EnvError> {
        let ready = self
            .ready
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(snapshot.id.clone())
            .or_default()
            .clone();
        ready
            .get_or_try_init(|| async {
                time::timeout(PREPARE_TIMEOUT, snapshot.prepare(snapshots))
                    .await
                    .map_err(|_| {
                        EnvError::backend(
                            "daytona",
                            "snapshot",
                            "runner snapshot preparation exceeded 15 minutes",
                        )
                    })?
            })
            .await?;
        Ok(())
    }
}

fn snapshot_error(error: &Error) -> EnvError {
    EnvError::backend("daytona", "snapshot", error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use sandbox_driver::{EventContext, ResourceKind, SnapshotFilter};
    use tokio::task::yield_now;

    use super::*;
    use crate::DaytonaResources;

    #[derive(Default)]
    struct Snapshots {
        status:            Mutex<Option<SnapshotStatus>>,
        creates:           AtomicUsize,
        reads:             AtomicUsize,
        activations:       AtomicUsize,
        lose_create_reply: bool,
    }

    #[async_trait]
    impl SnapshotProvider for Snapshots {
        async fn create(
            &self,
            spec: &SnapshotSpec,
            _: Option<EventContext>,
        ) -> sandbox_driver::Result<SnapshotId> {
            self.creates.fetch_add(1, Ordering::SeqCst);
            let id = SnapshotId::try_new(spec.name.as_ref().unwrap()).unwrap();
            let mut status = SnapshotStatus::new(id.clone(), SnapshotState::Active);
            status.sandbox_kind = spec.sandbox_kind;
            let mut resolved = spec.resources;
            resolved.disk_mb.get_or_insert(3 * 1024);
            status.resources = Some(resolved);
            status.regions = spec.region.iter().cloned().collect();
            *self.status.lock().unwrap() = Some(status);
            yield_now().await;
            if self.lose_create_reply {
                Err(Error::Timeout {
                    operation: "creating snapshot".to_owned(),
                    elapsed:   Duration::from_secs(1),
                })
            } else {
                Ok(id)
            }
        }

        async fn get(&self, id: &SnapshotId) -> sandbox_driver::Result<SnapshotStatus> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.status
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| Error::NotFound {
                    resource: ResourceKind::Snapshot,
                    id:       id.as_str().to_owned(),
                })
        }

        async fn activate(
            &self,
            _: &SnapshotId,
            _: Option<EventContext>,
        ) -> sandbox_driver::Result<()> {
            self.activations.fetch_add(1, Ordering::SeqCst);
            self.status.lock().unwrap().as_mut().unwrap().state = SnapshotState::Active;
            Ok(())
        }

        async fn list(&self, _: &SnapshotFilter) -> sandbox_driver::Result<Vec<SnapshotStatus>> {
            panic!("a runner lookup must not list every account snapshot")
        }

        async fn delete(
            &self,
            _: &SnapshotId,
            _: Option<EventContext>,
        ) -> sandbox_driver::Result<()> {
            panic!("runner snapshots are shared, not owned by a run")
        }
    }

    fn request() -> RunnerSnapshot {
        RunnerSnapshot::new(
            "runner:dind-pinned",
            DaytonaResources {
                disk_mb: Some(20 * 1024),
                ..Default::default()
            }
            .validated()
            .unwrap(),
            SandboxKind::VirtualMachine,
            None,
        )
    }

    #[tokio::test]
    async fn equivalent_allocations_reuse_the_snapshot_and_accept_provider_sizes() {
        let snapshots = Snapshots::default();
        for disk_mb in [None, Some(2862)] {
            let requested = DaytonaResources {
                memory_mb: 3815,
                disk_mb,
                ..Default::default()
            };
            let rounded = DaytonaResources {
                memory_mb: 4096,
                disk_mb: disk_mb.map(|_| 3072),
                ..requested
            };
            let snapshot = RunnerSnapshot::new(
                "runner:pinned",
                requested.validated().unwrap(),
                SandboxKind::Container,
                Some("us"),
            );
            let equivalent = RunnerSnapshot::new(
                "runner:pinned",
                rounded.validated().unwrap(),
                SandboxKind::Container,
                Some("us"),
            );
            assert_eq!(snapshot.id, equivalent.id);
            assert_eq!(snapshot.spec.resources.memory_mb, Some(4096));
            assert_eq!(snapshot.spec.resources.disk_mb, rounded.disk_mb);

            // A provider reports actual whole-GiB allocations, including
            // its chosen disk size when the request left that unspecified.
            let mut actual = rounded.validated().unwrap();
            actual.disk_mb = Some(3072);
            let mut status = SnapshotStatus::new(snapshot.id.clone(), SnapshotState::Active);
            status.sandbox_kind = Some(SandboxKind::Container);
            status.regions = vec!["us".to_owned()];
            status.resources = Some(actual);
            *snapshots.status.lock().unwrap() = Some(status.clone());
            RunnerSnapshots::default()
                .ensure(&snapshots, &snapshot)
                .await
                .unwrap();
            RunnerSnapshots::default()
                .ensure(&snapshots, &equivalent)
                .await
                .unwrap();

            status.resources.as_mut().unwrap().memory_mb = Some(5120);
            assert!(snapshot.validate_status(&status).is_err());
            status.resources = Some(actual);
            status.regions = vec!["eu".to_owned()];
            assert!(snapshot.validate_status(&status).is_err());
        }
        assert_eq!(snapshots.creates.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_unspecified_disk_uses_the_resolved_allocation_and_reuses_the_snapshot() {
        let resources = DaytonaResources {
            disk_mb: None,
            ..Default::default()
        }
        .validated()
        .unwrap();
        let request = RunnerSnapshot::new("runner:pinned", resources, SandboxKind::Container, None);
        let snapshots = Snapshots::default();
        RunnerSnapshots::default()
            .ensure(&snapshots, &request)
            .await
            .unwrap();
        // A fresh executor must also accept Daytona's concrete allocation.
        RunnerSnapshots::default()
            .ensure(&snapshots, &request)
            .await
            .unwrap();
        assert_eq!(snapshots.creates.load(Ordering::SeqCst), 1);
        let status = snapshots.get(&request.id).await.unwrap();
        assert_eq!(status.resources.unwrap().disk_mb, Some(3 * 1024));
        let mut explicit = resources;
        explicit.disk_mb = Some(10 * 1024);
        assert_ne!(
            request.id,
            RunnerSnapshot::new("runner:pinned", explicit, SandboxKind::Container, None).id
        );
    }

    #[test]
    fn provider_disk_defaults_are_accepted_and_explicit_resources_still_match() {
        let resources = DaytonaResources {
            disk_mb: None,
            ..Default::default()
        }
        .validated()
        .unwrap();
        let request = RunnerSnapshot::new("runner:pinned", resources, SandboxKind::Container, None);
        let mut status = SnapshotStatus::new(request.id.clone(), SnapshotState::Active);
        status.sandbox_kind = Some(SandboxKind::Container);
        for disk in [None, Some(0), Some(3 * 1024), Some(4096), Some(10 * 1024)] {
            let mut resolved = resources;
            resolved.disk_mb = disk;
            status.resources = Some(resolved);
            assert_eq!(
                request.validate_status(&status).is_ok(),
                disk.is_some_and(|disk| disk > 0)
            );
        }
        status.resources.as_mut().unwrap().cpu_cores = Some(4);
        assert!(request.validate_status(&status).is_err());
        status.resources.as_mut().unwrap().cpu_cores = resources.cpu_cores;
        status.sandbox_kind = Some(SandboxKind::VirtualMachine);
        assert!(request.validate_status(&status).is_err());
        status.sandbox_kind = Some(SandboxKind::Container);
        let explicit = RunnerSnapshot::new(
            "runner:pinned",
            DaytonaResources {
                disk_mb: Some(20 * 1024),
                ..Default::default()
            }
            .validated()
            .unwrap(),
            SandboxKind::Container,
            None,
        );
        assert!(explicit.validate_status(&status).is_err());
    }

    #[tokio::test]
    async fn concurrent_scopes_prepare_one_snapshot_and_reuse_it() {
        let snapshots = Snapshots::default();
        let cache = RunnerSnapshots::default();
        let request = request();
        let (first, second) = tokio::join!(
            cache.ensure(&snapshots, &request),
            cache.ensure(&snapshots, &request)
        );
        first.unwrap();
        second.unwrap();
        cache.ensure(&snapshots, &request).await.unwrap();
        assert_eq!(snapshots.creates.load(Ordering::SeqCst), 1);
        assert_eq!(snapshots.reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_lost_create_reply_is_reconciled_without_replaying_create() {
        let snapshots = Snapshots {
            lose_create_reply: true,
            ..Default::default()
        };
        RunnerSnapshots::default()
            .ensure(&snapshots, &request())
            .await
            .unwrap();
        assert_eq!(snapshots.creates.load(Ordering::SeqCst), 1);
        assert_eq!(snapshots.reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn inactive_snapshots_are_activated_and_wrong_allocations_are_rejected() {
        let request = request();
        let mut status = SnapshotStatus::new(request.id.clone(), SnapshotState::Inactive);
        status.sandbox_kind = Some(SandboxKind::VirtualMachine);
        status.resources = Some(request.spec.resources);
        let snapshots = Snapshots {
            status: Mutex::new(Some(status)),
            ..Default::default()
        };
        RunnerSnapshots::default()
            .ensure(&snapshots, &request)
            .await
            .unwrap();
        assert_eq!(snapshots.creates.load(Ordering::SeqCst), 0);
        assert_eq!(snapshots.activations.load(Ordering::SeqCst), 1);
        snapshots
            .status
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .resources
            .as_mut()
            .unwrap()
            .cpu_cores = Some(1);
        assert!(
            RunnerSnapshots::default()
                .ensure(&snapshots, &request)
                .await
                .is_err()
        );
        assert_eq!(snapshots.creates.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn image_resource_and_kind_changes_select_a_new_snapshot() {
        let old = request();
        let new_image = RunnerSnapshot::new(
            "runner:new-pin",
            old.spec.resources,
            SandboxKind::VirtualMachine,
            None,
        );
        let mut resources = old.spec.resources;
        resources.cpu_cores = Some(4);
        let new_size = RunnerSnapshot::new(
            "runner:dind-pinned",
            resources,
            SandboxKind::VirtualMachine,
            None,
        );
        let container = RunnerSnapshot::new(
            "runner:dind-pinned",
            old.spec.resources,
            SandboxKind::Container,
            None,
        );
        assert_ne!(old.id, new_image.id);
        assert_ne!(old.id, new_size.id);
        assert_ne!(old.id, container.id);
        let mut status = SnapshotStatus::new(container.id.clone(), SnapshotState::Active);
        status.sandbox_kind = Some(SandboxKind::Container);
        status.resources = Some(old.spec.resources);
        container.validate_status(&status).unwrap();
        assert!(old.validate_status(&status).is_err());
    }

    #[tokio::test]
    async fn runner_snapshots_and_sandboxes_use_the_selected_region() {
        let resources = request().spec.resources;
        let us = RunnerSnapshot::new(
            "runner:dind-pinned",
            resources,
            SandboxKind::VirtualMachine,
            Some("us-central-1"),
        );
        let eu = RunnerSnapshot::new(
            "runner:dind-pinned",
            resources,
            SandboxKind::VirtualMachine,
            Some("eu"),
        );
        assert_ne!(us.id, eu.id, "regions cannot reuse a runner snapshot");
        assert_eq!(us.sandbox_spec().region.as_deref(), Some("us-central-1"));
        let snapshots = Snapshots::default();
        RunnerSnapshots::default()
            .ensure(&snapshots, &us)
            .await
            .unwrap();
        let status = snapshots.get(&us.id).await.unwrap();
        assert_eq!(status.regions, ["us-central-1"]);
        us.validate_status(&status).unwrap();
        assert!(eu.validate_status(&status).is_err());
    }
}
