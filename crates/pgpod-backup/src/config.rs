//! Rendering `pgbackrest.conf`.
//!
//! Pure: no I/O, no process spawning. Everything that decides *what*
//! pgBackRest is told lives here, so the agent (which archives and
//! restores) and the daemon (which backs up and queries) cannot render two
//! different configurations for the same cluster — the failure that would
//! look like an empty repository rather than an error.
//!
//! The file is written into the volume, not to pgBackRest's default
//! `/etc/pgbackrest/pgbackrest.conf`, because the instance container has a
//! read-only rootfs (ADR 04 §3). Every path pgBackRest needs to write —
//! log, lock, spool — is under the volume for the same reason.

use std::fmt::Write as _;

use pgpod_core::{Destination, RetentionMode, container};

use crate::Error;

/// pgBackRest indexes repositories `repo1`..`repo4`.
pub const MAX_REPOSITORIES: usize = 4;

/// Where the agent writes the rendered config, inside the volume.
pub use pgpod_core::container::PGBACKREST_CONF as CONFIG_FILE;

/// Which object store a destination names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoKind {
    /// A filesystem path — a podman volume, in practice (ADR 04 §3).
    Posix,
    S3 {
        bucket: String,
        /// Host only, never a URL: pgBackRest takes the scheme's place
        /// with `repo-storage-port` and always speaks TLS.
        endpoint: Option<String>,
        region: Option<String>,
    },
    Gcs {
        bucket: String,
    },
    Azure {
        container: String,
        account: Option<String>,
    },
}

/// One repository, as pgBackRest will be told about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub kind: RepoKind,
    /// `repo-path`: the prefix inside the bucket, or the filesystem path.
    ///
    /// Always absolute, and always ends up with the stanza appended by
    /// pgBackRest itself, so several clusters can share one bucket.
    pub path: String,
    /// Port, when the destination named one.
    pub port: Option<u16>,
    /// Whether to verify the storage server's TLS certificate.
    ///
    /// `false` only for a self-signed test fixture. pgBackRest has no
    /// plaintext-HTTP option at all — it always speaks TLS to object
    /// storage — so this is as far as an insecure local setup can go.
    pub verify_tls: bool,
    /// Extra `repoN-*` options, from the mounted credentials secret and
    /// from `spec.backup.destinations[].options`. Rendered verbatim after
    /// the derived ones, so an explicit value wins.
    pub extra: Vec<(String, String)>,
}

impl Repository {
    /// Translate a manifest destination into a pgBackRest repository.
    ///
    /// The URL forms are the ones `pgpod_core::Destination` documents;
    /// this is where they stop being pgpod's vocabulary and become
    /// pgBackRest's.
    pub fn from_destination(dest: &Destination) -> Result<Self, Error> {
        let url = dest.url.trim();
        let bad = |detail: &str| Error::Destination {
            url: url.to_string(),
            detail: detail.to_string(),
        };

        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| bad("expected something like gs://bucket/prefix"))?;

        // For a bucket URL the first path component is the bucket and the
        // remainder is the prefix; for file:// the whole thing is a path.
        let (head, tail) = match rest.split_once('/') {
            Some((h, t)) => (h, t),
            None => (rest, ""),
        };

        // pgBackRest wants an absolute repo-path. A destination naming
        // only a bucket gets `/`, and pgBackRest appends the stanza — so
        // sharing a bucket between clusters is safe by construction, the
        // same property ADR 01 §2 wanted.
        let path = format!("/{}", tail.trim_matches('/'));

