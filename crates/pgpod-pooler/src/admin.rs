//! pg_doorman's admin console, as bytes.
//!
//! The console speaks the PostgreSQL wire protocol and authenticates with
//! **MD5 and nothing else** — `auth/mod.rs:128 authenticate_admin` issues
//! an `AuthenticationMD5Password` challenge and compares the response
//! (ADR 05 §6). That is a small enough surface to encode here rather than
//! pull a PostgreSQL client into a workspace that has never needed one,
//! and it keeps the agent a static musl binary with no TLS stack.
//!
//! Everything in this module is pure: messages in, messages out. The
//! socket belongs to `pgpod-agent`, which is the only thing that talks to
//! a pooler.
//!
//! Only the subset pgpod uses is decoded. Anything else is reported as
//! [`Backend::Other`] and skipped rather than treated as an error — a
//! `ParameterStatus` or `NoticeResponse` arriving where it was not
//! expected is normal, and a client that failed on one would be broken by
//! any upstream that decided to say more.

use md5::{Digest, Md5};

use crate::Error;

/// Protocol version 3.0, the only one PostgreSQL has spoken since 7.4.
const PROTOCOL_VERSION: i32 = 196_608;

/// A frontend startup message.
///
/// No `password` yet — the server names the method it wants, and for the
/// admin console that is always MD5.
pub fn startup_message(user: &str, database: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    for (k, v) in [
        ("user", user),
        ("database", database),
        // Named so `SHOW CLIENTS` on a pooler shows who is holding it.
        ("application_name", "pgpod-agent"),
    ] {
        body.extend_from_slice(k.as_bytes());
        body.push(0);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);

    // A startup message has no type byte; the length covers itself.
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// `md5(md5(password + user) + salt)`, hex, with the literal `md5` prefix
/// PostgreSQL expects.
pub fn md5_password(user: &str, password: &str, salt: [u8; 4]) -> String {
    let mut inner = Md5::new();
    inner.update(password.as_bytes());
    inner.update(user.as_bytes());
    let inner = hex::encode(inner.finalize());

    let mut outer = Md5::new();
    outer.update(inner.as_bytes());
    outer.update(salt);
    format!("md5{}", hex::encode(outer.finalize()))
}

/// A `PasswordMessage` carrying the MD5 response.
pub fn password_message(user: &str, password: &str, salt: [u8; 4]) -> Vec<u8> {
    tagged(b'p', md5_password(user, password, salt).as_bytes())
}

/// A simple `Query`.
pub fn query_message(sql: &str) -> Vec<u8> {
    tagged(b'Q', sql.as_bytes())
}

/// A polite goodbye, so the pooler logs a disconnect rather than a reset.
pub fn terminate_message() -> Vec<u8> {
    tagged(b'X', b"")
}

/// Type byte, length (covering itself), body, NUL terminator.
fn tagged(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 6);
    out.push(tag);
    out.extend_from_slice(&((body.len() + 5) as i32).to_be_bytes());
    out.extend_from_slice(body);
    out.push(0);
    out
}

/// The backend messages pgpod acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    AuthenticationOk,
    AuthenticationMd5 {
        salt: [u8; 4],
    },
    /// Any other authentication method. Carried rather than collapsed into
    /// an error so the message can name what was asked for.
    AuthenticationOther(i32),
    RowDescription(Vec<String>),
    DataRow(Vec<Option<String>>),
    CommandComplete(String),
    ErrorResponse(String),
    ReadyForQuery,
    /// Decoded far enough to skip: ParameterStatus, BackendKeyData,
    /// NoticeResponse, and anything added upstream later.
    Other(u8),
}

