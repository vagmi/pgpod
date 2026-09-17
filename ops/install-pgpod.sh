#!/bin/bash
# Install (or upgrade) pgpod and its boot-recovery unit.
#
# **No root needed in the default mode.** The daemon is unprivileged, the
# unit is a `systemd --user` unit, and everything it touches is under
# $HOME. Root is only for installing into a *different* user's account —
# the dedicated service user ops/provision-node.sh creates.
#
# Idempotent — re-running upgrades the binaries in place and restarts the
# unit. Restarting the daemon is not an outage: containers outlive it, and
# the next start adopts whatever is still up.
#
# Usage:
#   ops/install-pgpod.sh                                  # for yourself
#   sudo PGPOD_USER=pgpod ops/install-pgpod.sh            # service account
#
# What it installs, and where (all XDG, per AGENTS.md principle 8):
#   ~/bin/pgpod                                  the CLI and the daemon
#   ~/.local/share/pgpod/bin/pgpod-agent-<arch>  PID 1 inside containers
#   ~/.local/share/pgpod/pgbackrest-<arch>/      the pgBackRest bundle
#   ~/.config/systemd/user/pgpod-daemon.service  the unit
#
# Build the pieces first:
#   cargo build --release -p pgpod-cli     (or the musl target, see below)
#   ops/build-agent.sh
#   ops/build-pgbackrest.sh

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "${HERE}")"

UNIT_NAME="pgpod-daemon.service"
UNIT_SRC="${PGPOD_UNIT_SRC:-${HERE}/${UNIT_NAME}}"
ARCH="$(uname -m)"

