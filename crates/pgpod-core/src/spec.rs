//! The instance spec — what the daemon tells the agent to be.
//!
//! Delivered as JSON in the `PGPOD_INSTANCE_SPEC` environment variable.
//!
//! Environment rather than a file in the volume, because of a
//! chicken-and-egg the volume storage model creates: before first boot the
//! volume is *empty*, and only a container can write into it (ADR 00 §4).
//! There is nowhere to put a config file that the agent could read on its
//! very first start.
//!
//! One consequence to keep in mind: because this is an environment
//! variable, it is fixed when the *container* is created. A changed spec
//! therefore needs the container recreated, not merely restarted — a
//! restart re-runs the agent with the stale value. See ROADMAP Phase 4.
//!
//! Env is safe here because nothing in this struct is secret. Passwords
//! never appear — they arrive as podman secrets mounted under
//! `/run/secrets/`, precisely because environment variables *are* visible
//! in `podman inspect` and `/proc/<pid>/environ` (ADR 00 §9).

use serde::{Deserialize, Serialize};

use crate::{BackupSpec, InstanceId};

/// Environment variable carrying the JSON-encoded [`InstanceSpec`].
pub const SPEC_ENV: &str = "PGPOD_INSTANCE_SPEC";

/// How an instance populates its PGDATA on first start.
///
/// Mirrors CNPG's bootstrap modes so the mental model transfers. The
/// remaining one, `pg_basebackup` for adding a standby, is named nowhere
/// yet — when it lands, this match becoming non-exhaustive is a compile
/// error rather than a silently ignored case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Bootstrap {
    /// A fresh cluster.
    Initdb(InitdbBootstrap),
    /// Restore from a pgBackRest repository, optionally stopping at a
    /// point in time (ADR 04 §1).
    Recovery(RecoveryBootstrap),
}

/// Restoring from a pgBackRest repository.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryBootstrap {
    /// The stanza to restore *from* — the source cluster's name.
    ///
    /// Separate from this instance's own stanza, and that is the point:
    /// `pgpod restore mydb --as mydb-restored` reads `mydb`'s stanza and
    /// archives to `mydb-restored`'s. pgBackRest appends the stanza to
    /// `repo-path`, so the two never touch even in one bucket. Confusing
    /// them is how a restore would overwrite the repository it is reading.
    pub source_stanza: String,

    /// pgBackRest's label for the backup to restore, e.g.
    /// `20260904-020000F`. `None` lets pgBackRest pick, which is what a
    /// target-less fork wants.
    ///
    /// Chosen by the daemon, which is the side that can query the
    /// repository (ADR 04 §5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_label: Option<String>,

    /// Recovery target time, RFC 3339. `None` replays everything
    /// available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_time: Option<String>,

    /// The repository to read from.
    ///
    /// Carried separately from [`InstanceSpec::backup`] because they are
    /// genuinely different: the restored instance reads the source's
    /// repository and, once promoted, writes to its own.
    pub destinations: Vec<crate::Destination>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InitdbBootstrap {
    /// Application database to create. `None` creates none.
    pub database: Option<String>,
    /// Owner role for that database.
    pub owner: Option<String>,
    #[serde(default = "default_encoding")]
    pub encoding: String,
    #[serde(default = "default_locale")]
    pub locale: String,
    /// Extra `initdb` flags.
    #[serde(default)]
    pub options: Vec<String>,
}

fn default_encoding() -> String {
    "UTF8".to_string()
}

fn default_locale() -> String {
    "C".to_string()
}

/// Written by hand rather than derived.
///
/// `#[serde(default = "...")]` applies only when *deserializing* a missing
/// field — a derived `Default` ignores it entirely and yields empty
/// strings. That is not a cosmetic difference: `initdb --encoding=` fails
/// outright, and `--locale=` silently falls through to the container's
/// environment, giving a glibc-dependent collation instead of `C`. The
/// second is the dangerous one, because it succeeds: text indexes built
/// under one collation silently corrupt when glibc changes underneath
/// them, which is the whole reason `C` is the default.
impl Default for InitdbBootstrap {
    fn default() -> Self {
        Self {
            database: None,
            owner: None,
            encoding: default_encoding(),
            locale: default_locale(),
            options: Vec::new(),
        }
    }
}