/// Decode one message from the front of `buf`, consuming it.
///
/// `Ok(None)` means "not a whole message yet, read more" — the caller
/// cannot tell that from a protocol error any other way, and conflating
/// the two turns a short read into a spurious failure.
pub fn decode(buf: &mut Vec<u8>) -> Result<Option<Backend>, Error> {
    if buf.len() < 5 {
        return Ok(None);
    }
    let tag = buf[0];
    let len = i32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
    if !(4..=1_073_741_823).contains(&len) {
        return Err(Error::Protocol(format!(
            "message {:?} declares a length of {len}",
            tag as char
        )));
    }
    let total = 1 + len as usize;
    if buf.len() < total {
        return Ok(None);
    }
    let body = buf[5..total].to_vec();
    buf.drain(..total);
    parse(tag, &body).map(Some)
}

fn parse(tag: u8, body: &[u8]) -> Result<Backend, Error> {
    match tag {
        b'R' => {
            if body.len() < 4 {
                return Err(Error::Protocol("truncated authentication request".into()));
            }
            let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
            match code {
                0 => Ok(Backend::AuthenticationOk),
                5 => {
                    if body.len() < 8 {
                        return Err(Error::Protocol("MD5 request carries no salt".into()));
                    }
                    Ok(Backend::AuthenticationMd5 {
                        salt: [body[4], body[5], body[6], body[7]],
                    })
                }
                other => Ok(Backend::AuthenticationOther(other)),
            }
        }
        b'T' => {
            if body.len() < 2 {
                return Err(Error::Protocol("truncated row description".into()));
            }
            let count = u16::from_be_bytes([body[0], body[1]]) as usize;
            let mut rest = &body[2..];
            let mut names = Vec::with_capacity(count);
            for _ in 0..count {
                let (name, tail) = take_cstr(rest)?;
                names.push(name);
                // Each field carries 18 bytes of table/type metadata that
                // pgpod has no use for.
                if tail.len() < 18 {
                    return Err(Error::Protocol("truncated field description".into()));
                }
                rest = &tail[18..];
            }
            Ok(Backend::RowDescription(names))
        }
        b'D' => {
            if body.len() < 2 {
                return Err(Error::Protocol("truncated data row".into()));
            }
            let count = u16::from_be_bytes([body[0], body[1]]) as usize;
            let mut rest = &body[2..];
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                if rest.len() < 4 {
                    return Err(Error::Protocol("truncated column length".into()));
                }
                let len = i32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
                rest = &rest[4..];
                if len < 0 {
                    values.push(None);
                    continue;
                }
                let len = len as usize;
                if rest.len() < len {
                    return Err(Error::Protocol("truncated column value".into()));
                }
                values.push(Some(String::from_utf8_lossy(&rest[..len]).into_owned()));
                rest = &rest[len..];
            }
            Ok(Backend::DataRow(values))
        }
        b'C' => Ok(Backend::CommandComplete(take_cstr(body)?.0)),
        b'E' => Ok(Backend::ErrorResponse(error_text(body))),
        b'Z' => Ok(Backend::ReadyForQuery),
        other => Ok(Backend::Other(other)),
    }
}

/// The human-readable part of an `ErrorResponse`.
///
/// Fields are `<type byte><value>\0` pairs. `M` is the message and `S` the
/// severity; anything else is detail pgpod would only be repeating back.
fn error_text(body: &[u8]) -> String {
    let mut severity = None;
    let mut message = None;
    let mut rest = body;
    while let Some((&field, tail)) = rest.split_first() {
        if field == 0 {
            break;
        }
        let Ok((value, next)) = take_cstr(tail) else {
            break;
        };
        match field {
            b'S' => severity = Some(value),
            b'M' => message = Some(value),
            _ => {}
        }
        rest = next;
    }
    match (severity, message) {
        (Some(s), Some(m)) => format!("{s}: {m}"),
        (_, Some(m)) => m,
        _ => "the pooler reported an error with no message".to_string(),
    }
}

fn take_cstr(buf: &[u8]) -> Result<(String, &[u8]), Error> {
    let end = buf
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::Protocol("unterminated string".into()))?;
    Ok((
        String::from_utf8_lossy(&buf[..end]).into_owned(),
        &buf[end + 1..],
    ))
}

