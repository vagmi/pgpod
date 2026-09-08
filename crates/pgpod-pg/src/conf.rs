//! `postgresql.conf` rendering.
//!
//! pgpod never edits `postgresql.conf` in place. `initdb` output gains one
//! appended line — `include_dir 'conf.d'` — and pgpod owns two files under
//! that directory, rewritten idempotently on every instance start:
//!
//! * `00-pgpod.conf` — pgpod-managed, non-negotiable
//! * `10-user.conf`  — rendered from `spec.postgresql.parameters`
//!
//! Later files win in PostgreSQL's include order, so a user parameter
//! overrides a managed one of the same name. That is deliberate for tuning
//! knobs like `shared_buffers` — and dangerous for the handful of settings
//! that keep the cluster recoverable, which is why [`RESERVED_PARAMETERS`]
//! exists.

use std::fmt::Write as _;

use crate::Error;

/// The line appended to `postgresql.conf` exactly once at bootstrap.
pub const INCLUDE_DIR_LINE: &str = "include_dir 'conf.d'";

pub const MANAGED_CONF_FILE: &str = "00-pgpod.conf";
pub const USER_CONF_FILE: &str = "10-user.conf";

/// Parameters a user may not set in `spec.postgresql.parameters`.
///
/// These are not "pgpod prefers otherwise" — each one, set wrongly, either
/// breaks recovery permanently or makes the instance unmanageable:
///
/// * `wal_level`, `archive_mode`, `archive_command` — the archive is the
///   durability story (`AGENTS.md` principle 5).
/// * `wal_log_hints` — cannot be enabled later without a rebuild, and
///   `pg_rewind` is impossible without it (ADR 02 §5).
/// * `hot_standby`, `max_wal_senders`, `max_replication_slots` — replicas
///   silently stop working.
/// * `listen_addresses`, `port`, `unix_socket_directories` — pgpod would
///   lose the ability to reach its own instance.
/// * `restore_command`, `primary_conninfo`, `primary_slot_name`,
///   `recovery_target*` — owned by the bootstrap and failover machinery.
pub const RESERVED_PARAMETERS: &[&str] = &[
    "archive_command",
    "archive_mode",
    "hot_standby",
    "listen_addresses",
    "max_replication_slots",
    "max_wal_senders",
    "port",
    "primary_conninfo",
    "primary_slot_name",
    "recovery_target",
    "recovery_target_action",
    "recovery_target_lsn",
    "recovery_target_name",
    "recovery_target_time",
    "recovery_target_timeline",
    "recovery_target_xid",
    "restore_command",
    "unix_socket_directories",
    "wal_level",
    "wal_log_hints",
];

/// `archive_command`, as PostgreSQL will run it.
///
/// Points at pgBackRest **directly**, not at a pgpod shim that shells out
/// to it (ADR 04 §1). A wrapper process on the archive path buys nothing
/// and adds a failure mode in the one place where a wrong exit status is a
/// silent data-loss bug.
///
/// Absolute paths throughout, because `archive_command` runs through the
/// shell with the postmaster's `PATH` — which belongs to the image and
/// varies between image families. `--config` is explicit for the same
/// reason: nothing here should depend on a search order pgpod does not
/// control.
pub fn archive_command(stanza: &str) -> String {
    format!(
        "{} --config={} --stanza={stanza} archive-push %p",
        pgpod_core::container::PGBACKREST_BIN,
        pgpod_core::container::PGBACKREST_CONF,
    )
}

/// `restore_command`, as PostgreSQL will run it.
///
/// `%p` is quoted because PostgreSQL substitutes a path that, while it has
/// never contained a space in practice, is not pgpod's to guarantee.
pub fn restore_command(stanza: &str) -> String {
    format!(
        "{} --config={} --stanza={stanza} archive-get %f \"%p\"",
        pgpod_core::container::PGBACKREST_BIN,
        pgpod_core::container::PGBACKREST_CONF,
    )
}

/// WAL archiving state for an instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveMode {
    /// No backup destination is configured.
    ///
    /// pgpod renders `archive_mode = off` rather than pointing
    /// `archive_command` at something that always succeeds. A command that
    /// returns 0 without durably storing the segment tells PostgreSQL it
    /// may recycle WAL that was never archived — the exact silent
    /// data-loss shape ADR 01 §1 exists to prevent.
    ///
    /// Note for the reconciler: `archive_mode` is postmaster-level, so
    /// turning archiving on later requires an instance **restart**, not a
    /// reload.
    Off,
    /// Archive through the agent.
    On { command: String },
}

