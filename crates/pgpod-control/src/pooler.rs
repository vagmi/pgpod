//! Poolers: applying them, deleting them, and holding them.
//!
//! The hold is the reason this exists. `PAUSE` at the pooler, change the
//! instance underneath, `RESUME` — measured on the deployment target as
//! the difference between 32 failed transactions out of 66 and zero out of
//! 22 across the same recreate (ADR 05).

use std::time::Duration;

use pgpod_core::{ClusterId, PoolTarget, PoolerId, PoolerManifest, PoolerSpec, Secret, container};
use pgpod_registry::{PoolerPool, PoolerRecord};
use pgpod_runtime::{ContainerSpec, ExecSpec, Mount, PortPublish, SecretMount};

use crate::{Error, Pgpod, Result, allocate_port, names, password};

/// What `apply -f pooler.yaml` did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PoolerReport {
    pub pooler: String,
    pub generation: i64,
    pub created: bool,
    pub container: String,
    pub host_port: u16,
    pub pools: Vec<PoolSummary>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PoolSummary {
    /// What clients put in `dbname`.
    pub name: String,
    pub cluster: String,
    pub database: String,
    pub connection_uri: String,
}

/// One cluster's contribution to a pooler, as the manifest and the
/// registry between them describe it.
#[derive(Debug, Clone)]
pub(crate) struct ClusterPools {
    pub cluster: String,
    /// `bootstrap.initdb.database`, which is what an omitted `pools:`
    /// means.
    pub default_database: Option<String>,
    pub declared: Vec<pgpod_core::PoolRef>,
}

/// One pool, after defaults are filled in and names are checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedPool {
    pub cluster: String,
    /// Position of the cluster in the manifest, which is also the index of
    /// its mounted lookup secret.
    pub cluster_index: usize,
    pub database: String,
    /// What a client puts in `dbname`.
    pub name: String,
}

/// Turn the manifest's clusters into pools, or say why it cannot.
///
/// Pure, and separate from `apply_pooler`, because the interesting case
/// needs no podman at all: **two clusters that both call their application
/// database `appdb`**. The manifest cannot catch that one — an omitted
/// `pools:` has no name in it to compare — so the check has to live where
/// the defaults are filled in, and that is here.
pub(crate) fn resolve_pools(
    inputs: &[ClusterPools],
) -> std::result::Result<Vec<ResolvedPool>, String> {
    let mut out: Vec<ResolvedPool> = Vec::new();
    // Pool name -> (cluster, whether the name came from a default).
    let mut claimed: std::collections::BTreeMap<String, (String, bool)> =
        std::collections::BTreeMap::new();

    for (cluster_index, input) in inputs.iter().enumerate() {
        let defaulted = input.declared.is_empty();
        let pools: Vec<(String, String)> = if defaulted {
            // A cluster without an application database has nothing to
            // pool, and saying so beats a pooler that starts and serves
            // nothing.
            let database = input.default_database.clone().ok_or_else(|| {
                format!(
                    "cluster {:?} has no bootstrap.initdb.database, so there is \
                     no default pool to create — name the databases explicitly \
                     under spec.clusters[].pools",
                    input.cluster
                )
            })?;
            vec![(database.clone(), database)]
        } else {
            input
                .declared
                .iter()
                .map(|p| (p.database.clone(), p.pool_name().to_string()))
                .collect()
        };

        for (database, name) in pools {
            if let Some((other, other_defaulted)) =
                claimed.insert(name.clone(), (input.cluster.clone(), defaulted))
            {
                return Err(collision(
                    &name,
                    &other,
                    &input.cluster,
                    defaulted && other_defaulted,
                ));
            }
            out.push(ResolvedPool {
                cluster: input.cluster.clone(),
                cluster_index,
                database,
                name,
            });
        }
    }
    Ok(out)
}

/// The message for two pools claiming one name.
///
/// Two shapes, because the fix differs. When both names were written out,
/// `as:` on either one is the answer. When both were *defaulted*, the
/// operator never typed the colliding name at all — telling them to "set
/// `as:` on one of them" sends them looking for a field that is not in
/// their file, so the message shows what to add.
fn collision(name: &str, first: &str, second: &str, both_defaulted: bool) -> String {
    let head = format!(
        "two pools would both be named {name:?} — one on cluster {first:?} and \
         one on {second:?}. A client picks a pool by putting that name in \
         `dbname`, so it has to be unique."
    );
    if !both_defaulted {
        return format!("{head} Set `as:` on one of them.");
    }
    format!(
        "{head}\n\nNeither was written down: a cluster with no `pools:` exports \
         its application database under the database's own name, and both of \
         these are called {name:?}. Name them explicitly:\n\n  clusters:\n    \
         - cluster: {first}\n      pools: [{{ database: {name}, as: \
         {first}-{name} }}]\n    - cluster: {second}\n      pools: [{{ database: \
         {name}, as: {second}-{name} }}]"
    )
}

