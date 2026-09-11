//! A minimal HTTP/1.1 client over the podman unix socket.
//!
//! This exists for exactly one reason, and it is worth stating plainly so
//! nobody generalises it: **`podman-api` cannot set `no_new_privileges`.**
//!
//! Its builder spells the field `no_new_privilages` (a typo present in
//! 0.10 and still in 0.11), which serializes to a key libpod does not
//! recognise and silently ignores. The flag never lands. Every other
//! hardening control in `AGENTS.md` applies correctly; this one did not,
//! and `tests/hardening.rs` caught it on its first run — which is the
//! whole reason ADR 00 §8 said to assert hardening via inspect rather
//! than trust the builder.
//!
//! Rather than replace `podman-api` wholesale, container creation
//! serializes the options `podman-api` built, patches the one key, and
//! posts the result here. The crate keeps doing all the schema work; this
//! module is ~100 lines of transport. That is ADR 00 §1's stated
//! mitigation — "the blast radius is this directory" — being cashed in for
//! the smallest possible amount.
//!
//! **A second endpoint now lives here, and that is a signal.** `podman-api`
//! also cannot create a secret: `Secrets::create` posts
//! `serde_json::to_string(&secret)`, so libpod stores the JSON *encoding*
//! of the payload — surrounding quotes, and `\n` as two literal
//! characters — where it wants the raw bytes. Every pgpod secret was
//! stored that way since Phase 1, undetected because it is
//! self-consistent: `initdb` set the password from the quoted file and
//! later connections read the same quoted file back. It surfaced the
//! moment something *else* had to parse a secret, and it means a secret an
//! operator creates by hand with `podman secret create` does not match one
//! pgpod creates.
//!
//! Two independent defects in the same library, both silent, both found by
//! a test rather than by reading. Per ADR 00 §8's own warning, the next
//! one is the point at which to replace `podman-api` rather than keep
//! patching around it — recorded in ROADMAP.
//!
//! **A third endpoint, and this one is a gap rather than a defect.**
//! Reading a secret back needs `?showsecret=true`, and `podman-api`'s
//! `Secret::inspect` requests `/libpod/secrets/{id}/json` without it, so
//! the response never carries `SecretData`. Nothing is wrong with the
//! crate; it simply does not expose the query parameter, and the endpoint
//! is a plain GET that shares the response parser below.
//!
//! It is needed because a restored cluster carries the *source's* roles
//! verbatim, so its credentials have to be adopted rather than generated
//! (ADR 04 §8).
//!
//! Do not add endpoints here casually.

use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::error::{Error, Result};

/// POST a JSON body to the libpod socket and return `(status, body)`.
pub(crate) async fn post_json(socket: &Path, path: &str, body: &str) -> Result<(u16, String)> {
    post(socket, path, "application/json", body.as_bytes()).await
}

/// POST raw bytes, with the content type the endpoint expects.
///
/// `/libpod/secrets/create` takes the secret's bytes as the body, not a
/// JSON document containing them.
pub(crate) async fn post_bytes(socket: &Path, path: &str, body: &[u8]) -> Result<(u16, String)> {
    post(socket, path, "application/octet-stream", body).await
}

/// GET from the libpod socket and return `(status, body)`.
///
/// Callers branch on the status: 404 is a normal answer for "no such
/// object", not a transport failure.
pub(crate) async fn get(socket: &Path, path: &str) -> Result<(u16, String)> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| Error::Unreachable(format!("connect {}: {e}", socket.display())))?;

    let head = format!("GET {path} HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| Error::Unreachable(format!("write request: {e}")))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .map_err(|e| Error::Unreachable(format!("read response: {e}")))?;

    parse_response(&raw)
}

