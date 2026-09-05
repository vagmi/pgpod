//! `pgpod doctor` — preflight the host.
//!
//! pgpod's runtime assumptions are host properties, not application
//! properties (ADR 03). This command is the contract: it either passes on
//! your machine or names precisely what is missing, with the command that
//! fixes it. Every failure mode it covers is one that would otherwise
//! surface later as a confusing error three steps downstream.

use std::path::Path;

use pgpod_core::PathLayout;
use pgpod_runtime::{PodmanClient, PodmanInfo, SocketSource, discover};
use serde::Serialize;

use crate::output::CommandOutput;

/// Minimum podman we are willing to drive. Ubuntu 26.04 ships 5.7.0.
const MIN_PODMAN_MAJOR: u32 = 5;

/// Below this, `initdb` plus a base backup will not fit comfortably.
/// Advisory: it is a warning, never a failure — it is the operator's disk.
const GRAPH_ROOT_FREE_WARN_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// PostgreSQL wants a file descriptor per connection plus overhead.
const NOFILE_WARN: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    /// Works, but will bite later.
    Warn,
    /// pgpod cannot function until this is fixed.
    Fail,
    /// We could not determine the answer. Deliberately distinct from
    /// `Pass` — "podman did not report the network backend" is not
    /// evidence that the backend is correct.
    Unknown,
}

impl Status {
    fn glyph(&self) -> &'static str {
        match self {
            Self::Pass => "ok  ",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
            Self::Unknown => "?   ",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// What to actually run to fix it. Present on anything not passing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Check {
    fn new(name: &str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status,
            detail: detail.into(),
            hint: None,
        }
    }

    fn pass(name: &str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Pass, detail)
    }

    fn warn(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(name, Status::Warn, detail).with_hint(hint)
    }

    fn fail(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(name, Status::Fail, detail).with_hint(hint)
    }

    fn unknown(name: &str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Unknown, detail)
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
    pub healthy: bool,
}

impl DoctorReport {
    fn new(checks: Vec<Check>) -> Self {
        let healthy = !checks.iter().any(|c| c.status == Status::Fail);
        Self { checks, healthy }
    }

    /// Process exit code: non-zero when something must be fixed, so
    /// `pgpod doctor` is usable in a provisioning script's `set -e`.
    pub fn exit_code(&self) -> i32 {
        if self.healthy { 0 } else { 1 }
    }
}

impl CommandOutput for DoctorReport {
    fn to_text(&self) -> String {
        let width = self
            .checks
            .iter()
            .map(|c| c.name.len())
            .max()
            .unwrap_or(0)
            .max(4);

        let mut out = String::new();
        for c in &self.checks {
            out.push_str(&format!(
                "  [{}] {:width$}  {}\n",
                c.status.glyph(),
                c.name,
                c.detail,
                width = width
            ));
            if let Some(hint) = &c.hint {
                for line in hint.lines() {
                    out.push_str(&format!(
                        "         {:width$}  → {}\n",
                        "",
                        line,
                        width = width
                    ));
                }
            }
        }

        let fails = self
            .checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .count();
        let warns = self
            .checks
            .iter()
            .filter(|c| c.status == Status::Warn)
            .count();
        out.push('\n');
        if self.healthy && warns == 0 {
            out.push_str("host is ready for pgpod\n");
        } else if self.healthy {
            out.push_str(&format!(
                "host is usable, with {warns} warning(s) worth addressing\n"
            ));
        } else {
            out.push_str(&format!(
                "host is NOT ready: {fails} failure(s), {warns} warning(s)\n"
            ));
        }
        out
    }
}