/// Everything the managed config file depends on.
#[derive(Debug, Clone)]
pub struct ManagedConf {
    pub port: u16,
    pub archive: ArchiveMode,
    pub shared_preload_libraries: Vec<String>,
    /// Rendered on a standby; `None` on a primary.
    pub standby: Option<StandbyConf>,
    /// Rendered while an instance is recovering from the archive; `None`
    /// otherwise.
    pub recovery: Option<RecoveryConf>,
}

/// Settings that only apply while an instance is replaying the archive.
#[derive(Debug, Clone)]
pub struct RecoveryConf {
    /// How to fetch a segment. Points at the *source* cluster's archive,
    /// which is not the same as where this instance will archive once it
    /// is promoted (ADR 01 §5).
    pub restore_command: String,
    /// `recovery_target_time`, RFC 3339. `None` replays everything
    /// available.
    pub target_time: Option<String>,
}

/// Settings that only apply while an instance is following another.
#[derive(Debug, Clone)]
pub struct StandbyConf {
    pub primary_conninfo: String,
    pub primary_slot_name: String,
    /// Archive fallback, so a standby that outruns its slot recovers from
    /// object storage instead of needing a rebuild (ADR 02 §1).
    pub restore_command: Option<String>,
}

impl ManagedConf {
    pub fn primary(port: u16) -> Self {
        Self {
            port,
            archive: ArchiveMode::Off,
            shared_preload_libraries: Vec::new(),
            standby: None,
            recovery: None,
        }
    }

    pub fn with_recovery(mut self, recovery: Option<RecoveryConf>) -> Self {
        self.recovery = recovery;
        self
    }

    pub fn with_archive(mut self, archive: ArchiveMode) -> Self {
        self.archive = archive;
        self
    }

    pub fn with_shared_preload_libraries(mut self, libs: Vec<String>) -> Self {
        self.shared_preload_libraries = libs;
        self
    }

    /// Render `conf.d/00-pgpod.conf`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "# Managed by pgpod. Rewritten on every instance start.\n\
             # Edits here are lost; put overrides in spec.postgresql.parameters.\n\n",
        );

        out.push_str("# --- connectivity ---\n");
        push(&mut out, "listen_addresses", quote("*"));
        push(&mut out, "port", self.port.to_string());
        push(
            &mut out,
            "unix_socket_directories",
            quote(pgpod_core::container::SOCKET_DIR),
        );

        out.push_str("\n# --- write-ahead log ---\n");
        push(&mut out, "wal_level", quote("replica"));
        // Required for pg_rewind, and impossible to enable after the fact
        // without a rebuild (ADR 02 §5). Never conditional.
        push(&mut out, "wal_log_hints", "on");
        push(&mut out, "max_wal_senders", "10");
        push(&mut out, "max_replication_slots", "10");

        out.push_str("\n# --- archiving ---\n");
        match &self.archive {
            ArchiveMode::Off => {
                out.push_str(
                    "# No backup destination configured. archive_mode stays off rather\n\
                     # than archiving to nowhere — turning it on needs a restart.\n",
                );
                push(&mut out, "archive_mode", "off");
            }
            ArchiveMode::On { command } => {
                push(&mut out, "archive_mode", "on");
                push(&mut out, "archive_command", quote(command));
                push(&mut out, "archive_timeout", quote("5min"));
            }
        }

        out.push_str("\n# --- replication ---\n");
        push(&mut out, "hot_standby", "on");
        push(&mut out, "hot_standby_feedback", "on");

        out.push_str("\n# --- supervision ---\n");
        // The reconciler owns restarts. Letting postgres restart itself
        // after a crash hides the event from pgpod's probes.
        push(&mut out, "restart_after_crash", "off");
        push(&mut out, "logging_collector", "off");
        push(&mut out, "log_destination", quote("stderr"));

        if !self.shared_preload_libraries.is_empty() {
            out.push_str("\n# --- extensions ---\n");
            push(
                &mut out,
                "shared_preload_libraries",
                quote(&self.shared_preload_libraries.join(",")),
            );
        }

        if let Some(recovery) = &self.recovery {
            out.push_str("\n# --- recovery ---\n");
            push(
                &mut out,
                "restore_command",
                quote(&recovery.restore_command),
            );
            match &recovery.target_time {
                Some(t) => {
                    push(&mut out, "recovery_target_time", quote(t));
                    // Without this PostgreSQL pauses at the target and
                    // waits for an operator, which for pgpod means an
                    // instance that never becomes ready and a `restore`
                    // that appears to hang (ADR 01 §5 step 4).
                    push(&mut out, "recovery_target_action", quote("promote"));
                    // Never `latest`: after a promote the archive holds
                    // more than one timeline, and following the newest one
                    // would replay history the operator did not ask for.
                    push(&mut out, "recovery_target_timeline", quote("current"));
                }
                None => {
                    out.push_str(
                        "# No recovery target: replay everything the archive has, then\n\
                         # promote. This is what `pgpod fork` asks for.\n",
                    );
                    push(&mut out, "recovery_target_timeline", quote("current"));
                }
            }
        }

        if let Some(standby) = &self.standby {
            out.push_str("\n# --- standby ---\n");
            push(
                &mut out,
                "primary_conninfo",
                quote(&standby.primary_conninfo),
            );
            push(
                &mut out,
                "primary_slot_name",
                quote(&standby.primary_slot_name),
            );
            if let Some(rc) = &standby.restore_command {
                push(&mut out, "restore_command", quote(rc));
            }
        }

        out
    }
}

