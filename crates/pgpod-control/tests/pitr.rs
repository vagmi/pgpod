//! **Phase 2's acceptance test.**
//!
//! ```text
//! create → write rows → backup → write more → note T → write more
//!        → restore --at T → exactly the rows as of T
//! ```
//!
//! Nothing else in Phase 2 matters if this does not hold. Under the volume
//! storage model object storage is the *only* supported way to get data
//! out of a pgpod cluster (ADR 00, consequences), so this is not a backup
//! feature test — it is the test that the data is reachable at all.
//!
//! It runs against a `posix` repository in a podman volume, which is the
//! fast local loop. The same assertions are what an S3 or GCS repository
//! has to satisfy; the destination is the only thing that changes.
//!
//! The image is the stock `postgres:18` deliberately — pgBackRest reaches
//! it as a bind-mounted bundle, not baked in, and this test failing on an
//! unmodified image is how that promise stays honest (ADR 04 §2).
//!
//! ```sh
//! ops/build-agent.sh
//! eval "$(ops/dev-podman.sh start)"
//! cargo test -p pgpod-control --features podman-tests --test pitr -- --nocapture
//! ```

#![cfg(feature = "podman-tests")]

use std::time::Duration;

use pgpod_control::Pgpod;
use pgpod_core::{ClusterId, ClusterManifest, PathLayout, container};
use pgpod_registry::Registry;
use pgpod_runtime::{ExecSpec, PodmanClient};

const IMAGE: &str = "docker.io/library/postgres:18";
const READY_TIMEOUT: Duration = Duration::from_secs(240);
const BACKUP_TIMEOUT: Duration = Duration::from_secs(900);

fn manifest(name: &str, archive_volume: &str) -> ClusterManifest {
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
  backup:
    volume: {archive_volume}
    destinations:
      - url: file://{mount}/{name}
"#,
        mount = container::ARCHIVE_MOUNT
    ))
    .expect("manifest parses")
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

    // The pgBackRest bundle is ~19 MB of binary and libraries; symlink it
    // into the scratch layout rather than copying it per test.
    let real_bundle = PathLayout::from_env().pgbackrest_bundle_for_host();
    assert!(
        real_bundle.join("bin/pgbackrest").exists(),
        "pgbackrest bundle missing at {} — run ops/build-pgbackrest.sh first",
        real_bundle.display()
    );
    std::os::unix::fs::symlink(&real_bundle, layout.pgbackrest_bundle_for_host())
        .expect("link the pgbackrest bundle");

    let podman =
        PodmanClient::connect().expect("podman socket — run: eval \"$(ops/dev-podman.sh start)\"");
    let registry = Registry::open(&layout.registry_db()).expect("open registry");
    Pgpod::new(podman, registry, layout)
}

async fn sql(pg: &Pgpod, instance: &str, database: &str, query: &str) -> String {
    let id: pgpod_core::InstanceId = instance.parse().expect("instance id");
    let container = pg.running_container(&id).await.expect("running container");
    let out = container
        .exec(&ExecSpec::new([
            "psql",
            "-X",
            "-tA",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            container::SOCKET_DIR,
            "-U",
            "postgres",
            "-d",
            database,
            "-c",
            query,
        ]))
        .await
        .expect("exec psql");
    out.clone()
        .require_success("psql")
        .unwrap_or_else(|e| panic!("{query}\n{e}\nstderr: {}", out.stderr));
    out.stdout.trim().to_string()
}

