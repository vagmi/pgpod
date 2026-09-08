#!/bin/bash
# A local Garage S3 endpoint for testing pgpod's object-storage path.
#
# Garage (https://garagehq.deuxfleurs.fr/) is what ADR 03 §5 picks to test
# against: a single static Rust binary for self-hosting, and a *stricter*
# subset of S3 than MinIO — which is a testing virtue, because passing
# against it means pgpod has not grown a dependency on AWS-specific
# behaviour.
#
# **It is fronted by a TLS proxy, and that is not optional.** ADR 03 §5
# planned to reach Garage over plain HTTP on :3900 with `allow_http`.
# pgBackRest has no plaintext option at all — it always speaks TLS to
# object storage — so the plan does not survive ADR 04. `socat` terminates
# TLS with a self-signed certificate and forwards to Garage as raw TCP,
# which matters: a *transparent* proxy leaves the `Host` header untouched,
# and S3 request signatures are computed over it.
#
# Usage:
#   eval "$(ops/dev-garage.sh start)"          # own network
#   eval "$(ops/dev-garage.sh start --network pgpod-mydb --network pgpod-mydbr)"
#   ops/dev-garage.sh env
#   ops/dev-garage.sh stop
#
# `start` prints eval-able assignments: PGPOD_GARAGE_ENDPOINT,
# PGPOD_GARAGE_BUCKET, PGPOD_GARAGE_KEY, PGPOD_GARAGE_SECRET,
# PGPOD_GARAGE_NETWORK.

set -euo pipefail

cd "$(dirname "$0")/.."

GARAGE_IMAGE="${PGPOD_GARAGE_IMAGE:-docker.io/dxflrs/garage:v2.3.0}"
SOCAT_IMAGE="${PGPOD_SOCAT_IMAGE:-docker.io/alpine/socat:latest}"

GARAGE_NAME=pgpod-garage
PROXY_NAME=pgpod-garage-tls
# Not 443: a non-privileged port keeps this working if the fixture is ever
# run somewhere the container cannot bind low ports, and it exercises
# pgpod's endpoint host:port splitting rather than the default.
TLS_PORT=3443
BUCKET="${PGPOD_GARAGE_BUCKET:-pgpod-test}"

STATE_DIR="${XDG_RUNTIME_DIR:-/tmp}/pgpod-garage"

PODMAN=(podman)
if [ -n "${PGPOD_PODMAN_SOCKET:-}" ]; then
  PODMAN=(podman --url "unix://$PGPOD_PODMAN_SOCKET")
fi

# This workstation has a credential helper invoked for every registry that
# exits non-zero. An empty authfile sidesteps it; harmless elsewhere.
AUTHFILE="$STATE_DIR/auth.json"

die() { echo "$*" >&2; exit 1; }

