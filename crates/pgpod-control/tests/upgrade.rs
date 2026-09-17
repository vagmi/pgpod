//! The upgrade acceptance test: PostgreSQL 17 to 18, with a client
//! committing through the pooler the whole way.
//!
//! Two properties, and the second is the one that makes this worth
//! building rather than documenting `pg_upgrade`:
//!
//! 1. **The data survives.** Every row committed before the upgrade is
//!    still there afterwards, read back through the same pooled
//!    connection string, from a server that reports the new version.
//! 2. **The clients survive.** A client committing continuously across
//!    the window loses nothing — the property ADR 05 measured for a
//!    container recreate, now spanning a `pg_upgrade`.
//!
//! The images are deliberately from **different Debian releases**
//! (`17-bookworm` and `18`, which is trixie). That is not incidental
//! coverage: bookworm's PostgreSQL links `libicuuc.so.72` and the trixie
//! image ships only `.76`, so a staging step that copied binaries without
//! their libraries would pass against same-release images and fail here —
//! which is exactly the upgrade a real operator does, a year after
//! deploying.
//!
//! ```sh
//! ops/build-agent.sh
//! eval "$(ops/dev-podman.sh start)"
//! cargo test -p pgpod-control --features podman-tests --test upgrade -- --nocapture
//! ```

#![cfg(feature = "podman-tests")]

use std::time::Duration;

use pgpod_control::{Pgpod, UpgradeOptions};
use pgpod_core::{ClusterId, ClusterManifest, PathLayout, PoolerId, PoolerManifest, UpgradeMethod};
use pgpod_registry::Registry;
use pgpod_runtime::{ExecSpec, PodmanClient};

/// Deliberately a different Debian release from the target — see the
/// module docs.
const FROM_IMAGE: &str = "docker.io/library/postgres:17-bookworm";
const TO_IMAGE: &str = "docker.io/library/postgres:18";
const READY: Duration = Duration::from_secs(240);

