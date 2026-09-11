//! pg_doorman, as pure functions.
//!
//! The same shape as `pgpod-backup`: this crate renders a tool's
//! configuration and speaks its control protocol, and does no I/O at all.
//! The socket and the process belong to `pgpod-agent`, which is the only
//! thing that ever talks to a pooler (ADR 05 §5).
//!
//! Keeping it I/O-free is what lets the agent depend on it without
//! growing: the agent is a static musl binary bind-mounted into an
//! arbitrary image, and everything here is bytes and strings.

mod admin;
mod config;
mod control;

pub use admin::{
    Backend, PoolRow, QueryResult, decode, md5_password, parse_pools, password_message,
    query_message, startup_message, terminate_message,
};
pub use config::{RESERVED_PARAMETERS, Secrets, lookup_query, render};
pub use control::{PoolStatus, Request, Response};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the pooler spec is self-contradictory: {0}")]
    Spec(String),

    #[error(
        "{0:?} is managed by pgpod and cannot be set in \
         spec.pgDoorman.parameters"
    )]
    ReservedParameter(String),

    #[error("could not render the pg_doorman configuration: {0}")]
    Render(String),

    #[error(
        "no lookup password for pool {0:?} — the daemon must mount one \
         podman secret per fronted cluster"
    )]
    MissingSecret(String),

    #[error("the pooler spoke something other than the postgres protocol: {0}")]
    Protocol(String),
}
