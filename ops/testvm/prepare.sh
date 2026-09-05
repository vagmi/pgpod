#!/bin/bash
# Fetch the Ubuntu 26.04 (Resolute Raccoon) server cloud image and grow it.
#
# Idempotent — safe to re-run.
#
# The disk is sized well past sandpod's 20G because the podman graph root
# now holds PostgreSQL data directories *and* base backups in flight
# (adrs/00-project-setup.md §4).

set -euo pipefail

cd "$(dirname "$0")"
mkdir -p images
cd images

IMAGE=resolute-server-cloudimg-amd64.img
URL=https://cloud-images.ubuntu.com/resolute/current/${IMAGE}
DISK_SIZE="${PGPOD_TESTVM_DISK:-60G}"

if [ ! -f "$IMAGE" ]; then
    echo "Downloading Ubuntu 26.04 (Resolute) cloud image..."
    wget --progress=dot:giga "$URL"
else
    echo "Cloud image already present, skipping download."
fi

qemu-img resize "$IMAGE" "$DISK_SIZE"

echo "Ready. Boot with:"
echo "  ./vm.sh create"
