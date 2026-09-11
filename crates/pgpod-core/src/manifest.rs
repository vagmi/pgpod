//! The cluster manifest — what `pgpod apply` reads.
//!
//! Deliberately shaped like a CloudNativePG `Cluster` so the mental model
//! transfers: `apiVersion`/`kind`/`metadata`/`spec`, camelCase fields,
//! the same names for the same concepts.

use serde::{Deserialize, Serialize};

use crate::{BackupSpec, ClusterId, InitdbBootstrap, ParseInstanceIdError};

pub const API_VERSION: &str = "pgpod/v1";
pub const KIND: &str = "Cluster";
pub const KIND_POOLER: &str = "Pooler";

// Every type below rejects unknown fields.
//
// A manifest is written by hand, and serde's default is to ignore what it
// does not recognise — so `scheduel: "0 2 * * *"`, or a field pgpod has
// not implemented yet, parses cleanly and does nothing. The operator is
// then told their cluster was applied successfully and believes they have
// something they do not. That is the same failure shape as silently
// truncating `instances: 3`, which this file already refuses.
//
// The cost is forward compatibility: an older pgpod rejects a manifest
// written for a newer one. That is the right way round — better to refuse
// a file you cannot honour than to honour half of it.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ClusterManifest {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: ClusterSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ClusterSpec {
    #[serde(default = "default_instances")]
    pub instances: u32,
    pub image_name: String,
    #[serde(default)]
    pub bootstrap: BootstrapSpec,
    #[serde(default)]
    pub postgresql: PostgresqlSpec,
    #[serde(default)]
    pub storage: StorageSpec,
    /// Object-storage destinations, retention, prefetch.
    ///
    /// Presence of a destination is what flips `archive_mode` from `off`
    /// to `on` in the renderer (`pgpod_pg::ArchiveMode`), so a cluster
    /// created *with* one archives from its very first WAL segment. It is
    /// a postmaster-level setting, which is why adding a destination to a
    /// cluster that already exists needs more than a reload — see
    /// `validate`.
    #[serde(default)]
    pub backup: BackupSpec,
    /// UID/GID the container runs as.
    ///
    /// Exposed because it is genuinely per-image: 999 on Debian-based
    /// `postgres`, 26 on CNPG-style images. Getting it wrong does not fail
    /// cleanly — see `container::DEFAULT_POSTGRES_UID`.
    #[serde(default = "default_pg_uid")]
    pub postgres_uid: u32,
    #[serde(default = "default_pg_uid")]
    pub postgres_gid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct BootstrapSpec {
    #[serde(default)]
    pub initdb: InitdbBootstrap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct PostgresqlSpec {
    /// Tuning knobs. Rejected if they name a setting pgpod manages —
    /// see `pgpod_pg::RESERVED_PARAMETERS`.
    #[serde(default)]
    pub parameters: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub shared_preload_libraries: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct StorageSpec {
    /// **Advisory.** A podman volume has no quota unless the graph root is
    /// XFS with project quotas, and pgpod does not yet set one even there.
    /// Recorded so `pgpod status` can report it, never enforced — a field
    /// that looks like a limit and silently is not would be worse than no
    /// field, so `pgpod doctor` says so out loud.
    #[serde(default)]
    pub size: Option<String>,
}

fn default_instances() -> u32 {
    1
}

fn default_pg_uid() -> u32 {
    crate::container::DEFAULT_POSTGRES_UID
}

impl ClusterManifest {
    /// Parse YAML and validate.
    pub fn from_yaml(input: &str) -> Result<Self, ManifestError> {
        let m: Self =
            serde_yaml::from_str(input).map_err(|e| ManifestError::Parse(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    pub fn cluster_id(&self) -> Result<ClusterId, ManifestError> {
        ClusterId::new(self.metadata.name.clone()).map_err(ManifestError::Name)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        if self.api_version != API_VERSION {
            return Err(ManifestError::ApiVersion(self.api_version.clone()));
        }
        if self.kind != KIND {
            return Err(ManifestError::Kind(self.kind.clone()));
        }
        self.cluster_id()?;

        if self.spec.instances == 0 {
            return Err(ManifestError::Instances(
                "instances must be at least 1".into(),
            ));
        }
        // Phase 1 is single-instance. Say so plainly rather than creating
        // one instance and silently ignoring the rest of the request.
        if self.spec.instances > 1 {
            return Err(ManifestError::Instances(format!(
                "instances: {} — replicas land in Phase 3; pgpod currently \
                 supports a single instance",
                self.spec.instances
            )));
        }
        if self.spec.image_name.trim().is_empty() {
            return Err(ManifestError::ImageName);
        }
        self.validate_destinations()?;
        Ok(())
    }

    /// Catch a malformed destination here, in the manifest, rather than
    /// inside a container at the first `archive_command` invocation —
    /// where the symptom is `pg_wal` filling up and the cause is three
    /// layers away.
    fn validate_destinations(&self) -> Result<(), ManifestError> {
        for dest in &self.spec.backup.destinations {
            let url = dest.url.trim();
            let Some((scheme, rest)) = url.split_once("://") else {
                return Err(ManifestError::Destination(format!(
                    "{url:?} is not a URL — expected something like \
                     gs://bucket/prefix"
                )));
            };
            if rest.trim_start_matches('/').is_empty() {
                return Err(ManifestError::Destination(format!(
                    "{url:?} names no bucket or path"
                )));
            }
            // Matching `object_store::parse_url`'s schemes. An unknown one
            // would otherwise surface as a generic "unable to recognise
            // URL" from three crates down.
            const KNOWN: &[&str] = &["file", "s3", "s3a", "gs", "az", "abfs", "abfss", "memory"];
            if !KNOWN.contains(&scheme) {
                return Err(ManifestError::Destination(format!(
                    "{url:?} uses an unsupported scheme {scheme:?} — pgpod \
                     supports {}",
                    KNOWN.join(", ")
                )));
            }

            // A `file://` path only exists inside the container if
            // something is mounted there. Without this check the mistake
            // surfaces as a failing archive_command on the first segment,
            // three layers from the manifest that caused it.
            if scheme == "file" {
                let mount = crate::container::ARCHIVE_MOUNT;
                if self.spec.backup.volume.is_none() {
                    return Err(ManifestError::Destination(format!(
                        "{url:?} is a local path, so spec.backup.volume must name \
                         a podman volume for pgpod to mount at {mount}"
                    )));
                }
                let path = format!("/{}", rest.trim_start_matches('/'));
                if path != mount && !path.starts_with(&format!("{mount}/")) {
                    return Err(ManifestError::Destination(format!(
                        "{url:?} is outside {mount}, where spec.backup.volume is \
                         mounted — nothing else in the container is writable"
                    )));
                }
            }
        }

        if self.spec.backup.volume.is_some()
            && !self
                .spec
                .backup
                .destinations
                .iter()
                .any(|d| d.url.trim().starts_with("file://"))
        {
            // A mounted volume nothing writes to looks like a working
            // local archive and is an empty directory.
            return Err(ManifestError::Destination(
                "spec.backup.volume is set but no destination is a file:// URL".to_string(),
            ));
        }
        Ok(())
    }

    /// `spec.postgresql.parameters` as the ordered pairs the renderer wants.
    pub fn parameters(&self) -> Vec<(String, String)> {
        self.spec
            .postgresql
            .parameters
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// A manifest file, whichever kind it declares.
///
/// `pgpod apply -f` dispatches on this. It probes `kind` first and then
/// parses the concrete type, rather than leaning on an untagged enum:
/// every manifest type here is `deny_unknown_fields`, and serde's untagged
/// error for two such variants is "data did not match any variant", which
/// names neither the field that was wrong nor the kind it was trying to
/// be. The whole reason unknown fields are refused is to tell an operator
/// exactly what to edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Manifest {
    Cluster(Box<ClusterManifest>),
    Pooler(Box<PoolerManifest>),
}

/// Just enough of any manifest to find out what it is.
#[derive(Deserialize)]
struct KindProbe {
    #[serde(rename = "apiVersion")]
    api_version: Option<String>,
    kind: Option<String>,
}

impl Manifest {
    pub fn from_yaml(input: &str) -> Result<Self, ManifestError> {
        let probe: KindProbe =
            serde_yaml::from_str(input).map_err(|e| ManifestError::Parse(e.to_string()))?;

        // Checked before `kind`, so a file from a future pgpod is reported
        // as a version problem rather than as an unknown kind.
        match probe.api_version.as_deref() {
            Some(API_VERSION) => {}
            Some(other) => return Err(ManifestError::ApiVersion(other.to_string())),
            None => return Err(ManifestError::MissingKind("apiVersion".into())),
        }

        match probe.kind.as_deref() {
            Some(KIND) => Ok(Self::Cluster(Box::new(ClusterManifest::from_yaml(input)?))),
            Some(KIND_POOLER) => Ok(Self::Pooler(Box::new(PoolerManifest::from_yaml(input)?))),
            Some(other) => Err(ManifestError::Kind(other.to_string())),
            None => Err(ManifestError::MissingKind("kind".into())),
        }
    }

    /// The object's name, whichever kind it is.
    pub fn name(&self) -> &str {
        match self {
            Self::Cluster(m) => &m.metadata.name,
            Self::Pooler(m) => &m.metadata.name,
        }
    }
}

/// `kind: Pooler` — a connection pooler in front of one or more clusters.
///
/// A separate kind rather than a field on `Cluster`, and the reference
/// points this way only (ADR 05 §2): `apply -f cluster.yaml` must not be
/// able to reconfigure a pooler it does not name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct PoolerManifest {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: PoolerSpecManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct PoolerSpecManifest {
    /// `rw` today. `ro` needs standbys to route to (ADR 05 §8).
    #[serde(default, rename = "type")]
    pub pooler_type: PoolerType,

    #[serde(default = "default_pooler_image")]
    pub image_name: String,

    /// Host port. `None` allocates one and persists it, as instances do,
    /// so applications keep working across a recreate.
    #[serde(default)]
    pub port: Option<u16>,

    /// The clusters this pooler fronts, and the pools it exports for each.
    pub clusters: Vec<PoolerClusterRef>,

    #[serde(default)]
    pub pg_doorman: PgDoormanSpec,

    #[serde(default = "default_pooler_uid")]
    pub pooler_uid: u32,
    #[serde(default = "default_pooler_gid")]
    pub pooler_gid: u32,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoolerType {
    #[default]
    Rw,
    Ro,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct PoolerClusterRef {
    pub cluster: String,
    /// Which databases to pool. Empty means "the cluster's application
    /// database", resolved when the pooler is applied — the manifest does
    /// not repeat something the cluster already states.
    #[serde(default)]
    pub pools: Vec<PoolRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct PoolRef {
    /// The database on the cluster.
    pub database: String,
    /// What clients put in `dbname`. Defaults to `database`.
    ///
    /// A pg_doorman pool key is a user-visible identifier, not an internal
    /// one, so the default is the obvious thing and this is the escape
    /// hatch for two clusters exporting the same database name.
    #[serde(default, rename = "as")]
    pub as_name: Option<String>,
}

impl PoolRef {
    pub fn pool_name(&self) -> &str {
        self.as_name.as_deref().unwrap_or(&self.database)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct PgDoormanSpec {
    #[serde(default)]
    pub pool_mode: PoolMode,

    #[serde(default = "default_pool_size")]
    pub pool_size: u32,

    /// How long a client waits for a backend — under ordinary pool
    /// pressure **and** during a switchover hold.
    ///
    /// One field for both because pg_doorman has one setting:
    /// `query_wait_timeout` is captured when pools are constructed and is
    /// not re-read on `RELOAD` (measured — ADR 05 §3). A recreate that
    /// overruns this fails clients instead of holding them, and changing
    /// it recreates the pooler container.
    #[serde(default = "default_max_hold")]
    pub max_hold: crate::HumanDuration,

    /// Escape hatch into pg_doorman's `[general]` section. Keys pgpod
    /// manages are refused; see `pgpod_pooler::RESERVED_PARAMETERS`.
    #[serde(default)]
    pub parameters: std::collections::BTreeMap<String, String>,
}

impl Default for PgDoormanSpec {
    fn default() -> Self {
        Self {
            pool_mode: PoolMode::default(),
            pool_size: default_pool_size(),
            max_hold: default_max_hold(),
            parameters: std::collections::BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoolMode {
    #[default]
    Transaction,
    Session,
}

impl PoolMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transaction => "transaction",
            Self::Session => "session",
        }
    }
}

fn default_pooler_image() -> String {
    crate::container::DEFAULT_POOLER_IMAGE.to_string()
}

fn default_pooler_uid() -> u32 {
    crate::container::DEFAULT_POOLER_UID
}

fn default_pooler_gid() -> u32 {
    crate::container::DEFAULT_POOLER_GID
}

fn default_pool_size() -> u32 {
    40
}

fn default_max_hold() -> crate::HumanDuration {
    crate::HumanDuration::from_secs(60)
}

impl PoolerManifest {
    pub fn from_yaml(input: &str) -> Result<Self, ManifestError> {
        let m: Self =
            serde_yaml::from_str(input).map_err(|e| ManifestError::Parse(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    pub fn pooler_id(&self) -> Result<crate::PoolerId, ManifestError> {
        crate::PoolerId::new(self.metadata.name.clone()).map_err(ManifestError::Name)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        if self.api_version != API_VERSION {
            return Err(ManifestError::ApiVersion(self.api_version.clone()));
        }
        if self.kind != KIND_POOLER {
            return Err(ManifestError::Kind(self.kind.clone()));
        }
        self.pooler_id()?;

        if self.spec.pooler_type == PoolerType::Ro {
            return Err(ManifestError::Pooler(
                "type: ro needs standbys to route to — read pools land with \
                 replicas in Phase 3; use type: rw"
                    .into(),
            ));
        }
        if self.spec.clusters.is_empty() {
            return Err(ManifestError::Pooler(
                "spec.clusters must name at least one cluster — a pooler with \
                 nothing behind it would accept connections and have nowhere \
                 to send them"
                    .into(),
            ));
        }
        if self.spec.pg_doorman.pool_size == 0 {
            return Err(ManifestError::Pooler(
                "spec.pgDoorman.poolSize must be at least 1".into(),
            ));
        }

        let mut seen_clusters = std::collections::BTreeSet::new();
        for c in &self.spec.clusters {
            crate::ClusterId::new(c.cluster.clone()).map_err(ManifestError::Name)?;
            if !seen_clusters.insert(c.cluster.clone()) {
                return Err(ManifestError::Pooler(format!(
                    "spec.clusters names {:?} twice — merge its pools into one \
                     entry",
                    c.cluster
                )));
            }
            for p in &c.pools {
                if p.database.trim().is_empty() {
                    return Err(ManifestError::Pooler(format!(
                        "a pool on cluster {:?} has an empty database",
                        c.cluster
                    )));
                }
            }
        }

        // Pool names are what clients put in `dbname`, so two of them
        // cannot be the same. Refused here, naming both, rather than
        // silently disambiguating: guessing a name an operator did not
        // write is the failure `deny_unknown_fields` exists to prevent.
        let mut seen: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
        for c in &self.spec.clusters {
            for p in &c.pools {
                if let Some(other) = seen.insert(p.pool_name(), &c.cluster) {
                    return Err(ManifestError::Pooler(format!(
                        "two pools are both named {:?} — one on cluster {:?} and \
                         one on {:?}. Clients address a pool by that name, so it \
                         has to be unique; set `as:` on one of them",
                        p.pool_name(),
                        other,
                        c.cluster
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("could not parse the manifest: {0}")]
    Parse(String),

    #[error("unsupported apiVersion {0:?} — expected {API_VERSION:?}")]
    ApiVersion(String),

    #[error("unsupported kind {0:?} — expected {KIND:?}")]
    Kind(String),

    #[error("metadata.name: {0}")]
    Name(#[from] ParseInstanceIdError),

    #[error("spec.instances: {0}")]
    Instances(String),

    #[error("spec.imageName is required")]
    ImageName,

    #[error("the manifest declares no {0}")]
    MissingKind(String),

    #[error("{0}")]
    Pooler(String),

    #[error("spec.backup.destinations: {0}")]
    Destination(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
apiVersion: pgpod/v1
kind: Cluster
metadata:
  name: mydb
spec:
  imageName: docker.io/library/postgres:18
"#;

    #[test]
    fn parses_a_minimal_manifest_with_sensible_defaults() {
        let m = ClusterManifest::from_yaml(MINIMAL).unwrap();
        assert_eq!(m.cluster_id().unwrap().as_str(), "mydb");
        assert_eq!(m.spec.instances, 1);
        assert_eq!(m.spec.postgres_uid, 999);
        assert_eq!(
            m.spec.bootstrap.initdb.locale, "C",
            "locale must default to C even through the manifest path"
        );
        assert_eq!(m.spec.bootstrap.initdb.encoding, "UTF8");
    }

    #[test]
    fn parses_the_full_documented_shape() {
        let yaml = r#"
apiVersion: pgpod/v1
kind: Cluster
metadata:
  name: mydb
spec:
  instances: 1
  imageName: ghcr.io/example/pg:18
  postgresUid: 26
  postgresGid: 26
  bootstrap:
    initdb:
      database: appdb
      owner: app
  storage:
    size: 20Gi
  postgresql:
    parameters:
      shared_buffers: 512MB
      max_connections: "200"
    sharedPreloadLibraries: [pg_stat_statements]
"#;
        let m = ClusterManifest::from_yaml(yaml).unwrap();
        assert_eq!(m.spec.postgres_uid, 26, "CNPG-style images run as 26");
        assert_eq!(m.spec.bootstrap.initdb.database.as_deref(), Some("appdb"));
        assert_eq!(m.spec.storage.size.as_deref(), Some("20Gi"));
        assert_eq!(
            m.spec.postgresql.shared_preload_libraries,
            vec!["pg_stat_statements"]
        );
        // BTreeMap ordering makes the rendered config stable.
        assert_eq!(
            m.parameters(),
            vec![
                ("max_connections".to_string(), "200".to_string()),
                ("shared_buffers".to_string(), "512MB".to_string()),
            ]
        );
    }

    #[test]
    fn multiple_instances_are_refused_rather_than_silently_truncated() {
        // Creating one instance for a manifest asking for three would look
        // like success and leave the operator believing they had replicas.
        let yaml = MINIMAL.replace("spec:", "spec:\n  instances: 3");
        let err = ClusterManifest::from_yaml(&yaml).unwrap_err();
        assert!(matches!(err, ManifestError::Instances(_)), "{err}");
        assert!(err.to_string().contains("Phase 3"), "{err}");
    }

    #[test]
    fn wrong_api_version_or_kind_is_rejected() {
        for (field, bad) in [
            ("apiVersion: pgpod/v1", "apiVersion: pgpod/v2"),
            ("kind: Cluster", "kind: Pod"),
        ] {
            let yaml = MINIMAL.replace(field, bad);
            assert!(
                ClusterManifest::from_yaml(&yaml).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn an_invalid_cluster_name_is_caught_at_parse_time() {
        // Not at container-create time, where the error would name a
        // podman constraint instead of the manifest field.
        let yaml = MINIMAL.replace("name: mydb", "name: My_DB");
        let err = ClusterManifest::from_yaml(&yaml).unwrap_err();
        assert!(matches!(err, ManifestError::Name(_)), "{err}");
    }

    #[test]
    fn zero_instances_is_rejected() {
        let yaml = MINIMAL.replace("spec:", "spec:\n  instances: 0");
        assert!(ClusterManifest::from_yaml(&yaml).is_err());
    }

    #[test]
    fn parses_backup_destinations() {
        let yaml = r#"
apiVersion: pgpod/v1
kind: Cluster
metadata:
  name: mydb
spec:
  imageName: postgres:18
  backup:
    destinations:
      - url: gs://pgpod-backups/prod
      - url: s3://mirror/pgpod
        endpoint: garage.internal:3900
        region: garage
        verifyTls: false
    retention: 14d
"#;
        let m = ClusterManifest::from_yaml(yaml).unwrap();
        assert_eq!(m.spec.backup.destinations.len(), 2);
        assert_eq!(m.spec.backup.destinations[0].url, "gs://pgpod-backups/prod");
        assert!(!m.spec.backup.destinations[1].verify_tls);
        assert_eq!(m.spec.backup.retention.as_deref(), Some("14d"));
        assert_eq!(
            m.spec.backup.retention_mode,
            crate::RetentionMode::Report,
            "retention must not delete until an operator opts in"
        );
    }

    #[test]
    fn a_bucket_with_no_prefix_is_valid() {
        // The cluster name is always a path component, so several
        // clusters can share one bucket without colliding (ADR 01 §2).
        let yaml = MINIMAL.replace(
            "spec:",
            "spec:\n  backup:\n    destinations:\n      - url: gs://pgpod-backups",
        );
        let m = ClusterManifest::from_yaml(&yaml).unwrap();
        assert_eq!(m.spec.backup.destinations[0].url, "gs://pgpod-backups");
    }

    #[test]
    fn a_local_destination_needs_a_volume_to_live_in() {
        // Inside a container with a read-only rootfs, a file:// path that
        // nothing is mounted at fails on the first archived segment.
        let yaml = MINIMAL.replace(
            "spec:",
            "spec:\n  backup:\n    destinations:\n      - url: file:///archive/mydb",
        );
        let err = ClusterManifest::from_yaml(&yaml).unwrap_err();
        assert!(matches!(err, ManifestError::Destination(_)), "{err}");
        assert!(err.to_string().contains("spec.backup.volume"), "{err}");
    }

    #[test]
    fn a_local_destination_with_its_volume_is_accepted() {
        let yaml = MINIMAL.replace(
            "spec:",
            "spec:\n  backup:\n    volume: pgpod-mydb-archive\n    \
             destinations:\n      - url: file:///archive/mydb",
        );
        let m = ClusterManifest::from_yaml(&yaml).unwrap();
        assert_eq!(m.spec.backup.volume.as_deref(), Some("pgpod-mydb-archive"));
    }

    #[test]
    fn a_local_destination_outside_the_mount_point_is_refused() {
        // /srv/backups is not reachable from inside the container, and
        // archiving there would write into the read-only rootfs.
        let yaml = MINIMAL.replace(
            "spec:",
            "spec:\n  backup:\n    volume: v\n    destinations:\n      \
             - url: file:///srv/backups",
        );
        let err = ClusterManifest::from_yaml(&yaml).unwrap_err();
        assert!(err.to_string().contains("/archive"), "{err}");
    }

    #[test]
    fn a_volume_with_nothing_writing_to_it_is_refused() {
        // It would look like a working local archive and be an empty
        // directory.
        let yaml = MINIMAL.replace(
            "spec:",
            "spec:\n  backup:\n    volume: v\n    destinations:\n      \
             - url: gs://pgpod-backups",
        );
        assert!(ClusterManifest::from_yaml(&yaml).is_err());
    }

    #[test]
    fn a_malformed_destination_is_caught_in_the_manifest() {
        // Not at the first archive_command invocation, where the symptom
        // would be pg_wal filling up.
        for bad in ["/srv/backups", "ftp://host/path", "gs://"] {
            let yaml = MINIMAL.replace(
                "spec:",
                &format!("spec:\n  backup:\n    destinations:\n      - url: {bad}"),
            );
            let err = ClusterManifest::from_yaml(&yaml).unwrap_err();
            assert!(matches!(err, ManifestError::Destination(_)), "{bad}: {err}");
        }
    }

    #[test]
    fn round_trips_through_yaml() {
        let m = ClusterManifest::from_yaml(MINIMAL).unwrap();
        let out = serde_yaml::to_string(&m).unwrap();
        assert_eq!(ClusterManifest::from_yaml(&out).unwrap(), m);
    }

    // ---- kind: Pooler -------------------------------------------------

    const POOLER: &str = r#"
apiVersion: pgpod/v1
kind: Pooler
metadata:
  name: app
spec:
  clusters:
    - cluster: mydb
"#;

    #[test]
    fn parses_a_minimal_pooler_with_sensible_defaults() {
        let m = PoolerManifest::from_yaml(POOLER).unwrap();
        assert_eq!(m.pooler_id().unwrap().as_str(), "app");
        assert_eq!(m.spec.clusters.len(), 1);
        assert!(
            m.spec.clusters[0].pools.is_empty(),
            "no pools listed means the cluster's application database, \
             resolved at apply"
        );
        assert_eq!(m.spec.pooler_type, PoolerType::Rw);
        assert_eq!(m.spec.pg_doorman.pool_mode, PoolMode::Transaction);
        assert_eq!(m.spec.pg_doorman.pool_size, 40);
        assert_eq!(m.spec.pg_doorman.max_hold.as_secs(), 60);
        assert_eq!(m.spec.image_name, crate::container::DEFAULT_POOLER_IMAGE);
        assert!(
            !m.spec.image_name.ends_with(":latest"),
            "the default pooler image must be pinned: it is on the data path, \
             and a tag that moves under a restart is not something to discover \
             during an incident"
        );
    }

    #[test]
    fn parses_the_full_documented_pooler_shape() {
        let yaml = r#"
apiVersion: pgpod/v1
kind: Pooler
metadata:
  name: app
spec:
  type: rw
  imageName: ghcr.io/ozontech/pg_doorman:3.11.0
  port: 6432
  clusters:
    - cluster: mydb
      pools:
        - database: appdb
    - cluster: other
      pools:
        - database: shop
          as: other-shop
  pgDoorman:
    poolMode: session
    poolSize: 10
    maxHold: "90s"
    parameters:
      worker_threads: "4"
"#;
        let m = PoolerManifest::from_yaml(yaml).unwrap();
        assert_eq!(m.spec.port, Some(6432));
        assert_eq!(m.spec.clusters[1].pools[0].pool_name(), "other-shop");
        assert_eq!(m.spec.clusters[0].pools[0].pool_name(), "appdb");
        assert_eq!(m.spec.pg_doorman.pool_mode, PoolMode::Session);
        assert_eq!(m.spec.pg_doorman.max_hold.as_secs(), 90);
    }

    #[test]
    fn one_pooler_may_front_several_clusters() {
        // The property `spec.clusters` exists for. pg_doorman pools are
        // per-database with their own server_host, and a container can
        // join several podman networks, so ADR 00 SS10 is untouched.
        let m = PoolerManifest::from_yaml(
            r#"
apiVersion: pgpod/v1
kind: Pooler
metadata: { name: shared }
spec:
  clusters:
    - cluster: alpha
      pools: [{ database: appdb, as: alpha }]
    - cluster: beta
      pools: [{ database: appdb, as: beta }]
"#,
        )
        .unwrap();
        assert_eq!(m.spec.clusters.len(), 2);
    }

    #[test]
    fn two_pools_with_the_same_name_are_refused_naming_both_clusters() {
        // A pool name is what the client puts in dbname, so it has to be
        // unique. Silently disambiguating would hand the operator a
        // connection string they never wrote.
        let err = PoolerManifest::from_yaml(
            r#"
apiVersion: pgpod/v1
kind: Pooler
metadata: { name: shared }
spec:
  clusters:
    - cluster: alpha
      pools: [{ database: appdb }]
    - cluster: beta
      pools: [{ database: appdb }]
"#,
        )
        .unwrap_err();
        assert!(matches!(err, ManifestError::Pooler(_)), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("alpha") && msg.contains("beta"), "{msg}");
        assert!(msg.contains("as:"), "the message must name the fix: {msg}");
    }

    #[test]
    fn the_same_cluster_listed_twice_is_refused() {
        let err = PoolerManifest::from_yaml(
            r#"
apiVersion: pgpod/v1
kind: Pooler
metadata: { name: shared }
spec:
  clusters:
    - cluster: mydb
      pools: [{ database: a }]
    - cluster: mydb
      pools: [{ database: b }]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("twice"), "{err}");
    }

    #[test]
    fn a_pooler_with_no_clusters_is_refused() {
        let yaml = POOLER.replace("  clusters:\n    - cluster: mydb\n", "  clusters: []\n");
        let err = PoolerManifest::from_yaml(&yaml).unwrap_err();
        assert!(matches!(err, ManifestError::Pooler(_)), "{err}");
    }

    #[test]
    fn a_read_only_pooler_is_refused_naming_the_phase() {
        // The same treatment `instances: 3` gets: say what is missing
        // rather than accept the field and route reads to the primary.
        let yaml = POOLER.replace("spec:", "spec:\n  type: ro");
        let err = PoolerManifest::from_yaml(&yaml).unwrap_err();
        assert!(err.to_string().contains("Phase 3"), "{err}");
    }

    #[test]
    fn a_bare_number_for_max_hold_is_refused() {
        // pg_doorman reads a unitless duration as milliseconds, so
        // `maxHold: 60` is a 60ms switchover budget that looks like a
        // minute. It has to fail at parse time, not mid-window.
        let yaml = POOLER.replace("spec:", "spec:\n  pgDoorman:\n    maxHold: 60");
        let err = PoolerManifest::from_yaml(&yaml).unwrap_err();
        assert!(err.to_string().contains("millisecond"), "{err}");
    }

    #[test]
    fn an_unknown_pooler_field_is_refused_like_every_other_manifest() {
        let yaml = POOLER.replace("spec:", "spec:\n  poolMode: transaction");
        assert!(
            PoolerManifest::from_yaml(&yaml).is_err(),
            "poolMode belongs under pgDoorman; a manifest that ignores it \
             would report success and pool in the wrong mode"
        );
    }

    #[test]
    fn pooler_yaml_round_trips() {
        let m = PoolerManifest::from_yaml(POOLER).unwrap();
        let out = serde_yaml::to_string(&m).unwrap();
        assert_eq!(PoolerManifest::from_yaml(&out).unwrap(), m);
    }

    // ---- dispatch -----------------------------------------------------

    #[test]
    fn dispatch_routes_each_kind_to_its_own_parser() {
        assert!(matches!(
            Manifest::from_yaml(MINIMAL).unwrap(),
            Manifest::Cluster(_)
        ));
        assert!(matches!(
            Manifest::from_yaml(POOLER).unwrap(),
            Manifest::Pooler(_)
        ));
        assert_eq!(Manifest::from_yaml(POOLER).unwrap().name(), "app");
    }

    #[test]
    fn dispatch_reports_the_field_that_is_wrong_not_just_no_variant_matched() {
        // The reason for probing `kind` before parsing. An untagged enum
        // over two deny_unknown_fields structs answers "data did not match
        // any variant", which names neither the kind nor the bad field.
        let yaml = POOLER.replace("  clusters:", "  clusterz:");
        let err = Manifest::from_yaml(&yaml).unwrap_err();
        assert!(
            err.to_string().contains("clusterz"),
            "the error must name the offending field: {err}"
        );
    }

    #[test]
    fn dispatch_rejects_an_unknown_kind_and_a_missing_one() {
        let unknown = MINIMAL.replace("kind: Cluster", "kind: Banana");
        assert!(matches!(
            Manifest::from_yaml(&unknown).unwrap_err(),
            ManifestError::Kind(_)
        ));

        let none = MINIMAL.replace("kind: Cluster\n", "");
        assert!(matches!(
            Manifest::from_yaml(&none).unwrap_err(),
            ManifestError::MissingKind(_)
        ));
    }

    #[test]
    fn dispatch_reports_a_future_api_version_as_a_version_problem() {
        // Checked before `kind`, so a manifest from a newer pgpod says so
        // rather than complaining about a kind this build never heard of.
        let yaml = POOLER
            .replace("pgpod/v1", "pgpod/v2")
            .replace("kind: Pooler", "kind: Sharder");
        assert!(matches!(
            Manifest::from_yaml(&yaml).unwrap_err(),
            ManifestError::ApiVersion(_)
        ));
    }

    #[test]
    fn each_kind_refuses_the_other() {
        assert!(ClusterManifest::from_yaml(POOLER).is_err());
        assert!(PoolerManifest::from_yaml(MINIMAL).is_err());
    }
}