/// The whole point of the phase, against a local `posix` repository.
#[tokio::test]
async fn a_cluster_can_be_restored_to_a_point_in_time() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());

    let suffix = std::process::id() % 100_000;
    let name = format!("p{suffix}");
    let restored_name = format!("p{suffix}r");
    let archive_volume = format!("pgpod-{name}-archive");

    let cluster = ClusterId::new(name.clone()).unwrap();
    let restored = ClusterId::new(restored_name.clone()).unwrap();
    let m = manifest(&name, &archive_volume);

    // Best effort cleanup of a previous failed run, archive included —
    // stale WAL under the same keys would trip the replay guard, which is
    // the guard doing its job and not what this test is about.
    let _ = pg.delete(&cluster, true).await;
    let _ = pg.delete(&restored, true).await;
    let _ = pg
        .podman_client()
        .remove_volume_destroying_data(&archive_volume)
        .await;

    pg.apply(&m, READY_TIMEOUT).await.expect("apply");
    let instance = format!("{name}-1");

    // A destination in the manifest turns archiving on before postgres
    // ever starts — no restart, no second apply (ADR 01 §1).
    assert_eq!(
        sql(&pg, &instance, "postgres", "SHOW archive_mode").await,
        "on",
        "a cluster created with a destination must archive from its first segment"
    );
    let archive_command = sql(&pg, &instance, "postgres", "SHOW archive_command").await;
    assert!(
        archive_command.contains("archive-push"),
        "archive_command must invoke pgbackrest directly, with no pgpod \
         process in between (ADR 04 §1): {archive_command}"
    );
    assert!(
        archive_command.contains(container::PGBACKREST_BIN),
        "and through the bundle's wrapper, not whatever is on PATH: \
         {archive_command}"
    );

    // ---- write, back up, write, mark T, write ------------------------

    sql(
        &pg,
        &instance,
        "appdb",
        "CREATE TABLE t (id int primary key, tag text)",
    )
    .await;
    sql(
        &pg,
        &instance,
        "appdb",
        "INSERT INTO t VALUES (1, 'before')",
    )
    .await;

    let backup = pg
        .backup(&cluster, BACKUP_TIMEOUT)
        .await
        .expect("base backup");
    let label = backup
        .label
        .expect("a finished backup has a pgbackrest label");
    assert_eq!(backup.cluster, name);
    assert_eq!(
        backup.backup_type.as_deref(),
        Some("full"),
        "the first backup of a cluster has nothing to be incremental against"
    );

    // The repository is the record, not pgpod's registry (ADR 04 §5).
    let listed = pg.backups(&cluster).await.expect("list backups");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].label, label);

    sql(&pg, &instance, "appdb", "INSERT INTO t VALUES (2, 'kept')").await;

    // The recovery target. Read from the *server*, not from the test
    // host: recovery_target_time is compared against commit timestamps,
    // and a host clock even slightly ahead would silently include the row
    // written after it.
    // Formatted by the server rather than converted here: `now()` prints a
    // `+00` offset that RFC 3339 does not accept, and microseconds matter —
    // rounding the target to the second could move it past a commit.
    let target = sql(
        &pg,
        &instance,
        "postgres",
        "SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",
    )
    .await;
    // A commit strictly after the target, so "stop at T" is a real claim
    // rather than "there was nothing left to replay anyway".
    sql(&pg, &instance, "postgres", "SELECT pg_sleep(1)").await;
    sql(&pg, &instance, "appdb", "INSERT INTO t VALUES (3, 'after')").await;

    // Recovery cannot replay past what has been archived, and with -X none
    // the base backup carries no WAL of its own. Force the segment holding
    // row 3 out to the archive rather than waiting five minutes for
    // archive_timeout.
    sql(&pg, &instance, "postgres", "SELECT pg_switch_wal()").await;
    wait_for_archiver(&pg, &instance).await;

    assert_eq!(
        sql(&pg, &instance, "appdb", "SELECT count(*) FROM t").await,
        "3",
        "the source must have all three rows"
    );

    // ---- restore to T -------------------------------------------------

    let at = chrono::DateTime::parse_from_rfc3339(target.trim())
        .unwrap_or_else(|e| panic!("parse the server's now() {target:?}: {e}"))
        .with_timezone(&chrono::Utc);

    let restore = pg
        .restore(&cluster, &restored, Some(at), READY_TIMEOUT)
        .await
        .expect("restore");
    assert_eq!(restore.backup_label, label);

    let restored_instance = format!("{restored_name}-1");

    // Rows 1 and 2 committed before T; row 3 after it.
    assert_eq!(
        sql(&pg, &restored_instance, "appdb", "SELECT count(*) FROM t").await,
        "2",
        "the restored cluster must hold exactly the rows as of the target"
    );
    assert_eq!(
        sql(
            &pg,
            &restored_instance,
            "appdb",
            "SELECT string_agg(tag, ',' ORDER BY id) FROM t"
        )
        .await,
        "before,kept",
        "and they must be the *right* rows, not merely the right number"
    );

    // The restore replayed WAL rather than just untarring the base backup:
    // row 2 was written after the backup finished.
    assert_eq!(
        sql(
            &pg,
            &restored_instance,
            "appdb",
            "SELECT tag FROM t WHERE id = 2"
        )
        .await,
        "kept",
        "row 2 exists only in the WAL, so its presence proves replay happened"
    );

    // The restored cluster is a real, writable cluster — it promoted at
    // the target rather than sitting paused in recovery.
    assert_eq!(
        sql(
            &pg,
            &restored_instance,
            "postgres",
            "SELECT pg_is_in_recovery()"
        )
        .await,
        "f",
        "recovery_target_action = promote means the restore ends usable"
    );

    // ---- the restored cluster's credentials are the source's ----------
    //
    // A restore brings `pg_authid` with it, and the restore path
    // deliberately does not rewrite roles — restoring a database "as it
    // was" includes its passwords. pgpod used to generate *fresh* secrets
    // for the restored cluster anyway, so the password it stored was one
    // no role had: the `postgresql://app@…` URI in `pgpod status` could
    // not be used, and nothing said why, because every test connected
    // over the unix socket with `peer` and never touched a password.
    //
    // Asserted against the database rather than by comparing two secrets,
    // because equal secrets that both fail to authenticate would pass
    // that test and leave the operator exactly where they started.
    let app_password = sql(
        &pg,
        &restored_instance,
        "postgres",
        &format!(
            "SELECT pg_read_file('{}')",
            pgpod_core::container::SECRET_APP_OWNER
        ),
    )
    .await;
    let source_password = sql(
        &pg,
        &instance,
        "postgres",
        &format!(
            "SELECT pg_read_file('{}')",
            pgpod_core::container::SECRET_APP_OWNER
        ),
    )
    .await;
    assert_eq!(
        app_password, source_password,
        "the restored cluster must adopt the source's credentials, not \
         generate its own — its roles came from the backup"
    );

    // And it actually authenticates, over TCP, which is the path `peer`
    // was hiding.
    let restored_id: pgpod_core::InstanceId = restored_instance.parse().expect("instance id");
    let out = pg
        .running_container(&restored_id)
        .await
        .expect("restored instance running")
        .exec(
            &ExecSpec::new([
                "psql",
                "-X",
                "-tA",
                "-h",
                "127.0.0.1",
                "-U",
                "app",
                "-d",
                "appdb",
                "-c",
                "SELECT current_user",
            ])
            .env("PGPASSWORD", app_password.trim()),
        )
        .await
        .expect("exec psql");
    assert!(
        out.success() && out.stdout.trim() == "app",
        "the stored app-owner secret does not authenticate against the \
         restored cluster: {}{}",
        out.stdout.trim(),
        out.stderr.trim()
    );
    sql(
        &pg,
        &restored_instance,
        "appdb",
        "INSERT INTO t VALUES (99, 'new')",
    )
    .await;

    // And the source is untouched by any of it.
    assert_eq!(
        sql(&pg, &instance, "appdb", "SELECT count(*) FROM t").await,
        "3",
        "restoring must not disturb the cluster being restored from"
    );

    let _ = pg.delete(&restored, true).await;
    let _ = pg.delete(&cluster, true).await;
    let _ = pg
        .podman_client()
        .remove_volume_destroying_data(&archive_volume)
        .await;
}

