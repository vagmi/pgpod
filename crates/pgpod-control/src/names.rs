//! Podman object names derived from a cluster.
//!
//! Every name pgpod creates comes from here or from `pgpod_core::ids`, so
//! the create path and the adoption path cannot disagree about what a
//! thing is called.

use pgpod_core::{ClusterId, InstanceId};

/// Label marking every podman object pgpod owns. Filtering on this is how
/// the reconciler finds its own among whatever else the user runs.
pub const LABEL_CLUSTER: &str = "pgpod.cluster";
pub const LABEL_INSTANCE: &str = "pgpod.instance";

pub fn cluster_labels(cluster: &ClusterId) -> Vec<(String, String)> {
    vec![(LABEL_CLUSTER.to_string(), cluster.to_string())]
}

pub fn instance_labels(instance: &InstanceId) -> Vec<(String, String)> {
    vec![
        (LABEL_CLUSTER.to_string(), instance.cluster().to_string()),
        (LABEL_INSTANCE.to_string(), instance.to_string()),
    ]
}

/// Podman secret names for one cluster.
///
/// Scoped per cluster, not per instance: every instance of a cluster
/// shares the same superuser and replication credentials, and a standby
/// that could not authenticate with the primary's password would be
/// useless.
pub struct SecretNames {
    pub superuser: String,
    pub replication: String,
    pub monitor: String,
    pub app_owner: String,
}

impl SecretNames {
    pub fn for_cluster(cluster: &ClusterId) -> Self {
        let c = cluster.as_str();
        Self {
            superuser: format!("pgpod-{c}-superuser"),
            replication: format!("pgpod-{c}-replication"),
            monitor: format!("pgpod-{c}-monitor"),
            app_owner: format!("pgpod-{c}-app-owner"),
        }
    }

    pub fn all(&self) -> [&str; 4] {
        [
            &self.superuser,
            &self.replication,
            &self.monitor,
            &self.app_owner,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_names_are_scoped_per_cluster_not_per_instance() {
        // Instances of one cluster must share credentials, or a standby
        // could not authenticate against its primary.
        let a = SecretNames::for_cluster(&ClusterId::new("mydb").unwrap());
        let b = SecretNames::for_cluster(&ClusterId::new("other").unwrap());
        assert_eq!(a.superuser, "pgpod-mydb-superuser");
        assert_ne!(a.superuser, b.superuser);
    }

    #[test]
    fn instance_labels_carry_both_scopes() {
        // The reconciler filters by cluster; adoption matches by instance.
        let id = ClusterId::new("mydb").unwrap().instance(2);
        let labels = instance_labels(&id);
        assert!(labels.contains(&(LABEL_CLUSTER.into(), "mydb".into())));
        assert!(labels.contains(&(LABEL_INSTANCE.into(), "mydb-2".into())));
    }
}
