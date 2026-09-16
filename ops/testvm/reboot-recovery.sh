#!/bin/bash
# The reboot-recovery acceptance test (ADR 03 §4, ROADMAP Phase 4).
#
# Asserts the claim the pgpod daemon exists to make: **a host reboot
# restores the cluster with no human action.** Nothing here touches the
# guest after the reboot except to look at it — the moment this script
# starts a container itself, it stops testing anything.
#
# Why a VM and not a container test: what is under test is linger,
# `systemd --user`, a real `podman start` against a wiped
# XDG_RUNTIME_DIR, and the kernel actually going away. None of that can be
# imitated on the developer's host.
#
# Prerequisites:
#   ops/testvm/prepare.sh && ops/testvm/vm.sh create
#   ssh ubuntu@<ip> 'sudo bash' < ops/provision-node.sh    # once
#   cargo build --release -p pgpod-cli --target x86_64-unknown-linux-musl
#   ops/build-agent.sh
#
# Usage:
#   ops/testvm/reboot-recovery.sh [vm-name]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "${HERE}/../.." && pwd)"
VM="${1:-${PGPOD_TESTVM_NAME:-pgpod-testvm}}"
ARCH="${PGPOD_TESTVM_ARCH:-x86_64}"

CLUSTER="rebootdemo"
POOLER="rebootpool"
# Fixed rather than allocated, so the assertion "the port did not move" is
# about pgpod and not about what else the guest happened to bind.
POOLER_PORT=6544

log() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "  ok: $*"; }

BIN="${REPO}/target/${ARCH}-unknown-linux-musl/release/pgpod"
[ -x "${BIN}" ] || fail "no static binary at ${BIN}
  cargo build --release -p pgpod-cli --target ${ARCH}-unknown-linux-musl
(a glibc build from a rolling distribution will not run on Ubuntu LTS)"

AGENT="${XDG_DATA_HOME:-${HOME}/.local/share}/pgpod/bin/pgpod-agent-${ARCH}"
[ -x "${AGENT}" ] || fail "no agent at ${AGENT} — run ops/build-agent.sh"

# The guest is reached over plain ssh, so this test is not tied to the
# libvirt harness: a GCE instance, a bare-metal box or the local VM are all
# the same to it. `PGPOD_TESTVM_SSH_TARGET` names the guest directly;
# otherwise the address comes from PGPOD_TESTVM_IP or from vm.sh, which
# reaches libvirt through sudo.
if [ -n "${PGPOD_TESTVM_SSH_TARGET:-}" ]; then
  TARGET="${PGPOD_TESTVM_SSH_TARGET}"
else
  IP="${PGPOD_TESTVM_IP:-$("${HERE}/vm.sh" status "${VM}" | awk '/IP:/ {print $2}')}"
  [ -n "${IP}" ] || fail "could not find ${VM}'s IP — is it running?
Set PGPOD_TESTVM_IP=<addr>, or PGPOD_TESTVM_SSH_TARGET=user@host for a
guest that libvirt does not know about."
  TARGET="ubuntu@${IP}"
fi

# These guests are recreated constantly and land on recycled addresses, so
# a remembered host key is guaranteed to be wrong and only ever blocks.
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
          -o LogLevel=ERROR -o ConnectTimeout=15)
[ -n "${PGPOD_TESTVM_SSH_KEY:-}" ] && SSH_OPTS+=(-i "${PGPOD_TESTVM_SSH_KEY}")

vm() { ssh "${SSH_OPTS[@]}" "${TARGET}" "$@"; }
scp_to() { scp "${SSH_OPTS[@]}" "$1" "${TARGET}:$2"; }

