#!/usr/bin/env bash
set -euo pipefail
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
image_ref=$(cat "$script_dir/webtop-image.ref")
exec "$script_dir/litebox_runner_linux_userland" --oci-image "$image_ref" \
  --publish 3000:3000 --publish 3001:3001 "$@"
