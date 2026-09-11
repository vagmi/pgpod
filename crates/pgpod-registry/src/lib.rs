//! SQLite state store for pgpod.
//!
//! Records pgpod's intent and what it last observed. It is deliberately
//! *not* a cache of podman's state: anything that must be current is asked
//! of podman directly, and the role column here is for display rather than
//! for decisions (ADR 02 §7).
//!
//! SQLite rather than a JSON file (paagan's choice) because the reconciler
//! and the CLI run concurrently, promote is a multi-step state machine that
//! has to survive a crash halfway through, and `events` wants an
//! append-only table. A JSON file loses the last write under concurrency —
//! precisely during a failover.

mod schema {
    refinery::embed_migrations!("migrations");
}

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use pgpod_core::{
    ClusterId, ClusterManifest, InstancePhase, InstanceRole, PoolerId, PoolerManifest,
};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("registry database: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("registry migration failed: {0}")]
    Migration(#[from] refinery::Error),

    #[error("could not create {path}: {source}")]
    CreateDir {
        path: String,
        source: std::io::Error,
    },

    #[error("cluster {0} is not known to pgpod")]
    UnknownCluster(String),

    #[error("pooler {0} is not known to pgpod")]
    UnknownPooler(String),

    #[error(
        "cluster {cluster} is still fronted by pooler {pooler}. Delete the \
         pooler first (`pgpod pooler delete {pooler}`), or it would be left \
         accepting connections for a cluster that no longer exists."
    )]
    PoolerInUse { cluster: String, pooler: String },

    #[error("stored data is corrupt: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// One cluster's persisted row.
#[derive(Debug, Clone)]
pub struct ClusterRecord {
    pub name: String,
    pub manifest: ClusterManifest,
    pub generation: i64,
    pub phase: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// One instance's persisted row.
#[derive(Debug, Clone)]
pub struct InstanceRecord {
    pub cluster: String,
    pub ordinal: u32,
    pub container_name: String,
    pub container_id: Option<String>,
    pub volume_name: String,
    pub host_port: u16,
    pub phase: InstancePhase,
    pub role: InstanceRole,
    pub timeline: Option<i64>,
    pub last_probe_at_ms: Option<i64>,
}

/// The `poolers` columns, as SQLite hands them back.
type PoolerRow = (String, String, i64, String, Option<String>, i64, String);

/// One pooler's persisted row.
#[derive(Debug, Clone)]
pub struct PoolerRecord {
    pub name: String,
    pub manifest: PoolerManifest,
    pub generation: i64,
    pub container_name: String,
    pub container_id: Option<String>,
    pub host_port: u16,
    pub phase: String,
    /// Which clusters it fronts, and under what pool names.
    pub pools: Vec<PoolerPool>,
}

/// One pool a pooler exports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolerPool {
    pub cluster: String,
    pub database: String,
    /// What clients put in `dbname`.
    pub pool_name: String,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub cluster: String,
    pub ordinal: Option<u32>,
    pub at_ms: i64,
    pub level: String,
    pub message: String,
}

pub struct Registry {
    conn: Connection,
}

