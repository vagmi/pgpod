//! XDG path layout for pgpod's own state.
//!
//! Deriving every pgpod-owned path from one value keeps the daemon, the
//! CLI, and any future migration tool from computing them independently
//! and drifting (ADR 00 §11).
//!
//! Database storage is deliberately **absent** from this type. PGDATA lives
//! in podman volumes under the graph root, not under XDG (ADR 00 §4). If
//! you find yourself wanting to add a `pgdata_path()` here, that is the
//! signal that something is reaching for a host path it should not have.

use std::path::{Path, PathBuf};

/// Container-side path constants.
///
/// These are the other half of the layout: the volume is mounted at
/// [`VOLUME_MOUNT`] and everything below it is fixed, so the agent and the
/// daemon agree on where things are without passing paths around.
pub mod container {
    /// Where an instance's volume is mounted inside its container.
    pub const VOLUME_MOUNT: &str = "/pgdata";

    /// `PGDATA` — one level below the mount point, because `initdb`
    /// demands an empty directory and mount points collect entries
    /// (ADR 00 §5).
    pub const PGDATA: &str = "/pgdata/pgdata";

    /// Agent-rendered configuration and the replication passfile.
    pub const CONF_DIR: &str = "/pgdata/conf";

    /// WAL prefetch staging. A sibling of `pgdata/` on the same filesystem
    /// so `restore_command` delivery is a rename, not a copy (ADR 01 §5).
    pub const SPOOL_DIR: &str = "/pgdata/spool";

    /// The agent's status socket, which the daemon reads instead of
    /// opening a SQL connection per probe (ADR 02 §7).
    pub const STATUS_SOCKET: &str = "/pgdata/conf/agent.sock";

    /// Where the static agent binary is bind-mounted read-only.
    pub const AGENT_BIN: &str = "/usr/local/bin/pgpod-agent";

    /// Where PostgreSQL puts its unix socket.
    ///
    /// Inside the volume, **not** the conventional
    /// `/var/run/postgresql`. That path belongs to the image, and its
    /// ownership varies: the stock `postgres` images use
    /// `postgres:postgres`, while CNPG-style images use uid 100 with the
    /// `postgres` group. Backing it with a tmpfs does not rescue this —
    /// podman's `tmpcopyup` does not preserve the directory's ownership,
    /// so the mount comes up root-owned and PostgreSQL cannot create its
    /// lock file.
    ///
    /// The volume is chowned to the container user on first mount by
    /// construction (ADR 00 §4), so a directory inside it is always
    /// writable regardless of what the image does. CloudNativePG solves
    /// this the same way, with its own `/controller/run`.
    pub const SOCKET_DIR: &str = "/pgdata/run";

    /// UID/GID the PostgreSQL container runs as.
    ///
    /// The instance container must run as this user explicitly. The
    /// official `postgres` images declare no `USER` — they start as root
    /// and drop privileges inside their entrypoint via `gosu`, and pgpod
    /// bypasses that entrypoint entirely (ADR 00 §6). Left unset the
    /// container would run as root, where two things go wrong at once:
    /// PostgreSQL refuses to start as root, and container-root cannot
    /// read a 0400 secret owned by another uid because `cap_drop: ALL`
    /// removed `CAP_DAC_OVERRIDE`.
    ///
    /// 999 on Debian-based `postgres` images. CNPG-style images use 26,
    /// so this is a per-image setting rather than a constant to rely on.
    pub const DEFAULT_POSTGRES_UID: u32 = 999;
    pub const DEFAULT_POSTGRES_GID: u32 = 999;

    /// Mounted podman secrets. Passwords reach the container this way and
    /// never through the environment (ADR 00 §9).
    pub const SECRET_SUPERUSER: &str = "/run/secrets/pgpod-superuser";
    pub const SECRET_REPLICATION: &str = "/run/secrets/pgpod-replication";
    pub const SECRET_MONITOR: &str = "/run/secrets/pgpod-monitor";
    pub const SECRET_APP_OWNER: &str = "/run/secrets/pgpod-app-owner";
}

/// Host-side layout for pgpod's own state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathLayout {
    config_dir: PathBuf,
    state_dir: PathBuf,
    data_dir: PathBuf,
    runtime_dir: PathBuf,
}