# Reboot and wait for a *different* boot id, not merely for ssh to answer:
# the pre-reboot sshd keeps accepting connections for a moment, and polling
# for reachability would report success against the system we just asked to
# go away — after which every assertion tests the wrong boot.
reboot_guest() {
  local before now i=0
  before="$(vm 'cat /proc/sys/kernel/random/boot_id')" \
    || fail "could not read the boot id"
  echo "rebooting ${TARGET} (boot id ${before})"
  vm 'sudo systemctl reboot' || true
  while [ $i -lt 80 ]; do
    sleep 3
    now="$(vm 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null)" || { i=$((i+1)); continue; }
    if [ -n "${now}" ] && [ "${now}" != "${before}" ]; then
      echo "back up (boot id ${now})"
      return 0
    fi
    i=$((i + 1))
  done
  fail "${TARGET} did not come back within 4 minutes"
}

# ---------------------------------------------------------------------------
log "installing pgpod on ${TARGET}"
# ---------------------------------------------------------------------------

vm 'mkdir -p pgpod-ops/ops .local/share/pgpod/bin'
# scp destinations are relative to the remote home already; a quoted `~`
# is not expanded by the shell here and scp takes it literally.
scp_to "${BIN}" /tmp/pgpod
# Staged and renamed rather than written in place: the agent is
# bind-mounted into every running container, so overwriting the inode
# fails with ETXTBSY. `mv` swaps the directory entry and leaves running
# containers on the old inode, which is what we want — they are about to
# be restarted anyway.
scp_to "${AGENT}" "/tmp/pgpod-agent-${ARCH}"
vm "install -m 0755 /tmp/pgpod-agent-${ARCH} .local/share/pgpod/bin/pgpod-agent-${ARCH}.new \
    && mv -f .local/share/pgpod/bin/pgpod-agent-${ARCH}.new .local/share/pgpod/bin/pgpod-agent-${ARCH}"
scp_to "${REPO}/ops/install-pgpod.sh" 'pgpod-ops/ops/install-pgpod.sh'
scp_to "${REPO}/ops/pgpod-daemon.service" 'pgpod-ops/ops/pgpod-daemon.service'

# Deliberately *without* sudo: the install path for your own user needs no
# root at all, and this is where that claim is checked.
vm 'chmod +x pgpod-ops/ops/install-pgpod.sh'
vm 'PGPOD_BIN_SRC=/tmp/pgpod ./pgpod-ops/ops/install-pgpod.sh'

# ---------------------------------------------------------------------------
log "the daemon is a *user* unit, running unprivileged"
# ---------------------------------------------------------------------------

[ "$(vm 'systemctl --user is-enabled pgpod-daemon.service')" = "enabled" ] \
  || fail "the unit is not enabled"
ok "enabled under systemd --user"

vm 'systemctl status pgpod-daemon.service' >/dev/null 2>&1 \
  && fail "pgpod-daemon.service exists in the SYSTEM manager — it must be a user unit only"
ok "the system manager has no such unit"

vm 'sudo ~/bin/pgpod daemon --once' >/dev/null 2>&1 \
  && fail "pgpod daemon ran as root — it must refuse"
ok "the daemon refuses to run as root"

# ---------------------------------------------------------------------------
log "containers must not live in the daemon's cgroup"
# ---------------------------------------------------------------------------
#
# If they did, `systemctl --user stop pgpod-daemon` would take every
# database down with it, and the unit would need KillMode=process. It does
# not — verified here rather than assumed, because it is a podman
# implementation detail that pgpod depends on.

echo "  cgroupManager: $(vm 'podman info --format json | jq -r .host.cgroupManager')"

# ---------------------------------------------------------------------------
log "creating a cluster and a pooler, and writing a row"
# ---------------------------------------------------------------------------

# Start from nothing, so the script is repeatable. `--purge` destroys the
# volume, which is the point: a previous run's data would make "the row
# survived" true for the wrong reason.
vm "~/bin/pgpod pooler delete ${POOLER} 2>/dev/null; ~/bin/pgpod delete ${CLUSTER} --purge 2>/dev/null; true" >/dev/null

vm "cat > /tmp/${CLUSTER}.yaml" <<YAML
apiVersion: pgpod/v1
kind: Cluster
metadata:
  name: ${CLUSTER}
