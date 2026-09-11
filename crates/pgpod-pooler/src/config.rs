//! Rendering `pg_doorman.yaml`.
//!
//! Pure, like `pgpod_backup::config`: a spec plus the secret material goes
//! in, a config file comes out, and the whole thing is unit-testable with
//! no container in sight.
//!
//! The rendering goes through `serde_yaml` rather than a format string.
//! That is not fastidiousness — the file carries the `auth_query` and
//! admin passwords in plaintext, because pg_doorman accepts no other form,
//! and a generated password containing `:`, `#`, a leading `*` or a
//! newline would either break the parse or silently change value. Letting
//! the YAML serializer decide the quoting is the only version of this that
//! is correct for every password rather than for the ones we happened to
//! try.

use std::collections::BTreeMap;

use pgpod_core::{PoolerSpec, container};
use serde::Serialize;

use crate::Error;

/// `[general]` keys pgpod owns.
///
/// The same posture as `pgpod_pg::RESERVED_PARAMETERS`: an operator may
/// tune pg_doorman through `spec.pgDoorman.parameters`, but not out from
/// under the things pgpod depends on. `query_wait_timeout` is here because
/// it *is* `maxHold` — setting it twice, in two places, with two meanings
/// is how a switchover budget silently stops matching the manifest.
pub const RESERVED_PARAMETERS: &[&str] = &[
    "host",
    "port",
    "admin_username",
    "admin_password",
    "query_wait_timeout",
    "daemon_pid_file",
    "pools",
];

/// The secret material the agent reads from `/run/secrets` and the daemon
/// never puts in the environment.
#[derive(Debug, Clone)]
pub struct Secrets {
    /// Password for pg_doorman's own admin console.
    pub admin_password: String,
    /// The `auth_query` lookup role's password, per pool, keyed by the
    /// pool name — a pooler fronting several clusters holds one per
    /// cluster and shares none between them.
    pub lookup_passwords: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct DoormanConfig {
    general: General,
    pools: BTreeMap<String, Pool>,
}

#[derive(Debug, Serialize)]
struct General {
    host: String,
    port: u16,
    admin_username: String,
    admin_password: String,
    query_wait_timeout: String,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Serialize)]
struct Pool {
    server_host: String,
    server_port: u16,
    server_database: String,
    pool_mode: String,
    auth_query: AuthQuery,
}

#[derive(Debug, Serialize)]
struct AuthQuery {
    query: String,
    user: String,
    password: String,
    database: String,
    pool_size: u32,
    cache_ttl: String,
}

/// The `SECURITY DEFINER` lookup, as pg_doorman will call it.
///
/// Parameterised on the role name so the SQL that creates the function and
/// the config that calls it cannot drift apart.
pub fn lookup_query(role: &str) -> String {
    format!("SELECT passwd FROM {role}_lookup($1)")
}

