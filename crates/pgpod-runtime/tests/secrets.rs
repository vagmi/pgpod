//! Podman secrets arrive in the container as the exact bytes pgpod sent.
//!
//! This is not a formality. `podman-api`'s `Secrets::create` posts
//! `serde_json::to_string(&secret)`, so libpod stores the JSON *encoding*
//! of the payload: wrapped in quotes, with `\n` as two literal characters.
//! pgpod stored every secret that way from Phase 1 until it was found, and
//! nothing caught it — because it is **self-consistent**. `initdb` set the
//! superuser password from the quoted file, and every later connection
//! read the same quoted file back, so the extra characters cancelled out
//! and the cluster worked.
//!
//! It surfaces the moment anything else has to parse a secret — pgpod's
//! object-store credentials are `KEY=value` lines, which a leading quote
//! and literal `\n` destroy — and it means a secret an operator creates by
//! hand with `podman secret create` does **not** match one pgpod creates.
//!
//! So the assertion is byte equality, made against the file as a container
//! actually sees it rather than against what podman reports, for the same
//! reason `hardening.rs` asserts against the kernel: the layer that lied
//! is the one in between.
//!
//! ```sh
//! eval "$(ops/dev-podman.sh start)"
//! cargo test -p pgpod-runtime --features podman-tests --test secrets -- --test-threads=1
//! ```

#![cfg(feature = "podman-tests")]

use std::sync::atomic::{AtomicU32, Ordering};

use pgpod_runtime::{ContainerSpec, PodmanClient, SecretMount};

/// Alpine, because it is small and this test does not care about
/// PostgreSQL — only about what podman put on disk.
const IMAGE: &str = "docker.io/library/postgres:18-alpine";

static SEQ: AtomicU32 = AtomicU32::new(0);

fn unique(prefix: &str) -> String {
    format!(
        "pgpod-it-{prefix}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Read a secret back the way the agent does: as a mounted file, and as
/// hex.
///
/// Hex rather than `cat`, because the bug is entirely in characters you
/// cannot see — a leading quote, and `\n` arriving as backslash-then-n.
/// Comparing hex to hex also avoids asserting on `od -c`'s column
/// padding, which is a property of `od` and not of pgpod.
async fn mounted_bytes(client: &PodmanClient, secret: &str) -> String {
    let name = unique("secret-reader");
    let spec = ContainerSpec::hardened(IMAGE)
        .name(&name)
        .user("70:70")
        .entrypoint(["od", "-An", "-tx1", "/run/secrets/probe"])
        .secret(SecretMount {
            name: secret.to_string(),
            target: "/run/secrets/probe".to_string(),
            mode: 0o400,
            uid: 70,
            gid: 70,
        })
        .label("pgpod.test", "true");

    let container = client.create_container(&spec).await.expect("create reader");
    container.start().await.expect("start reader");
    container.wait_for_exit().await.expect("wait for reader");
    let logs = container.logs_string().await.expect("reader logs");
    let _ = container.remove(true).await;
    // One flat lowercase hex string, whatever od chose for line breaks.
    logs.split_whitespace().collect::<String>().to_lowercase()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn a_secret_is_mounted_as_the_exact_bytes_it_was_created_with() {
    let client =
        PodmanClient::connect().expect("podman socket — run: eval \"$(ops/dev-podman.sh start)\"");
    client.pull_image_if_absent(IMAGE).await.expect("pull");

    let name = unique("secret");
    // Multi-line, because that is the shape pgpod's object-store
    // credentials take and the shape JSON encoding mangles worst.
    let payload = "s3-key=GKabc123\ns3-key-secret=shh\n";
    client.put_secret(&name, payload).await.expect("put secret");

    let seen = mounted_bytes(&client, &name).await;
    let _ = client.remove_secret(&name).await;

    assert_eq!(
        seen,
        hex(payload.as_bytes()),
        "the mounted secret is not the bytes pgpod sent.\n           sent:   {}\n  got:    {seen}\n         A leading 22 (a quote) and 5c6e (backslash-n) instead of 0a mean \
         the payload was JSON-encoded on the way to libpod.",
        hex(payload.as_bytes())
    );
}

#[tokio::test]
async fn a_password_with_shell_and_json_metacharacters_survives() {
    // Generated passwords are alphanumeric today, but nothing forces an
    // operator-supplied one to be, and a secret that mangles some values
    // and not others is worse than one that mangles all of them.
    let client =
        PodmanClient::connect().expect("podman socket — run: eval \"$(ops/dev-podman.sh start)\"");
    client.pull_image_if_absent(IMAGE).await.expect("pull");

    let name = unique("secret-meta");
    // Every character JSON escaping, shell quoting or podman'"'"'s own
    // handling might mangle, plus a newline in the middle.
    let payload = "a\"b\\c$d'e`f\ng";
    client.put_secret(&name, payload).await.expect("put secret");

    let seen = mounted_bytes(&client, &name).await;
    let _ = client.remove_secret(&name).await;

    assert_eq!(
        seen,
        hex(payload.as_bytes()),
        "a password containing quotes, backslashes or a newline was \
         altered in transit.\n  sent: {}\n  got:  {seen}",
        hex(payload.as_bytes())
    );
}
