//! Talking to pg_doorman's admin console.
//!
//! The codec lives in `pgpod-pooler`; this is the socket around it. Over
//! loopback *inside* the pooler container — the console is on the same
//! port clients use, and connecting from in here means the admin password
//! never crosses the cluster network.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use pgpod_pooler::{Backend, QueryResult};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A connection to the admin console, good for one batch of commands.
pub struct Admin {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Admin {
    /// Connect and authenticate.
    pub async fn connect(port: u16, user: &str, password: &str, database: &str) -> Result<Self> {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .with_context(|| format!("could not reach the admin console on port {port}"))?;
        let mut admin = Self {
            stream,
            buf: Vec::new(),
        };
        admin.authenticate(user, password, database).await?;
        Ok(admin)
    }

    async fn authenticate(&mut self, user: &str, password: &str, database: &str) -> Result<()> {
        self.send(&pgpod_pooler::startup_message(user, database))
            .await?;
        loop {
            match self.next_message().await? {
                Backend::AuthenticationMd5 { salt } => {
                    self.send(&pgpod_pooler::password_message(user, password, salt))
                        .await?;
                }
                Backend::AuthenticationOk => {}
                // Authentication is finished when the server says it is
                // ready, not when it stops talking about authentication —
                // ParameterStatus and BackendKeyData arrive in between.
                Backend::ReadyForQuery => return Ok(()),
                Backend::ErrorResponse(e) => {
                    bail!("the pooler refused the admin connection: {e}")
                }
                Backend::AuthenticationOther(code) => {
                    // pg_doorman's console is MD5-only today (ADR 05 §6).
                    // If that ever changes, say which method was asked for
                    // rather than leaving "authentication failed" to send
                    // someone looking at the password.
                    bail!(
                        "the pooler asked for authentication method {code}, and \
                         the agent implements only MD5 (method 5)"
                    )
                }
                _ => {}
            }
        }
    }

    /// Run one command and collect its rows.
    pub async fn execute(&mut self, sql: &str) -> Result<QueryResult> {
        self.send(&pgpod_pooler::query_message(sql)).await?;

        let mut result = QueryResult::default();
        let mut failure = None;
        loop {
            match self.next_message().await? {
                Backend::RowDescription(columns) => result.columns = columns,
                Backend::DataRow(values) => result.rows.push(values),
                Backend::CommandComplete(tag) => result.tag = Some(tag),
                // Kept, but the loop continues: the server still owes a
                // ReadyForQuery, and returning here would desynchronise
                // the stream for the next command on this connection.
                Backend::ErrorResponse(e) => failure = Some(e),
                Backend::ReadyForQuery => break,
                _ => {}
            }
        }
        match failure {
            Some(e) => bail!("{sql}: {e}"),
            None => Ok(result),
        }
    }

    /// Close politely, so the pooler logs a disconnect rather than a reset.
    pub async fn close(mut self) {
        let _ = self.send(&pgpod_pooler::terminate_message()).await;
        let _ = self.stream.shutdown().await;
    }

    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream
            .write_all(bytes)
            .await
            .context("failed to write to the admin console")
    }

    async fn next_message(&mut self) -> Result<Backend> {
        loop {
            if let Some(message) = pgpod_pooler::decode(&mut self.buf)? {
                return Ok(message);
            }
            let mut chunk = [0u8; 4096];
            // A bounded read: a pooler that accepts the connection and
            // then says nothing must not hang a pause that has a deadline
            // to keep.
            let n = tokio::time::timeout(Duration::from_secs(30), self.stream.read(&mut chunk))
                .await
                .context("the admin console stopped responding")?
                .context("failed to read from the admin console")?;
            if n == 0 {
                bail!("the admin console closed the connection mid-response");
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}
