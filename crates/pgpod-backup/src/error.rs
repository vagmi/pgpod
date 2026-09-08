//! Errors, shaped so the message names the fix.
//!
//! Most of these are seen either at `apply` time or in a PostgreSQL log
//! line, and in both places the reader has no access to pgpod's internals.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("backup destination {url:?}: {detail}")]
    Destination { url: String, detail: String },

    #[error("no backup destination is configured")]
    NoDestinations,

    #[error(
        "pgBackRest supports at most {max} repositories, but {given} \
         destinations are configured"
    )]
    TooManyRepositories { max: usize, given: usize },

    #[error("could not read pgBackRest's output: {0}")]
    Info(String),

    #[error("{0}")]
    Invalid(String),
}
