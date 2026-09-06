-- pgpod registry, initial schema.
--
-- Deliberately narrow: this records pgpod's *intent* and what it last
-- observed. It is not a cache of podman's state — podman is asked directly
-- whenever the answer must be current (see the adopt-vs-reap path in
-- ADR 00 §7). Anything derivable from a live podman query does not belong
-- in a column here.

CREATE TABLE clusters (
    name          TEXT PRIMARY KEY,
    -- The applied manifest, verbatim, as JSON. Storing the whole thing
    -- rather than exploded columns means a manifest field added later
    -- needs no migration to be round-tripped.
    manifest      TEXT NOT NULL,
    -- Bumped on every apply that changes the manifest, so a reconciler can
    -- tell "spec changed" from "nothing to do" without diffing JSON.
    generation    INTEGER NOT NULL DEFAULT 1,
    phase         TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE instances (
    cluster          TEXT NOT NULL REFERENCES clusters(name) ON DELETE CASCADE,
    ordinal          INTEGER NOT NULL,
    container_name   TEXT NOT NULL,
    -- NULL between the durable row insert and podman returning an id.
    container_id     TEXT,
    volume_name      TEXT NOT NULL,
    -- Persisted so a recreated container keeps the port applications are
    -- already pointed at.
    host_port        INTEGER NOT NULL,
    phase            TEXT NOT NULL,
    -- Last observed role. Always re-read from the database before it is
    -- acted on (ADR 02 §7) — this column is for display, never a decision.
    role             TEXT NOT NULL DEFAULT 'unknown',
    timeline         INTEGER,
    last_probe_at_ms INTEGER,
    created_at_ms    INTEGER NOT NULL,
    PRIMARY KEY (cluster, ordinal)
) STRICT;

CREATE INDEX instances_by_container ON instances(container_name);

-- Populated from Phase 2. Created now because migrations are cheaper to
-- write together than to retrofit around live data.
CREATE TABLE backups (
    id            TEXT PRIMARY KEY,
    cluster       TEXT NOT NULL REFERENCES clusters(name) ON DELETE CASCADE,
    ordinal       INTEGER,
    destination   TEXT NOT NULL,
    timeline      INTEGER,
    begin_lsn     TEXT,
    end_lsn       TEXT,
    begin_wal     TEXT,
    end_wal       TEXT,
    size_bytes    INTEGER,
    status        TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    ended_at_ms   INTEGER
) STRICT;

CREATE INDEX backups_by_cluster ON backups(cluster, started_at_ms DESC);

-- Append-only. What `pgpod status` shows and what makes a half-finished
-- promote reconstructable after a crash.
CREATE TABLE events (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    cluster  TEXT NOT NULL,
    ordinal  INTEGER,
    at_ms    INTEGER NOT NULL,
    level    TEXT NOT NULL,
    message  TEXT NOT NULL
) STRICT;

CREATE INDEX events_by_cluster ON events(cluster, at_ms DESC);
