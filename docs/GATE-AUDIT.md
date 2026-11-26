# Gate audit: `litebox-smoke` on `34001d81` (2026-11-26)

## What the ledger shows

`/config/workspace/tmp/gate/history.jsonl`, verbatim rows for that label and tree:

```
{"label": "litebox-smoke", "tree": "34001d81", "waited": 1, "ran": 79, "peakMb": 1253, "code": 12, "ended": "2026-11-26T10:19:40.152Z"}
{"label": "litebox-smoke", "tree": "34001d81", "waited": 1, "ran": 6, "peakMb": 830, "code": 0, "ended": "2026-11-26T10:25:56.297Z"}
```

## Why the second run was needed

The first window failed with `SMOKE_DONE 12`, so the gate correctly had nothing to cache:
`gate-ticket.cjs` only prints `CACHED` when a row for the same label and `GATE_TREE` has
`code === 0`. Two independent causes produced that failure:

1. The published-port check asked the pinned alpine image for `/bin/busybox httpd`, but that
   busybox build has no `httpd` applet (`httpd: applet not found`), so the guest never listened
   and `curl` returned `000`.
2. The Webtop warm-up command (`/bin/true`) exited `rc=11`, and the smoke window counted that as
   a failure bit instead of recording it as measurement evidence.

Between the two runs the repository tree did **not** change (`GATE_TREE=34001d81` for both), but
the witness did: the scratch `smoke-window.sh` was edited to prove publishing with a host-compiled
static guest HTTP server (`published_http_code=200`) and to record the Webtop warm-up exit code as
evidence rather than a failure. That is exactly the standing rule "a red check is rerun alone when
its code or its witness changed", so the second run was legitimate. The defect the audit found is
that the witness path was a scratch launcher with no guard and no record of why a rerun was
allowed: nothing consulted label/tree before queueing, and the reason lived only in the session.

## The guard now in the tree

`scripts/gate-window.sh <label> <window-script> <result-file> [wait-seconds]` replaces the ad-hoc
launcher. It resolves `GATE_TREE` from `git rev-parse --short HEAD` (or the caller's `GATE_TREE`),
reads the same `GATE_HISTORY` the gate uses, and decides before a ticket exists:

- a passing row for that label/tree, no `GATE_FORCE`: print
  `CACHED <label> on <tree> already passed; GATE_FORCE=1 reruns it`, write `<PART>_DONE 0`, queue
  nothing;
- only failing rows, no `GATE_FORCE`: print `BLOCKED`, exit 2, queue nothing — an unexplained
  rerun cannot happen by accident;
- `GATE_FORCE=1` requires a non-empty `GATE_RERUN_REASON`, which is written into the result file
  as a `FORCED` line; the gate then appends its own history row;
- no rows: queue the detached gate ticket, wait once on the gate PID, and append
  `<PART>_DONE <exit code>` as the result file's last line.

`gate-ticket.cjs`'s own success-only `CACHED` check remains as defence in depth; it was not
modified, because the standing rules confine this peer to the `litebox` checkout. The shared gate
is read, not written.

`scripts/check-gate-guard.sh` proves all four decisions with a stub gate and then checks the two
real things: the audited `litebox-smoke`/`34001d81` pair now prints `CACHED` without invoking any
gate, and the real gate prints `CACHED` for that pair without creating a ticket.
