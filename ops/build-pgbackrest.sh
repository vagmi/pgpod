#!/bin/bash
# Build the self-contained pgBackRest bundle and install it where the
# daemon expects to find it.
#
# pgBackRest is dynamically linked against ~27 shared objects, so unlike
# `pgpod-agent` it cannot be shipped as one static file. This extracts the
# binary *and* every library it needs *and* the dynamic loader, then
# rewrites the binary'"'"'s ELF headers so it finds all of them itself:
#
#   interpreter -> /opt/pgpod/lib/ld-linux-x86-64.so.2
#   RPATH       -> /opt/pgpod/lib
#
# Patching rather than wrapping matters. A wrapper script that invoked the
# loader by hand worked for the parent process and broke every worker:
# pgBackRest forks its local workers by re-executing itself through
# /proc/self/exe, which resolves to the real binary and never sees the
# wrapper.
#
# It also carries the two things `ldd` cannot tell you about, because they
# are opened at runtime rather than linked: glibc'"'"'s NSS modules, and the
# CA certificate store OpenSSL needs to verify a TLS peer. Missing the
# latter is invisible until the first backup to a real object store.
#
# Nothing from the target image is used, so the bundle runs on any Linux
# image regardless of its libc — verified on Debian 12 (older glibc than
# the build) and on Alpine (musl, no glibc at all). That is what keeps
# pgpod'"'"'s bring-your-own-image promise intact for backups (ADR 04 §2).
#
# Usage:
#   ops/build-pgbackrest.sh                    # host architecture
#   ops/build-pgbackrest.sh --from IMAGE       # different source image
#   PGPOD_HOME=/tmp/x ops/build-pgbackrest.sh  # install elsewhere

set -euo pipefail

cd "$(dirname "$0")/.."

# Debian-based, and the same family the official PostgreSQL images use, so
# the pgdg package is the one pgBackRest's own project publishes.
FROM_IMAGE="docker.io/library/postgres:18"
while [ $# -gt 0 ]; do
  case "$1" in
    --from) FROM_IMAGE="$2"; shift 2 ;;
    *) echo "Unknown argument: $1" >&2; exit 1 ;;
  esac
done

# Baked into the patched binary, so it must match
# pgpod_core::container::PGBACKREST_BUNDLE.
PGBACKREST_BUNDLE=/opt/pgpod

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64)  ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

# Mirrors pgpod_core::PathLayout — keep in step with it.
if [ -n "${PGPOD_HOME:-}" ]; then
  DEST_DIR="$PGPOD_HOME/data/pgbackrest-$ARCH"
else
  DEST_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/pgpod/pgbackrest-$ARCH"
fi

PODMAN=(podman)
if [ -n "${PGPOD_PODMAN_SOCKET:-}" ]; then
  PODMAN=(podman --url "unix://$PGPOD_PODMAN_SOCKET")
fi

# This workstation has a credential helper invoked for *every* registry
# that exits non-zero, breaking docker.io and ghcr.io alike. An empty
# authfile sidesteps it and is harmless where the helper is fine.
AUTHFILE="$(mktemp)"
echo '{"auths":{}}' > "$AUTHFILE"
trap 'rm -f "$AUTHFILE"' EXIT

echo "== extracting pgbackrest from $FROM_IMAGE"
rm -rf "$DEST_DIR"
install -d -m 0755 "$DEST_DIR"