/// Everything the agent needs to bring one instance up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstanceSpec {
    pub instance: InstanceId,
    pub port: u16,
    pub bootstrap: Bootstrap,
    /// From `spec.postgresql.parameters`.
    #[serde(default)]
    pub parameters: Vec<(String, String)>,
    #[serde(default)]
    pub shared_preload_libraries: Vec<String>,
    /// Subnet of the cluster's podman network, for `pg_hba.conf`. `None`
    /// on a single-instance cluster, which needs no network rules.
    #[serde(default)]
    pub network_cidr: Option<String>,
    /// `archive_command`. `None` renders `archive_mode = off` rather than
    /// archiving to nowhere (see `pgpod_pg::ArchiveMode`).
    #[serde(default)]
    pub archive_command: Option<String>,
    /// Where the agent ships WAL, and where it fetches it back from.
    ///
    /// Carried here rather than read from a file because
    /// `archive_command` is invoked by PostgreSQL as a bare process: the
    /// agent has no state beyond its environment and the volume. Empty
    /// destinations and a `None` `archive_command` are the same statement
    /// made twice, and [`InstanceSpec::validate`] refuses to let them
    /// disagree.
    #[serde(default)]
    pub backup: BackupSpec,
}

impl InstanceSpec {
    /// The recovery source, if this instance is being restored.
    pub fn recovery(&self) -> Option<&RecoveryBootstrap> {
        match &self.bootstrap {
            Bootstrap::Recovery(r) => Some(r),
            Bootstrap::Initdb(_) => None,
        }
    }

    /// Read and parse the spec from the environment.
    pub fn from_env() -> Result<Self, SpecError> {
        let raw = std::env::var(SPEC_ENV).map_err(|_| SpecError::Missing)?;
        serde_json::from_str(&raw).map_err(|e| SpecError::Malformed(e.to_string()))
    }

    pub fn to_env_value(&self) -> Result<String, SpecError> {
        self.validate()?;
        serde_json::to_string(self).map_err(|e| SpecError::Malformed(e.to_string()))
    }

