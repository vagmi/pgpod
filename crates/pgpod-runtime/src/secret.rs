//! Podman secrets.
//!
//! Passwords and object-store credentials reach containers as mounted
//! files under `/run/secrets/`, never as environment variables — env vars
//! show up in `podman inspect`, in `/proc/<pid>/environ`, and in error
//! paths that log a whole spec (ADR 00 §9).

use podman_api::opts::SecretCreateOpts;

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
    pub async fn put_secret(&self, name: &str, value: &str) -> Result<SecretInfo> {
        // Ignore a missing-secret error on the delete: this is the normal
        // first-create path.
        let _ = self.remove_secret(name).await;

        let opts = SecretCreateOpts::builder(name).build();
        let created = self
            .podman()
            .secrets()
            .create(&opts, value.to_string())
            .await
            // Deliberately does not include the underlying error's body:
            // podman echoes request context on some failures, and the
            // request body here is the secret.
            .map_err(|_| Error::Secret(format!("failed to create secret {name}")))?;

        Ok(SecretInfo {
            id: created.id().to_string(),
            name: name.to_string(),
        })
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
