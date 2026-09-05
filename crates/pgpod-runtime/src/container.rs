//! Container lifecycle.
//!
//! Phase 0 covers what the volume smoke test needs — pull, create, start,
//! wait, logs, remove — with the spec type shaped for what Phase 1 adds
//! (hardening flags, networks, secrets, streamed exec).

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use podman_api::opts::{
    ContainerCreateOpts, ContainerDeleteOpts, ContainerLogsOpts, ContainerWaitOpts, PullOpts,
};

use crate::client::PodmanClient;
use crate::error::{Error, Result};

/// A mount into a container. Either a named volume (how PGDATA is
/// delivered) or a read-only host bind (how the agent binary is
/// delivered). Nothing else should need a host bind — see ADR 00 §4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// A podman named volume — how PGDATA is delivered.
    ///
    /// `chown` maps to podman's `U` mount option, which recursively
    /// chowns the volume to the container's user. Podman already does
    /// this for an *empty* volume on first mount, which is the case
    /// pgpod's storage model relies on; `U` is the explicit lever for
    /// when that implicit behaviour does not apply (a volume that already
    /// has content, or an image that ships content at the mount point).
    /// `volume_smoke.rs` establishes which is needed on a given host.
    Volume {
        name: String,
        target: String,
        chown: bool,
    },
    /// A read-only bind of a single host path. Used for the static agent
    /// binary, which has no ownership problem because it is mode 0755 and
    /// never written to.
    BindReadOnly { source: String, target: String },
    /// A tmpfs. The container root filesystem is read-only, so `/tmp` and
    /// the postgres socket directory need these.
    Tmpfs { target: String },
}

/// What to create a container with.
#[derive(Debug, Clone)]
pub struct ContainerSpec {
    pub name: Option<String>,
    pub image: String,
    /// Argv. Empty leaves the image's own `ENTRYPOINT`/`CMD` in effect.
    pub command: Vec<String>,
    /// Overrides the image's `ENTRYPOINT`. pgpod instance containers set
    /// this to the agent, bypassing the image entrypoint entirely
    /// (ADR 00 §6).
    pub entrypoint: Vec<String>,
    pub env: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    pub labels: Vec<(String, String)>,
    /// Run as this user inside the container (`"999"` or `"999:999"`).
    /// `None` uses the image's own `USER`.
    pub user: Option<String>,
    pub working_dir: Option<String>,
    /// Remove automatically on exit. Right for job containers; wrong for
    /// instances, whose lifetime the reconciler owns.
    pub auto_remove: bool,
}

impl ContainerSpec {
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            name: None,
            image: image.into(),
            command: Vec::new(),
            entrypoint: Vec::new(),
            env: Vec::new(),
            mounts: Vec::new(),
            labels: Vec::new(),
            user: None,
            working_dir: None,
            auto_remove: false,
        }
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn command<I, S>(mut self, cmd: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.command = cmd.into_iter().map(Into::into).collect();
        self
    }

    pub fn mount(mut self, m: Mount) -> Self {
        self.mounts.push(m);
        self
    }

    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    pub fn label(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.labels.push((k.into(), v.into()));
        self
    }
}

/// Handle to a container podman created. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Container {
    id: String,
    client: PodmanClient,
}

/// What podman knows about a container. `None` from [`Container::probe`]
/// means podman has never heard of it, which is how the reconciler tells
/// adopt from recreate.
#[derive(Debug, Clone)]
pub struct ContainerProbe {
    pub id: String,
    pub name: Option<String>,
    pub running: bool,
    pub status: Option<String>,
    pub exit_code: Option<i32>,
}

impl PodmanClient {
    /// Pull an image if it is not already present. Podman's pull is a
    /// progress stream; we drain it and surface only the outcome, because
    /// nothing in Phase 0 renders progress.
    pub async fn pull_image_if_absent(&self, image: &str) -> Result<()> {
        if self
            .podman()
            .images()
            .get(image)
            .exists()
            .await
            .unwrap_or(false)
        {
            return Ok(());
        }

        let opts = PullOpts::builder().reference(image).build();
        let images = self.podman().images();
        let mut stream = images.pull(&opts);
        while let Some(chunk) = stream.next().await {
            chunk.map_err(|e| Error::Container(format!("pull {image}: {e}")))?;
        }
        Ok(())
    }