impl Pgpod {
    /// Create or converge a pooler.
    pub async fn apply_pooler(
        &self,
        manifest: &PoolerManifest,
        wait: Duration,
    ) -> Result<PoolerReport> {
        let pooler = manifest.pooler_id()?;
        let agent = self.agent_binary()?;

        // Resolve every cluster first, so a manifest naming one that does
        // not exist fails before anything is created. Ordering matters:
        // the pools are indexed, and the index is how the agent finds each
        // cluster's mounted lookup secret.
        let mut inputs = Vec::new();
        for entry in &manifest.spec.clusters {
            let cluster = ClusterId::new(entry.cluster.clone())
                .map_err(|e| Error::Invalid(format!("spec.clusters: {e}")))?;
            let record = self.registry.cluster(&cluster)?.ok_or_else(|| {
                Error::Invalid(format!(
                    "pooler {pooler} fronts cluster {cluster}, which pgpod does \
                     not know about — apply the cluster first"
                ))
            })?;
            inputs.push(ClusterPools {
                cluster: entry.cluster.clone(),
                default_database: record.manifest.spec.bootstrap.initdb.database.clone(),
                declared: entry.pools.clone(),
            });
        }

        let resolved = resolve_pools(&inputs).map_err(Error::Invalid)?;

        let mut targets = Vec::new();
        let mut rows = Vec::new();
        for r in &resolved {
            let cluster = ClusterId::new(r.cluster.clone())
                .map_err(|e| Error::Invalid(format!("spec.clusters: {e}")))?;
            targets.push(PoolTarget {
                name: r.name.clone(),
                cluster: r.cluster.clone(),
                // A pooler reaches the primary, which at one instance is
                // ordinal 1. Phase 5's promote is what makes this a lookup
                // rather than a constant.
                server_host: cluster.instance(1).container_name(),
                server_port: container::PG_PORT,
                database: r.database.clone(),
                lookup_role: pgpod_pg::POOLER_ROLE.to_string(),
                lookup_database: "postgres".to_string(),
                lookup_secret_index: r.cluster_index,
            });
            rows.push(PoolerPool {
                cluster: r.cluster.clone(),
                database: r.database.clone(),
                pool_name: r.name.clone(),
            });
        }

        for input in &inputs {
            let cluster = ClusterId::new(input.cluster.clone())
                .map_err(|e| Error::Invalid(format!("spec.clusters: {e}")))?;
            // The lookup role, created on demand so a pooler works against
            // a cluster built before poolers existed. Idempotent, and the
            // same statements bootstrap runs.
            self.ensure_pooler_role(&cluster).await?;
        }

        let existing = self.registry.pooler(&pooler)?;
        let host_port = match (&existing, manifest.spec.port) {
            // An explicit port wins; otherwise reuse the one a previous
            // apply chose, so applications already pointed at it keep
            // working across a recreate.
            (_, Some(port)) => port,
            (Some(record), None) => record.host_port,
            (None, None) => allocate_port()?,
        };

        let spec = PoolerSpec {
            pooler: pooler.clone(),
            port: container::POOLER_PORT,
            pools: targets,
            pool_mode: manifest.spec.pg_doorman.pool_mode,
            pool_size: manifest.spec.pg_doorman.pool_size,
            max_hold: manifest.spec.pg_doorman.max_hold,
            parameters: manifest
                .spec
                .pg_doorman
                .parameters
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        };
        self.validate_pooler_parameters(&spec)?;
        spec.validate()?;

        // Only the agent inside the container ever reads this, from the
        // mount, so it is created and never read back here.
        let admin_secret = names::pooler_admin_secret(&pooler);
        if !self.podman.secret_exists(&admin_secret).await? {
            let generated: Secret = password::generate();
            self.podman
                .put_secret(&admin_secret, generated.expose())
                .await?;
        }

        // **Divergence is checked before the registry is told anything.**
        // `put_pooler` writes the new manifest and replaces the pool rows,
        // so refusing afterwards leaves the registry describing a pooler
        // that was never created: `pgpod pooler list` shows pools the
        // running container does not serve, and the phase sticks at
        // "applying". Found by reading `pooler list` after a refused
        // apply and seeing the wrong pools.
        let handle = self.podman.container(pooler.container_name());
        let probe = handle.probe().await?;
        if let Some(p) = &probe {
            Self::refuse_if_pooler_diverged(&pooler, &spec, p)?;
        }

        let generation = self.registry.put_pooler(
            manifest,
            &pooler.container_name(),
            None,
            host_port,
            "applying",
            &rows,
        )?;

        let container_spec =
            self.pooler_container_spec(manifest, &spec, &agent, host_port, &admin_secret)?;

        let container_id = match probe {
            Some(p) if p.running => p.id,
            Some(p) => {
                self.podman.container(&p.id).start().await?;
                p.id
            }
            None => {
                self.podman
                    .pull_image_if_absent(&manifest.spec.image_name)
                    .await?;
                let c = self.podman.create_container(&container_spec).await?;
                c.start().await?;
                c.id().to_string()
            }
        };

        self.registry.put_pooler(
            manifest,
            &pooler.container_name(),
            Some(&container_id),
            host_port,
            "running",
            &rows,
        )?;

        if !wait.is_zero() {
            self.wait_pooler_ready(&pooler, wait).await?;
        }

        for cluster in spec.clusters() {
            self.registry.record_event(
                cluster,
                None,
                "info",
                &format!("pooler {pooler} applied"),
            )?;
        }

        Ok(PoolerReport {
            pooler: pooler.to_string(),
            generation,
            created: existing.is_none(),
            container: pooler.container_name(),
            host_port,
            pools: spec
                .pools
                .iter()
                .map(|p| PoolSummary {
                    name: p.name.clone(),
                    cluster: p.cluster.clone(),
                    database: p.database.clone(),
                    connection_uri: format!("postgresql://127.0.0.1:{host_port}/{}", p.name),
                })
                .collect(),
        })
    }

