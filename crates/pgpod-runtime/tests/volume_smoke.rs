//! Does podman give a container ownership of an empty named volume?
//!
//! pgpod's entire storage model rests on one podman behaviour: mounting an
//! empty named volume into a container leaves that container's user able to
//! own and write the volume's contents. If it does not hold, PostgreSQL
//! cannot `initdb` into its own PGDATA and `adrs/00-project-setup.md` §4 is
//! wrong — so this is verified on the host under test rather than assumed
//! (ROADMAP Phase 0).
//!
//! Requires a live rootless podman socket:
//!
//! ```sh
//! cargo test -p pgpod-runtime --features podman-tests --test volume_smoke -- --nocapture --test-threads=1
//! ```

#![cfg(feature = "podman-tests")]

use std::sync::atomic::{AtomicU32, Ordering};

use pgpod_runtime::{ContainerSpec, Mount, PodmanClient};

/// Container names must be unique across concurrently running tests.
/// Deriving them from the PID alone collided as soon as two tests in this
/// file used the same `chown` value — cargo runs tests in parallel by
/// default, so the second `create` failed on a name already in use.
static PROBE_SEQ: AtomicU32 = AtomicU32::new(0);

/// Small, always-available, and has `stat` and `id`. Nothing about this
/// test is postgres-specific — it is probing podman, not an image.
const IMAGE: &str = "docker.io/library/alpine:3";

/// The UID stock `docker.io/library/postgres` runs as. CNPG-style images
/// use 26; the number does not matter to the test, only that it is neither
/// root nor the invoking user.
const PG_UID: &str = "999";

/// Create a uniquely-named volume for one test.
///
/// Cleanup is explicit at the end of each test rather than in a `Drop`
/// impl: `Drop` cannot await, and a volume left behind by a panicking test
/// is worth inspecting anyway. The create path removes any leftover of the
/// same name first, so a re-run after a failure starts clean.
async fn make_volume(client: &PodmanClient, tag: &str) -> String {
    let name = format!("pgpod-smoke-{tag}-{}", std::process::id());
    let _ = client.remove_volume_destroying_data(&name).await;
    client
        .create_volume(&name, &[("pgpod.smoke".into(), "true".into())])
        .await
        .expect("create volume");
    name
}

async fn connect() -> PodmanClient {
    let client = PodmanClient::connect()
        .expect("podman socket — run `systemctl --user enable --now podman.socket`");
    client.ping().await.expect("podman must answer /_ping");
    client
}

/// Run `script` in a container with `volume` mounted at `/pgdata` as
/// `PG_UID`, and return `(exit_code, output)`.
async fn run_probe(
    client: &PodmanClient,
    volume: &str,
    chown: bool,
    script: &str,
) -> (i32, String) {
    client
        .pull_image_if_absent(IMAGE)
        .await
        .expect("pull alpine");

    let spec = ContainerSpec::new(IMAGE)
        .name(format!(
            "pgpod-smoke-{}-{}",
            std::process::id(),
            PROBE_SEQ.fetch_add(1, Ordering::Relaxed)
        ))
        .user(format!("{PG_UID}:{PG_UID}"))
        .command(["sh", "-c", script])
        .mount(Mount::Volume {
            name: volume.to_string(),
            target: "/pgdata".to_string(),
            chown,
        })
        .label("pgpod.smoke", "true");

    let container = client
        .create_container(&spec)
        .await
        .expect("create container");
    container.start().await.expect("start container");
    let code = container.wait_for_exit().await.expect("wait for exit");
    let output = container.logs_string().await.expect("read logs");
    container.remove(true).await.expect("remove container");
    (code, output)
}

