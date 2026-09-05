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
    pub(crate) fn new(image: &str, resources: Resources, kind: SandboxKind) -> Self {
        let identity = serde_json::to_vec(&(image, resources, kind))
            .expect("runner snapshot identity is serializable");
        let digest = format!("{:x}", Sha256::digest(identity));
        let name = format!("petri-runner-{}", &digest[..48]);
        let mut spec = SnapshotSpec::new(SnapshotSource::Image {
            reference: image.to_owned(),
        });
        spec.name = Some(name.clone());
        spec.resources = resources;
        spec.sandbox_kind = Some(kind);
        Self {
            id: SnapshotId::try_new(name).expect("generated snapshot name is valid"),
            spec,
        }
    }

    pub(crate) fn sandbox_spec(&self) -> SandboxSpec {
        SandboxSpec::new(SandboxSource::Snapshot {
            id: self.id.clone(),
        })
        .sandbox_kind(
            self.spec
                .sandbox_kind
                .expect("runner snapshots have a kind"),
        )
    }

    fn validate_status(&self, status: &SnapshotStatus) -> Result<(), EnvError> {
        if status.sandbox_kind != self.spec.sandbox_kind
            || status.resources != Some(self.spec.resources)
        {
            return Err(EnvError::backend(
                "daytona",
                "snapshot",
                "the named runner snapshot has a different kind or resource allocation",
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
            status.resources = Some(spec.resources);
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
            DaytonaResources::default().validated().unwrap(),
            SandboxKind::VirtualMachine,
        )
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
        );
        let mut resources = old.spec.resources;
        resources.cpu_cores = Some(4);
        let new_size =
            RunnerSnapshot::new("runner:dind-pinned", resources, SandboxKind::VirtualMachine);
        let container = RunnerSnapshot::new(
            "runner:dind-pinned",
            old.spec.resources,
            SandboxKind::Container,
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
}
