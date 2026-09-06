//! Do the container hardening flags actually reach podman?
//!
//! Every flag in `AGENTS.md`'s hardening table is set through a builder
//! that could silently stop applying — `podman-api` 0.10 even misspells
//! one of them (`no_new_privilages`). A hardening flag that does not land
//! is worse than one that was never set, because it looks set.
//!
//! So this asserts the *observed* container config via inspect, not the
//! spec we sent (ROADMAP Phase 1).
//!
//! ```sh
//! eval "$(ops/dev-podman.sh start)"
//! cargo test -p pgpod-runtime --features podman-tests --test hardening
//! ```

#![cfg(feature = "podman-tests")]

use std::sync::atomic::{AtomicU32, Ordering};

use pgpod_runtime::{ContainerSpec, PodmanClient};

const IMAGE: &str = "docker.io/library/alpine:3";
static SEQ: AtomicU32 = AtomicU32::new(0);

async fn connect() -> PodmanClient {
    let client =
        PodmanClient::connect().expect("podman socket — run: eval \"$(ops/dev-podman.sh start)\"");
    client.ping().await.expect("podman must answer /_ping");
    client
}

fn unique(prefix: &str) -> String {
    format!(
        "pgpod-harden-{prefix}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// The whole hardening table, asserted against what podman reports.
#[tokio::test]
async fn hardening_flags_land_in_the_created_container() {
    let client = connect().await;
    client.pull_image_if_absent(IMAGE).await.expect("pull");

    let name = unique("flags");
    let spec = ContainerSpec::hardened(IMAGE)
        .name(&name)
        .command(["sleep", "30"]);

    let container = client.create_container(&spec).await.expect("create");

    // Inspect the raw config: the point is to check what podman *did*,
    // not what pgpod's own types say it asked for.
    let raw = client.inspect_raw(container.id()).await.expect("inspect");

    let host_config = raw
        .get("HostConfig")
        .expect("inspect payload has HostConfig");

    // read_only_filesystem
    assert_eq!(
        host_config.get("ReadonlyRootfs").and_then(|v| v.as_bool()),
        Some(true),
        "read-only rootfs did not land:\n{host_config:#}"
    );

    // cap_drop: ALL — podman reports the effective set, so assert the
    // dangerous ones are gone rather than matching the literal ["ALL"].
    let effective = raw
        .get("EffectiveCaps")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_ascii_uppercase)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for dangerous in ["CAP_SYS_ADMIN", "CAP_NET_RAW", "CAP_CHOWN", "CAP_SETUID"] {
        assert!(
            !effective.contains(&dangerous.to_string()),
            "{dangerous} survived cap_drop=ALL: {effective:?}"
        );
    }

    // restart_policy: no — podman's own restarts would race the
    // reconciler and revive a deliberately fenced instance.
    let policy = host_config
        .get("RestartPolicy")
        .and_then(|p| p.get("Name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        policy.is_empty() || policy == "no",
        "restart policy should be 'no', got {policy:?}"
    );

    container.remove(true).await.expect("cleanup");
}

/// `no_new_privileges` must be observably in effect *inside* the
/// container, not merely present in the create payload.
///
/// This is the check that caught the real bug: `podman-api`'s builder
/// spells the field `no_new_privilages`, libpod ignores the unknown key,
/// and the flag silently never applied. Asserting the kernel's own view
/// via `/proc/self/status` cannot be fooled by a payload that looks right.
#[tokio::test]
async fn no_new_privileges_is_in_effect_inside_the_container() {
    let client = connect().await;
    client.pull_image_if_absent(IMAGE).await.expect("pull");

    let name = unique("nnp");
    let spec = ContainerSpec::hardened(IMAGE).name(&name).command([
        "sh",
        "-c",
        "grep NoNewPrivs /proc/self/status",
    ]);
    let container = client.create_container(&spec).await.expect("create");
    container.start().await.expect("start");
    let code = container.wait_for_exit().await.expect("wait");
    let logs = container.logs_string().await.expect("logs");
    container.remove(true).await.expect("cleanup");

    assert_eq!(code, 0, "probe failed:\n{logs}");
    assert!(
        logs.contains("NoNewPrivs:\t1") || logs.contains("NoNewPrivs:  1"),
        "no_new_privileges is NOT in effect in the container.\n\
         podman-api's builder misspells this field; container creation \
         patches the payload to compensate (see src/http.rs). If this \
         fails, that patch has stopped working.\n\
         got: {logs:?}"
    );
}

/// The negative control: without hardening, `NoNewPrivs` is 0.
///
/// Without this, the test above would still pass if podman started
/// defaulting the flag on — and pgpod would look protected by its own
/// code when it was protected by a default it does not control.
#[tokio::test]
async fn an_unhardened_container_does_not_get_no_new_privileges_for_free() {
    let client = connect().await;
    client.pull_image_if_absent(IMAGE).await.expect("pull");

    let name = unique("nnp-control");
    let spec = ContainerSpec::new(IMAGE).name(&name).command([
        "sh",
        "-c",
        "grep NoNewPrivs /proc/self/status",
    ]);
    let container = client.create_container(&spec).await.expect("create");
    container.start().await.expect("start");
    container.wait_for_exit().await.expect("wait");
    let logs = container.logs_string().await.expect("logs");
    container.remove(true).await.expect("cleanup");

    assert!(
        logs.contains("NoNewPrivs:\t0") || logs.contains("NoNewPrivs:  0"),
        "podman now sets no_new_privileges by default, so the positive \
         test above no longer proves pgpod is doing anything: {logs:?}"
    );
}

/// The read-only rootfs must not make the container useless: the tmpfs
/// mounts `hardened()` supplies have to be writable, and the rootfs must
/// not be.
///
/// Note what is *not* checked here: the postgres socket directory. It
/// lives inside the instance volume (`container::SOCKET_DIR`) rather than
/// being a tmpfs over the image's own `/var/run/postgresql`, because
/// podman's `tmpcopyup` does not preserve that directory's ownership and
/// the mount comes up root-owned. This container has no volume, so the
/// socket directory legitimately does not exist.
#[tokio::test]
async fn a_hardened_container_can_still_write_its_tmpfs_mounts() {
    let client = connect().await;
    client.pull_image_if_absent(IMAGE).await.expect("pull");

    let name = unique("write");
    let script = "set -e; \
         touch /tmp/probe && echo TMP_OK; \
         touch /run/secrets/probe && echo SECRETS_OK; \
         if touch /rootfs-probe 2>/dev/null; then echo ROOTFS_WRITABLE; fi; \
         echo DONE";
    let spec = ContainerSpec::hardened(IMAGE)
        .name(&name)
        .command(["sh", "-c", script]);

    let container = client.create_container(&spec).await.expect("create");
    container.start().await.expect("start");
    let code = container.wait_for_exit().await.expect("wait");
    let logs = container.logs_string().await.expect("logs");

    assert_eq!(
        code, 0,
        "hardened container could not write its tmpfs:\n{logs}"
    );
    assert!(logs.contains("TMP_OK"), "/tmp not writable:\n{logs}");
    assert!(
        logs.contains("SECRETS_OK"),
        "/run/secrets not writable — podman could not have created secret \
         mountpoints here:\n{logs}"
    );
    assert!(
        !logs.contains("ROOTFS_WRITABLE"),
        "container root filesystem is writable — read_only_fs did not land:\n{logs}"
    );

    container.remove(true).await.expect("cleanup");
}

/// The socket directory must be inside the volume, not the image.
///
/// Asserted as a test rather than left to the constant's doc comment
/// because a well-meaning change back to `/var/run/postgresql` would work
/// on the stock `postgres` image and fail only on CNPG-style ones, which
/// is the worst possible place to discover it.
#[test]
fn the_socket_directory_lives_inside_the_volume() {
    assert!(
        pgpod_core::container::SOCKET_DIR.starts_with(pgpod_core::container::VOLUME_MOUNT),
        "SOCKET_DIR must be inside the volume so it is writable regardless \
         of what the image ships: {}",
        pgpod_core::container::SOCKET_DIR
    );
}