/// Run every check. Never returns `Err` for a *host* problem — a broken
/// host is a report, not an error. `Err` is reserved for pgpod itself
/// malfunctioning.
pub async fn run(layout: &PathLayout) -> DoctorReport {
    let mut checks = vec![check_not_root()];

    let (socket_path, socket_source) = discover();
    checks.push(check_socket(&socket_path, socket_source));

    match PodmanClient::connect_at(&socket_path) {
        Err(_) => {
            // Connecting failed, so every podman-dependent check is
            // unanswerable. Say so rather than emitting a wall of
            // failures that all share one root cause.
            checks.push(Check::unknown(
                "podman api",
                "skipped — no socket to talk to",
            ));
        }
        Ok(client) => match client.ping().await {
            Err(e) => {
                checks.push(Check::fail(
                    "podman api",
                    format!("socket exists but the daemon did not answer: {e}"),
                    "systemctl --user restart podman.socket",
                ));
            }
            Ok(()) => {
                checks.push(Check::pass("podman api", "responding to /_ping"));
                match client.info().await {
                    Err(e) => checks.push(Check::fail(
                        "podman info",
                        format!("could not read host info: {e}"),
                        "check `podman info` directly",
                    )),
                    Ok(info) => checks.extend(checks_from_info(&info)),
                }
            }
        },
    }

    checks.push(check_nofile());
    checks.push(check_agent_binary(layout));
    DoctorReport::new(checks)
}

fn check_not_root() -> Check {
    let uid = nix::unistd::Uid::current();
    if uid.is_root() {
        Check::fail(
            "not root",
            "running as uid 0",
            "pgpod is rootless by design — run as an unprivileged user",
        )
    } else {
        Check::pass("not root", format!("uid {uid}"))
    }
}

fn check_socket(path: &Path, source: SocketSource) -> Check {
    let via = source.as_str();
    if path.exists() {
        Check::pass("podman socket", format!("{} (via {via})", path.display()))
    } else {
        let mut hint = String::from(
            "systemctl --user enable --now podman.socket\n\
             loginctl enable-linger $(id -un)",
        );
        if source == SocketSource::UidFallback {
            // ADR 03 §2: this is the `sudo -iu` shape, where the real
            // problem is usually a session without pam_systemd rather
            // than a missing socket unit.
            hint.push_str(
                "\nXDG_RUNTIME_DIR is unset — if you reached this shell via \
                 `sudo -iu`, use `sudo -Hu` and export XDG_RUNTIME_DIR and \
                 DBUS_SESSION_BUS_ADDRESS explicitly",
            );
        }
        Check::fail(
            "podman socket",
            format!("not found at {} (via {via})", path.display()),
            hint,
        )
    }
}

fn checks_from_info(info: &PodmanInfo) -> Vec<Check> {
    vec![
        check_version(info),
        check_rootless(info),
        check_network_backend(info),
        check_cgroups(info),
        check_oci_runtime(info),
        check_subids(info),
        check_graph_root(info),
    ]
}

fn check_version(info: &PodmanInfo) -> Check {
    let reported = info.server_version.as_deref().unwrap_or("unknown");
    let api = info.api_version.as_deref().unwrap_or("unknown");
    match info.server_version_parts() {
        None => Check::unknown("podman version", format!("could not parse {reported:?}")),
        Some((major, _)) if major < MIN_PODMAN_MAJOR => Check::fail(
            "podman version",
            format!("{reported} (need >= {MIN_PODMAN_MAJOR}.0)"),
            "Ubuntu 26.04 ships podman 5.7; on older releases install a backport",
        ),
        Some((major, minor)) => Check::pass(
            "podman version",
            // The client-vs-server gap is expected and recorded (ADR 03
            // §3), so report it plainly rather than warning about it.
            format!("server {major}.{minor} ({reported}), libpod api {api}"),
        ),
    }
}

fn check_rootless(info: &PodmanInfo) -> Check {
    match info.rootless {
        Some(true) => Check::pass("rootless", "podman is running rootless"),
        Some(false) => Check::fail(
            "rootless",
            "podman reports it is running as root",
            "pgpod refuses a rootful podman — use the user socket, not the system one",
        ),
        None => Check::unknown("rootless", "podman did not report a security section"),
    }
}

fn check_network_backend(info: &PodmanInfo) -> Check {
    match info.uses_netavark() {
        Some(true) => Check::pass("network backend", "netavark"),
        Some(false) => Check::fail(
            "network backend",
            format!(
                "{} — pgpod needs netavark for container-name DNS",
                info.network_backend.as_deref().unwrap_or("unknown")
            ),
            "add to /etc/containers/containers.conf:\n\
             [network]\n\
             network_backend = \"netavark\"\n\
             then: podman system reset  (destroys existing containers)",
        ),
        None => Check::unknown("network backend", "podman did not report it"),
    }
}

