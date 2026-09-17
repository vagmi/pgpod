//! Does the agent actually bring a PostgreSQL instance up?
//!
//! The Phase 1 acceptance test: the static agent runs as PID 1 inside a
//! stock Debian-based `postgres` image, `initdb`s into an empty podman
//! volume, renders its config, creates roles, and leaves a server
//! accepting connections.
//!
//! This exercises every Phase 1 decision at once — the volume storage
//! model (ADR 00 §4), the agent as entrypoint (§6), the static musl build
//! (§3), and the rendering in `pgpod-pg`.
//!
//! ```sh
//! ops/build-agent.sh
//! eval "$(ops/dev-podman.sh start)"
//! cargo test -p pgpod-runtime --features podman-tests --test instance_bootstrap -- --nocapture --test-threads=1
//! ```

#![cfg(feature = "podman-tests")]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use pgpod_core::{
    Bootstrap, ClusterId, InitdbBootstrap, InstanceSpec, PathLayout, SPEC_ENV, container,
};

/// The instance container must run as the postgres user explicitly — see
/// `container::DEFAULT_POSTGRES_UID` for why leaving it as root fails two
/// different ways.
const PG_UID: u32 = container::DEFAULT_POSTGRES_UID;
const PG_GID: u32 = container::DEFAULT_POSTGRES_GID;
use pgpod_runtime::{ContainerSpec, ExecSpec, Mount, PodmanClient, SecretMount};

/// Stock Debian-based image, running as uid 999. Deliberately not a
/// pgpod-built image: the agent has to work in something it did not make.
const IMAGE: &str = "docker.io/library/postgres:18";

static SEQ: AtomicU32 = AtomicU32::new(0);