        let kind = match scheme {
            "file" => {
                if head.is_empty() && tail.is_empty() {
                    return Err(bad("names no path"));
                }
                // `file:///archive/demo` parses as host="" tail="archive/demo".
                if !head.is_empty() {
                    return Err(bad(
                        "a file:// URL must have an empty host — write file:///path",
                    ));
                }
                return Ok(Self {
                    kind: RepoKind::Posix,
                    path: format!("/{}", tail.trim_matches('/')),
                    port: None,
                    verify_tls: true,
                    extra: dest
                        .options
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                });
            }
            "s3" | "s3a" => {
                if head.is_empty() {
                    return Err(bad("names no bucket"));
                }
                let (endpoint, port) = split_endpoint(dest.endpoint.as_deref())?;
                RepoKind::S3 {
                    bucket: head.to_string(),
                    endpoint,
                    region: dest.region.clone(),
                }
                .with_port(port)
            }
            "gs" => {
                if head.is_empty() {
                    return Err(bad("names no bucket"));
                }
                let (_, port) = split_endpoint(dest.endpoint.as_deref())?;
                RepoKind::Gcs {
                    bucket: head.to_string(),
                }
                .with_port(port)
            }
            "az" | "abfs" | "abfss" => {
                if head.is_empty() {
                    return Err(bad("names no container"));
                }
                let (_, port) = split_endpoint(dest.endpoint.as_deref())?;
                RepoKind::Azure {
                    container: head.to_string(),
                    account: dest.region.clone(),
                }
                .with_port(port)
            }
            other => {
                return Err(bad(&format!(
                    "unsupported scheme {other:?} — pgBackRest handles \
                     file, s3, gs, az"
                )));
            }
        };

        let (kind, port) = kind;
        Ok(Self {
            kind,
            path,
            port,
            // A destination that asked for plaintext cannot have it, so
            // the nearest thing is to stop verifying the certificate.
            // Never inferred: see `Destination::verify_tls`.
            verify_tls: dest.verify_tls,
            extra: dest
                .options
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        })
    }

    /// The `repoN-*` lines for this repository, at index `n` (1-based).
    fn render(&self, n: usize, out: &mut String) {
        let p = format!("repo{n}");
        let mut set = |k: &str, v: &str| {
            let _ = writeln!(out, "{p}-{k}={v}");
        };

        match &self.kind {
            RepoKind::Posix => set("type", "posix"),
            RepoKind::S3 {
                bucket,
                endpoint,
                region,
            } => {
                set("type", "s3");
                set("s3-bucket", bucket);
                if let Some(e) = endpoint {
                    set("s3-endpoint", e);
                }
                if let Some(r) = region {
                    set("s3-region", r);
                }
                // Path style, because every S3-compatible store supports
                // it and virtual-host style needs DNS per bucket. AWS
                // accepts both.
                set("s3-uri-style", "path");
            }
            RepoKind::Gcs { bucket } => {
                set("type", "gcs");
                set("gcs-bucket", bucket);
                // `auto` authorizes with the instance service account, so
                // a GCE VM needs no credential mounted at all — the same
                // conclusion ADR 01 reached for object_store, reached
                // again here. An explicit key in the credentials secret
                // overrides this, because `extra` is rendered last.
                set("gcs-key-type", "auto");
            }
            RepoKind::Azure { container, account } => {
                set("type", "azure");
                set("azure-container", container);
                if let Some(a) = account {
                    set("azure-account", a);
                }
            }
        }

        set("path", &self.path);
        if let Some(port) = self.port {
            set("storage-port", &port.to_string());
        }
        if !self.verify_tls {
            set("storage-verify-tls", "n");
        }

        // Point TLS at the bundle's own trust store rather than at
        // whatever the image happens to have. A posix repository needs
        // none of this.
        if !matches!(self.kind, RepoKind::Posix) {
            set("storage-ca-file", container::PGBACKREST_CA_FILE);
        }

        // Last, so an operator's explicit option corrects anything above
        // rather than being silently overridden by it.
        for (k, v) in &self.extra {
            set(k, v);
        }
    }
}

impl RepoKind {
    fn with_port(self, port: Option<u16>) -> (Self, Option<u16>) {
        (self, port)
    }
}

/// Split `host:port` — pgBackRest's endpoint is a bare host, and the port
/// is a separate option.
///
/// A URL is rejected rather than silently stripped: `http://garage:3900`
/// would otherwise become an endpoint of `http` and a very confusing
/// connection failure. pgBackRest always speaks TLS to object storage, so
/// the scheme was never meaningful here.
fn split_endpoint(endpoint: Option<&str>) -> Result<(Option<String>, Option<u16>), Error> {
    let Some(raw) = endpoint else {
        return Ok((None, None));
    };
    let raw = raw.trim();
    if raw.contains("://") {
        return Err(Error::Destination {
            url: raw.to_string(),
            detail: "endpoint must be a host, not a URL — pgBackRest always \
                     uses TLS, so the scheme has no meaning. Write \
                     `garage.internal` and set a port if it is not 443"
                .to_string(),
        });
    }
    match raw.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse::<u16>().map_err(|_| Error::Destination {
                url: raw.to_string(),
                detail: format!("{port:?} is not a port"),
            })?;
            Ok((Some(host.to_string()), Some(port)))
        }
        None => Ok((Some(raw.to_string()), None)),
    }
}

