use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The rootless socket is not where we expected it. Almost always
    /// means `podman.socket` is not enabled under `systemd --user`, or
    /// that linger is off and the user session went away (ADR 03 §2).
    #[error(
        "podman socket not found at {0}\n\
         hint: systemctl --user enable --now podman.socket\n\
         hint: loginctl enable-linger $(id -un)   # so it survives logout"
    )]
    SocketNotFound(PathBuf),

    #[error("podman daemon not reachable: {0}")]
    Unreachable(String),

    #[error("container operation failed: {0}")]
    Container(String),

    #[error("volume operation failed: {0}")]
    Volume(String),

    #[error("podman-api: {0}")]
    Podman(#[from] podman_api::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
