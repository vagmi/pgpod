//! Major-version upgrades: what the daemon tells the agent to do, and
//! what the agent reports back.
//!
//! A major upgrade is the one operation that needs **two images at once**.
//! `pg_upgrade` reads the old cluster with the old server's binaries and
//! writes the new one with the new server's, and no PostgreSQL image
//! carries both. pgpod solves it the way CloudNativePG does — stage the
//! old installation out of the old image, then run `pg_upgrade` in the new
//! one — and the two halves talk through these types (ADR 06).
//!
//! Three job containers, and one spec between them. The first two —
//! staging the old installation out of the old image, and asking the new
//! image what it carries — need nothing told to them: they read the
//! volume they mount and the image they run in, which is why only the
//! third takes an [`UpgradeSpec`]. A step that cannot be told the wrong
//! answer cannot be given one.
//!
//! Each reports a JSON document on stdout, marked with [`REPORT_MARKER`]
//! so the control plane can find it in a log stream that also carries
//! `pg_upgrade`'s own output.

use serde::{Deserialize, Serialize};

/// Environment variable carrying the JSON-encoded job spec.
///
/// One variable for all three, because a container runs exactly one of
/// them and the subcommand already says which.
pub const UPGRADE_SPEC_ENV: &str = "PGPOD_UPGRADE_SPEC";

/// Prefix on the one line of stdout that is a report rather than a log.
///
/// `pg_upgrade` writes pages of progress to the same stream, and podman's
/// log stream merges stdout and stderr, so the report needs a marker
/// rather than a position. Same problem `pgbackrest info --output=json`
/// has; there the document is findable because it is the only line
/// starting with `[`, which is luck this does not have.
pub const REPORT_MARKER: &str = "pgpod-report:";

/// How `pg_upgrade` moves the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeMethod {
    /// Hard-link the data files into the new cluster. Seconds regardless
    /// of database size, which is the whole reason a major upgrade can
    /// fit inside a pooler's hold — and **one-way**: `pg_upgrade` renames
    /// the old cluster's `global/pg_control` out of the way, so the old
    /// cluster cannot be started again.
    #[default]
    Link,
    /// Copy every file. The old cluster stays startable, at the cost of a
    /// window proportional to the data.
    Copy,
}

impl UpgradeMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Link => "link",
            Self::Copy => "copy",
        }
    }

    /// The `pg_upgrade` flag, if any. `copy` is the default and has none
    /// worth passing.
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Self::Link => Some("--link"),
            Self::Copy => None,
        }
    }

    /// Whether the old data directory is still a cluster afterwards.
    pub fn old_cluster_survives(self) -> bool {
        matches!(self, Self::Copy)
    }
}

impl std::str::FromStr for UpgradeMethod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "link" => Ok(Self::Link),
            "copy" => Ok(Self::Copy),
            other => Err(format!("unknown upgrade method {other:?} — link or copy")),
        }
    }
}

/// Run `pg_upgrade`, then put the new cluster where PGDATA belongs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeSpec {
    /// Major version of the cluster on disk, as the stage reported it.
    pub from_version: String,
    /// Major version this image carries, as the probe reported it.
    pub to_version: String,
    /// Where the staged old installation's `bin` ended up — the `-b`
    /// `pg_upgrade` will be given, via the wrappers described in
    /// [`crate::container::UPGRADE_WRAPPER_DIR`].
    pub staged_bindir: String,
    /// Directories under the staging root that hold shared libraries.
    /// They become the old binaries' `LD_LIBRARY_PATH`, and **only**
    /// theirs (ADR 06 §3).
    pub staged_lib_dirs: Vec<String>,
    pub method: UpgradeMethod,
    /// `pg_upgrade --jobs`.
    pub jobs: u32,
    /// `shared_preload_libraries` for the new server while `pg_upgrade`
    /// restores the schema into it. A `CREATE EXTENSION` for a library
    /// that has to be preloaded fails otherwise, against a server that
    /// could have loaded it.
    #[serde(default)]
    pub preload_libraries: Vec<String>,
    /// Check only. `pg_upgrade --check` rehearses the whole thing and
    /// changes nothing, which is the only way to find out whether an
    /// upgrade will work before the maintenance window it needs.
    #[serde(default)]
    pub check: bool,
    /// How the new cluster is `initdb`'d. Locale and encoding have to
    /// match the old cluster or `pg_upgrade` refuses, so these are the
    /// cluster's own bootstrap settings rather than defaults.
    pub initdb: crate::InitdbBootstrap,
}