async fn post(socket: &Path, path: &str, content_type: &str, body: &[u8]) -> Result<(u16, String)> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| Error::Unreachable(format!("connect {}: {e}", socket.display())))?;

    // `Connection: close` lets us read to EOF instead of tracking
    // keep-alive framing. This is a one-shot call per container create, so
    // the cost of a fresh connection is irrelevant next to the pull and
    // start that follow.
    let head = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: d\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        len = body.len(),
    );

    // Head and body written separately because the body is bytes, not
    // necessarily UTF-8 — a secret is whatever the operator put in it.
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| Error::Unreachable(format!("write request: {e}")))?;
    stream
        .write_all(body)
        .await
        .map_err(|e| Error::Unreachable(format!("write body: {e}")))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .map_err(|e| Error::Unreachable(format!("read response: {e}")))?;

    parse_response(&raw)
}

/// Split an HTTP response into status and decoded body.
///
/// Kept separate from the socket so it can be unit-tested without podman —
/// framing bugs here would surface as bizarre "container created but no
/// id" errors.
fn parse_response(raw: &[u8]) -> Result<(u16, String)> {
    let split = find_header_end(raw)
        .ok_or_else(|| Error::Unreachable("malformed response: no header terminator".into()))?;
    let (head, body) = raw.split_at(split);
    let body = &body[4..]; // skip the \r\n\r\n itself

    let head = String::from_utf8_lossy(head);
    let mut lines = head.lines();

    let status_line = lines
        .next()
        .ok_or_else(|| Error::Unreachable("malformed response: no status line".into()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Unreachable(format!("malformed status line: {status_line:?}")))?;

    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });

    let body = if chunked {
        decode_chunked(body)?
    } else {
        body.to_vec()
    };

    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Decode `Transfer-Encoding: chunked`. Podman uses it for some responses
/// and not others, and guessing wrong leaves chunk-size lines embedded in
/// what should be JSON.
fn decode_chunked(mut input: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = input
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| Error::Unreachable("truncated chunk header".into()))?;
        let size_line = String::from_utf8_lossy(&input[..line_end]);
        // A chunk header may carry extensions after a ';'.
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| Error::Unreachable(format!("bad chunk size {size_hex:?}")))?;

        input = &input[line_end + 2..];
        if size == 0 {
            break;
        }
        if input.len() < size {
            return Err(Error::Unreachable("truncated chunk body".into()));
        }
        out.extend_from_slice(&input[..size]);
        // Skip the CRLF that terminates the chunk body.
        input = input.get(size + 2..).unwrap_or(&[]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_content_length_response() {
        let raw = b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"Id\":\"abc\"}\n";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 201);
        assert!(body.starts_with("{\"Id\":\"abc\"}"));
    }

    #[test]
    fn parses_a_chunked_response() {
        // Podman uses chunked for some responses; leaving the framing in
        // would corrupt the JSON in a way that reads as a podman bug.
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"Id\":\"\r\n5\r\nabc\"}\r\n0\r\n\r\n";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "{\"Id\":\"abc\"}");
    }

    #[test]
    fn handles_chunk_extensions() {
        let raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3;foo=bar\r\nabc\r\n0\r\n\r\n";
        let (_, body) = parse_response(raw).unwrap();
        assert_eq!(body, "abc");
    }

    #[test]
    fn surfaces_error_statuses_rather_than_guessing() {
        let raw = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 22\r\n\r\n{\"cause\":\"it broke\"}\r\n";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 500);
        assert!(body.contains("it broke"));
    }

    #[test]
    fn rejects_a_response_with_no_header_terminator() {
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 0").is_err());
    }

    #[test]
    fn rejects_a_malformed_status_line() {
        assert!(parse_response(b"NOT-HTTP\r\n\r\n").is_err());
    }

    #[test]
    fn case_insensitive_transfer_encoding_header() {
        let raw = b"HTTP/1.1 200 OK\r\nTRANSFER-ENCODING: Chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n";
        let (_, body) = parse_response(raw).unwrap();
        assert_eq!(body, "abc");
    }
}