impl Registry {
    /// Open (creating if absent) and migrate.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::CreateDir {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let mut conn = Connection::open(path)?;
        Self::prepare(&mut conn)?;
        Ok(Self { conn })
    }

    /// An in-memory registry, for tests.
    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        Self::prepare(&mut conn)?;
        Ok(Self { conn })
    }

    fn prepare(conn: &mut Connection) -> Result<()> {
        // Not on by default in SQLite, and the instances/backups tables
        // rely on ON DELETE CASCADE to avoid orphan rows.
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;",
        )?;
        schema::migrations::runner().run(conn)?;
        Ok(())
    }

    // ---- clusters ----------------------------------------------------

    /// Insert or update a cluster.
    ///
    /// `generation` is bumped only when the manifest actually changed, so
    /// a re-apply of an unchanged file is a no-op a reconciler can detect
    /// without diffing JSON itself.
    pub fn put_cluster(&self, manifest: &ClusterManifest, phase: &str) -> Result<i64> {
        let name = manifest.metadata.name.clone();
        let encoded = serde_json::to_string(manifest)
            .map_err(|e| Error::Corrupt(format!("could not encode manifest: {e}")))?;
        let now = now_ms();

        let existing: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT manifest, generation FROM clusters WHERE name = ?1",
                params![name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        let generation = match existing {
            None => {
                self.conn.execute(
                    "INSERT INTO clusters (name, manifest, generation, phase, created_at_ms, updated_at_ms)
                     VALUES (?1, ?2, 1, ?3, ?4, ?4)",
                    params![name, encoded, phase, now],
                )?;
                1
            }
            Some((stored, prev)) => {
                let generation = if stored == encoded { prev } else { prev + 1 };
                self.conn.execute(
                    "UPDATE clusters SET manifest = ?2, generation = ?3, phase = ?4, updated_at_ms = ?5
                     WHERE name = ?1",
                    params![name, encoded, generation, phase, now],
                )?;
                generation
            }
        };
        Ok(generation)
    }

    pub fn cluster(&self, cluster: &ClusterId) -> Result<Option<ClusterRecord>> {
        self.conn
            .query_row(
                "SELECT name, manifest, generation, phase, created_at_ms, updated_at_ms
                 FROM clusters WHERE name = ?1",
                params![cluster.as_str()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?
            .map(|(name, manifest, generation, phase, created, updated)| {
                Ok(ClusterRecord {
                    name,
                    manifest: serde_json::from_str(&manifest)
                        .map_err(|e| Error::Corrupt(format!("stored manifest: {e}")))?,
                    generation,
                    phase,
                    created_at_ms: created,
                    updated_at_ms: updated,
                })
            })
            .transpose()
    }

    pub fn require_cluster(&self, cluster: &ClusterId) -> Result<ClusterRecord> {
        self.cluster(cluster)?
            .ok_or_else(|| Error::UnknownCluster(cluster.to_string()))
    }

    pub fn list_clusters(&self) -> Result<Vec<ClusterRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM clusters ORDER BY name")?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        names
            .into_iter()
            .map(|n| {
                let id = ClusterId::new(n.clone())
                    .map_err(|e| Error::Corrupt(format!("stored cluster name {n:?}: {e}")))?;
                self.require_cluster(&id)
            })
            .collect()
    }

    pub fn set_cluster_phase(&self, cluster: &ClusterId, phase: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE clusters SET phase = ?2, updated_at_ms = ?3 WHERE name = ?1",
            params![cluster.as_str(), phase, now_ms()],
        )?;
        Ok(())
    }

    /// Remove a cluster and its instance rows.
    ///
    /// Deletes *rows*, never volumes. A registry row is bookkeeping; a
    /// volume is a database (`AGENTS.md` principle 4).
    pub fn delete_cluster(&self, cluster: &ClusterId) -> Result<()> {
        // `pooler_clusters` references this row ON DELETE RESTRICT, so
        // SQLite would refuse anyway — with "FOREIGN KEY constraint
        // failed", which names neither the cluster nor the pooler holding
        // it. Checked here so the operator is told what to delete first.
        if let Some(pooler) = self
            .conn
            .query_row(
                "SELECT pooler FROM pooler_clusters WHERE cluster = ?1 LIMIT 1",
                params![cluster.as_str()],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            return Err(Error::PoolerInUse {
                cluster: cluster.to_string(),
                pooler,
            });
        }
        self.conn.execute(
            "DELETE FROM clusters WHERE name = ?1",
            params![cluster.as_str()],
        )?;
        Ok(())
    }

    // ---- instances ---------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn put_instance(&self, rec: &InstanceRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO instances
               (cluster, ordinal, container_name, container_id, volume_name, host_port,
                phase, role, timeline, last_probe_at_ms, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(cluster, ordinal) DO UPDATE SET
               container_name = excluded.container_name,
               container_id   = excluded.container_id,
               volume_name    = excluded.volume_name,
               host_port      = excluded.host_port,
               phase          = excluded.phase,
               role           = excluded.role,
               timeline       = excluded.timeline,
               last_probe_at_ms = excluded.last_probe_at_ms",
            params![
                rec.cluster,
                rec.ordinal,
                rec.container_name,
                rec.container_id,
                rec.volume_name,
                rec.host_port,
                rec.phase.as_str(),
                rec.role.as_str(),
                rec.timeline,
                rec.last_probe_at_ms,
                now_ms(),
            ],
        )?;
        Ok(())
    }

    pub fn instances(&self, cluster: &ClusterId) -> Result<Vec<InstanceRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT cluster, ordinal, container_name, container_id, volume_name, host_port,
                    phase, role, timeline, last_probe_at_ms
             FROM instances WHERE cluster = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![cluster.as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, u16>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, Option<i64>>(8)?,
                r.get::<_, Option<i64>>(9)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (cluster, ordinal, cname, cid, vol, port, phase, role, tl, probe) = row?;
            out.push(InstanceRecord {
                cluster,
                ordinal,
                container_name: cname,
                container_id: cid,
                volume_name: vol,
                host_port: port,
                phase: phase
                    .parse()
                    .map_err(|e| Error::Corrupt(format!("stored phase {phase:?}: {e}")))?,
                role: role
                    .parse()
                    .map_err(|e| Error::Corrupt(format!("stored role {role:?}: {e}")))?,
                timeline: tl,
                last_probe_at_ms: probe,
            });
        }
        Ok(out)
    }

    pub fn instance(&self, cluster: &ClusterId, ordinal: u32) -> Result<Option<InstanceRecord>> {
        Ok(self
            .instances(cluster)?
            .into_iter()
            .find(|i| i.ordinal == ordinal))
    }

    // ---- poolers -----------------------------------------------------

    /// Insert or update a pooler and the pools it exports.
    ///
    /// The pool rows are replaced wholesale rather than diffed: they are
    /// derived from the manifest, so anything in the table that is not in
    /// the manifest is stale by definition. Done in one transaction, so a
    /// crash cannot leave a pooler advertising pools it no longer has.
    pub fn put_pooler(
        &self,
        manifest: &PoolerManifest,
        container_name: &str,
        container_id: Option<&str>,
        host_port: u16,
        phase: &str,
        pools: &[PoolerPool],
    ) -> Result<i64> {
        let name = manifest.metadata.name.clone();
        let encoded = serde_json::to_string(manifest)
            .map_err(|e| Error::Corrupt(format!("could not encode pooler manifest: {e}")))?;
        let now = now_ms();
        // `unchecked_transaction` because `Pgpod` holds the registry
        // behind a shared reference — the borrow checker cannot see that
        // there is one connection, but SQLite's own locking does.
        let tx = self.conn.unchecked_transaction()?;

        let existing: Option<(String, i64)> = tx
            .query_row(
                "SELECT manifest, generation FROM poolers WHERE name = ?1",
                params![name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        let generation = match existing {
            None => {
                tx.execute(
                    "INSERT INTO poolers (name, manifest, generation, container_name,
                         container_id, host_port, phase, created_at_ms, updated_at_ms)
                     VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?7)",
                    params![
                        name,
                        encoded,
                        container_name,
                        container_id,
                        host_port,
                        phase,
                        now
                    ],
                )?;
                1
            }
            Some((stored, prev)) => {
                let generation = if stored == encoded { prev } else { prev + 1 };
                tx.execute(
                    "UPDATE poolers SET manifest = ?2, generation = ?3, container_name = ?4,
                         container_id = ?5, host_port = ?6, phase = ?7, updated_at_ms = ?8
                     WHERE name = ?1",
                    params![
                        name,
                        encoded,
                        generation,
                        container_name,
                        container_id,
                        host_port,
                        phase,
                        now
                    ],
                )?;
                generation
            }
        };

        tx.execute(
            "DELETE FROM pooler_clusters WHERE pooler = ?1",
            params![name],
        )?;
        for pool in pools {
            tx.execute(
                "INSERT INTO pooler_clusters (pooler, cluster, database, pool_name)
                 VALUES (?1, ?2, ?3, ?4)",
                params![name, pool.cluster, pool.database, pool.pool_name],
            )?;
        }
        tx.commit()?;
        Ok(generation)
    }

    pub fn pooler(&self, pooler: &PoolerId) -> Result<Option<PoolerRecord>> {
        let row: Option<PoolerRow> = self
            .conn
            .query_row(
                "SELECT name, manifest, generation, container_name, container_id,
                        host_port, phase
                 FROM poolers WHERE name = ?1",
                params![pooler.as_str()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()?;

        let Some((name, manifest, generation, container_name, container_id, host_port, phase)) =
            row
        else {
            return Ok(None);
        };
        Ok(Some(PoolerRecord {
            manifest: serde_json::from_str(&manifest)
                .map_err(|e| Error::Corrupt(format!("stored pooler manifest: {e}")))?,
            pools: self.pools_of(&name)?,
            name,
            generation,
            container_name,
            container_id,
            host_port: host_port as u16,
            phase,
        }))
    }

    pub fn require_pooler(&self, pooler: &PoolerId) -> Result<PoolerRecord> {
        self.pooler(pooler)?
            .ok_or_else(|| Error::UnknownPooler(pooler.to_string()))
    }

    fn pools_of(&self, pooler: &str) -> Result<Vec<PoolerPool>> {
        let mut stmt = self.conn.prepare(
            "SELECT cluster, database, pool_name FROM pooler_clusters
             WHERE pooler = ?1 ORDER BY pool_name",
        )?;
        let rows = stmt
            .query_map(params![pooler], |r| {
                Ok(PoolerPool {
                    cluster: r.get(0)?,
                    database: r.get(1)?,
                    pool_name: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_poolers(&self) -> Result<Vec<PoolerRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM poolers ORDER BY name")?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        names
            .into_iter()
            .map(|n| {
                let id = PoolerId::new(n.clone())
                    .map_err(|e| Error::Corrupt(format!("stored pooler name {n:?}: {e}")))?;
                self.require_pooler(&id)
            })
            .collect()
    }

    /// Every pooler fronting a cluster.
    ///
    /// What a switchover consults to find the pools to hold, and what
    /// `delete` consults to refuse.
    pub fn poolers_for_cluster(&self, cluster: &ClusterId) -> Result<Vec<PoolerRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT pooler FROM pooler_clusters WHERE cluster = ?1 ORDER BY pooler",
        )?;
        let names: Vec<String> = stmt
            .query_map(params![cluster.as_str()], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        names
            .into_iter()
            .map(|n| {
                let id = PoolerId::new(n.clone())
                    .map_err(|e| Error::Corrupt(format!("stored pooler name {n:?}: {e}")))?;
                self.require_pooler(&id)
            })
            .collect()
    }

    pub fn set_pooler_phase(&self, pooler: &PoolerId, phase: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE poolers SET phase = ?2, updated_at_ms = ?3 WHERE name = ?1",
            params![pooler.as_str(), phase, now_ms()],
        )?;
        Ok(())
    }

    pub fn delete_pooler(&self, pooler: &PoolerId) -> Result<()> {
        self.conn.execute(
            "DELETE FROM poolers WHERE name = ?1",
            params![pooler.as_str()],
        )?;
        Ok(())
    }

    // ---- events ------------------------------------------------------

    pub fn record_event(
        &self,
        cluster: &str,
        ordinal: Option<u32>,
        level: &str,
        message: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (cluster, ordinal, at_ms, level, message)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![cluster, ordinal, now_ms(), level, message],
        )?;
        Ok(())
    }

    pub fn recent_events(&self, cluster: &ClusterId, limit: u32) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT cluster, ordinal, at_ms, level, message FROM events
             WHERE cluster = ?1 ORDER BY at_ms DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![cluster.as_str(), limit], |r| {
            Ok(Event {
                cluster: r.get(0)?,
                ordinal: r.get(1)?,
                at_ms: r.get(2)?,
                level: r.get(3)?,
                message: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(name: &str, image: &str) -> ClusterManifest {
        ClusterManifest::from_yaml(&format!(
            "apiVersion: pgpod/v1\nkind: Cluster\nmetadata:\n  name: {name}\nspec:\n  imageName: {image}\n"
        ))
        .unwrap()
    }

    fn registry() -> Registry {
        Registry::open_in_memory().unwrap()
    }

    fn id(name: &str) -> ClusterId {
        ClusterId::new(name).unwrap()
    }

    #[test]
    fn migrations_run_on_a_fresh_database() {
        let r = registry();
        assert!(r.list_clusters().unwrap().is_empty());
    }

    #[test]
    fn opening_twice_is_idempotent() {
        // The daemon and the CLI both open the same file; a second
        // migration run must not fail or duplicate anything.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("pgpod.db");
        let first = Registry::open(&path).unwrap();
        first
            .put_cluster(&manifest("mydb", "postgres:18"), "running")
            .unwrap();
        drop(first);

        let second = Registry::open(&path).unwrap();
        assert_eq!(second.list_clusters().unwrap().len(), 1);
    }

    #[test]
    fn manifests_round_trip_through_storage() {
        let r = registry();
        let m = manifest("mydb", "docker.io/library/postgres:18");
        r.put_cluster(&m, "running").unwrap();
        let stored = r.require_cluster(&id("mydb")).unwrap();
        assert_eq!(stored.manifest, m);
    }

    #[test]
    fn generation_bumps_only_when_the_manifest_changes() {
        // A reconciler uses this to tell "spec changed" from "nothing to
        // do" without diffing JSON, so an unchanged re-apply must not
        // look like a change.
        let r = registry();
        let m = manifest("mydb", "postgres:18");
        assert_eq!(r.put_cluster(&m, "running").unwrap(), 1);
        assert_eq!(
            r.put_cluster(&m, "running").unwrap(),
            1,
            "re-apply is not a change"
        );

        let changed = manifest("mydb", "postgres:17");
        assert_eq!(r.put_cluster(&changed, "running").unwrap(), 2);
    }

    #[test]
    fn instances_upsert_rather_than_duplicate() {
        let r = registry();
        r.put_cluster(&manifest("mydb", "postgres:18"), "running")
            .unwrap();

        let mut rec = InstanceRecord {
            cluster: "mydb".into(),
            ordinal: 1,
            container_name: "pgpod-mydb-1".into(),
            container_id: None,
            volume_name: "pgpod-mydb-1-pgdata".into(),
            host_port: 5432,
            phase: InstancePhase::Creating,
            role: InstanceRole::Unknown,
            timeline: None,
            last_probe_at_ms: None,
        };
        r.put_instance(&rec).unwrap();

        rec.container_id = Some("abc123".into());
        rec.phase = InstancePhase::Running;
        rec.role = InstanceRole::Primary;
        r.put_instance(&rec).unwrap();

        let stored = r.instances(&id("mydb")).unwrap();
        assert_eq!(stored.len(), 1, "upsert must not duplicate");
        assert_eq!(stored[0].container_id.as_deref(), Some("abc123"));
        assert_eq!(stored[0].phase, InstancePhase::Running);
        assert_eq!(stored[0].role, InstanceRole::Primary);
        assert_eq!(stored[0].host_port, 5432);
    }

    #[test]
    fn deleting_a_cluster_cascades_to_its_instances() {
        // Requires PRAGMA foreign_keys = ON, which SQLite does not enable
        // by default — without it these rows would silently orphan.
        let r = registry();
        r.put_cluster(&manifest("mydb", "postgres:18"), "running")
            .unwrap();
        r.put_instance(&InstanceRecord {
            cluster: "mydb".into(),
            ordinal: 1,
            container_name: "pgpod-mydb-1".into(),
            container_id: None,
            volume_name: "v".into(),
            host_port: 5432,
            phase: InstancePhase::Running,
            role: InstanceRole::Primary,
            timeline: None,
            last_probe_at_ms: None,
        })
        .unwrap();

        r.delete_cluster(&id("mydb")).unwrap();
        assert!(r.instances(&id("mydb")).unwrap().is_empty());
    }

    #[test]
    fn an_unknown_cluster_is_a_named_error_not_a_panic() {
        let err = registry().require_cluster(&id("nope")).unwrap_err();
        assert!(matches!(err, Error::UnknownCluster(_)));
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn events_come_back_newest_first() {
        let r = registry();
        r.put_cluster(&manifest("mydb", "postgres:18"), "running")
            .unwrap();
        for i in 0..5 {
            r.record_event("mydb", Some(1), "info", &format!("event {i}"))
                .unwrap();
        }
        let events = r.recent_events(&id("mydb"), 3).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].message, "event 4", "newest first");
    }

    #[test]
    fn phases_and_roles_survive_the_string_round_trip() {
        // These are stored as TEXT; a rename on the enum side without a
        // migration would surface here rather than at read time in prod.
        let r = registry();
        r.put_cluster(&manifest("mydb", "postgres:18"), "running")
            .unwrap();
        for phase in [
            InstancePhase::Creating,
            InstancePhase::Bootstrapping,
            InstancePhase::Running,
            InstancePhase::Fenced,
            InstancePhase::NeedsRebuild,
        ] {
            r.put_instance(&InstanceRecord {
                cluster: "mydb".into(),
                ordinal: 1,
                container_name: "c".into(),
                container_id: None,
                volume_name: "v".into(),
                host_port: 5432,
                phase,
                role: InstanceRole::Standby,
                timeline: Some(3),
                last_probe_at_ms: Some(now_ms()),
            })
            .unwrap();
            let stored = &r.instances(&id("mydb")).unwrap()[0];
            assert_eq!(stored.phase, phase);
            assert_eq!(stored.role, InstanceRole::Standby);
        }
    }

    // ---- poolers -----------------------------------------------------

    fn pooler_manifest(name: &str, clusters: &[&str]) -> PoolerManifest {
        let mut yaml = format!(
            "apiVersion: pgpod/v1\nkind: Pooler\nmetadata:\n  name: {name}\nspec:\n  clusters:\n"
        );
        for (i, c) in clusters.iter().enumerate() {
            yaml.push_str(&format!(
                "    - cluster: {c}\n      pools:\n        - database: appdb\n          as: pool{i}\n"
            ));
        }
        PoolerManifest::from_yaml(&yaml).unwrap()
    }

    fn pool(cluster: &str, name: &str) -> PoolerPool {
        PoolerPool {
            cluster: cluster.into(),
            database: "appdb".into(),
            pool_name: name.into(),
        }
    }

    fn pid(name: &str) -> PoolerId {
        PoolerId::new(name).unwrap()
    }

    fn with_cluster(names: &[&str]) -> Registry {
        let r = registry();
        for n in names {
            r.put_cluster(&manifest(n, "postgres:18"), "running")
                .unwrap();
        }
        r
    }

    #[test]
    fn a_pooler_round_trips_with_its_pools() {
        let r = with_cluster(&["mydb"]);
        let m = pooler_manifest("app", &["mydb"]);
        let generation = r
            .put_pooler(
                &m,
                "pgpod-pooler-app",
                Some("abc"),
                6432,
                "running",
                &[pool("mydb", "pool0")],
            )
            .unwrap();
        assert_eq!(generation, 1);

        let stored = r.require_pooler(&pid("app")).unwrap();
        assert_eq!(stored.container_name, "pgpod-pooler-app");
        assert_eq!(stored.container_id.as_deref(), Some("abc"));
        assert_eq!(stored.host_port, 6432);
        assert_eq!(stored.pools, vec![pool("mydb", "pool0")]);
        assert_eq!(stored.manifest, m);
    }

    #[test]
    fn re_applying_an_unchanged_pooler_does_not_bump_the_generation() {
        // Same contract as clusters: a reconciler tells "spec changed"
        // from "nothing to do" without diffing JSON itself.
        let r = with_cluster(&["mydb"]);
        let m = pooler_manifest("app", &["mydb"]);
        let pools = [pool("mydb", "pool0")];
        assert_eq!(
            r.put_pooler(&m, "c", None, 6432, "running", &pools)
                .unwrap(),
            1
        );
        assert_eq!(
            r.put_pooler(&m, "c", None, 6432, "running", &pools)
                .unwrap(),
            1
        );

        let changed = pooler_manifest("app", &["mydb", "other"]);
        r.put_cluster(&manifest("other", "postgres:18"), "running")
            .unwrap();
        assert_eq!(
            r.put_pooler(
                &changed,
                "c",
                None,
                6432,
                "running",
                &[pool("mydb", "pool0"), pool("other", "pool1")]
            )
            .unwrap(),
            2
        );
    }

    #[test]
    fn pool_rows_are_replaced_wholesale_not_merged() {
        // They are derived from the manifest, so a row the manifest no
        // longer mentions is stale by definition — and a pooler
        // advertising a pool it does not serve would hold a cluster that
        // is not there.
        let r = with_cluster(&["mydb", "other"]);
        r.put_pooler(
            &pooler_manifest("app", &["mydb", "other"]),
            "c",
            None,
            6432,
            "running",
            &[pool("mydb", "pool0"), pool("other", "pool1")],
        )
        .unwrap();
        r.put_pooler(
            &pooler_manifest("app", &["mydb"]),
            "c",
            None,
            6432,
            "running",
            &[pool("mydb", "pool0")],
        )
        .unwrap();
        let stored = r.require_pooler(&pid("app")).unwrap();
        assert_eq!(stored.pools, vec![pool("mydb", "pool0")]);
    }

    #[test]
    fn poolers_are_findable_by_the_cluster_they_front() {
        // What a switchover consults to find the pools to hold.
        let r = with_cluster(&["mydb", "other"]);
        r.put_pooler(
            &pooler_manifest("shared", &["mydb", "other"]),
            "c1",
            None,
            6432,
            "running",
            &[pool("mydb", "pool0"), pool("other", "pool1")],
        )
        .unwrap();
        r.put_pooler(
            &pooler_manifest("solo", &["mydb"]),
            "c2",
            None,
            6433,
            "running",
            &[pool("mydb", "solo-appdb")],
        )
        .unwrap();

        let names: Vec<String> = r
            .poolers_for_cluster(&id("mydb"))
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["shared".to_string(), "solo".to_string()]);
        assert_eq!(r.poolers_for_cluster(&id("other")).unwrap().len(), 1);
    }

    #[test]
    fn deleting_a_cluster_a_pooler_fronts_is_refused_by_name() {
        // SQLite would refuse this anyway, with "FOREIGN KEY constraint
        // failed" — which names neither the cluster nor the pooler, and
        // leaves the operator with nothing to act on.
        let r = with_cluster(&["mydb"]);
        r.put_pooler(
            &pooler_manifest("app", &["mydb"]),
            "c",
            None,
            6432,
            "running",
            &[pool("mydb", "pool0")],
        )
        .unwrap();

        let err = r.delete_cluster(&id("mydb")).unwrap_err();
        assert!(matches!(err, Error::PoolerInUse { .. }), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("mydb") && msg.contains("app"), "{msg}");
        assert!(
            msg.contains("pgpod pooler delete"),
            "the message must name the fix: {msg}"
        );

        // And once the pooler is gone, the cluster deletes normally.
        r.delete_pooler(&pid("app")).unwrap();
        r.delete_cluster(&id("mydb")).unwrap();
        assert!(r.list_clusters().unwrap().is_empty());
    }

    #[test]
    fn deleting_a_pooler_takes_its_pool_rows_with_it() {
        let r = with_cluster(&["mydb"]);
        r.put_pooler(
            &pooler_manifest("app", &["mydb"]),
            "c",
            None,
            6432,
            "running",
            &[pool("mydb", "pool0")],
        )
        .unwrap();
        r.delete_pooler(&pid("app")).unwrap();
        assert!(r.pooler(&pid("app")).unwrap().is_none());
        assert!(r.poolers_for_cluster(&id("mydb")).unwrap().is_empty());
    }

    #[test]
    fn two_pools_cannot_share_a_name_within_one_pooler() {
        // Enforced durably, not only at manifest parse time: the pool name
        // is what a client puts in dbname.
        let r = with_cluster(&["mydb", "other"]);
        let err = r.put_pooler(
            &pooler_manifest("app", &["mydb", "other"]),
            "c",
            None,
            6432,
            "running",
            &[pool("mydb", "same"), pool("other", "same")],
        );
        assert!(err.is_err(), "duplicate pool names must be refused");
        // The failed insert must not have left a half-written pooler.
        assert!(r.pooler(&pid("app")).unwrap().is_none());
    }

    #[test]
    fn an_unknown_pooler_is_a_distinct_error() {
        let r = registry();
        assert!(matches!(
            r.require_pooler(&pid("nope")).unwrap_err(),
            Error::UnknownPooler(_)
        ));
    }
}
