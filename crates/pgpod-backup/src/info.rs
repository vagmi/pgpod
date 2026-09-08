//! Reading `pgbackrest info --output=json`.
//!
//! This is what closes the gap ADR 01 §5 could not: **the repository is
//! self-describing**. pgpod's own Phase 2 implementation needed a copy of
//! every backup manifest in its SQLite registry, because the daemon had no
//! other way to know what was restorable — which meant a host that lost
//! its registry could not restore, even though object storage was the only
//! way to get data out of a cluster at all.
//!
//! pgBackRest answers the question from the bucket. The registry keeps
//! cluster and instance state and nothing about backups.
//!
//! Only the fields pgpod actually uses are deserialized. pgBackRest's JSON
//! is large, versioned, and grows; binding all of it would turn every
//! upstream addition into a parse error.

use chrono::{DateTime, TimeZone as _, Utc};
use serde::Deserialize;

use crate::Error;

/// One stanza's entry in `pgbackrest info`.
#[derive(Debug, Clone, Deserialize)]
pub struct StanzaInfo {
    pub name: String,
    #[serde(default)]
    pub backup: Vec<Backup>,
    #[serde(default)]
    pub archive: Vec<ArchiveInfo>,
    #[serde(default)]
    pub status: Status,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Status {
    /// 0 is ok. Anything else is explained by `message`.
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: String,
}

/// What the archive looks like for one PostgreSQL version of a stanza.
///
/// `min`/`max` are the oldest and newest WAL segments present. They are
/// what `pgpod status` needs in order to say how far behind the archive
/// is, which ADR 01's consequences insist must be visible.
#[derive(Debug, Clone, Deserialize)]
pub struct ArchiveInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub min: Option<String>,
    #[serde(default)]
    pub max: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackupType {
    Full,
    Diff,
    Incr,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Backup {
    /// pgBackRest's label, e.g. `20260904-020000F`. This is what
    /// `pgbackrest restore --set` takes.
    pub label: String,
    #[serde(rename = "type")]
    pub backup_type: BackupType,
    pub timestamp: Timestamp,
    #[serde(default)]
    pub archive: Option<BackupArchive>,
    #[serde(default)]
    pub info: Option<BackupSize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Timestamp {
    /// Unix seconds.
    pub start: i64,
    pub stop: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BackupArchive {
    /// First WAL segment the backup needs.
    #[serde(default)]
    pub start: Option<String>,
    /// Last WAL segment the backup needs.
    #[serde(default)]
    pub stop: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BackupSize {
    /// Size of the database, uncompressed.
    #[serde(default)]
    pub size: u64,
    /// What the repository actually holds for this backup.
    #[serde(default)]
    pub repository: Option<RepoSize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoSize {
    #[serde(default)]
    pub delta: u64,
    #[serde(default)]
    pub size: u64,
}

impl Backup {
    /// When the backup finished.
    ///
    /// `stop`, not `start`: a backup that was still running at an instant
    /// does not contain a consistent view of it, so this is the value that
    /// decides whether a backup can serve a PITR target.
    pub fn ended_at(&self) -> DateTime<Utc> {
        Utc.timestamp_opt(self.timestamp.stop, 0)
            .single()
            .unwrap_or_else(Utc::now)
    }

    pub fn started_at(&self) -> DateTime<Utc> {
        Utc.timestamp_opt(self.timestamp.start, 0)
            .single()
            .unwrap_or_else(Utc::now)
    }

    pub fn size_bytes(&self) -> u64 {
        self.info.as_ref().map(|i| i.size).unwrap_or(0)
    }
}

impl StanzaInfo {
    /// Backups oldest first.
    ///
    /// pgBackRest already returns them in order; sorting is cheap and
    /// means nothing downstream depends on that staying true.
    pub fn backups_oldest_first(&self) -> Vec<Backup> {
        let mut out = self.backup.clone();
        out.sort_by_key(|b| b.timestamp.stop);
        out
    }

    /// The newest backup that finished at or before `at`.
    ///
    /// `None` when nothing qualifies, rather than a best guess: pgBackRest
    /// will refuse a target it cannot reach anyway, and failing here gives
    /// a message that names the backups that *do* exist.
    pub fn backup_for_target(&self, at: Option<DateTime<Utc>>) -> Option<Backup> {
        self.backups_oldest_first()
            .into_iter()
            .rfind(|b| at.is_none_or(|t| b.ended_at() <= t))
    }
}

/// Parse the whole `info --output=json` document.
pub fn parse_info(json: &str) -> Result<Vec<StanzaInfo>, Error> {
    serde_json::from_str(json).map_err(|e| Error::Info(e.to_string()))
}

/// Parse and pick out one stanza.
///
/// A stanza that pgBackRest does not know about is `Ok(None)`, not an
/// error: it is what an unconfigured or never-backed-up cluster looks
/// like, and `pgpod backups` on one should print "none" rather than fail.
pub fn stanza(json: &str, name: &str) -> Result<Option<StanzaInfo>, Error> {
    Ok(parse_info(json)?.into_iter().find(|s| s.name == name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from real `pgbackrest info --output=json` output.
    const SAMPLE: &str = r#"[
      {
        "archive": [
          {
            "database": {"id": 1},
            "id": "18-1",
            "max": "000000010000000000000009",
            "min": "000000010000000000000002"
          }
        ],
        "backup": [
          {
            "archive": {
              "start": "000000010000000000000002",
              "stop": "000000010000000000000002"
            },
            "info": {
              "delta": 24000000,
              "repository": {"delta": 3000000, "size": 3000000},
              "size": 24000000
            },
            "label": "20260904-020000F",
            "prior": null,
            "timestamp": {"start": 1788487200, "stop": 1788487215},
            "type": "full"
          },
          {
            "archive": {
              "start": "000000010000000000000007",
              "stop": "000000010000000000000007"
            },
            "info": {
              "delta": 100000,
              "repository": {"delta": 20000, "size": 3100000},
              "size": 24500000
            },
            "label": "20260904-020000F_20260905-020000I",
            "timestamp": {"start": 1788573600, "stop": 1788573605},
            "type": "incr"
          }
        ],
        "cipher": "none",
        "db": [{"id": 1, "version": "18"}],
        "name": "mydb",
        "status": {"code": 0, "message": "ok"}
      }
    ]"#;

    /// What an empty repository actually returns — captured by running it.
    const EMPTY: &str = r#"[{"archive":[],"backup":[],"cipher":"none","db":[],
      "name":"t","repo":[{"cipher":"none","key":1,
      "status":{"code":1,"message":"missing stanza path"}}],
      "status":{"code":1,"message":"missing stanza path"}}]"#;

    #[test]
    fn parses_a_real_info_document() {
        let s = stanza(SAMPLE, "mydb").unwrap().unwrap();
        assert_eq!(s.backup.len(), 2);
        assert_eq!(s.status.code, 0);
        assert_eq!(
            s.archive[0].min.as_deref(),
            Some("000000010000000000000002")
        );
        assert_eq!(
            s.archive[0].max.as_deref(),
            Some("000000010000000000000009")
        );
    }

    #[test]
    fn reads_the_label_restore_will_be_given() {
        // `pgbackrest restore --set` takes this string; getting it wrong
        // restores a different backup than the one pgpod chose.
        let s = stanza(SAMPLE, "mydb").unwrap().unwrap();
        assert_eq!(s.backup[0].label, "20260904-020000F");
        assert_eq!(s.backup[0].backup_type, BackupType::Full);
        assert_eq!(s.backup[1].backup_type, BackupType::Incr);
    }

    #[test]
    fn an_empty_repository_is_not_an_error() {
        // What a cluster that has never been backed up looks like.
        // `pgpod backups` on one should say "none", not fail.
        let s = stanza(EMPTY, "t").unwrap().unwrap();
        assert!(s.backup.is_empty());
        assert_ne!(s.status.code, 0, "and it says why");
    }

    #[test]
    fn an_unknown_stanza_is_absent_rather_than_an_error() {
        assert!(stanza(SAMPLE, "otherdb").unwrap().is_none());
    }

    #[test]
    fn picks_the_newest_backup_at_or_before_the_target() {
        let s = stanza(SAMPLE, "mydb").unwrap().unwrap();
        // Between the two backups.
        let between = Utc.timestamp_opt(1788500000, 0).single().unwrap();
        assert_eq!(
            s.backup_for_target(Some(between)).unwrap().label,
            "20260904-020000F"
        );
        // After both.
        let after = Utc.timestamp_opt(1788600000, 0).single().unwrap();
        assert_eq!(
            s.backup_for_target(Some(after)).unwrap().label,
            "20260904-020000F_20260905-020000I"
        );
    }

    #[test]
    fn a_backup_still_running_at_the_target_is_not_eligible() {
        // Its stop time is after the target, so it holds no consistent
        // view of that instant.
        let s = stanza(SAMPLE, "mydb").unwrap().unwrap();
        let during = Utc.timestamp_opt(1788487205, 0).single().unwrap();
        assert!(
            s.backup_for_target(Some(during)).is_none(),
            "a backup that had not finished must not be chosen"
        );
    }

    #[test]
    fn no_target_means_the_newest_backup() {
        // What `pgpod fork` wants.
        let s = stanza(SAMPLE, "mydb").unwrap().unwrap();
        assert_eq!(
            s.backup_for_target(None).unwrap().label,
            "20260904-020000F_20260905-020000I"
        );
    }

    #[test]
    fn a_target_before_every_backup_yields_nothing() {
        let s = stanza(SAMPLE, "mydb").unwrap().unwrap();
        let early = Utc.timestamp_opt(1, 0).single().unwrap();
        assert!(s.backup_for_target(Some(early)).is_none());
    }

    #[test]
    fn unknown_fields_do_not_break_parsing() {
        // pgBackRest's JSON grows between releases. Binding it strictly
        // would turn every upstream addition into a failure to list
        // backups — on the path an operator reaches for during an
        // incident.
        let json = r#"[{"name":"mydb","backup":[],"archive":[],
            "status":{"code":0,"message":"ok"},
            "somethingNew":{"nested":[1,2,3]}}]"#;
        assert!(stanza(json, "mydb").unwrap().is_some());
    }

    #[test]
    fn malformed_output_is_an_error_rather_than_an_empty_list() {
        // "no backups" and "pgBackRest did not run" need different fixes,
        // so they must not look alike.
        assert!(parse_info("not json").is_err());
        assert!(parse_info("").is_err());
    }
}