    /// Reject parameters pgpod manages, before anything is created.
    ///
    /// The renderer would catch them too, but only once the agent is
    /// already running inside a container — leaving a failed pooler behind
    /// for a mistake that is visible in the manifest.
    fn validate_pooler_parameters(&self, spec: &PoolerSpec) -> Result<()> {
        for (key, _) in &spec.parameters {
            let normalized = key.trim().to_ascii_lowercase();
            if pgpod_pooler::RESERVED_PARAMETERS.contains(&normalized.as_str()) {
                return Err(Error::Invalid(format!(
                    "spec.pgDoorman.parameters: {normalized:?} is managed by \
                     pgpod and cannot be set here"
                )));
            }
        }
        Ok(())
    }

    /// Create the lookup role and its function on one cluster.
    ///
    /// Runs through `psql` in the instance container, the way `status`
    /// already does. The instance has to be up: a pooler applied against a
    /// stopped cluster would otherwise come up unable to authenticate
    /// anyone, and the reason would be three layers away.
    ///
    /// **The SQL runs before the secret is stored**, which is what makes
    /// the two halves safe to interrupt. pgpod can create a podman secret
    /// but deliberately cannot read one back, so "secret exists" has to
    /// mean "the role in the database already has this password". Storing
    /// it first would break that the moment the SQL failed: the next apply
    /// would see the secret, skip the SQL, and mount a credential no role
    /// has. This order fails the other way — a secret that never got
    /// written just means the next apply generates a fresh password and
    /// rotates the role, which `ALTER ROLE` does anyway.
    async fn ensure_pooler_role(&self, cluster: &ClusterId) -> Result<()> {
        let secret = names::pooler_lookup_secret(cluster);
        if self.podman.secret_exists(&secret).await? {
            return Ok(());
        }

        let password: Secret = password::generate();
        let instance = cluster.instance(1);
        let container = self.running_container(&instance).await.map_err(|e| {
            Error::Invalid(format!(
                "cluster {cluster} must be running to apply a pooler to it — the \
                 lookup role is created inside the instance: {e}"
            ))
        })?;

        for statement in pgpod_pg::pooler_lookup_sql(&password)? {
            let exec = ExecSpec::new([
                "psql",
                "-X",
                "-q",
                "-v",
                "ON_ERROR_STOP=1",
                "-h",
                container::SOCKET_DIR,
                "-U",
                "postgres",
                "-d",
                "postgres",
                "-c",
                &statement,
            ]);
            let out = container.exec(&exec).await?;
            if !out.success() {
                // The statement carries the generated password, so the
                // error reports only what postgres said — never the
                // command that produced it.
                return Err(Error::Invalid(format!(
                    "could not create the pooler lookup role on {cluster}: {}",
                    out.stderr.trim()
                )));
            }
        }

        self.podman.put_secret(&secret, password.expose()).await?;
        Ok(())
    }