fn cluster_manifest(name: &str, image: &str) -> ClusterManifest {
    ClusterManifest::from_yaml(&format!(
        r#"
apiVersion: pgpod/v1
kind: Cluster
metadata:
  name: {name}
spec:
  imageName: {image}
  bootstrap:
    initdb:
      database: appdb
      owner: app
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
    maxHold: "120s"
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

    // The pgBackRest bundle, for the test that upgrades a cluster which
    // archives. Symlinked rather than copied: it is ~19 MB.
    let real_bundle = PathLayout::from_env().pgbackrest_bundle_for_host();
    if real_bundle.join("bin/pgbackrest").exists() {
        let _ = std::os::unix::fs::symlink(&real_bundle, layout.pgbackrest_bundle_for_host());
    }

    let podman =
        PodmanClient::connect().expect("podman socket — run: eval \"$(ops/dev-podman.sh start)\"");
    let registry = Registry::open(&layout.registry_db()).expect("open registry");
    Pgpod::new(podman, registry, layout)
}

fn podman() -> PodmanClient {
    PodmanClient::connect().expect("podman socket — run: eval \"$(ops/dev-podman.sh start)\"")
}

/// Run a query through the pooler, exactly as an application would:
/// SCRAM to pg_doorman, `auth_query` behind it.
async fn query(
    podman: &PodmanClient,
    pooler: &PoolerId,
    password: &str,
    sql: &str,
) -> Result<String, String> {
    let out = podman
        .container(pooler.container_name())
        .exec(
            &ExecSpec::new([
                "psql",
                "-X",
                "-tA",
                "-q",
                "-h",
                "127.0.0.1",
                "-p",
                "6432",
                "-U",
                "app",
                "-d",
                "appdb",
                "-c",
                sql,
            ])
            .env("PGPASSWORD", password),
        )
        .await
        .map_err(|e| e.to_string())?;
    if out.success() {
        Ok(out.stdout.trim().to_string())
    } else {
        Err(format!("{}{}", out.stdout.trim(), out.stderr.trim()))
    }
}

async fn app_password(pg: &Pgpod, cluster: &ClusterId) -> String {
    let container = pg
        .running_container(&cluster.instance(1))
        .await
        .expect("instance up");
    let out = container
        .exec(&ExecSpec::new([
            "cat",
            pgpod_core::container::SECRET_APP_OWNER,
        ]))
        .await
        .expect("read the mounted secret");
    out.stdout.trim().to_string()
}

/// What the instance itself reports, over its unix socket — independent
/// of the pooler, so a stale pool cannot make this agree by accident.
async fn server_version(pg: &Pgpod, cluster: &ClusterId) -> String {
    let container = pg
        .running_container(&cluster.instance(1))
        .await
        .expect("instance up");
    let out = container
        .exec(&ExecSpec::new([
            "psql",
            "-X",
            "-tA",
            "-h",
            pgpod_core::container::SOCKET_DIR,
            "-U",
            "postgres",
            "-c",
            "SHOW server_version",
        ]))
        .await
        .expect("ask the server its version");
    out.stdout.trim().to_string()
}

#[tokio::test]
async fn a_major_upgrade_keeps_every_row_and_every_client() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("u{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();
    let pooler = PoolerId::new(format!("{name}-pool")).unwrap();

    let _ = pg.delete_pooler(&pooler).await;
    let _ = pg.delete(&cluster, true).await;

    pg.apply(&cluster_manifest(&name, FROM_IMAGE), READY)
        .await
        .expect("apply the 17 cluster");
    let password = app_password(&pg, &cluster).await;
    assert!(
        server_version(&pg, &cluster).await.starts_with("17"),
        "the test has to start on 17 for the upgrade to mean anything"
    );

    pg.apply_pooler(&pooler_manifest(pooler.as_str(), &name), READY)
        .await
        .expect("apply pooler");

    let client = podman();
    query(
        &client,
        &pooler,
        &password,
        "CREATE TABLE t(id serial primary key, note text)",
    )
    .await
    .expect("create a table through the pooler");
    query(
        &client,
        &pooler,
        &password,
        "INSERT INTO t(note) SELECT 'before' FROM generate_series(1, 5000)",
    )
    .await
    .expect("write rows before the upgrade");

    // ---- the acceptance property --------------------------------------
    //
    // Commit continuously through the pooler while PostgreSQL is replaced
    // underneath by a *different major version*. Every transaction must
    // land. Its own thread with its own runtime, because `Pgpod` owns a
    // rusqlite connection and is not `Send` — and a client sharing this
    // test's handle would not be an independent client anyway.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let committer = {
        let stop = std::sync::Arc::clone(&stop);
        let (pooler, password) = (pooler.clone(), password.clone());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("client runtime");
            rt.block_on(async move {
                let podman = podman();
                let mut ok = 0u32;
                let mut failures = Vec::new();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    match query(
                        &podman,
                        &pooler,
                        &password,
                        "INSERT INTO t(note) VALUES ('during')",
                    )
                    .await
                    {
                        Ok(_) => ok += 1,
                        Err(e) => failures.push(e),
                    }
                }
                (ok, failures)
            })
        })
    };

    // pg_doorman instantiates a pool on first connection, and a pool that
    // does not exist yet is nothing to hold.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let report = pg
        .upgrade(
            &cluster,
            UpgradeOptions {
                to_image: TO_IMAGE.to_string(),
                method: UpgradeMethod::Link,
                jobs: 2,
                check: false,
                analyze: true,
                // This cluster archives nowhere, so there is nothing to
                // back up. Covered separately by the archiving test below.
                backup: false,
                wait: READY,
            },
        )
        .await
        .expect("upgrade 17 -> 18");

    tokio::time::sleep(Duration::from_secs(2)).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (committed, failures) = committer.join().expect("client thread");

    assert_eq!(report.from_version, "17");
    assert_eq!(report.to_version, "18");
    assert_eq!(
        report.poolers_held, 1,
        "the pooler should have held clients"
    );
    assert!(
        !report.old_data_dir.is_empty(),
        "the pre-upgrade data directory must be kept and named"
    );

    assert!(
        committed > 0,
        "the client never committed anything, so the test proved nothing"
    );
    assert!(
        failures.is_empty(),
        "{} of {} transactions failed across the upgrade — the hold did not \
         cover the window ({}ms held, {}s in pg_upgrade).\nfirst failure: {}",
        failures.len(),
        committed as usize + failures.len(),
        report.held_ms,
        report.upgrade_seconds,
        failures.first().map(String::as_str).unwrap_or(""),
    );

    // The server really is the new major version...
    let version = server_version(&pg, &cluster).await;
    assert!(
        version.starts_with("18"),
        "expected PostgreSQL 18 after the upgrade, got {version}"
    );

    // ...and every row is there: the 5000 written before, plus everything
    // the client committed across the window. Asserted through the pooler,
    // on the same connection string the application has been using
    // throughout.
    let rows = query(&client, &pooler, &password, "SELECT count(*) FROM t")
        .await
        .expect("count after the upgrade");
    assert_eq!(
        rows,
        (5000 + committed).to_string(),
        "5000 rows were written before the upgrade and {committed} during it"
    );
    let before = query(
        &client,
        &pooler,
        &password,
        "SELECT count(*) FROM t WHERE note = 'before'",
    )
    .await
    .expect("count the pre-upgrade rows");
    assert_eq!(before, "5000", "pre-upgrade rows must survive verbatim");

    // The registry has to agree with the disk, or the next `apply` starts
    // a PostgreSQL 17 against a PostgreSQL 18 data directory.
    let stored = pg
        .registry()
        .require_cluster(&cluster)
        .expect("cluster row")
        .manifest;
    assert_eq!(stored.spec.image_name, TO_IMAGE);

    // An `apply` of the upgraded manifest is a no-op rather than a
    // divergence: the upgrade must leave a container indistinguishable
    // from one apply would have made.
    pg.apply(&cluster_manifest(&name, TO_IMAGE), READY)
        .await
        .expect("apply after the upgrade must not report divergence");

    // And the stale half of the same story: `pgpod upgrade` rewrites what
    // pgpod stores but not the operator's file, so the next `apply -f` of
    // an un-edited cluster.yaml still names PostgreSQL 17. Applying it
    // would start an older server on a newer data directory, which fails
    // the same way in reverse.
    let err = pg
        .apply(&cluster_manifest(&name, FROM_IMAGE), READY)
        .await
        .expect_err("a manifest older than the data must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("older") && msg.contains("18"),
        "the refusal must say which way round it is: {msg}"
    );
    assert!(
        server_version(&pg, &cluster).await.starts_with("18"),
        "and must leave the upgraded cluster running"
    );

    let _ = pg.delete_pooler(&pooler).await;
    let _ = pg.delete(&cluster, true).await;
}