/// Render the config pg_doorman reads.
pub fn render(spec: &PoolerSpec, secrets: &Secrets) -> Result<String, Error> {
    spec.validate().map_err(|e| Error::Spec(e.to_string()))?;

    let mut extra = BTreeMap::new();
    for (key, value) in &spec.parameters {
        let key = key.trim().to_ascii_lowercase();
        if RESERVED_PARAMETERS.contains(&key.as_str()) {
            return Err(Error::ReservedParameter(key));
        }
        // Manifest values are strings, as postgres parameters are, but
        // pg_doorman wants `worker_threads: 4` as a number and
        // `tcp_no_delay: true` as a bool. Parsing each through YAML gives
        // the value its natural type and leaves anything else a string.
        let parsed: serde_yaml::Value = serde_yaml::from_str(value)
            .unwrap_or_else(|_| serde_yaml::Value::String(value.clone()));
        extra.insert(key, parsed);
    }

    let mut pools = BTreeMap::new();
    for target in &spec.pools {
        let password = secrets
            .lookup_passwords
            .get(&target.name)
            .ok_or_else(|| Error::MissingSecret(target.name.clone()))?;
        pools.insert(
            target.name.clone(),
            Pool {
                server_host: target.server_host.clone(),
                server_port: target.server_port,
                server_database: target.database.clone(),
                pool_mode: spec.pool_mode.as_str().to_string(),
                auth_query: AuthQuery {
                    query: lookup_query(&target.lookup_role),
                    user: target.lookup_role.clone(),
                    password: password.clone(),
                    database: target.lookup_database.clone(),
                    pool_size: spec.pool_size,
                    // Explicit and unit-bearing. pg_doorman reads a bare
                    // integer here as milliseconds, so an unqualified
                    // `3600` would cache credentials for 3.6 seconds and
                    // hammer the lookup role.
                    cache_ttl: "1h".to_string(),
                },
            },
        );
    }

    let config = DoormanConfig {
        general: General {
            // The container's own namespace; the host only ever reaches
            // this through the published port.
            host: "0.0.0.0".to_string(),
            port: spec.port,
            admin_username: container::POOLER_ADMIN_USER.to_string(),
            admin_password: secrets.admin_password.clone(),
            query_wait_timeout: spec.max_hold.render(),
            extra,
        },
        pools,
    };

    serde_yaml::to_string(&config).map_err(|e| Error::Render(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgpod_core::{HumanDuration, PoolMode, PoolTarget, PoolerId};

    fn target(name: &str, cluster: &str, index: usize) -> PoolTarget {
        PoolTarget {
            name: name.to_string(),
            cluster: cluster.to_string(),
            server_host: format!("pgpod-{cluster}-1"),
            server_port: 5432,
            database: "appdb".to_string(),
            lookup_role: "pgpod_pooler".to_string(),
            lookup_database: "postgres".to_string(),
            lookup_secret_index: index,
        }
    }

    fn spec() -> PoolerSpec {
        PoolerSpec {
            pooler: PoolerId::new("app").unwrap(),
            port: 6432,
            pools: vec![target("appdb", "mydb", 0)],
            pool_mode: PoolMode::Transaction,
            pool_size: 40,
            max_hold: HumanDuration::from_secs(60),
            parameters: Vec::new(),
        }
    }

    fn secrets() -> Secrets {
        Secrets {
            admin_password: "adminpw".to_string(),
            lookup_passwords: [("appdb".to_string(), "lookuppw".to_string())]
                .into_iter()
                .collect(),
        }
    }

    /// Re-read the rendered config the way pg_doorman would.
    fn reparse(out: &str) -> serde_yaml::Value {
        serde_yaml::from_str(out).expect("rendered config must be valid YAML")
    }

    #[test]
    fn renders_a_pool_pointing_at_the_instance_container() {
        let out = render(&spec(), &secrets()).unwrap();
        let v = reparse(&out);
        let pool = &v["pools"]["appdb"];
        assert_eq!(pool["server_host"].as_str(), Some("pgpod-mydb-1"));
        assert_eq!(pool["server_port"].as_u64(), Some(5432));
        assert_eq!(pool["server_database"].as_str(), Some("appdb"));
        assert_eq!(pool["pool_mode"].as_str(), Some("transaction"));
    }

    #[test]
    fn max_hold_becomes_query_wait_timeout_with_its_unit() {
        // The one knob. Asserted as a duration rather than as a spelling:
        // 60s canonicalises to "1m", and pinning the spelling would make
        // this fail on a change that means exactly the same thing. What
        // must hold is the value, and that it carries a unit at all —
        // pg_doorman reads a bare number as milliseconds.
        let out = render(&spec(), &secrets()).unwrap();
        let v = reparse(&out);
        let rendered = v["general"]["query_wait_timeout"]
            .as_str()
            .expect("query_wait_timeout must be a string, not a bare number");
        assert_eq!(
            rendered.parse::<HumanDuration>().expect(rendered).as_secs(),
            60
        );
    }

    #[test]
    fn every_duration_in_the_rendered_config_carries_a_unit() {
        // A guard for any duration added later with the same trap: a
        // number here does not fail, it means milliseconds.
        let out = render(&spec(), &secrets()).unwrap();
        for line in out.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim();
            if !(key.ends_with("_timeout") || key.ends_with("_ttl") || key.ends_with("lifetime")) {
                continue;
            }
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            assert!(
                value.ends_with(|c: char| c.is_ascii_alphabetic()),
                "{key} is rendered as {value:?} with no unit — pg_doorman \
                 would read it as milliseconds"
            );
        }
    }

    #[test]
    fn a_password_with_yaml_metacharacters_survives_a_round_trip() {
        // The reason this renders through serde_yaml rather than a format
        // string. `#` starts a comment, a leading `*` is an alias, `: `
        // splits a mapping, and a newline ends the scalar — each would
        // either break the parse or silently change the password, and the
        // symptom is the pooler failing to authenticate with no clue why.
        for pw in [
            "a#b",
            "*star",
            "key: value",
            "line\nbreak",
            "&anchor",
            "  padded  ",
            "'quoted'",
            "\"dquoted\"",
            "@at",
            "%percent",
            "null",
            "123",
        ] {
            let mut s = secrets();
            s.admin_password = pw.to_string();
            s.lookup_passwords
                .insert("appdb".to_string(), pw.to_string());
            let out = render(&spec(), &s).unwrap();
            let v = reparse(&out);
            assert_eq!(
                v["general"]["admin_password"].as_str(),
                Some(pw),
                "admin password {pw:?} did not survive rendering:\n{out}"
            );
            assert_eq!(
                v["pools"]["appdb"]["auth_query"]["password"].as_str(),
                Some(pw),
                "lookup password {pw:?} did not survive rendering:\n{out}"
            );
        }
    }

    #[test]
    fn the_lookup_query_names_the_role_it_is_granted_to() {
        // The SQL that creates the function and the config that calls it
        // are generated from the same role name, so they cannot drift.
        let out = render(&spec(), &secrets()).unwrap();
        let v = reparse(&out);
        let aq = &v["pools"]["appdb"]["auth_query"];
        assert_eq!(aq["user"].as_str(), Some("pgpod_pooler"));
        assert_eq!(
            aq["query"].as_str(),
            Some("SELECT passwd FROM pgpod_pooler_lookup($1)")
        );
        assert_eq!(
            aq["database"].as_str(),
            Some("postgres"),
            "the lookup runs in one database so the function exists in one place"
        );
    }

    #[test]
    fn several_clusters_get_separate_pools_and_separate_credentials() {
        // Co-tenancy must not share a credential: each cluster has its own
        // lookup role password (ADR 05 §2).
        let mut s = spec();
        s.pools.push(target("shop", "other", 1));
        let mut sec = secrets();
        sec.lookup_passwords
            .insert("shop".to_string(), "otherpw".to_string());

        let v = reparse(&render(&s, &sec).unwrap());
        assert_eq!(
            v["pools"]["appdb"]["server_host"].as_str(),
            Some("pgpod-mydb-1")
        );
        assert_eq!(
            v["pools"]["shop"]["server_host"].as_str(),
            Some("pgpod-other-1")
        );
        assert_ne!(
            v["pools"]["appdb"]["auth_query"]["password"],
            v["pools"]["shop"]["auth_query"]["password"],
            "two clusters must not share a lookup credential"
        );
    }

    #[test]
    fn operator_parameters_land_in_general_with_their_natural_type() {
        // pg_doorman wants `worker_threads: 4` as a number and
        // `tcp_no_delay: true` as a bool; the manifest carries strings.
        let mut s = spec();
        s.parameters = vec![
            ("worker_threads".into(), "4".into()),
            ("tcp_no_delay".into(), "true".into()),
            ("pooler_check_query".into(), ";".into()),
        ];
        let v = reparse(&render(&s, &secrets()).unwrap());
        assert_eq!(v["general"]["worker_threads"].as_u64(), Some(4));
        assert_eq!(v["general"]["tcp_no_delay"].as_bool(), Some(true));
        assert_eq!(v["general"]["pooler_check_query"].as_str(), Some(";"));
    }

    #[test]
    fn a_reserved_parameter_is_refused() {
        // Especially `query_wait_timeout`: set here as well as through
        // maxHold, the switchover budget would stop matching the manifest
        // and nothing would say so.
        for key in RESERVED_PARAMETERS {
            let mut s = spec();
            s.parameters = vec![(key.to_string(), "1".into())];
            let err = render(&s, &secrets()).unwrap_err();
            assert!(
                matches!(err, Error::ReservedParameter(_)),
                "{key} should be refused, got {err}"
            );
        }
    }

    #[test]
    fn a_reserved_parameter_is_refused_however_it_is_spelled() {
        let mut s = spec();
        s.parameters = vec![("  Query_Wait_Timeout ".into(), "1s".into())];
        assert!(matches!(
            render(&s, &secrets()).unwrap_err(),
            Error::ReservedParameter(_)
        ));
    }

    #[test]
    fn a_pool_with_no_lookup_password_is_refused_rather_than_rendered_empty() {
        // An empty password renders as valid YAML and fails at the first
        // client connection, three layers from the cause.
        let mut s = spec();
        s.pools.push(target("shop", "other", 1));
        let err = render(&s, &secrets()).unwrap_err();
        assert!(matches!(err, Error::MissingSecret(_)), "{err}");
    }

    #[test]
    fn rendering_is_deterministic() {
        // The daemon compares rendered output to decide whether a reload
        // is needed; map iteration order must not make identical input
        // look changed.
        assert_eq!(
            render(&spec(), &secrets()).unwrap(),
            render(&spec(), &secrets()).unwrap()
        );
    }

    #[test]
    fn an_invalid_spec_is_refused_before_anything_is_rendered() {
        let mut s = spec();
        s.pools.clear();
        assert!(matches!(
            render(&s, &secrets()).unwrap_err(),
            Error::Spec(_)
        ));
    }
}
