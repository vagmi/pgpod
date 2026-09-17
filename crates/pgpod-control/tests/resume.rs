//! Boot recovery, against a real podman.
//!
//! The unit tests in `daemon.rs` pin every branch of the decision; this
//! pins that acting on those decisions actually works. What it imitates is
//! a host reboot: containers stopped out from under pgpod, with the
//! registry still describing a running cluster.
//!
//! One thing here is worth stating because it is invisible otherwise. The
//! agent refuses to start without `/run/secrets/{superuser,replication,
//! monitor}`, and it checks for them *before* it looks at PGDATA — so an
//! instance that becomes ready after a `start()` is proof that podman
//! re-materialised its mounted secrets into a tmpfs. That is the single
//! assumption the whole design rests on, and this is where it is tested.
//!
//! It also covers the case the existing `instance_bootstrap.rs` does not:
//! that test stops, **removes**, and creates a new container, which proves
//! the volume survives and says nothing about `podman start` on the same
//! container.
//!
//! ```sh
//! ops/build-agent.sh
//! eval "$(ops/dev-podman.sh start)"
//! cargo test -p pgpod-control --features podman-tests --test resume -- --nocapture
//! ```

#![cfg(feature = "podman-tests")]

use std::time::Duration;

use pgpod_control::{DaemonLock, Pgpod, ResumeAction, ResumeOptions};
use pgpod_core::{ClusterId, ClusterManifest, InstancePhase, PathLayout, PoolerId, PoolerManifest};
use pgpod_registry::Registry;
use pgpod_runtime::{ExecSpec, PodmanClient};

const IMAGE: &str = "docker.io/library/postgres:18";
const READY: Duration = Duration::from_secs(180);

fn cluster_manifest(name: &str) -> ClusterManifest {
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
    .expect("cluster manifest parses")
}

fn pooler_manifest(name: &str, cluster: &str) -> PoolerManifest {
    PoolerManifest::from_yaml(&format!(
        r#"
apiVersion: pgpod/v1
kind: Pooler
metadata:
  name: {name}
spec:
  clusters:
    - cluster: {cluster}
  pgDoorman:
    poolSize: 10
    maxHold: "90s"
"#
    ))
    .expect("pooler manifest parses")
}

