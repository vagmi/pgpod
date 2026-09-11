//! The pooler spec — what the daemon tells the pooler's agent to be.
//!
//! Delivered as JSON in `PGPOD_POOLER_SPEC`, for the same reasons
//! [`crate::InstanceSpec`] travels that way, and with the same
//! consequence: it is fixed when the **container** is created, so a
//! changed spec needs the container recreated rather than restarted.
//!
//! For a pooler that consequence has teeth. Recreating the pooler drops
//! every pooled connection — it is the one operation the pooler cannot
//! hold for — so anything that lands here is something an operator pays an
//! outage to change. That is why `maxHold` is here (pg_doorman captures
//! `query_wait_timeout` at pool construction and never re-reads it) while
//! the backend a pool points at is **not**: `server_host` is a DNS name
//! re-resolved per connect, so an instance recreated under the same name
//! is picked up with no config change at all.
//!
//! Nothing here is secret. The `auth_query` lookup password and the admin
//! console password are plaintext in the rendered config — pg_doorman
//! accepts no other form — but they reach the container as mounted podman
//! secrets and are read by the agent, never passed through the
//! environment that `podman inspect` prints (ADR 00 §9).

use serde::{Deserialize, Serialize};

use crate::{HumanDuration, PoolMode, PoolerId};

/// Environment variable carrying the JSON-encoded [`PoolerSpec`].
pub const POOLER_SPEC_ENV: &str = "PGPOD_POOLER_SPEC";

/// Everything the agent needs to bring one pooler up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PoolerSpec {
    pub pooler: PoolerId,

    /// Port pg_doorman listens on inside the container.
    #[serde(default = "default_port")]
    pub port: u16,

    /// One pool per entry. The pool's key is what clients put in `dbname`.
    pub pools: Vec<PoolTarget>,

    #[serde(default)]
    pub pool_mode: PoolMode,

    #[serde(default = "default_pool_size")]
    pub pool_size: u32,

    /// Rendered as `general.query_wait_timeout`, which is both the
    /// ordinary pool-pressure wait and the longest hold a switchover can
    /// take (ADR 05 §3).
    #[serde(default = "default_max_hold")]
    pub max_hold: HumanDuration,

    /// From `spec.pgDoorman.parameters`, already checked against
    /// `pgpod_pooler::RESERVED_PARAMETERS` by the daemon.
    #[serde(default)]
    pub parameters: Vec<(String, String)>,
}

/// One pg_doorman pool: a name clients connect to, and the instance behind
/// it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PoolTarget {
    /// The pool key — what a client puts in `dbname`.
    pub name: String,

    /// Which cluster this pool belongs to.
    ///
    /// Carried so a hold can be scoped to one cluster's pools while a
    /// co-tenant cluster's clients keep running (ADR 05 §2).
    pub cluster: String,

    /// The instance container's name on the cluster network.
    ///
    /// A name, not an address, and that is load-bearing: pg_doorman
    /// resolves it per backend connect, so a container recreated under the
    /// same name is picked up at its new address with no config change.
    pub server_host: String,

    #[serde(default = "default_server_port")]
    pub server_port: u16,

    /// The actual database on that cluster, which may differ from [`Self::name`].
    pub database: String,

    /// Role the `auth_query` lookup connects as.
    pub lookup_role: String,

    /// Database the lookup query runs in — `postgres`, so the
    /// `SECURITY DEFINER` function exists in exactly one place.
    #[serde(default = "default_lookup_database")]
    pub lookup_database: String,

    /// Which mounted secret holds that role's password.
    ///
    /// An index rather than the podman secret's name, for the reason
    /// `Destination` credentials are indexed: the name is a host concept
    /// that resolves to nothing inside the container.
    pub lookup_secret_index: usize,
}

fn default_port() -> u16 {
    crate::container::POOLER_PORT
}

fn default_server_port() -> u16 {
    crate::container::PG_PORT
}

fn default_pool_size() -> u32 {
    40
}

fn default_max_hold() -> HumanDuration {
    HumanDuration::from_secs(60)
}

fn default_lookup_database() -> String {
    "postgres".to_string()
}

impl PoolerSpec {
    pub fn from_env() -> Result<Self, crate::SpecError> {
        let raw = std::env::var(POOLER_SPEC_ENV).map_err(|_| crate::SpecError::Missing)?;
        serde_json::from_str(&raw).map_err(|e| crate::SpecError::Malformed(e.to_string()))
    }

    pub fn to_env_value(&self) -> Result<String, crate::SpecError> {
        self.validate()?;
        serde_json::to_string(self).map_err(|e| crate::SpecError::Malformed(e.to_string()))
    }

    /// Pool names belonging to one cluster.
    ///
    /// What a hold pauses: scoping by cluster is what keeps recreating one
    /// cluster from stalling another's clients on a shared pooler.
    pub fn pools_for(&self, cluster: &str) -> Vec<&str> {
        self.pools
            .iter()
            .filter(|p| p.cluster == cluster)
            .map(|p| p.name.as_str())
            .collect()
    }