/// The rows one command produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    /// e.g. `PAUSE`, `RESUME`, `SHOW`.
    pub tag: Option<String>,
}

impl QueryResult {
    /// One cell, by column name. `None` for an absent column or a NULL.
    pub fn get<'a>(&'a self, row: usize, column: &str) -> Option<&'a str> {
        let index = self.columns.iter().position(|c| c == column)?;
        self.rows.get(row)?.get(index)?.as_deref()
    }

    /// Every value in one column.
    pub fn column<'a>(&'a self, column: &str) -> Vec<&'a str> {
        let Some(index) = self.columns.iter().position(|c| c == column) else {
            return Vec::new();
        };
        self.rows
            .iter()
            .filter_map(|r| r.get(index).and_then(|v| v.as_deref()))
            .collect()
    }
}

/// One row of `SHOW POOLS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolRow {
    pub database: String,
    pub user: String,
    pub paused: bool,
    pub clients_waiting: u64,
    pub servers_active: u64,
    pub servers_idle: u64,
}

/// Parse `SHOW POOLS`.
///
/// By column **name**, never by position: the column list is upstream's to
/// change, and pgpod reading `cl_waiting` out of whatever ended up fifth
/// would be wrong silently. Columns pgpod does not know about are ignored;
/// ones it needs and cannot find are an error rather than a zero.
pub fn parse_pools(result: &QueryResult) -> Result<Vec<PoolRow>, Error> {
    for required in ["database", "user", "paused"] {
        if !result.columns.iter().any(|c| c == required) {
            return Err(Error::Protocol(format!(
                "SHOW POOLS has no {required:?} column — got {:?}",
                result.columns
            )));
        }
    }
    let mut out = Vec::with_capacity(result.rows.len());
    for i in 0..result.rows.len() {
        let cell = |name: &str| result.get(i, name).unwrap_or("").trim().to_string();
        let number = |name: &str| cell(name).parse::<u64>().unwrap_or(0);
        out.push(PoolRow {
            database: cell("database"),
            user: cell("user"),
            // pg_doorman renders this as 0/1; accept the booleans a future
            // version might print instead.
            paused: matches!(cell("paused").as_str(), "1" | "t" | "true" | "yes"),
            clients_waiting: number("cl_waiting"),
            servers_active: number("sv_active"),
            servers_idle: number("sv_idle"),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a backend message the way a server would.
    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    fn cstr(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }

    fn row_description(names: &[&str]) -> Vec<u8> {
        let mut body = (names.len() as u16).to_be_bytes().to_vec();
        for n in names {
            body.extend_from_slice(&cstr(n));
            body.extend_from_slice(&[0u8; 18]);
        }
        frame(b'T', &body)
    }

    fn data_row(values: &[Option<&str>]) -> Vec<u8> {
        let mut body = (values.len() as u16).to_be_bytes().to_vec();
        for v in values {
            match v {
                None => body.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(s) => {
                    body.extend_from_slice(&(s.len() as i32).to_be_bytes());
                    body.extend_from_slice(s.as_bytes());
                }
            }
        }
        frame(b'D', &body)
    }

    #[test]
    fn md5_matches_the_documented_construction() {
        // md5(md5(password + user) + salt), hex, prefixed "md5". Computed
        // here against a hand-worked value so a refactor of the hashing
        // cannot quietly change what is sent.
        let got = md5_password("admin", "secret", [0x01, 0x02, 0x03, 0x04]);
        let inner = hex::encode(Md5::digest(b"secretadmin"));
        let mut outer = Md5::new();
        outer.update(inner.as_bytes());
        outer.update([0x01, 0x02, 0x03, 0x04]);
        assert_eq!(got, format!("md5{}", hex::encode(outer.finalize())));
        assert!(got.starts_with("md5") && got.len() == 35);
    }

    #[test]
    fn the_salt_changes_the_response() {
        // Otherwise a replayed response would authenticate, and the test
        // above would pass against an implementation that ignored it.
        assert_ne!(
            md5_password("admin", "secret", [1, 2, 3, 4]),
            md5_password("admin", "secret", [1, 2, 3, 5])
        );
    }

    #[test]
    fn a_startup_message_is_self_describing() {
        let msg = startup_message("pgpod_admin", "pgdoorman");
        let len = i32::from_be_bytes([msg[0], msg[1], msg[2], msg[3]]) as usize;
        assert_eq!(len, msg.len(), "the length must cover the whole message");
        assert_eq!(
            i32::from_be_bytes([msg[4], msg[5], msg[6], msg[7]]),
            PROTOCOL_VERSION
        );
        let text = String::from_utf8_lossy(&msg);
        assert!(text.contains("pgpod_admin") && text.contains("pgdoorman"));
        assert_eq!(
            *msg.last().unwrap(),
            0,
            "the parameter list is NUL-terminated"
        );
    }

    #[test]
    fn tagged_messages_declare_a_length_covering_themselves() {
        for msg in [
            query_message("SHOW POOLS"),
            password_message("admin", "pw", [1, 2, 3, 4]),
            terminate_message(),
        ] {
            let len = i32::from_be_bytes([msg[1], msg[2], msg[3], msg[4]]) as usize;
            assert_eq!(
                len + 1,
                msg.len(),
                "a tagged message's length covers everything after the tag"
            );
        }
    }

    #[test]
    fn decodes_the_md5_challenge_and_its_salt() {
        let mut body = 5i32.to_be_bytes().to_vec();
        body.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let mut buf = frame(b'R', &body);
        assert_eq!(
            decode(&mut buf).unwrap(),
            Some(Backend::AuthenticationMd5 {
                salt: [0xde, 0xad, 0xbe, 0xef]
            })
        );
        assert!(buf.is_empty(), "the frame must be consumed");
    }

    #[test]
    fn an_unexpected_auth_method_is_carried_not_collapsed() {
        // If pg_doorman ever asks for SASL, the failure has to say which
        // method it wanted — "authentication failed" would send whoever
        // reads it looking at passwords.
        let mut buf = frame(b'R', &10i32.to_be_bytes());
        assert_eq!(
            decode(&mut buf).unwrap(),
            Some(Backend::AuthenticationOther(10))
        );
    }

    #[test]
    fn a_partial_message_asks_for_more_rather_than_failing() {
        // The difference between a short read and a protocol error. A
        // client that conflated them would fail whenever a response
        // happened to straddle two packets.
        let full = row_description(&["database", "user"]);
        for cut in 0..full.len() {
            let mut buf = full[..cut].to_vec();
            assert_eq!(
                decode(&mut buf).unwrap(),
                None,
                "{cut} of {} bytes should be incomplete, not an error",
                full.len()
            );
            assert_eq!(buf.len(), cut, "an incomplete frame must not be consumed");
        }
    }

    #[test]
    fn decodes_a_whole_query_response_in_one_buffer() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&frame(b'S', &[b'x', 0, b'y', 0])); // ParameterStatus
        buf.extend_from_slice(&row_description(&["database", "paused"]));
        buf.extend_from_slice(&data_row(&[Some("appdb"), Some("1")]));
        buf.extend_from_slice(&data_row(&[Some("shop"), None]));
        buf.extend_from_slice(&frame(b'C', &cstr("SHOW")));
        buf.extend_from_slice(&frame(b'Z', b"I"));

        let mut seen = Vec::new();
        while let Some(m) = decode(&mut buf).unwrap() {
            seen.push(m);
        }
        assert!(buf.is_empty());
        assert_eq!(seen[0], Backend::Other(b'S'));
        assert_eq!(
            seen[1],
            Backend::RowDescription(vec!["database".into(), "paused".into()])
        );
        assert_eq!(
            seen[2],
            Backend::DataRow(vec![Some("appdb".into()), Some("1".into())])
        );
        assert_eq!(
            seen[3],
            Backend::DataRow(vec![Some("shop".into()), None]),
            "a NULL column is None, not an empty string"
        );
        assert_eq!(seen[4], Backend::CommandComplete("SHOW".into()));
        assert_eq!(seen[5], Backend::ReadyForQuery);
    }

    #[test]
    fn an_error_response_keeps_severity_and_message() {
        let mut body = Vec::new();
        body.extend_from_slice(b"SERROR\0");
        body.extend_from_slice(b"C42P01\0");
        body.extend_from_slice(b"MNo pool for database \"appdb\"\0");
        body.push(0);
        let mut buf = frame(b'E', &body);
        let Some(Backend::ErrorResponse(text)) = decode(&mut buf).unwrap() else {
            panic!("expected an error response");
        };
        assert_eq!(text, "ERROR: No pool for database \"appdb\"");
    }

    #[test]
    fn a_malformed_length_is_an_error_not_a_huge_allocation() {
        let mut buf = vec![b'D'];
        buf.extend_from_slice(&(-1i32).to_be_bytes());
        buf.extend_from_slice(b"junk");
        assert!(decode(&mut buf).is_err());
    }

    #[test]
    fn show_pools_is_read_by_column_name() {
        // Upstream owns this column list — pg_doorman 3.11 prints
        // eighteen columns and has added some before. Reading `paused`
        // out of whatever landed fifteenth would be wrong silently.
        let result = QueryResult {
            columns: vec![
                "database".into(),
                "user".into(),
                "pool_mode".into(),
                "cl_waiting".into(),
                "sv_active".into(),
                "sv_idle".into(),
                "paused".into(),
            ],
            rows: vec![
                vec![
                    Some("appdb".into()),
                    Some("app".into()),
                    Some("transaction".into()),
                    Some("3".into()),
                    Some("4".into()),
                    Some("36".into()),
                    Some("1".into()),
                ],
                vec![
                    Some("shop".into()),
                    Some("app".into()),
                    Some("transaction".into()),
                    Some("0".into()),
                    Some("0".into()),
                    Some("1".into()),
                    Some("0".into()),
                ],
            ],
            tag: Some("SHOW".into()),
        };
        let pools = parse_pools(&result).unwrap();
        assert_eq!(pools[0].database, "appdb");
        assert!(pools[0].paused);
        assert_eq!(pools[0].clients_waiting, 3);
        assert_eq!(pools[0].servers_idle, 36);
        assert!(!pools[1].paused);
    }

    #[test]
    fn reordering_the_columns_changes_nothing() {
        let mut columns = vec!["paused".to_string(), "database".into(), "user".into()];
        let mut rows = vec![vec![
            Some("1".to_string()),
            Some("appdb".into()),
            Some("app".into()),
        ]];
        let a = parse_pools(&QueryResult {
            columns: columns.clone(),
            rows: rows.clone(),
            tag: None,
        })
        .unwrap();
        columns.swap(0, 2);
        rows[0].swap(0, 2);
        let b = parse_pools(&QueryResult {
            columns,
            rows,
            tag: None,
        })
        .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_missing_required_column_is_an_error_not_a_default() {
        // `paused: false` invented from a missing column would tell the
        // daemon a hold succeeded when nothing was held.
        let err = parse_pools(&QueryResult {
            columns: vec!["database".into(), "user".into()],
            rows: vec![vec![Some("appdb".into()), Some("app".into())]],
            tag: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("paused"), "{err}");
    }

    #[test]
    fn boolean_spellings_are_all_understood() {
        for (raw, expected) in [
            ("1", true),
            ("t", true),
            ("true", true),
            ("0", false),
            ("f", false),
            ("", false),
        ] {
            let pools = parse_pools(&QueryResult {
                columns: vec!["database".into(), "user".into(), "paused".into()],
                rows: vec![vec![Some("a".into()), Some("b".into()), Some(raw.into())]],
                tag: None,
            })
            .unwrap();
            assert_eq!(pools[0].paused, expected, "{raw:?}");
        }
    }
}