fn check_cgroups(info: &PodmanInfo) -> Check {
    // podman 6.x reports "v2"; older versions report a bare "2". Matching
    // one spelling exactly turns a perfectly healthy host red, so
    // normalize before comparing.
    let version = info
        .cgroup_version
        .as_deref()
        .map(|v| v.trim_start_matches('v'));
    match version {
        Some("2") => Check::pass(
            "cgroups",
            format!(
                "v2 ({})",
                info.cgroup_manager.as_deref().unwrap_or("unknown manager")
            ),
        ),
        Some(other) => Check::fail(
            "cgroups",
            format!("v{other} — rootless resource limits need v2"),
            "boot with systemd.unified_cgroup_hierarchy=1",
        ),
        None => Check::unknown("cgroups", "podman did not report a cgroup version"),
    }
}

fn check_oci_runtime(info: &PodmanInfo) -> Check {
    match info.oci_runtime.as_deref() {
        Some(name) => Check::pass("oci runtime", name.to_string()),
        None => Check::unknown("oci runtime", "podman did not report one"),
    }
}

fn check_subids(info: &PodmanInfo) -> Check {
    // Rootless podman needs subordinate ranges to run a container as any
    // UID but the user's own, and PostgreSQL runs as 999 (or 26 on
    // CNPG-style images). Dropping keep-id did not remove this.
    if info.subuid_count == 0 || info.subgid_count == 0 {
        return Check::fail(
            "subuid/subgid",
            format!(
                "subuid={} subgid={} — containers are confined to a single UID, \
                 so postgres (uid 999) cannot start",
                info.subuid_count, info.subgid_count
            ),
            "sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $(id -un)\n\
             podman system migrate",
        );
    }
    Check::pass(
        "subuid/subgid",
        format!(
            "{} subuids, {} subgids",
            info.subuid_count, info.subgid_count
        ),
    )
}

fn check_graph_root(info: &PodmanInfo) -> Check {
    let root = info.graph_root.as_deref().unwrap_or("unknown");
    let driver = info.graph_driver.as_deref().unwrap_or("unknown");
    match info.graph_root_free() {
        None => Check::unknown(
            "graph root",
            format!("{root} ({driver}) — podman did not report free space"),
        ),
        Some(free) if free < GRAPH_ROOT_FREE_WARN_BYTES => Check::warn(
            "graph root",
            format!("{root} ({driver}), {} free", human_bytes(free)),
            // Volumes now hold databases (ADR 00 §4), so this directory is
            // not just image layers any more.
            "PGDATA lives here, not under your home directory.\n\
             Set `graphroot` in ~/.config/containers/storage.conf BEFORE \
             creating clusters — moving it later means moving live databases.",
        ),
        Some(free) => Check::pass(
            "graph root",
            format!("{root} ({driver}), {} free", human_bytes(free)),
        ),
    }
}

fn check_nofile() -> Check {
    match nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE) {
        Err(e) => Check::unknown("open files", format!("could not read RLIMIT_NOFILE: {e}")),
        Ok((soft, _hard)) if soft < NOFILE_WARN => Check::warn(
            "open files",
            format!("soft limit {soft} is low for a busy postgres"),
            format!("raise to at least {NOFILE_WARN}: ulimit -n {NOFILE_WARN}"),
        ),
        Ok((soft, hard)) => Check::pass("open files", format!("soft {soft}, hard {hard}")),
    }
}