    pub fn clusters(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.pools.iter().map(|p| p.cluster.as_str()).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    pub fn validate(&self) -> Result<(), crate::SpecError> {
        if self.pools.is_empty() {
            return Err(crate::SpecError::PoolerInconsistent(
                "a pooler with no pools would accept connections and have \
                 nowhere to send them"
                    .into(),
            ));
        }
        if self.pool_size == 0 {
            return Err(crate::SpecError::PoolerInconsistent(
                "poolSize must be at least 1".into(),
            ));
        }
        // A zero budget makes every wait non-blocking in pg_doorman, so a
        // PAUSE would error clients instantly instead of holding them —
        // the feature switched off by a value that looks like "no limit".
        if self.max_hold.as_millis() == 0 {
            return Err(crate::SpecError::PoolerInconsistent(
                "maxHold of zero makes every wait non-blocking, so a hold \
                 would fail clients immediately rather than holding them"
                    .into(),
            ));
        }

        let mut seen = std::collections::BTreeSet::new();
        for p in &self.pools {
            if !seen.insert(p.name.as_str()) {
                return Err(crate::SpecError::PoolerInconsistent(format!(
                    "two pools are both named {:?}; clients address a pool by \
                     that name",
                    p.name
                )));
            }
            if p.server_host.trim().is_empty() {
                return Err(crate::SpecError::PoolerInconsistent(format!(
                    "pool {:?} names no server host",
                    p.name
                )));
            }
            if p.database.trim().is_empty() {
                return Err(crate::SpecError::PoolerInconsistent(format!(
                    "pool {:?} names no database",
                    p.name
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            parameters: vec![("worker_threads".into(), "4".into())],
        }
    }

    #[test]
    fn round_trips_through_the_environment_encoding() {
        let s = spec();
        let decoded: PoolerSpec = serde_json::from_str(&s.to_env_value().unwrap()).unwrap();
        assert_eq!(decoded, s);
    }

    #[test]
    fn no_field_can_carry_a_password() {
        // The lookup and admin passwords are plaintext in the rendered
        // config because pg_doorman accepts no other form. They must still
        // never take this route: an environment variable is visible in
        // `podman inspect` and /proc/<pid>/environ (ADR 00 §9).
        let encoded = spec().to_env_value().unwrap().to_lowercase();
        for forbidden in ["password", "passwd", "secret=", "credential"] {
            assert!(
                !encoded.contains(forbidden),
                "spec encoding mentions {forbidden:?}: {encoded}"
            );
        }
    }

    #[test]
    fn a_pooler_with_no_pools_is_refused() {
        let mut s = spec();
        s.pools.clear();
        assert!(matches!(
            s.to_env_value().unwrap_err(),
            crate::SpecError::PoolerInconsistent(_)
        ));
    }

    #[test]
    fn a_zero_hold_budget_is_refused() {
        // pg_doorman treats a zero wait as non-blocking, so this reads as
        // "no limit" and means "never hold" — the feature switched off by
        // the value that looks most generous.
        let mut s = spec();
        s.max_hold = HumanDuration::from_millis(0);
        let err = s.to_env_value().unwrap_err();
        assert!(err.to_string().contains("non-blocking"), "{err}");
    }

    #[test]
    fn duplicate_pool_names_are_refused() {
        let mut s = spec();
        s.pools.push(target("appdb", "other", 1));
        assert!(s.to_env_value().is_err());
    }

    #[test]
    fn pools_are_addressable_per_cluster_so_a_hold_can_be_scoped() {
        // The property that keeps recreating one cluster from stalling a
        // co-tenant's clients on a shared pooler.
        let mut s = spec();
        s.pools.push(target("shop", "other", 1));
        assert_eq!(s.pools_for("mydb"), vec!["appdb"]);
        assert_eq!(s.pools_for("other"), vec!["shop"]);
        assert_eq!(s.clusters(), vec!["mydb", "other"]);
        assert!(s.pools_for("absent").is_empty());
    }

    #[test]
    fn an_older_spec_still_parses() {
        // A daemon mid-upgrade may send a spec written by an older
        // version; missing optional fields must not fail the pooler.
        let minimal = r#"{
            "pooler": "app",
            "pools": [{
                "name": "appdb", "cluster": "mydb",
                "serverHost": "pgpod-mydb-1", "database": "appdb",
                "lookupRole": "pgpod_pooler", "lookupSecretIndex": 0
            }]
        }"#;
        let s: PoolerSpec = serde_json::from_str(minimal).unwrap();
        assert_eq!(s.port, 6432);
        assert_eq!(s.pools[0].server_port, 5432);
        assert_eq!(s.pools[0].lookup_database, "postgres");
        assert_eq!(s.max_hold.as_secs(), 60);
    }
}
