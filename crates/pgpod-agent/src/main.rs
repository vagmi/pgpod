//! pgpod instance manager — PID 1 inside the PostgreSQL container.
//!
//! Not yet implemented — landing in **Phase 1** (see `ROADMAP.md`).
//!
//! This binary is statically linked against musl and bind-mounted into
//! every instance container, so its dependency list is load-bearing: it
//! must never grow `podman-api`, `rusqlite`, or anything pulling in
//! OpenSSL (`adrs/00-project-setup.md` §3).

fn main() -> anyhow::Result<()> {
    anyhow::bail!("pgpod-agent is not implemented yet (Phase 1)")
}
