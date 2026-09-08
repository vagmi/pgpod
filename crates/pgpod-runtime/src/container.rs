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

/// Where podman materialises mounted secrets. Must be a writable
/// filesystem before container init, or secret mounting fails — see
/// [`ContainerSpec::hardened`].
pub const SECRETS_DIR: &str = "/run/secrets";

/// One host-side port publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortPublish {
    /// Host interface. Defaults to loopback via [`PortPublish::loopback`]
    /// — a database should not appear on the LAN because someone forgot a
    /// flag.
    pub host_ip: Option<String>,
    pub host_port: u16,
    pub container_port: u16,
}

impl PortPublish {
    pub fn loopback(host_port: u16, container_port: u16) -> Self {
        Self {
            host_ip: Some("127.0.0.1".to_string()),
            host_port,
            container_port,
        }
    }
}

/// A podman secret mounted into the container at `target`.
///
/// Secrets go here rather than into `env` because environment variables
/// are visible in `podman inspect`, in `/proc/<pid>/environ`, and in log
/// lines on error paths (ADR 00 §9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretMount {
    /// Name of an existing podman secret.
    pub name: String,
    /// Absolute path inside the container.
    pub target: String,
    /// Octal mode, e.g. `0o400`.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// What podman should do when the container exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartPolicy {
    /// pgpod's default. The reconciler owns restarts — podman's own
    /// policy would race it and restart instances the reconciler has
    /// deliberately fenced (`AGENTS.md` container hardening).
    #[default]
    No,
    OnFailure,
    Always,
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
    /// Podman networks to join. Empty uses podman's default.
    pub networks: Vec<String>,
    pub port_publishes: Vec<PortPublish>,
    pub secrets: Vec<SecretMount>,
    /// Linux capabilities to drop. pgpod passes `["ALL"]`; postgres needs
    /// none.
    pub cap_drop: Vec<String>,
    pub no_new_privileges: bool,
    /// Override the host's log driver. `None` — the right answer for
    /// anything an operator will ever read — inherits it.
    ///
    /// Instance containers must **not** set this. Their output is
    /// PostgreSQL's log, and on a VM that belongs in journald: rotation
    /// comes for free and the o11y agent already on the box collects it
    /// with everything else. Overriding that would trade the operator's
    /// story for pgpod's convenience.
    ///
    /// The exception is a job container whose stdout is a *return value*
    /// rather than a log — `pgbackrest info --output=json`, which the
    /// control plane parses. Routing a machine-readable payload through
    /// the system journal is wrong twice over: it puts noise in the
    /// operator's journal, and journald's `LineMax` (48 KiB by default)
    /// silently truncates a long document, which would surface as a JSON
    /// parse error whose cause is nowhere near the code that failed.
    pub log_driver: Option<String>,
    /// Read-only container root filesystem. The volume and any tmpfs
    /// mounts remain writable.
    pub read_only_fs: bool,
    pub restart_policy: RestartPolicy,
    pub hostname: Option<String>,
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
            networks: Vec::new(),
            port_publishes: Vec::new(),
            secrets: Vec::new(),
            cap_drop: Vec::new(),
            no_new_privileges: false,
            log_driver: None,
            read_only_fs: false,
            restart_policy: RestartPolicy::No,
            hostname: None,
        }
    }

    /// A spec with pgpod's container hardening applied.
    ///
    /// This is the shape every PostgreSQL instance and job container gets
    /// (`AGENTS.md`, "Container hardening"). It is a constructor rather
    /// than a set of defaults on `new` so that a caller who genuinely
    /// wants an unhardened container has to say so, and so the hardening
    /// can be asserted as a unit in tests.
    pub fn hardened(image: impl Into<String>) -> Self {
        Self {
            cap_drop: vec!["ALL".to_string()],
            no_new_privileges: true,
            read_only_fs: true,
            restart_policy: RestartPolicy::No,
            // A read-only root filesystem means every writable path has
            // to be an explicit tmpfs. Only two are needed:
            //
            // * `/tmp` — scratch.
            // * `/run/secrets` — podman materialises mounted secrets here
            //   and must *create* each mountpoint. On a read-only rootfs
            //   that fails during container init with an opaque runc
            //   error, before anything of ours runs. A tmpfs at `/run`
            //   does not substitute: runc creates the secret mountpoints
            //   before that mount is applied, so the parent of the secret
            //   targets is what has to be writable.
            //
            // The postgres socket directory is deliberately *not* here.
            // It lives inside the volume (`container::SOCKET_DIR`),
            // because a tmpfs over the image's own directory comes up
            // root-owned — `tmpcopyup` does not preserve ownership — and
            // PostgreSQL then cannot create its lock file.
            mounts: vec![
                Mount::Tmpfs {
                    target: "/tmp".to_string(),
                },
                Mount::Tmpfs {
                    target: SECRETS_DIR.to_string(),
                },
            ],
            ..Self::new(image)
        }
    }

    pub fn entrypoint<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.entrypoint = argv.into_iter().map(Into::into).collect();
        self
    }

    pub fn network(mut self, name: impl Into<String>) -> Self {
        self.networks.push(name.into());
        self
    }

    pub fn publish(mut self, p: PortPublish) -> Self {
        self.port_publishes.push(p);
        self
    }

    /// Send this container's output to a specific log driver instead of
    /// the host's. See [`ContainerSpec::log_driver`] — only a job
    /// container whose stdout is a return value should use this.
    pub fn log_driver(mut self, driver: impl Into<String>) -> Self {
        self.log_driver = Some(driver.into());
        self
    }

    pub fn secret(mut self, s: SecretMount) -> Self {
        self.secrets.push(s);
        self
    }

    pub fn hostname(mut self, name: impl Into<String>) -> Self {
        self.hostname = Some(name.into());
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
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
    /// The container's environment, as podman reports it: `KEY=value`.
    ///
    /// Carried because the instance spec travels in one of these
    /// (`pgpod_core::SPEC_ENV`) and is fixed at *container-create* time.
    /// Reading it back is how the reconciler learns that a running
    /// container is enacting an older manifest than the one just applied.
    pub env: Vec<String>,
}