    /// Create a container from `spec`. Created, not started.
    pub async fn create_container(&self, spec: &ContainerSpec) -> Result<Container> {
        let mut b = ContainerCreateOpts::builder().image(&spec.image);

        if let Some(name) = &spec.name {
            b = b.name(name);
        }
        if !spec.command.is_empty() {
            b = b.command(spec.command.clone());
        }
        if !spec.entrypoint.is_empty() {
            b = b.entrypoint(spec.entrypoint.clone());
        }
        if !spec.env.is_empty() {
            b = b.env(spec.env.iter().cloned().collect::<HashMap<_, _>>());
        }
        if !spec.labels.is_empty() {
            b = b.labels(spec.labels.iter().cloned().collect::<HashMap<_, _>>());
        }
        if let Some(user) = &spec.user {
            b = b.user(user);
        }
        if let Some(wd) = &spec.working_dir {
            b = b.work_dir(wd);
        }
        if spec.auto_remove {
            b = b.remove(true);
        }

        // Named volumes and bind mounts go through different fields of the
        // libpod create payload: `volumes` for named volumes, `mounts` for
        // binds and tmpfs. Conflating them makes podman create an
        // anonymous volume and silently ignore the source path.
        let mut named_volumes = Vec::new();
        let mut oci_mounts = Vec::new();
        for m in &spec.mounts {
            match m {
                Mount::Volume {
                    name,
                    target,
                    chown,
                } => {
                    named_volumes.push(podman_api::models::NamedVolume {
                        dest: Some(target.clone()),
                        name: Some(name.clone()),
                        options: chown.then(|| vec!["U".to_string()]),
                        is_anonymous: Some(false),
                    });
                }
                Mount::BindReadOnly { source, target } => {
                    oci_mounts.push(podman_api::models::ContainerMount {
                        destination: Some(target.clone()),
                        source: Some(source.clone()),
                        _type: Some("bind".into()),
                        options: Some(vec!["ro".into()]),
                        uid_mappings: None,
                        gid_mappings: None,
                    });
                }
                Mount::Tmpfs { target } => {
                    oci_mounts.push(podman_api::models::ContainerMount {
                        destination: Some(target.clone()),
                        source: None,
                        _type: Some("tmpfs".into()),
                        options: Some(vec!["rw".into(), "nosuid".into(), "nodev".into()]),
                        uid_mappings: None,
                        gid_mappings: None,
                    });
                }
            }
        }
        if !named_volumes.is_empty() {
            b = b.volumes(named_volumes);
        }
        if !oci_mounts.is_empty() {
            b = b.mounts(oci_mounts);
        }

        let created = self
            .podman()
            .containers()
            .create(&b.build())
            .await
            .map_err(|e| Error::Container(format!("create: {e}")))?;

        Ok(Container {
            id: created.id,
            client: self.clone(),
        })
    }

    /// Attach to an existing container by id or name. Does not verify it
    /// exists — call [`Container::probe`] for that.
    pub fn container(&self, id: impl Into<String>) -> Container {
        Container {
            id: id.into(),
            client: self.clone(),
        }
    }
}

impl Container {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub async fn start(&self) -> Result<()> {
        self.client
            .podman()
            .containers()
            .get(&self.id)
            .start(None)
            .await
            .map_err(|e| Error::Container(format!("start {}: {e}", self.id)))
    }

    pub async fn stop(&self, timeout: Duration) -> Result<()> {
        let opts = podman_api::opts::ContainerStopOpts::builder()
            .timeout(timeout.as_secs() as usize)
            .build();
        self.client
            .podman()
            .containers()
            .get(&self.id)
            .stop(&opts)
            .await
            .map_err(|e| Error::Container(format!("stop {}: {e}", self.id)))
    }