/// Everything the rendered `pgbackrest.conf` depends on.
#[derive(Debug, Clone)]
pub struct PgBackRestConfig {
    /// pgBackRest's name for a cluster. pgpod uses the cluster id, which
    /// is already constrained to what podman and pgBackRest both accept.
    pub stanza: String,
    pub repositories: Vec<Repository>,
    /// `retention: 14d` from the manifest, if retention is enforced.
    pub retention_days: Option<u32>,
    /// zstd level. pgBackRest's default is gzip; zstd is both faster and
    /// smaller, and ADR 01 §10's reasoning about compression carries over.
    pub compress_level: u8,
    /// A second stanza this instance must also be able to address.
    ///
    /// Set while restoring: `restore_command` names the *source* cluster's
    /// stanza, and pgBackRest will not accept a stanza the configuration
    /// does not define. Both sections point at the same `pg1-path` —
    /// they differ only in which subtree of the repository they name,
    /// because pgBackRest appends the stanza to `repo-path`.
    pub recovery_stanza: Option<String>,
}

impl PgBackRestConfig {
    /// Build from a cluster's manifest backup spec.
    pub fn new(
        stanza: impl Into<String>,
        destinations: &[Destination],
        retention: Option<&str>,
        retention_mode: RetentionMode,
    ) -> Result<Self, Error> {
        if destinations.is_empty() {
            return Err(Error::NoDestinations);
        }
        if destinations.len() > MAX_REPOSITORIES {
            return Err(Error::TooManyRepositories {
                max: MAX_REPOSITORIES,
                given: destinations.len(),
            });
        }

        let repositories = destinations
            .iter()
            .map(Repository::from_destination)
            .collect::<Result<Vec<_>, _>>()?;

        // Retention is only configured when the operator has opted into
        // enforcement. Unset means pgBackRest keeps everything, which is
        // ADR 01 §6's "deletion is off by default" preserved exactly: a
        // tool that silently deletes backups has to earn that trust first.
        let retention_days = match retention_mode {
            RetentionMode::Report => None,
            RetentionMode::Enforce => match retention {
                None => None,
                Some(r) => Some(parse_days(r)?),
            },
        };

        Ok(Self {
            stanza: stanza.into(),
            repositories,
            retention_days,
            compress_level: 3,
            recovery_stanza: None,
        })
    }

    /// Attach per-repository credentials, in destination order.
    ///
    /// Read by the caller from the mounted podman secrets — this crate
    /// stays pure, and the agent is the only thing that should be opening
    /// files under `/run/secrets`. Rendered with the rest of `extra`,
    /// after the derived options, so an explicit `gcs-key-type=service`
    /// in a secret corrects the `auto` default rather than fighting it.
    ///
    /// Fewer entries than repositories is fine and normal: on GCE nothing
    /// is mounted at all, because pgBackRest authorizes with the instance
    /// service account.
    pub fn with_credentials(mut self, per_repo: Vec<Vec<(String, String)>>) -> Self {
        for (repo, creds) in self.repositories.iter_mut().zip(per_repo) {
            repo.extra.extend(creds);
        }
        self
    }

    /// Also define `stanza`, so `restore_command` can name it.
    pub fn with_recovery_stanza(mut self, stanza: Option<String>) -> Self {
        // A restored cluster whose source happens to share its name needs
        // one section, not two identical ones.
        self.recovery_stanza = stanza.filter(|s| *s != self.stanza);
        self
    }