#[tokio::test]
async fn a_rehearsal_changes_nothing_and_says_whether_it_would_work() {
    // `--check` is what an operator runs the day before. It costs a short
    // outage — pg_upgrade cannot read a running cluster — and has to end
    // with the cluster exactly as it was, on its original image.
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("c{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();

    let _ = pg.delete(&cluster, true).await;
    pg.apply(&cluster_manifest(&name, FROM_IMAGE), READY)
        .await
        .expect("apply the 17 cluster");

    let container = pg
        .running_container(&cluster.instance(1))
        .await
        .expect("instance up");
    container
        .exec(&ExecSpec::new([
            "psql",
            "-X",
            "-q",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            pgpod_core::container::SOCKET_DIR,
            "-U",
            "postgres",
            "-d",
            "appdb",
            "-c",
            "CREATE TABLE kept(i int); INSERT INTO kept SELECT generate_series(1,100)",
        ]))
        .await
        .expect("write rows before the rehearsal");

    let report = pg
        .upgrade(
            &cluster,
            UpgradeOptions {
                to_image: TO_IMAGE.to_string(),
                check: true,
                backup: false,
                analyze: false,
                ..Default::default()
            },
        )
        .await
        .expect("check must pass for a cluster that can be upgraded");
    assert!(report.checked_only);
    assert!(
        report.old_data_dir.is_empty(),
        "a rehearsal must not move the data directory"
    );

    // Still 17, still running, still holding its rows.
    assert!(
        server_version(&pg, &cluster).await.starts_with("17"),
        "a rehearsal must leave the cluster on its original major version"
    );
    // Including the phase. `--check` opens the same window a real upgrade
    // does, so it fences the cluster the same way — and a rehearsal that
    // left `upgrading` behind would leave the cluster looking mid-upgrade
    // to anything that reads the phase before acting.
    assert_eq!(
        pg.status(&cluster).await.expect("status").phase,
        "running",
        "a rehearsal must put the phase back"
    );
    let stored = pg
        .registry()
        .require_cluster(&cluster)
        .expect("cluster row")
        .manifest;
    assert_eq!(
        stored.spec.image_name, FROM_IMAGE,
        "a rehearsal must not rewrite the stored manifest"
    );
    let rows = pg
        .running_container(&cluster.instance(1))
        .await
        .expect("instance up")
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
            "SELECT count(*) FROM kept",
        ]))
        .await
        .expect("count");
    assert_eq!(rows.stdout.trim(), "100");

    let _ = pg.delete(&cluster, true).await;
}

