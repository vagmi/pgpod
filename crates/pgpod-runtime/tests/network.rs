//! Removing a cluster network must never take a container with it.
//!
//! `pgpod delete <cluster>` removes the cluster's network on a
//! best-effort basis, on the stated reasoning that a network holds no data
//! and that podman refuses to remove one while containers are attached.
//! The second half of that was wrong for two phases: `podman-api` spells
//! the **force** variant `Network::remove` and the ordinary one
//! `Network::delete` — its own doc comment reads "Force remove this
//! network removing associated containers. To delete network normally use
//! `Network::delete`" — and pgpod called `remove`.
//!
//! Nothing caught it because, until a pooler existed, the only containers
//! on a cluster network were the instances `delete` had already removed by
//! the time it got there. Attach anything else and `pgpod delete` destroys
//! it: found by running it, with a pg_doorman container that was gone
//! before the next command.
//!
//! This is the third `podman-api` naming trap after `no_new_privilages`
//! and the secret encoding, and it is the same lesson each time — assert
//! the behaviour, never the method name. A test that called
//! `remove_network` and expected `Err` would still pass if the call went
//! back to forcing; what makes this a regression test is that it asserts
//! the *container is still running afterwards*.
//!
//! ```sh
//! eval "$(ops/dev-podman.sh start)"
//! cargo test -p pgpod-runtime --features podman-tests --test network -- --test-threads=1
//! ```

#![cfg(feature = "podman-tests")]

use std::sync::atomic::{AtomicU32, Ordering};

use pgpod_runtime::{ContainerSpec, PodmanClient};

const IMAGE: &str = "docker.io/library/postgres:18-alpine";

static SEQ: AtomicU32 = AtomicU32::new(0);

fn unique(prefix: &str) -> String {
    format!(
        "pgpod-it-{prefix}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

#[tokio::test]
async fn removing_a_network_in_use_fails_and_leaves_the_container_running() {
    let client =
        PodmanClient::connect().expect("podman socket — run: eval \"$(ops/dev-podman.sh start)\"");
    client.pull_image_if_absent(IMAGE).await.expect("pull");

    let net = unique("net");
    let name = unique("tenant");
    client
        .ensure_network(&net, &[("pgpod.test".to_string(), "true".to_string())])
        .await
        .expect("create network");

    // Stands in for a pooler: something on the cluster network that is not
    // one of the instances `delete` removes on its way past.
    let spec = ContainerSpec::hardened(IMAGE)
        .name(&name)
        .user("70:70")
        .entrypoint(["sleep", "300"])
        .network(&net)
        .label("pgpod.test", "true");
    let container = client.create_container(&spec).await.expect("create tenant");
    container.start().await.expect("start tenant");

    let err = client.remove_network(&net).await.unwrap_err();

    let probe = client
        .container(&name)
        .probe()
        .await
        .expect("probe tenant")
        .expect("the tenant container must still exist");
    assert!(
        probe.running,
        "removing a network with a container attached took the container \
         with it — `remove_network` is forcing. podman-api spells the force \
         variant `remove` and the plain one `delete`. (network removal said: \
         {err})"
    );

    let _ = container.stop(std::time::Duration::from_secs(5)).await;
    let _ = container.remove(true).await;
    client
        .remove_network(&net)
        .await
        .expect("with nothing attached, the same call must succeed");
    assert!(
        client
            .network(&net)
            .await
            .expect("look up network")
            .is_none(),
        "the network survived a removal that reported success — the call is \
         not removing anything and `pgpod delete` would leak a network per \
         cluster"
    );
}
