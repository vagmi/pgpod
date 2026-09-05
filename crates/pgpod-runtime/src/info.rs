//! Host facts pgpod reads from podman, in pgpod's own types.
//!
//! `podman-api`'s `models::Info` is a deep tree of `Option`s tracking
//! libpod's schema. Flattening it here is what keeps
//! `adrs/00-project-setup.md` §1's mitigation honest: no `podman_api::*`
//! type crosses a crate boundary, so swapping the client library touches
//! this file and nothing downstream.

use serde::Serialize;

/// A summary of `podman info`, restricted to facts pgpod makes decisions
/// on. Every field is optional because libpod omits keys across versions
/// and pgpod spans 5.7 through 6.x (ADR 03 §3) — a missing field must
/// degrade a check to "unknown", never panic.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PodmanInfo {
    /// Server version, e.g. `"5.7.0"`.
    pub server_version: Option<String>,
    /// libpod API version the server advertises.
    pub api_version: Option<String>,
    /// Whether podman believes it is running rootless. pgpod requires this.
    pub rootless: Option<bool>,
    /// `netavark` or `cni`. pgpod requires netavark for container-name DNS
    /// (ADR 00 §10).
    pub network_backend: Option<String>,
    /// `"2"` on any supported host.
    pub cgroup_version: Option<String>,
    pub cgroup_manager: Option<String>,
    /// Usually `crun` on Ubuntu.
    pub oci_runtime: Option<String>,
    /// Where volumes — and therefore PGDATA — actually live (ADR 00 §4).
    pub graph_root: Option<String>,
    pub graph_driver: Option<String>,
    /// Bytes allocated to and used by the graph root's filesystem. These
    /// are the numbers that matter now that databases live there.
    pub graph_root_allocated: Option<u64>,
    pub graph_root_used: Option<u64>,
    pub distribution: Option<String>,
    pub distribution_version: Option<String>,
    pub kernel: Option<String>,
    pub arch: Option<String>,
    /// Subordinate UIDs/GIDs available to this user, summed across every
    /// mapped range beyond the user's own identity mapping.
    ///
    /// Rootless podman needs these to run a container as any UID other
    /// than the invoking user's, and PostgreSQL runs as 999 (or 26 on
    /// CNPG-style images). Dropping `keep-id` did not remove this
    /// requirement — see `adrs/00-project-setup.md` consequences.
    pub subuid_count: u64,
    pub subgid_count: u64,
}

impl PodmanInfo {
    /// Free bytes on the graph root's filesystem, when podman reported
    /// both numbers.
    pub fn graph_root_free(&self) -> Option<u64> {
        match (self.graph_root_allocated, self.graph_root_used) {
            (Some(total), Some(used)) => Some(total.saturating_sub(used)),
            _ => None,
        }
    }

    /// Whether the network backend is the one pgpod needs. `None` when
    /// podman did not report it, which is distinct from "reported cni".
    pub fn uses_netavark(&self) -> Option<bool> {
        self.network_backend
            .as_deref()
            .map(|b| b.eq_ignore_ascii_case("netavark"))
    }

    /// Parse the server version into `(major, minor)` for comparisons.
    /// Returns `None` on anything unparseable rather than guessing.
    pub fn server_version_parts(&self) -> Option<(u32, u32)> {
        let v = self.server_version.as_deref()?;
        let mut it = v.split(['.', '-', '+']);
        let major = it.next()?.parse().ok()?;
        let minor = it.next().unwrap_or("0").parse().unwrap_or(0);
        Some((major, minor))
    }
}

