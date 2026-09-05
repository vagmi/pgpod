//! Rootless Podman socket discovery.
//!
//! Resolution order:
//! 1. `PGPOD_PODMAN_SOCKET` — explicit override for tests and for hosts
//!    with an unusual layout.
//! 2. `$XDG_RUNTIME_DIR/podman/podman.sock` — the `systemd --user` default.
//! 3. `/run/user/<uid>/podman/podman.sock` — the fallback for when
//!    `XDG_RUNTIME_DIR` is unset, which is exactly what happens under
//!    `sudo -iu` (ADR 03 §2). Guessing here beats failing, because the
//!    guess is right on every systemd host.

use std::path::{Path, PathBuf};

pub const ENV_OVERRIDE: &str = "PGPOD_PODMAN_SOCKET";

/// How the socket path was arrived at. Reported by `pgpod doctor`, because
/// "the socket is missing" and "the socket is missing *and* we guessed the
/// path because XDG_RUNTIME_DIR was unset" are different problems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketSource {
    EnvOverride,
    XdgRuntimeDir,
    UidFallback,
}

impl SocketSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EnvOverride => "PGPOD_PODMAN_SOCKET",
            Self::XdgRuntimeDir => "XDG_RUNTIME_DIR",
            Self::UidFallback => "/run/user/<uid> fallback",
        }
    }
}

/// Compute the expected socket path and record how we got it. Does not
/// check that the socket exists — that is the caller's job, so the error
/// can carry the resolution provenance.
pub fn discover() -> (PathBuf, SocketSource) {
    if let Some(explicit) = std::env::var_os(ENV_OVERRIDE) {
        return (PathBuf::from(explicit), SocketSource::EnvOverride);
    }
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return (
            PathBuf::from(runtime_dir).join("podman/podman.sock"),
            SocketSource::XdgRuntimeDir,
        );
    }
    let uid = nix::unistd::Uid::current();
    (
        PathBuf::from(format!("/run/user/{uid}/podman/podman.sock")),
        SocketSource::UidFallback,
    )
}

/// Just the path, for callers that do not care about provenance.
pub fn default_socket_path() -> PathBuf {
    discover().0
}

/// Format a socket path as the `unix://…` URI `podman-api` wants.
pub fn socket_uri(path: &Path) -> String {
    format!("unix://{}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_uri_has_the_unix_scheme() {
        assert_eq!(
            socket_uri(Path::new("/run/user/1000/podman/podman.sock")),
            "unix:///run/user/1000/podman/podman.sock"
        );
    }

    #[test]
    fn discovery_always_yields_an_absolute_path() {
        // Whichever branch is taken on the test host, the result must be
        // usable as a socket path without further massaging.
        let (path, _source) = discover();
        assert!(path.is_absolute(), "got {path:?}");
        assert!(path.to_string_lossy().ends_with(".sock"), "got {path:?}");
    }
}
