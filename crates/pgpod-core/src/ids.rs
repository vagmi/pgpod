//! Stable identifiers for clusters and instances.
//!
//! Every podman-visible name pgpod creates is derived here — container,
//! volume, network, replication slot — so the reconciler, the CLI, and the
//! adoption path all format them identically. A name computed ad hoc
//! somewhere else is a bug waiting for a restart to surface it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Characters permitted in a cluster name.
///
/// Deliberately narrower than podman's own rules: a cluster name becomes
/// part of a container name, a volume name, a network name, *and* a
/// PostgreSQL replication slot name. Slot names are the strictest of those
/// — lowercase letters, digits, and underscore only — so the intersection
/// is what we accept. Rejecting at the door beats discovering it at
/// `CREATE_REPLICATION_SLOT` time.
fn valid_name_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
}

/// Stable identifier for a cluster.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClusterId(String);

impl ClusterId {
    /// Validate and construct. Names must be 1–40 characters of
    /// `[a-z0-9-]`, starting with a letter and not ending in `-`.
    pub fn new(name: impl Into<String>) -> Result<Self, ParseInstanceIdError> {
        let name = name.into();
        if name.is_empty() || name.len() > 40 {
            return Err(ParseInstanceIdError::BadClusterName(name));
        }
        if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
            return Err(ParseInstanceIdError::BadClusterName(name));
        }
        if name.ends_with('-') || !name.chars().all(valid_name_char) {
            return Err(ParseInstanceIdError::BadClusterName(name));
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Podman network name for this cluster. One bridge network per
    /// cluster gives aardvark-dns name resolution between instances
    /// (ADR 00 §10).
    pub fn network_name(&self) -> String {
        format!("pgpod-{}", self.0)
    }

    /// The instance with the given ordinal in this cluster.
    pub fn instance(&self, ordinal: u32) -> InstanceId {
        InstanceId {
            cluster: self.clone(),
            ordinal,
        }
    }
}

impl fmt::Display for ClusterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ClusterId {
    type Err = ParseInstanceIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Stable identifier for one instance within a cluster, rendered as
/// `<cluster>-<ordinal>` (e.g. `mydb-1`).
///
/// Ordinals start at 1 and are never reused within a cluster's lifetime,
/// so a rebuilt instance gets a fresh ordinal rather than inheriting the
/// stale volume and container of its predecessor.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InstanceId {
    cluster: ClusterId,
    ordinal: u32,
}

impl InstanceId {
    pub fn new(cluster: ClusterId, ordinal: u32) -> Self {
        Self { cluster, ordinal }
    }

    pub fn cluster(&self) -> &ClusterId {
        &self.cluster
    }

    pub fn ordinal(&self) -> u32 {
        self.ordinal
    }

    /// Podman container name. The `pgpod-` prefix is what lets the
    /// reconciler recognise its own containers among anything else the
    /// service user runs (ADR 00 §7).
    pub fn container_name(&self) -> String {
        format!("pgpod-{}-{}", self.cluster.0, self.ordinal)
    }

    /// Podman volume name holding this instance's `/pgdata`.
    pub fn volume_name(&self) -> String {
        format!("pgpod-{}-{}-pgdata", self.cluster.0, self.ordinal)
    }

    /// Name of the short-lived job container used for `pg_rewind` and
    /// other work that requires the instance's postmaster to be stopped
    /// (ADR 02 §5).
    pub fn job_container_name(&self, job: &str) -> String {
        format!("pgpod-{}-{}-{}", self.cluster.0, self.ordinal, job)
    }

    /// Physical replication slot this instance holds on its primary.
    ///
    /// Slot names allow only `[a-z0-9_]`, so the `-` separators used
    /// everywhere else become `_` here. This is the reason `ClusterId`
    /// restricts its own charset.
    pub fn slot_name(&self) -> String {
        format!(
            "pgpod_{}_{}",
            self.cluster.0.replace('-', "_"),
            self.ordinal
        )
    }

    /// `application_name` this instance reports to its primary, which is
    /// what `synchronous_standby_names` matches against.
    pub fn application_name(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.cluster.0, self.ordinal)
    }
}

