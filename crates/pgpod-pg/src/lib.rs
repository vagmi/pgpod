//! PostgreSQL domain logic for pgpod.
//!
//! Everything in this crate is a pure function over a spec: rendering
//! `postgresql.conf` and `pg_hba.conf`, building `initdb` argv, generating
//! bootstrap SQL, planning a recovery. It is the majority of the logic
//! that can lose data, so it is deliberately free of I/O and testable
//! without a container in sight (`adrs/00-project-setup.md` §2).
//!
//! It is shared by the daemon and the agent, which is why it must never
//! grow a dependency on `podman-api` or `rusqlite`.

mod conf;
mod hba;
mod initdb;
mod roles;

pub use conf::{
    ArchiveMode, INCLUDE_DIR_LINE, MANAGED_CONF_FILE, ManagedConf, RESERVED_PARAMETERS,
    StandbyConf, USER_CONF_FILE, render_user_conf,
};
pub use hba::{AuthMethod, HbaConfig};
pub use initdb::{InitdbOptions, PWFILE_PATH};
pub use roles::{AppDatabase, BootstrapRoles, MONITOR_ROLE, REPLICATION_ROLE};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error(
        "{0} is managed by pgpod and cannot be set in spec.postgresql.parameters — \
         changing it would break backups, replication, or recovery"
    )]
    ReservedParameter(String),

    #[error("{0:?} is not a valid PostgreSQL parameter name")]
    InvalidParameterName(String),

    #[error(
        "{0:?} is not a valid PostgreSQL identifier — use lowercase letters, \
         digits, and underscores, starting with a letter or underscore"
    )]
    InvalidIdentifier(String),
}