fn check_agent_binary(layout: &PathLayout) -> Check {
    let path = layout.agent_binary_for_host();
    if path.exists() {
        Check::pass("agent binary", path.display().to_string())
    } else {
        // Phase 0 has not built it yet. Reporting this as a failure would
        // make `doctor` red on a correctly provisioned host, which trains
        // people to ignore it.
        Check::warn(
            "agent binary",
            format!("not present at {}", path.display()),
            "built and installed from Phase 1 — nothing uses it yet",
        )
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> PodmanInfo {
        PodmanInfo {
            server_version: Some("5.7.0".into()),
            rootless: Some(true),
            network_backend: Some("netavark".into()),
            cgroup_version: Some("2".into()),
            oci_runtime: Some("crun".into()),
            subuid_count: 65536,
            subgid_count: 65536,
            graph_root: Some("/home/u/.local/share/containers/storage".into()),
            graph_root_allocated: Some(500 * 1024 * 1024 * 1024),
            graph_root_used: Some(10 * 1024 * 1024 * 1024),
            ..Default::default()
        }
    }

    #[test]
    fn a_well_provisioned_host_passes_every_podman_check() {
        let checks = checks_from_info(&info());
        for c in &checks {
            assert_eq!(
                c.status,
                Status::Pass,
                "{} should pass: {}",
                c.name,
                c.detail
            );
        }
    }

    #[test]
    fn cgroup_version_is_accepted_in_both_spellings_podman_uses() {
        // podman 6.1 reports "v2"; older releases report "2". pgpod spans
        // both (adrs/03 §3), and matching one exactly made a healthy host
        // report FAIL with the nonsense detail "vv2".
        for spelling in ["2", "v2"] {
            let i = PodmanInfo {
                cgroup_version: Some(spelling.into()),
                ..Default::default()
            };
            assert_eq!(
                check_cgroups(&i).status,
                Status::Pass,
                "cgroup version {spelling:?} should pass"
            );
        }
        let v1 = PodmanInfo {
            cgroup_version: Some("v1".into()),
            ..Default::default()
        };
        assert_eq!(
            check_cgroups(&v1).status,
            Status::Fail,
            "v1 is genuinely unsupported"
        );
    }

    #[test]
    fn cni_is_a_failure_not_a_warning() {
        // On CNI, `network inspect` succeeds for a network that
        // `run --network` cannot find (ADR 03 §1). Degrading this to a
        // warning would let a cluster get created and then fail to
        // replicate for reasons nobody can trace.
        let mut i = info();
        i.network_backend = Some("cni".into());
        assert_eq!(check_network_backend(&i).status, Status::Fail);
    }

    #[test]
    fn missing_subids_fail_because_postgres_cannot_start() {
        let mut i = info();
        i.subuid_count = 0;
        assert_eq!(check_subids(&i).status, Status::Fail);
    }

    #[test]
    fn old_podman_fails_and_new_podman_passes() {
        let mut i = info();
        i.server_version = Some("4.9.3".into());
        assert_eq!(check_version(&i).status, Status::Fail);

        // A server newer than podman-api's declared 4.3 target is the
        // expected case, not a problem (ADR 03 §3).
        i.server_version = Some("6.1.1".into());
        assert_eq!(check_version(&i).status, Status::Pass);
    }

    #[test]
    fn unreported_facts_are_unknown_not_pass() {
        let empty = PodmanInfo::default();
        for c in checks_from_info(&empty) {
            assert_ne!(
                c.status,
                Status::Pass,
                "{} must not pass on absent data: {}",
                c.name,
                c.detail
            );
        }
    }

    #[test]
    fn low_disk_warns_but_does_not_fail() {
        // It is the operator's disk; pgpod says so and gets out of the way.
        let mut i = info();
        i.graph_root_used = Some(i.graph_root_allocated.unwrap() - 1024);
        assert_eq!(check_graph_root(&i).status, Status::Warn);
    }

    #[test]
    fn report_health_ignores_warnings_but_not_failures() {
        let warn_only = DoctorReport::new(vec![Check::warn("w", "d", "h")]);
        assert!(warn_only.healthy);
        assert_eq!(warn_only.exit_code(), 0);

        let with_fail =
            DoctorReport::new(vec![Check::warn("w", "d", "h"), Check::fail("f", "d", "h")]);
        assert!(!with_fail.healthy);
        assert_eq!(with_fail.exit_code(), 1);
    }

    #[test]
    fn every_non_passing_check_tells_you_what_to_do() {
        let i = PodmanInfo {
            network_backend: Some("cni".into()),
            cgroup_version: Some("1".into()),
            server_version: Some("4.0.0".into()),
            ..Default::default()
        };
        for c in checks_from_info(&i) {
            if c.status == Status::Fail {
                assert!(c.hint.is_some(), "{} failed without a hint", c.name);
            }
        }
    }

    #[test]
    fn human_bytes_reads_naturally() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(10 * 1024 * 1024 * 1024), "10.0 GiB");
    }
}