spec:
  imageName: docker.io/library/postgres:18
  bootstrap:
    initdb:
      database: appdb
      owner: app
  postgresql:
    parameters:
      work_mem: 8MB
YAML

vm "cat > /tmp/${POOLER}.yaml" <<YAML
apiVersion: pgpod/v1
kind: Pooler
metadata:
  name: ${POOLER}
spec:
  clusters:
    - cluster: ${CLUSTER}
  port: ${POOLER_PORT}
  pgDoorman:
    poolMode: transaction
    poolSize: 20
    maxHold: "60s"
YAML

vm "~/bin/pgpod apply -f /tmp/${CLUSTER}.yaml"
vm "~/bin/pgpod apply -f /tmp/${POOLER}.yaml"

vm "podman exec pgpod-${CLUSTER}-1 psql -X -q -v ON_ERROR_STOP=1 -h /pgdata/run -U postgres -d appdb \
      -c \"CREATE TABLE survivor(id int, note text); INSERT INTO survivor VALUES (42,'written before the reboot'); GRANT SELECT ON survivor TO app\""
# Force it to disk: what comes back must be the volume, not a page cache.
vm "podman exec pgpod-${CLUSTER}-1 psql -X -tA -h /pgdata/run -U postgres -c CHECKPOINT" >/dev/null

PORT_BEFORE="$(vm "~/bin/pgpod status ${CLUSTER} -o json | jq -r '.instances[0].host_port'")"
ok "instance on port ${PORT_BEFORE}, pooler on ${POOLER_PORT}"

# ---------------------------------------------------------------------------
log "stopping the daemon must NOT stop the databases"
# ---------------------------------------------------------------------------

BEFORE="$(vm 'podman ps -q | sort')"
vm 'systemctl --user stop pgpod-daemon.service'
AFTER="$(vm 'podman ps -q | sort')"
[ "${BEFORE}" = "${AFTER}" ] \
  || fail "stopping the daemon killed containers — the unit needs KillMode=process
before: ${BEFORE}
after:  ${AFTER}"
ok "containers outlive the daemon"
vm 'systemctl --user start pgpod-daemon.service'

# ---------------------------------------------------------------------------
log "REBOOT — nothing is touched after this point"
# ---------------------------------------------------------------------------

reboot_guest

# Wait for the *daemon* to finish, not for a fixed sleep: the claim is "no
# human action", and polling a readiness signal is not an action.
for _ in $(seq 1 60); do
  [ "$(vm 'systemctl --user is-active pgpod-daemon.service' 2>/dev/null)" = "active" ] && break
  sleep 2
done

BOOT_AT="$(vm 'date -d "$(uptime -s)" +%s')"
UP_AT="$(vm "date -d \"\$(systemctl --user show pgpod-daemon.service -p ActiveEnterTimestamp --value)\" +%s")"
echo "  boot to daemon active: $((UP_AT - BOOT_AT))s"

# ---------------------------------------------------------------------------
log "asserting recovery"
# ---------------------------------------------------------------------------

RUNNING="$(vm 'podman ps --format "{{.Names}}"' | sort | tr '\n' ' ')"
case "${RUNNING}" in
  *"pgpod-${CLUSTER}-1"*) ok "the instance came back on its own" ;;
  *) fail "the instance did not come back. running: ${RUNNING}" ;;
esac
case "${RUNNING}" in
  *"pgpod-pooler-${POOLER}"*) ok "the pooler came back on its own" ;;
  *) fail "the pooler did not come back. running: ${RUNNING}" ;;
esac

# A pooler can start and then exit seconds later — it renders its whole
# config on every start. Settle before believing it.
sleep 15
vm "podman ps --format '{{.Names}}'" | grep -q "pgpod-pooler-${POOLER}" \
  || fail "the pooler started and then died — check: podman logs pgpod-pooler-${POOLER}"
