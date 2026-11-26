#!/usr/bin/env bash
set -euo pipefail
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
exec bash "$repo/tools/webtop/readiness.sh" "$@"
