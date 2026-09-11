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

    /// pgBackRest's spool directory. A sibling of `pgdata/` on the same
    /// filesystem, which is what lets pgBackRest deliver a restored
    /// segment by renaming rather than copying it.
    pub const SPOOL_DIR: &str = "/pgdata/spool";

    /// pgBackRest's log directory.
    ///
    /// Inside the volume because the container rootfs is read-only, so
    /// pgBackRest's default `/var/log/pgbackrest` is not writable
    /// (ADR 04 §3). File logging is switched *off* in the rendered config
    /// — output goes to the container stream with everything else — but
    /// pgBackRest still wants a valid path.
    pub const LOG_DIR: &str = "/pgdata/log";

    /// Where the pgBackRest bundle is mounted, read-only.
    ///
    /// A directory rather than the agent's single file, because pgBackRest
    /// is dynamically linked and travels with its own libraries and loader
    /// (ADR 04 §2). Outside the volume: it belongs to the host
    /// installation, not to this instance's data, and it is the same
    /// bundle for every cluster.
    pub const PGBACKREST_BUNDLE: &str = "/opt/pgpod";

    /// The wrapper that invokes the bundled loader. Everything calls this
    /// rather than spelling out the loader invocation.
    pub const PGBACKREST_BIN: &str = "/opt/pgpod/bin/pgbackrest";

    /// The CA certificate store shipped in the pgBackRest bundle.
    ///
    /// OpenSSL opens its trust store by path at *runtime*, so it is
    /// invisible to `ldd` and the bundle missed it at first. Without it,
    /// TLS verification depends on the target image happening to carry
    /// certificates — which the stock `postgres:18` and Alpine images do
    /// not, so an S3 or GCS repository fails with "unable to get local
    /// issuer certificate" while a posix one works perfectly (ADR 04 §2).
    pub const PGBACKREST_CA_FILE: &str = "/opt/pgpod/share/ca-bundle.crt";

    /// The agent-rendered pgBackRest configuration.
    ///
    /// Inside the volume rather than at pgBackRest's default
    /// `/etc/pgbackrest/pgbackrest.conf`, which a read-only rootfs makes
    /// unwritable (ADR 04 §3). Every invocation passes `--config` at this
    /// path, so nothing depends on pgBackRest's search order.
    pub const PGBACKREST_CONF: &str = "/pgdata/conf/pgbackrest.conf";

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

    /// The port PostgreSQL serves on inside the container.
    pub const PG_PORT: u16 = 5432;

    /// Port the *bootstrap* postmaster listens on, and nothing else.
    ///
    /// The bootstrap window — where `initdb`'s roles are created and the
    /// pgBackRest stanza is made — is supposed to be private: the comment
    /// on `with_local_postgres` says an outside client must not see a
    /// half-created role set. `listen_addresses=''` closed TCP but not the
    /// unix socket, and a unix socket's *filename* encodes the port
    /// (`.s.PGSQL.<port>`), so a different port here is what actually
    /// makes the window private.
    ///
    /// It also fixes a race that was real rather than theoretical:
    /// `pgpod apply --wait` probes readiness with `pg_isready` on the
    /// standard port, which the bootstrap postmaster was answering. Apply
    /// could therefore return "ready" and the caller's first connection
    /// would land on a server already shutting down.
    pub const BOOTSTRAP_PORT: u16 = 5433;

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

    /// Where `spec.backup.volume` is mounted, when a cluster archives to
    /// a `file://` destination.
    ///
    /// Deliberately **not** under [`VOLUME_MOUNT`]: the archive must
    /// outlive the data directory it protects, and putting it inside the
    /// instance's own volume would mean `pgpod delete --purge` destroys
    /// the backups along with the database it was taking them for.
    ///
    /// A named volume rather than a host bind mount because that is the
    /// one thing that works without UID mapping tricks: podman chowns an
    /// empty named volume to the container's user on first mount, which
    /// Phase 0 verified rather than assumed (ADR 00 §4). A host directory
    /// would land in the user namespace owned by the wrong uid, which is
    /// precisely the failure the volume storage model exists to avoid.
    pub const ARCHIVE_MOUNT: &str = "/archive";

    /// Mounted podman secrets. Passwords reach the container this way and
    /// never through the environment (ADR 00 §9).
    pub const SECRET_SUPERUSER: &str = "/run/secrets/pgpod-superuser";
    pub const SECRET_REPLICATION: &str = "/run/secrets/pgpod-replication";
    pub const SECRET_MONITOR: &str = "/run/secrets/pgpod-monitor";
    pub const SECRET_APP_OWNER: &str = "/run/secrets/pgpod-app-owner";

    /// The pg_doorman admin console password, for this pooler.
    pub const SECRET_POOLER_ADMIN: &str = "/run/secrets/pgpod-pooler-admin";

    /// The `auth_query` lookup role's password, one per fronted cluster.
    ///
    /// Indexed rather than named, for the reason `Destination` credentials
    /// are: the podman secret's *name* is a host concept that means
    /// nothing inside the container, and a pooler may front several
    /// clusters (ADR 05 §2).
    pub fn pooler_lookup_secret(index: usize) -> String {
        format!("/run/secrets/pgpod-pooler-lookup-{index}")
    }

    /// The pooler container's writable directory — a tmpfs, because the
    /// rootfs is read-only and nothing here outlives the container.
    ///
    /// A tmpfs rather than a volume because there is genuinely no durable
    /// state: the config is re-rendered from `PGPOD_POOLER_SPEC` and the
    /// mounted secrets on every start. Verified on podman 5.7 that a
    /// fresh tmpfs mounts `1777`, so a non-root uid can write it — which
    /// is also why the agent sets the config's own mode rather than
    /// inheriting one (ADR 05 §5).
    pub const POOLER_DIR: &str = "/pooler";

    /// The agent-rendered pg_doorman configuration.
    ///
    /// **Not** a bind mount from the host, and this is not a style
    /// preference: rootless podman maps the host user to container UID 0,
    /// so a container running as uid 999 gets `Permission denied` reading
    /// a file owned by the host user. A podman secret is chowned
    /// correctly but is fixed at container-create time, leaving `RELOAD`
    /// nothing new to read. Rendering in-container is the only delivery
    /// that is both readable and rewritable (ADR 05 §5).
    pub const POOLER_CONF: &str = "/pooler/pg_doorman.yaml";

    /// Mode the rendered config is written with. It carries the
    /// `auth_query` and admin passwords in plaintext, because that is the
    /// only form pg_doorman accepts.
    pub const POOLER_CONF_MODE: u32 = 0o600;

    /// The pooler agent's control socket.
    ///
    /// The daemon reaches it by `podman exec`, the way `status.rs`
    /// already execs into an instance. PID 1 answers, which is what lets
    /// a pause carry a deadline that outlives the exec that asked for it
    /// (ADR 05, consequences).
    pub const POOLER_CONTROL_SOCKET: &str = "/pooler/agent.sock";

    /// Port pg_doorman listens on inside the container.
    pub const POOLER_PORT: u16 = 6432;

    /// pg_doorman's own admin database, reached on the same port.
    pub const POOLER_ADMIN_DB: &str = "pgdoorman";

    /// The admin console user pgpod creates for itself.
    pub const POOLER_ADMIN_USER: &str = "pgpod_admin";

    /// pg_doorman inside the upstream image.
    pub const POOLER_BIN: &str = "/usr/bin/pg_doorman";

    /// Default pooler image.
    ///
    /// Pinned rather than `latest`: the pooler is on the data path, and a
    /// tag that moves underneath a restart is not something to discover
    /// during an incident.
    pub const DEFAULT_POOLER_IMAGE: &str = "ghcr.io/ozontech/pg_doorman:v3.11.0";

    /// UID/GID the pooler container runs as.
    ///
    /// Any non-root uid works — pg_doorman writes only to the tmpfs — but
    /// it must be set explicitly for the same reason the instance's is:
    /// the image declares no `USER`, and container-root cannot read a
    /// 0400 secret once `cap_drop: ALL` removes `CAP_DAC_OVERRIDE`.
    pub const DEFAULT_POOLER_UID: u32 = 999;
    pub const DEFAULT_POOLER_GID: u32 = 999;
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

    /// The pgBackRest bundle for `arch`, bind-mounted into every container
    /// that touches the repository.
    ///
    /// Per-architecture for the same reason the agent is: it contains
    /// native code, and mounting the wrong one fails at exec time with a
    /// message about a file that plainly exists (ADR 00 §3).
    pub fn pgbackrest_bundle(&self, arch: &str) -> PathBuf {
        self.data_dir.join(format!("pgbackrest-{arch}"))
    }

    /// The pgBackRest bundle matching the architecture pgpod is running on.
    pub fn pgbackrest_bundle_for_host(&self) -> PathBuf {
        self.pgbackrest_bundle(std::env::consts::ARCH)
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
    fn the_pgbackrest_bundle_is_per_architecture() {
        // It carries native code and its own loader; mounting an x86_64
        // bundle into an aarch64 container fails at exec.
        let l = layout();
        assert_eq!(
            l.pgbackrest_bundle("x86_64"),
            PathBuf::from("/data/pgbackrest-x86_64")
        );
        assert_ne!(
            l.pgbackrest_bundle("x86_64"),
            l.pgbackrest_bundle("aarch64")
        );
    }

    #[test]
    fn the_pgbackrest_bundle_is_mounted_outside_the_volume() {
        // It belongs to the host installation and is shared by every
        // cluster, so it must not live inside one instance's data — where
        // `delete --purge` would take it.
        assert!(!container::PGBACKREST_BUNDLE.starts_with(container::VOLUME_MOUNT));
        assert!(container::PGBACKREST_BIN.starts_with(container::PGBACKREST_BUNDLE));
    }

    #[test]
    fn container_paths_nest_under_the_volume_mount() {
        // The agent and the daemon both hardcode these; if they ever
        // disagree the instance writes its config where postgres will not
        // read it.
        assert!(container::PGDATA.starts_with(container::VOLUME_MOUNT));
        assert!(container::CONF_DIR.starts_with(container::VOLUME_MOUNT));
        assert!(container::SPOOL_DIR.starts_with(container::VOLUME_MOUNT));
        assert!(container::LOG_DIR.starts_with(container::VOLUME_MOUNT));
        assert!(container::STATUS_SOCKET.starts_with(container::CONF_DIR));
        assert!(container::PGBACKREST_CONF.starts_with(container::CONF_DIR));
    }

    #[test]
    fn pgdata_is_below_the_mount_point_not_at_it() {
        // initdb requires an empty directory and mount points collect
        // entries like lost+found (ADR 00 §5).
        assert_ne!(container::PGDATA, container::VOLUME_MOUNT);
    }
}
