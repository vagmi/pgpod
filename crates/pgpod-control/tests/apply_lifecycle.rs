//! The Phase 1 acceptance test, at the level an operator actually uses.
//!
//! `instance_bootstrap.rs` proves the agent can bring PostgreSQL up given
//! a hand-built container spec. This proves the thing above it: that a
//! manifest goes in and a working cluster comes out, and that deleting
//! without `--purge` really does keep the data.
//!
//! That last one is the behaviour most worth pinning down. "Your data is
//! still there" is a promise pgpod makes in its own delete output, and a
//! regression would be discovered by an operator, not by a developer.
//!
//! ```sh
//! ops/build-agent.sh
//! eval "$(ops/dev-podman.sh start)"
//! cargo test -p pgpod-control --features podman-tests -- --nocapture
//! ```

#![cfg(feature = "podman-tests")]

use std::time::Duration;

use pgpod_control::Pgpod;
use pgpod_core::{ClusterId, ClusterManifest, PathLayout};
use pgpod_registry::Registry;
use pgpod_runtime::{ExecSpec, PodmanClient};

const IMAGE: &str = "docker.io/library/postgres:18";
const READY_TIMEOUT: Duration = Duration::from_secs(180);

fn manifest(name: &str) -> ClusterManifest {
    ClusterManifest::from_yaml(&format!(
        r#"
apiVersion: pgpod/v1
kind: Cluster
metadata:
  name: {name}
spec:
  imageName: {IMAGE}
  bootstrap:
    initdb:
      database: appdb
      owner: app
  postgresql:
    parameters:
      work_mem: 8MB
"#
    ))
    .expect("manifest parses")
}

/// A `Pgpod` with its registry and agent in a scratch directory, so the
/// test cannot disturb a real installation on the developer's machine.
fn pgpod(dir: &std::path::Path) -> Pgpod {
    let layout = PathLayout::new(
        dir.join("config"),
        dir.join("state"),
        dir.join("data"),
        dir.join("run"),
    );

    // The agent is built by ops/build-agent.sh into the *real* XDG data
    // dir; link it into the scratch layout rather than rebuilding.
    let real = PathLayout::from_env().agent_binary_for_host();
    assert!(
        real.exists(),
        "agent binary missing at {} — run ops/build-agent.sh first",
        real.display()
    );
    let dest = layout.agent_binary_for_host();
    std::fs::create_dir_all(dest.parent().unwrap()).expect("create bin dir");
    std::fs::copy(&real, &dest).expect("copy agent");

    let podman =
        PodmanClient::connect().expect("podman socket — run: eval \"$(ops/dev-podman.sh start)\"");
    let registry = Registry::open(&layout.registry_db()).expect("open registry");
    Pgpod::new(podman, registry, layout)
}

async fn sql(pgpod: &Pgpod, instance: &str, database: &str, query: &str) -> String {
    let id: pgpod_core::InstanceId = instance.parse().expect("instance id");
    let container = pgpod
        .running_container(&id)
        .await
        .expect("running container");
    container
        .exec(&ExecSpec::new([
            "psql",
            "-X",
            "-tA",
            "-h",
            pgpod_core::container::SOCKET_DIR,
            "-U",
            "postgres",
            "-d",
            database,
            "-c",
            query,
        ]))
        .await
        .expect("exec psql")
        .require_success("psql")
        .expect("psql succeeded")
        .stdout
        .trim()
        .to_string()
}

