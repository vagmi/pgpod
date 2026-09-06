//! Minimal logging to stderr.
//!
//! Not `tracing`: the agent is PID 1 in a container, so its output goes
//! straight to `podman logs` interleaved with PostgreSQL's own. A plain
//! prefixed line is the most legible thing there, and it keeps a
//! subscriber and its formatting machinery out of a binary that has to
//! stay small and statically linked.

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        eprintln!("pgpod-agent: {}", format!($($arg)*))
    };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        eprintln!("pgpod-agent: WARN {}", format!($($arg)*))
    };
}