    /// Render the file.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "# Managed by pgpod. Rewritten on every instance start.\n\
             # Edits here are lost; change spec.backup in the cluster manifest.\n\n\
             [global]\n",
        );

        for (i, repo) in self.repositories.iter().enumerate() {
            repo.render(i + 1, &mut out);
        }

        if let Some(days) = self.retention_days {
            out.push_str(
                "\n# Retention. Only present when spec.backup.retentionMode\n\
                          # is `enforce` — unset means pgBackRest keeps everything.\n",
            );
            let _ = writeln!(out, "repo1-retention-full-type=time");
            let _ = writeln!(out, "repo1-retention-full={days}");
        }

        out.push_str(
            "\n# --- how pgBackRest re-invokes itself ---\n\
             # pgBackRest forks worker processes by re-executing its own\n\
             # binary, which it finds via /proc/self/exe — the *real* binary,\n\
             # not the wrapper that set the library path. Without this the\n\
             # parent runs fine and every worker dies with\n\
             # `libssh2.so.1: cannot open shared object file` (ADR 04 §2).\n",
        );
        let _ = writeln!(out, "cmd={}", container::PGBACKREST_BIN);

        out.push_str(
            "\n# --- compression ---\n\
             # pgBackRest spells it `zst`, not `zstd`, and rejects the latter\n\
             # outright: allowed values are none, bz2, gz, lz4, zst.\n",
        );
        let _ = writeln!(out, "compress-type=zst");
        let _ = writeln!(out, "compress-level={}", self.compress_level);

        out.push_str(
            "\n# --- paths ---\n\
             # All inside the volume: the container's rootfs is read-only, so\n\
             # pgBackRest's own defaults under /var and /etc are not writable\n\
             # (ADR 04 §3).\n",
        );
        let _ = writeln!(out, "lock-path={}", container::SOCKET_DIR);
        let _ = writeln!(out, "spool-path={}", container::SPOOL_DIR);
        let _ = writeln!(out, "log-path={}", container::LOG_DIR);

        out.push_str(
            "\n# --- logging ---\n\
             # To the container stream, which is where every other pgpod log\n\
             # line goes and what `pgpod logs` reads.\n",
        );
        let _ = writeln!(out, "log-level-console=info");
        let _ = writeln!(out, "log-level-file=off");

        out.push_str(
            "\n# --- archiving ---\n\
             # Synchronous, deliberately. With archive-async the command can\n\
             # return before the segment is durable, and `archive_command`\n\
             # returning 0 is PostgreSQL's signal that pg_wal may recycle it\n\
             # (ADR 01 §1, kept by ADR 04 §4).\n\
             #\n\
             # archive-push-queue-max is deliberately absent: it makes\n\
             # pgBackRest *drop* WAL to keep the primary running, which turns\n\
             # a loud, recoverable outage into a silent gap in the archive.\n",
        );
        let _ = writeln!(out, "archive-async=n");

        for stanza in std::iter::once(&self.stanza).chain(self.recovery_stanza.iter()) {
            let _ = write!(
                out,
                "\n[{stanza}]\n\
                 pg1-path={}\n\
                 pg1-port={}\n\
                 pg1-socket-path={}\n",
                container::PGDATA,
                5432,
                container::SOCKET_DIR,
            );
        }
        out
    }
}

