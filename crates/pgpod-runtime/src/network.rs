//! Podman networks — one bridge per cluster.
//!
//! Standbys reach their primary by container name (`pgpod-mydb-1`), which
//! survives container recreation and IP changes. That resolution comes
//! from aardvark-dns, which only runs under the netavark backend — hence
//! `pgpod doctor` treating CNI as a failure rather than a warning
//! (ADR 03 §1).

use std::collections::HashMap;

use podman_api::opts::{NetworkCreateOpts, NetworkListOpts};

use crate::client::PodmanClient;
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInfo {
    pub name: String,
    pub id: Option<String>,
    pub driver: Option<String>,
    pub dns_enabled: bool,
    /// The IPv4 subnet podman assigned, e.g. `10.89.3.0/24`.
    ///
    /// This is what `pg_hba.conf` needs: a rule scoped to the cluster's
    /// own network rather than a guessed or wide-open CIDR
    /// (`pgpod_pg::HbaConfig::with_network`).
    pub subnet: Option<String>,
}

impl PodmanClient {
    /// Create the cluster's network if it does not exist. Idempotent.
    ///
    /// Not `--internal`: containers need outbound reachability to ship WAL
    /// to object storage (ADR 00 §10).
    pub async fn ensure_network(
        &self,
        name: &str,
        labels: &[(String, String)],
    ) -> Result<NetworkInfo> {
        if let Some(existing) = self.network(name).await? {
            return Ok(existing);
        }

        let opts = NetworkCreateOpts::builder()
            .name(name)
            .driver("bridge")
            // Without this there is no aardvark-dns and container names do
            // not resolve, which breaks `primary_conninfo`.
            .dns_enabled(true)
            .labels(labels.iter().cloned().collect::<HashMap<_, _>>())
            .build();

        self.podman()
            .networks()
            .create(&opts)
            .await
            .map_err(|e| Error::Network(format!("create {name}: {e}")))?;

        self.network(name)
            .await?
            .ok_or_else(|| Error::Network(format!("created network {name} but it is not visible")))
    }

    /// Look up a network. `Ok(None)` if podman does not have it.
    pub async fn network(&self, name: &str) -> Result<Option<NetworkInfo>> {
        let handle = self.podman().networks().get(name);
        if !handle
            .exists()
            .await
            .map_err(|e| Error::Network(format!("exists {name}: {e}")))?
        {
            return Ok(None);
        }
        let n = handle
            .inspect()
            .await
            .map_err(|e| Error::Network(format!("inspect {name}: {e}")))?;
        Ok(Some(network_info(name, n)))
    }

    /// Networks carrying a label key, so the reconciler finds pgpod's own.
    pub async fn list_networks_labelled(&self, label: &str) -> Result<Vec<NetworkInfo>> {
        let opts = NetworkListOpts::builder()
            .filter([podman_api::opts::NetworkListFilter::LabelKey(
                label.to_string(),
            )])
            .build();
        let nets = self
            .podman()
            .networks()
            .list(&opts)
            .await
            .map_err(|e| Error::Network(format!("list: {e}")))?;
        Ok(nets
            .into_iter()
            .map(|n| {
                let name = n.name.clone().unwrap_or_default();
                network_info(&name, n)
            })
            .collect())
    }

    /// Remove a network. Safe: podman refuses while containers are
    /// attached, and a network holds no data.
    pub async fn remove_network(&self, name: &str) -> Result<()> {
        self.podman()
            .networks()
            .get(name)
            .remove()
            .await
            .map(|_| ())
            .map_err(|e| Error::Network(format!("remove {name}: {e}")))
    }
}

fn network_info(name: &str, n: podman_api::models::Network) -> NetworkInfo {
    // Take the first IPv4 subnet. pgpod creates single-subnet networks, so
    // a second one means someone edited it by hand — using the first is
    // the same choice podman makes for address assignment.
    let subnet = n
        .subnets
        .as_ref()
        .and_then(|s| s.first())
        .and_then(|s| s.subnet.clone());

    NetworkInfo {
        name: n.name.clone().unwrap_or_else(|| name.to_string()),
        id: n.id,
        driver: n.driver,
        dns_enabled: n.dns_enabled.unwrap_or(false),
        subnet,
    }
}