impl PathLayout {
    /// Construct explicitly. Used by tests and by anything that needs to
    /// point pgpod at a scratch directory.
    pub fn new(
        config_dir: impl Into<PathBuf>,
        state_dir: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
        runtime_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            config_dir: config_dir.into(),
            state_dir: state_dir.into(),
            data_dir: data_dir.into(),
            runtime_dir: runtime_dir.into(),
        }
    }

    /// Resolve from the environment.
    ///
    /// `PGPOD_HOME` overrides everything, rooting all four directories
    /// under one path — the escape hatch for tests and for running two
    /// pgpod instances side by side. Otherwise each resolves through the
    /// `dirs` crate, which reads the XDG variables.
    ///
    /// `XDG_RUNTIME_DIR` is genuinely optional (it is unset under `sudo
    /// -iu`, which is exactly the situation ADR 03 §2 warns about), so it
    /// falls back to `/run/user/<uid>` rather than failing. The other three
    /// have unconditional defaults in the spec.
    pub fn from_env() -> Self {
        if let Some(home) = std::env::var_os("PGPOD_HOME") {
            let home = PathBuf::from(home);
            return Self::new(
                home.join("config"),
                home.join("state"),
                home.join("data"),
                home.join("run"),
            );
        }

        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from(".config"))
            .join("pgpod");
        let state_dir = dirs::state_dir()
            .unwrap_or_else(|| PathBuf::from(".local/state"))
            .join("pgpod");
        let data_dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from(".local/share"))
            .join("pgpod");
        let runtime_dir = dirs::runtime_dir()
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc_getuid() })))
            .join("pgpod");

        Self::new(config_dir, state_dir, data_dir, runtime_dir)
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// The daemon's TOML configuration file.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// The SQLite registry (ADR 00 §7).
    pub fn registry_db(&self) -> PathBuf {
        self.state_dir.join("pgpod.db")
    }

    /// The static agent binary for `arch`, which gets bind-mounted into
    /// every instance container. Per-architecture because the binary is
    /// statically linked for a specific target (ADR 00 §3).
    pub fn agent_binary(&self, arch: &str) -> PathBuf {
        self.data_dir
            .join("bin")
            .join(format!("pgpod-agent-{arch}"))
    }

    /// The agent binary matching the architecture pgpod is running on.
    pub fn agent_binary_for_host(&self) -> PathBuf {
        self.agent_binary(std::env::consts::ARCH)
    }

    /// Per-cluster runtime directory, holding endpoint sockets.
    pub fn cluster_runtime_dir(&self, cluster: &crate::ClusterId) -> PathBuf {
        self.runtime_dir.join(cluster.as_str())
    }
}

/// `dirs::runtime_dir()` returns `None` when `XDG_RUNTIME_DIR` is unset, so
/// we need the uid to build the conventional fallback. Pulling in `nix`
/// just for this would make `pgpod-core` depend on a libc wrapper it
/// otherwise has no use for.
unsafe fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClusterId;

    fn layout() -> PathLayout {
        PathLayout::new("/cfg", "/state", "/data", "/run")
    }

    #[test]
    fn derives_state_and_config_paths() {
        let l = layout();
        assert_eq!(l.registry_db(), PathBuf::from("/state/pgpod.db"));
        assert_eq!(l.config_file(), PathBuf::from("/cfg/config.toml"));
    }

    #[test]
    fn agent_binary_is_per_architecture() {
        // A single `pgpod-agent` path would silently mount an x86_64
        // binary into an aarch64 container (ADR 00 §3).
        let l = layout();
        assert_eq!(
            l.agent_binary("x86_64"),
            PathBuf::from("/data/bin/pgpod-agent-x86_64")
        );
        assert_ne!(l.agent_binary("x86_64"), l.agent_binary("aarch64"));
    }

    #[test]
    fn cluster_runtime_dirs_are_namespaced_by_cluster() {
        let l = layout();
        let c = ClusterId::new("mydb").unwrap();
        assert_eq!(l.cluster_runtime_dir(&c), PathBuf::from("/run/mydb"));
    }

    #[test]
    fn container_paths_nest_under_the_volume_mount() {
        // The agent and the daemon both hardcode these; if they ever
        // disagree the instance writes its config where postgres will not
        // read it.
        assert!(container::PGDATA.starts_with(container::VOLUME_MOUNT));
        assert!(container::CONF_DIR.starts_with(container::VOLUME_MOUNT));
        assert!(container::SPOOL_DIR.starts_with(container::VOLUME_MOUNT));
        assert!(container::STATUS_SOCKET.starts_with(container::CONF_DIR));
    }

    #[test]
    fn pgdata_is_below_the_mount_point_not_at_it() {
        // initdb requires an empty directory and mount points collect
        // entries like lost+found (ADR 00 §5).
        assert_ne!(container::PGDATA, container::VOLUME_MOUNT);
    }
}