/// Parse `14d` into days.
///
/// pgBackRest's time-based retention is expressed in whole days, so the
/// hour and minute windows ADR 01 §6 accepted have nowhere to go. Rejecting
/// them is better than rounding: a window that silently became something
/// else deletes backups.
fn parse_days(window: &str) -> Result<u32, Error> {
    let w = window.trim();
    let (value, unit) = w.split_at(
        w.find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| Error::Invalid(format!("{w:?} has no unit — try 14d")))?,
    );
    let n: u32 = value
        .parse()
        .map_err(|_| Error::Invalid(format!("{w:?} does not start with a number")))?;
    if n == 0 {
        return Err(Error::Invalid(format!("{w:?} is not a positive duration")));
    }
    match unit {
        "d" => Ok(n),
        other => Err(Error::Invalid(format!(
            "unknown unit {other:?} in {w:?} — pgBackRest's time-based \
             retention counts whole days, so only `d` is accepted"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(url: &str) -> Destination {
        Destination::new(url)
    }

    fn repo(url: &str) -> Repository {
        Repository::from_destination(&dest(url)).unwrap()
    }

    fn config(urls: &[&str]) -> PgBackRestConfig {
        let d: Vec<Destination> = urls.iter().map(|u| dest(u)).collect();
        PgBackRestConfig::new("mydb", &d, None, RetentionMode::Report).unwrap()
    }

    #[test]
    fn a_local_destination_becomes_a_posix_repository() {
        let r = repo("file:///archive/demo");
        assert_eq!(r.kind, RepoKind::Posix);
        assert_eq!(r.path, "/archive/demo");
    }

    #[test]
    fn an_s3_destination_splits_bucket_from_prefix() {
        let r = repo("s3://pgpod-backups/prod");
        assert_eq!(
            r.kind,
            RepoKind::S3 {
                bucket: "pgpod-backups".into(),
                endpoint: None,
                region: None
            }
        );
        assert_eq!(r.path, "/prod");
    }

    #[test]
    fn a_bucket_with_no_prefix_gets_the_repository_root() {
        // pgBackRest appends the stanza to repo-path, so several clusters
        // can share one bucket without colliding — the property ADR 01 §2
        // wanted, now provided by pgBackRest rather than by pgpod.
        let r = repo("gs://pgpod-backups");
        assert_eq!(
            r.kind,
            RepoKind::Gcs {
                bucket: "pgpod-backups".into()
            }
        );
        assert_eq!(r.path, "/");
    }

    #[test]
    fn gcs_defaults_to_the_instance_service_account() {
        // On GCE this means no credential is mounted at all.
        let rendered = config(&["gs://pgpod-backups/prod"]).render();
        assert!(rendered.contains("repo1-gcs-key-type=auto"), "{rendered}");
    }

    #[test]
    fn an_endpoint_is_split_into_host_and_port() {
        let d = Destination {
            url: "s3://mirror/pgpod".into(),
            endpoint: Some("garage.internal:3900".into()),
            region: Some("garage".into()),
            ..Default::default()
        };
        let r = Repository::from_destination(&d).unwrap();
        assert_eq!(
            r.kind,
            RepoKind::S3 {
                bucket: "mirror".into(),
                endpoint: Some("garage.internal".into()),
                region: Some("garage".into())
            }
        );
        assert_eq!(r.port, Some(3900));
    }

    #[test]
    fn an_endpoint_written_as_a_url_is_refused() {
        // pgBackRest's endpoint is a bare host and it always speaks TLS,
        // so `http://garage:3900` would set the endpoint to "http" and
        // fail somewhere far away. The scheme was never meaningful.
        let d = Destination {
            url: "s3://mirror/pgpod".into(),
            endpoint: Some("http://garage.internal:3900".into()),
            ..Default::default()
        };
        let err = Repository::from_destination(&d).unwrap_err();
        assert!(err.to_string().contains("not a URL"), "{err}");
    }

    #[test]
    fn tls_verification_is_only_disabled_when_asked() {
        let plain = config(&["s3://mirror/pgpod"]).render();
        assert_eq!(setting(&plain, "repo1-storage-verify-tls"), None, "{plain}");

        let d = Destination {
            url: "s3://mirror/pgpod".into(),
            verify_tls: false,
            ..Default::default()
        };
        let mut out = String::new();
        Repository::from_destination(&d)
            .unwrap()
            .render(1, &mut out);
        assert!(out.contains("repo1-storage-verify-tls=n"), "{out}");
    }

    #[test]
    fn several_destinations_become_numbered_repositories() {
        let rendered = config(&["file:///archive/a", "s3://mirror/pgpod"]).render();
        assert!(rendered.contains("repo1-type=posix"), "{rendered}");
        assert!(rendered.contains("repo2-type=s3"), "{rendered}");
    }

    #[test]
    fn more_destinations_than_pgbackrest_supports_is_refused() {
        // Silently dropping the fifth would leave an operator believing
        // they had a mirror they do not have.
        let d: Vec<Destination> = (0..5).map(|i| dest(&format!("s3://b{i}/p"))).collect();
        let err = PgBackRestConfig::new("mydb", &d, None, RetentionMode::Report).unwrap_err();
        assert!(matches!(err, Error::TooManyRepositories { .. }), "{err}");
    }

    #[test]
    fn no_destination_is_an_error_rather_than_an_empty_config() {
        // An empty config would archive successfully to nowhere.
        assert!(matches!(
            PgBackRestConfig::new("mydb", &[], None, RetentionMode::Report),
            Err(Error::NoDestinations)
        ));
    }

    #[test]
    fn retention_is_absent_unless_enforcement_is_asked_for() {
        // ADR 01 §6: deletion is off until the operator opts in, and an
        // unset retention means pgBackRest keeps everything.
        let d = [dest("s3://b/p")];
        let report = PgBackRestConfig::new("mydb", &d, Some("14d"), RetentionMode::Report).unwrap();
        assert_eq!(report.retention_days, None);
        let rendered = report.render();
        assert!(
            !settings(&rendered)
                .iter()
                .any(|(k, _)| k.contains("retention")),
            "{rendered}"
        );

        let enforce =
            PgBackRestConfig::new("mydb", &d, Some("14d"), RetentionMode::Enforce).unwrap();
        assert_eq!(enforce.retention_days, Some(14));
        let rendered = enforce.render();
        assert!(
            rendered.contains("repo1-retention-full-type=time"),
            "{rendered}"
        );
        assert!(rendered.contains("repo1-retention-full=14"), "{rendered}");
    }

    #[test]
    fn a_retention_window_pgbackrest_cannot_express_is_refused() {
        // Its time-based retention counts whole days. Rounding `48h` to a
        // day, or silently ignoring it, deletes backups.
        let d = [dest("s3://b/p")];
        for bad in ["48h", "30m", "14", "d", "0d", "2w", ""] {
            assert!(
                PgBackRestConfig::new("mydb", &d, Some(bad), RetentionMode::Enforce).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    /// The settings actually in effect, ignoring comments and blanks.
    ///
    /// Rendered configs carry a lot of explanation, and matching on the
    /// whole string would let a comment satisfy — or violate — an
    /// assertion about a setting.
    fn settings(rendered: &str) -> Vec<(String, String)> {
        rendered
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('['))
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn setting(rendered: &str, key: &str) -> Option<String> {
        settings(rendered)
            .into_iter()
            .rfind(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    #[test]
    fn archiving_is_synchronous_and_never_drops_wal() {
        // The two settings that would quietly convert a loud archive
        // outage into a silent gap. ADR 01 §1 is the whole reason pgpod
        // archives from inside the container at all.
        let rendered = config(&["s3://b/p"]).render();
        assert_eq!(setting(&rendered, "archive-async").as_deref(), Some("n"));
        assert_eq!(
            setting(&rendered, "archive-push-queue-max"),
            None,
            "pgpod must never set a queue limit that drops WAL"
        );
    }

    #[test]
    fn every_writable_path_is_inside_the_volume() {
        // The container rootfs is read-only, so pgBackRest's defaults
        // under /var and /etc would fail at the first archived segment.
        let rendered = config(&["s3://b/p"]).render();
        for key in ["lock-path", "spool-path", "log-path"] {
            let line = rendered
                .lines()
                .find(|l| l.starts_with(key))
                .unwrap_or_else(|| panic!("{key} missing from:\n{rendered}"));
            let (_, path) = line.split_once('=').unwrap();
            assert!(
                path.starts_with(container::VOLUME_MOUNT),
                "{key} is outside the volume: {line}"
            );
        }
    }

    #[test]
    fn the_stanza_section_points_at_pgdata_and_the_socket() {
        let rendered = config(&["s3://b/p"]).render();
        assert!(rendered.contains("\n[mydb]\n"), "{rendered}");
        assert!(
            rendered.contains(&format!("pg1-path={}", container::PGDATA)),
            "{rendered}"
        );
        // The socket lives in the volume, not /var/run/postgresql — see
        // container::SOCKET_DIR for why.
        assert!(
            rendered.contains(&format!("pg1-socket-path={}", container::SOCKET_DIR)),
            "{rendered}"
        );
    }

    #[test]
    fn credentials_land_on_the_repository_they_belong_to() {
        // Off by one here would sign requests to one destination with
        // another's key, which fails as an authentication error naming the
        // wrong bucket.
        let rendered = config(&["s3://one/p", "s3://two/p"])
            .with_credentials(vec![
                vec![("s3-key".into(), "KEY-ONE".into())],
                vec![("s3-key".into(), "KEY-TWO".into())],
            ])
            .render();
        assert_eq!(
            setting(&rendered, "repo1-s3-key").as_deref(),
            Some("KEY-ONE")
        );
        assert_eq!(
            setting(&rendered, "repo2-s3-key").as_deref(),
            Some("KEY-TWO")
        );
    }

    #[test]
    fn a_destination_with_no_credentials_is_left_alone() {
        // The GCE case: nothing is mounted, because pgBackRest authorizes
        // with the instance service account.
        let rendered = config(&["gs://b/p", "s3://c/p"])
            .with_credentials(vec![vec![]])
            .render();
        assert_eq!(
            setting(&rendered, "repo1-gcs-key-type").as_deref(),
            Some("auto")
        );
        assert_eq!(setting(&rendered, "repo2-s3-key"), None);
    }

    #[test]
    fn object_storage_verifies_against_the_bundle_s_own_ca_store() {
        // OpenSSL opens its trust store at runtime, so it is invisible to
        // ldd and the bundle shipped without one at first. Left to the
        // image, TLS fails with "unable to get local issuer certificate"
        // on any image that has no certificates — the stock postgres:18
        // and Alpine images among them — while a posix repository stays
        // perfectly happy, so nothing local would catch it.
        let rendered = config(&["s3://b/p"]).render();
        assert_eq!(
            setting(&rendered, "repo1-storage-ca-file").as_deref(),
            Some(container::PGBACKREST_CA_FILE)
        );
    }

    #[test]
    fn a_local_repository_is_told_nothing_about_tls() {
        // There is no peer to verify, and a CA file on a posix repository
        // would only be something to misread later.
        let rendered = config(&["file:///archive/demo"]).render();
        assert_eq!(setting(&rendered, "repo1-storage-ca-file"), None);
        assert_eq!(setting(&rendered, "repo1-storage-verify-tls"), None);
    }

    #[test]
    fn workers_are_told_to_re_invoke_through_the_wrapper() {
        // pgBackRest re-executes itself for its local worker processes.
        // Left to find its own path it picks the real binary, skips the
        // wrapper that sets --library-path, and every worker dies on a
        // missing shared object while the parent looks healthy.
        let rendered = config(&["s3://b/p"]).render();
        assert_eq!(
            setting(&rendered, "cmd").as_deref(),
            Some(container::PGBACKREST_BIN)
        );
    }

    #[test]
    fn compression_uses_the_spelling_pgbackrest_accepts() {
        // `zstd` is rejected outright — pgBackRest's value is `zst`. Found
        // by running it, and pinned here because the wrong spelling fails
        // at backup time rather than at apply time.
        let rendered = config(&["s3://b/p"]).render();
        assert_eq!(setting(&rendered, "compress-type").as_deref(), Some("zst"));
    }

    #[test]
    fn a_recovery_stanza_gets_its_own_section() {
        // restore_command names the source cluster's stanza, and
        // pgBackRest refuses a stanza the config does not define.
        let rendered = config(&["s3://b/p"])
            .with_recovery_stanza(Some("sourcedb".into()))
            .render();
        assert!(rendered.contains("\n[mydb]\n"), "{rendered}");
        assert!(rendered.contains("\n[sourcedb]\n"), "{rendered}");
        // Both point at this instance's data directory: they differ only
        // in which subtree of the repository they name.
        assert_eq!(
            rendered
                .matches(&format!("pg1-path={}", container::PGDATA))
                .count(),
            2,
            "{rendered}"
        );
    }

    #[test]
    fn a_recovery_stanza_matching_our_own_is_not_duplicated() {
        // pgBackRest would reject a config with the same section twice.
        let rendered = config(&["s3://b/p"])
            .with_recovery_stanza(Some("mydb".into()))
            .render();
        assert_eq!(rendered.matches("\n[mydb]\n").count(), 1, "{rendered}");
    }

    #[test]
    fn an_explicit_option_can_correct_a_derived_one() {
        // The escape hatch is useless if a derived value silently wins.
        let mut d = dest("gs://b/p");
        d.options.insert("gcs-key-type".into(), "service".into());
        let mut out = String::new();
        Repository::from_destination(&d)
            .unwrap()
            .render(1, &mut out);
        let positions: Vec<_> = out.match_indices("repo1-gcs-key-type=").collect();
        assert_eq!(positions.len(), 2, "{out}");
        assert!(
            out.rfind("repo1-gcs-key-type=service").unwrap()
                > out.find("repo1-gcs-key-type=auto").unwrap(),
            "the explicit value must come last: {out}"
        );
    }

    #[test]
    fn a_malformed_destination_is_refused() {
        for bad in [
            "/archive/demo",
            "ftp://host/path",
            "s3://",
            "gs://",
            "file://host/path",
        ] {
            assert!(
                Repository::from_destination(&dest(bad)).is_err(),
                "{bad:?} should be refused"
            );
        }
    }
}
