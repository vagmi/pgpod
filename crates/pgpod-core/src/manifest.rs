//! The cluster manifest — what `pgpod apply` reads.
//!
//! Deliberately shaped like a CloudNativePG `Cluster` so the mental model
//! transfers: `apiVersion`/`kind`/`metadata`/`spec`, camelCase fields,
//! the same names for the same concepts.

use serde::{Deserialize, Serialize};

use crate::{ClusterId, InitdbBootstrap, ParseInstanceIdError};

pub const API_VERSION: &str = "pgpod/v1";
pub const KIND: &str = "Cluster";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClusterManifest {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: ClusterSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Metadata {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
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
pub struct BootstrapSpec {
    #[serde(default)]
    pub initdb: InitdbBootstrap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
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
    fn round_trips_through_yaml() {
        let m = ClusterManifest::from_yaml(MINIMAL).unwrap();
        let out = serde_yaml::to_string(&m).unwrap();
        assert_eq!(ClusterManifest::from_yaml(&out).unwrap(), m);
    }
}
