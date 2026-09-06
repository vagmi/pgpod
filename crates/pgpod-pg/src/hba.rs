//! `pg_hba.conf` rendering.
//!
//! Fully pgpod-owned: the file `initdb` writes is replaced, not appended
//! to. Appending would leave `initdb`'s defaults in place *above* pgpod's
//! rules, and `pg_hba.conf` is first-match-wins — so a permissive default
//! would shadow every rule pgpod added below it.

use std::fmt::Write as _;

/// Authentication method for a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// Unix-socket only. The OS has already authenticated the peer, and
    /// the container runs exactly one user.
    Peer,
    Scram,
    /// Never rendered by pgpod for a TCP rule. Present so the type can
    /// describe an imported cluster's existing file.
    Trust,
}

impl AuthMethod {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Peer => "peer",
            Self::Scram => "scram-sha-256",
            Self::Trust => "trust",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConnType {
    Local,
    Host,
}

#[derive(Debug, Clone)]
struct Rule {
    conn: ConnType,
    database: String,
    user: String,
    address: Option<String>,
    method: AuthMethod,
    comment: Option<String>,
}

/// What the rendered `pg_hba.conf` needs to know.
#[derive(Debug, Clone)]
pub struct HbaConfig {
    /// CIDR of the cluster's podman network, so instances can reach each
    /// other for replication. `None` renders no network rules at all —
    /// a single-instance cluster does not need them, and a rule with a
    /// guessed CIDR would be either useless or too wide.
    pub network_cidr: Option<String>,
    /// Role standbys authenticate as.
    pub replication_role: String,
    /// Extra operator-supplied rules, appended last.
    pub extra: Vec<String>,
}

impl HbaConfig {
    pub fn new(replication_role: impl Into<String>) -> Self {
        Self {
            network_cidr: None,
            replication_role: replication_role.into(),
            extra: Vec::new(),
        }
    }

    pub fn with_network(mut self, cidr: impl Into<String>) -> Self {
        self.network_cidr = Some(cidr.into());
        self
    }

    fn rules(&self) -> Vec<Rule> {
        let mut rules = vec![
            // The agent and `pgpod psql` connect over the unix socket as
            // the container's own user. `peer` needs no password, which
            // keeps the superuser password off the local path entirely.
            Rule {
                conn: ConnType::Local,
                database: "all".into(),
                user: "all".into(),
                address: None,
                method: AuthMethod::Peer,
                comment: Some("agent and pgpod psql, over the unix socket".into()),
            },
            // Loopback inside the container's own netns. Not reachable
            // from the host or other containers, but postgres tooling
            // reaches for it, so it must not fall through to `reject`.
            Rule {
                conn: ConnType::Host,
                database: "all".into(),
                user: "all".into(),
                address: Some("127.0.0.1/32".into()),
                method: AuthMethod::Scram,
                comment: Some("container-local loopback".into()),
            },
            Rule {
                conn: ConnType::Host,
                database: "all".into(),
                user: "all".into(),
                address: Some("::1/128".into()),
                method: AuthMethod::Scram,
                comment: None,
            },
        ];

        if let Some(cidr) = &self.network_cidr {
            rules.push(Rule {
                conn: ConnType::Host,
                database: "all".into(),
                user: "all".into(),
                address: Some(cidr.clone()),
                method: AuthMethod::Scram,
                comment: Some("application traffic from the cluster network".into()),
            });
            // Replication is a separate pseudo-database and needs its own
            // rule; the `all` rules above do not cover it.
            rules.push(Rule {
                conn: ConnType::Host,
                database: "replication".into(),
                user: self.replication_role.clone(),
                address: Some(cidr.clone()),
                method: AuthMethod::Scram,
                comment: Some("standbys streaming from this instance".into()),
            });
        }

        rules
    }