impl UpgradeSpec {
    /// `LD_LIBRARY_PATH` for the staged old binaries.
    pub fn library_path(&self) -> String {
        self.staged_lib_dirs.join(":")
    }

    /// Read and parse the spec from the environment, as the job container
    /// receives it.
    pub fn from_env() -> Result<Self, crate::SpecError> {
        let raw = std::env::var(UPGRADE_SPEC_ENV).map_err(|_| crate::SpecError::Missing)?;
        serde_json::from_str(&raw).map_err(|e| crate::SpecError::Malformed(e.to_string()))
    }

    pub fn to_env_value(&self) -> Result<String, crate::SpecError> {
        serde_json::to_string(self).map_err(|e| crate::SpecError::Malformed(e.to_string()))
    }
}

/// What the stage job did.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StageReport {
    /// `pg_config --version`, reduced to the major version.
    pub version: String,
    /// Where the staged copy of the old `bin` directory is.
    pub bindir: String,
    pub sharedir: String,
    pub pkglibdir: String,
    /// Directories holding the staged shared libraries.
    pub lib_dirs: Vec<String>,
    pub files: usize,
    pub bytes: u64,
}

/// What the probe job found: what the **image** carries, and what the
/// **volume** holds.
///
/// Both, from one container, because the two questions are always asked
/// together — an upgrade compares them to decide whether it can go ahead,
/// and `apply` compares them to decide whether it must refuse.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    pub version: String,
    pub bindir: String,
    /// The major version of the data directory in the mounted volume.
    /// `None` when there is no cluster there yet — a volume that has
    /// never been bootstrapped, which is not an error and must not be
    /// mistaken for a mismatch.
    #[serde(default)]
    pub data_version: Option<String>,
}

/// What the upgrade job did.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeRunReport {
    pub from_version: String,
    pub to_version: String,
    pub method: UpgradeMethod,
    /// Where the pre-upgrade data directory was left. Never deleted by
    /// pgpod — principle 4 applies inside a volume as much as to the
    /// volume itself, and in `copy` mode this directory is the way back.
    pub old_data_dir: String,
    /// Seconds `pg_upgrade` itself took, which is the number that has to
    /// fit inside the pooler's budget.
    pub seconds: u64,
}

/// Find a report in a container's log stream.
///
/// The job's stdout carries `pg_upgrade`'s progress as well, and podman
/// merges stderr into the same stream, so the marker is the only reliable
/// way to tell the return value from the narration.
pub fn parse_report<T: serde::de::DeserializeOwned>(logs: &str) -> Result<T, String> {
    let line = logs
        .lines()
        .filter_map(|l| l.trim().strip_prefix(REPORT_MARKER))
        .next_back()
        .ok_or_else(|| format!("the job produced no {REPORT_MARKER} line"))?;
    serde_json::from_str(line.trim()).map_err(|e| format!("unreadable job report: {e}"))
}

/// Render a report onto stdout, where the control plane will look for it.
pub fn print_report<T: Serialize>(report: &T) -> Result<(), String> {
    let json = serde_json::to_string(report).map_err(|e| e.to_string())?;
    println!("{REPORT_MARKER}{json}");
    Ok(())
}

/// The major version named by a `PG_VERSION` file or a
/// `pg_config --version` string, as PostgreSQL itself would write it.
///
/// One number from 10 onwards and two before it, and both spellings still
/// turn up: an image running 9.6 writes `9.6` into `PG_VERSION`.
pub fn major_label(raw: &str) -> Option<String> {
    let digits = raw.trim().trim_start_matches(|c: char| !c.is_ascii_digit());
    let mut parts = digits.split(['.', ' ', '-', '+', '(']);
    let major: u32 = parts.next()?.parse().ok()?;
    if major < 9 {
        return None;
    }
    if major == 9 {
        // 9.x is a major version in two parts. pgpod has never created
        // one, but `pg_upgrade` accepts one as a source, and treating
        // "9.6" and "9.5" as the same major would let an upgrade that
        // changes nothing look like one that does.
        let minor: u32 = parts.next()?.parse().ok()?;
        return Some(format!("9.{minor}"));
    }
    Some(major.to_string())
}