    fn pooler_container_spec(
        &self,
        manifest: &PoolerManifest,
        spec: &PoolerSpec,
        agent: &std::path::Path,
        host_port: u16,
        admin_secret: &str,
    ) -> Result<ContainerSpec> {
        let pooler = spec.pooler.clone();
        let uid = manifest.spec.pooler_uid;
        let gid = manifest.spec.pooler_gid;

        let mut c = ContainerSpec::hardened(&manifest.spec.image_name)
            .name(pooler.container_name())
            .hostname(pooler.container_name())
            // The image declares no USER; container-root could not read a
            // 0400 secret once cap_drop: ALL removed CAP_DAC_OVERRIDE.
            .user(format!("{uid}:{gid}"))
            .entrypoint([container::AGENT_BIN, "pooler", "run"])
            .env(pgpod_core::POOLER_SPEC_ENV, spec.to_env_value()?)
            .publish(PortPublish::loopback(host_port, container::POOLER_PORT))
            // Writable, because the config is rendered in-container: a
            // host bind mount is unreadable to the container's uid under
            // rootless podman, and a secret cannot be rewritten for a
            // RELOAD (ADR 05 §5).
            .mount(Mount::Tmpfs {
                target: container::POOLER_DIR.to_string(),
            })
            .mount(Mount::BindReadOnly {
                source: agent.display().to_string(),
                target: container::AGENT_BIN.to_string(),
            })
            .secret(SecretMount {
                name: admin_secret.to_string(),
                target: container::SECRET_POOLER_ADMIN.to_string(),
                mode: 0o400,
                uid,
                gid,
            });

        // One network per fronted cluster. ADR 00 §10 gives each cluster
        // its own bridge, and a container may join several — which is what
        // makes co-tenancy cost one entry here and nothing else.
        for cluster in spec.clusters() {
            let id = ClusterId::new(cluster.to_string())
                .map_err(|e| Error::Invalid(format!("stored cluster name: {e}")))?;
            c = c.network(id.network_name());
        }

        // One lookup credential per cluster, addressed by the index the
        // spec carries, so nothing has to agree about names across the
        // container boundary.
        let mut mounted = std::collections::BTreeSet::new();
        for target in &spec.pools {
            if !mounted.insert(target.lookup_secret_index) {
                continue;
            }
            let id = ClusterId::new(target.cluster.clone())
                .map_err(|e| Error::Invalid(format!("stored cluster name: {e}")))?;
            c = c.secret(SecretMount {
                name: names::pooler_lookup_secret(&id),
                target: container::pooler_lookup_secret(target.lookup_secret_index),
                mode: 0o400,
                uid,
                gid,
            });
        }

        for (k, v) in names::pooler_labels(&pooler, &spec.clusters()) {
            c = c.label(k, v);
        }
        Ok(c)
    }

    /// Refuse to adopt a pooler running an older spec.
    ///
    /// Same reasoning as `refuse_if_diverged` for instances: the spec
    /// travels in an environment variable fixed at container-create time,
    /// so a changed manifest needs the container recreated. For a pooler
    /// that is an outage for pooled clients — the one thing the pooler
    /// cannot hold for — which is exactly why it must not happen silently.
    fn refuse_if_pooler_diverged(
        pooler: &PoolerId,
        desired: &PoolerSpec,
        probe: &pgpod_runtime::ContainerProbe,
    ) -> Result<()> {
        let Some(running) = probe.env_var(pgpod_core::POOLER_SPEC_ENV) else {
            return Err(Error::PoolerDiverged {
                pooler: pooler.to_string(),
                detail: format!(
                    "the running container has no {}",
                    pgpod_core::POOLER_SPEC_ENV
                ),
            });
        };
        let running: PoolerSpec = serde_json::from_str(running)
            .map_err(|e| Error::Spec(pgpod_core::SpecError::Malformed(e.to_string())))?;
        if &running == desired {
            return Ok(());
        }
        Err(Error::PoolerDiverged {
            pooler: pooler.to_string(),
            detail: pooler_differences(&running, desired).join("\n"),
        })
    }

