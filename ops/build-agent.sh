#!/bin/bash
# Build the statically-linked pgpod-agent and install it where the daemon
# expects to find it.
#
# The agent is bind-mounted read-only into an arbitrary PostgreSQL image —
# Debian, Alpine, or a custom extension build — so it must not assume a
# libc. `x86_64-unknown-linux-musl` with Rust's bundled musl gives a
# static-pie binary with no shared-library dependencies at all
# (adrs/00-project-setup.md §3).
#
# One binary per architecture: the daemon mounts the one matching the host
# it is running on, because a container shares the host kernel and ISA.
#
# Usage:
#   ops/build-agent.sh                 # host architecture
#   ops/build-agent.sh aarch64         # cross-build
#   PGPOD_HOME=/tmp/x ops/build-agent.sh   # install elsewhere

set -euo pipefail

cd "$(dirname "$0")/.."

ARCH="${1:-$(uname -m)}"
case "$ARCH" in
  x86_64|amd64)  TARGET=x86_64-unknown-linux-musl;  ARCH=x86_64 ;;
  aarch64|arm64) TARGET=aarch64-unknown-linux-musl; ARCH=aarch64 ;;
  *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

# Mirrors pgpod_core::PathLayout — keep in step with it.
if [ -n "${PGPOD_HOME:-}" ]; then
  DEST_DIR="$PGPOD_HOME/data/bin"
else
  DEST_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/pgpod/bin"
fi
DEST="$DEST_DIR/pgpod-agent-$ARCH"

echo "== ensuring the $TARGET toolchain is present"
rustup target add "$TARGET" >/dev/null

# No features: object-store backends were cargo features while pgpod
# implemented archiving itself. pgBackRest carries posix, s3, gcs, azure
# and sftp in one binary, so the agent no longer links any of it — and the
# musl build lost `object_store`, `zstd`, `sha2` and a whole TLS stack with
# them (ADR 04).
echo "== building pgpod-agent for $TARGET"
cargo build -p pgpod-agent --target "$TARGET" --release

BIN="target/$TARGET/release/pgpod-agent"

# The whole point is portability into an unknown image; a dynamic
# dependency here would surface as a cryptic exec failure at instance
# start rather than a build error.
if ldd "$BIN" 2>&1 | grep -qv 'statically linked\|not a dynamic executable'; then
  echo "ERROR: $BIN is not statically linked:" >&2
  ldd "$BIN" >&2
  exit 1
fi

install -d -m 0755 "$DEST_DIR"
# 0755: the container runs as its own uid and must be able to execute
# this through a read-only bind mount.
install -m 0755 "$BIN" "$DEST"

echo "== installed $DEST"
ls -l "$DEST"
