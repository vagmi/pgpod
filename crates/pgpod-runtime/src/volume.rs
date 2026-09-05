//! Podman named volumes — where PGDATA lives (ADR 00 §4).
//!
//! The whole storage model rests on one podman behaviour: mounting an
//! empty named volume into a container chowns it to that container's user,
//! so PostgreSQL owns its own data with no UID mapping. `volume_smoke.rs`
//! proves that on the host under test rather than taking it on faith.

use std::collections::HashMap;

use podman_api::opts::{VolumeCreateOpts, VolumeListOpts, VolumePruneOpts};

use crate::client::PodmanClient;
use crate::error::{Error, Result};

/// A volume pgpod knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    pub name: String,
    /// Host path of the volume's data directory, under the graph root.
    /// Reachable only via `podman unshare` — this is what
    /// `pgpod volume path` prints.
    pub mountpoint: String,
    pub driver: String,
    pub labels: HashMap<String, String>,
}

impl PodmanClient {
    /// Create a named volume, labelled so the reconciler can find it again
    /// after a restart. Idempotent: an existing volume of the same name is
    /// returned rather than being recreated, because recreating would mean
    /// destroying a database.
    pub async fn create_volume(
        &self,
        name: &str,
        labels: &[(String, String)],
    ) -> Result<VolumeInfo> {
        if let Some(existing) = self.volume(name).await? {
            return Ok(existing);
        }

        let opts = VolumeCreateOpts::builder()
            .name(name)
            .labels(labels.iter().cloned().collect::<HashMap<_, _>>())
            .build();

        self.podman()
            .volumes()
            .create(&opts)
            .await
            .map_err(|e| Error::Volume(format!("create {name}: {e}")))?;

        self.volume(name)
            .await?
            .ok_or_else(|| Error::Volume(format!("created volume {name} but it is not visible")))
    }

    /// Look up a volume. `Ok(None)` means podman does not have it, which
    /// is a normal answer and not an error.
    pub async fn volume(&self, name: &str) -> Result<Option<VolumeInfo>> {
        let handle = self.podman().volumes().get(name);
        if !handle
            .exists()
            .await
            .map_err(|e| Error::Volume(format!("exists {name}: {e}")))?
        {
            return Ok(None);
        }
        let v = handle
            .inspect()
            .await
            .map_err(|e| Error::Volume(format!("inspect {name}: {e}")))?;
        Ok(Some(VolumeInfo {
            name: v.name,
            mountpoint: v.mountpoint,
            driver: v.driver,
            labels: v.labels,
        }))
    }

    /// List volumes carrying a given label, which is how the reconciler
    /// finds pgpod's own among whatever else the user runs.
    pub async fn list_volumes_labelled(&self, label: &str) -> Result<Vec<VolumeInfo>> {
        let opts = VolumeListOpts::builder()
            .filter([podman_api::opts::VolumeListFilter::LabelKey(
                label.to_string(),
            )])
            .build();
        let volumes = self
            .podman()
            .volumes()
            .list(&opts)
            .await
            .map_err(|e| Error::Volume(format!("list: {e}")))?;
        Ok(volumes
            .into_iter()
            .map(|v| VolumeInfo {
                name: v.name,
                mountpoint: v.mountpoint,
                driver: v.driver,
                labels: v.labels,
            })
            .collect())
    }

    /// Remove a volume. **This destroys a database.**
    ///
    /// Nothing in the reconcile path may call this. It exists for
    /// `pgpod delete --purge`, where the operator has said so explicitly
    /// (principle 4 in `AGENTS.md`).
    pub async fn remove_volume_destroying_data(&self, name: &str) -> Result<()> {
        self.podman()
            .volumes()
            .get(name)
            .delete()
            .await
            .map_err(|e| Error::Volume(format!("delete {name}: {e}")))
    }

    /// Prune anonymous unused volumes. Never touches labelled volumes, so
    /// it cannot reach a pgpod instance's data.
    pub async fn prune_unlabelled_volumes(&self) -> Result<usize> {
        let reports = self
            .podman()
            .volumes()
            .prune(&VolumePruneOpts::builder().build())
            .await
            .map_err(|e| Error::Volume(format!("prune: {e}")))?;
        Ok(reports.len())
    }
}
