//! Where backups go.
//!
//! A [`Destination`] is a bucket plus an optional path prefix, written as
//! a URL. `pgpod-backup` translates it into a pgBackRest repository, and
//! pgBackRest appends the stanza — so the cluster name is always a path
//! component and several clusters can share one bucket without colliding
//! (ADR 04 §2).
//!
//! This type lives in `pgpod-core` rather than `pgpod-backup` because both
//! ends of the wire need it and neither may depend on the other: the
//! manifest parses it, and the [`crate::InstanceSpec`] carries it into the
//! container so the agent can render `pgbackrest.conf`.
//!
//! **Nothing secret goes in here.** Credentials reach the container as
//! mounted podman secrets, the same as every other password (ADR 00 §9),
//! and on GCE there are none at all — `object_store` reads the instance
//! metadata server. See [`SECRET_OBJECT_STORE_PREFIX`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One object-storage destination.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct Destination {
    /// `gs://bucket/prefix`, `s3://bucket/prefix`, or `file:///path`.
    ///
    /// The prefix is optional; `gs://pgpod-backups` and
    /// `gs://pgpod-backups/prod` are both valid and differ only in where
    /// the cluster directory lands.
    pub url: String,

    /// S3-compatible endpoint, for Garage, MinIO, or R2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// Region. Garage's default is `garage`, not `us-east-1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// Verify the storage server's TLS certificate.
    ///
    /// **There is no plaintext-HTTP option.** pgBackRest always speaks TLS
    /// to object storage, so a self-signed certificate is as far as an
    /// insecure local fixture can go — turning this off accepts one. It
    /// defaults to `true` and is never inferred from an endpoint, because
    /// disabling certificate verification should be a thing an operator
    /// wrote down.
    #[serde(default = "default_verify_tls")]
    pub verify_tls: bool,

    /// Name of the podman secret holding this destination's credentials.
    ///
    /// A **host** concept: the daemon mounts the named secret at
    /// [`Destination::credentials_path`] for the destination's index, and
    /// the agent reads it from there. It is therefore meaningless inside
    /// the container, and [`crate::InstanceSpec::validate`] refuses a spec
    /// that still carries one — see [`Destination::sanitized`].
    ///
    /// Absent is the normal case on GCE, where `object_store`
    /// authenticates through the instance metadata server and there is
    /// nothing to mount at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,

    /// Raw pgBackRest repository options, without their `repoN-` prefix —
    /// `s3-key-type`, `storage-upload-chunk-size`, `cipher-type`. pgpod
    /// adds the index, so an operator never has to know which repository
    /// number their destination became.
    ///
    /// The escape hatch for knobs pgpod has no opinion about. Named fields
    /// exist for the ones the ADRs discuss; this exists so an operator is
    /// never blocked waiting for pgpod to grow a field. Rendered *after*
    /// the derived options, so an explicit value corrects rather than is
    /// corrected.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
}

fn default_verify_tls() -> bool {
    true
}

/// Written by hand for the same reason `InitdbBootstrap`'s is:
/// `#[serde(default = "...")]` feeds only the *deserializing* path, and a
/// derived `Default` would silently give `verify_tls: false` — turning off
/// certificate verification for every destination built in code rather
/// than parsed from a manifest.
impl Default for Destination {
    fn default() -> Self {
        Self {
            url: String::new(),
            endpoint: None,
            region: None,
            verify_tls: default_verify_tls(),
            credentials: None,
            options: BTreeMap::new(),
        }
    }
}

/// Prefix for the mounted podman secret carrying a destination's
/// credentials, suffixed with the destination's index.
///
/// Destination *zero* is `/run/secrets/pgpod-store-0`. The index rather
/// than a name from the manifest keeps credential material entirely out of
/// [`crate::InstanceSpec`], which travels in an environment variable
/// visible to `podman inspect`.
///
/// The file holds `KEY=value` lines using `object_store`'s own
/// configuration names, so what an operator writes matches what the
/// backend documents.
pub const SECRET_OBJECT_STORE_PREFIX: &str = "/run/secrets/pgpod-store-";

