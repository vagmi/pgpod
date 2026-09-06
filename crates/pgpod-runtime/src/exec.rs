//! Running commands inside a container.
//!
//! Used for health probes, `pgpod psql`, `pg_ctl promote`, and the SQL the
//! agent runs during bootstrap. Most callers want the collected result, so
//! that is what [`Container::exec`] returns; streaming is a later concern
//! for `pgpod logs -f` and long-running restores.

use futures::StreamExt;
use podman_api::conn::TtyChunk;
use podman_api::opts::{ExecCreateOpts, ExecStartOpts, UserOpt};

use crate::container::Container;
use crate::error::{Error, Result};

/// A command to run inside an already-running container.
#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub command: Vec<String>,
    pub working_dir: Option<String>,
    pub env: Vec<(String, String)>,
    /// Run as a specific user (`"999"` or `"postgres"`).
    pub user: Option<String>,
}

impl ExecSpec {
    pub fn new<I, S>(command: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            command: command.into_iter().map(Into::into).collect(),
            working_dir: None,
            env: Vec::new(),
            user: None,
        }
    }

    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }
}

/// Result of a completed exec.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    /// `None` when podman produced no inspectable exit code, which is
    /// distinct from zero and must not be treated as success.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// The output as a `Result`, with stderr in the error.
    ///
    /// Most callers want this: an exec that "succeeded" with exit code 1
    /// is a bug waiting to be attributed to something else entirely.
    pub fn require_success(self, what: &str) -> Result<Self> {
        if self.success() {
            return Ok(self);
        }
        Err(Error::Exec(format!(
            "{what} exited with {}: {}",
            self.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "no exit code".into()),
            // Some tools report to stdout; include both so the message is
            // never mysteriously empty.
            if self.stderr.trim().is_empty() {
                self.stdout.trim()
            } else {
                self.stderr.trim()
            }
        )))
    }
}

impl Container {
    /// Run `spec` inside the container and collect its output.
    ///
    /// TTY allocation is deliberately off: with a TTY the kernel merges
    /// stdout and stderr into one stream and they cannot be demultiplexed,
    /// which would make every error message ambiguous about its source.
    pub async fn exec(&self, spec: &ExecSpec) -> Result<ExecOutput> {
        let mut b = ExecCreateOpts::builder()
            .command(spec.command.clone())
            .attach_stdout(true)
            .attach_stderr(true);
        if let Some(wd) = &spec.working_dir {
            b = b.working_dir(wd);
        }
        if !spec.env.is_empty() {
            b = b.env(
                spec.env
                    .iter()
                    .cloned()
                    .collect::<std::collections::HashMap<_, _>>(),
            );
        }
        if let Some(user) = &spec.user {
            b = b.user(UserOpt::User(user.clone()));
        }

        let containers = self.client_ref().podman().containers();
        let exec = containers
            .get(self.id())
            .create_exec(&b.build())
            .await
            .map_err(|e| Error::Exec(format!("create exec: {e}")))?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let start_opts = ExecStartOpts::builder().build();
        if let Some(mut stream) = exec
            .start(&start_opts)
            .await
            .map_err(|e| Error::Exec(format!("start exec: {e}")))?
        {
            while let Some(chunk) = stream.next().await {
                match chunk.map_err(|e| Error::Exec(format!("read exec output: {e}")))? {
                    TtyChunk::StdOut(b) => stdout.extend_from_slice(&b),
                    TtyChunk::StdErr(b) => stderr.extend_from_slice(&b),
                    TtyChunk::StdIn(_) => {}
                }
            }
            // The stream borrows `exec`; it must drop before inspect.
        }

        let inspect = exec
            .inspect()
            .await
            .map_err(|e| Error::Exec(format!("inspect exec: {e}")))?;
        let exit_code = inspect
            .get("ExitCode")
            .and_then(|v| v.as_i64())
            .map(|c| c as i32);

        Ok(ExecOutput {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(code: Option<i32>, out: &str, err: &str) -> ExecOutput {
        ExecOutput {
            exit_code: code,
            stdout: out.into(),
            stderr: err.into(),
        }
    }

    #[test]
    fn a_missing_exit_code_is_not_success() {
        // Treating "podman told us nothing" as zero would silently pass
        // over a failed promote or a failed initdb.
        assert!(!output(None, "", "").success());
        assert!(output(Some(0), "", "").success());
        assert!(!output(Some(1), "", "").success());
    }

    #[test]
    fn require_success_carries_stderr_into_the_error() {
        let e = output(Some(1), "", "FATAL: database does not exist")
            .require_success("psql")
            .unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("psql"), "{msg}");
        assert!(msg.contains("database does not exist"), "{msg}");
    }

    #[test]
    fn require_success_falls_back_to_stdout_when_stderr_is_empty() {
        // pg_ctl and initdb report some failures on stdout; an empty error
        // message is the worst possible diagnostic.
        let e = output(Some(2), "could not locate a valid checkpoint record", "")
            .require_success("pg_ctl")
            .unwrap_err();
        assert!(e.to_string().contains("checkpoint record"), "{e}");
    }

    #[test]
    fn tty_is_never_requested() {
        // Documented as a test because the consequence is subtle: with a
        // TTY the kernel merges stdout and stderr irreversibly.
        let spec = ExecSpec::new(["true"]);
        assert!(spec.user.is_none());
        assert!(spec.env.is_empty());
    }
}