/// An orderable key for a major version.
///
/// Compared as a number rather than a string, because `"9.6"` sorts after
/// `"18"` and a downgrade that looked like an upgrade would destroy a
/// cluster. The value is `major * 100 + minor`, so it is comparable and
/// not something to print.
pub fn version_key(raw: &str) -> Option<u32> {
    let label = major_label(raw)?;
    let mut parts = label.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some(major * 100 + minor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_is_found_among_the_narration() {
        let logs = format!(
            "Performing Consistency Checks\nChecking cluster versions ok\n\
             {REPORT_MARKER}{{\"version\":\"17\",\"bindir\":\"/b\",\"sharedir\":\"/s\",\
             \"pkglibdir\":\"/p\",\"libDirs\":[\"/l\"],\"files\":3,\"bytes\":12}}\n\
             Upgrade Complete\n"
        );
        let report: StageReport = parse_report(&logs).expect("report found");
        assert_eq!(report.version, "17");
        assert_eq!(report.lib_dirs, vec!["/l".to_string()]);
    }

    #[test]
    fn the_last_report_wins() {
        // A retried job appends; the newest line is the one that
        // describes the state on disk.
        let logs = format!(
            "{REPORT_MARKER}{{\"version\":\"17\",\"bindir\":\"/old\"}}\n\
             {REPORT_MARKER}{{\"version\":\"18\",\"bindir\":\"/new\"}}\n"
        );
        let report: ProbeReport = parse_report(&logs).expect("report found");
        assert_eq!(report.bindir, "/new");
    }

    #[test]
    fn a_job_that_printed_no_report_is_an_error_not_a_default() {
        // Silently defaulting here would report a successful upgrade for
        // a job that died before doing anything.
        let err = parse_report::<ProbeReport>("pg_upgrade: command not found\n").unwrap_err();
        assert!(err.contains(REPORT_MARKER), "{err}");
    }

    #[test]
    fn major_versions_read_the_way_postgresql_writes_them() {
        assert_eq!(major_label("17"), Some("17".to_string()));
        assert_eq!(major_label("18\n"), Some("18".to_string()));
        assert_eq!(
            major_label("PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2)"),
            Some("18".to_string())
        );
        assert_eq!(
            major_label("pg_config (PostgreSQL) 17.11"),
            Some("17".to_string())
        );
        assert_eq!(major_label("9.6"), Some("9.6".to_string()));
        assert_eq!(major_label(""), None);
        assert_eq!(major_label("not a version"), None);
    }

    #[test]
    fn version_keys_order_9_6_before_10() {
        // The reason this is a number at all: as strings, "9.6" sorts
        // after "18", and a downgrade would read as an upgrade.
        assert!(version_key("9.6").unwrap() < version_key("10").unwrap());
        assert!(version_key("17").unwrap() < version_key("18").unwrap());
        assert_eq!(version_key("18"), version_key("18.6"));
    }

    #[test]
    fn link_is_the_one_way_method() {
        assert!(!UpgradeMethod::Link.old_cluster_survives());
        assert!(UpgradeMethod::Copy.old_cluster_survives());
        assert_eq!(UpgradeMethod::Link.flag(), Some("--link"));
        assert_eq!(
            UpgradeMethod::Copy.flag(),
            None,
            "copy is pg_upgrade's default and needs no flag"
        );
    }

    #[test]
    fn the_library_path_is_a_colon_list() {
        let spec = UpgradeSpec {
            from_version: "17".into(),
            to_version: "18".into(),
            staged_bindir: "/pgdata/upgrade/old/usr/lib/postgresql/17/bin".into(),
            staged_lib_dirs: vec!["/a".into(), "/b".into()],
            method: UpgradeMethod::Link,
            jobs: 2,
            preload_libraries: Vec::new(),
            check: false,
            initdb: crate::InitdbBootstrap::default(),
        };
        assert_eq!(spec.library_path(), "/a:/b");
    }
}
