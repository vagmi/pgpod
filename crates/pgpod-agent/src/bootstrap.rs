//! Bringing an instance's PGDATA into existence and keeping its config
//! in sync.
//!
//! Everything here runs inside the container, because with the volume
//! storage model that is the only place a PGDATA can be written at all
//! (ADR 00 §4, §6).

use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use pgpod_core::{Bootstrap, InstanceSpec, Secret, container};
use pgpod_pg::{
    AppDatabase, ArchiveMode, BootstrapRoles, HbaConfig, INCLUDE_DIR_LINE, InitdbOptions,
    MANAGED_CONF_FILE, ManagedConf, PWFILE_PATH, USER_CONF_FILE, render_user_conf,
};

use crate::psql;
use crate::secrets::InstanceSecrets;
use crate::{info, warn};

/// PGDATA and everything beside it must be 0700 or PostgreSQL refuses to
/// start. The stock image entrypoint does this; pgpod bypasses the
/// entrypoint, so the agent must.
const DIR_MODE: u32 = 0o700;

/// Create the directory layout inside the volume.
pub fn ensure_layout() -> Result<()> {
    for dir in [
        container::PGDATA,
        container::CONF_DIR,
        container::SPOOL_DIR,
        // Inside the volume rather than the image's own
        // /var/run/postgresql — see container::SOCKET_DIR.
        container::SOCKET_DIR,
    ] {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {dir}"))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))
            .with_context(|| format!("failed to chmod {dir} to 0700"))?;
    }

    Ok(())
}

/// Run `initdb` if PGDATA is empty. Returns whether it actually ran.
pub fn initdb_if_needed(spec: &InstanceSpec, secrets: &InstanceSecrets) -> Result<bool> {
    let pgdata = Path::new(container::PGDATA);
    if psql::is_initialised(pgdata) {
        info!("PGDATA already initialised, skipping initdb");
        return Ok(false);
    }

    let Bootstrap::Initdb(init) = &spec.bootstrap;

    let opts = InitdbOptions {
        superuser: "postgres".to_string(),
        encoding: init.encoding.clone(),
        locale: init.locale.clone(),
        extra: init.options.clone(),
    };

    write_pwfile(&secrets.superuser)?;
    // Unlink the password file whichever way initdb goes — leaving a
    // cleartext superuser password in the volume would outlive the
    // bootstrap by the lifetime of the cluster.
    let result = run_initdb(&opts);
    let _ = fs::remove_file(PWFILE_PATH);
    result?;

    // PostgreSQL reads `include_dir` relative to the data directory, so
    // this one line is what makes conf.d/ take effect at all.
    append_include_dir(pgdata)?;

    info!("initdb complete");
    Ok(true)
}

fn write_pwfile(password: &Secret) -> Result<()> {
    use std::io::Write as _;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        // 0600 before any bytes are written, not chmod'd afterwards —
        // otherwise there is a window where the password is readable.
        .mode(0o600)
        .open(PWFILE_PATH)
        .with_context(|| format!("failed to create {PWFILE_PATH}"))?;
    f.write_all(password.expose().as_bytes())
        .context("failed to write the initdb password file")?;
    Ok(())
}

fn run_initdb(opts: &InitdbOptions) -> Result<()> {
    let argv = opts.argv();
    info!("running initdb with --data-checksums");
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .context("failed to run initdb — is it on PATH in this image?")?;
    if !status.success() {
        bail!("initdb failed with {status}");
    }
    Ok(())
}

/// Append `include_dir 'conf.d'` to postgresql.conf, exactly once.
fn append_include_dir(pgdata: &Path) -> Result<()> {
    use std::io::Write as _;
    let path = pgdata.join("postgresql.conf");
    let existing =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;

    // Idempotent: bootstrap can be re-entered after a crash, and a
    // duplicated include_dir makes PostgreSQL warn on every start.
    if existing.lines().any(|l| l.trim() == INCLUDE_DIR_LINE) {
        return Ok(());
    }

    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {} for append", path.display()))?;
    writeln!(
        f,
        "\n# Added by pgpod. Configuration lives in conf.d/.\n{INCLUDE_DIR_LINE}"
    )
    .context("failed to append include_dir")?;
    Ok(())
}

