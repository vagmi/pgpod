//! pgBackRest integration: configuration, invocation, and repository queries.
//!
//! pgpod does not implement WAL archiving or base backups. pgBackRest owns
//! the repository format, the manifests, retention, verification and the
//! restore; this crate is the seam between pgpod's manifest vocabulary and
//! pgBackRest's (ADR 04).
//!
//! Everything here is **pure** — rendering configuration and parsing
//! output. Running pgBackRest is the caller's job, because the two callers
//! do it differently: the agent spawns it inside the instance container,
//! and the daemon runs it in a job container.

mod config;
mod error;
mod info;

pub use config::{CONFIG_FILE, MAX_REPOSITORIES, PgBackRestConfig, RepoKind, Repository};
pub use error::Error;
pub use info::{ArchiveInfo, Backup, BackupType, StanzaInfo, Status, parse_info, stanza};
