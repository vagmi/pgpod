#!/bin/bash
# pgpod test VM: libvirt/QEMU harness for a disposable Ubuntu 26.04 guest.
#
# Adapted from references/sandpod/ops/testvm/vm.sh. The guest is
# Ubuntu-only by design — 26.04 is pgpod's deployment target
# (adrs/03-deployment-target-and-test-harness.md §1).
#
# Most of pgpod's integration tests do NOT need this VM: they need
# rootless podman and disk, which a dev workstation already has. Use the
# VM for what genuinely needs a clean machine — `pgpod doctor` on an
# unprovisioned host, ops/provision-node.sh itself, linger and
# `systemd --user` behaviour, and the reboot-recovery test (adrs/03 §4).

set -uo pipefail

cd "$(dirname "$0")"
HERE="$(pwd)"

BASE_IMAGE="$HERE/images/resolute-server-cloudimg-amd64.img"
OS_VARIANT="${PGPOD_OS_VARIANT:-ubuntu26.04}"
SSH_USER="ubuntu"

CPU=4
MEMORY=8192
VMNAME=pgpod-testvm
SSH_PUBKEY_PATH="${PGPOD_SSH_PUBKEY:-$HOME/.ssh/id_ed25519.pub}"

usage() {
  cat <<EOF
pgpod test VM (Ubuntu 26.04 Resolute Raccoon)

Usage: $0 <command> [name] [options]

Commands:
  create [name]     Create a new VM (default name: $VMNAME)
  start [name]      Start an existing VM
  stop [name]       Stop a running VM
  delete [name]     Delete a VM and its overlay image
  list              List all VMs
  status [name]     Show VM state and IP
  console [name]    Attach to the serial console

Options:
  --cpu=N               vCPUs (default: $CPU)
  --memory=N            Memory in MB (default: $MEMORY)
  --image=FILE          Base qcow2 (default: $BASE_IMAGE)
  --ssh-pubkey=FILE     Public key baked into cloud-init at create time
                        (default: \$PGPOD_SSH_PUBKEY or ~/.ssh/id_ed25519.pub)

Typical flow:
  ./prepare.sh
  $0 create
  $0 status
  ssh ubuntu@<ip>
  # then, on the guest:
  #   sudo bash provision-node.sh && pgpod doctor
EOF
  exit 1
}

# ---------- helpers ----------

vm_exists() { sudo virsh dominfo "$1" &>/dev/null; }

get_vm_ip() {
  local vm_name=$1 attempts=30 i=0
  while [ $i -lt $attempts ]; do
    local ip
    ip=$(sudo virsh domifaddr "$vm_name" 2>/dev/null \
         | grep -oE '([0-9]{1,3}\.){3}[0-9]{1,3}' | head -n 1)
    if [ -n "$ip" ]; then echo "$ip"; return 0; fi
    i=$((i + 1)); sleep 2
  done
  return 1
}

# Render user-data.template with the caller's SSH public key. Prints the
# temp path on stdout so the caller can clean it up.
render_user_data() {
  if [ ! -f "$SSH_PUBKEY_PATH" ]; then
    echo "Error: SSH public key not found at $SSH_PUBKEY_PATH" >&2
    echo "Set PGPOD_SSH_PUBKEY or pass --ssh-pubkey=<file>." >&2
    return 1
  fi
  local pubkey rendered
  pubkey=$(head -n 1 "$SSH_PUBKEY_PATH")
  rendered=$(mktemp -t pgpod-user-data.XXXXXX)
  # `|` delimiter because public keys can contain `/`.
  sed "s|%%SSH_PUBLIC_KEY%%|${pubkey}|" user-data.template > "$rendered"
  echo "$rendered"
}

# libvirt's osinfo-db may predate 26.04. Falling back keeps `create`
# working on an older host instead of failing on a cosmetic hint.
#
# Two things here were wrong and both failed the same way — silently
# choosing a fallback that virt-install then rejected, so `create` had
# never worked on any host:
#
#   * `osinfo-query os short-id` is not a column selector. osinfo-query
#     reads trailing words as `KEY=VALUE` filter conditions, so that
#     spelling exits with "Unable to construct filter" and prints nothing.
#     The test therefore always failed, even where the id existed. The
#     column flag is `-f`.
#   * `ubuntulatest` is not an osinfo id. The real aliases are
#     `ubuntu-lts-latest` and `ubuntu-stable-latest`.
#
# `virt-install --osinfo list` is asked rather than osinfo-query, because
# virt-install is the thing that has to accept the answer — and it lists
# aliases (`ubuntu26.04, ubunturesolute`) that osinfo-query's short-id
# column does not.
resolve_os_variant() {
  local known candidate
  known=$(virt-install --osinfo list 2>/dev/null | tr ',' '\n' | tr -d ' ')
  if [ -z "$known" ]; then
    # No osinfo at all: let virt-install decide rather than guessing, and
    # say so, because the guest may get generic virtio defaults.
    echo "generic"
    return
  fi

  # Most specific first. `generic` last: it always exists, and it is
  # better than failing, but it disables the device tuning osinfo exists
  # to provide.
  for candidate in "$OS_VARIANT" ubuntu-lts-latest linux2024 linux2022 generic; do
    if printf '%s\n' "$known" | grep -qx "$candidate"; then
      echo "$candidate"
      return
    fi
  done
  echo "generic"
}

