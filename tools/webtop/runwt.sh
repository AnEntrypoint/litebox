#!/usr/bin/env bash
set -euo pipefail
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
runner=${RUNNER:-$repo/target/release/litebox_runner_linux_userland}
image_ref=${WEBTOP_IMAGE_REF:-$(cat "$repo/dist_tools/webtop-image.ref")}
exec "$runner" --oci-image "$image_ref" --publish 3000:3000 --publish 3001:3001 "$@"
