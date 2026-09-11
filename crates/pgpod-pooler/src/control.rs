//! The pooler agent's control protocol.
//!
//! One JSON request per connection, one JSON response — the wire between
//! the daemon (through `podman exec`) and PID 1 inside a pooler container.
//!
//! It lives here rather than in `pgpod-agent` because both ends need it
//! and `pgpod-agent` is a binary: without a shared home, the daemon would
//! be reduced to picking fields out of untyped JSON, and the two ends
//! would be free to drift. Pure data, like everything else in this crate.

use pgpod_core::HumanDuration;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Hold one cluster's pools. Always deadlined.
    Pause {
        cluster: String,
        /// How long PID 1 will hold before releasing on its own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hold: Option<HumanDuration>,
    },
    /// Release one cluster's pools.
    Resume { cluster: String },
    /// Recycle backend connections for one cluster's pools.
    Reconnect { cluster: String },
    /// `SHOW POOLS`.
    Pools,
    /// Re-read the config from disk (`RELOAD`).
    ///
    /// Note what this cannot do: `query_wait_timeout` is captured when
    /// pools are constructed and is not re-read, so the hold budget is
    /// fixed at container-create time (ADR 05 §3).
    Reload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Ok {
        /// Pools the command actually applied to.
        pools: Vec<String>,
    },
    Pools {
        pools: Vec<PoolStatus>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PoolStatus {
    pub database: String,
    pub user: String,
    pub paused: bool,
    pub clients_waiting: u64,
    pub servers_active: u64,
    pub servers_idle: u64,
}

impl Response {
    /// The pools, or the error the pooler reported.
    pub fn into_pools(self) -> Result<Vec<PoolStatus>, crate::Error> {
        match self {
            Self::Pools { pools } => Ok(pools),
            Self::Error { message } => Err(crate::Error::Protocol(message)),
            Self::Ok { .. } => Err(crate::Error::Protocol(
                "asked the pooler for its pools and got an acknowledgement".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_as_the_daemon_writes_them() {
        for request in [
            Request::Pause {
                cluster: "mydb".into(),
                hold: Some(HumanDuration::from_secs(90)),
            },
            Request::Pause {
                cluster: "mydb".into(),
                hold: None,
            },
            Request::Resume {
                cluster: "mydb".into(),
            },
            Request::Reconnect {
                cluster: "mydb".into(),
            },
            Request::Pools,
            Request::Reload,
        ] {
            let encoded = serde_json::to_string(&request).unwrap();
            assert_eq!(
                serde_json::from_str::<Request>(&encoded).unwrap(),
                request,
                "{encoded} did not round trip"
            );
        }
    }

    #[test]
    fn a_hold_crosses_the_wire_with_its_unit() {
        // It becomes a budget on the other side; a bare number would be
        // milliseconds, and a 90-second hold would last 90ms.
        let encoded = serde_json::to_string(&Request::Pause {
            cluster: "mydb".into(),
            hold: Some(HumanDuration::from_secs(90)),
        })
        .unwrap();
        assert!(encoded.contains("\"90s\""), "{encoded}");
    }

    #[test]
    fn responses_round_trip_and_pools_come_back_typed() {
        let response = Response::Pools {
            pools: vec![PoolStatus {
                database: "appdb".into(),
                user: "app".into(),
                paused: true,
                clients_waiting: 3,
                servers_active: 1,
                servers_idle: 2,
            }],
        };
        let encoded = serde_json::to_string(&response).unwrap();
        let decoded: Response = serde_json::from_str(&encoded).unwrap();
        let pools = decoded.into_pools().unwrap();
        assert_eq!(pools[0].database, "appdb");
        assert!(pools[0].paused);
    }

    #[test]
    fn an_error_response_surfaces_as_an_error_not_an_empty_list() {
        // An empty pool list and a failed command look identical to a
        // caller that only reads `pools`, and the difference is whether
        // the pooler is fine or unreachable.
        let response = Response::Error {
            message: "no pool for database \"appdb\"".into(),
        };
        assert!(response.into_pools().is_err());
    }
}