    pub(crate) async fn wait_pooler_ready(
        &self,
        pooler: &PoolerId,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        let container = self.podman.container(pooler.container_name());
        let mut last = String::new();

        while std::time::Instant::now() < deadline {
            // Asking the pooler itself, through the agent's control
            // socket: it answers only once pg_doorman is accepting
            // connections *and* the admin console authenticates, which is
            // the whole of what "ready" means here.
            match self.pooler_control(pooler, "pools", &[]).await {
                Ok(_) => return Ok(()),
                Err(e) => last = e.to_string(),
            }
            if let Some(probe) = container.probe().await?
                && !probe.running
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let logs = container
            .logs_string()
            .await
            .unwrap_or_else(|e| format!("<logs unavailable: {e}>"));
        Err(Error::NotReady {
            instance: pooler.to_string(),
            seconds: timeout.as_secs(),
            detail: format!("last probe: {last}\n\ncontainer logs:\n{logs}"),
        })
    }

    /// Run one control verb inside a pooler container.
    ///
    /// `podman exec` rather than a socket the daemon opens directly: the
    /// control socket lives in the container's tmpfs, and the exec'd agent
    /// is already the thing that knows how to speak to it.
    pub(crate) async fn pooler_control(
        &self,
        pooler: &PoolerId,
        verb: &str,
        args: &[&str],
    ) -> Result<String> {
        let mut argv = vec![container::AGENT_BIN, "pooler", verb];
        argv.extend_from_slice(args);
        let out = self
            .podman
            .container(pooler.container_name())
            .exec(&ExecSpec::new(argv))
            .await?;
        if !out.success() {
            return Err(Error::Invalid(format!(
                "pooler {pooler}: {} {}",
                out.stdout.trim(),
                out.stderr.trim()
            )));
        }
        Ok(out.stdout)
    }

    /// Remove a pooler's container and its registry rows.
    ///
    /// There is no volume and nothing durable — the config is re-rendered
    /// on every start — so this is the one pgpod delete that destroys
    /// nothing an operator could want back.
    pub async fn delete_pooler(&self, pooler: &PoolerId) -> Result<()> {
        let record = self.registry.require_pooler(pooler)?;
        let handle = self.podman.container(&record.container_name);
        if handle.probe().await?.is_some() {
            let _ = handle.stop(Duration::from_secs(30)).await;
            handle.remove(true).await?;
        }
        for pool in &record.pools {
            self.registry.record_event(
                &pool.cluster,
                None,
                "info",
                &format!("pooler {pooler} deleted"),
            )?;
        }
        self.registry.delete_pooler(pooler)?;
        Ok(())
    }

    pub async fn poolers(&self) -> Result<Vec<PoolerRecord>> {
        Ok(self.registry.list_poolers()?)
    }

    /// `SHOW POOLS`, as the pooler reports it right now.
    ///
    /// Typed rather than raw JSON: both ends share the protocol types, so
    /// a field rename is a compile error here instead of a silently
    /// missing column three layers up.
    pub async fn pooler_pools(&self, pooler: &PoolerId) -> Result<Vec<pgpod_pooler::PoolStatus>> {
        let raw = self.pooler_control(pooler, "pools", &[]).await?;
        let response: pgpod_pooler::Response = serde_json::from_str(raw.trim())
            .map_err(|e| Error::Invalid(format!("the pooler agent sent unreadable JSON: {e}")))?;
        response
            .into_pools()
            .map_err(|e| Error::Invalid(e.to_string()))
    }

    pub async fn pooler_pause(&self, pooler: &PoolerId, cluster: &ClusterId) -> Result<()> {
        self.pooler_control(pooler, "pause", &[cluster.as_str()])
            .await
            .map(|_| ())
    }

    pub async fn pooler_resume(&self, pooler: &PoolerId, cluster: &ClusterId) -> Result<()> {
        self.pooler_control(pooler, "resume", &[cluster.as_str()])
            .await
            .map(|_| ())
    }
}

impl Pgpod {
    /// Replace an instance's container with one running the current spec,
    /// holding pooled clients across the window.
    ///
    /// The steps are journalled to `events` **before** they run, the way
    /// ADR 02 §4 requires of promote: a crash halfway through should leave
    /// a state an operator can read, not one they have to infer.
    ///
    /// Note what this does *not* do. The container keeps its name, and
    /// pg_doorman resolves `server_host` per backend connect, so nothing
    /// about the pooler's configuration changes — the new container is
    /// picked up at its new address by the `RECONNECT` on the way out.
    /// Only a promote to a different ordinal (Phase 5) would need more.
    pub(crate) async fn recreate_instance(
        &self,
        manifest: &pgpod_core::ClusterManifest,
        instance: &pgpod_core::InstanceId,
        ctx: &crate::ApplyContext<'_>,
        host_port: u16,
    ) -> Result<String> {
        let cluster = instance.cluster().clone();
        let started = std::time::Instant::now();

        let mut hold = PoolerHold::acquire(self, &cluster).await?;
        if hold.is_empty() {
            // Said plainly rather than discovered afterwards: with nothing
            // in front, this is an ordinary restart and every open
            // connection goes with it.
            self.registry.record_event(
                cluster.as_str(),
                Some(instance.ordinal()),
                "warn",
                "recreating with no pooler in front — open connections will drop",
            )?;
        }

        let held_count = hold.len();
        let outcome = self
            .recreate_held(manifest, instance, ctx, host_port, &cluster)
            .await;

        match outcome {
            Ok(id) => {
                hold.release_after_recreate().await?;
                self.registry.record_event(
                    cluster.as_str(),
                    Some(instance.ordinal()),
                    "info",
                    &format!(
                        // Milliseconds, because the whole point is that
                        // this window is short, and "0s" tells an operator
                        // nothing about whether it fit inside the budget.
                        "recreated in {}ms; {} pooler(s) held clients throughout",
                        started.elapsed().as_millis(),
                        held_count
                    ),
                )?;
                Ok(id)
            }
            Err(e) => {
                // Release before returning, so a failed recreate does not
                // also leave clients held for the rest of the budget. The
                // in-container deadline would catch it; this makes it
                // immediate.
                hold.release().await;
                self.registry.record_event(
                    cluster.as_str(),
                    Some(instance.ordinal()),
                    "error",
                    &format!("recreate failed: {e}"),
                )?;
                Err(e)
            }
        }
    }