fn pgpod(dir: &std::path::Path) -> Pgpod {
    let layout = PathLayout::new(
        dir.join("config"),
        dir.join("state"),
        dir.join("data"),
        dir.join("run"),
    );
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

async fn sql(pg: &Pgpod, instance: &str, query: &str) -> String {
    let id: pgpod_core::InstanceId = instance.parse().expect("instance id");
    pg.running_container(&id)
        .await
        .expect("running container")
        .exec(&ExecSpec::new([
            "psql",
            "-X",
            "-tA",
            "-h",
            pgpod_core::container::SOCKET_DIR,
            "-U",
            "postgres",
            "-d",
            "appdb",
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

/// A bare podman client, for reaching containers by name.
///
/// `Pgpod::podman` is `pub(crate)`, and building a second `Pgpod` here
/// would re-copy the agent binary, which is bind-mounted into running
/// containers and therefore `ETXTBSY` — the same reason `pooler.rs` keeps
/// one of these.
fn raw_podman() -> PodmanClient {
    PodmanClient::connect().expect("podman socket — run: eval \"$(ops/dev-podman.sh start)\"")
}

/// Stop every container of a cluster without telling pgpod, the way a
/// reboot does. `Duration::ZERO` so it is abrupt rather than graceful.
async fn simulate_reboot(pg: &Pgpod, cluster: &ClusterId) {
    let podman = raw_podman();
    for inst in pg.registry().instances(cluster).expect("rows") {
        let _ = podman
            .container(&inst.container_name)
            .stop(Duration::ZERO)
            .await;
    }
}

fn action_of<'a>(
    report: &'a pgpod_control::ResumeReport,
    name: &str,
) -> &'a pgpod_control::UnitResume {
    report
        .clusters
        .iter()
        .flat_map(|c| c.instances.iter())
        .chain(report.poolers.iter())
        .find(|u| u.name == name)
        .unwrap_or_else(|| panic!("{name} is missing from the resume report: {report:#?}"))
}

/// The acceptance test: a reboot leaves everything down, and resume brings
/// it back with the data and the ports intact.
#[tokio::test]
async fn resume_brings_back_a_cluster_and_its_pooler_after_a_reboot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("rs{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();
    let pooler_name = format!("{name}p");
    let pooler = PoolerId::new(pooler_name.clone()).unwrap();

    let _ = pg.delete_pooler(&pooler).await;
    let _ = pg.delete(&cluster, true).await;

    pg.apply(&cluster_manifest(&name), READY)
        .await
        .expect("apply");
    pg.apply_pooler(&pooler_manifest(&pooler_name, &name), READY)
        .await
        .expect("apply pooler");

    let instance = format!("{name}-1");
    sql(
        &pg,
        &instance,
        "CREATE TABLE survivor(id int); INSERT INTO survivor VALUES (42)",
    )
    .await;
    // Force it to disk, so what comes back proves the volume rather than
    // a page cache that survived a graceless stop.
    sql(&pg, &instance, "CHECKPOINT").await;

    let port_before = pg.registry().instances(&cluster).expect("rows")[0].host_port;
    let pooler_port_before = pg
        .registry()
        .pooler(&pooler)
        .expect("pooler row")
        .expect("pooler exists")
        .host_port;

    simulate_reboot(&pg, &cluster).await;
    let _ = raw_podman()
        .container(pooler.container_name())
        .stop(Duration::ZERO)
        .await;

    // Nothing restarts on its own — the whole reason this module exists.
    assert!(
        pg.podman_container(&instance.parse().unwrap())
            .probe()
            .await
            .expect("probe")
            .expect("container still exists")
            .running
            .eq(&false),
        "the container should be down before resume runs"
    );

    let report = pg.resume().await.expect("resume");
    assert_eq!(action_of(&report, &instance).action, ResumeAction::Started);
    assert_eq!(
        action_of(&report, &pooler_name).action,
        ResumeAction::Started
    );
    assert!(
        !report.needs_attention(),
        "a clean boot recovery should need nothing: {report:#?}"
    );

    // The data, and the proof that podman re-materialised the secrets:
    // the agent would have refused to start without them.
    assert_eq!(sql(&pg, &instance, "SELECT id FROM survivor").await, "42");
    assert_eq!(sql(&pg, &instance, "SHOW work_mem").await, "8MB");

    // Ports must not move: they are in `pgpod status`, in the connection
    // URI, and in whatever applications were pointed at them.
    assert_eq!(
        pg.registry().instances(&cluster).expect("rows")[0].host_port,
        port_before,
        "resume moved the instance's host port"
    );
    assert_eq!(
        pg.registry()
            .pooler(&pooler)
            .expect("pooler row")
            .expect("exists")
            .host_port,
        pooler_port_before,
        "resume moved the pooler's host port"
    );

    // Bootstrap must not have re-run over a populated PGDATA.
    let logs = pg
        .podman_container(&instance.parse().unwrap())
        .logs_string()
        .await
        .unwrap_or_default();
    assert!(
        logs.contains("already initialised"),
        "the agent re-ran initdb on an existing PGDATA:\n{logs}"
    );

    // Idempotent: running it again adopts rather than restarting, so the
    // unit and a hand-run `pgpod daemon --once` cannot fight.
    let again = pg.resume().await.expect("second resume");
    assert_eq!(action_of(&again, &instance).action, ResumeAction::Adopted);
    assert_eq!(
        action_of(&again, &pooler_name).action,
        ResumeAction::Adopted
    );

    let _ = pg.delete_pooler(&pooler).await;
    let _ = pg.delete(&cluster, true).await;
}

/// Repeated kill-and-resume cycles, which is what a flapping host does.
///
/// Guards the stale-lock hazard: PGDATA's `postmaster.pid` and the socket
/// directory both live *inside* the volume, unlike a stock image where the
/// socket dir is on the ephemeral rootfs, so both survive a graceless
/// stop. PostgreSQL clears a stale lock when the recorded pid is dead, but
/// a container with a handful of processes can in principle reuse one.
#[tokio::test]
async fn repeated_kill_and_resume_cycles_keep_working() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("rc{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();
    let _ = pg.delete(&cluster, true).await;

    pg.apply(&cluster_manifest(&name), READY)
        .await
        .expect("apply");
    let instance = format!("{name}-1");
    sql(&pg, &instance, "CREATE TABLE t(i int)").await;

    for cycle in 1..=3 {
        sql(&pg, &instance, &format!("INSERT INTO t VALUES ({cycle})")).await;
        sql(&pg, &instance, "CHECKPOINT").await;
        simulate_reboot(&pg, &cluster).await;

        let report = pg.resume().await.expect("resume");
        assert_eq!(
            action_of(&report, &instance).action,
            ResumeAction::Started,
            "cycle {cycle} did not start the instance: {:?}",
            action_of(&report, &instance).detail
        );
        assert_eq!(
            sql(&pg, &instance, "SELECT count(*) FROM t").await,
            cycle.to_string(),
            "cycle {cycle} lost rows"
        );
    }

    let _ = pg.delete(&cluster, true).await;
}

/// The refusals: a fenced instance, an interrupted apply, and a cluster
/// the operator stopped on purpose.
#[tokio::test]
async fn resume_refuses_what_an_operator_has_to_decide() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("rf{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();
    let _ = pg.delete(&cluster, true).await;

    pg.apply(&cluster_manifest(&name), READY)
        .await
        .expect("apply");
    let instance = format!("{name}-1");
    simulate_reboot(&pg, &cluster).await;

    // 1. A fenced instance. This is the split-brain guard: during a
    //    failover the old primary is deliberately kept down, and a
    //    helpful restart is precisely the accident to avoid.
    let mut row = pg
        .registry()
        .instance(&cluster, 1)
        .expect("row")
        .expect("exists");
    row.phase = InstancePhase::Fenced;
    pg.registry().put_instance(&row).expect("fence it");

    let report = pg.resume().await.expect("resume");
    let fenced = action_of(&report, &instance);
    assert_eq!(fenced.action, ResumeAction::Skipped);
    assert!(
        fenced
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("fenced"),
        "the report should say why: {:?}",
        fenced.detail
    );
    assert!(
        !pg.podman_container(&instance.parse().unwrap())
            .probe()
            .await
            .expect("probe")
            .expect("exists")
            .running,
        "a fenced instance was started anyway"
    );

    // 2. An interrupted apply. The cluster phase gates every instance,
    //    whatever their own phases say, because a cluster caught mid-apply
    //    may have containers built from a spec nobody chose.
    row.phase = InstancePhase::Running;
    pg.registry().put_instance(&row).expect("unfence");
    pg.registry()
        .set_cluster_phase(&cluster, "applying")
        .expect("set phase");

    let report = pg.resume().await.expect("resume");
    let skipped = report
        .clusters
        .iter()
        .find(|c| c.cluster == name)
        .expect("the cluster is in the report");
    assert!(
        skipped.skipped.is_some(),
        "a cluster mid-apply must be skipped whole: {skipped:#?}"
    );
    assert!(
        skipped.instances.is_empty(),
        "and none of its instances looked at"
    );
    assert!(
        !pg.podman_container(&instance.parse().unwrap())
            .probe()
            .await
            .expect("probe")
            .expect("exists")
            .running
    );

    // 3. Deliberately stopped. `delete` without --purge is an operator
    //    saying so, and resume must not undo it.
    pg.registry()
        .set_cluster_phase(&cluster, "running")
        .expect("set phase");
    pg.delete(&cluster, false).await.expect("delete");

    let report = pg.resume().await.expect("resume");
    let stopped = report
        .clusters
        .iter()
        .find(|c| c.cluster == name)
        .expect("still in the report");
    assert!(
        stopped.skipped.is_some(),
        "a stopped cluster must stay stopped: {stopped:#?}"
    );
    // And nothing was created to replace the removed container.
    assert!(
        pg.podman_container(&instance.parse().unwrap())
            .probe()
            .await
            .expect("probe")
            .is_none(),
        "resume created a container, which it must never do"
    );

    let _ = pg.delete(&cluster, true).await;
}

/// A dry run answers the question without touching anything — including
/// while a daemon holds the lock, which is when the question gets asked.
#[tokio::test]
async fn a_dry_run_reports_what_would_happen_and_changes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("dr{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();
    let _ = pg.delete(&cluster, true).await;

    pg.apply(&cluster_manifest(&name), READY)
        .await
        .expect("apply");
    let instance = format!("{name}-1");
    sql(
        &pg,
        &instance,
        "CREATE TABLE t(i int); INSERT INTO t VALUES (1)",
    )
    .await;

    simulate_reboot(&pg, &cluster).await;
    let events_before = pg
        .registry()
        .recent_events(&cluster, 100)
        .expect("events")
        .len();

    let dry = pg
        .resume_with(ResumeOptions {
            dry_run: true,
            ..Default::default()
        })
        .await
        .expect("dry run");

    assert!(dry.dry_run, "the report should say it was a dry run");
    assert_eq!(
        action_of(&dry, &instance).action,
        ResumeAction::WouldStart,
        "a stopped instance of a running cluster is what a dry run is for"
    );

    // The load-bearing assertion: it changed nothing.
    assert!(
        !pg.podman_container(&instance.parse().unwrap())
            .probe()
            .await
            .expect("probe")
            .expect("exists")
            .running,
        "the dry run started the container"
    );
    assert_eq!(
        pg.registry()
            .recent_events(&cluster, 100)
            .expect("events")
            .len(),
        events_before,
        "the dry run wrote events — asking what would happen must not \
         pollute the record of what did"
    );

    // Running for real does the thing the dry run described.
    let real = pg.resume().await.expect("resume");
    assert!(!real.dry_run);
    assert_eq!(action_of(&real, &instance).action, ResumeAction::Started);
    assert_eq!(sql(&pg, &instance, "SELECT count(*) FROM t").await, "1");

    // And now a dry run says there is nothing to do.
    let after = pg
        .resume_with(ResumeOptions {
            dry_run: true,
            ..Default::default()
        })
        .await
        .expect("dry run");
    assert_eq!(action_of(&after, &instance).action, ResumeAction::Adopted);
    assert_eq!(after.counts().would_start, 0);

    let _ = pg.delete(&cluster, true).await;
}

/// The reason dry run exists: it works while the daemon holds the lock.
#[tokio::test]
async fn a_dry_run_needs_no_daemon_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let layout = PathLayout::new(
        dir.path().join("config"),
        dir.path().join("state"),
        dir.path().join("data"),
        dir.path().join("run"),
    );

    // Stand in for a running daemon.
    let _held = DaemonLock::acquire(&layout).expect("take the lock");
    assert!(
        DaemonLock::acquire(&layout).is_err(),
        "the lock should be exclusive"
    );

    // A real resume would be refused by the CLI before reaching here; the
    // dry run is the path that does not ask for the lock at all.
    pg.resume_with(ResumeOptions {
        dry_run: true,
        ..Default::default()
    })
    .await
    .expect("a dry run must work while the daemon holds the lock");
}