#[tokio::test]
async fn a_downgrade_and_a_same_version_image_are_both_refused() {
    // Both refusals happen before anything is stopped, which is the
    // property worth asserting: a cluster that is serving traffic must
    // not be taken down to discover that the target image was wrong.
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("d{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();

    let _ = pg.delete(&cluster, true).await;
    pg.apply(&cluster_manifest(&name, TO_IMAGE), READY)
        .await
        .expect("apply an 18 cluster");

    let err = pg
        .upgrade(
            &cluster,
            UpgradeOptions {
                to_image: FROM_IMAGE.to_string(),
                backup: false,
                analyze: false,
                ..Default::default()
            },
        )
        .await
        .expect_err("downgrading must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("older"),
        "the refusal must say the target is older: {msg}"
    );

    // A different tag of the same major version is not an upgrade, and
    // saying so points at the command that does do it.
    let err = pg
        .upgrade(
            &cluster,
            UpgradeOptions {
                to_image: "docker.io/library/postgres:18-alpine".to_string(),
                backup: false,
                analyze: false,
                ..Default::default()
            },
        )
        .await
        .expect_err("a same-major image must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("apply --recreate"),
        "the refusal must name the command that does do this: {msg}"
    );

    // Still up, still serving, on the image it started on.
    assert!(server_version(&pg, &cluster).await.starts_with("18"));

    let _ = pg.delete(&cluster, true).await;
}

#[tokio::test]
async fn an_upgraded_cluster_keeps_archiving_and_its_old_backups_stay_restorable() {
    // The data-loss-critical half of an upgrade, and the half that is
    // silent when it breaks. A major upgrade gives the cluster a new
    // system identifier, and pgBackRest checks it on every
    // `archive-push`: without `stanza-upgrade` the first segment after
    // the upgrade is rejected, `archive_command` fails from then on, and
    // the only symptom is `pg_wal` growing.
    //
    // The second half is the trap it creates. One stanza now holds
    // backups of two PostgreSQL versions, and pgBackRest will restore
    // either — so a PITR target from before the upgrade has to be refused
    // by pgpod, or it produces a data directory the cluster's image
    // cannot start.
    // This one needs the pgBackRest bundle, where the other four do not.
    // Asserted by name rather than skipped: a backup path that silently
    // does not run is how an upgrade ships with archiving broken.
    let bundle = PathLayout::from_env().pgbackrest_bundle_for_host();
    assert!(
        bundle.join("bin/pgbackrest").exists(),
        "pgbackrest bundle missing at {} — run ops/build-pgbackrest.sh first",
        bundle.display()
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("b{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();
    let restored = ClusterId::new(format!("{name}-old")).unwrap();
    let archive = format!("pgpod-{name}-archive");

    let manifest = |image: &str| {
        ClusterManifest::from_yaml(&format!(
            r#"
apiVersion: pgpod/v1
kind: Cluster
metadata:
  name: {name}
spec:
  imageName: {image}
  bootstrap:
    initdb:
      database: appdb
      owner: app
  backup:
    volume: {archive}
    destinations:
      - url: file:///archive/{name}
"#
        ))
        .expect("manifest parses")
    };

    let _ = pg.delete(&restored, true).await;
    let _ = pg.delete(&cluster, true).await;
    pg.apply(&manifest(FROM_IMAGE), READY)
        .await
        .expect("apply the 17 cluster");

    let write = |sql: &'static str| {
        let pg = &pg;
        let cluster = cluster.clone();
        async move {
            pg.running_container(&cluster.instance(1))
                .await
                .expect("instance up")
                .exec(&ExecSpec::new([
                    "psql",
                    "-X",
                    "-q",
                    "-v",
                    "ON_ERROR_STOP=1",
                    "-h",
                    pgpod_core::container::SOCKET_DIR,
                    "-U",
                    "postgres",
                    "-d",
                    "appdb",
                    "-c",
                    sql,
                ]))
                .await
                .expect("write")
        }
    };
    write("CREATE TABLE t(i int primary key); INSERT INTO t SELECT generate_series(1,1000)").await;

    let before = pg
        .backup(&cluster, Duration::from_secs(900))
        .await
        .expect("pre-upgrade backup");
    let before_label = before.label.expect("the repository recorded the backup");
    // The instant to aim a PITR at. Read back from the repository rather
    // than the clock: the repository is the record of what is restorable
    // (ADR 04 §5).
    let before_ended = pg
        .backups(&cluster)
        .await
        .expect("list backups")
        .into_iter()
        .find(|b| b.label == before_label)
        .expect("the backup is in the repository")
        .ended_at();

    // Rows written *after* the backup, with a segment switch behind them,
    // for two reasons. A recovery target has to lie inside the WAL the
    // repository actually holds — PostgreSQL FATALs with "recovery ended
    // before configured recovery target was reached" if it does not, which
    // is a property of PITR rather than of upgrades. And they are what
    // makes the restore below an assertion about *time*: they must not
    // come back.
    tokio::time::sleep(Duration::from_secs(2)).await;
    write("INSERT INTO t SELECT generate_series(10001,11000)").await;
    write("SELECT pg_switch_wal()").await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let report = pg
        .upgrade(
            &cluster,
            UpgradeOptions {
                to_image: TO_IMAGE.to_string(),
                wait: READY,
                ..Default::default()
            },
        )
        .await
        .expect("upgrade 17 -> 18");
    assert!(
        report.backup_label.is_some(),
        "the post-upgrade backup is what makes the new cluster restorable \
         at all — warnings were: {:?}",
        report.warnings
    );

    // Archiving works, which is what `stanza-upgrade` bought. Asserted
    // against pg_stat_archiver rather than against the absence of an
    // error: a failing archive_command is retried silently forever.
    write("INSERT INTO t SELECT generate_series(1001,2000)").await;
    let switched = pg
        .running_container(&cluster.instance(1))
        .await
        .expect("instance up")
        .exec(&ExecSpec::new([
            "psql",
            "-X",
            "-tA",
            "-h",
            pgpod_core::container::SOCKET_DIR,
            "-U",
            "postgres",
            "-c",
            "SELECT pg_switch_wal(); SELECT pg_sleep(5); \
             SELECT failed_count = 0 AND archived_count > 0 FROM pg_stat_archiver",
        ]))
        .await
        .expect("switch a segment and read the archiver");
    assert_eq!(
        switched.stdout.trim().lines().last().unwrap_or("").trim(),
        "t",
        "WAL is not reaching the repository after the upgrade — \
         stanza-upgrade did not happen: {}",
        switched.stdout
    );

    // The pre-upgrade backup is still in the repository, and restoring it
    // into the cluster's *current* image would produce a PostgreSQL 17
    // data directory that PostgreSQL 18 refuses to open.
    let err = pg
        .restore(
            &cluster,
            &restored,
            Some(before_ended),
            None,
            Duration::from_secs(900),
        )
        .await
        .expect_err("restoring a pre-upgrade backup onto the new image must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("--image") && msg.contains("17"),
        "the refusal has to name the way to do it: {msg}"
    );
    assert!(
        msg.contains(&before_label),
        "and the backup it is about: {msg}"
    );

    // With the version it was taken by, it restores — 1000 rows, on 17,
    // out of the same repository the upgraded cluster is still writing to.
    pg.restore(
        &cluster,
        &restored,
        Some(before_ended),
        Some(FROM_IMAGE),
        Duration::from_secs(900),
    )
    .await
    .expect("restoring with a 17 image");
    assert!(
        server_version(&pg, &restored).await.starts_with("17"),
        "the restored cluster must be the version its backup was taken by"
    );
    let rows = pg
        .running_container(&restored.instance(1))
        .await
        .expect("restored instance up")
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
            "SELECT count(*) FROM t",
        ]))
        .await
        .expect("count the restored rows");
    assert_eq!(
        rows.stdout.trim(),
        "1000",
        "the restore must land on the target time: the 1000 rows written \
         after the backup are not part of it"
    );

    let _ = pg.delete(&restored, true).await;
    let _ = pg.delete(&cluster, true).await;
}

