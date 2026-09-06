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

use pgpod_core::{ClusterId, ClusterManifest, InstancePhase, InstanceRole};
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
}