/// The load-bearing assertion: a container running as a non-root, non-host
/// UID can create and write PGDATA inside a freshly created named volume.
///
/// This is exactly what `initdb` does on first boot, reduced to its
/// essentials. It asserts *capability*, not a specific ownership triple,
/// because podman's chown rules differ between "image has content at the
/// mount point" and "it does not" — and pgpod only needs the capability.
#[tokio::test]
async fn container_user_can_own_and_write_a_fresh_volume() {
    let client = connect().await;
    let vol = make_volume(&client, "own").await;

    let script = r#"
set -e
echo "mount-owner: $(stat -c %u:%g /pgdata)"
echo "running-as:  $(id -u):$(id -g)"
mkdir -p /pgdata/pgdata
chmod 0700 /pgdata/pgdata
echo probe > /pgdata/pgdata/PG_VERSION
echo "pgdata-owner: $(stat -c %u:%g /pgdata/pgdata)"
echo "pgdata-mode:  $(stat -c %a /pgdata/pgdata)"
cat /pgdata/pgdata/PG_VERSION
"#;

    let (code, output) = run_probe(&client, &vol, false, script).await;
    eprintln!("--- probe output ---\n{output}\n--------------------");

    assert_eq!(
        code, 0,
        "a container running as uid {PG_UID} could not set up PGDATA in an \
         empty named volume.\n\
         This invalidates adrs/00-project-setup.md §4. The remedy is the \
         `U` mount option — see `chown` on Mount::Volume, and the \
         `explicit_chown_also_works` test below.\n\
         output:\n{output}"
    );
    assert!(
        output.contains("probe"),
        "file written into PGDATA was not readable back:\n{output}"
    );
    assert!(
        output.contains(&format!("pgdata-owner: {PG_UID}:{PG_UID}")),
        "PGDATA is not owned by the container user, so postgres would \
         refuse to start on it:\n{output}"
    );
    assert!(
        output.contains("pgdata-mode:  700"),
        "PGDATA must be mode 0700 or postgres refuses to start \
         (adrs/00-project-setup.md §5):\n{output}"
    );

    client
        .remove_volume_destroying_data(&vol)
        .await
        .expect("cleanup");
}

/// The `U` mount option is pgpod's fallback if the implicit behaviour above
/// ever stops holding. Proving it works keeps the remedy from being
/// theoretical.
#[tokio::test]
async fn explicit_chown_also_works() {
    let client = connect().await;
    let vol = make_volume(&client, "chown").await;

    let script = r#"
set -e
mkdir -p /pgdata/pgdata
echo probe > /pgdata/pgdata/PG_VERSION
echo "pgdata-owner: $(stat -c %u:%g /pgdata/pgdata)"
"#;

    let (code, output) = run_probe(&client, &vol, true, script).await;
    eprintln!("--- U-option output ---\n{output}\n-----------------------");

    assert_eq!(code, 0, "U-option mount failed:\n{output}");
    assert!(
        output.contains(&format!("pgdata-owner: {PG_UID}:{PG_UID}")),
        "U option did not chown the volume to the container user:\n{output}"
    );

    client
        .remove_volume_destroying_data(&vol)
        .await
        .expect("cleanup");
}

/// Data written by one container must still be there for the next one.
/// This is what makes `pgpod delete` (without `--purge`) followed by
/// `pgpod apply` return the same database rather than an empty one.
#[tokio::test]
async fn volume_contents_survive_the_container() {
    let client = connect().await;
    let vol = make_volume(&client, "persist").await;

    let (code, _) = run_probe(
        &client,
        &vol,
        false,
        "set -e; mkdir -p /pgdata/pgdata; echo 17 > /pgdata/pgdata/PG_VERSION",
    )
    .await;
    assert_eq!(code, 0, "first container failed to write");

    let (code, output) = run_probe(
        &client,
        &vol,
        false,
        "set -e; cat /pgdata/pgdata/PG_VERSION",
    )
    .await;
    assert_eq!(code, 0, "second container failed to read:\n{output}");
    assert!(
        output.contains("17"),
        "volume did not survive container removal — data would be lost on \
         every reconcile:\n{output}"
    );

    client
        .remove_volume_destroying_data(&vol)
        .await
        .expect("cleanup");
}

/// `podman info` must report everything `pgpod doctor` renders. This is the
/// ADR 03 §3 version-span check in miniature: it runs against whatever
/// podman the host has, so a field that vanishes in a future release fails
/// here rather than silently turning a doctor check into "unknown".
#[tokio::test]
async fn podman_info_reports_the_fields_doctor_depends_on() {
    let client = connect().await;
    let info = client.info().await.expect("info");
    eprintln!("--- podman info ---\n{info:#?}\n-------------------");

    assert!(info.server_version.is_some(), "no server version");
    assert!(info.server_version_parts().is_some(), "version unparseable");
    assert_eq!(info.rootless, Some(true), "pgpod requires rootless podman");
    assert!(
        info.network_backend.is_some(),
        "no network backend reported"
    );
    assert!(info.cgroup_version.is_some(), "no cgroup version reported");
    assert!(info.graph_root.is_some(), "no graph root reported");
    assert!(
        info.subuid_count > 0,
        "no subuid ranges — postgres (uid 999) could not start"
    );
}