/// Wait until the archiver has caught up.
///
/// `pg_stat_archiver` is the server's own view, which is the only
/// authority on whether `archive_command` succeeded — the alternative is
/// looking for files in the archive, which would test pgpod's idea of the
/// layout against itself.
async fn wait_for_archiver(pg: &Pgpod, instance: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        // The queue draining is the thing that matters. A non-zero
        // `failed_count` is *not* asserted against: PostgreSQL retries,
        // and a transient failure that was retried successfully is not a
        // problem — only one that never drains is.
        let pending = sql(
            pg,
            instance,
            "postgres",
            "SELECT count(*) FROM pg_ls_dir('pg_wal/archive_status') f \
             WHERE f LIKE '%.ready'",
        )
        .await;
        if pending == "0" {
            return;
        }
        if std::time::Instant::now() >= deadline {
            let stats = sql(
                pg,
                instance,
                "postgres",
                "SELECT last_archived_wal || ' | failed=' || failed_count \
                 || ' | last_failed=' || coalesce(last_failed_wal, 'none') \
                 || ' | ' || coalesce(last_failed_time::text, '') \
                 FROM pg_stat_archiver",
            )
            .await;
            panic!(
                "the archiver never caught up: {pending} segments still .ready\n\
                 pg_stat_archiver: {stats}\n\
                 WAL is not reaching the repository, so no restore is possible. \
                 `pgpod logs {instance}` has the reason."
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ---- the same round trip, over the network, to Garage ----------------

/// A `posix` repository in a podman volume exercises no network at all —
/// no DNS, no TLS, no request signing, no multipart upload. Those are
/// exactly the parts most likely to be wrong, and they are the parts a
/// real deployment uses.
///
/// [Garage](https://garagehq.deuxfleurs.fr/) is the S3 implementation
/// ADR 03 §5 picks: a stricter subset than MinIO, so passing against it
/// means pgpod has not grown a dependency on AWS-specific behaviour.
///
/// **It is fronted by a TLS proxy**, because pgBackRest has no plaintext
/// option — which is a change to ADR 03 §5's plan, not a detail of the
/// fixture. Bring it up first, on the network this test's cluster will
/// use:
///
/// ```sh
/// eval "$(ops/dev-garage.sh start --network pgpod-gtest --network pgpod-gtestr)"
/// cargo test -p pgpod-control --features podman-tests --test pitr -- --ignored
/// ```
///
/// Two networks, because a restored cluster gets its own and still has to
/// reach the source's repository. That is a fixture artifact: a real S3 or
/// GCS endpoint resolves by ordinary DNS and does not care.
///
/// `#[ignore]` because it needs that fixture running. The cluster name is
/// fixed rather than derived from the pid so the network name is
/// predictable enough to hand to the script — which means this one cannot
/// run concurrently with itself.
#[tokio::test]
#[ignore = "needs ops/dev-garage.sh start --network pgpod-gtest"]
async fn a_cluster_can_be_restored_from_an_s3_repository() {
    let Ok(endpoint) = std::env::var("PGPOD_GARAGE_ENDPOINT") else {
        panic!(
            "PGPOD_GARAGE_ENDPOINT is not set.\nRun: eval \"$(ops/dev-garage.sh \
             start --network pgpod-gtest --network pgpod-gtestr)\""
        );
    };
    let bucket = std::env::var("PGPOD_GARAGE_BUCKET").expect("PGPOD_GARAGE_BUCKET");
    let key = std::env::var("PGPOD_GARAGE_KEY").expect("PGPOD_GARAGE_KEY");
    let secret = std::env::var("PGPOD_GARAGE_SECRET").expect("PGPOD_GARAGE_SECRET");
    let region = std::env::var("PGPOD_GARAGE_REGION").unwrap_or_else(|_| "garage".into());

    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());

    // Fixed, because the network name must be predictable enough to hand
    // to ops/dev-garage.sh. That makes leftovers from a previous run this
    // test's problem to clear.
    let name = "gtest".to_string();
    let restored_name = "gtestr".to_string();
    let cluster = ClusterId::new(name.clone()).unwrap();
    let restored = ClusterId::new(restored_name.clone()).unwrap();

    // `pg.delete` cannot help here: the registry is a fresh tempdir every
    // run, so pgpod has no record of what a previous run created. Remove
    // the podman objects by name instead.
    scrub(&pg, &[&name, &restored_name]).await;

    // And a fresh prefix in the bucket, so a previous run's stanza — with
    // a different system identifier — cannot collide with this one.
    let prefix = format!("{name}-{}", std::process::id());

    // pgBackRest reads credentials from the config file, which the agent
    // renders from this mounted secret — never from argv, which pgBackRest
    // refuses outright to keep secrets out of the process list.
    let secret_name = format!("pgpod-{name}-s3");
    let _ = pg.podman_client().remove_secret(&secret_name).await;
    pg.podman_client()
        .put_secret(
            &secret_name,
            &format!("s3-key={key}\ns3-key-secret={secret}\n"),
        )
        .await
        .expect("create the s3 credentials secret");

    let (host, port) = endpoint.rsplit_once(':').expect("endpoint is host:port");
    let manifest = ClusterManifest::from_yaml(&format!(
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
  backup:
    destinations:
      - url: s3://{bucket}/{prefix}
        endpoint: {host}:{port}
        region: {region}
        # The fixture's certificate is self-signed and there is nowhere to
        # publish a CA for it. TLS is still mandatory — pgBackRest has no
        # plaintext option — so this is as far as an insecure local setup
        # can go.
        verifyTls: false
        credentials: {secret_name}
"#
    ))
    .expect("manifest parses");

    pg.apply(&manifest, READY_TIMEOUT).await.expect("apply");
    let instance = format!("{name}-1");

    assert_eq!(
        sql(&pg, &instance, "postgres", "SHOW archive_mode").await,
        "on"
    );

    sql(
        &pg,
        &instance,
        "appdb",
        "CREATE TABLE t (id int primary key, tag text)",
    )
    .await;
    sql(
        &pg,
        &instance,
        "appdb",
        "INSERT INTO t VALUES (1, 'before')",
    )
    .await;

    // The backup itself is the point: this writes over TLS to Garage,
    // which a posix repository never exercises.
    let backup = pg
        .backup(&cluster, BACKUP_TIMEOUT)
        .await
        .expect("base backup to garage");
    let label = backup.label.expect("a finished backup has a label");

    sql(&pg, &instance, "appdb", "INSERT INTO t VALUES (2, 'kept')").await;

    let target = sql(
        &pg,
        &instance,
        "postgres",
        "SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",
    )
    .await;
    sql(&pg, &instance, "postgres", "SELECT pg_sleep(1)").await;
    sql(&pg, &instance, "appdb", "INSERT INTO t VALUES (3, 'after')").await;
    sql(&pg, &instance, "postgres", "SELECT pg_switch_wal()").await;
    wait_for_archiver(&pg, &instance).await;

    let at = chrono::DateTime::parse_from_rfc3339(target.trim())
        .unwrap_or_else(|e| panic!("parse {target:?}: {e}"))
        .with_timezone(&chrono::Utc);

    let restore = pg
        .restore(&cluster, &restored, Some(at), READY_TIMEOUT)
        .await
        .expect("restore from garage");
    assert_eq!(restore.backup_label, label);

    let restored_instance = format!("{restored_name}-1");
    assert_eq!(
        sql(&pg, &restored_instance, "appdb", "SELECT count(*) FROM t").await,
        "2",
        "the restored cluster must hold exactly the rows as of the target"
    );
    assert_eq!(
        sql(
            &pg,
            &restored_instance,
            "appdb",
            "SELECT string_agg(tag, ',' ORDER BY id) FROM t"
        )
        .await,
        "before,kept",
        "and the right rows — row 2 exists only in WAL replayed out of S3"
    );
    assert_eq!(
        sql(
            &pg,
            &restored_instance,
            "postgres",
            "SELECT pg_is_in_recovery()"
        )
        .await,
        "f",
        "the restore must end promoted and usable"
    );

    let _ = pg.delete(&restored, true).await;
    let _ = pg.delete(&cluster, true).await;
    let _ = pg.podman_client().remove_secret(&secret_name).await;
}

/// Remove every podman object a previous run of a fixed-name cluster could
/// have left behind.
///
/// Not `pg.delete`: that reads the registry, and each run gets a fresh one
/// in a tempdir, so pgpod genuinely does not know these exist. The names
/// are derivable, so removing them by name is both possible and the only
/// option.
async fn scrub(pg: &Pgpod, clusters: &[&str]) {
    let podman = pg.podman_client();
    for c in clusters {
        let id = ClusterId::new((*c).to_string()).expect("cluster id");
        let instance = id.instance(1);

        let container = podman.container(instance.container_name());
        if container.probe().await.ok().flatten().is_some() {
            let _ = container.stop(Duration::from_secs(20)).await;
            let _ = container.remove(true).await;
        }
        let _ = podman
            .remove_volume_destroying_data(&instance.volume_name())
            .await;
        for suffix in ["superuser", "replication", "monitor", "app-owner", "s3"] {
            let _ = podman.remove_secret(&format!("pgpod-{c}-{suffix}")).await;
        }
    }
}

/// Restoring where the source's secrets are **not** on this host.
///
/// The fresh-machine restore ADR 04 §5 exists to support: the repository
/// is self-describing, so the backup is all you brought. Its roles carry
/// passwords nobody here knows, and pgpod cannot adopt a credential that
/// does not exist — so it generates one and *rotates the role to match*,
/// because a restore that returns an unreachable cluster is not a restore.
///
/// Simulated by removing the source cluster's podman secrets, which is
/// exactly the condition `ensure_secrets` tests for.
///
/// **This test passed before the code was correct**, and it is worth
/// knowing why. Rotation needs a *writable* cluster, but `wait_ready`
/// polls `pg_isready`, which succeeds while a restore is still replaying
/// and has not yet promoted. Against this small archive the promotion won
/// the race every time; against a real one on the deployment target it
/// lost, with `cannot execute ALTER ROLE in a read-only transaction`. The
/// `wait_writable` call in `rotate_restored_roles` is what closes it —
/// this test cannot be relied on to catch its removal.
#[tokio::test]
async fn a_restore_without_the_source_secrets_rotates_the_roles() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pg = pgpod(dir.path());

    let suffix = std::process::id() % 100_000;
    let name = format!("f{suffix}");
    let restored_name = format!("f{suffix}r");
    let archive_volume = format!("pgpod-{name}-archive");
    let cluster = ClusterId::new(name.clone()).unwrap();
    let restored = ClusterId::new(restored_name.clone()).unwrap();

    let _ = pg.delete(&cluster, true).await;
    let _ = pg.delete(&restored, true).await;
    let _ = pg
        .podman_client()
        .remove_volume_destroying_data(&archive_volume)
        .await;

    pg.apply(&manifest(&name, &archive_volume), READY_TIMEOUT)
        .await
        .expect("apply");
    let instance = format!("{name}-1");
    sql(
        &pg,
        &instance,
        "appdb",
        "CREATE TABLE t (id int primary key)",
    )
    .await;
    sql(&pg, &instance, "appdb", "INSERT INTO t VALUES (1)").await;
    pg.backup(&cluster, BACKUP_TIMEOUT).await.expect("backup");

    // The source's credentials leave the host. The running container keeps
    // the files podman already materialised, so the source stays up — only
    // pgpod's ability to read them back is gone, which is the whole
    // condition under test.
    for secret in pgpod_control::SecretNames::for_cluster(&cluster).all() {
        pg.podman_client()
            .remove_secret(secret)
            .await
            .unwrap_or_else(|e| panic!("remove {secret}: {e}"));
    }

    pg.restore(&cluster, &restored, None, READY_TIMEOUT)
        .await
        .expect("restore without the source's secrets");
    let restored_instance = format!("{restored_name}-1");

    // The data arrived.
    assert_eq!(
        sql(&pg, &restored_instance, "appdb", "SELECT count(*) FROM t").await,
        "1"
    );

    // And the cluster is reachable with what pgpod stores — over TCP,
    // which is the path `peer` auth hides. Without the rotation the
    // generated password would belong to no role and this fails.
    let password = sql(
        &pg,
        &restored_instance,
        "postgres",
        &format!("SELECT pg_read_file('{}')", container::SECRET_APP_OWNER),
    )
    .await;
    let id: pgpod_core::InstanceId = restored_instance.parse().expect("instance id");
    let out = pg
        .running_container(&id)
        .await
        .expect("restored instance running")
        .exec(
            &ExecSpec::new([
                "psql",
                "-X",
                "-tA",
                "-h",
                "127.0.0.1",
                "-U",
                "app",
                "-d",
                "appdb",
                "-c",
                "SELECT current_user",
            ])
            .env("PGPASSWORD", password.trim()),
        )
        .await
        .expect("exec psql");
    assert!(
        out.success() && out.stdout.trim() == "app",
        "the restored cluster is not reachable with the credentials pgpod \
         generated for it: {}{}",
        out.stdout.trim(),
        out.stderr.trim()
    );

    // The superuser too — `peer` would mask a broken password here for
    // every local caller, including pgpod's own.
    let su = sql(
        &pg,
        &restored_instance,
        "postgres",
        &format!("SELECT pg_read_file('{}')", container::SECRET_SUPERUSER),
    )
    .await;
    let out = pg
        .running_container(&id)
        .await
        .expect("running")
        .exec(
            &ExecSpec::new([
                "psql",
                "-X",
                "-tA",
                "-h",
                "127.0.0.1",
                "-U",
                "postgres",
                "-d",
                "postgres",
                "-c",
                "SELECT 1",
            ])
            .env("PGPASSWORD", su.trim()),
        )
        .await
        .expect("exec psql");
    assert!(
        out.success(),
        "the superuser password was not rotated: {}",
        out.stderr.trim()
    );

    // And it said so, rather than rotating credentials silently.
    let events: Vec<String> = pg
        .registry()
        .recent_events(&restored, 20)
        .expect("events")
        .into_iter()
        .map(|e| format!("[{}] {}", e.level, e.message))
        .collect();
    assert!(
        events.iter().any(|e| e.contains("rotating")),
        "a restore that changes the database's passwords must say so: {events:?}"
    );

    let _ = pg.delete(&restored, true).await;
    let _ = pg.delete(&cluster, true).await;
    let _ = pg
        .podman_client()
        .remove_volume_destroying_data(&archive_volume)
        .await;
}
