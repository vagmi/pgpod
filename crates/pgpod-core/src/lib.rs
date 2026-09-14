//! Shared types for the pgpod workspace.
//!
//! Everything here is pure: no podman, no SQLite, no object storage, no
//! I/O beyond reading environment variables for the XDG path layout. Two
//! crates that need the same type promote it here rather than duplicating
//! it (see `adrs/00-project-setup.md` §2).

mod backup;
mod duration;
mod ids;
mod lifecycle;
mod manifest;
mod paths;
mod pooler;
mod secret;
mod spec;
mod upgrade;

pub use backup::{
    BACKUP_SPEC_ENV, BackupJobSpec, BackupSpec, Destination, RetentionMode,
    SECRET_OBJECT_STORE_PREFIX,
};
pub use duration::{HumanDuration, ParseDurationError};
pub use ids::{ClusterId, InstanceId, ParseInstanceIdError, PoolerId};
pub use lifecycle::{InstancePhase, InstanceRole, ParsePhaseError, ParseRoleError};
pub use manifest::{
    API_VERSION, BootstrapSpec, ClusterManifest, ClusterSpec, KIND, KIND_POOLER, Manifest,
    ManifestError, Metadata, PgDoormanSpec, PoolMode, PoolRef, PoolerClusterRef, PoolerManifest,
    PoolerSpecManifest, PoolerType, PostgresqlSpec, StorageSpec,
};
pub use paths::{PathLayout, container};
pub use pooler::{POOLER_SPEC_ENV, PoolTarget, PoolerSpec};
pub use secret::Secret;
pub use spec::{Bootstrap, InitdbBootstrap, InstanceSpec, RecoveryBootstrap, SPEC_ENV, SpecError};
pub use upgrade::{
    ProbeReport, REPORT_MARKER, StageReport, UPGRADE_SPEC_ENV, UpgradeMethod, UpgradeRunReport,
    UpgradeSpec, major_label, parse_report, print_report, version_key,
};
