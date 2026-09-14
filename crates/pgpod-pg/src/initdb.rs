//! `initdb` invocation.
//!
//! The flags here are the one genuinely irreversible decision pgpod makes.
//! `--data-checksums` and the `wal_log_hints` that accompanies it (see
//! `conf.rs`) are prerequisites for `pg_rewind`, and neither can be turned
//! on afterwards without rebuilding the instance from scratch. A cluster
//! created without them can never rejoin a demoted primary.

use pgpod_core::container;

/// Where the superuser password is handed to `initdb`.
///
/// A file, not `--pwprompt` and not an argument: process arguments are
/// world-readable through `/proc/<pid>/cmdline` for the lifetime of the
/// call, and the container has other processes in it. The agent writes
/// this at mode 0600 and unlinks it as soon as `initdb` returns.
pub const PWFILE_PATH: &str = "/pgdata/conf/.initdb-pw";

#[derive(Debug, Clone)]
pub struct InitdbOptions {
    pub superuser: String,
    pub encoding: String,
    pub locale: String,
    /// Extra flags from `spec.bootstrap.initdb.options`.
    pub extra: Vec<String>,
}

impl Default for InitdbOptions {
    fn default() -> Self {
        Self {
            superuser: "postgres".into(),
            encoding: "UTF8".into(),
            // `C` sorts deterministically and never changes under a glibc
            // upgrade. A locale change silently corrupts text indexes —
            // the collation-version problem — so the default is the one
            // that cannot drift. Operators who need linguistic sorting
            // set it explicitly and own the consequence.
            locale: "C".into(),
            extra: Vec::new(),
        }
    }
}

/// Whether the cluster stores data page checksums.
///
/// Not a preference on the upgrade path: `pg_upgrade` refuses when the
/// two clusters disagree, so the new cluster's setting is dictated by the
/// old one's `pg_controldata`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataChecksums {
    On,
    Off,
}

impl InitdbOptions {
    /// Full argv for bootstrapping a pgpod instance's own PGDATA.
    ///
    /// Checksums are on and the superuser password comes from a file —
    /// both non-negotiable here, which is why this is a distinct entry
    /// point from [`Self::argv_for`] rather than a defaulted call of it.
    pub fn argv(&self) -> Vec<String> {
        self.argv_for(container::PGDATA, Some(PWFILE_PATH), DataChecksums::On, 0)
    }

