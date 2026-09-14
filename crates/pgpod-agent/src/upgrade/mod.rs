//! Major-version upgrades, from inside the containers that can do them.
//!
//! Three entry points, each PID 1 of a short-lived job container and each
//! a different *image* (ADR 06):
//!
//! * [`stage`] runs in the **old** image, next to a running instance, and
//!   copies that image's PostgreSQL installation into the volume. No
//!   downtime: nothing is stopped, nothing is written outside
//!   [`container::UPGRADE_DIR`].
//! * [`probe`] runs in the **new** image and reports what it carries, so
//!   the control plane can refuse a downgrade or a no-op *before* it
//!   stops anything.
//! * [`run`] runs in the **new** image with the instance stopped, and is
//!   the only one that touches data.
//!
//! The division exists because `pg_upgrade` needs both installations at
//! once and no image has both. Everything hard about that is in
//! [`elf`] and in the wrapper scripts here.

mod elf;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use pgpod_core::{StageReport, UpgradeRunReport, UpgradeSpec, container, major_label};
use pgpod_pg::{DataChecksums, InitdbOptions, PgUpgradePlan};

use crate::{info, warn};

/// Directories inside the installation that are never worth staging.
///
/// `bitcode/` is LLVM IR for JIT inlining — tens of megabytes that only
/// `llvmjit.so` reads, and the upgrade runs both servers with `jit=off`
/// precisely so that neither is needed (see [`PgUpgradePlan`]).
const SKIP_DIRS: [&str; 1] = ["bitcode"];

/// Files whose dependencies are not followed.
///
/// `llvmjit.so` pulls in LLVM and Z3 — around 200 MB, or four times the
/// rest of the staged installation put together — to compile expressions
/// that `pg_upgrade`'s catalog queries never evaluate. The file itself is
/// still copied, because it lives inside `pkglibdir` which is staged
/// wholesale; it is simply never loaded.
const SKIP_DEPENDENCIES_OF: [&str; 1] = ["llvmjit"];

// ---- stage (old image) ------------------------------------------------

/// Copy this image's PostgreSQL installation into the instance volume.
pub fn stage() -> Result<()> {
    let bindir = pg_config("--bindir")?;
    let sharedir = pg_config("--sharedir")?;
    let pkglibdir = pg_config("--pkglibdir")?;
    let version = major_label(&pg_config("--version")?)
        .ok_or_else(|| anyhow::anyhow!("pg_config reported no version this agent understands"))?;

    // The data directory is right here, in the mounted volume. Comparing
    // against it rather than against something the control plane passes
    // in means the check cannot be skipped by a caller: staging the wrong
    // image's binaries surfaces as a `pg_upgrade` failure several steps
    // later that names neither image.
    let data_version = read_pg_version(Path::new(container::PGDATA))?;
    if data_version != version {
        bail!(
            "this image carries PostgreSQL {version}, but the cluster in the \
             volume is {data_version}. The staging image must be the one the \
             cluster is running — upgrade from the image it is on."
        );
    }

    let root = PathBuf::from(container::UPGRADE_STAGE_DIR);
    if root.exists() {
        // Its own scratch from an attempt that did not finish. Nothing
        // durable lives here: the installation is re-copied from the
        // image every time.
        info!("removing a staged installation left by an earlier attempt");
        fs::remove_dir_all(&root).with_context(|| format!("failed to clear {}", root.display()))?;
    }
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;

    let mut copied = Copied::default();
    for dir in [&bindir, &sharedir, &pkglibdir] {
        info!("staging {dir}");
        copy_tree(
            Path::new(dir),
            &mirrored(&root, Path::new(dir)),
            &mut copied,
        )?;
    }

    // The libraries the staged binaries name, as they exist in *this*
    // image — the whole reason an installation can be lifted out of one
    // image and run in another (ADR 06 §3).
    let roots = elf_files(&[Path::new(&bindir), Path::new(&pkglibdir)])?;
    let skip_below = [PathBuf::from(&bindir), PathBuf::from(&pkglibdir)];
    let libraries = elf::resolve_dependencies(&roots, &skip_below)?;
    info!("staging {} shared libraries", libraries.len());

    let mut lib_dirs: Vec<String> = Vec::new();
    for lib in &libraries {
        let target = mirrored(&root, lib);
        copy_file(lib, &target, &mut copied)?;
        if let Some(parent) = target.parent() {
            let parent = parent.display().to_string();
            if !lib_dirs.contains(&parent) {
                lib_dirs.push(parent);
            }
        }
    }
    // Extensions in pkglibdir link against their siblings, and the staged
    // copy is where those siblings now are.
    lib_dirs.push(mirrored(&root, Path::new(&pkglibdir)).display().to_string());

    let report = StageReport {
        version,
        bindir: mirrored(&root, Path::new(&bindir)).display().to_string(),
        sharedir: mirrored(&root, Path::new(&sharedir)).display().to_string(),
        pkglibdir: mirrored(&root, Path::new(&pkglibdir)).display().to_string(),
        lib_dirs,
        files: copied.files,
        bytes: copied.bytes,
    };
    info!(
        "staged {} files, {} MiB",
        report.files,
        report.bytes / (1024 * 1024)
    );
    pgpod_core::print_report(&report).map_err(anyhow::Error::msg)
}