impl ContainerProbe {
    /// The value of one environment variable, if the container has it.
    pub fn env_var(&self, key: &str) -> Option<&str> {
        let prefix = format!("{key}=");
        self.env
            .iter()
            .find_map(|e| e.strip_prefix(prefix.as_str()))
    }
}

/// Correct libpod's create payload after `podman-api` has built it.
///
/// Pure, and separated out so both corrections can be asserted without a
/// podman socket. Each exists because `podman-api` gets a field wrong in a
/// way that is *silent* — libpod ignores what it does not recognise — so
/// nothing fails, the setting simply never applies.
fn patch_create_payload(
    mut json: serde_json::Value,
    no_new_privileges: bool,
    log_driver: Option<&str>,
) -> Result<serde_json::Value> {
    let obj = json
        .as_object_mut()
        .ok_or_else(|| Error::Container("create payload is not an object".into()))?;

    if no_new_privileges {
        // `podman-api`'s builder spells it `no_new_privilages` (a typo in
        // both 0.10 and 0.11). Drop the misspelling so libpod is not left
        // with a stray unknown field, then set the key it reads.
        obj.remove("no_new_privilages");
        obj.insert(
            "no_new_privileges".to_string(),
            serde_json::Value::Bool(true),
        );
    }

    // The log driver is *not* pinned here. Container output goes wherever
    // the host sends it — journald on Ubuntu 26.04 — because that is what
    // gives an operator log rotation for free and lets whatever o11y agent
    // already runs on the machine collect PostgreSQL's output alongside
    // everything else. Overriding that would be a regression against the
    // whole point of running PostgreSQL on a VM rather than in Kubernetes.
    //
    // Reading logs back is a *permissions* problem, not a driver problem:
    // rootless podman writes journald entries the service user must be in
    // `systemd-journal` to read. `ops/provision-node.sh` grants it. Without
    // that, `podman logs` returns an empty stream and no error — which is
    // how this was first mistaken for a broken driver.
    //
    // `ContainerSpec::log_driver` exists for the one case that genuinely
    // is not an operator log; see its documentation.
    if let Some(driver) = log_driver {
        obj.insert(
            "log_configuration".to_string(),
            serde_json::json!({ "driver": driver }),
        );
    }

    Ok(serde_json::Value::Object(obj.clone()))
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
        if let Some(h) = &spec.hostname {
            b = b.hostname(h);
        }
        if !spec.cap_drop.is_empty() {
            b = b.drop_capabilities(spec.cap_drop.clone());
        }
        if spec.no_new_privileges {
            // `podman-api` 0.10 misspells this builder as
            // `no_new_privilages`. It serializes to the correct wire
            // field, but a typo'd builder is exactly the kind of thing
            // that could silently stop applying — `hardening_flags_land`
            // in tests/hardening.rs asserts it via inspect.
            b = b.no_new_privilages(true);
        }
        if spec.read_only_fs {
            b = b.read_only_fs(true);
        }
        b = b.restart_policy(match spec.restart_policy {
            RestartPolicy::No => podman_api::opts::ContainerRestartPolicy::No,
            RestartPolicy::OnFailure => podman_api::opts::ContainerRestartPolicy::OnFailure,
            RestartPolicy::Always => podman_api::opts::ContainerRestartPolicy::Always,
        });

        if !spec.networks.is_empty() {
            let nets: HashMap<String, serde_json::Value> = spec
                .networks
                .iter()
                .map(|n| (n.clone(), serde_json::json!({})))
                .collect();
            b = b.networks(nets);
            // Rootless podman defaults to pasta/slirp4netns, which ignores
            // the `networks` map entirely — the container would come up
            // attached to nothing and standbys could not resolve their
            // primary. Forcing bridge mode is what makes joining a named
            // network actually work rootless.
            b = b.net_namespace(podman_api::models::Namespace {
                nsmode: Some("bridge".to_string()),
                value: None,
            });
        }

        if !spec.port_publishes.is_empty() {
            let ports: Vec<podman_api::models::PortMapping> = spec
                .port_publishes
                .iter()
                .map(|p| podman_api::models::PortMapping {
                    container_port: Some(p.container_port),
                    host_ip: p.host_ip.clone(),
                    host_port: Some(p.host_port),
                    protocol: Some("tcp".to_string()),
                    range: None,
                })
                .collect();
            b = b.portmappings(ports);
        }

        if !spec.secrets.is_empty() {
            let secrets: Vec<podman_api::models::Secret> = spec
                .secrets
                .iter()
                .map(|s| podman_api::models::Secret {
                    source: Some(s.name.clone()),
                    target: Some(s.target.clone()),
                    mode: Some(s.mode),
                    uid: Some(s.uid),
                    gid: Some(s.gid),
                })
                .collect();
            b = b.secrets(secrets);
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

        // podman-api builds the payload; we post it ourselves so the keys
        // its builder gets wrong actually land. See `http.rs` for why this
        // detour exists and why it must not grow.
        let opts = b.build();
        let payload = opts
            .serialize()
            .map_err(|e| Error::Container(format!("serialize create options: {e}")))?;
        let json: serde_json::Value = serde_json::from_str(&payload)
            .map_err(|e| Error::Container(format!("re-parse create options: {e}")))?;

        let json =
            patch_create_payload(json, spec.no_new_privileges, spec.log_driver.as_deref())?;

        let body = json.to_string();
        let (status, response) = crate::http::post_json(
            self.socket_path(),
            "/v4.0.0/libpod/containers/create",
            &body,
        )
        .await?;

        if !(200..300).contains(&status) {
            return Err(Error::Container(format!(
                "create returned HTTP {status}: {}",
                response.trim()
            )));
        }

        let created: serde_json::Value = serde_json::from_str(&response)
            .map_err(|e| Error::Container(format!("parse create response: {e}: {response}")))?;
        let id = created
            .get("Id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Container(format!("create response had no Id: {response}")))?
            .to_string();

        Ok(Container {
            id,
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

    /// The client that owns this container. `pub(crate)` so the podman
    /// handle stays inside this crate (ADR 00 §1).
    pub(crate) fn client_ref(&self) -> &PodmanClient {
        &self.client
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
        let env = data
            .config
            .as_ref()
            .and_then(|c| c.env.clone())
            .unwrap_or_default();
        Ok(Some(ContainerProbe {
            id: data.id.unwrap_or_else(|| self.id.clone()),
            name: data.name,
            running: state.as_ref().and_then(|s| s.running).unwrap_or(false),
            status: state.as_ref().and_then(|s| s.status.clone()),
            exit_code: state.as_ref().and_then(|s| s.exit_code),
            env,
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

    /// The payload `podman-api` hands us, near enough: it has the typo'd
    /// key and no log configuration at all.
    fn built_payload() -> serde_json::Value {
        serde_json::json!({
            "image": "postgres:18",
            "name": "pgpod-mydb-1",
            "no_new_privilages": true,
        })
    }

    #[test]
    fn the_misspelled_hardening_key_is_replaced_not_merely_added() {
        // Leaving `no_new_privilages` behind would mean libpod carries an
        // unknown field forever, and would hide the fact that the builder
        // is wrong from anyone reading the wire payload.
        let out = patch_create_payload(built_payload(), true, None).unwrap();
        assert_eq!(out.get("no_new_privileges"), Some(&serde_json::json!(true)));
        assert!(
            out.get("no_new_privilages").is_none(),
            "the typo'd key must be removed: {out}"
        );
    }

    #[test]
    fn hardening_is_not_asserted_when_it_was_not_asked_for() {
        let out = patch_create_payload(built_payload(), false, None).unwrap();
        assert!(out.get("no_new_privileges").is_none(), "{out}");
    }

    #[test]
    fn a_container_inherits_the_host_log_driver_by_default() {
        // PostgreSQL's output belongs wherever the host sends it —
        // journald on the deployment target, which is what gives an
        // operator rotation and lets an o11y agent collect it. pgpod must
        // not override that for its own convenience.
        let out = patch_create_payload(built_payload(), true, None).unwrap();
        assert!(
            out.get("log_configuration").is_none(),
            "pgpod must not pin a log driver on instance containers: {out}"
        );
    }

    #[test]
    fn a_job_container_can_opt_out_of_the_host_driver() {
        // Inheriting the host default silently breaks `pgpod logs`, the
        // JSON `pgbackrest info` prints from a job container, and the
        // container-logs detail on a readiness failure. Ubuntu 26.04's
        // podman 5.7 defaults to `journald`, where a rootless
        // `podman logs` returns an empty stream and no error; Arch's 6.1
        // defaults to `k8s-file`, which is why it went unnoticed until
        // pgpod ran on its own deployment target.
        // Only for output that is a return value rather than a log:
        // `pgbackrest info --output=json`, which the control plane parses
        // and journald's LineMax would truncate.
        for hardened in [true, false] {
            let out =
                patch_create_payload(built_payload(), hardened, Some("k8s-file")).unwrap();
            assert_eq!(
                out.pointer("/log_configuration/driver"),
                Some(&serde_json::json!("k8s-file")),
                "an explicit driver must survive regardless of hardening: {out}"
            );
        }
    }

    #[test]
    fn the_log_driver_uses_libpods_shape_not_dockers() {
        // `podman-api`'s `log_configuration` serializes the Docker-compat
        // `{"Config","Type"}` object; libpod's create endpoint reads
        // `{"driver": ...}` and ignores the other silently.
        let out = patch_create_payload(built_payload(), true, Some("k8s-file")).unwrap();
        let cfg = out.get("log_configuration").expect("log_configuration");
        assert!(cfg.get("driver").is_some(), "{cfg}");
        assert!(cfg.get("Type").is_none(), "Docker-compat shape: {cfg}");
        assert!(cfg.get("Config").is_none(), "Docker-compat shape: {cfg}");
    }

    #[test]
    fn nothing_else_in_the_payload_is_disturbed() {
        let out = patch_create_payload(built_payload(), true, None).unwrap();
        assert_eq!(out.get("image"), Some(&serde_json::json!("postgres:18")));
        assert_eq!(out.get("name"), Some(&serde_json::json!("pgpod-mydb-1")));
    }

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