    /// The part that happens while clients are held.
    async fn recreate_held(
        &self,
        manifest: &pgpod_core::ClusterManifest,
        instance: &pgpod_core::InstanceId,
        ctx: &crate::ApplyContext<'_>,
        host_port: u16,
        cluster: &ClusterId,
    ) -> Result<String> {
        let handle = self.podman.container(instance.container_name());

        self.registry.record_event(
            cluster.as_str(),
            Some(instance.ordinal()),
            "info",
            "removing the container to apply a changed spec",
        )?;
        let _ = handle.stop(Duration::from_secs(30)).await;
        handle.remove(true).await?;

        self.registry.record_event(
            cluster.as_str(),
            Some(instance.ordinal()),
            "info",
            "creating the replacement container",
        )?;
        let spec = self.container_spec(manifest, instance, ctx, host_port)?;
        let container = self.podman.create_container(&spec).await?;
        container.start().await?;
        let id = container.id().to_string();

        // The volume is untouched, so this is a restart rather than a
        // bootstrap: seconds, not minutes. It still has to be waited for —
        // releasing the hold before postgres is accepting connections
        // would hand every held client a refused connection.
        self.wait_ready(instance, Duration::from_secs(120)).await?;
        Ok(id)
    }
}

/// What changed between the running pooler spec and the desired one.
///
/// Named fields rather than a JSON diff, for the same reason
/// `differences` exists for instances: an operator has to be able to read
/// this and recognise the edit they made.
fn pooler_differences(running: &PoolerSpec, desired: &PoolerSpec) -> Vec<String> {
    let mut out = Vec::new();
    if running.pool_mode != desired.pool_mode {
        out.push(format!(
            "  poolMode: {} -> {}",
            running.pool_mode.as_str(),
            desired.pool_mode.as_str()
        ));
    }
    if running.pool_size != desired.pool_size {
        out.push(format!(
            "  poolSize: {} -> {}",
            running.pool_size, desired.pool_size
        ));
    }
    if running.max_hold != desired.max_hold {
        out.push(format!(
            "  maxHold: {} -> {}",
            running.max_hold.render(),
            desired.max_hold.render()
        ));
    }
    if running.parameters != desired.parameters {
        out.push(format!(
            "  parameters: {:?} -> {:?}",
            running.parameters, desired.parameters
        ));
    }

    let names = |s: &PoolerSpec| -> Vec<String> {
        let mut v: Vec<String> = s
            .pools
            .iter()
            .map(|p| format!("{} -> {}/{}", p.name, p.cluster, p.database))
            .collect();
        v.sort();
        v
    };
    let (before, after) = (names(running), names(desired));
    for gone in before.iter().filter(|p| !after.contains(p)) {
        out.push(format!("  pool removed: {gone}"));
    }
    for added in after.iter().filter(|p| !before.contains(p)) {
        out.push(format!("  pool added:   {added}"));
    }

    if out.is_empty() {
        // Something outside the fields named above; say so rather than
        // print nothing and look like a spurious refusal.
        out.push("  the specs differ in a field this message does not break out".into());
    }
    out
}

/// A hold over every pooler fronting one cluster.
///
/// Scoped to that cluster's pools, so recreating one cluster never stalls
/// a co-tenant's clients on a shared pooler (ADR 05 §2).
///
/// The release is best-effort and deliberately *not* the guarantee: the
/// real backstop is the deadline PID 1 arms inside each pooler container,
/// which fires whether or not this process is still alive. This is the
/// tidy path, not the safe one.
pub struct PoolerHold<'a> {
    pgpod: &'a Pgpod,
    cluster: ClusterId,
    held: Vec<PoolerId>,
}

