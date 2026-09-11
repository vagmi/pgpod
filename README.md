# pgpod

A PostgreSQL operator for **rootless Podman**.

pgpod takes CloudNativePG's model — a declarative manifest describing
instances, backups, and recovery, reconciled continuously by a controller —
and implements it against Podman instead of Kubernetes. For most
applications the operational value of CNPG is that model, not the
Kubernetes runtime carrying it.

> **Status: early but usable — single instance, with working backups.**
> A manifest brings up a PostgreSQL instance you can connect to, stop and
> restart without losing data; it archives WAL and takes base backups
> through [pgBackRest](https://pgbackrest.org/), and
> `pgpod restore --at <t>` reconstructs a cluster with exactly the rows as
> of `t`. Verified on Arch with podman 6.1 and on the deployment target,
> Ubuntu 26.04 with podman 5.7 — against a local repository and against
> [Garage](https://garagehq.deuxfleurs.fr/) over S3.
>
> A [pg_doorman](https://github.com/ozontech/pg_doorman) pooler can sit in
> front, which is what lets `pgpod apply --recreate` change a running
> cluster's configuration **without dropping a connection**.
>
> **Not yet:** replicas, failover, a reconciling daemon, or scheduled
> backups — `pgpod backup` is something you run. See
> [`ROADMAP.md`](ROADMAP.md).

## What works today

```yaml
apiVersion: pgpod/v1
kind: Cluster
metadata: { name: mydb }
spec:
  imageName: docker.io/library/postgres:18
  bootstrap:
    initdb: { database: appdb, owner: app }
  backup:
    destinations: [{ url: "s3://mydb-backups/mydb" }]
    retention: 14d
```

```sh
pgpod apply -f cluster.yaml
pgpod status mydb
pgpod backup mydb
pgpod backups mydb
pgpod restore mydb --at '2026-09-04T10:00:00Z' --as mydb-restored
```

Designed for clusters that can tolerate a small amount of downtime.

## Try it

```sh
cargo build --workspace
cargo run -p pgpod-cli -- doctor        # check the host first
```

`pgpod doctor` checks the rootless podman socket, server version, network
backend, cgroups, subuid ranges, and the graph root's free space. It exits
non-zero if something must be fixed and prints the command that fixes it.

Then bring up a cluster:

```sh
ops/build-agent.sh                      # static agent, mounted into the container
ops/build-pgbackrest.sh                 # pgBackRest bundle, likewise
eval "$(ops/dev-podman.sh start)"       # or use your own podman.socket

cat > cluster.yaml <<'YAML'
apiVersion: pgpod/v1
kind: Cluster
metadata:
  name: demo
spec:
  imageName: docker.io/library/postgres:18
  bootstrap:
    initdb:
      database: appdb
      owner: app
YAML

cargo run -p pgpod-cli -- apply -f cluster.yaml
cargo run -p pgpod-cli -- status demo
cargo run -p pgpod-cli -- psql demo
```

`pgpod delete demo` removes the container and **keeps the volume** — a
later `apply` reuses it, with the data intact and on the same port. Only
`--purge` destroys data.

### Backups

Backups are powered by pgBackRest. 

```sh
pgpod apply -f examples/backup-local.yaml   # repository in a podman volume
pgpod backup demo-backed-up
pgpod backups demo-backed-up                # what is actually restorable
pgpod restore demo-backed-up --as recovered --at '2026-09-08T12:00:00Z'
```

[`examples/backup-gcs.yaml`](examples/backup-gcs.yaml) is the same thing
against object storage. `s3://`, `gs://`, `az://` and `file://` all work;
on GCE no credential is needed at all, because pgBackRest authorizes with
the instance service account.

**You do not need a special image.** pgBackRest is dynamically linked, so
unlike pgpod's static agent it cannot be shipped as one file, instead
`ops/build-pgbackrest.sh` bundles it with its libraries and loader and
patches the ELF to find them, and pgpod bind-mounts that read-only.
Nothing from the surrounding image is used, so the same bundle runs on
Debian, on a CNPG-based image, and on Alpine.

A `file://` repository needs `spec.backup.volume`: inside a container with
a read-only root filesystem, a local path exists only if something is
mounted there. pgpod never deletes that volume — not even `--purge`.

A restored cluster keeps the roles that were in the backup, so it is
reachable with the **source cluster's** credentials: `pgpod restore` copies
those secrets onto the new cluster's names rather than inventing passwords
no role has. Restoring onto a machine that does not have them — the
repository is self-describing, so the backup may be all you brought —
generates fresh ones and rotates the roles to match, saying so in
`pgpod status`. Either way the URI `pgpod status` prints is one you can
actually connect with.

### Connection pooling, and changing a running cluster

The instance spec reaches the container in an environment variable fixed
when the container is created, so changing a manifest means **replacing**
the container. `pgpod apply` refuses to pretend otherwise:

```
Error: demo-1 is running a different spec than the manifest describes …
  parameters: [] -> [("work_mem", "8MB")]
```

Pooler allows short downtimes for clusters. You can define a pooler like so.

```yaml
apiVersion: pgpod/v1
kind: Pooler
metadata: { name: app }
spec:
  clusters: [{ cluster: demo }]
  port: 6432
  pgDoorman:
    poolMode: transaction
    maxHold: "60s"
```

```sh
pgpod apply -f examples/pooler.yaml
psql postgresql://app@127.0.0.1:6432/appdb     # through the pooler

pgpod apply -f cluster.yaml --recreate         # holds, replaces, releases
```

`--recreate` pauses the pooler's pools for that cluster, replaces the
container, waits for PostgreSQL, recycles the backends and resumes.
Clients wait instead of being disconnected. On the deployment target the
same recreate lost **32 of 66** transactions with nothing in front, and
**0 of 73** with a pooler holding.

`maxHold` is the budget for that window — and, because pg_doorman has one
setting for both, also how long a client waits for a backend under
ordinary pool pressure. A recreate that overruns it fails clients rather
than holding them, which is stated rather than discovered.

Clients authenticate with scram-sha-256 all the way through: pgpod creates
a `pgpod_pooler` role with a `SECURITY DEFINER` lookup, and pg_doorman
verifies the client and replays its ClientKey to PostgreSQL. No password
or verifier is copied anywhere.

One pooler can front several clusters — each gets its own pool, its own
podman network and its own credential — but one per cluster stays the
default, because one pooler process failing takes every cluster it fronts
off its pooled endpoint. Instance ports stay published either way, so a
pooler that is down degrades pooling and not availability.

A pool is addressed by the name a client puts in `dbname`, which defaults
to the database's own name. Two clusters that both call their database
`appdb` therefore collide, and pgpod refuses the manifest rather than
picking a name you did not write; `as:` renames either one.

**Why pg_doorman** and not PgBouncer or pgcat: it authenticates clients
with scram-sha-256. pgcat is MD5-only on the client side, which would mean
md5-encoding every role's password to put it in front of a pgpod cluster.

### Custom images

pgpod runs any PostgreSQL image, not just the official ones. It bypasses
the image entrypoint and drives `initdb`/`postgres` directly, so an image
only needs the standard binaries on `PATH`.

[`examples/searchbase.yaml`](examples/searchbase.yaml) runs
[searchbase](https://github.com/vagmi/searchbase) — a CloudNativePG-based
image bundling pgvector, pgvectorscale, and pg_textsearch:

```sh
cargo run -p pgpod-cli -- apply -f examples/searchbase.yaml
cargo run -p pgpod-cli -- psql search -d appdb
```

The one thing to get right for a non-standard image is
`postgresUid`/`postgresGid` — CNPG-based images run as uid 26 with group
102, not 999:999 like the Debian ones, and the two numbers differ from each
other.

Backups work on such an image unchanged; `examples/searchbase.yaml`
configures them, and the full backup-and-restore round trip has been run
against it.


## Development

```sh
cargo test --workspace          # host unit tests — no podman needed

# Tests that drive a real podman socket. `dev-podman.sh` runs a throwaway
# `podman system service` on a private socket — no systemd units enabled
# on your machine, and a wedged test cannot take out your own podman.
eval "$(ops/dev-podman.sh start)"
cargo test -p pgpod-runtime --features podman-tests -- --nocapture
cargo test -p pgpod-control --features podman-tests -- --nocapture
ops/dev-podman.sh stop

# The S3 path, against a real Garage server behind a TLS proxy. Separate
# because it needs that fixture running — pgBackRest has no plaintext
# option, so Garage cannot be reached over plain HTTP.
eval "$(ops/dev-garage.sh start --network pgpod-gtest --network pgpod-gtestr)"
cargo test -p pgpod-control --features podman-tests --test pitr -- --ignored
ops/dev-garage.sh stop

# Add --isolated for a private image/container store, fully separated
# from anything else you have running.
```

Deployment target is **Ubuntu 26.04 LTS** (podman 5.7, netavark by
default). Most integration tests run fine on any Linux workstation with
rootless podman; `ops/testvm/` boots a disposable 26.04 guest for the
things that need a clean machine — provisioning, linger behaviour, and
reboot recovery.

## License

MIT