    pub fn render(&self) -> String {
        let mut out = String::from(
            "# Managed by pgpod. Rewritten on every instance start.\n\
             # Edits here are lost; use spec.postgresql.pg_hba for extra rules.\n\
             #\n\
             # First match wins, so order matters.\n\n\
             # TYPE  DATABASE        USER               ADDRESS              METHOD\n",
        );

        for rule in self.rules() {
            if let Some(comment) = &rule.comment {
                let _ = writeln!(out, "# {comment}");
            }
            let conn = match rule.conn {
                ConnType::Local => "local",
                ConnType::Host => "host",
            };
            let _ = writeln!(
                out,
                "{:<7} {:<15} {:<18} {:<20} {}",
                conn,
                rule.database,
                rule.user,
                rule.address.as_deref().unwrap_or(""),
                rule.method.as_str()
            );
        }

        if !self.extra.is_empty() {
            out.push_str("\n# --- from spec.postgresql.pg_hba ---\n");
            for line in &self.extra {
                let _ = writeln!(out, "{line}");
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rule lines, with comments and the header stripped.
    fn rule_lines(conf: &str) -> Vec<&str> {
        conf.lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .collect()
    }

    #[test]
    fn local_connections_use_peer_and_never_a_password() {
        let conf = HbaConfig::new("streaming_replica").render();
        let local = rule_lines(&conf)
            .into_iter()
            .find(|l| l.starts_with("local"))
            .expect("a local rule");
        assert!(local.ends_with("peer"), "got: {local}");
    }

    #[test]
    fn no_tcp_rule_is_ever_trust() {
        // A `trust` rule on a host line means anything that can reach the
        // port is a superuser.
        let conf = HbaConfig::new("streaming_replica")
            .with_network("10.89.0.0/24")
            .render();
        for line in rule_lines(&conf) {
            if line.starts_with("host") {
                assert!(
                    line.ends_with("scram-sha-256"),
                    "non-scram host rule: {line}"
                );
            }
        }
    }

    #[test]
    fn a_single_instance_cluster_gets_no_network_rules() {
        // Guessing a CIDR would produce a rule that is either useless or
        // wider than intended.
        let conf = HbaConfig::new("streaming_replica").render();
        assert!(
            !conf.contains("replication"),
            "no replication rule expected:\n{conf}"
        );
        for line in rule_lines(&conf) {
            assert!(
                !line.contains("10.") && !line.contains("0.0.0.0"),
                "unexpected network rule: {line}"
            );
        }
    }

    #[test]
    fn replication_needs_its_own_rule_because_all_does_not_cover_it() {
        let conf = HbaConfig::new("streaming_replica")
            .with_network("10.89.0.0/24")
            .render();
        let repl = rule_lines(&conf)
            .into_iter()
            .find(|l| l.contains("replication"))
            .expect("a replication rule");
        assert!(repl.contains("streaming_replica"), "got: {repl}");
        assert!(repl.contains("10.89.0.0/24"), "got: {repl}");
        assert!(repl.ends_with("scram-sha-256"), "got: {repl}");
    }

    #[test]
    fn the_replication_role_name_is_not_hardcoded() {
        let conf = HbaConfig::new("custom_repl")
            .with_network("10.89.0.0/24")
            .render();
        assert!(conf.contains("custom_repl"));
    }

    #[test]
    fn rendering_is_deterministic() {
        let c = HbaConfig::new("streaming_replica").with_network("10.89.0.0/24");
        assert_eq!(c.render(), c.render());
    }

    #[test]
    fn extra_rules_land_after_the_managed_ones() {
        // Appending means an operator rule cannot shadow a managed one,
        // since first match wins.
        let mut c = HbaConfig::new("streaming_replica");
        c.extra
            .push("host all readonly 10.0.0.0/8 scram-sha-256".into());
        let conf = c.render();
        let managed = conf.find("local").unwrap();
        let extra = conf.find("readonly").unwrap();
        assert!(managed < extra, "extra rules must come last:\n{conf}");
    }
}
