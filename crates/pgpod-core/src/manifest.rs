//! The cluster manifest — what `pgpod apply` reads.
//!
//! Deliberately shaped like a CloudNativePG `Cluster` so the mental model
//! transfers: `apiVersion`/`kind`/`metadata`/`spec`, camelCase fields,
//! the same names for the same concepts.

use serde::{Deserialize, Serialize};

use crate::{BackupSpec, ClusterId, InitdbBootstrap, ParseInstanceIdError};

pub const API_VERSION: &str = "pgpod/v1";
pub const KIND: &str = "Cluster";

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
}
