//! Instance lifecycle phase and role.
//!
//! The phase is what the registry persists and what drives reconciliation
//! on daemon restart: rows in a live phase (see [`InstancePhase::is_live`])
//! are candidates for adoption against podman.
//!
//! Phase and role are deliberately separate. Phase is pgpod's *intent* and
//! bookkeeping; role is what the database reports about itself. Collapsing
//! them into one enum would make it impossible to represent the condition
//! ADR 02 §7 insists on surfacing — a container pgpod believes is a standby
//! that reports otherwise.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstancePhase {
    /// Row inserted; volume and container creation are in flight.
    Creating,
    /// Container is up and the agent is bootstrapping — `initdb`,
    /// `pg_basebackup`, or a restore from object storage. May be a long
    /// wait for a large recovery, which is why it is distinct from
    /// `Creating`.
    Bootstrapping,
    /// Container is running and probes are healthy.
    Running,
    /// Stop requested; cleanup in flight.
    Stopping,
    /// User-intent stop. The container is gone but **the volume is
    /// retained** — the reconciler leaves these alone across restarts.
    Stopped,
    /// Deliberately stopped and barred from restart during a failover
    /// (ADR 02 §4 step 2). The reconciler must never bring these back on
    /// its own; only an explicit rejoin or rebuild clears the phase.
    Fenced,
    /// `pg_rewind` failed and the instance cannot rejoin its primary. Data
    /// is intact and the volume is kept; `pgpod rebuild` is the way out.
    /// Distinct from `Failed` because the remedy is known and specific.
    NeedsRebuild,
    /// An operation failed. The row is kept for diagnostics.
    Failed,
    /// Terminal. The container is gone; the volume may or may not be,
    /// depending on whether `--purge` was given.
    Terminated,
}

impl InstancePhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Bootstrapping => "bootstrapping",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Fenced => "fenced",
            Self::NeedsRebuild => "needs-rebuild",
            Self::Failed => "failed",
            Self::Terminated => "terminated",
        }
    }

    /// pgpod thought this instance was live when it last wrote the row.
    /// The reconciler must decide adopt-vs-recreate for these on start.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            Self::Creating | Self::Bootstrapping | Self::Running | Self::Stopping
        )
    }

    /// The reconciler must not start a container for an instance in this
    /// phase, even though the desired spec still calls for one. Separating
    /// this from `is_live` is what keeps a fenced old primary down during
    /// a failover instead of being helpfully restarted into a split brain.
    pub fn blocks_restart(&self) -> bool {
        matches!(
            self,
            Self::Fenced | Self::NeedsRebuild | Self::Stopped | Self::Terminated
        )
    }
}

impl std::fmt::Display for InstancePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid instance phase: {0}")]
pub struct ParsePhaseError(pub String);

impl FromStr for InstancePhase {
    type Err = ParsePhaseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "creating" => Self::Creating,
            "bootstrapping" => Self::Bootstrapping,
            "running" => Self::Running,
            "stopping" => Self::Stopping,
            "stopped" => Self::Stopped,
            "fenced" => Self::Fenced,
            "needs-rebuild" => Self::NeedsRebuild,
            "failed" => Self::Failed,
            "terminated" => Self::Terminated,
            other => return Err(ParsePhaseError(other.to_string())),
        })
    }
}

/// What the database reports about itself, read from
/// `pg_is_in_recovery()` — never inferred from the registry (ADR 02 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceRole {
    Primary,
    Standby,
    /// Postgres is not reachable, so its role is genuinely unknown. This
    /// is not the same as "assume it is what we last saw" — an instance
    /// whose role cannot be read must not be treated as either.
    Unknown,
}

impl InstanceRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Standby => "standby",
            Self::Unknown => "unknown",
        }
    }

    /// Map `pg_is_in_recovery()` to a role.
    pub fn from_in_recovery(in_recovery: bool) -> Self {
        if in_recovery {
            Self::Standby
        } else {
            Self::Primary
        }
    }
}

impl std::fmt::Display for InstanceRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid instance role: {0}")]
pub struct ParseRoleError(pub String);

impl FromStr for InstanceRole {
    type Err = ParseRoleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "primary" => Self::Primary,
            "standby" => Self::Standby,
            "unknown" => Self::Unknown,
            other => return Err(ParseRoleError(other.to_string())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_PHASES: [InstancePhase; 9] = [
        InstancePhase::Creating,
        InstancePhase::Bootstrapping,
        InstancePhase::Running,
        InstancePhase::Stopping,
        InstancePhase::Stopped,
        InstancePhase::Fenced,
        InstancePhase::NeedsRebuild,
        InstancePhase::Failed,
        InstancePhase::Terminated,
    ];

    #[test]
    fn phases_round_trip_via_str() {
        for p in ALL_PHASES {
            assert_eq!(InstancePhase::from_str(p.as_str()).unwrap(), p);
        }
    }

    #[test]
    fn live_phases_are_the_ones_the_reconciler_probes() {
        for p in [
            InstancePhase::Creating,
            InstancePhase::Bootstrapping,
            InstancePhase::Running,
            InstancePhase::Stopping,
        ] {
            assert!(p.is_live(), "{p} should be live");
        }
        for p in [
            InstancePhase::Stopped,
            InstancePhase::Fenced,
            InstancePhase::NeedsRebuild,
            InstancePhase::Failed,
            InstancePhase::Terminated,
        ] {
            assert!(!p.is_live(), "{p} should not be live");
        }
    }

    #[test]
    fn fenced_instances_are_never_restarted() {
        // This is the split-brain guard in enum form: if the reconciler
        // could restart a fenced instance, a failover would race a
        // helpful restart of the old primary (ADR 02 §4).
        assert!(InstancePhase::Fenced.blocks_restart());
        assert!(InstancePhase::NeedsRebuild.blocks_restart());
        assert!(!InstancePhase::Running.blocks_restart());
        assert!(!InstancePhase::Failed.blocks_restart());
    }

    #[test]
    fn roles_come_from_pg_is_in_recovery() {
        assert_eq!(InstanceRole::from_in_recovery(true), InstanceRole::Standby);
        assert_eq!(InstanceRole::from_in_recovery(false), InstanceRole::Primary);
    }

    #[test]
    fn roles_round_trip_via_str() {
        for r in [
            InstanceRole::Primary,
            InstanceRole::Standby,
            InstanceRole::Unknown,
        ] {
            assert_eq!(InstanceRole::from_str(r.as_str()).unwrap(), r);
        }
    }
}