    /// Full argv for `initdb` into an arbitrary directory.
    ///
    /// `pg_upgrade` needs a second cluster created beside the first, with
    /// the same encoding and locale and **no** password file: the new
    /// cluster's roles arrive from the old one's dump, so writing a fresh
    /// superuser password here would be overwritten a minute later and
    /// would leave a cleartext password in the volume in the meantime.
    ///
    /// `target_major` is the version of the `initdb` being invoked. It
    /// matters for exactly one flag: PostgreSQL 18 turned checksums on by
    /// default and added `--no-data-checksums` to turn them off, and no
    /// earlier release accepts that spelling. Passing it to a 17 `initdb`
    /// is an error; omitting it on 18 silently produces a cluster
    /// `pg_upgrade` then refuses.
    ///
    /// Empty `encoding` or `locale` fall back to the defaults rather than
    /// being passed through. `--encoding=` is a hard initdb error, but
    /// `--locale=` *succeeds* and silently inherits the container's
    /// environment — producing a glibc-dependent collation where `C` was
    /// intended. A setting that fails safe is worth the two lines.
    pub fn argv_for(
        &self,
        pgdata: &str,
        pwfile: Option<&str>,
        checksums: DataChecksums,
        target_major: u32,
    ) -> Vec<String> {
        let encoding = if self.encoding.trim().is_empty() {
            "UTF8"
        } else {
            self.encoding.trim()
        };
        let locale = if self.locale.trim().is_empty() {
            "C"
        } else {
            self.locale.trim()
        };
        let mut v = vec![
            "initdb".to_string(),
            "--pgdata".to_string(),
            pgdata.to_string(),
            format!("--username={}", self.superuser),
            format!("--encoding={encoding}"),
            format!("--locale={locale}"),
            // Local connections are `peer`; TCP is scram. Matches the
            // pg_hba pgpod renders immediately afterwards.
            "--auth-local=peer".to_string(),
            "--auth-host=scram-sha-256".to_string(),
        ];
        match checksums {
            // Not negotiable on the bootstrap path — see the module docs.
            DataChecksums::On => v.push("--data-checksums".to_string()),
            // Only 18 and later have the flag, because only they default
            // the other way.
            DataChecksums::Off if target_major >= 18 => {
                v.push("--no-data-checksums".to_string());
            }
            DataChecksums::Off => {}
        }
        if let Some(pwfile) = pwfile {
            v.push(format!("--pwfile={pwfile}"));
        }
        v.extend(self.extra.iter().cloned());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_checksums_is_always_present() {
        // Without it pg_rewind is impossible and every failover degrades
        // to a full rebuild — and it cannot be added later.
        assert!(
            InitdbOptions::default()
                .argv()
                .contains(&"--data-checksums".to_string())
        );
    }

    #[test]
    fn no_extra_flag_can_remove_data_checksums() {
        // Extras are appended, and initdb has no --no-data-checksums, so
        // the guarantee holds. This test exists to fail loudly if the
        // ordering is ever changed to put extras first.
        let opts = InitdbOptions {
            extra: vec!["--no-sync".into()],
            ..Default::default()
        };
        let argv = opts.argv();
        let checksums = argv.iter().position(|a| a == "--data-checksums").unwrap();
        let extra = argv.iter().position(|a| a == "--no-sync").unwrap();
        assert!(
            checksums < extra,
            "managed flags must precede extras: {argv:?}"
        );
    }

    #[test]
    fn the_password_goes_through_a_file_not_the_command_line() {
        // /proc/<pid>/cmdline is readable by other processes in the
        // container for as long as initdb runs.
        let argv = InitdbOptions::default().argv();
        assert!(argv.iter().any(|a| a == &format!("--pwfile={PWFILE_PATH}")));
        assert!(
            !argv.iter().any(|a| a.contains("--pwprompt")),
            "must not prompt: there is no tty"
        );
    }

    #[test]
    fn initdb_targets_pgdata_below_the_volume_mount() {
        let argv = InitdbOptions::default().argv();
        let idx = argv.iter().position(|a| a == "--pgdata").unwrap();
        assert_eq!(argv[idx + 1], container::PGDATA);
        assert_ne!(
            argv[idx + 1],
            container::VOLUME_MOUNT,
            "must not initdb the mount point"
        );
    }

    #[test]
    fn auth_defaults_match_the_rendered_pg_hba() {
        let argv = InitdbOptions::default().argv();
        assert!(argv.iter().any(|a| a == "--auth-local=peer"));
        assert!(argv.iter().any(|a| a == "--auth-host=scram-sha-256"));
        assert!(
            !argv.iter().any(|a| a.contains("trust")),
            "trust auth must never be initdb'd in"
        );
    }

    #[test]
    fn locale_defaults_to_c_so_collation_cannot_drift() {
        assert_eq!(InitdbOptions::default().locale, "C");
    }

    #[test]
    fn empty_encoding_or_locale_never_reach_initdb() {
        // `--locale=` succeeds and inherits the environment's collation,
        // which is the silent-corruption case.
        let opts = InitdbOptions {
            encoding: String::new(),
            locale: "  ".into(),
            ..Default::default()
        };
        let argv = opts.argv();
        assert!(argv.contains(&"--encoding=UTF8".to_string()), "{argv:?}");
        assert!(argv.contains(&"--locale=C".to_string()), "{argv:?}");
        for a in &argv {
            assert!(!a.ends_with('='), "empty flag value reached initdb: {a:?}");
        }
    }

    #[test]
    fn a_second_data_directory_can_be_created_without_a_password_file() {
        // The pg_upgrade path: roles arrive from the old cluster's dump,
        // so a fresh password here would be both pointless and cleartext
        // in the volume until it was overwritten.
        let argv =
            InitdbOptions::default().argv_for("/pgdata/pgdata.new", None, DataChecksums::On, 18);
        let idx = argv.iter().position(|a| a == "--pgdata").unwrap();
        assert_eq!(argv[idx + 1], "/pgdata/pgdata.new");
        assert!(!argv.iter().any(|a| a.starts_with("--pwfile")), "{argv:?}");
        assert!(argv.contains(&"--data-checksums".to_string()));
    }

    #[test]
    fn checksums_off_only_spells_the_flag_versions_that_have_it_understand() {
        // PostgreSQL 18 defaults checksums *on* and added
        // --no-data-checksums; 17 defaults off and rejects the flag
        // outright. Getting this backwards produces either an initdb that
        // fails or a cluster pg_upgrade refuses for a checksum mismatch.
        let on_18 = InitdbOptions::default().argv_for("/d", None, DataChecksums::Off, 18);
        assert!(
            on_18.contains(&"--no-data-checksums".to_string()),
            "{on_18:?}"
        );

        let on_17 = InitdbOptions::default().argv_for("/d", None, DataChecksums::Off, 17);
        assert!(
            !on_17.iter().any(|a| a.contains("checksums")),
            "17 has no way to say it, and defaults to off: {on_17:?}"
        );
    }

    #[test]
    fn the_bootstrap_path_still_gets_checksums_and_a_password_file() {
        let argv = InitdbOptions::default().argv();
        assert!(argv.contains(&"--data-checksums".to_string()));
        assert!(argv.iter().any(|a| a == &format!("--pwfile={PWFILE_PATH}")));
    }
}