#[tokio::test]
async fn an_upgrade_the_images_cannot_support_leaves_the_cluster_running() {
    // glibc to musl: the staged PostgreSQL 17 binaries cannot execute in
    // an Alpine image at all, because the loader they were linked against
    // is not there. This is the one compatibility assumption the design
    // makes (ADR 06 §3), and the point of checking it by *running* the
    // staged `pg_ctl` is that the failure lands before the data moves.
    //
    // It is also the only end-to-end exercise of the rollback: the
    // instance has already been stopped and removed by the time this
    // fails, so the cluster only comes back if the failure path really
    // does start it again.
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("r{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();

    let _ = pg.delete(&cluster, true).await;
    pg.apply(
        &cluster_manifest(&name, "docker.io/library/postgres:17"),
        READY,
    )
    .await
    .expect("apply the 17 cluster");
    let container_before = pg
        .running_container(&cluster.instance(1))
        .await
        .expect("instance up");
    container_before
        .exec(&ExecSpec::new([
            "psql",
            "-X",
            "-q",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            pgpod_core::container::SOCKET_DIR,
            "-U",
            "postgres",
            "-d",
            "appdb",
            "-c",
            "CREATE TABLE t(i int); INSERT INTO t VALUES (1),(2),(3)",
        ]))
        .await
        .expect("write rows");

    let err = pg
        .upgrade(
            &cluster,
            UpgradeOptions {
                to_image: "docker.io/library/postgres:18-alpine".to_string(),
                analyze: false,
                backup: false,
                wait: READY,
                ..Default::default()
            },
        )
        .await
        .expect_err("staged glibc binaries cannot run in a musl image");
    let msg = err.to_string();
    assert!(
        msg.contains("cannot run in this image"),
        "the failure must be the compatibility check rather than something \
         further in: {msg}"
    );
    assert!(
        msg.contains("the cluster is untouched"),
        "and it must say the data was not touched: {msg}"
    );

    // Back up, on its own image, with its rows — the rollback. The phase
    // is part of that: nothing was swapped, the old instance is serving,
    // so the cluster is exactly what it was before the window opened.
    assert_eq!(
        pg.status(&cluster).await.expect("status").phase,
        "running",
        "a rolled-back upgrade must not leave the phase at `upgrading`"
    );
    assert!(
        server_version(&pg, &cluster).await.starts_with("17"),
        "the failed upgrade must leave the cluster running its old version"
    );
    let rows = pg
        .running_container(&cluster.instance(1))
        .await
        .expect("instance back up")
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
            "SELECT count(*) FROM t",
        ]))
        .await
        .expect("count");
    assert_eq!(rows.stdout.trim(), "3");
    assert_eq!(
        pg.registry()
            .require_cluster(&cluster)
            .expect("cluster row")
            .manifest
            .spec
            .image_name,
        "docker.io/library/postgres:17",
        "a failed upgrade must not rewrite the stored manifest"
    );

    let _ = pg.delete(&cluster, true).await;
}

