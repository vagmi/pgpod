#!/bin/bash
# Provision an Ubuntu 26.04 (Resolute Raccoon) host to run pgpod.
#
# Idempotent — safe to re-run.
#
#  (adrs/03-deployment-target-and-test-harness.md §1):
#   * netavark is already the default backend — no containers.conf pinning
#   * pgpod needs no FUSE, so no /etc/fuse.conf edit
#   * pgpod needs no KVM, so no kvm group membership
#
# What it does:
#   1. Installs the rootless podman stack.
#   2. Creates a dedicated service user with subuid/subgid ranges.
#   3. Enables lingering so `systemd --user` survives logout.
#   4. Enables the rootless podman socket under that user.
#
# It does NOT install the pgpod binaries — see ops/install-pgpod.sh.
#
# Usage:
#   sudo bash ops/provision-node.sh
#   sudo PGPOD_USER=$(id -un) bash ops/provision-node.sh   # use your own login

set -euo pipefail

PGPOD_USER="${PGPOD_USER:-pgpod}"
PGPOD_HOME="${PGPOD_HOME:-/var/lib/pgpod}"
SUBID_START="${PGPOD_SUBID_START:-100000}"
SUBID_COUNT="${PGPOD_SUBID_COUNT:-65536}"

log() { printf '\n== %s\n' "$*"; }

if [ "$(id -u)" -ne 0 ]; then
  echo "This script must run as root (use sudo)." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 1. Runtime packages
# ---------------------------------------------------------------------------

log "installing the rootless podman stack"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
# netavark + aardvark-dns give container-name DNS on the per-cluster
# network, which is how standbys reach their primary (adrs/00 §10). On
# 26.04 these are already the default backend; installing them explicitly
# keeps the script honest on an upgraded-in-place host that may still be
# carrying CNI config.
apt-get install -y --no-install-recommends \
  podman uidmap passt netavark aardvark-dns fuse-overlayfs \
  ca-certificates jq

log "podman version"
podman --version

# ---------------------------------------------------------------------------
# 2. Service user with subordinate id ranges
# ---------------------------------------------------------------------------

log "ensuring service user ${PGPOD_USER} exists"
if ! id "${PGPOD_USER}" >/dev/null 2>&1; then
  useradd --system --create-home --home-dir "${PGPOD_HOME}" --shell /bin/bash "${PGPOD_USER}"
fi

# Rootless podman needs these to run a container as any UID other than the
# user's own. PostgreSQL runs as 999 (or 26 on CNPG-style images), so
# without them no instance can start. `useradd` populates them on some
# Ubuntu builds and not others — assert rather than hope.
log "ensuring subuid/subgid ranges for ${PGPOD_USER}"
if ! grep -q "^${PGPOD_USER}:" /etc/subuid; then
  echo "${PGPOD_USER}:${SUBID_START}:${SUBID_COUNT}" >> /etc/subuid
fi
if ! grep -q "^${PGPOD_USER}:" /etc/subgid; then
  echo "${PGPOD_USER}:${SUBID_START}:${SUBID_COUNT}" >> /etc/subgid
fi
# Picks up newly added ranges for a user that had already run podman.
sudo -Hu "${PGPOD_USER}" podman system migrate >/dev/null 2>&1 || true

# ---------------------------------------------------------------------------
# 3. Linger, so systemd --user survives without a login session
# ---------------------------------------------------------------------------

log "enabling linger for ${PGPOD_USER}"
loginctl enable-linger "${PGPOD_USER}"

# On 26.04's systemd, `enable-linger` marks the user but does not start
# their systemd instance until the next reboot or an interactive login.
# Force-start it now and wait for the runtime dir, so the next step finds a
# live user manager and DBus (adrs/03 §2).
PGPOD_UID="$(id -u "${PGPOD_USER}")"
systemctl start "user@${PGPOD_UID}.service"
for _ in $(seq 1 30); do
  [ -d "/run/user/${PGPOD_UID}" ] && break
  sleep 0.2
done

# ---------------------------------------------------------------------------
# 4. Rootless podman socket
# ---------------------------------------------------------------------------

log "enabling podman.socket under ${PGPOD_USER}"
# `sudo -iu` runs a login shell but does not invoke pam_systemd, so
# XDG_RUNTIME_DIR / DBUS_SESSION_BUS_ADDRESS stay unset and
# `systemctl --user` fails with "Failed to connect to user scope bus".
# Setting them explicitly is deterministic across releases (adrs/03 §2).
# Kept on one line — newlines get mangled through the nested shells.
sudo -Hu "${PGPOD_USER}" bash -c \
  "export XDG_RUNTIME_DIR=/run/user/${PGPOD_UID} && export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/${PGPOD_UID}/bus && systemctl --user daemon-reload && systemctl --user enable --now podman.socket && ls -l /run/user/${PGPOD_UID}/podman/podman.sock"

# ---------------------------------------------------------------------------
# 5. Report
# ---------------------------------------------------------------------------

log "podman info for ${PGPOD_USER}"
sudo -Hu "${PGPOD_USER}" bash -c \
  "export XDG_RUNTIME_DIR=/run/user/${PGPOD_UID} && podman info --format 'graphRoot: {{.Store.GraphRoot}}
runRoot:   {{.Store.RunRoot}}
backend:   {{.Host.NetworkBackend}}
runtime:   {{.Host.OCIRuntime.Name}}
cgroups:   v{{.Host.CgroupVersion}}
rootless:  {{.Host.Security.Rootless}}
distro:    {{.Host.Distribution.Distribution}} {{.Host.Distribution.Version}}'"

log "provisioning complete"
cat <<HINT
Next: install the pgpod binary and run its own preflight, which checks
rather more than the summary above:

  sudo -Hu ${PGPOD_USER} env XDG_RUNTIME_DIR=/run/user/${PGPOD_UID} pgpod doctor

NOTE: podman volumes now hold PostgreSQL data directories, so the graph
root above is where your databases live. If it is on a small filesystem,
set \`graphroot\` in ${PGPOD_HOME}/.config/containers/storage.conf BEFORE
creating any cluster — moving it afterwards means moving live databases.
HINT