// ---- probe (new image) ------------------------------------------------

/// Report what PostgreSQL this image carries, changing nothing.
pub fn probe() -> Result<()> {
    let version = major_label(&pg_config("--version")?)
        .ok_or_else(|| anyhow::anyhow!("pg_config reported no version this agent understands"))?;
    // The volume is mounted here too, so the same container can say what
    // is already on disk. `None` for a volume with no cluster in it: a
    // fresh one, which every first apply has.
    let data_version = read_pg_version(Path::new(container::PGDATA)).ok();
    let report = pgpod_core::ProbeReport {
        version,
        bindir: pg_config("--bindir")?,
        data_version,
    };
    pgpod_core::print_report(&report).map_err(anyhow::Error::msg)
}

// ---- run (new image, instance stopped) --------------------------------

/// Upgrade the cluster in the volume to this image's major version.
pub fn run() -> Result<()> {
    let spec = UpgradeSpec::from_env().context("could not read the upgrade spec")?;

    let pgdata = PathBuf::from(container::PGDATA);
    let new_data = PathBuf::from(container::PGDATA_NEW);

    // A swap that was interrupted between its two renames leaves no
    // PGDATA at all, and the new cluster — complete and upgraded —
    // sitting beside it. Finishing it is the only safe move: re-running
    // the upgrade is impossible (there is nothing to upgrade) and
    // reporting failure would leave a working cluster invisible.
    if !pgdata.exists() && is_cluster(&new_data) {
        warn!(
            "PGDATA is missing and an upgraded cluster is beside it — completing the interrupted swap"
        );
        fs::rename(&new_data, &pgdata).context("failed to complete the interrupted swap")?;
        let report = UpgradeRunReport {
            from_version: spec.from_version.clone(),
            to_version: spec.to_version.clone(),
            method: spec.method,
            old_data_dir: String::new(),
            seconds: 0,
        };
        return pgpod_core::print_report(&report).map_err(anyhow::Error::msg);
    }

    let data_version = read_pg_version(&pgdata)?;
    if data_version != spec.from_version {
        bail!(
            "the cluster in the volume is PostgreSQL {data_version}, but the \
             staged installation is {}. Nothing has been changed.",
            spec.from_version
        );
    }

    let here = major_label(&pg_config("--version")?).unwrap_or_default();
    if here != spec.to_version {
        bail!(
            "this image carries PostgreSQL {here}, but the upgrade was planned \
             for {}. Nothing has been changed.",
            spec.to_version
        );
    }

    // `pg_upgrade` puts the sockets of the two servers it starts here.
    // Every pgpod volume has it — the agent creates it on every instance
    // start — but this job runs without the agent's instance path, and a
    // missing socket directory surfaces as a connection failure to a
    // postmaster that started fine.
    fs::create_dir_all(container::SOCKET_DIR)
        .with_context(|| format!("failed to create {}", container::SOCKET_DIR))?;

    let wrappers = write_wrappers(&spec)?;
    verify_staged_binaries(&wrappers, &spec.from_version)?;

    let control = read_control_data(&wrappers, &pgdata)?;
    if !control.cleanly_shut_down {
        bail!(
            "the old cluster was not shut down cleanly (pg_controldata says \
             {:?}), and pg_upgrade cannot read a cluster that still needs \
             recovery. Start the instance, let it finish recovery, and stop it \
             again.",
            control.state
        );
    }

    if new_data.exists() {
        // Scratch from an attempt that failed before the swap. It is not
        // anybody's data: no instance has ever been started against it,
        // and the cluster that *is* data sits untouched at PGDATA.
        info!("removing an unfinished new cluster left by an earlier attempt");
        fs::remove_dir_all(&new_data)
            .with_context(|| format!("failed to clear {}", new_data.display()))?;
    }

    let initdb = InitdbOptions {
        superuser: "postgres".to_string(),
        encoding: spec.initdb.encoding.clone(),
        locale: spec.initdb.locale.clone(),
        extra: spec.initdb.options.clone(),
    };
    let target_major: u32 = spec.to_version.split('.').next().unwrap_or("0").parse()?;
    let argv = initdb.argv_for(container::PGDATA_NEW, None, control.checksums, target_major);
    info!("initdb for the new cluster: {}", argv.join(" "));
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .context("failed to run initdb — is it on PATH in this image?")?;
    if !status.success() {
        bail!("initdb for the new cluster failed with {status}");
    }

    let plan = PgUpgradePlan {
        old_bindir: wrappers.display().to_string(),
        new_bindir: pg_config("--bindir")?,
        old_data: container::PGDATA.to_string(),
        new_data: container::PGDATA_NEW.to_string(),
        socket_dir: container::SOCKET_DIR.to_string(),
        method: spec.method,
        jobs: spec.jobs,
        new_preload: spec.preload_libraries.clone(),
        check_only: spec.check,
    };
    let argv = plan.argv();
    info!("running {}", argv.join(" "));
    let started = std::time::Instant::now();
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        // pg_upgrade writes delete_old_cluster.sh and its working files
        // here. The container's rootfs is read-only, so this must be a
        // path inside the volume.
        .current_dir(container::UPGRADE_DIR)
        .status()
        .context("failed to run pg_upgrade")?;
    let seconds = started.elapsed().as_secs();

    if !status.success() {
        // The half-built new cluster is removed so a retry can start
        // clean. The old one has not been touched: every check
        // pg_upgrade makes happens before it moves a single file, and in
        // link mode the point of no return is announced in its output.
        let _ = fs::remove_dir_all(&new_data);
        bail!(
            "pg_upgrade failed with {status}. The cluster at {} is unchanged \
             — its own output above says why.",
            container::PGDATA
        );
    }

    if spec.check {
        info!("check passed; removing the trial cluster and changing nothing");
        fs::remove_dir_all(&new_data).context("failed to remove the trial cluster")?;
        let report = UpgradeRunReport {
            from_version: spec.from_version.clone(),
            to_version: spec.to_version.clone(),
            method: spec.method,
            old_data_dir: String::new(),
            seconds,
        };
        return pgpod_core::print_report(&report).map_err(anyhow::Error::msg);
    }

    // The swap. Two renames within one directory, so each is atomic and
    // the window where PGDATA does not exist is as short as a rename.
    let old_dir = format!(
        "{}{}-{}",
        container::PGDATA_OLD_PREFIX,
        spec.from_version,
        stamp()
    );
    fs::rename(&pgdata, &old_dir)
        .with_context(|| format!("failed to move the old cluster aside to {old_dir}"))?;
    if let Err(e) = fs::rename(&new_data, &pgdata) {
        // Put it back, so the failure leaves what it found.
        let _ = fs::rename(&old_dir, &pgdata);
        return Err(e).context("failed to move the upgraded cluster into place");
    }
    info!("upgraded cluster is in place; the old one is kept at {old_dir}");

    // `initdb` wrote a fresh postgresql.conf, so the line that makes
    // pgpod's conf.d take effect has to be added again. Without it the
    // instance comes back up ignoring every managed setting —
    // `archive_mode` included, which would be silent until the next
    // restore.
    //
    // The directory is created here as well, empty. `include_dir` naming
    // a directory that does not exist is **fatal**: PostgreSQL refuses to
    // start with `could not open configuration directory`. The agent
    // creates conf.d before every start, so pgpod's own path never sees
    // it — which is exactly what makes it a trap. An upgraded data
    // directory that only pgpod can start is not a data directory an
    // operator can debug. (Found by starting one by hand.)
    fs::create_dir_all(pgdata.join("conf.d"))
        .with_context(|| format!("failed to create {}/conf.d", pgdata.display()))?;
    crate::bootstrap::append_include_dir(&pgdata)?;

    // pgpod's own scratch, and nothing else: the staged installation and
    // the wrappers, re-created from the image on the next upgrade.
    if let Err(e) = fs::remove_dir_all(container::UPGRADE_DIR) {
        warn!("could not remove {}: {e}", container::UPGRADE_DIR);
    }

    let report = UpgradeRunReport {
        from_version: spec.from_version.clone(),
        to_version: spec.to_version.clone(),
        method: spec.method,
        old_data_dir: old_dir,
        seconds,
    };
    pgpod_core::print_report(&report).map_err(anyhow::Error::msg)
}

