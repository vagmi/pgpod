#!/bin/bash
# Manage a transient podman service for pgpod development and testing.
#
# pgpod talks to a podman socket, and tests need one. Rather than requiring
# `systemctl --user enable --now podman.socket` — a persistent change to
# the developer's machine, and a socket shared with everything else they
# run — this starts a throwaway `podman system service` on a private
# socket that goes away when you stop it or when it idles out.
#
# Why transient:
#   * No systemd units enabled on anyone's machine, including CI runners.
#   * The socket path is pgpod's alone, so a test that wedges the service
#     cannot take out the developer's own podman.
#   * Identical on a workstation, in CI, and inside ops/testvm — one code
#     path instead of "works on my machine" divergence.
#
# The service shares the user's normal graph root by default, so image
# pulls stay cached between runs. `--isolated` gives it a private
# root/runroot under $PGPOD_DEV_ROOT instead: total isolation from
# anything else you have running, at the cost of re-pulling images.
#
# Usage:
#   eval "$(ops/dev-podman.sh start)"   # start + export PGPOD_PODMAN_SOCKET
#   ops/dev-podman.sh status
#   ops/dev-podman.sh stop
#   ops/dev-podman.sh env               # print the export line only
#
#   ops/dev-podman.sh start --isolated  # private image/container store

set -euo pipefail

# Unix socket paths are capped near 107 bytes, so this must stay short —
# a path under a scratch directory nested a few levels deep silently
# fails with EINVAL from bind(2).
RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
SOCKET="${PGPOD_DEV_SOCKET:-${RUNTIME_DIR}/pgpod-dev.sock}"
PIDFILE="${RUNTIME_DIR}/pgpod-dev.pid"
LOGFILE="${RUNTIME_DIR}/pgpod-dev.log"

# How long the service lingers with no client attached, in seconds. Long
# enough for a full test run, short enough that a forgotten service
# reaps itself.
IDLE_TIMEOUT="${PGPOD_DEV_IDLE:-3600}"

DEV_ROOT="${PGPOD_DEV_ROOT:-${RUNTIME_DIR}/pgpod-dev-store}"
ISOLATED=0

is_running() {
  [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null
}

cmd_start() {
  if is_running; then
    echo "# already running (pid $(cat "$PIDFILE"))" >&2
    cmd_env
    return 0
  fi

  # A socket file left by a crashed service blocks bind(2).
  rm -f "$SOCKET"

  local args=()
  if [ "$ISOLATED" -eq 1 ]; then
    mkdir -p "$DEV_ROOT/root" "$DEV_ROOT/runroot"
    args+=(--root "$DEV_ROOT/root" --runroot "$DEV_ROOT/runroot")
    echo "# isolated store: $DEV_ROOT (images will be re-pulled)" >&2
  fi

  podman "${args[@]}" system service --time="$IDLE_TIMEOUT" "unix://$SOCKET" \
    >"$LOGFILE" 2>&1 &
  echo $! > "$PIDFILE"

  local i=0
  while [ $i -lt 40 ]; do
    [ -S "$SOCKET" ] && break
    if ! is_running; then
      echo "podman service died on startup:" >&2
      sed 's/^/  /' "$LOGFILE" >&2
      rm -f "$PIDFILE"
      return 1
    fi
    i=$((i + 1)); sleep 0.25
  done

  if [ ! -S "$SOCKET" ]; then
    echo "timed out waiting for $SOCKET" >&2
    return 1
  fi

  echo "# podman service on $SOCKET (pid $(cat "$PIDFILE"), idle timeout ${IDLE_TIMEOUT}s)" >&2
  cmd_env
}

cmd_stop() {
  if ! is_running; then
    echo "# not running" >&2
    rm -f "$PIDFILE" "$SOCKET"
    return 0
  fi
  local pid
  pid=$(cat "$PIDFILE")
  kill "$pid" 2>/dev/null || true
  local i=0
  while [ $i -lt 20 ] && kill -0 "$pid" 2>/dev/null; do
    i=$((i + 1)); sleep 0.25
  done
  kill -9 "$pid" 2>/dev/null || true
  rm -f "$PIDFILE" "$SOCKET"
  echo "# stopped" >&2
}

cmd_status() {
  if is_running; then
    echo "running: pid $(cat "$PIDFILE"), socket $SOCKET"
    PGPOD_PODMAN_SOCKET="$SOCKET" podman --url "unix://$SOCKET" version --format \
      '  client {{.Client.Version}} / server {{.Server.Version}}' 2>/dev/null || true
  else
    echo "not running"
    return 1
  fi
}

cmd_env() {
  # Printed on stdout so `eval "$(... start)"` works while the human-facing
  # notes above go to stderr.
  echo "export PGPOD_PODMAN_SOCKET=$SOCKET"
}

COMMAND="${1:-}"; shift || true
for arg in "$@"; do
  case "$arg" in
    --isolated) ISOLATED=1 ;;
    *) echo "Unknown option: $arg" >&2; exit 1 ;;
  esac
done

case "$COMMAND" in
  start)   cmd_start ;;
  stop)    cmd_stop ;;
  status)  cmd_status ;;
  env)     cmd_env ;;
  restart) cmd_stop; cmd_start ;;
  *)
    sed -n '2,30p' "$0" | sed 's/^# \?//'
    exit 1
    ;;
esac