/// Render every pgpod-managed config file. Idempotent — called on every
/// start, not just after bootstrap, so a spec change takes effect on
/// restart without special-casing.
pub fn render_config(spec: &InstanceSpec) -> Result<()> {
    let conf_d = Path::new(container::PGDATA).join("conf.d");
    fs::create_dir_all(&conf_d)
        .with_context(|| format!("failed to create {}", conf_d.display()))?;

    let archive = match &spec.archive_command {
        Some(cmd) => ArchiveMode::On {
            command: cmd.clone(),
        },
        None => ArchiveMode::Off,
    };
    let managed = ManagedConf::primary(spec.port)
        .with_archive(archive)
        .with_shared_preload_libraries(spec.shared_preload_libraries.clone());

    write_atomically(&conf_d.join(MANAGED_CONF_FILE), &managed.render())?;

    let user_conf = render_user_conf(&spec.parameters)
        .context("spec.postgresql.parameters could not be rendered")?;
    write_atomically(&conf_d.join(USER_CONF_FILE), &user_conf)?;

    let mut hba = HbaConfig::new(pgpod_pg::REPLICATION_ROLE);
    if let Some(cidr) = &spec.network_cidr {
        hba = hba.with_network(cidr.clone());
    }
    write_atomically(
        &Path::new(container::PGDATA).join("pg_hba.conf"),
        &hba.render(),
    )?;

    info!("configuration rendered");
    Ok(())
}

/// Write via a temp file and rename.
///
/// A crash midway through rewriting `pg_hba.conf` in place would leave a
/// truncated file, and PostgreSQL would either refuse every connection or
/// refuse to start. Rename is atomic within a filesystem, and conf.d is on
/// the same volume as its target by construction.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("pgpod-tmp");
    fs::write(&tmp, contents).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to move {} into place", path.display()))?;
    Ok(())
}

/// Create pgpod's roles and the application database.
///
/// Runs against a PostgreSQL started on the unix socket only — see
/// [`crate::supervise::with_local_postgres`]. Idempotent.
pub fn create_roles(spec: &InstanceSpec, secrets: &InstanceSecrets) -> Result<()> {
    let Bootstrap::Initdb(init) = &spec.bootstrap;

    let app = match (&init.database, &init.owner) {
        (Some(database), Some(owner)) => Some(AppDatabase {
            database: database.clone(),
            owner: owner.clone(),
            owner_password: secrets.app_owner.clone().unwrap_or_else(|| {
                // A spec that asks for an app database without supplying
                // an owner secret is a daemon bug. Failing here would
                // leave a half-bootstrapped instance, so fall back to the
                // superuser password and say so loudly.
                warn!("no app owner secret provided; reusing the superuser password");
                secrets.superuser.clone()
            }),
        }),
        (Some(_), None) | (None, Some(_)) => {
            bail!("spec.bootstrap.initdb needs both `database` and `owner`, or neither")
        }
        (None, None) => None,
    };

    let roles = BootstrapRoles {
        superuser: "postgres".to_string(),
        superuser_password: secrets.superuser.clone(),
        replication_password: secrets.replication.clone(),
        monitor_password: secrets.monitor.clone(),
        app,
    };

    for stmt in roles
        .statements()
        .context("failed to render bootstrap SQL")?
    {
        // `what` rather than the statement itself: these contain passwords.
        psql::execute("postgres", &stmt, "role bootstrap")?;
    }

    if let Some((check, create)) = roles.app_database_sql()? {
        if psql::query("postgres", &check)?.trim().is_empty() {
            psql::execute("postgres", &create, "create application database")?;
            info!("application database created");
        } else {
            info!("application database already exists");
        }
    }

    info!("roles and database ready");
    Ok(())
}