/// Write one wrapper per staged binary.
///
/// Each exports `LD_LIBRARY_PATH` for the staged libraries and execs the
/// real staged binary. Two properties matter and neither is incidental:
///
/// * the variable is set for the **old** binaries only, so the new
///   `pg_dump` and the new postmaster keep the new image's libraries —
///   an old `libpq` under a new `pg_dump` is the kind of mismatch that
///   fails somewhere else entirely;
/// * the wrapper `exec`s the real path, so `argv[0]` is the staged
///   binary. PostgreSQL finds `postgres` next to `pg_ctl` by looking
///   beside its own `argv[0]`, and finds its `share` directory by
///   stripping the compiled-in `bindir` from it — both of which need the
///   real, mirrored path rather than the wrapper's.
///
/// `/bin/sh` is not a new dependency: `pg_upgrade` runs every one of
/// these through `system()` already.
fn write_wrappers(spec: &UpgradeSpec) -> Result<PathBuf> {
    let dir = PathBuf::from(container::UPGRADE_WRAPPER_DIR);
    if dir.exists() {
        fs::remove_dir_all(&dir).with_context(|| format!("failed to clear {}", dir.display()))?;
    }
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    let library_path = spec.library_path();
    let staged = Path::new(&spec.staged_bindir);
    let mut count = 0;
    for entry in fs::read_dir(staged).with_context(|| {
        format!(
            "failed to read the staged bin directory {}",
            staged.display()
        )
    })? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let script = format!(
            "#!/bin/sh\n\
             # Written by pgpod-agent for a major-version upgrade (ADR 06 §3).\n\
             LD_LIBRARY_PATH='{library_path}'\n\
             export LD_LIBRARY_PATH\n\
             exec '{}' \"$@\"\n",
            entry.path().display()
        );
        let path = dir.join(&name);
        fs::write(&path, script).with_context(|| format!("failed to write {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        count += 1;
    }
    if count == 0 {
        bail!(
            "the staged bin directory {} is empty — the staging step did not \
             produce an installation",
            staged.display()
        );
    }
    Ok(dir)
}

/// Prove the staged binaries actually run **here**, before any data moves.
///
/// This is the whole safety net under the one assumption the design makes:
/// that the new image's glibc can run the old image's binaries. It holds
/// for a newer glibc running older binaries, which is every forward
/// upgrade between distribution releases — and does not hold between
/// libc families, where the failure would otherwise arrive with the old
/// cluster already renamed aside.
fn verify_staged_binaries(wrappers: &Path, expected: &str) -> Result<()> {
    let out = Command::new(wrappers.join("pg_ctl"))
        .arg("--version")
        .output()
        .with_context(|| format!("failed to run {}/pg_ctl", wrappers.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        bail!(
            "the staged PostgreSQL {expected} binaries cannot run in this image \
             ({}): {stderr}\n\
             This is the compatibility check, and it ran before anything was \
             changed — the cluster is untouched. It usually means the two \
             images do not share a libc (glibc and musl, say), or that the \
             new image's glibc is older than the old image's.",
            out.status
        );
    }
    match major_label(&stdout) {
        Some(v) if v == expected => {
            info!("staged binaries run here: {stdout}");
            Ok(())
        }
        other => bail!(
            "the staged binaries report {other:?}, not the expected \
             PostgreSQL {expected}"
        ),
    }
}

/// What the old cluster's control file says.
struct ControlData {
    state: String,
    cleanly_shut_down: bool,
    checksums: DataChecksums,
}

/// Read `pg_controldata` with the **old** binaries.
///
/// The new `pg_controldata` refuses a control file from an older catalog
/// version, which is exactly the file that has to be read here.
fn read_control_data(wrappers: &Path, pgdata: &Path) -> Result<ControlData> {
    let out = Command::new(wrappers.join("pg_controldata"))
        .arg("-D")
        .arg(pgdata)
        .output()
        .context("failed to run the staged pg_controldata")?;
    if !out.status.success() {
        bail!(
            "pg_controldata could not read {}: {}",
            pgdata.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |name: &str| -> Option<String> {
        text.lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == name))
            .map(|(_, v)| v.trim().to_string())
    };

    let state = field("Database cluster state").unwrap_or_else(|| "unknown".to_string());
    let checksums = match field("Data page checksum version").as_deref() {
        Some("0") => DataChecksums::Off,
        // Anything else is a checksum version, and pgpod's own clusters
        // are always version 1. Defaulting to "on" for an unreadable
        // field would make initdb disagree with the old cluster, which
        // pg_upgrade refuses — loudly, which is the right way round.
        _ => DataChecksums::On,
    };
    Ok(ControlData {
        // "shut down" is a clean stop; "shut down in recovery" is a clean
        // stop of a standby, which pg_upgrade also accepts. "in
        // production" means it crashed or is still running.
        cleanly_shut_down: state.starts_with("shut down"),
        state,
        checksums,
    })
}

// ---- helpers ----------------------------------------------------------

#[derive(Default)]
struct Copied {
    files: usize,
    bytes: u64,
}

/// The path `source` gets inside the staging root, with its absolute path
/// preserved.
///
/// `/usr/lib/postgresql/17/bin` becomes
/// `<root>/usr/lib/postgresql/17/bin`, and that shape is load-bearing:
/// PostgreSQL finds its `share` directory by stripping the compiled-in
/// `bindir` suffix off its own location, so the suffix has to still be
/// there (see [`container::UPGRADE_STAGE_DIR`]).
fn mirrored(root: &Path, source: &Path) -> PathBuf {
    root.join(source.strip_prefix("/").unwrap_or(source))
}

fn copy_tree(source: &Path, target: &Path, copied: &mut Copied) -> Result<()> {
    let meta =
        fs::metadata(source).with_context(|| format!("failed to stat {}", source.display()))?;
    if meta.is_file() {
        return copy_file(source, target, copied);
    }
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        if SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        // Metadata rather than the entry's own file type, so a symlink is
        // followed: a staged copy has to be self-contained, and a
        // symlink into a path that exists only in the old image would
        // dangle in the new one.
        let path = entry.path();
        match fs::metadata(&path) {
            Ok(m) if m.is_dir() => copy_tree(&path, &target.join(&name), copied)?,
            Ok(_) => copy_file(&path, &target.join(&name), copied)?,
            // A dangling symlink in the source image. It was already
            // broken there; carrying it over would only move the
            // confusion.
            Err(_) => continue,
        }
    }
    Ok(())
}

fn copy_file(source: &Path, target: &Path, copied: &mut Copied) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let bytes = fs::copy(source, target).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            target.display()
        )
    })?;
    // Executability is what makes a staged bin directory a bin directory.
    if let Ok(meta) = fs::metadata(source) {
        let mode = meta.permissions().mode() & 0o7777;
        let _ = fs::set_permissions(target, fs::Permissions::from_mode(mode));
    }
    copied.files += 1;
    copied.bytes += bytes;
    Ok(())
}