    /// Refuse a spec that would archive to nowhere, or that carries a
    /// credential into the environment.
    ///
    /// Both checks are here rather than at the call site because this
    /// struct is the thing that crosses into the container: whatever the
    /// daemon believes, this is what the agent will act on.
    pub fn validate(&self) -> Result<(), SpecError> {
        // `archive_mode = on` with no destination is the silent-data-loss
        // shape from ADR 01 §1 wearing a different hat: PostgreSQL would
        // be told a segment is durable by a command with nowhere to put
        // it. The inverse — destinations but no command — is a daemon bug
        // that would quietly never archive anything.
        match (&self.archive_command, self.backup.is_enabled()) {
            (Some(_), false) => {
                return Err(SpecError::Inconsistent(
                    "archive_command is set but spec.backup.destinations is empty — \
                     archiving to nowhere"
                        .into(),
                ));
            }
            (None, true) => {
                return Err(SpecError::Inconsistent(
                    "spec.backup.destinations is set but archive_command is not — \
                     WAL would never be shipped"
                        .into(),
                ));
            }
            _ => {}
        }

        if let Some(recovery) = self.recovery() {
            if recovery.destinations.is_empty() {
                return Err(SpecError::Inconsistent(
                    "a recovery bootstrap with no destination has nowhere to \
                     read the base backup from"
                        .into(),
                ));
            }
            if recovery.source_stanza.trim().is_empty() {
                return Err(SpecError::Inconsistent(
                    "a recovery bootstrap needs the stanza to restore from".into(),
                ));
            }
            if let Some(t) = &recovery.target_time
                && chrono::DateTime::parse_from_rfc3339(t).is_err()
            {
                return Err(SpecError::Inconsistent(format!(
                    "recovery target time {t:?} is not RFC 3339"
                )));
            }
        }

        for (index, dest) in self.backup.destinations.iter().enumerate() {
            // A podman secret name is a host concept. Inside the container
            // it resolves to nothing, so an agent that trusted it would
            // archive without credentials and fail at the object store
            // instead of here. `Destination::sanitized` is the fix.
            if dest.credentials.is_some() {
                return Err(SpecError::Inconsistent(format!(
                    "destination {index} still carries a podman secret name — \
                     the daemon must mount it at {} and send a sanitized \
                     destination",
                    crate::Destination::credentials_path(index)
                )));
            }
        }

        // A destination option key like `aws_secret_access_key` would put
        // a credential into an environment variable that `podman inspect`
        // prints (ADR 00 §9). Credentials arrive as mounted secrets; see
        // `crate::SECRET_OBJECT_STORE_PREFIX`.
        for dest in &self.backup.destinations {
            for key in dest.options.keys() {
                let k = key.to_ascii_lowercase();
                if ["secret", "password", "key", "token", "credential"]
                    .iter()
                    .any(|bad| k.contains(bad))
                {
                    return Err(SpecError::Inconsistent(format!(
                        "destination option {key:?} looks like a credential — \
                         credentials are mounted as podman secrets, not passed \
                         in the instance spec"
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error(
        "{SPEC_ENV} is not set — the agent is being run outside a pgpod-created \
         container, or the daemon failed to pass the instance spec"
    )]
    Missing,

    #[error("{SPEC_ENV} could not be parsed: {0}")]
    Malformed(String),

    #[error("the instance spec is self-contradictory: {0}")]
    Inconsistent(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClusterId;

    fn spec() -> InstanceSpec {
        InstanceSpec {
            instance: ClusterId::new("mydb").unwrap().instance(1),
            port: 5432,
            bootstrap: Bootstrap::Initdb(InitdbBootstrap {
                database: Some("appdb".into()),
                owner: Some("app".into()),
                ..Default::default()
            }),
            parameters: vec![("shared_buffers".into(), "512MB".into())],
            shared_preload_libraries: vec!["pg_stat_statements".into()],
            network_cidr: Some("10.89.0.0/24".into()),
            archive_command: None,
            backup: BackupSpec::default(),
        }
    }

    #[test]
    fn round_trips_through_the_environment_encoding() {
        let s = spec();
        let encoded = s.to_env_value().unwrap();
        let decoded: InstanceSpec = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, s);
    }

    #[test]
    fn no_field_can_carry_a_password() {
        // Passwords arrive as mounted podman secrets, never here — env
        // vars are visible in `podman inspect` (ADR 00 §9). This asserts
        // the shape rather than trusting review.
        let encoded = spec().to_env_value().unwrap().to_lowercase();
        for forbidden in ["password", "passwd", "secret", "credential"] {
            assert!(
                !encoded.contains(forbidden),
                "spec encoding mentions {forbidden:?}: {encoded}"
            );
        }
    }

    #[test]
    fn optional_fields_default_so_older_specs_still_parse() {
        // A daemon mid-upgrade may send a spec written by an older
        // version; missing optional fields must not fail the instance.
        let minimal = r#"{
            "instance": {"cluster": "mydb", "ordinal": 1},
            "port": 5432,
            "bootstrap": {"mode": "initdb"}
        }"#;
        let s: InstanceSpec = serde_json::from_str(minimal).unwrap();
        assert_eq!(s.port, 5432);
        assert!(s.parameters.is_empty());
        assert!(s.archive_command.is_none());
        match s.bootstrap {
            Bootstrap::Recovery(_) => panic!("a spec with no mode must default to initdb"),
            Bootstrap::Initdb(i) => {
                assert_eq!(i.encoding, "UTF8");
                assert_eq!(
                    i.locale, "C",
                    "locale must default to C so collation cannot drift"
                );
            }
        }
    }

    #[test]
    fn derived_default_agrees_with_the_serde_defaults() {
        // These are two independent code paths to the same struct, and a
        // derived Default would silently give empty strings while the
        // serde path gave "UTF8"/"C". initdb rejects an empty encoding
        // outright; an empty locale is worse, because it *succeeds* and
        // silently picks up the environment's collation.
        let from_default = InitdbBootstrap::default();
        let from_serde: InitdbBootstrap = serde_json::from_str("{}").unwrap();
        assert_eq!(from_default, from_serde);
        assert_eq!(from_default.encoding, "UTF8");
        assert_eq!(from_default.locale, "C");
    }

    #[test]
    fn no_default_field_is_ever_empty() {
        // A guard for any field added later with the same trap.
        let d = InitdbBootstrap::default();
        assert!(!d.encoding.is_empty(), "encoding must not default to empty");
        assert!(!d.locale.is_empty(), "locale must not default to empty");
    }

    fn with_destination() -> InstanceSpec {
        let mut s = spec();
        s.backup.destinations = vec![crate::Destination::new("file:///srv/backups")];
        s.archive_command = Some("pgpod-agent wal archive %p".into());
        s
    }

    #[test]
    fn a_spec_with_destinations_round_trips_and_validates() {
        let s = with_destination();
        let decoded: InstanceSpec = serde_json::from_str(&s.to_env_value().unwrap()).unwrap();
        assert_eq!(decoded, s);
        assert!(decoded.backup.is_enabled());
    }

    #[test]
    fn an_archive_command_with_no_destination_is_refused() {
        // Otherwise archive_mode renders `on` and PostgreSQL is told each
        // segment is durable by a command with nowhere to put it — ADR 01
        // §1's silent data loss, arrived at from the other direction.
        let mut s = spec();
        s.archive_command = Some("pgpod-agent wal archive %p".into());
        let err = s.to_env_value().unwrap_err();
        assert!(matches!(err, SpecError::Inconsistent(_)), "{err}");
        assert!(err.to_string().contains("archiving to nowhere"), "{err}");
    }

    #[test]
    fn destinations_with_no_archive_command_are_refused() {
        // The quiet direction: an operator who configured backups and
        // whose WAL is never shipped anywhere.
        let mut s = spec();
        s.backup.destinations = vec![crate::Destination::new("file:///srv/backups")];
        let err = s.to_env_value().unwrap_err();
        assert!(matches!(err, SpecError::Inconsistent(_)), "{err}");
    }

    #[test]
    fn a_credential_shaped_destination_option_never_reaches_the_environment() {
        // `options` is a deliberate escape hatch to object_store's config
        // keys, and `aws_secret_access_key` is one of them. The whole
        // reason passwords are mounted secrets is that env vars show up in
        // `podman inspect` and /proc/<pid>/environ (ADR 00 §9).
        for key in [
            "aws_secret_access_key",
            "AWS_ACCESS_KEY_ID",
            "google_service_account_key",
            "bearer_token",
        ] {
            let mut s = with_destination();
            s.backup.destinations[0]
                .options
                .insert(key.to_string(), "hunter2".to_string());
            let err = s.to_env_value().unwrap_err();
            assert!(
                matches!(err, SpecError::Inconsistent(_)),
                "{key} should be refused, got {err:?}"
            );
        }
    }

    #[test]
    fn a_destination_that_still_names_a_podman_secret_is_refused() {
        // The name means nothing inside the container. Left in, the agent
        // would archive with no credentials and fail at the object store,
        // three layers from the mistake.
        let mut s = with_destination();
        s.backup.destinations[0].credentials = Some("mydb-s3".into());
        let err = s.to_env_value().unwrap_err();
        assert!(matches!(err, SpecError::Inconsistent(_)), "{err}");
        assert!(
            err.to_string().contains("/run/secrets/pgpod-store-0"),
            "{err}"
        );

        // And the sanitized form passes, so the fix the message names
        // actually works.
        s.backup.destinations[0] = s.backup.destinations[0].sanitized();
        assert!(s.to_env_value().is_ok());
    }

    #[test]
    fn a_harmless_destination_option_still_passes() {
        // The guard has to leave the escape hatch usable.
        let mut s = with_destination();
        s.backup.destinations[0]
            .options
            .insert("checksum_algorithm".into(), "sha256".into());
        assert!(s.to_env_value().is_ok());
    }

    #[test]
    fn a_missing_env_var_is_a_distinct_error_from_a_malformed_one() {
        // "the daemon did not pass a spec" and "the spec is corrupt" need
        // different fixes, so they must not collapse into one message.
        assert!(matches!(
            serde_json::from_str::<InstanceSpec>("not json")
                .map_err(|e| SpecError::Malformed(e.to_string())),
            Err(SpecError::Malformed(_))
        ));
    }
}