log() { printf '\n== %s\n' "$*"; }
die() { echo "Error: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Who are we installing for?
# ---------------------------------------------------------------------------

if [ -n "${PGPOD_USER:-}" ] && [ "${PGPOD_USER}" != "$(id -un)" ]; then
  # Cross-user install: needs root to write into someone else's home and
  # to drive their user manager.
  TARGET_USER="${PGPOD_USER}"
  [ "$(id -u)" -eq 0 ] || die "installing for ${TARGET_USER} needs root — re-run with sudo."
  id "${TARGET_USER}" >/dev/null 2>&1 \
    || die "user ${TARGET_USER} does not exist — run ops/provision-node.sh first."
  CROSS_USER=1
else
  TARGET_USER="$(id -un)"
  CROSS_USER=0
  # The daemon refuses to run as root, so installing it into root's own
  # account would produce a unit that can never start.
  [ "$(id -u)" -ne 0 ] \
    || die "pgpod must not run as root. Install it for an unprivileged user: sudo PGPOD_USER=pgpod $0"
fi

[ "${TARGET_USER}" != "root" ] || die "pgpod must not run as root."

TARGET_UID="$(id -u "${TARGET_USER}")"
TARGET_HOME="$(getent passwd "${TARGET_USER}" | cut -d: -f6)"
[ -n "${TARGET_HOME}" ] || die "could not determine ${TARGET_USER}'s home directory."

BIN_DST="${TARGET_HOME}/bin/pgpod"
DATA_DST="${TARGET_HOME}/.local/share/pgpod"
UNIT_DST="${TARGET_HOME}/.config/systemd/user/${UNIT_NAME}"

log "installing pgpod for ${TARGET_USER} (uid ${TARGET_UID}, home ${TARGET_HOME})"

# ---------------------------------------------------------------------------
# Locate the built artefacts
# ---------------------------------------------------------------------------

# A musl build is preferred when present: it is static, so it runs on a
# host whose glibc is older than the build machine's. Building pgpod on a
# rolling distribution and installing it on Ubuntu LTS hits exactly that.
#
# `${HERE}/pgpod` comes second, after only an explicit override: it is
# what an unpacked release tarball looks like, where this script sits
# beside the binaries it is installing. Checking it before the target/
# directories means an unpacked release never accidentally installs some
# stale build from a checkout it happens to be sitting inside.
find_binary() {
  for candidate in \
    "${PGPOD_BIN_SRC:-}" \
    "${HERE}/pgpod" \
    "${REPO}/target/${ARCH}-unknown-linux-musl/release/pgpod" \
    "${REPO}/target/release/pgpod" \
    "/tmp/pgpod"; do
    [ -n "${candidate}" ] && [ -x "${candidate}" ] && { echo "${candidate}"; return 0; }
  done
  return 1
}

BIN_SRC="$(find_binary)" || die "no pgpod binary found. Build one:
  cargo build --release -p pgpod-cli
  # or, for a host with an older glibc than this machine:
  cargo build --release -p pgpod-cli --target ${ARCH}-unknown-linux-musl
Or point PGPOD_BIN_SRC at one."

[ -f "${UNIT_SRC}" ] || die "unit file not found at ${UNIT_SRC}"

# The agent and the bundle are built into the *building* user's XDG data
# directory by ops/build-agent.sh and ops/build-pgbackrest.sh. On a
# cross-user install they have to be copied across.
SRC_DATA="${PGPOD_DATA_SRC:-${XDG_DATA_HOME:-${HOME}/.local/share}/pgpod}"
AGENT_SRC="${SRC_DATA}/bin/pgpod-agent-${ARCH}"
BUNDLE_SRC="${SRC_DATA}/pgbackrest-${ARCH}"

# In an unpacked release tarball the agent sits beside this script rather
# than in an XDG tree, so `tar xf … && ./install-pgpod.sh` is the whole
# installation. An explicit PGPOD_DATA_SRC still wins.
if [ -z "${PGPOD_DATA_SRC:-}" ] && [ -x "${HERE}/pgpod-agent-${ARCH}" ]; then
  AGENT_SRC="${HERE}/pgpod-agent-${ARCH}"
fi
if [ -z "${PGPOD_DATA_SRC:-}" ] && [ -d "${HERE}/pgbackrest-${ARCH}" ]; then
  BUNDLE_SRC="${HERE}/pgbackrest-${ARCH}"
fi

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------

# `install -o/-g` only when root; as yourself the files are already yours
# and passing -o would fail.
if [ "${CROSS_USER}" -eq 1 ]; then
  OWN=(-o "${TARGET_USER}" -g "${TARGET_USER}")
else
  OWN=()
fi

log "installing the binary to ${BIN_DST}"
install -d "${OWN[@]}" -m 0755 "$(dirname "${BIN_DST}")"
install "${OWN[@]}" -m 0755 "${BIN_SRC}" "${BIN_DST}"
echo "   from ${BIN_SRC}"

AGENT_DST="${DATA_DST}/bin/pgpod-agent-${ARCH}"
# On a self-install the build script already wrote into this very path, so
# there is nothing to copy — and `install` refuses same-file anyway.
if [ -x "${AGENT_SRC}" ] && [ "${AGENT_SRC}" != "${AGENT_DST}" ]; then
  log "installing the agent to ${DATA_DST}/bin/"
  install -d "${OWN[@]}" -m 0755 "${DATA_DST}/bin"
  install "${OWN[@]}" -m 0755 "${AGENT_SRC}" "${AGENT_DST}"
elif [ -x "${AGENT_DST}" ]; then
  echo "   agent already in place at ${AGENT_DST}"
else
  echo "   WARNING: no agent binary at ${AGENT_SRC} — run ops/build-agent.sh." >&2
  echo "            pgpod cannot start an instance without it." >&2
fi

BUNDLE_DST="${DATA_DST}/pgbackrest-${ARCH}"
if [ -d "${BUNDLE_SRC}" ] && [ "${BUNDLE_SRC}" != "${BUNDLE_DST}" ]; then
  log "installing the pgBackRest bundle to ${BUNDLE_DST}/"
  install -d "${OWN[@]}" -m 0755 "${DATA_DST}"
  rm -rf "${DATA_DST}/pgbackrest-${ARCH}.new"
  cp -a "${BUNDLE_SRC}" "${DATA_DST}/pgbackrest-${ARCH}.new"
  rm -rf "${DATA_DST}/pgbackrest-${ARCH}"
  mv "${DATA_DST}/pgbackrest-${ARCH}.new" "${DATA_DST}/pgbackrest-${ARCH}"
  [ "${CROSS_USER}" -eq 1 ] && chown -R "${TARGET_USER}:${TARGET_USER}" "${DATA_DST}/pgbackrest-${ARCH}"
elif [ -d "${BUNDLE_DST}" ]; then
  echo "   bundle already in place at ${BUNDLE_DST}"
else
  echo "   note: no pgBackRest bundle at ${BUNDLE_SRC} (ops/build-pgbackrest.sh)." >&2
  echo "         Only clusters with backups configured need it." >&2
fi

log "installing the unit to ${UNIT_DST}"
install -d "${OWN[@]}" -m 0755 "$(dirname "${UNIT_DST}")"
install "${OWN[@]}" -m 0644 "${UNIT_SRC}" "${UNIT_DST}"

# ---------------------------------------------------------------------------
# Linger — the one thing that genuinely needs root
# ---------------------------------------------------------------------------

if [ -e "/var/lib/systemd/linger/${TARGET_USER}" ]; then
  log "linger is enabled for ${TARGET_USER}"
else
  if [ "$(id -u)" -eq 0 ]; then
    log "enabling linger for ${TARGET_USER}"
    loginctl enable-linger "${TARGET_USER}"
    # On 26.04 `enable-linger` marks the user but does not start their
    # manager until the next reboot or an interactive login (ADR 03 §2).
    systemctl start "user@${TARGET_UID}.service" || true
    for _ in $(seq 1 30); do [ -d "/run/user/${TARGET_UID}" ] && break; sleep 0.2; done
  else
    echo
    echo "   WARNING: linger is not enabled for ${TARGET_USER}." >&2
    echo "   Without it there is no systemd --user between logins, so nothing" >&2
    echo "   starts your clusters after a reboot. This is the one step that" >&2
    echo "   needs root:" >&2
    echo >&2
    echo "       sudo loginctl enable-linger ${TARGET_USER}" >&2
    echo >&2
  fi
fi

# ---------------------------------------------------------------------------
# Enable and start the unit
# ---------------------------------------------------------------------------

log "enabling ${UNIT_NAME} under ${TARGET_USER}"
if [ "${CROSS_USER}" -eq 1 ]; then
  # `sudo -Hu` and not `-iu`: a login shell does not invoke pam_systemd, so
  # XDG_RUNTIME_DIR / DBUS_SESSION_BUS_ADDRESS stay unset and
  # `systemctl --user` fails with "Failed to connect to user scope bus".
  # Setting them explicitly is deterministic across releases (ADR 03 §2).
  # `cd` first, because -H sets HOME but not the working directory, and
  # the current one may belong to another user.
  sudo -Hu "${TARGET_USER}" bash -c \
    "cd ${TARGET_HOME} && export XDG_RUNTIME_DIR=/run/user/${TARGET_UID} && export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/${TARGET_UID}/bus && systemctl --user daemon-reload && systemctl --user enable ${UNIT_NAME} && systemctl --user restart ${UNIT_NAME} && sleep 1 && systemctl --user is-active ${UNIT_NAME}"
else
  # A live session already has both variables set by pam_systemd.
  systemctl --user daemon-reload
  systemctl --user enable "${UNIT_NAME}"
  systemctl --user restart "${UNIT_NAME}"
  sleep 1
  systemctl --user is-active "${UNIT_NAME}"
fi

log "installed"
cat <<EOF
Check the host:      ${BIN_DST} doctor
Follow the daemon:   journalctl --user -u ${UNIT_NAME} -f
Resume by hand:      ${BIN_DST} daemon --once

${BIN_DST} is on \$PATH if ~/bin is.
EOF
