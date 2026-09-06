//! Reading the passwords podman mounted into the container.
//!
//! Files under `/run/secrets/`, never environment variables — env vars are
//! visible in `podman inspect` and `/proc/<pid>/environ` (ADR 00 §9).

use std::path::Path;

use anyhow::{Context, Result, bail};
use pgpod_core::{Secret, container};

#[derive(Debug, Clone)]
pub struct InstanceSecrets {
    pub superuser: Secret,
    pub replication: Secret,
    pub monitor: Secret,
    /// `None` when the instance has no application database.
    pub app_owner: Option<Secret>,
}

impl InstanceSecrets {
    pub fn from_mounts() -> Result<Self> {
        Ok(Self {
            superuser: read_required(container::SECRET_SUPERUSER)?,
            replication: read_required(container::SECRET_REPLICATION)?,
            monitor: read_required(container::SECRET_MONITOR)?,
            app_owner: read_optional(container::SECRET_APP_OWNER)?,
        })
    }
}

fn read_required(path: &str) -> Result<Secret> {
    let s = read_optional(path)?;
    s.ok_or_else(|| {
        anyhow::anyhow!(
            "required secret {path} is not mounted — the daemon did not attach it \
             to this container"
        )
    })
}

fn read_optional(path: &str) -> Result<Option<Secret>> {
    let p = Path::new(path);
    if !p.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(p).with_context(|| format!("failed to read {path}"))?;
    // Trailing newlines are easy to introduce when creating a secret from
    // a shell, and a password with a stray \n authenticates against
    // nothing. Trim, then reject empty.
    let value = raw.trim_end_matches(['\n', '\r']).to_string();
    if value.is_empty() {
        bail!("secret {path} is empty");
    }
    Ok(Some(Secret::new(value)))
}
