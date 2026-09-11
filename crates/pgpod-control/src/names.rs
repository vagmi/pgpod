//! Podman object names derived from a cluster.
//!
//! Every name pgpod creates comes from here or from `pgpod_core::ids`, so
//! the create path and the adoption path cannot disagree about what a
//! thing is called.

use pgpod_core::{ClusterId, InstanceId, PoolerId};

/// Label marking every podman object pgpod owns. Filtering on this is how
/// the reconciler finds its own among whatever else the user runs.
pub const LABEL_CLUSTER: &str = "pgpod.cluster";
pub const LABEL_INSTANCE: &str = "pgpod.instance";
pub const LABEL_POOLER: &str = "pgpod.pooler";

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

    /// This cluster's secrets paired with the same role's on another.
    ///
    /// Written out field by field rather than zipping two [`Self::all`]
    /// arrays. The arrays are in the same order today, and a zip would
    /// keep compiling if someone reordered one of them — pairing the
    /// superuser's name with the monitor's password is not a failure that
    /// announces itself.
    pub fn paired<'a>(&'a self, source: &'a SecretNames) -> [(&'a str, &'a str); 4] {
        [
            (self.superuser.as_str(), source.superuser.as_str()),
            (self.replication.as_str(), source.replication.as_str()),
            (self.monitor.as_str(), source.monitor.as_str()),
            (self.app_owner.as_str(), source.app_owner.as_str()),
        ]
    }
}

/// Labels for a pooler container.
///
/// It carries `pgpod.cluster` for every cluster it fronts, comma
/// separated, because a pooler is not owned by one of them — filtering by
/// a single cluster label would make a shared pooler invisible to some of
/// the clusters it serves.
pub fn pooler_labels(pooler: &PoolerId, clusters: &[&str]) -> Vec<(String, String)> {
    vec![
        (LABEL_POOLER.to_string(), pooler.to_string()),
        (LABEL_CLUSTER.to_string(), clusters.join(",")),
    ]
}

/// Podman secret holding a pooler's admin console password.
///
/// Scoped to the pooler: it is that process's own credential, not the
/// cluster's.
pub fn pooler_admin_secret(pooler: &PoolerId) -> String {
    format!("pgpod-pooler-{pooler}-admin")
}

/// Podman secret holding one cluster's `auth_query` lookup password.
///
/// Scoped to the **cluster**, not the pooler: the role lives in that
/// cluster's database, and two poolers fronting it authenticate as the
/// same role. It also means a pooler fronting several clusters holds
/// several credentials and shares none between them.
pub fn pooler_lookup_secret(cluster: &ClusterId) -> String {
    format!("pgpod-{cluster}-pooler")
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

    #[test]
    fn the_lookup_secret_is_scoped_to_the_cluster_not_the_pooler() {
        // The role lives in the cluster's database, so two poolers
        // fronting it authenticate as the same role with the same
        // password. Scoping this per pooler would create a second role
        // password for a role that has one.
        let a = pooler_lookup_secret(&ClusterId::new("mydb").unwrap());
        assert_eq!(a, "pgpod-mydb-pooler");
        assert_ne!(a, pooler_lookup_secret(&ClusterId::new("other").unwrap()));
    }

    #[test]
    fn the_admin_secret_is_scoped_to_the_pooler() {
        let a = pooler_admin_secret(&PoolerId::new("app").unwrap());
        assert_eq!(a, "pgpod-pooler-app-admin");
        assert_ne!(a, pooler_admin_secret(&PoolerId::new("other").unwrap()));
    }

    #[test]
    fn a_shared_pooler_is_labelled_with_every_cluster_it_fronts() {
        // Filtering on a single cluster label would make a shared pooler
        // invisible to some of the clusters it serves.
        let labels = pooler_labels(&PoolerId::new("app").unwrap(), &["alpha", "beta"]);
        assert!(labels.contains(&(LABEL_POOLER.into(), "app".into())));
        assert!(labels.contains(&(LABEL_CLUSTER.into(), "alpha,beta".into())));
    }

    #[test]
    fn pairing_matches_each_role_to_the_same_role() {
        // Restore adopts the source's credentials role by role. Pairing
        // the superuser's secret name with the monitor's value would
        // produce a cluster whose passwords are all subtly wrong, and
        // nothing would say so until someone tried to connect.
        let target = SecretNames::for_cluster(&ClusterId::new("restored").unwrap());
        let source = SecretNames::for_cluster(&ClusterId::new("mydb").unwrap());
        for (to, from) in target.paired(&source) {
            let role = to.strip_prefix("pgpod-restored-").expect(to);
            assert_eq!(
                from,
                format!("pgpod-mydb-{role}"),
                "{to} was paired with {from}"
            );
        }
    }

    #[test]
    fn pairing_covers_every_secret() {
        let target = SecretNames::for_cluster(&ClusterId::new("restored").unwrap());
        let source = SecretNames::for_cluster(&ClusterId::new("mydb").unwrap());
        let paired: Vec<&str> = target.paired(&source).iter().map(|(to, _)| *to).collect();
        for name in target.all() {
            assert!(paired.contains(&name), "{name} is not in the pairing");
        }
    }
}
