//! Shared types for the pgpod workspace.
//!
//! Everything here is pure: no podman, no SQLite, no object storage, no
//! I/O beyond reading environment variables for the XDG path layout. Two
//! crates that need the same type promote it here rather than duplicating
//! it (see `adrs/00-project-setup.md` §2).

mod ids;
mod lifecycle;
mod paths;

pub use ids::{ClusterId, InstanceId, ParseInstanceIdError};
pub use lifecycle::{InstancePhase, InstanceRole, ParsePhaseError, ParseRoleError};
pub use paths::{PathLayout, container};