impl FromStr for InstanceId {
    type Err = ParseInstanceIdError;

    /// Parse `<cluster>-<ordinal>`. Splits on the *last* `-` because
    /// cluster names may themselves contain one.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (cluster, ordinal) = s
            .rsplit_once('-')
            .ok_or_else(|| ParseInstanceIdError::MissingOrdinal(s.to_string()))?;
        let ordinal: u32 = ordinal
            .parse()
            .map_err(|_| ParseInstanceIdError::MissingOrdinal(s.to_string()))?;
        if ordinal == 0 {
            return Err(ParseInstanceIdError::ZeroOrdinal(s.to_string()));
        }
        Ok(Self {
            cluster: ClusterId::new(cluster)?,
            ordinal,
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseInstanceIdError {
    #[error(
        "invalid cluster name {0:?}: expected 1-40 characters of [a-z0-9-], \
         starting with a letter and not ending in '-'"
    )]
    BadClusterName(String),

    #[error("invalid instance id {0:?}: expected <cluster>-<ordinal>, e.g. \"mydb-1\"")]
    MissingOrdinal(String),

    #[error("invalid instance id {0:?}: ordinals start at 1")]
    ZeroOrdinal(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(name: &str) -> ClusterId {
        ClusterId::new(name).expect("valid cluster name")
    }

    #[test]
    fn accepts_reasonable_cluster_names() {
        for name in ["mydb", "my-db", "a", "pg17-staging"] {
            assert!(ClusterId::new(name).is_ok(), "{name} should be valid");
        }
    }

    #[test]
    fn rejects_names_that_would_break_downstream_identifiers() {
        // Uppercase and '_' break podman/DNS conventions; a leading digit
        // and a trailing '-' break container naming; '.' would break the
        // network name.
        for name in ["MyDb", "my_db", "1db", "db-", "my.db", ""] {
            assert!(ClusterId::new(name).is_err(), "{name} should be rejected");
        }
        assert!(
            ClusterId::new("x".repeat(41)).is_err(),
            "over-long rejected"
        );
    }

    #[test]
    fn derives_every_podman_visible_name() {
        let id = cluster("mydb").instance(2);
        assert_eq!(id.to_string(), "mydb-2");
        assert_eq!(id.container_name(), "pgpod-mydb-2");
        assert_eq!(id.volume_name(), "pgpod-mydb-2-pgdata");
        assert_eq!(id.job_container_name("rewind"), "pgpod-mydb-2-rewind");
        assert_eq!(id.cluster().network_name(), "pgpod-mydb");
    }

    #[test]
    fn slot_names_replace_hyphens_with_underscores() {
        // PostgreSQL replication slot names allow only [a-z0-9_], so a
        // hyphenated cluster name must not leak through verbatim.
        let id = cluster("my-db").instance(3);
        assert_eq!(id.slot_name(), "pgpod_my_db_3");
        assert!(
            id.slot_name()
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "slot name must be a legal PostgreSQL identifier"
        );
    }

    #[test]
    fn instance_ids_round_trip_through_strings() {
        for s in ["mydb-1", "my-db-12", "pg17-staging-3"] {
            let id: InstanceId = s.parse().expect("parses");
            assert_eq!(id.to_string(), s);
        }
    }

    #[test]
    fn parsing_splits_on_the_last_hyphen() {
        // "my-db-2" is cluster "my-db" ordinal 2, not cluster "my" ordinal
        // "db-2". Splitting on the first '-' would silently mis-attribute
        // every hyphenated cluster.
        let id: InstanceId = "my-db-2".parse().expect("parses");
        assert_eq!(id.cluster().as_str(), "my-db");
        assert_eq!(id.ordinal(), 2);
    }

    #[test]
    fn rejects_malformed_instance_ids() {
        assert!("mydb".parse::<InstanceId>().is_err(), "no ordinal");
        assert!(
            "mydb-x".parse::<InstanceId>().is_err(),
            "non-numeric ordinal"
        );
        assert!(
            "mydb-0".parse::<InstanceId>().is_err(),
            "ordinals start at 1"
        );
    }
}