fn unique(prefix: &str) -> String {
    format!(
        "pgpod-it-{prefix}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

async fn connect() -> PodmanClient {
    let client =
        PodmanClient::connect().expect("podman socket — run: eval \"$(ops/dev-podman.sh start)\"");
    client.ping().await.expect("podman must answer /_ping");
    client
}

fn agent_binary() -> String {
    let path = PathLayout::from_env().agent_binary_for_host();
    assert!(
        path.exists(),
        "agent binary missing at {} — run ops/build-agent.sh first",
        path.display()
    );
    path.display().to_string()
}

/// Everything one test instance owns, so cleanup is one call and a
/// panicking test leaves a named, inspectable mess rather than an
/// anonymous one.
struct Instance {
    client: PodmanClient,
    name: String,
    volume: String,
    secrets: Vec<String>,
}

impl Instance {
    async fn create(client: &PodmanClient, tag: &str, spec: &InstanceSpec) -> Self {
        let name = unique(tag);
        let volume = format!("{name}-pgdata");

        client
            .create_volume(&volume, &[("pgpod.test".into(), "true".into())])
            .await
            .expect("create volume");

        // Passwords go in as podman secrets, never env vars (ADR 00 §9).
        let mut secrets = Vec::new();
        let mut mounts = Vec::new();
        for (suffix, target, value) in [
            ("superuser", container::SECRET_SUPERUSER, "su-pw"),
            ("replication", container::SECRET_REPLICATION, "repl-pw"),
            ("monitor", container::SECRET_MONITOR, "mon-pw"),
            ("app-owner", container::SECRET_APP_OWNER, "app-pw"),
        ] {
            let secret_name = format!("{name}-{suffix}");
            client
                .put_secret(&secret_name, value)
                .await
                .expect("create secret");
            mounts.push(SecretMount {
                name: secret_name.clone(),
                target: target.to_string(),
                mode: 0o400,
                uid: PG_UID,
                gid: PG_GID,
            });
            secrets.push(secret_name);
        }

        let mut container_spec = ContainerSpec::hardened(IMAGE)
            .name(&name)
            .user(format!("{PG_UID}:{PG_GID}"))
            .entrypoint([container::AGENT_BIN, "instance", "run"])
            .env(SPEC_ENV, spec.to_env_value().expect("encode spec"))
            .mount(Mount::Volume {
                name: volume.clone(),
                target: container::VOLUME_MOUNT.to_string(),
                chown: false,
            })
            .mount(Mount::BindReadOnly {
                source: agent_binary(),
                target: container::AGENT_BIN.to_string(),
            })
            .label("pgpod.test", "true");
        for m in mounts {
            container_spec = container_spec.secret(m);
        }

        client
            .pull_image_if_absent(IMAGE)
            .await
            .expect("pull postgres image");
        let container = client
            .create_container(&container_spec)
            .await
            .expect("create container");
        container.start().await.expect("start container");

        Self {
            client: client.clone(),
            name,
            volume,
            secrets,
        }
    }

    fn container(&self) -> pgpod_runtime::Container {
        self.client.container(&self.name)
    }

    /// Wait until the agent reports PostgreSQL is accepting connections.
    async fn wait_ready(&self, timeout: Duration) -> Result<(), String> {
        let deadline = std::time::Instant::now() + timeout;
        let mut last = String::new();
        while std::time::Instant::now() < deadline {
            // `pg_isready` rather than the status socket: it is the
            // narrowest possible check and does not depend on the agent's
            // own code being right.
            let probe =
                ExecSpec::new(["pg_isready", "-h", container::SOCKET_DIR, "-U", "postgres"]);
            match self.container().exec(&probe).await {
                Ok(out) if out.success() => return Ok(()),
                Ok(out) => last = format!("{}{}", out.stdout, out.stderr),
                Err(e) => last = e.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let logs = self
            .container()
            .logs_string()
            .await
            .unwrap_or_else(|e| format!("<logs unavailable: {e}>"));
        Err(format!(
            "instance never became ready.\nlast probe: {last}\ncontainer logs:\n{logs}"
        ))
    }

    async fn sql(&self, database: &str, query: &str) -> String {
        let spec = ExecSpec::new([
            "psql",
            "-X",
            "-tA",
            "-h",
            container::SOCKET_DIR,
            "-U",
            "postgres",
            "-d",
            database,
            "-c",
            query,
        ]);
        self.container()
            .exec(&spec)
            .await
            .expect("exec psql")
            .require_success("psql")
            .expect("psql query")
            .stdout
            .trim()
            .to_string()
    }

    async fn cleanup(self) {
        let c = self.container();
        let _ = c.stop(Duration::from_secs(20)).await;
        let _ = c.remove(true).await;
        let _ = self
            .client
            .remove_volume_destroying_data(&self.volume)
            .await;
        for s in &self.secrets {
            let _ = self.client.remove_secret(s).await;
        }
    }
}

fn spec_for(instance: &str) -> InstanceSpec {
    InstanceSpec {
        instance: ClusterId::new(instance).unwrap().instance(1),
        port: 5432,
        bootstrap: Bootstrap::Initdb(InitdbBootstrap {
            database: Some("appdb".into()),
            owner: Some("app".into()),
            ..Default::default()
        }),
        parameters: vec![("work_mem".into(), "8MB".into())],
        shared_preload_libraries: vec![],
        network_cidr: None,
        archive_command: None,
        backup: pgpod_core::BackupSpec::default(),
    }
}

/// The Phase 1 acceptance test.
#[tokio::test]
async fn agent_bootstraps_a_working_instance_in_a_stock_image() {
    let client = connect().await;
    let inst = Instance::create(&client, "boot", &spec_for("mydb")).await;

    if let Err(e) = inst.wait_ready(Duration::from_secs(120)).await {
        inst.cleanup().await;
        panic!("{e}");
    }

    // The agent must be PID 1 — the whole design rests on it, and if the
    // image entrypoint were running instead, everything below would still
    // pass while pgpod controlled nothing.
    let pid1 = inst
        .container()
        .exec(&ExecSpec::new(["cat", "/proc/1/cmdline"]))
        .await
        .expect("read pid 1")
        .stdout
        .replace('\0', " ");
    assert!(
        pid1.contains("pgpod-agent"),
        "agent is not PID 1, got: {pid1:?}"
    );

    // Data checksums — irreversible, and the precondition for pg_rewind.
    assert_eq!(
        inst.sql("postgres", "SHOW data_checksums").await,
        "on",
        "data checksums are off; pg_rewind would be impossible forever"
    );
    assert_eq!(inst.sql("postgres", "SHOW wal_log_hints").await, "on");

    // Managed settings reached the running server, which proves both the
    // rendering and the `include_dir` append.
    assert_eq!(inst.sql("postgres", "SHOW wal_level").await, "replica");
    assert_eq!(
        inst.sql("postgres", "SHOW restart_after_crash").await,
        "off"
    );
    assert_eq!(
        inst.sql("postgres", "SHOW archive_mode").await,
        "off",
        "no destination configured, so archiving must be off rather than archiving to nowhere"
    );

    // A user parameter from the spec took effect.
    assert_eq!(inst.sql("postgres", "SHOW work_mem").await, "8MB");

    // Roles and the application database exist.
    let roles = inst
        .sql(
            "postgres",
            "SELECT rolname FROM pg_roles WHERE rolname IN \
             ('streaming_replica','pgpod_monitor','app') ORDER BY rolname",
        )
        .await;
    assert_eq!(
        roles, "app\npgpod_monitor\nstreaming_replica",
        "got: {roles:?}"
    );

    assert_eq!(
        inst.sql(
            "postgres",
            "SELECT rolreplication FROM pg_roles WHERE rolname = 'streaming_replica'"
        )
        .await,
        "t"
    );
    assert_eq!(
        inst.sql(
            "postgres",
            "SELECT rolsuper FROM pg_roles WHERE rolname = 'streaming_replica'"
        )
        .await,
        "f",
        "the replication role must not be a superuser"
    );

    // The application database exists and is owned by the app role.
    assert_eq!(
        inst.sql(
            "postgres",
            "SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname = 'appdb'"
        )
        .await,
        "app"
    );

    // And it actually works as a database.
    inst.sql(
        "appdb",
        "CREATE TABLE t (id int); INSERT INTO t VALUES (1), (2)",
    )
    .await;
    assert_eq!(inst.sql("appdb", "SELECT count(*) FROM t").await, "2");

    inst.cleanup().await;
}

/// Data must survive the container, and a second start must not re-initdb.
///
/// This is what makes `pgpod delete` (without `--purge`) followed by
/// `pgpod apply` return the same database rather than an empty one.
#[tokio::test]
async fn data_survives_container_replacement_and_bootstrap_is_not_repeated() {
    let client = connect().await;
    let spec = spec_for("keepdb");
    let first = Instance::create(&client, "keep", &spec).await;

    if let Err(e) = first.wait_ready(Duration::from_secs(120)).await {
        first.cleanup().await;
        panic!("{e}");
    }
    first
        .sql(
            "appdb",
            "CREATE TABLE survivor (id int); INSERT INTO survivor VALUES (42)",
        )
        .await;

    // Replace the container, keeping the volume — the reconciler's
    // restart path.
    let volume = first.volume.clone();
    let secrets = first.secrets.clone();
    let c = first.container();
    c.stop(Duration::from_secs(20)).await.expect("stop");
    c.remove(true).await.expect("remove");

    let second_name = unique("keep2");
    let mut container_spec = ContainerSpec::hardened(IMAGE)
        .name(&second_name)
        .user(format!("{PG_UID}:{PG_GID}"))
        .entrypoint([container::AGENT_BIN, "instance", "run"])
        .env(SPEC_ENV, spec.to_env_value().unwrap())
        .mount(Mount::Volume {
            name: volume.clone(),
            target: container::VOLUME_MOUNT.to_string(),
            chown: false,
        })
        .mount(Mount::BindReadOnly {
            source: agent_binary(),
            target: container::AGENT_BIN.to_string(),
        })
        .label("pgpod.test", "true");
    for (suffix, target) in [
        ("superuser", container::SECRET_SUPERUSER),
        ("replication", container::SECRET_REPLICATION),
        ("monitor", container::SECRET_MONITOR),
        ("app-owner", container::SECRET_APP_OWNER),
    ] {
        container_spec = container_spec.secret(SecretMount {
            name: format!("{}-{suffix}", first.name),
            target: target.to_string(),
            mode: 0o400,
            uid: PG_UID,
            gid: PG_GID,
        });
    }

    let second = Instance {
        client: client.clone(),
        name: second_name,
        volume,
        secrets,
    };
    let container = client
        .create_container(&container_spec)
        .await
        .expect("create second container");
    container.start().await.expect("start second container");

    if let Err(e) = second.wait_ready(Duration::from_secs(120)).await {
        second.cleanup().await;
        panic!("second start failed: {e}");
    }

    assert_eq!(
        second.sql("appdb", "SELECT id FROM survivor").await,
        "42",
        "data did not survive container replacement"
    );

    let logs = second.container().logs_string().await.unwrap_or_default();
    assert!(
        logs.contains("already initialised"),
        "the agent re-ran bootstrap on an existing PGDATA — it must detect \
         PG_VERSION and skip initdb.\nlogs:\n{logs}"
    );

    second.cleanup().await;
}