impl<'a> PoolerHold<'a> {
    /// Pause every pooler fronting `cluster`.
    ///
    /// A pooler that cannot be held fails the whole hold, and the ones
    /// already paused are released on the way out: half a hold is worse
    /// than none, because it looks like the switchover is covered.
    pub async fn acquire(pgpod: &'a Pgpod, cluster: &ClusterId) -> Result<Self> {
        let poolers = pgpod.registry.poolers_for_cluster(cluster)?;
        let mut hold = Self {
            pgpod,
            cluster: cluster.clone(),
            held: Vec::new(),
        };

        for record in poolers {
            let id = PoolerId::new(record.name.clone())
                .map_err(|e| Error::Invalid(format!("stored pooler name: {e}")))?;
            match pgpod.pooler_pause(&id, cluster).await {
                Ok(()) => hold.held.push(id),
                Err(e) => {
                    hold.release().await;
                    return Err(Error::Invalid(format!(
                        "could not hold pooler {} for {cluster}, so the \
                         switchover would drop connections it promised to \
                         keep: {e}",
                        record.name
                    )));
                }
            }
        }

        pgpod.registry.record_event(
            cluster.as_str(),
            None,
            "info",
            &format!("held {} pooler(s)", hold.held.len()),
        )?;
        Ok(hold)
    }

    /// How many poolers are holding. Zero means nothing is in front of
    /// this cluster and a recreate will drop connections.
    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Recycle backends, then release.
    ///
    /// `RECONNECT` first: the instance the pools pointed at has been
    /// replaced, and its old backend connections are stale. `server_host`
    /// is re-resolved per connect, so the new container is picked up at
    /// its new address with no config change — but only for connections
    /// made after this.
    pub async fn release_after_recreate(mut self) -> Result<()> {
        for id in std::mem::take(&mut self.held) {
            self.pgpod
                .pooler_control(&id, "reconnect", &[self.cluster.as_str()])
                .await?;
            self.pgpod.pooler_resume(&id, &self.cluster).await?;
        }
        Ok(())
    }

