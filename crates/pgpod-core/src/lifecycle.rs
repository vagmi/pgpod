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

/// Where a *cluster* is in its lifecycle, as the registry records it.
///
/// Deliberately a parsed enum rather than the bare `String` the registry
/// column holds. The instance phase has been an enum since Phase 1 while
/// the cluster phase was free text, and that asymmetry is exactly where a
/// typo turns into a silent skip: code comparing against `"running"` sees
/// `"Running"` as an unknown state and quietly does nothing.
///
/// Parsing stays lenient at the edges — [`ClusterPhase::parse_lenient`]
/// maps anything unrecognised to [`ClusterPhase::Unknown`] rather than
/// failing — because a value written by an older pgpod must not make
/// `pgpod status` error out on every cluster. `Unknown` is then treated as
/// "not safe to act on", which is the conservative direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClusterPhase {
    /// An `apply` is in flight. Containers may be half-created and the
    /// stored manifest may be ahead of what is running.
    Applying,
    /// The last operation completed. This is the only phase in which the
    /// cluster is known to be exactly what its manifest describes.
    Running,
    /// A major-version upgrade holds the cluster here for its whole
    /// window — from the pooler hold to the image swap (ADR 06). A host
    /// that reboots inside it comes back with the primary's container
    /// stopped or removed and its data mid-migration.
    Upgrading,
    /// `pgpod delete` without `--purge`. The containers are gone and the
    /// volumes were kept on purpose. This is an operator's intent, not a
    /// fault, and nothing should undo it on its own.
    Stopped,
    /// An operation failed with the cluster already recorded. Distinct
    /// from `Applying` precisely so a failure is not mistaken for work
    /// still in progress: nothing clears this but another `apply`.
    Failed,
    /// A phase string this version does not recognise — from a newer
    /// pgpod, or a hand-edited registry.
    Unknown,
}

impl ClusterPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Applying => "applying",
            Self::Running => "running",
            Self::Upgrading => "upgrading",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    /// Whether boot recovery may start this cluster's containers.
    ///
    /// Only `Running` qualifies, and the reason is the same for each of
    /// the others: they describe a cluster whose on-disk state a human
    /// has to reconcile with its records. `Applying` and `Upgrading` mean
    /// an operation was cut off partway; `Stopped` is a deliberate
    /// shutdown; `Failed` needs the manifest fixed; `Unknown` is not
    /// understood at all. Starting containers under any of them would be
    /// guessing at intent, and the guess is unattended.
    pub fn resumable(&self) -> bool {
        matches!(self, Self::Running)
    }

    /// Why this cluster is not resumable, phrased for an operator.
    ///
    /// Returns `None` when it is. Each answer names the way out, because
    /// a boot report that only says "skipped" leaves someone reading
    /// source to find out what to do.
    pub fn skip_reason(&self) -> Option<&'static str> {
        match self {
            Self::Running => None,
            Self::Applying => Some(
                "an apply was interrupted — re-run `pgpod apply -f <manifest>` \
                 to finish it",
            ),
            Self::Upgrading => Some(
                "an upgrade was interrupted — check `pgpod status` and the \
                 instance volume before starting anything",
            ),
            Self::Stopped => {
                Some("deliberately stopped — `pgpod apply -f <manifest>` brings it back")
            }
            Self::Failed => Some("the last apply failed — fix the manifest and apply it again"),
            Self::Unknown => {
                Some("its recorded phase is not one this version of pgpod understands")
            }
        }
    }

    /// Parse a stored phase, mapping anything unrecognised to
    /// [`ClusterPhase::Unknown`].
    ///
    /// There is no `FromStr` on purpose. Every caller reads this from the
    /// registry, where an unknown value is a thing to report rather than
    /// an error to propagate, and offering a failing parse alongside would
    /// invite using the wrong one.
    pub fn parse_lenient(s: &str) -> Self {
        match s {
            "applying" => Self::Applying,
            "running" => Self::Running,
            "upgrading" => Self::Upgrading,
            "stopped" => Self::Stopped,
            "failed" => Self::Failed,
            _ => Self::Unknown,
        }
    }
}

impl std::fmt::Display for ClusterPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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

#[cfg(test)]
mod cluster_phase_tests {
    use super::*;

    const ALL: [ClusterPhase; 6] = [
        ClusterPhase::Applying,
        ClusterPhase::Running,
        ClusterPhase::Upgrading,
        ClusterPhase::Stopped,
        ClusterPhase::Failed,
        ClusterPhase::Unknown,
    ];

    #[test]
    fn known_phases_round_trip_via_str() {
        for p in ALL {
            assert_eq!(ClusterPhase::parse_lenient(p.as_str()), p);
        }
    }

    #[test]
    fn only_a_running_cluster_may_be_resumed() {
        // The whole point of the enum. Every other phase describes a
        // cluster whose records and on-disk state a human has to
        // reconcile, and boot recovery runs with nobody watching.
        assert!(ClusterPhase::Running.resumable());
        for p in ALL.iter().filter(|p| **p != ClusterPhase::Running) {
            assert!(!p.resumable(), "{p} must not be resumable");
        }
    }

    #[test]
    fn an_unrecognised_phase_is_unknown_rather_than_an_error() {
        // A value from a newer pgpod, or a hand-edited row, must not make
        // every command that reads the registry fail — and must not be
        // mistaken for something safe to act on either.
        for s in ["", "Running", "promoting", "garbage"] {
            assert_eq!(
                ClusterPhase::parse_lenient(s),
                ClusterPhase::Unknown,
                "{s:?} should parse as unknown"
            );
        }
        assert!(!ClusterPhase::Unknown.resumable());
    }

    #[test]
    fn every_skipped_phase_says_why_and_the_resumable_one_does_not() {
        // A boot report that only says "skipped" sends the reader to the
        // source to find out what to do about it.
        assert!(ClusterPhase::Running.skip_reason().is_none());
        for p in ALL.iter().filter(|p| **p != ClusterPhase::Running) {
            let reason = p
                .skip_reason()
                .unwrap_or_else(|| panic!("{p} needs a reason"));
            assert!(!reason.is_empty(), "{p} has an empty reason");
        }
    }

    #[test]
    fn the_phases_apply_and_upgrade_actually_write_are_all_known() {
        // These are the literals in pgpod-control. If one is renamed
        // there without being added here, `parse_lenient` starts
        // returning Unknown and boot recovery silently stops resuming.
        for written in ["applying", "running", "upgrading", "stopped", "failed"] {
            assert_ne!(
                ClusterPhase::parse_lenient(written),
                ClusterPhase::Unknown,
                "{written} is written by pgpod-control but not understood here"
            );
        }
    }
}
