//! `pg_upgrade` invocation, as a pure plan.
//!
//! Everything decided here is decided *before* a container exists: which
//! binaries, which data directories, which options the two servers
//! `pg_upgrade` starts for itself will run with. The agent turns this into
//! a process; nothing in this module knows what a container is.
//!
//! The two option strings are the parts that are easy to get silently
//! wrong, so they are built here where they can be asserted:
//!
//! * the **old** server must not archive — it is about to be replaced,
//!   and a segment shipped from it belongs to a cluster that will not
//!   exist in a minute — and must not JIT, because the staging step
//!   deliberately leaves LLVM behind (ADR 06 §3);
//! * the **new** server must preload whatever the cluster preloads, or a
//!   `CREATE EXTENSION` in the restored schema fails against a server that
//!   could have loaded it.

use pgpod_core::UpgradeMethod;

/// What `pg_upgrade` will be told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgUpgradePlan {
    /// `-b`. The **wrapper** directory, not the staged binaries
    /// themselves: the wrappers are what scope `LD_LIBRARY_PATH` to the
    /// old installation (ADR 06 §3).
    pub old_bindir: String,
    /// `-B`, in the image this runs in.
    pub new_bindir: String,
    pub old_data: String,
    pub new_data: String,
    /// Where the two servers put their unix sockets. Inside the volume,
    /// like every other socket pgpod uses, because the path has to be
    /// writable and short.
    pub socket_dir: String,
    pub method: UpgradeMethod,
    pub jobs: u32,
    /// `shared_preload_libraries` for the new server.
    pub new_preload: Vec<String>,
    /// Check only, changing nothing.
    pub check_only: bool,
}

impl PgUpgradePlan {
    /// Full argv, including the program.
    pub fn argv(&self) -> Vec<String> {
        let mut v = vec![
            format!("{}/pg_upgrade", self.new_bindir.trim_end_matches('/')),
            format!("--old-bindir={}", self.old_bindir),
            format!("--new-bindir={}", self.new_bindir),
            format!("--old-datadir={}", self.old_data),
            format!("--new-datadir={}", self.new_data),
            // Without this the sockets land in the current directory,
            // and a unix socket path is capped near 107 bytes.
            format!("--socketdir={}", self.socket_dir),
            format!("--jobs={}", self.jobs.max(1)),
            format!("--old-options={}", self.old_options()),
        ];
        if let Some(flag) = self.method.flag() {
            v.push(flag.to_string());
        }
        if !self.new_preload.is_empty() {
            v.push(format!("--new-options={}", self.new_options()));
        }
        if self.check_only {
            v.push("--check".to_string());
        }
        v
    }

    /// Options for the old server `pg_upgrade` starts.
    fn old_options(&self) -> String {
        // `archive_mode` is postmaster-level, so this is the only place it
        // can be turned off for the upgrade window — the cluster's own
        // conf.d, which the old data directory still includes, says `on`.
        //
        // `jit=off` pairs with the staging step: LLVM is ~200 MB of
        // shared libraries that exist only to compile expressions, which
        // `pg_upgrade`'s catalog queries never need. Staging it would
        // quadruple the copy; leaving it out without this would turn a
        // fast upgrade into a `could not load library "llvmjit.so"`.
        "-c archive_mode=off -c jit=off".to_string()
    }

    fn new_options(&self) -> String {
        format!("-c shared_preload_libraries={}", self.new_preload.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> PgUpgradePlan {
        PgUpgradePlan {
            old_bindir: "/pgdata/upgrade/bin".into(),
            new_bindir: "/usr/lib/postgresql/18/bin".into(),
            old_data: "/pgdata/pgdata".into(),
            new_data: "/pgdata/pgdata.new".into(),
            socket_dir: "/pgdata/run".into(),
            method: UpgradeMethod::Link,
            jobs: 2,
            new_preload: Vec::new(),
            check_only: false,
        }
    }

    fn joined(p: &PgUpgradePlan) -> String {
        p.argv().join(" ")
    }

    #[test]
    fn pg_upgrade_comes_from_the_new_installation() {
        // The binary that drives the upgrade is always the *new* one:
        // pg_upgrade knows how to read the version below it, never the
        // one above.
        assert_eq!(plan().argv()[0], "/usr/lib/postgresql/18/bin/pg_upgrade");
    }

    #[test]
    fn the_old_server_neither_archives_nor_jits() {
        let out = joined(&plan());
        assert!(
            out.contains("--old-options=-c archive_mode=off -c jit=off"),
            "{out}"
        );
    }

    #[test]
    fn link_is_requested_explicitly_and_copy_is_not() {
        assert!(joined(&plan()).contains("--link"));
        let copy = PgUpgradePlan {
            method: UpgradeMethod::Copy,
            ..plan()
        };
        assert!(
            !joined(&copy).contains("--link"),
            "copy must not smuggle in link: it is the difference between a \
             reversible upgrade and a one-way one"
        );
    }

    #[test]
    fn the_old_bindir_is_the_wrapper_dir_not_the_staged_binaries() {
        // The wrappers are what set LD_LIBRARY_PATH for the old
        // installation only. Pointing -b at the staged binaries directly
        // works on an image whose libraries happen to match and fails on
        // one where they do not — the failure this indirection exists for.
        let out = joined(&plan());
        assert!(out.contains("--old-bindir=/pgdata/upgrade/bin"), "{out}");
    }

    #[test]
    fn preload_libraries_reach_the_new_server_only_when_there_are_any() {
        assert!(!joined(&plan()).contains("--new-options"));
        let preload = PgUpgradePlan {
            new_preload: vec!["pg_stat_statements".into(), "vector".into()],
            ..plan()
        };
        assert!(
            joined(&preload)
                .contains("--new-options=-c shared_preload_libraries=pg_stat_statements,vector"),
            "{}",
            joined(&preload)
        );
    }

    #[test]
    fn jobs_is_never_zero() {
        // `--jobs=0` is rejected by pg_upgrade, and a spec that defaulted
        // the field would otherwise fail at the worst possible moment.
        let zero = PgUpgradePlan { jobs: 0, ..plan() };
        assert!(joined(&zero).contains("--jobs=1"));
    }

    #[test]
    fn check_only_changes_nothing_and_says_so() {
        let check = PgUpgradePlan {
            check_only: true,
            ..plan()
        };
        assert!(joined(&check).contains("--check"));
    }

    #[test]
    fn the_socket_directory_is_passed_explicitly() {
        // Left out, pg_upgrade puts sockets in the current directory, and
        // a unix socket path is capped near 107 bytes.
        assert!(joined(&plan()).contains("--socketdir=/pgdata/run"));
    }
}
