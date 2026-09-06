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

impl InitdbOptions {
    /// Full argv for `initdb`, including the program name.
    ///
    /// Empty `encoding` or `locale` fall back to the defaults rather than
    /// being passed through. `--encoding=` is a hard initdb error, but
    /// `--locale=` *succeeds* and silently inherits the container's
    /// environment — producing a glibc-dependent collation where `C` was
    /// intended. A setting that fails safe is worth the two lines.
    pub fn argv(&self) -> Vec<String> {
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
            container::PGDATA.to_string(),
            format!("--username={}", self.superuser),
            format!("--encoding={encoding}"),
            format!("--locale={locale}"),
            // Not negotiable — see the module docs.
            "--data-checksums".to_string(),
            // Local connections are `peer`; TCP is scram. Matches the
            // pg_hba pgpod renders immediately afterwards.
            "--auth-local=peer".to_string(),
            "--auth-host=scram-sha-256".to_string(),
            format!("--pwfile={PWFILE_PATH}"),
        ];
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
}