cmd_start() {
  # Repeatable. A restored cluster gets its *own* podman network, so
  # reaching the same repository from both means being on both — which is
  # a fixture artifact, not a product one: a real endpoint is reachable by
  # ordinary DNS and does not care what network the client is on.
  local networks=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --network) networks+=("$2"); shift 2 ;;
      *) die "Unknown argument: $1" ;;
    esac
  done
  [ ${#networks[@]} -gt 0 ] || networks=(pgpod-garage-net)
  local network="${networks[0]}"

  mkdir -p "$STATE_DIR"
  echo '{"auths":{}}' > "$AUTHFILE"

  if "${PODMAN[@]}" container exists "$GARAGE_NAME" 2>/dev/null; then
    echo "# garage is already running; run 'ops/dev-garage.sh stop' first" >&2
    cmd_env
    return 0
  fi

  local net_args=()
  for n in "${networks[@]}"; do
    "${PODMAN[@]}" network exists "$n" 2>/dev/null \
      || "${PODMAN[@]}" network create "$n" >/dev/null
    net_args+=(--network "$n")
  done

  local key secret
  # Garage access keys look like GK + 24 hex.
  key="GK$(openssl rand -hex 12)"
  secret="$(openssl rand -hex 32)"

  # Self-signed, and the certificate is never verified — pgBackRest is
  # pointed at it with verify-tls off, because there is nowhere to publish
  # a CA for a throwaway fixture. It still has to be a *valid* certificate:
  # TLS is mandatory even when verification is not.
  if [ ! -s "$STATE_DIR/tls.crt" ]; then
    openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
      -keyout "$STATE_DIR/tls.key" -out "$STATE_DIR/tls.crt" \
      -subj "/CN=$PROXY_NAME" \
      -addext "subjectAltName=DNS:$PROXY_NAME,DNS:$GARAGE_NAME,DNS:localhost" \
      >/dev/null 2>&1
    # socat wants one file with both.
    cat "$STATE_DIR/tls.crt" "$STATE_DIR/tls.key" > "$STATE_DIR/tls.pem"
    chmod 0644 "$STATE_DIR/tls.pem"
  fi

  # `replication_factor = 1` and sqlite: a single test node, not a cluster.
  cat > "$STATE_DIR/garage.toml" <<TOML
metadata_dir = "/var/lib/garage/meta"
data_dir = "/var/lib/garage/data"
db_engine = "sqlite"
replication_factor = 1
rpc_bind_addr = "[::]:3901"
rpc_secret = "$(openssl rand -hex 32)"

[s3_api]
# Garage's default region is literally "garage", not us-east-1, and
# signature validation rejects the wrong one (ADR 03 §5).
s3_region = "garage"
api_bind_addr = "[::]:3900"
root_domain = ".s3.garage"

[admin]
api_bind_addr = "[::]:3903"
admin_token = "$(openssl rand -hex 32)"
TOML
  # Garage refuses to start if a config holding secrets is world-readable.
  chmod 0600 "$STATE_DIR/garage.toml"

  echo "== starting garage on networks: ${networks[*]}" >&2
  "${PODMAN[@]}" run -d --rm --authfile "$AUTHFILE" \
    --name "$GARAGE_NAME" "${net_args[@]}" \
    -e "GARAGE_DEFAULT_BUCKET=$BUCKET" \
    -e "GARAGE_DEFAULT_ACCESS_KEY=$key" \
    -e "GARAGE_DEFAULT_SECRET_KEY=$secret" \
    -v "$STATE_DIR/garage.toml:/etc/garage.toml:ro" \
    --entrypoint /garage "$GARAGE_IMAGE" \
    server --single-node --default-bucket >/dev/null

  # Wait for the S3 API to answer before the proxy points at it, so a
  # caller that gets a successful `start` can actually use the endpoint.
  local ready=0
  for _ in $(seq 1 60); do
    if "${PODMAN[@]}" exec "$GARAGE_NAME" /garage status >/dev/null 2>&1; then
      ready=1; break
    fi
    sleep 1
  done
  [ "$ready" = 1 ] || {
    "${PODMAN[@]}" logs "$GARAGE_NAME" 2>&1 | tail -20 >&2
    die "garage did not become ready"
  }

  echo "== starting the TLS proxy on $PROXY_NAME:$TLS_PORT" >&2
  # A transparent TCP forwarder: it terminates TLS and leaves the HTTP
  # bytes — Host header included — exactly as pgBackRest sent them, which
  # is what keeps the S3 signature valid.
  "${PODMAN[@]}" run -d --rm --authfile "$AUTHFILE" \
    --name "$PROXY_NAME" "${net_args[@]}" \
    -v "$STATE_DIR/tls.pem:/tls.pem:ro" \
    --entrypoint socat "$SOCAT_IMAGE" \
    "OPENSSL-LISTEN:$TLS_PORT,cert=/tls.pem,verify=0,reuseaddr,fork" \
    "TCP:$GARAGE_NAME:3900" >/dev/null

  cat > "$STATE_DIR/env" <<ENV
PGPOD_GARAGE_ENDPOINT=$PROXY_NAME:$TLS_PORT
PGPOD_GARAGE_BUCKET=$BUCKET
PGPOD_GARAGE_KEY=$key
PGPOD_GARAGE_SECRET=$secret
PGPOD_GARAGE_NETWORK=${networks[*]}
PGPOD_GARAGE_REGION=garage
ENV
  chmod 0600 "$STATE_DIR/env"
  cmd_env
}

cmd_env() {
  [ -s "$STATE_DIR/env" ] || die "garage is not running — ops/dev-garage.sh start"
  # Values are quoted: PGPOD_GARAGE_NETWORK can hold several names, and an
  # unquoted `export X=a b` is a syntax error rather than a mistake you
  # notice.
  while IFS='=' read -r key value; do
    [ -n "$key" ] && printf 'export %s=%q
' "$key" "$value"
  done < "$STATE_DIR/env"
}

cmd_stop() {
  for c in "$PROXY_NAME" "$GARAGE_NAME"; do
    "${PODMAN[@]}" rm -f "$c" >/dev/null 2>&1 || true
  done
  rm -f "$STATE_DIR/env"
  echo "# stopped" >&2
}

case "${1:-}" in
  start) shift; cmd_start "$@" ;;
  stop)  cmd_stop ;;
  env)   cmd_env ;;
  *) die "Usage: ops/dev-garage.sh {start [--network NET]|stop|env}" ;;
esac
