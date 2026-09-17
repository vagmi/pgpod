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
//! cargo test -p pgpod-control --features podman-tests -- --nocapture --test-threads=1
//! ```

#![cfg(feature = "podman-tests")]

use std::time::Duration;

use pgpod_control::Pgpod;
use pgpod_core::{ClusterId, ClusterManifest, InstancePhase, PathLayout};
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

    // The rows stay, but they must not still claim the instance is
    // running: the container was just removed. Anything that rebuilds a
    // cluster from the registry reads this to tell an operator's
    // deliberate stop from a crash, and `Stopped` is the phase that says
    // "do not start this on your own".
    let stopped = pg.registry().instances(&cluster).expect("rows kept");
    assert_eq!(
        stopped.len(),
        1,
        "delete without --purge must keep the rows"
    );
    assert_eq!(
        stopped[0].phase,
        InstancePhase::Stopped,
        "delete left the instance row claiming {}",
        stopped[0].phase
    );
    assert!(
        stopped[0].container_id.is_none(),
        "the row still names a container that was removed"
    );
    assert_eq!(
        pg.status(&cluster).await.expect("status").phase,
        "stopped",
        "the cluster phase should say so too"
    );

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

/// An apply that fails *after* the cluster is recorded must leave a
/// terminal phase, and a later apply must be able to clear it.
///
/// The phase is not cosmetic. Nothing resets it on its own, so a failure
/// that left it reading `applying` would leave the row permanently
/// indistinguishable from an apply still in flight — and anything that
/// decides whether a cluster may be touched by reading it (boot recovery,
/// most of all) would skip the cluster forever over a failure from weeks
/// ago. A cluster that fails must be recoverable by fixing the manifest
/// and applying it again, with no registry surgery.
#[tokio::test]
async fn an_apply_that_fails_after_recording_leaves_a_terminal_phase() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("f{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();

    let _ = pg.delete(&cluster, true).await;

    // `shared_buffers` is not a reserved parameter, so this passes
    // validation and reaches a real postmaster, which then cannot map
    // that much shared memory and exits. That is the shape this test
    // needs: a failure *after* `put_cluster` recorded the cluster,
    // rather than one refused up front.
    let bad = ClusterManifest::from_yaml(&format!(
        "apiVersion: pgpod/v1\nkind: Cluster\nmetadata:\n  name: {name}\nspec:\n  \
         imageName: {IMAGE}\n  bootstrap:\n    initdb:\n      database: appdb\n      \
         owner: app\n  postgresql:\n    parameters:\n      shared_buffers: 4096GB\n"
    ))
    .expect("manifest parses");

    let err = pg
        .apply(&bad, Duration::from_secs(60))
        .await
        .expect_err("a postmaster that cannot start must fail the apply");

    let status = pg.status(&cluster).await.expect("status");
    assert_eq!(
        status.phase, "failed",
        "a failed apply left the cluster at {:?}, which nothing ever clears \
         — the original error was: {err}",
        status.phase
    );
    assert!(
        status
            .recent_events
            .iter()
            .any(|e| e.contains("apply failed")),
        "the failure should be in the event trail: {:?}",
        status.recent_events
    );

    // The way out is fixing the manifest, not touching the registry.
    pg.apply(&manifest(&name), READY_TIMEOUT)
        .await
        .expect("a corrected manifest should recover the cluster");
    assert_eq!(
        pg.status(&cluster).await.expect("status").phase,
        "running",
        "a successful apply must clear the failed phase"
    );

    let _ = pg.delete(&cluster, true).await;
}