/// apply → write → delete → apply → the data is still there.
#[tokio::test]
async fn a_manifest_becomes_a_working_cluster_and_delete_keeps_the_data() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("t{}", std::process::id() % 100_000);
    let m = manifest(&name);
    let cluster = ClusterId::new(name.clone()).unwrap();

    // Best-effort cleanup of a previous failed run.
    let _ = pg.delete(&cluster, true).await;

    let report = pg.apply(&m, READY_TIMEOUT).await.expect("apply");
    assert!(report.created, "first apply should report a creation");
    assert_eq!(report.instances.len(), 1);
    let port = report.instances[0].host_port;
    assert!(
        report.instances[0].connection_uri.contains("app@"),
        "uri should use the app role: {}",
        report.instances[0].connection_uri
    );

    let instance = format!("{name}-1");

    // The rendering reached the running server.
    assert_eq!(
        sql(&pg, &instance, "postgres", "SHOW data_checksums").await,
        "on"
    );
    assert_eq!(
        sql(&pg, &instance, "postgres", "SHOW wal_log_hints").await,
        "on"
    );
    assert_eq!(
        sql(&pg, &instance, "postgres", "SHOW work_mem").await,
        "8MB"
    );
    assert_eq!(
        sql(&pg, &instance, "postgres", "SHOW archive_mode").await,
        "off",
        "no backup destination yet, so archiving must be off rather than archiving to nowhere"
    );

    // Status reads the role from the database, not from the registry.
    let status = pg.status(&cluster).await.expect("status");
    assert_eq!(status.instances.len(), 1);
    assert!(status.instances[0].running);
    assert_eq!(status.instances[0].role, pgpod_core::InstanceRole::Primary);
    assert!(status.primary_uri().is_some());

    sql(
        &pg,
        &instance,
        "appdb",
        "CREATE TABLE survivor (id int); INSERT INTO survivor VALUES (42)",
    )
    .await;

    // Delete without purge: containers go, volumes stay.
    let deleted = pg.delete(&cluster, false).await.expect("delete");
    assert_eq!(deleted.containers_removed.len(), 1);
    assert!(
        deleted.volumes_removed.is_empty(),
        "delete without --purge must not destroy a volume: {:?}",
        deleted.volumes_removed
    );
    assert_eq!(deleted.volumes_retained.len(), 1);

    // Re-apply: same volume, same port, same data.
    let again = pg.apply(&m, READY_TIMEOUT).await.expect("re-apply");
    assert!(!again.created, "second apply should converge, not create");
    assert_eq!(
        again.instances[0].host_port, port,
        "the port must be reused so applications pointed at it keep working"
    );
    assert_eq!(
        sql(&pg, &instance, "appdb", "SELECT id FROM survivor").await,
        "42",
        "data did not survive delete-without-purge"
    );

    // Bootstrap must not have re-run over an existing PGDATA.
    let logs = pg
        .podman_container(&instance.parse().unwrap())
        .logs_string()
        .await
        .unwrap_or_default();
    assert!(
        logs.contains("already initialised"),
        "the agent re-ran initdb on an existing PGDATA:\n{logs}"
    );

    // Purge really does destroy it.
    let purged = pg.delete(&cluster, true).await.expect("purge");
    assert_eq!(purged.volumes_removed.len(), 1);
    assert!(
        pg.podman_volume(&purged.volumes_removed[0])
            .await
            .expect("volume lookup")
            .is_none(),
        "volume survived --purge"
    );
    assert!(
        pg.registry().cluster(&cluster).expect("registry").is_none(),
        "purge should remove the registry row too"
    );
}

/// A manifest naming a pgpod-managed setting is refused before anything is
/// created — not after a container is already running.
#[tokio::test]
async fn reserved_parameters_are_rejected_before_any_resource_is_made() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("r{}", std::process::id() % 100_000);

    let m = ClusterManifest::from_yaml(&format!(
        "apiVersion: pgpod/v1\nkind: Cluster\nmetadata:\n  name: {name}\nspec:\n  \
         imageName: {IMAGE}\n  postgresql:\n    parameters:\n      archive_mode: \"off\"\n"
    ))
    .expect("manifest parses");

    let err = pg.apply(&m, Duration::ZERO).await.unwrap_err();
    assert!(
        err.to_string().contains("archive_mode"),
        "error should name the offending parameter: {err}"
    );

    // Nothing should have been created — failing after making a volume
    // would leave the operator to clean up after a typo.
    let cluster = ClusterId::new(name.clone()).unwrap();
    let volume = cluster.instance(1).volume_name();
    assert!(
        pg.podman_volume(&volume)
            .await
            .expect("volume lookup")
            .is_none(),
        "a rejected manifest left {volume} behind"
    );
}