# ---------- commands ----------

create_vm() {
  local vm_name=$1
  vm_exists "$vm_name" && { echo "Error: VM '$vm_name' already exists."; return 1; }
  if [ ! -f "$BASE_IMAGE" ]; then
    echo "Error: base image not found: $BASE_IMAGE"
    echo "Run ./prepare.sh first."
    return 1
  fi

  local variant
  variant=$(resolve_os_variant)
  [ "$variant" = "$OS_VARIANT" ] || \
    echo "note: osinfo-db has no '$OS_VARIANT'; using '$variant'"

  echo "Creating VM: $vm_name  ($CPU vCPU, ${MEMORY}MB)"
  mkdir -p "$HERE/images"
  local overlay="$HERE/images/${vm_name}.qcow2"
  qemu-img create -f qcow2 -b "$(realpath "$BASE_IMAGE")" -F qcow2 "$overlay" \
    || { echo "Error: failed to create overlay image"; return 1; }

  local user_data
  user_data=$(render_user_data) || return 1
  trap 'rm -f "$user_data"' RETURN

  sudo virt-install \
    --accelerate \
    --name "$vm_name" \
    --memory "$MEMORY" \
    --vcpus "$CPU" \
    --cpu host-passthrough \
    --disk path="$overlay",format=qcow2,bus=virtio \
    --cloud-init root-password-file="$HERE/rootpasswd",user-data="$user_data",meta-data="$HERE/meta-data" \
    --os-variant "$variant" \
    --graphics none \
    --console pty,target_type=serial \
    --network network=default \
    --import

  local status=$?
  if [ $status -ne 0 ]; then
    echo "Error: virt-install failed (exit $status)"
    return $status
  fi

  echo "VM '$vm_name' created."
  local ip
  if ip=$(get_vm_ip "$vm_name"); then
    echo "SSH:     ssh $SSH_USER@$ip"
    echo "Next:    scp ../provision-node.sh $SSH_USER@$ip:/tmp/ && \\"
    echo "         ssh $SSH_USER@$ip 'sudo bash /tmp/provision-node.sh'"
  else
    echo "Could not determine the VM's IP; try: $0 status $vm_name"
  fi
}

start_vm() {
  local vm_name=$1
  vm_exists "$vm_name" || { echo "Error: VM '$vm_name' does not exist."; return 1; }
  if sudo virsh domstate "$vm_name" | grep -q running; then
    echo "VM '$vm_name' already running."
  else
    sudo virsh start "$vm_name" || return 1
  fi
  get_vm_ip "$vm_name"
}

stop_vm() {
  local vm_name=$1
  vm_exists "$vm_name" || { echo "Error: VM '$vm_name' does not exist."; return 1; }
  if sudo virsh domstate "$vm_name" | grep -q "shut off"; then
    echo "VM '$vm_name' already stopped."; return 0
  fi
  sudo virsh shutdown "$vm_name"
  local i=0
  while [ $i -lt 30 ]; do
    sudo virsh domstate "$vm_name" | grep -q "shut off" && { echo "Stopped."; return 0; }
    i=$((i + 1)); printf '.'; sleep 2
  done
  echo; echo "Graceful shutdown timed out; forcing off."
  sudo virsh destroy "$vm_name"
}

delete_vm() {
  local vm_name=$1
  vm_exists "$vm_name" || { echo "Error: VM '$vm_name' does not exist."; return 1; }
  sudo virsh domstate "$vm_name" | grep -q running && stop_vm "$vm_name"
  sudo virsh undefine "$vm_name" --remove-all-storage
  rm -f "$HERE/images/${vm_name}.qcow2"
  echo "VM '$vm_name' deleted."
}

vm_status() {
  local vm_name=$1
  vm_exists "$vm_name" || { echo "Error: VM '$vm_name' does not exist."; return 1; }
  sudo virsh dominfo "$vm_name"
  if sudo virsh domstate "$vm_name" | grep -q running; then
    echo
    local ip
    ip=$(get_vm_ip "$vm_name") && echo "IP:  $ip" && echo "SSH: ssh $SSH_USER@$ip"
  fi
}

# ---------- argument parsing ----------

[ $# -ge 1 ] || usage
COMMAND=$1; shift
NAME="$VMNAME"
if [ $# -ge 1 ] && [[ "$1" != --* ]]; then NAME=$1; shift; fi
for arg in "$@"; do
  case "$arg" in
    --cpu=*)         CPU="${arg#*=}" ;;
    --memory=*)      MEMORY="${arg#*=}" ;;
    --image=*)       BASE_IMAGE="${arg#*=}" ;;
    --ssh-pubkey=*)  SSH_PUBKEY_PATH="${arg#*=}" ;;
    *) echo "Unknown option: $arg"; usage ;;
  esac
done

case "$COMMAND" in
  create)  create_vm "$NAME" ;;
  start)   start_vm "$NAME" ;;
  stop)    stop_vm "$NAME" ;;
  delete)  delete_vm "$NAME" ;;
  list)    sudo virsh list --all ;;
  status)  vm_status "$NAME" ;;
  console) sudo virsh console "$NAME" ;;
  *)       usage ;;
esac