ok "the pooler stayed up"

ROW="$(vm "podman exec pgpod-${CLUSTER}-1 psql -X -tA -h /pgdata/run -U postgres -d appdb \
          -c 'SELECT note FROM survivor'")"
[ "${ROW}" = "written before the reboot" ] || fail "the data did not survive: '${ROW}'"
ok "the row written before the reboot is still there"

PORT_AFTER="$(vm "~/bin/pgpod status ${CLUSTER} -o json | jq -r '.instances[0].host_port'")"
[ "${PORT_AFTER}" = "${PORT_BEFORE}" ] \
  || fail "the host port moved: ${PORT_BEFORE} -> ${PORT_AFTER}"
ok "the host port did not move"

# The published host port is back on the same number an application would
# already be pointed at.
vm "ss -ltn | grep -q ':${POOLER_PORT}'" \
  || fail "the pooler's host port ${POOLER_PORT} is not listening"
ok "the pooler's host port ${POOLER_PORT} is published again"

# And the whole client path works: connection, SCRAM passthrough against
# the cluster's own credentials, and routing to the backend. Port 6432 is
# what pg_doorman listens on *inside* the container — ${POOLER_PORT} is the
# host-side publish, and is asserted separately above.
PW="$(vm "podman secret inspect --showsecret pgpod-${CLUSTER}-app-owner --format '{{.SecretData}}'")"
THROUGH="$(vm "podman run --rm --network pgpod-${CLUSTER} -e PGPASSWORD='${PW}' \
              docker.io/library/postgres:18 psql -X -tA \
              'postgresql://app@pgpod-pooler-${POOLER}:6432/appdb' \
              -c 'SELECT note FROM survivor'")"
[ "${THROUGH}" = "written before the reboot" ] \
  || fail "reading through the pooler failed: '${THROUGH}'"
ok "a real client authenticates and reads through the pooler"

# Bootstrap must not have re-run, and the shutdown should have been clean —
# `was interrupted` means every boot pays crash-recovery time.
vm "podman logs pgpod-${CLUSTER}-1 2>&1 | grep -q 'already initialised'" \
  || fail "the agent re-ran bootstrap over a populated PGDATA"
ok "bootstrap was skipped on the existing PGDATA"

if vm "podman logs pgpod-${CLUSTER}-1 2>&1 | grep -q 'database system was shut down'"; then
  ok "the shutdown was clean (no crash recovery)"
else
  echo "  note: the last shutdown was not clean — every boot will pay WAL replay."
fi

# Prove it was the *unit* that did this, not something incidental. The
# daemon's own journal line for this boot is the direct evidence.
#
# Note what cannot be used here: `pgpod daemon --once` is refused while the
# unit holds the daemon lock, which is correct — two resumes must not race
# — but it means the check has to read the journal rather than re-run the
# work.
BOOT_LOG="$(vm 'journalctl --user -u pgpod-daemon.service -b --no-pager -o cat')"
case "${BOOT_LOG}" in
  *"boot recovery:"*) ok "the daemon logged its boot recovery this boot" ;;
  *) fail "no boot-recovery line in the daemon journal for this boot:
${BOOT_LOG}" ;;
esac
case "${BOOT_LOG}" in
  *"0 missing, 0 failed"*) ok "it reported nothing missing and nothing failed" ;;
  *) fail "the daemon reported problems at boot:
${BOOT_LOG}" ;;
esac

# And the lock is genuinely held, so a second resume cannot race the unit.
vm '~/bin/pgpod daemon --once' >/dev/null 2>&1 \
  && fail "a second daemon took the lock while the unit holds it"
ok "the daemon lock refuses a concurrent resume"

log "PASS — the host rebooted and the cluster came back with no human action"

echo
echo "Clean up with:"
echo "  ssh ${TARGET} '~/bin/pgpod pooler delete ${POOLER}; ~/bin/pgpod delete ${CLUSTER} --purge'"