#[tokio::test]
async fn applying_a_manifest_with_a_newer_major_image_is_refused_and_names_upgrade() {
    // The mistake this is here for: an operator edits `imageName` from 17
    // to 18 and applies, because that is how every other manifest change
    // works. It cannot work — `apply` would start PostgreSQL 18 on a
    // PostgreSQL 17 data directory, which fails with "database files are
    // incompatible with server" and leaves an instance that never becomes
    // ready.
    //
    // Before this check, the same edit was worse than refused: the image
    // is not part of the instance spec, so the divergence comparison could
    // not see it, and `apply` reported "converged" having changed
    // nothing at all.
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("i{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();

    let _ = pg.delete(&cluster, true).await;
    pg.apply(&cluster_manifest(&name, FROM_IMAGE), READY)
        .await
        .expect("apply the 17 cluster");

    for recreate in [false, true] {
        // `--recreate` must not be a way through: it is permission to
        // replace a container, not permission to run pg_upgrade.
        let err = pg
            .apply_with(
                &cluster_manifest(&name, TO_IMAGE),
                pgpod_control::ApplyOptions {
                    wait: READY,
                    recreate,
                },
            )
            .await
            .expect_err("a major-version image bump must be refused (recreate: {recreate})");
        let msg = err.to_string();
        assert!(
            msg.contains("pgpod upgrade") && msg.contains(&name),
            "the refusal must name the command that does do this: {msg}"
        );
        assert!(
            msg.contains("Nothing has been changed"),
            "and must say the cluster was left alone: {msg}"
        );
    }

    // Still up, still 17, still serving — the refusals cost nothing.
    assert!(server_version(&pg, &cluster).await.starts_with("17"));

    let _ = pg.delete(&cluster, true).await;
}

#[tokio::test]
async fn a_same_major_image_change_is_an_ordinary_recreate() {
    // The other half of the same rule. Moving between two images of the
    // *same* PostgreSQL — a minor-version patch, or a different base — is
    // a real change that needs the container replaced, and `apply` has to
    // both notice it and be able to apply it. Until the image was part of
    // the comparison, neither happened: a patched image was adopted
    // silently, so there was no way to take a PostgreSQL security update
    // through `apply` at all.
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("s{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();
    // Both are PostgreSQL 17, from different Debian releases.
    let (before, after) = ("docker.io/library/postgres:17", FROM_IMAGE);

    let _ = pg.delete(&cluster, true).await;
    pg.apply(&cluster_manifest(&name, before), READY)
        .await
        .expect("apply the first 17 image");
    pg.running_container(&cluster.instance(1))
        .await
        .expect("instance up")
        .exec(&ExecSpec::new([
            "psql",
            "-X",
            "-q",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            pgpod_core::container::SOCKET_DIR,
            "-U",
            "postgres",
            "-d",
            "appdb",
            "-c",
            "CREATE TABLE t(i int); INSERT INTO t SELECT generate_series(1,50)",
        ]))
        .await
        .expect("write rows");

    // Noticed, and refused by default like any other spec change.
    let err = pg
        .apply(&cluster_manifest(&name, after), READY)
        .await
        .expect_err("a changed image is a change, and must not be adopted silently");
    let msg = err.to_string();
    assert!(
        msg.contains("image:") && msg.contains(after),
        "the diff must name the image and what it is changing to: {msg}"
    );

    // And applied when the operator says so.
    pg.apply_with(
        &cluster_manifest(&name, after),
        pgpod_control::ApplyOptions {
            wait: READY,
            recreate: true,
        },
    )
    .await
    .expect("--recreate applies a same-major image change");

    let version = pg
        .running_container(&cluster.instance(1))
        .await
        .expect("instance up")
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
            "SELECT version(), count(*) FROM t",
        ]))
        .await
        .expect("ask the new container what it is");
    assert!(
        version.stdout.contains("pgdg12"),
        "the container must be running the bookworm image now: {}",
        version.stdout
    );
    assert!(
        version.stdout.trim().ends_with("|50"),
        "and the data must have survived the swap: {}",
        version.stdout
    );

    let _ = pg.delete(&cluster, true).await;
}