impl From<podman_api::models::Info> for PodmanInfo {
    fn from(i: podman_api::models::Info) -> Self {
        let host = i.host;
        let store = i.store;
        let version = i.version;

        let (subuid_count, subgid_count) = host
            .as_ref()
            .and_then(|h| h.id_mappings.as_ref())
            .map(|m| {
                // Each entry maps a contiguous range. The user's own
                // identity mapping (container uid 0 -> host uid) is a
                // range of size 1, so summing everything and subtracting
                // it would be fragile; instead count only ranges larger
                // than one, which is what a /etc/subuid entry produces.
                let sum = |ranges: &Option<Vec<podman_api::models::IdMap>>| -> u64 {
                    ranges
                        .as_ref()
                        .map(|v| {
                            v.iter()
                                .filter_map(|r| r.size)
                                .filter(|&s| s > 1)
                                .map(|s| s as u64)
                                .sum()
                        })
                        .unwrap_or(0)
                };
                (sum(&m.uidmap), sum(&m.gidmap))
            })
            .unwrap_or((0, 0));

        Self {
            server_version: version.as_ref().and_then(|v| v.version.clone()),
            api_version: version.as_ref().and_then(|v| v.api_version.clone()),
            rootless: host
                .as_ref()
                .and_then(|h| h.security.as_ref())
                .and_then(|s| s.rootless),
            network_backend: host.as_ref().and_then(|h| h.network_backend.clone()),
            cgroup_version: host.as_ref().and_then(|h| h.cgroup_version.clone()),
            cgroup_manager: host.as_ref().and_then(|h| h.cgroup_manager.clone()),
            oci_runtime: host
                .as_ref()
                .and_then(|h| h.oci_runtime.as_ref())
                .and_then(|r| r.name.clone()),
            graph_root: store.as_ref().and_then(|s| s.graph_root.clone()),
            graph_driver: store.as_ref().and_then(|s| s.graph_driver_name.clone()),
            graph_root_allocated: store.as_ref().and_then(|s| s.graph_root_allocated),
            graph_root_used: store.as_ref().and_then(|s| s.graph_root_used),
            distribution: host
                .as_ref()
                .and_then(|h| h.distribution.as_ref())
                .and_then(|d| d.distribution.clone()),
            distribution_version: host
                .as_ref()
                .and_then(|h| h.distribution.as_ref())
                .and_then(|d| d.version.clone()),
            kernel: host.as_ref().and_then(|h| h.kernel.clone()),
            arch: host.as_ref().and_then(|h| h.arch.clone()),
            subuid_count,
            subgid_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_space_needs_both_numbers() {
        let mut i = PodmanInfo {
            graph_root_allocated: Some(100),
            graph_root_used: Some(30),
            ..Default::default()
        };
        assert_eq!(i.graph_root_free(), Some(70));

        i.graph_root_used = None;
        assert_eq!(i.graph_root_free(), None, "must not guess from one number");
    }

    #[test]
    fn free_space_does_not_underflow() {
        // podman has been known to report used > allocated on some
        // overlay setups; a wrapping subtraction here would print an
        // absurd free-space figure in doctor's output.
        let i = PodmanInfo {
            graph_root_allocated: Some(10),
            graph_root_used: Some(40),
            ..Default::default()
        };
        assert_eq!(i.graph_root_free(), Some(0));
    }

    #[test]
    fn netavark_detection_distinguishes_unknown_from_wrong() {
        let netavark = PodmanInfo {
            network_backend: Some("netavark".into()),
            ..Default::default()
        };
        let cni = PodmanInfo {
            network_backend: Some("cni".into()),
            ..Default::default()
        };
        assert_eq!(netavark.uses_netavark(), Some(true));
        assert_eq!(cni.uses_netavark(), Some(false));
        // Not reported is not the same as "reported cni" — doctor renders
        // these differently.
        assert_eq!(PodmanInfo::default().uses_netavark(), None);
    }

    #[test]
    fn parses_the_version_shapes_podman_actually_emits() {
        let parse = |v: &str| {
            PodmanInfo {
                server_version: Some(v.into()),
                ..Default::default()
            }
            .server_version_parts()
        };
        assert_eq!(parse("5.7.0"), Some((5, 7)));
        assert_eq!(parse("6.1.1"), Some((6, 1)));
        assert_eq!(parse("4.9.3-rhel"), Some((4, 9)));
        assert_eq!(parse("5"), Some((5, 0)));
        assert_eq!(parse("not-a-version"), None, "must not guess");
        assert_eq!(PodmanInfo::default().server_version_parts(), None);
    }
}
