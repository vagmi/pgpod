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
//! Env is safe here because nothing in this struct is secret. Passwords
//! never appear — they arrive as podman secrets mounted under
//! `/run/secrets/`, precisely because environment variables *are* visible
//! in `podman inspect` and `/proc/<pid>/environ` (ADR 00 §9).

use serde::{Deserialize, Serialize};

use crate::InstanceId;

/// Environment variable carrying the JSON-encoded [`InstanceSpec`].
pub const SPEC_ENV: &str = "PGPOD_INSTANCE_SPEC";

/// How an instance populates its PGDATA on first start.
///
/// Mirrors CNPG's bootstrap modes so the mental model transfers. Only
/// `Initdb` exists in Phase 1; the others are named here so the agent's
/// match is exhaustive from the start and adding them is a compile error
/// away from being handled, not silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Bootstrap {
    /// A fresh cluster.
    Initdb(InitdbBootstrap),
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
}

impl InstanceSpec {
    /// Read and parse the spec from the environment.
    pub fn from_env() -> Result<Self, SpecError> {
        let raw = std::env::var(SPEC_ENV).map_err(|_| SpecError::Missing)?;
        serde_json::from_str(&raw).map_err(|e| SpecError::Malformed(e.to_string()))
    }

    pub fn to_env_value(&self) -> Result<String, SpecError> {
        serde_json::to_string(self).map_err(|e| SpecError::Malformed(e.to_string()))
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
