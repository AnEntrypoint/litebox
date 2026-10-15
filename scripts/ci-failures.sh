#!/usr/bin/env bash
# Summarise the failing jobs of the CI run for one commit, with their error lines.
#
# usage: scripts/ci-failures.sh <sha-prefix> [workflow]   (workflow defaults to CI)
#
# One `gh run view --log-failed` per run (not per job: each per-job call re-downloads the
# whole log). The full log is cached under $CACHE so repeat queries are instant.
set -euo pipefail

sha="${1:?usage: ci-failures.sh <sha-prefix> [workflow]}"
workflow="${2:-CI}"
cache="${CI_FAILURES_CACHE:-${TMPDIR:-/tmp}/ci-failures}"
mkdir -p "$cache"

runs=$(gh run list --branch main --limit 40 --json databaseId,headSha,name,conclusion \
  | python3 -c '
import json, sys
sha, wf = sys.argv[1], sys.argv[2]
for r in json.load(sys.stdin):
    if r["headSha"].startswith(sha) and (r["name"] == wf or r["name"].startswith(wf)):
        print(r["databaseId"], r["name"], r["conclusion"])
' "$sha" "$workflow")

if [ -z "$runs" ]; then
  echo "no runs for $sha on main (workflow $workflow)" >&2
  exit 1
fi

while read -r rid name conclusion; do
  echo "== run $rid [$name] $conclusion"
  if [ "$conclusion" != "failure" ]; then continue; fi
  log="$cache/$rid.log"
  if [ ! -s "$log" ]; then
    gh run view "$rid" --log-failed 2>/dev/null | sed -E 's/(\x1b|\^\[)\[[0-9;]*m//g' > "$log" || true
  fi
  # Strip the "job<TAB>step<TAB>timestamp" prefix so the job name leads each line.
  sed -E 's/^([^\t]*)\t([^\t]*)\t[0-9T:.Z-]+ /\1 :: \2 :: /' "$log" \
    | grep -E ' error(\[E[0-9]+\])?: | FAIL \[| TIMEOUT \[| ABORT \[|panicked at|has overflowed|exit code [1-9]|-->' \
    | sed -E 's/\s+/ /g' | sort -u | head -60 | cut -c1-260
done <<< "$runs"
