#!/usr/bin/env bash
# Run a command in the Linux build container with the repository mounted.
#
#   tools/ci/run.sh cargo test --workspace
#
# Works with podman or docker. The first run builds the image; build products
# and the cargo registry live in named volumes, so later runs are incremental.
# The mount tests need /dev/fuse and SYS_ADMIN, which is why both are passed.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
engine="${CONTAINER_ENGINE:-$(command -v podman || command -v docker || true)}"
[ -n "$engine" ] || { echo "install podman or docker to run the Linux tests" >&2; exit 1; }
image="${ICLOUD_CI_IMAGE:-icloud-linux-ci}"

if ! "$engine" image inspect "$image" >/dev/null 2>&1; then
  "$engine" build -t "$image" -f "$root/tools/ci/Containerfile" "$root/tools/ci"
fi

exec "$engine" run --rm --device /dev/fuse --cap-add SYS_ADMIN \
  -v "$root":/src \
  -v icloud-linux-target:/cargo-target \
  -v icloud-linux-cargo:/usr/local/cargo/registry \
  -w /src "$image" "$@"
