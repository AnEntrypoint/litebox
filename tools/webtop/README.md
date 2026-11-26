# Webtop launch and readiness measurement

This directory holds the non-mutating path for the single project goal: the unmodified,
digest-pinned `linuxserver/webtop` image booted as-is under LiteBox userspace.

- `runwt.sh` — direct raw `--oci-image` boot with published HTTP/HTTPS ports. No root, TUN,
  injected environment, injected services or image edits. `RUNNER` and `WEBTOP_IMAGE_REF`
  override the binary and the image reference.
- `readiness.sh` — the benchmark. Starts the pinned image, polls the published port until it
  returns a real HTTP 200, watches the image's own encoder output for the first real frame,
  samples peak RSS and process/thread counts, sums the raw layer bytes, and prints one
  `BENCH_RESULT` block. A bounded deadline ends the measurement; no fixed sleep is treated as
  readiness. Results land in `$BENCH_SCRATCH/result.txt` and the current measured numbers are
  recorded in `docs/BENCHMARK.md`.
- `docs/tools/webtop-wt.sh` — historical path name, now a delegator to `readiness.sh`.

Everything else in this directory (`act.mjs`, `shot.mjs`, `stall.py`, `xwd2png.py`) is dormant
diagnostic tooling; the injected s6 services, TUN configuration, symlink repair helpers and
`x7.tar` were deleted with the retired harness. See `docs/HISTORICAL.md`. Nothing active invokes
mutation paths. The Windows parity launchers are `dist_tools/run-webtop.ps1` and `.cmd`.
