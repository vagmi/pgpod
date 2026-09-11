//! Podman secrets.
//!
//! Passwords and object-store credentials reach containers as mounted
//! files under `/run/secrets/`, never as environment variables — env vars
//! show up in `podman inspect`, in `/proc/<pid>/environ`, and in error
//! paths that log a whole spec (ADR 00 §9).

use crate::client::PodmanClient;
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretInfo {
    pub id: String,
    pub name: String,
}

impl PodmanClient {
    /// Create or replace a secret.
    ///
    /// Podman secrets are immutable, so a rotation means delete-then-create.
    /// The value is taken as `&str` and never logged; callers hold it in a
    /// `pgpod_core::Secret` right up to this call.
    ///
    /// **Posted as raw bytes through `crate::http`, not through
    /// `podman-api`.** Its `Secrets::create` sends
    /// `serde_json::to_string(&secret)`, so libpod stores the JSON
    /// *encoding* — a payload wrapped in quotes with `\n` as two literal
    /// characters — where the endpoint wants the bytes themselves. pgpod
    /// stored every secret that way from Phase 1 until this was found, and
    /// nothing noticed, because it is self-consistent: `initdb` set the
    /// superuser password from the quoted file and every later connection
    /// read the same quoted file. It breaks the moment anything else has
    /// to parse a secret, and it means a secret created by hand with
    /// `podman secret create` does not match one pgpod created.
    pub async fn put_secret(&self, name: &str, value: &str) -> Result<SecretInfo> {
        // Ignore a missing-secret error on the delete: this is the normal
        // first-create path.
        let _ = self.remove_secret(name).await;

        let (status, response) = crate::http::post_bytes(
            self.socket_path(),
            &format!("/v4.0.0/libpod/secrets/create?name={name}"),
            value.as_bytes(),
        )
        .await?;

        if !(200..300).contains(&status) {
            // Deliberately does not echo the response body: podman repeats
            // request context on some failures, and the request body here
            // is the secret.
            return Err(Error::Secret(format!(
                "failed to create secret {name}: HTTP {status}"
            )));
        }

        let created: serde_json::Value = serde_json::from_str(&response)
            .map_err(|_| Error::Secret(format!("create secret {name}: unreadable response")))?;

        Ok(SecretInfo {
            id: created
                .get("ID")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            name: name.to_string(),
        })
    }

    /// Read a secret's value back, or `None` if there is no such secret.
    ///
    /// **This is not a privilege boundary being crossed.** pgpod's secrets
    /// live in `~/.local/share/containers/storage/secrets/`, base64 in a
    /// file owned by the user the daemon already runs as — it could always
    /// read them, and going through libpod is simply the supported way to
    /// ask. What the absence of this function bought until now was a
    /// discipline: code that cannot read a credential cannot accidentally
    /// log, copy, or compare one.
    ///
    /// It exists for one caller. A restored cluster's `pg_authid` comes
    /// out of the backup carrying the **source's** roles, so generating
    /// fresh passwords for it produces secrets no role has (ADR 04 §8).
    /// Adopting the source's credentials means reading them.
    ///
    /// `podman-api` cannot do this: its `Secret::inspect` omits
    /// `?showsecret=true`, so the response has no `SecretData`.
    pub async fn secret_value(&self, name: &str) -> Result<Option<String>> {
        let (status, body) = crate::http::get(
            self.socket_path(),
            &format!("/v4.0.0/libpod/secrets/{name}/json?showsecret=true"),
        )
        .await?;

        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            // Deliberately does not echo the body: on some paths podman
            // repeats request context, and this response contains a
            // secret.
            return Err(Error::Secret(format!(
                "failed to read secret {name}: HTTP {status}"
            )));
        }

        let parsed: serde_json::Value = serde_json::from_str(&body)
            .map_err(|_| Error::Secret(format!("read secret {name}: unreadable response")))?;
        Ok(parsed
            .get("SecretData")
            .and_then(|v| v.as_str())
            .map(str::to_string))
    }

    pub async fn secret_exists(&self, name: &str) -> Result<bool> {
        let secrets = self
            .podman()
            .secrets()
            .list()
            .await
            .map_err(|e| Error::Secret(format!("list: {e}")))?;
        Ok(secrets
            .into_iter()
            .any(|s| s.spec.and_then(|sp| sp.name).as_deref() == Some(name)))
    }

    pub async fn remove_secret(&self, name: &str) -> Result<()> {
        self.podman()
            .secrets()
            .get(name)
            .delete()
            .await
            .map_err(|e| Error::Secret(format!("delete {name}: {e}")))
    }
}
