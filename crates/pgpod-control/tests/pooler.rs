//! The pooler acceptance test: a changed manifest applied to a running
//! cluster without dropping a connection.
//!
//! This is the phase's whole point, so it is asserted the way the PITR
//! test asserts rows rather than counts: a client commits continuously
//! through the pooler while `apply --recreate` replaces the instance
//! container underneath it, and **every** transaction has to succeed.
//!
//! Measured by hand on the deployment target before this existed, the same
//! recreate without a hold lost 32 of 66 transactions. That is the number
//! this test exists to keep at zero.
//!
//! ```sh
//! ops/build-agent.sh
//! eval "$(ops/dev-podman.sh start)"
//! cargo test -p pgpod-control --features podman-tests --test pooler -- --nocapture
//! ```

#![cfg(feature = "podman-tests")]

use std::time::Duration;

use pgpod_control::{ApplyOptions, Pgpod};
use pgpod_core::{ClusterId, ClusterManifest, PathLayout, PoolerId, PoolerManifest};
use pgpod_registry::Registry;
use pgpod_runtime::{ExecSpec, PodmanClient};

const IMAGE: &str = "docker.io/library/postgres:18";
const READY: Duration = Duration::from_secs(180);

fn cluster_manifest(name: &str, work_mem: &str) -> ClusterManifest {
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
      work_mem: {work_mem}
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

/// A bare podman client, for the load generator.
///
/// It needs no registry and no agent: it only execs `psql` inside the
/// pooler container. Building a whole `Pgpod` there would re-copy the
/// agent binary, which is bind-mounted into running containers and
/// therefore `ETXTBSY`.
fn podman() -> PodmanClient {
    PodmanClient::connect().expect("podman socket — run: eval \"$(ops/dev-podman.sh start)\"")
}

/// Run a query through the pooler, given a raw podman client.
async fn query(
    podman: &PodmanClient,
    pooler: &PoolerId,
    password: &str,
    pool: &str,
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
                pool,
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

/// Run a query **through the pooler**, from inside the pooler container.
///
/// From in there because it is the one place guaranteed to reach the
/// published port regardless of what the host's loopback looks like, and
/// the upstream image ships `psql`. The password comes from the podman
/// secret pgpod generated, so the test authenticates exactly as an
/// application would: SCRAM to the pooler, `auth_query` behind it.
async fn through_pooler(
    pg: &Pgpod,
    pooler: &PoolerId,
    password: &str,
    pool: &str,
    sql: &str,
) -> Result<String, String> {
    query(pg.podman_client(), pooler, password, pool, sql).await
}

/// The app owner's password, as pgpod generated it.
async fn app_password(pg: &Pgpod, cluster: &ClusterId) -> String {
    // Read through the instance rather than podman: pgpod deliberately has
    // no "read a secret back" primitive, and adding one for a test would
    // put it in the product.
    let instance = cluster.instance(1);
    let container = pg.running_container(&instance).await.expect("instance up");
    let out = container
        .exec(&ExecSpec::new([
            "cat",
            pgpod_core::container::SECRET_APP_OWNER,
        ]))
        .await
        .expect("read the mounted secret");
    out.stdout.trim().to_string()
}

#[tokio::test]
async fn a_changed_manifest_is_applied_without_dropping_a_connection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("p{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();
    let pooler = PoolerId::new(format!("{name}-pool")).unwrap();

    let _ = pg.delete_pooler(&pooler).await;
    let _ = pg.delete(&cluster, true).await;

    pg.apply(&cluster_manifest(&name, "4MB"), READY)
        .await
        .expect("apply cluster");
    let password = app_password(&pg, &cluster).await;

    let report = pg
        .apply_pooler(&pooler_manifest(pooler.as_str(), &name), READY)
        .await
        .expect("apply pooler");
    assert!(report.created);
    assert_eq!(report.pools.len(), 1, "one pool for the app database");
    assert_eq!(
        report.pools[0].name, "appdb",
        "a pool defaults to the database's own name, which is what clients type"
    );

    // The SCRAM-passthrough path: the client authenticates to the pooler
    // with the app role's password, and the pooler replays the ClientKey
    // to PostgreSQL. Nothing in the pooler's config knows this password.
    let who = through_pooler(&pg, &pooler, &password, "appdb", "SELECT current_user")
        .await
        .expect("connect through the pooler");
    assert_eq!(who, "app");

    through_pooler(
        &pg,
        &pooler,
        &password,
        "appdb",
        "CREATE TABLE t(id serial primary key)",
    )
    .await
    .expect("create a table through the pooler");

    let before = pg
        .podman_client()
        .container(cluster.instance(1).container_name())
        .probe()
        .await
        .expect("probe")
        .expect("instance exists")
        .id;

    // ---- the acceptance property --------------------------------------
    //
    // Commit continuously through the pooler while the instance container
    // is replaced underneath. Every transaction must land.
    // On its own thread with its own runtime, not a `tokio::spawn`:
    // `Pgpod` owns a rusqlite connection and is not `Send`, and a client
    // that shared this test's handle would not be an independent client
    // anyway.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let client = {
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
                        "appdb",
                        "INSERT INTO t DEFAULT VALUES",
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

    // Let the client establish the pool before anything moves: pg_doorman
    // creates a pool on the first connection, and a pool that does not
    // exist yet has nothing to hold.
    tokio::time::sleep(Duration::from_secs(3)).await;

    pg.apply_with(
        &cluster_manifest(&name, "8MB"),
        ApplyOptions {
            wait: READY,
            recreate: true,
        },
    )
    .await
    .expect("apply --recreate");

    tokio::time::sleep(Duration::from_secs(2)).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (committed, failures) = client.join().expect("client thread");

    assert!(
        committed > 0,
        "the client never committed anything, so the test proved nothing"
    );
    assert!(
        failures.is_empty(),
        "{} of {} transactions failed across the recreate — the hold did not \
         cover the window.\nfirst failure: {}",
        failures.len(),
        committed as usize + failures.len(),
        failures.first().map(String::as_str).unwrap_or(""),
    );

    // The container really was replaced, and the new spec really is live —
    // otherwise "no connections dropped" would be trivially true.
    let after = pg
        .podman_client()
        .container(cluster.instance(1).container_name())
        .probe()
        .await
        .expect("probe")
        .expect("instance exists")
        .id;
    assert_ne!(before, after, "the container was not actually recreated");
    let work_mem = through_pooler(&pg, &pooler, &password, "appdb", "SHOW work_mem")
        .await
        .expect("query after the recreate");
    assert_eq!(work_mem, "8MB", "the changed parameter did not take effect");

    // Every row the client thought it committed is there.
    let rows = through_pooler(&pg, &pooler, &password, "appdb", "SELECT count(*) FROM t")
        .await
        .expect("count");
    assert_eq!(
        rows,
        committed.to_string(),
        "the client committed {committed} rows but {rows} survived"
    );

    let _ = pg.delete_pooler(&pooler).await;
    let _ = pg.delete(&cluster, true).await;
}

#[tokio::test]
async fn a_cluster_a_pooler_fronts_cannot_be_deleted_out_from_under_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("q{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();
    let pooler = PoolerId::new(format!("{name}-pool")).unwrap();

    let _ = pg.delete_pooler(&pooler).await;
    let _ = pg.delete(&cluster, true).await;

    pg.apply(&cluster_manifest(&name, "4MB"), READY)
        .await
        .expect("apply cluster");
    pg.apply_pooler(&pooler_manifest(pooler.as_str(), &name), READY)
        .await
        .expect("apply pooler");

    // `--purge` destroys the volumes, so it is refused by name.
    let err = pg
        .delete(&cluster, true)
        .await
        .expect_err("purging a fronted cluster must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains(pooler.as_str()),
        "the error must name the pooler blocking the delete: {msg}"
    );

    // **Refused before anything was destroyed.** This is the half that
    // matters: the registry would have refused the row deletion anyway,
    // but only after the containers were gone and the volumes purged, so
    // the operator would read "refused" over the wreckage of their data.
    assert!(
        pg.podman_volume(&cluster.instance(1).volume_name())
            .await
            .expect("look up the volume")
            .is_some(),
        "the refused --purge destroyed the volume anyway"
    );
    assert!(
        pg.podman_client()
            .container(pooler.container_name())
            .probe()
            .await
            .expect("probe")
            .is_some(),
        "the refused --purge removed the pooler container"
    );
    assert!(
        pg.running_container(&cluster.instance(1)).await.is_ok(),
        "the refused --purge stopped the instance"
    );

    pg.delete_pooler(&pooler).await.expect("delete pooler");
    pg.delete(&cluster, true).await.expect("now it deletes");
}

#[tokio::test]
async fn one_pooler_fronts_several_databases_on_one_port() {
    // Pools are addressed by the name a client puts in `dbname`, so one
    // published port serves them all. Asserted live rather than by
    // construction: the shape is easy to get right in a unit test and
    // wrong against a real pg_doorman, which instantiates a pool per
    // user×database on first connection.
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());
    let name = format!("m{}", std::process::id() % 100_000);
    let cluster = ClusterId::new(name.clone()).unwrap();
    let pooler = PoolerId::new(format!("{name}-pool")).unwrap();

    let _ = pg.delete_pooler(&pooler).await;
    let _ = pg.delete(&cluster, true).await;

    pg.apply(&cluster_manifest(&name, "4MB"), READY)
        .await
        .expect("apply cluster");
    let password = app_password(&pg, &cluster).await;

    // A second database, created the way an operator would.
    let instance = cluster.instance(1);
    pg.running_container(&instance)
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
            "postgres",
            "-c",
            "CREATE DATABASE analytics OWNER app",
        ]))
        .await
        .expect("exec")
        .require_success("create the second database")
        .expect("second database");

    let manifest = PoolerManifest::from_yaml(&format!(
        r#"
apiVersion: pgpod/v1
kind: Pooler
metadata:
  name: {pooler}
spec:
  clusters:
    - cluster: {name}
      pools:
        - database: appdb
        - database: analytics
  pgDoorman:
    maxHold: "90s"
"#
    ))
    .expect("pooler manifest parses");

    let report = pg
        .apply_pooler(&manifest, READY)
        .await
        .expect("apply pooler");
    assert_eq!(report.pools.len(), 2);

    // One port, two dbnames, and each lands in its own database.
    for db in ["appdb", "analytics"] {
        let got = through_pooler(&pg, &pooler, &password, db, "SELECT current_database()")
            .await
            .unwrap_or_else(|e| panic!("connecting to {db} through the pooler: {e}"));
        assert_eq!(got, db, "dbname={db} reached the wrong database");
    }

    // Both pools exist at the pooler, not just at PostgreSQL.
    let pools = pg.pooler_pools(&pooler).await.expect("show pools");
    let mut names: Vec<&str> = pools.iter().map(|p| p.database.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["analytics", "appdb"]);

    // And changing the pool set is refused with pooler-shaped advice: a
    // pooler has no volume, so the instance message would be wrong.
    let fewer = PoolerManifest::from_yaml(&format!(
        r#"
apiVersion: pgpod/v1
kind: Pooler
metadata:
  name: {pooler}
spec:
  clusters:
    - cluster: {name}
      pools:
        - database: appdb
  pgDoorman:
    maxHold: "90s"
"#
    ))
    .expect("parses");
    let err = pg
        .apply_pooler(&fewer, Duration::from_secs(0))
        .await
        .expect_err("a changed pool set must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("pool removed") && msg.contains("analytics"),
        "the error must name what changed: {msg}"
    );
    assert!(
        msg.contains("pgpod pooler delete"),
        "the error must name the pooler command, not the cluster one: {msg}"
    );
    assert!(
        !msg.contains("keeps the volume"),
        "a pooler has no volume; this is the instance message leaking: {msg}"
    );

    // **A refused apply must not have rewritten the registry.** `apply`
    // replaces the pool rows wholesale, so checking divergence afterwards
    // would leave `pgpod pooler list` describing a pooler that was never
    // created — the wrong pools, and a phase stuck at "applying".
    let stored = pg
        .registry()
        .require_pooler(&pooler)
        .expect("the pooler row survives a refused apply");
    let mut still: Vec<&str> = stored.pools.iter().map(|p| p.pool_name.as_str()).collect();
    still.sort_unstable();
    assert_eq!(
        still,
        ["analytics", "appdb"],
        "a refused apply rewrote the registry to a manifest that never took effect"
    );
    assert_eq!(
        stored.phase, "running",
        "a refused apply left the phase at {:?}",
        stored.phase
    );

    let _ = pg.delete_pooler(&pooler).await;
    let _ = pg.delete(&cluster, true).await;
}