# Runs as container-root, which rootless podman maps to *this* user, so
# everything lands owned by us on the host side.
"${PODMAN[@]}" run --rm --authfile "$AUTHFILE" \
  -e "FROM_IMAGE_LABEL=$FROM_IMAGE" \
  -v "$DEST_DIR:/out" "$FROM_IMAGE" bash -eu -c '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq >/dev/null
    apt-get install -y -qq --no-install-recommends pgbackrest patchelf ca-certificates >/dev/null

    BIN="$(command -v pgbackrest)"
    install -d -m 0755 /out/bin /out/lib /out/share

    # Every "x => /path" dependency, dereferenced past its symlinks.
    ldd "$BIN" | awk "/=>/ {print \$3}" | grep -E "^/" | sort -u \
      | xargs -I{} cp -L {} /out/lib/

    # The loader itself: the ldd line that has no "=>" and is a real path.
    LOADER="$(ldd "$BIN" | awk "!/=>/ {print \$1}" | grep -E "^/" | head -1)"
    cp -L "$LOADER" /out/lib/
    LOADER_NAME="$(basename "$LOADER")"

    install -m 0755 "$BIN" /out/bin/pgbackrest

    # Make the binary self-contained rather than wrapping it.
    #
    # A wrapper that invoked the loader by hand worked for the parent
    # process and failed for every worker: pgBackRest forks its local
    # workers by re-executing itself via /proc/self/exe, which resolves to
    # the real binary and skips the wrapper entirely. `--cmd` does not
    # help — that governs *remote* processes only.
    #
    # Patching the ELF fixes it at the source: the kernel uses our
    # interpreter and our RPATH no matter who execs it, wrapper or not.
    patchelf --set-interpreter "'"$PGBACKREST_BUNDLE"'/lib/$LOADER_NAME" \
             --force-rpath --set-rpath "'"$PGBACKREST_BUNDLE"'/lib" \
             /out/bin/pgbackrest

    # Two things ldd cannot see, because they are opened at *runtime*
    # rather than linked. Both were missed by the first version of this
    # script, and neither shows up in a posix-repository test.
    #
    # 1. NSS modules. glibc resolves hostnames through dlopen()ed
    #    libnss_*.so.2 plugins chosen by /etc/nsswitch.conf. Since glibc
    #    2.34 `files` and `dns` are compiled into libc itself, so a bundle
    #    built on a modern base resolves names with nothing extra — which
    #    is why this went unnoticed. Older bases still need the modules,
    #    and `--from` makes that reachable, so copy them when they exist.
    #    libc.so.6 carries our RPATH, and dlopen resolves against the
    #    calling object'"'"'s RPATH, so they are found in the bundle.
    cp -L /lib/*/libnss_files.so.2 /lib/*/libnss_dns.so.2 /out/lib/ 2>/dev/null || true

    # 2. The CA store. OpenSSL opens it by path at runtime, so TLS
    #    verification silently depends on the *target* image having
    #    certificates — which is exactly the coupling this bundle exists
    #    to remove. Verified: the stock postgres:18 and Alpine images have
    #    no /usr/lib/ssl/certs/ca-certificates.crt, so an S3 or GCS
    #    repository fails with "unable to get local issuer certificate"
    #    while a posix repository is perfectly happy. pgpod renders
    #    repoN-storage-ca-file to point here.
    cp -L /etc/ssl/certs/ca-certificates.crt /out/share/ca-bundle.crt
    test -s /out/share/ca-bundle.crt   # empty is worse than missing

    # RPATH on the libraries too. DT_RPATH is inherited by transitive
    # lookups where DT_RUNPATH is not, and these libraries depend on each
    # other — libssl needs libcrypto, libkrb5 needs libk5crypto. Without
    # this the executable resolves and its dependencies do not.
    for lib in /out/lib/*; do
      case "$(basename "$lib")" in
        ld-*) continue ;;   # never patch the loader
      esac
      patchelf --force-rpath --set-rpath "'"$PGBACKREST_BUNDLE"'/lib" "$lib" || true
    done

    # Provenance, so what is mounted into every database container is
    # traceable rather than mysterious.
    {
      echo "source_image=$FROM_IMAGE_LABEL"
      echo "pgbackrest_version=$(pgbackrest version | awk "{print \$2}")"
      echo "built_on=$(. /etc/os-release; echo "$PRETTY_NAME")"
      echo "loader=$LOADER_NAME"
      echo "glibc=$(/out/lib/$LOADER_NAME --version | head -1 | grep -oE "[0-9]+\.[0-9]+" | head -1)"
      echo "ca_bundle_bytes=$(stat -c %s /out/share/ca-bundle.crt)"
      echo "built_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    } > /out/BUNDLE

    # Readable and executable by any uid: the container runs as the
    # image'"'"'s postgres user, not as us (ADR 04 §2).
    chmod -R a+rX /out
  ' 2>&1 | tail -5

echo "== verifying the bundle runs where it was not built"
# Alpine is the hostile case: musl, no glibc, and no CA certificates
# anywhere in the image. If it works here it works on any glibc image too.
VERIFY_IMAGE=docker.io/library/postgres:18-alpine
"${PODMAN[@]}" run --rm --authfile "$AUTHFILE" --user 70:70 \
  -v "$DEST_DIR:/opt/pgpod:ro" "$VERIFY_IMAGE" \
  /opt/pgpod/bin/pgbackrest version

# `version` proves almost nothing: it touches no DNS, no TLS and no
# runtime-loaded module. The things this bundle gets wrong are invisible
# until the first backup to a real object store, so reach one.
#
# A deliberately invalid key against a real S3 endpoint exercises the whole
# chain — hostname resolution through glibc's NSS, a TLS handshake verified
# against the *bundle's* CA store, an HTTP request, and request signing —
# and ends in an S3-level error, which is the proof. Both earlier versions
# of this script failed here: first with a missing libssh2 in the worker
# process, then with "unable to get local issuer certificate".
echo "== verifying DNS and TLS from inside a foreign image"
PROBE_OUT="$(
  "${PODMAN[@]}" run --rm --authfile "$AUTHFILE" --user 70:70 \
    -v "$DEST_DIR:/opt/pgpod:ro" \
    -e PGBACKREST_REPO1_S3_KEY=AKIAIOSFODNN7EXAMPLE \
    -e PGBACKREST_REPO1_S3_KEY_SECRET=wJalrXUtnFEMIbogus \
    "$VERIFY_IMAGE" \
    /opt/pgpod/bin/pgbackrest --stanza=probe --repo1-type=s3 \
      --repo1-s3-bucket=pgpod-bundle-probe --repo1-s3-endpoint=s3.amazonaws.com \
      --repo1-s3-region=us-east-1 --repo1-path=/probe \
      --repo1-storage-ca-file=/opt/pgpod/share/ca-bundle.crt \
      --log-path=/tmp --lock-path=/tmp --log-level-console=error info 2>&1 || true
)"

# Matched on the specific CA failures, not on `CryptoError` generally: a
# certificate *hostname* mismatch is also a CryptoError and says nothing
# about the bundle, so failing on all of them would cry wolf.
case "$PROBE_OUT" in
  *"local issuer"*|*"unable to set user-defined CA"*|*"no certificate or crl found"*)
    echo "ERROR: TLS verification failed from inside $VERIFY_IMAGE." >&2
    echo "The bundled CA store is missing or unusable:" >&2
    echo "$PROBE_OUT" | sed "s/^/    /" >&2
    exit 1 ;;
  *"unable to resolve"*|*"host not found"*|*DnsError*)
    echo "ERROR: DNS resolution failed from inside $VERIFY_IMAGE." >&2
    echo "glibc's NSS modules are probably missing from the bundle:" >&2
    echo "$PROBE_OUT" | sed "s/^/    /" >&2
    exit 1 ;;
  *InvalidAccessKeyId*|*ProtocolError*|*SignatureDoesNotMatch*)
    # Reached S3 and got a real answer. DNS, TLS and signing all work.
    echo "   DNS + TLS + HTTP verified (S3 answered, as expected, with a 403)" ;;
  *)
    # No network in the build environment, most likely. Not a reason to
    # fail a build, but say plainly what was *not* checked.
    echo "   WARNING: could not reach s3.amazonaws.com, so DNS and TLS are" >&2
    echo "   UNVERIFIED. A posix repository will work; an S3/GCS one may not." >&2
    echo "$PROBE_OUT" | tail -3 | sed "s/^/       /" >&2 ;;
esac

echo "== installed $DEST_DIR"
cat "$DEST_DIR/BUNDLE"
du -sh "$DEST_DIR"