/// Render `conf.d/10-user.conf` from user-supplied parameters.
///
/// Returns [`Error::ReservedParameter`] rather than silently dropping a
/// reserved key: a user who set `archive_mode = off` in their manifest and
/// saw it ignored would reasonably believe archiving was off.
pub fn render_user_conf(params: &[(String, String)]) -> Result<String, Error> {
    let mut out = String::from(
        "# Rendered by pgpod from spec.postgresql.parameters.\n\
         # Included after 00-pgpod.conf, so these win where they overlap.\n\n",
    );

    // Sorted so the file is stable across reconciles — an unstable
    // rendering makes every reconcile look like a config change and
    // triggers pointless reloads.
    let mut sorted: Vec<_> = params.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    for (key, value) in sorted {
        let normalized = key.trim().to_ascii_lowercase();
        if RESERVED_PARAMETERS.contains(&normalized.as_str()) {
            return Err(Error::ReservedParameter(normalized));
        }
        if !is_valid_parameter_name(&normalized) {
            return Err(Error::InvalidParameterName(key.clone()));
        }
        push(&mut out, &normalized, quote(value));
    }

    Ok(out)
}

/// PostgreSQL GUC names are `[A-Za-z][A-Za-z0-9_]*`. Anything else would
/// let a crafted manifest inject arbitrary config lines.
fn is_valid_parameter_name(name: &str) -> bool {
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Quote a value as a PostgreSQL config string literal.
///
/// Single quotes are escaped by doubling, which is what the config parser
/// expects. Without this, a password or path containing `'` terminates the
/// literal early and the rest of it is parsed as configuration — a config
/// injection, and a startup failure at best.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn push(out: &mut String, key: &str, value: impl AsRef<str>) {
    let _ = writeln!(out, "{key} = {}", value.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered_value<'a>(conf: &'a str, key: &str) -> Option<&'a str> {
        conf.lines()
            .find(|l| l.starts_with(&format!("{key} = ")))
            .map(|l| l.split_once(" = ").unwrap().1)
    }

    #[test]
    fn managed_conf_always_sets_the_settings_recovery_depends_on() {
        // wal_log_hints and wal_level cannot be fixed after the fact
        // without rebuilding the instance, so their presence is not a
        // preference — it is the reason failover is possible at all.
        let conf = ManagedConf::primary(5432).render();
        assert_eq!(rendered_value(&conf, "wal_log_hints"), Some("on"));
        assert_eq!(rendered_value(&conf, "wal_level"), Some("'replica'"));
        assert_eq!(rendered_value(&conf, "hot_standby"), Some("on"));
        assert_eq!(rendered_value(&conf, "restart_after_crash"), Some("off"));
    }

    #[test]
    fn no_destination_means_archive_mode_off_not_a_no_op_command() {
        // Pointing archive_command at /bin/true would let postgres recycle
        // WAL it never archived (ADR 01 §1).
        let conf = ManagedConf::primary(5432).render();
        assert_eq!(rendered_value(&conf, "archive_mode"), Some("off"));
        assert!(
            rendered_value(&conf, "archive_command").is_none(),
            "no archive_command may be rendered when archiving is off"
        );
    }

    #[test]
    fn archive_on_renders_the_agent_command() {
        let conf = ManagedConf::primary(5432)
            .with_archive(ArchiveMode::On {
                command: "pgpod-agent wal archive %p".into(),
            })
            .render();
        assert_eq!(rendered_value(&conf, "archive_mode"), Some("on"));
        assert_eq!(
            rendered_value(&conf, "archive_command"),
            Some("'pgpod-agent wal archive %p'")
        );
    }

    #[test]
    fn standby_settings_appear_only_on_a_standby() {
        let primary = ManagedConf::primary(5432).render();
        assert!(rendered_value(&primary, "primary_conninfo").is_none());

        let mut standby = ManagedConf::primary(5432);
        standby.standby = Some(StandbyConf {
            primary_conninfo: "host=pgpod-mydb-1 user=streaming_replica".into(),
            primary_slot_name: "pgpod_mydb_2".into(),
            restore_command: Some("pgpod-agent wal restore %f %p".into()),
        });
        let conf = standby.render();
        assert_eq!(
            rendered_value(&conf, "primary_conninfo"),
            Some("'host=pgpod-mydb-1 user=streaming_replica'")
        );
        assert_eq!(
            rendered_value(&conf, "primary_slot_name"),
            Some("'pgpod_mydb_2'")
        );
    }

    #[test]
    fn single_quotes_are_doubled_not_dropped() {
        // A value containing `'` would otherwise terminate the literal and
        // turn the remainder into config directives.
        assert_eq!(quote("it's"), "'it''s'");
        assert_eq!(quote("a'b'c"), "'a''b''c'");
        assert_eq!(quote("plain"), "'plain'");
    }

    #[test]
    fn a_quote_in_a_conninfo_cannot_inject_config() {
        let mut c = ManagedConf::primary(5432);
        c.standby = Some(StandbyConf {
            // A password field containing a quote and a newline is the
            // realistic injection vector.
            primary_conninfo: "host=h password=x'\nssl=off".into(),
            primary_slot_name: "s".into(),
            restore_command: None,
        });
        let conf = c.render();
        assert!(
            conf.contains("password=x''"),
            "quote must be doubled:\n{conf}"
        );
        assert!(
            !conf.lines().any(|l| l.trim_start().starts_with("ssl = ")),
            "injected directive escaped the literal:\n{conf}"
        );
    }

    #[test]
    fn user_parameters_render_sorted_for_a_stable_file() {
        // An unstable rendering makes every reconcile look like a change.
        let params = vec![
            ("work_mem".to_string(), "16MB".to_string()),
            ("shared_buffers".to_string(), "512MB".to_string()),
        ];
        let conf = render_user_conf(&params).unwrap();
        let shared = conf.find("shared_buffers").unwrap();
        let work = conf.find("work_mem").unwrap();
        assert!(shared < work, "parameters must be sorted:\n{conf}");
        assert_eq!(
            render_user_conf(&params).unwrap(),
            conf,
            "must be deterministic"
        );
    }

    #[test]
    fn reserved_parameters_are_rejected_loudly_not_dropped() {
        // Silently ignoring `archive_mode = off` would leave the operator
        // believing archiving was disabled when it was not.
        for key in [
            "archive_mode",
            "wal_level",
            "WAL_LOG_HINTS",
            " restore_command ",
        ] {
            let params = vec![(key.to_string(), "whatever".to_string())];
            assert!(
                matches!(render_user_conf(&params), Err(Error::ReservedParameter(_))),
                "{key} must be rejected"
            );
        }
    }

    #[test]
    fn ordinary_tuning_parameters_are_allowed() {
        let params = vec![
            ("shared_buffers".to_string(), "512MB".to_string()),
            ("max_connections".to_string(), "200".to_string()),
        ];
        let conf = render_user_conf(&params).unwrap();
        assert_eq!(rendered_value(&conf, "shared_buffers"), Some("'512MB'"));
        assert_eq!(rendered_value(&conf, "max_connections"), Some("'200'"));
    }

    #[test]
    fn parameter_names_cannot_smuggle_in_extra_directives() {
        for bad in [
            "shared_buffers = 1MB\narchive_mode",
            "foo bar",
            "9lives",
            "",
            "wal-level",
        ] {
            let params = vec![(bad.to_string(), "x".to_string())];
            assert!(
                render_user_conf(&params).is_err(),
                "{bad:?} must be rejected as a parameter name"
            );
        }
    }

    #[test]
    fn shared_preload_libraries_join_with_commas() {
        let conf = ManagedConf::primary(5432)
            .with_shared_preload_libraries(vec!["pg_stat_statements".into(), "pgaudit".into()])
            .render();
        assert_eq!(
            rendered_value(&conf, "shared_preload_libraries"),
            Some("'pg_stat_statements,pgaudit'")
        );
    }

    #[test]
    fn every_reserved_parameter_is_lowercase_and_sorted() {
        // The lookup lowercases its input, so an uppercase entry here
        // would be unreachable and silently permit a reserved key.
        let mut sorted = RESERVED_PARAMETERS.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            sorted, RESERVED_PARAMETERS,
            "keep the list sorted for review"
        );
        for p in RESERVED_PARAMETERS {
            assert_eq!(*p, p.to_ascii_lowercase(), "{p} must be lowercase");
        }
    }
}