    /// Ask podman about this container. `Ok(None)` when podman does not
    /// know it.
    pub async fn probe(&self) -> Result<Option<ContainerProbe>> {
        let handle = self.client.podman().containers().get(&self.id);
        if !handle
            .exists()
            .await
            .map_err(|e| Error::Container(format!("exists {}: {e}", self.id)))?
        {
            return Ok(None);
        }
        let data = handle
            .inspect()
            .await
            .map_err(|e| Error::Container(format!("inspect {}: {e}", self.id)))?;
        let state = data.state;
        Ok(Some(ContainerProbe {
            id: data.id.unwrap_or_else(|| self.id.clone()),
            name: data.name,
            running: state.as_ref().and_then(|s| s.running).unwrap_or(false),
            status: state.as_ref().and_then(|s| s.status.clone()),
            exit_code: state.as_ref().and_then(|s| s.exit_code),
        }))
    }

    /// Block until the container exits, then return its exit code.
    ///
    /// `podman-api`'s `wait` discards the response body, so the code comes
    /// from a follow-up inspect. That is also why this cannot be used with
    /// `auto_remove` containers — they are gone before we can look.
    pub async fn wait_for_exit(&self) -> Result<i32> {
        self.client
            .podman()
            .containers()
            .get(&self.id)
            .wait(&ContainerWaitOpts::builder().build())
            .await
            .map_err(|e| Error::Container(format!("wait {}: {e}", self.id)))?;

        self.probe()
            .await?
            .and_then(|p| p.exit_code)
            .ok_or_else(|| Error::Container(format!("{}: exited without an exit code", self.id)))
    }

    /// Collected stdout+stderr as a string. Fine for short-lived job
    /// containers; instance logs get streamed instead.
    pub async fn logs_string(&self) -> Result<String> {
        let opts = ContainerLogsOpts::builder()
            .stdout(true)
            .stderr(true)
            .build();
        let containers = self.client.podman().containers();
        let handle = containers.get(&self.id);
        let mut stream = handle.logs(&opts);
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            match chunk.map_err(|e| Error::Container(format!("logs {}: {e}", self.id)))? {
                podman_api::conn::TtyChunk::StdOut(b) | podman_api::conn::TtyChunk::StdErr(b) => {
                    out.extend_from_slice(&b)
                }
                podman_api::conn::TtyChunk::StdIn(_) => {}
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Remove the container. Does not touch its volumes — removing a
    /// volume is a separate, explicit call (`AGENTS.md` principle 4).
    pub async fn remove(&self, force: bool) -> Result<()> {
        let opts = ContainerDeleteOpts::builder().force(force).build();
        self.client
            .podman()
            .containers()
            .get(&self.id)
            .delete(&opts)
            .await
            .map_err(|e| Error::Container(format!("delete {}: {e}", self.id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_builder_composes() {
        let s = ContainerSpec::new("docker.io/library/alpine:3")
            .name("pgpod-test")
            .command(["stat", "-c", "%u:%g", "/pgdata"])
            .mount(Mount::Volume {
                name: "pgpod-test-pgdata".into(),
                target: "/pgdata".into(),
                chown: false,
            })
            .label("pgpod.cluster", "test");

        assert_eq!(s.name.as_deref(), Some("pgpod-test"));
        assert_eq!(s.command.len(), 4);
        assert_eq!(s.mounts.len(), 1);
        assert_eq!(
            s.labels,
            vec![("pgpod.cluster".to_string(), "test".to_string())]
        );
    }

    #[test]
    fn volume_and_bind_mounts_are_distinct_variants() {
        // They serialize into different libpod payload fields; a single
        // "mount" type that guessed from the source string would create an
        // anonymous volume and drop the bind silently.
        let vol = Mount::Volume {
            name: "v".into(),
            target: "/t".into(),
            chown: false,
        };
        let bind = Mount::BindReadOnly {
            source: "/h".into(),
            target: "/t".into(),
        };
        assert_ne!(vol, bind);
    }
}