impl Destination {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    /// Where this destination's credentials would be mounted, if it has
    /// any. Absent on GCE, where the metadata server supplies them.
    pub fn credentials_path(index: usize) -> String {
        format!("{SECRET_OBJECT_STORE_PREFIX}{index}")
    }

    /// The copy that goes into the container.
    ///
    /// Drops the podman secret name, which names a thing on the host and
    /// resolves to nothing inside the container. The agent finds the same
    /// credentials at the indexed path instead. Being a distinct method
    /// rather than a field the caller remembers to clear is the point:
    /// forgetting it is a validation error, not a leak.
    pub fn sanitized(&self) -> Self {
        Self {
            credentials: None,
            ..self.clone()
        }
    }
}

/// `spec.backup` in the cluster manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct BackupSpec {
    /// Every destination receives every object. An archive that succeeded
    /// on only some of them is not independently restorable from any of
    /// them (ADR 01 §7).
    #[serde(default)]
    pub destinations: Vec<Destination>,

    /// How long to keep base backups, e.g. `14d`. `None` keeps everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<String>,

    /// Whether retention actually deletes. Defaults to reporting only:
    /// a tool that silently deletes backups has to earn that trust first
    /// (ADR 01 §6).
    #[serde(default)]
    pub retention_mode: RetentionMode,

    /// A podman volume mounted at `container::ARCHIVE_MOUNT` in every
    /// container that touches the archive.
    ///
    /// Only meaningful for `file://` destinations, and required by them:
    /// a container has no other way to reach a filesystem path, and a
    /// `file://` URL pointing at something not mounted would archive into
    /// the container's own read-only rootfs and fail on the first segment.
    /// [`crate::ClusterManifest`] checks the two agree rather than letting
    /// them drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RetentionMode {
    /// List what would be deleted; delete nothing.
    #[default]
    Report,
    /// Actually delete.
    Enforce,
}

impl BackupSpec {
    pub fn is_enabled(&self) -> bool {
        !self.destinations.is_empty()
    }
}

/// Environment variable carrying the JSON-encoded [`BackupJobSpec`].
pub const BACKUP_SPEC_ENV: &str = "PGPOD_BACKUP_SPEC";

/// What the one-shot base backup container is told to do.
///
/// A separate type from [`crate::InstanceSpec`], and a separate
/// environment variable, because a job container is not an instance: it
/// has **no data volume at all** (ADR 01 §4), never touches a PGDATA, and
/// would have nothing to say about `archive_mode` or bootstrap. Folding it
/// into the instance spec would mean a struct whose fields are meaningful
/// only half the time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct BackupJobSpec {
    pub cluster: String,
    /// The instance the backup is taken *from*, recorded in the manifest.
    pub instance: String,
    /// UTC timestamp id, e.g. `20260904T020000Z`.
    pub backup_id: String,
    /// Hostname of the source on the cluster's podman network — the
    /// container name, resolved by aardvark-dns.
    pub source_host: String,
    #[serde(default = "default_pg_port")]
    pub source_port: u16,
    pub destinations: Vec<Destination>,
}

fn default_pg_port() -> u16 {
    5432
}

impl BackupJobSpec {
    pub fn from_env() -> Result<Self, crate::SpecError> {
        let raw = std::env::var(BACKUP_SPEC_ENV).map_err(|_| crate::SpecError::Missing)?;
        serde_json::from_str(&raw).map_err(|e| crate::SpecError::Malformed(e.to_string()))
    }