/// Every ELF file whose dependencies have to be resolved.
fn elf_files(dirs: &[&Path]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if SKIP_DEPENDENCIES_OF.iter().any(|s| name.starts_with(s)) {
                continue;
            }
            match fs::metadata(&path) {
                Ok(m) if m.is_file() => out.push(path),
                _ => continue,
            }
        }
    }
    Ok(out)
}

fn pg_config(flag: &str) -> Result<String> {
    let out = Command::new("pg_config")
        .arg(flag)
        .output()
        .context("failed to run pg_config — is it on PATH in this image?")?;
    if !out.status.success() {
        bail!(
            "pg_config {flag} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn read_pg_version(pgdata: &Path) -> Result<String> {
    let path = pgdata.join("PG_VERSION");
    let raw = fs::read_to_string(&path).with_context(|| {
        format!(
            "failed to read {} — is this a data directory?",
            path.display()
        )
    })?;
    major_label(&raw)
        .ok_or_else(|| anyhow::anyhow!("{} does not name a version: {raw:?}", path.display()))
}

fn is_cluster(dir: &Path) -> bool {
    dir.join("PG_VERSION").is_file() && dir.join("global/pg_control").is_file()
}

/// A compact timestamp, so two upgrades of one cluster keep two old
/// directories rather than colliding.
fn stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_preserves_the_absolute_path() {
        // Flattened, the old postmaster computes its share directory as
        // the compiled-in /usr/share/postgresql/17 — which exists in the
        // old image and not in the new one, and the error names a
        // directory rather than the mistake.
        let root = Path::new("/pgdata/upgrade/old");
        assert_eq!(
            mirrored(root, Path::new("/usr/lib/postgresql/17/bin")),
            PathBuf::from("/pgdata/upgrade/old/usr/lib/postgresql/17/bin")
        );
        assert_eq!(
            mirrored(root, Path::new("/usr/local/share/postgresql")),
            PathBuf::from("/pgdata/upgrade/old/usr/local/share/postgresql")
        );
    }

    #[test]
    fn a_tree_is_copied_with_its_modes_and_without_the_bitcode() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::create_dir_all(src.path().join("bitcode/postgres")).unwrap();
        fs::write(src.path().join("bitcode/postgres/x.bc"), b"llvm ir").unwrap();
        fs::write(src.path().join("postgres"), b"binary").unwrap();
        fs::set_permissions(
            src.path().join("postgres"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let mut copied = Copied::default();
        copy_tree(src.path(), &dst.path().join("out"), &mut copied).unwrap();

        let staged = dst.path().join("out/postgres");
        assert!(staged.is_file());
        assert_eq!(
            fs::metadata(&staged).unwrap().permissions().mode() & 0o777,
            0o755,
            "a staged binary that is not executable is not a binary"
        );
        assert!(
            !dst.path().join("out/bitcode").exists(),
            "bitcode is only read by the JIT, which the upgrade turns off"
        );
        assert_eq!(copied.files, 1);
    }

    #[test]
    fn a_symlink_is_staged_as_the_file_it_points_at() {
        // A staged tree has to be self-contained: a symlink to a path
        // that exists only in the old image dangles in the new one.
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("real.so"), b"elf").unwrap();
        std::os::unix::fs::symlink(src.path().join("real.so"), src.path().join("link.so")).unwrap();

        let mut copied = Copied::default();
        copy_tree(src.path(), &dst.path().join("out"), &mut copied).unwrap();

        let staged = dst.path().join("out/link.so");
        assert!(
            staged.is_file() && !staged.is_symlink(),
            "the staged copy must be a file, not a link into the old image"
        );
        assert_eq!(fs::read(staged).unwrap(), b"elf");
    }

    #[test]
    fn a_dangling_symlink_does_not_fail_the_stage() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/nowhere/at/all", src.path().join("broken.so")).unwrap();
        fs::write(src.path().join("postgres"), b"binary").unwrap();

        let mut copied = Copied::default();
        copy_tree(src.path(), &dst.path().join("out"), &mut copied).unwrap();
        assert_eq!(copied.files, 1);
    }

    #[test]
    fn llvm_is_not_followed_but_the_installation_is_still_complete() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("llvmjit.so"), b"elf").unwrap();
        fs::write(dir.path().join("vector.so"), b"elf").unwrap();
        let found = elf_files(&[dir.path()]).unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"vector.so".to_string()));
        assert!(
            !names.contains(&"llvmjit.so".to_string()),
            "following llvmjit stages 200 MB of LLVM for a JIT the upgrade \
             switches off"
        );
    }

    #[test]
    fn a_version_file_is_read_the_way_postgresql_writes_it() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("PG_VERSION"), "17\n").unwrap();
        assert_eq!(read_pg_version(dir.path()).unwrap(), "17");

        let dir9 = tempfile::tempdir().unwrap();
        fs::write(dir9.path().join("PG_VERSION"), "9.6\n").unwrap();
        assert_eq!(read_pg_version(dir9.path()).unwrap(), "9.6");
    }

    #[test]
    fn a_directory_without_a_control_file_is_not_a_cluster() {
        // The interrupted-swap recovery hinges on this: a half-built new
        // cluster must never be renamed into place as if it were done.
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_cluster(dir.path()));
        fs::write(dir.path().join("PG_VERSION"), "18\n").unwrap();
        assert!(!is_cluster(dir.path()));
        fs::create_dir_all(dir.path().join("global")).unwrap();
        fs::write(dir.path().join("global/pg_control"), b"\0").unwrap();
        assert!(is_cluster(dir.path()));
    }
}
