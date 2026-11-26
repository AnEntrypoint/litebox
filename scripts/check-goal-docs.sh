#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { printf 'FAIL %s\n' "$1" >&2; exit 1; }
require() {
  grep -Eq "$1" "$2" || fail "missing '$1' in $2"
  printf 'PASS require %-42s %s\n' "$1" "$2"
}
deny() {
  if grep -Eqn "$1" "$2"; then
    printf 'FAIL forbidden %s in %s: ' "$1" "$2" >&2
    grep -En "$1" "$2" >&2 || true
    exit 1
  fi
  printf 'PASS deny   %-42s %s\n' "$1" "$2"
}

ACTIVE_DOCS="README.md AGENTS.md dist_tools/README.md dist_tools/README-linux.md docs/HISTORICAL.md"
for doc in $ACTIVE_DOCS; do
  test -f "$doc" || fail "missing active doc $doc"
  require 'unmodified|as-is' "$doc"
  require 'LiteBox' "$doc"
done
require 'under-5|under 5' README.md
require 'digest' README.md
require 'receive_ip_packet|published|publish' AGENTS.md

for doc in docs/LINUX-CHROMIUM.md docs/WEBTOP-IN-LITEBOX-2026-10-10.md docs/WEBTOP-REBUILD-2026-10-12.md docs/HARNESS-LESSONS.md; do
  require '^> \*\*HISTORICAL' "$doc"
done
require 'AGENTS_ARCHIVE' docs/HISTORICAL.md

REF_FILE=dist_tools/webtop-image.ref
test -f "$REF_FILE" || fail "missing $REF_FILE"
require '^docker\.io/linuxserver/webtop@sha256:[0-9a-f]{64}$' "$REF_FILE"
test "$(wc -l < "$REF_FILE")" -eq 1 || fail "image ref must be one line"

ACTIVE_CODE=".github/workflows/release-linux.yml .github/workflows/release-windows.yml .github/workflows/webtop-base-image.yml dist_tools/run-webtop.sh dist_tools/run-webtop.ps1 dist_tools/run-webtop.cmd tools/webtop/runwt.sh docs/tools/webtop-wt.sh tools/webtop/readiness.sh"
FORBIDDEN='alpine-rootfs\.tar|config_and_run\.sh|--rewrite-syscalls|--initial-files|--program-from-tar|litebox_packager --oci-image|docker (run|build|compose|pull)|dockerd|--tun-device-name|run-alpine|rootfs\.tar|custom-services\.d|x7\.tar|monkey-patch|sleep (60|90|120|180)'
for file in $ACTIVE_CODE; do
  test -f "$file" || fail "missing active path $file"
  deny "$FORBIDDEN" "$file"
done
require 'webtop-image.ref|@sha256:' .github/workflows/release-linux.yml
require 'webtop-image.ref|@sha256:' .github/workflows/release-windows.yml
require 'webtop-image.ref|@sha256:[0-9a-f]{64}' .github/workflows/webtop-base-image.yml
require '\-\-oci-image' tools/webtop/readiness.sh
require '200' tools/webtop/readiness.sh
require 'EncFPS|frame' tools/webtop/readiness.sh
require 'peak|rss|VmRSS' tools/webtop/readiness.sh
require 'CACHED' scripts/gate-window.sh
require 'GATE_TREE' scripts/gate-window.sh
require 'GATE_RERUN_REASON' scripts/gate-window.sh
require 'GATE-AUDIT' docs/GATE-AUDIT.md
python3 -I - <<'PY'
import yaml
from pathlib import Path
files = ['.github/workflows/release-linux.yml', '.github/workflows/release-windows.yml', '.github/workflows/webtop-base-image.yml']
forbidden = ('litebox_packager', 'alpine-rootfs.tar', '--rewrite-syscalls', 'config_and_run.sh', 'docker run', 'dockerd', 'run-alpine')
for name in files:
    data = yaml.safe_load(Path(name).read_text())
    steps = sum((list(job.get('steps', [])) for job in data.get('jobs', {}).values()), [])
    runs = '\n'.join(str(step.get('run', '')) for step in steps) + '\n' + '\n'.join(str(data.get(key, '')) for key in ('env', 'on'))
    hits = [word for word in forbidden if word in runs]
    assert not hits, (name, hits)
    print('PASS yaml', name, 'steps', len(steps))
PY
printf 'PASS goal docs consistency\n'