    /// Best-effort release, for the failure paths.
    pub async fn release(&mut self) {
        for id in std::mem::take(&mut self.held) {
            let _ = self.pgpod.pooler_resume(&id, &self.cluster).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgpod_core::PoolRef;

    fn defaulted(cluster: &str, database: &str) -> ClusterPools {
        ClusterPools {
            cluster: cluster.into(),
            default_database: Some(database.into()),
            declared: Vec::new(),
        }
    }

    fn declared(cluster: &str, pools: &[(&str, Option<&str>)]) -> ClusterPools {
        ClusterPools {
            cluster: cluster.into(),
            default_database: Some("appdb".into()),
            declared: pools
                .iter()
                .map(|(db, alias)| PoolRef {
                    database: (*db).into(),
                    as_name: alias.map(str::to_string),
                })
                .collect(),
        }
    }

    #[test]
    fn an_omitted_pool_list_means_the_application_database() {
        let got = resolve_pools(&[defaulted("mydb", "appdb")]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].database, "appdb");
        assert_eq!(got[0].name, "appdb", "the pool is named after the database");
        assert_eq!(got[0].cluster_index, 0);
    }

    #[test]
    fn the_cluster_index_is_the_lookup_secret_index() {
        // Every pool of one cluster shares that cluster's mounted lookup
        // credential, and pools of different clusters must not. Getting
        // this wrong would authenticate one cluster's clients against
        // another's verifiers.
        let got = resolve_pools(&[
            declared("alpha", &[("appdb", Some("a1")), ("reporting", Some("a2"))]),
            declared("beta", &[("appdb", Some("b1"))]),
        ])
        .unwrap();
        let index = |name: &str| got.iter().find(|p| p.name == name).unwrap().cluster_index;
        assert_eq!(index("a1"), 0);
        assert_eq!(index("a2"), 0, "two pools on one cluster share its secret");
        assert_eq!(index("b1"), 1, "a second cluster gets its own");
    }

    #[test]
    fn two_clusters_with_the_same_database_name_are_refused() {
        // The whole reason this function is pure: it is the likeliest
        // mistake, and the manifest cannot see it because neither name was
        // written down.
        let err = resolve_pools(&[
            defaulted("cluster1", "appdb"),
            defaulted("cluster2", "appdb"),
        ])
        .unwrap_err();
        assert!(
            err.contains("cluster1") && err.contains("cluster2"),
            "{err}"
        );
        assert!(err.contains("appdb"), "{err}");
    }

    #[test]
    fn the_defaulted_collision_shows_the_yaml_to_write() {
        // "Set `as:` on one of them" would send an operator looking for a
        // field their file does not contain — they never named the pools.
        let err = resolve_pools(&[
            defaulted("cluster1", "appdb"),
            defaulted("cluster2", "appdb"),
        ])
        .unwrap_err();
        assert!(
            err.contains("as: cluster1-appdb") && err.contains("as: cluster2-appdb"),
            "the message must show the fix, not just name the problem: {err}"
        );
        assert!(
            !err.contains("Set `as:` on one of them"),
            "that advice is for the case where the names were written out: {err}"
        );
    }

    #[test]
    fn the_declared_collision_says_to_use_as() {
        // Here the operator *did* write the names, so naming the field is
        // enough and printing a whole YAML block would be noise.
        let err = resolve_pools(&[
            declared("cluster1", &[("appdb", None)]),
            declared("cluster2", &[("appdb", None)]),
        ])
        .unwrap_err();
        assert!(err.contains("Set `as:` on one of them"), "{err}");
        assert!(
            !err.contains("clusters:"),
            "no YAML block needed here: {err}"
        );
    }

    #[test]
    fn an_alias_resolves_the_collision() {
        let got = resolve_pools(&[
            declared("cluster1", &[("appdb", Some("c1"))]),
            declared("cluster2", &[("appdb", Some("c2"))]),
        ])
        .unwrap();
        let names: Vec<&str> = got.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["c1", "c2"]);
        // Both still point at the real database, which is what
        // `server_database` will carry.
        assert!(got.iter().all(|p| p.database == "appdb"));
    }

    #[test]
    fn one_cluster_repeating_a_pool_name_is_refused_too() {
        let err = resolve_pools(&[declared(
            "mydb",
            &[("appdb", Some("same")), ("analytics", Some("same"))],
        )])
        .unwrap_err();
        assert!(err.contains("same"), "{err}");
    }

    #[test]
    fn a_cluster_with_no_application_database_says_so() {
        let err = resolve_pools(&[ClusterPools {
            cluster: "mydb".into(),
            default_database: None,
            declared: Vec::new(),
        }])
        .unwrap_err();
        assert!(err.contains("bootstrap.initdb.database"), "{err}");
        assert!(err.contains("spec.clusters[].pools"), "the fix: {err}");
    }

    #[test]
    fn such_a_cluster_is_fine_once_its_pools_are_named() {
        let got = resolve_pools(&[ClusterPools {
            cluster: "mydb".into(),
            default_database: None,
            declared: vec![PoolRef {
                database: "things".into(),
                as_name: None,
            }],
        }])
        .unwrap();
        assert_eq!(got[0].name, "things");
    }
}