    pub fn to_env_value(&self) -> Result<String, crate::SpecError> {
        for (index, dest) in self.destinations.iter().enumerate() {
            // Same rule as the instance spec: a podman secret name is a
            // host concept, and this string is visible in
            // `podman inspect` (ADR 00 §9).
            if dest.credentials.is_some() {
                return Err(crate::SpecError::Inconsistent(format!(
                    "backup destination {index} still carries a podman secret name"
                )));
            }
        }
        if self.destinations.is_empty() {
            return Err(crate::SpecError::Inconsistent(
                "a backup job with no destination would upload nowhere and \
                 report success"
                    .into(),
            ));
        }
        serde_json::to_string(self).map_err(|e| crate::SpecError::Malformed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_url_is_a_complete_destination() {
        let d = Destination::new("gs://pgpod-backups");
        assert_eq!(d.url, "gs://pgpod-backups");
        assert!(d.credentials.is_none());
        assert!(d.options.is_empty());
    }

    #[test]
    fn tls_verification_defaults_to_on_through_both_paths() {
        // Two independent routes to the same struct. A derived `Default`
        // would give `false` while the serde path gave `true` — the same
        // trap InitdbBootstrap hit, and worse here, because the wrong
        // value silently stops checking certificates.
        let from_default = Destination::default();
        let from_serde: Destination = serde_yaml::from_str("url: gs://b").unwrap();
        assert!(from_default.verify_tls);
        assert!(from_serde.verify_tls);
    }

    #[test]
    fn tls_verification_is_never_inferred_from_an_endpoint() {
        // Disabling certificate checks should be something an operator
        // wrote down, not something an endpoint's shape implied.
        let d: Destination =
            serde_yaml::from_str("url: s3://b\nendpoint: garage.internal:3900").unwrap();
        assert!(d.verify_tls);
    }

    #[test]
    fn sanitizing_drops_the_secret_name_and_nothing_else() {
        let d = Destination {
            url: "s3://mirror/pgpod".into(),
            endpoint: Some("garage.internal:3900".into()),
            region: Some("garage".into()),
            verify_tls: false,
            credentials: Some("mydb-s3".into()),
            options: BTreeMap::new(),
        };
        let s = d.sanitized();
        assert_eq!(s.credentials, None);
        assert_eq!(s.url, d.url);
        assert_eq!(s.endpoint, d.endpoint);
        assert_eq!(s.region, d.region);
        assert_eq!(s.verify_tls, d.verify_tls);
    }

    #[test]
    fn credentials_are_addressed_by_index_not_by_name() {
        // The spec travels in an environment variable; a secret *name*
        // there would be one step from a secret value there.
        assert_eq!(
            Destination::credentials_path(0),
            "/run/secrets/pgpod-store-0"
        );
        assert_ne!(
            Destination::credentials_path(0),
            Destination::credentials_path(1)
        );
    }

    #[test]
    fn retention_defaults_to_reporting_rather_than_deleting() {
        let s: BackupSpec = serde_yaml::from_str("destinations: []").unwrap();
        assert_eq!(s.retention_mode, RetentionMode::Report);
        assert!(!s.is_enabled());
    }

    #[test]
    fn a_destination_never_serializes_a_credential() {
        let d = Destination {
            url: "s3://b/p".into(),
            endpoint: Some("garage.internal:3900".into()),
            region: Some("garage".into()),
            verify_tls: false,
            credentials: None,
            options: BTreeMap::new(),
        };
        let json = serde_json::to_string(&d).unwrap().to_lowercase();
        for forbidden in ["password", "secret", "access_key", "credential"] {
            assert!(!json.contains(forbidden), "{json}");
        }
    }

    fn job() -> BackupJobSpec {
        BackupJobSpec {
            cluster: "mydb".into(),
            instance: "mydb-1".into(),
            backup_id: "20260904T020000Z".into(),
            source_host: "pgpod-mydb-1".into(),
            source_port: 5432,
            destinations: vec![Destination::new("file:///archive/demo")],
        }
    }

    #[test]
    fn a_backup_job_round_trips_through_the_environment() {
        let j = job();
        let back: BackupJobSpec = serde_json::from_str(&j.to_env_value().unwrap()).unwrap();
        assert_eq!(back, j);
    }

    #[test]
    fn a_backup_job_with_no_destination_is_refused() {
        // It would take the whole backup and upload it nowhere.
        let mut j = job();
        j.destinations.clear();
        assert!(j.to_env_value().is_err());
    }

    #[test]
    fn a_backup_job_never_carries_a_podman_secret_name() {
        let mut j = job();
        j.destinations[0].credentials = Some("mydb-s3".into());
        assert!(j.to_env_value().is_err());
    }
}
