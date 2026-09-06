//! Shared types for the pgpod workspace.
//!
//! Everything here is pure: no podman, no SQLite, no object storage, no
//! I/O beyond reading environment variables for the XDG path layout. Two
//! crates that need the same type promote it here rather than duplicating
//! it (see `adrs/00-project-setup.md` §2).

mod ids;
mod lifecycle;
mod manifest;
mod paths;
mod secret;
mod spec;

pub use ids::{ClusterId, InstanceId, ParseInstanceIdError};
pub use lifecycle::{InstancePhase, InstanceRole, ParsePhaseError, ParseRoleError};
pub use manifest::{
    API_VERSION, BootstrapSpec, ClusterManifest, ClusterSpec, KIND, ManifestError, Metadata,
    PostgresqlSpec, StorageSpec,
};
pub use paths::{PathLayout, container};
pub use secret::Secret;
pub use spec::{Bootstrap, InitdbBootstrap, InstanceSpec, SPEC_ENV, SpecError};
