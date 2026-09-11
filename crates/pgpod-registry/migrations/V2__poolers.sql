-- Poolers, and which clusters each one fronts.
--
-- A pooler is its own object with its own name and lifecycle (ADR 05 §2),
-- not a column on `clusters`: one pooler may front several clusters, and
-- the reference points from the pooler outwards so that applying a cluster
-- manifest can never reconfigure something it does not name.

CREATE TABLE poolers (
    name           TEXT PRIMARY KEY,
    -- The applied manifest, verbatim, as JSON — same reasoning as
    -- `clusters.manifest`: a field added later needs no migration to be
    -- round-tripped.
    manifest       TEXT NOT NULL,
    generation     INTEGER NOT NULL DEFAULT 1,
    container_name TEXT NOT NULL,
    -- NULL between the durable row insert and podman returning an id.
    container_id   TEXT,
    -- Persisted so a recreated pooler keeps the port applications are
    -- already pointed at.
    host_port      INTEGER NOT NULL,
    phase          TEXT NOT NULL,
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
) STRICT;

-- One row per pool: which cluster it reaches, which database on it, and
-- the name clients address it by.
--
-- ON DELETE RESTRICT on the cluster side, CASCADE on the pooler side.
-- Deleting a pooler takes its own rows with it; deleting a cluster out
-- from under a live pooler is refused, so `pgpod delete` can name the
-- pooler instead of silently leaving it pointed at nothing.
CREATE TABLE pooler_clusters (
    pooler    TEXT NOT NULL REFERENCES poolers(name) ON DELETE CASCADE,
    cluster   TEXT NOT NULL REFERENCES clusters(name) ON DELETE RESTRICT,
    database  TEXT NOT NULL,
    -- What a client puts in `dbname`. Unique per pooler, which is what
    -- this primary key enforces durably rather than only at parse time.
    pool_name TEXT NOT NULL,
    PRIMARY KEY (pooler, pool_name)
) STRICT;

CREATE INDEX pooler_clusters_by_cluster ON pooler_clusters(cluster);
