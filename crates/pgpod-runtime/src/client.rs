//! Rootless Podman client.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use podman_api::Podman;

use crate::error::{Error, Result};
use crate::info::PodmanInfo;
use crate::socket::{SocketSource, discover, socket_uri};

/// A handle to the rootless Podman socket. Cheap to clone (`Arc` inside).
#[derive(Debug, Clone)]
pub struct PodmanClient {
    inner: Arc<Podman>,
    socket_path: PathBuf,
    socket_source: SocketSource,
}

impl PodmanClient {
    /// Discover the socket and connect.
    pub fn connect() -> Result<Self> {
        let (path, source) = discover();
        Self::connect_at_with_source(path, source)
    }

    /// Connect to a specific socket path. Fails immediately if the socket
    /// file is absent, so the error names the path rather than surfacing
    /// later as an opaque connection refusal.
    pub fn connect_at(path: impl Into<PathBuf>) -> Result<Self> {
        Self::connect_at_with_source(path.into(), SocketSource::EnvOverride)
    }

    fn connect_at_with_source(path: PathBuf, source: SocketSource) -> Result<Self> {
        if !path.exists() {
            return Err(Error::SocketNotFound(path));
        }
        let inner =
            Podman::new(socket_uri(&path)).map_err(|e| Error::Unreachable(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(inner),
            socket_path: path,
            socket_source: source,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn socket_source(&self) -> SocketSource {
        self.socket_source
    }

    /// The underlying handle, for operations this wrapper does not expose
    /// yet.
    ///
    /// Deliberately `pub(crate)`. `adrs/00-project-setup.md` §1 makes the
    /// case that no `podman_api` type crosses a crate boundary — that
    /// mitigation only holds if the escape hatch is not public.
    pub(crate) fn podman(&self) -> &Podman {
        &self.inner
    }

    /// `GET /_ping`. The cheapest proof the daemon is actually answering,
    /// as opposed to the socket file merely existing — a stale socket from
    /// a killed session looks fine on the filesystem.
    pub async fn ping(&self) -> Result<()> {
        self.inner
            .ping()
            .await
            .map(|_| ())
            .map_err(|e| Error::Unreachable(e.to_string()))
    }

    /// The raw inspect payload for a container, as JSON.
    ///
    /// Exposed for the hardening tests, which must assert what podman
    /// actually applied rather than what pgpod's own types claim was
    /// requested — a builder that silently stops mapping to its wire field
    /// would otherwise go unnoticed (ADR 00 §8). Not for production code
    /// paths: use the typed accessors.
    pub async fn inspect_raw(&self, id: &str) -> Result<serde_json::Value> {
        let containers = self.inner.containers();
        let data = containers
            .get(id)
            .inspect()
            .await
            .map_err(|e| Error::Container(format!("inspect {id}: {e}")))?;
        serde_json::to_value(data)
            .map_err(|e| Error::Container(format!("serialize inspect payload: {e}")))
    }

    /// Host facts, flattened into pgpod's own type.
    pub async fn info(&self) -> Result<PodmanInfo> {
        let raw = self
            .inner
            .info()
            .await
            .map_err(|e| Error::Unreachable(e.to_string()))?;
        Ok(PodmanInfo::from(raw))
    }
}
