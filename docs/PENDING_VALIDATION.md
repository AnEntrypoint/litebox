# Pending validation

Phase A records each change with the witness that proves it and the red check that must fail if
the change is wrong. Phase B runs every witness once on the final tree, fixes failures, and
pushes once.

## Phase B results (gate audit, tree `8669f4be` + witness fixes)

- `bash -n` on `scripts/gate-window.sh`, `scripts/check-gate-guard.sh`,
  `scripts/check-goal-docs.sh`: PASS.
- `bash scripts/check-goal-docs.sh`: PASS (`DOCS_RC=0`), including the new `CACHED`,
  `GATE_TREE`, `GATE_RERUN_REASON` and `litebox-smoke.*34001d81` requires.
- `bash scripts/check-gate-guard.sh`: PASS 6/6 — cached pass row prints CACHED and queues
  nothing; failed-only row blocks with exit 2; forced rerun records its reason and runs once;
  the same label/tree after that run prints CACHED; the audited `litebox-smoke`/`34001d81` pair
  prints CACHED without invoking a gate; the real gate prints CACHED without creating a ticket.
- Real gate ledger unchanged (14 rows, `litebox-smoke`/`34001d81` still `(12, 79)` and `(0, 6)`),
  no live gate tickets, no leftover processes.
- Rows 3-10 were verified on `46b79af8`; the two gate-audit commits change only shell scripts and
  docs, none of which those windows read, so they were not repeated.

## Phase B results (tree `46b79af8`)

- `scripts/check-goal-docs.sh`: PASS, including YAML parsing of all three release/pin workflows.
- `litebox-final` gate window (`BUILD_DONE 0`, `TEST_DONE 0`, `CROSS_DONE 0`, `SMOKE_DONE 0`,
  `FINAL_DONE 0`): release build, `cargo fmt --all -- --check`, clippy with `-D warnings`,
  nextest 8/8, x86_64 and Windows cross checks, raw OCI smoke, cache provenance and rootless
  published port `200`.
- `litebox-live` exclusive gate window (`LIVE_DONE 0`): readiness measurement complete,
  `status=NOT_READY`; exact numbers and the boot exception are in `docs/BENCHMARK.md`. The gap to
  the under-5-second target is named, not hidden.
- Gate history: 14 litebox windows, 677 s total run, longest 150 s.


| # | Change | Witness | Red check |
| --- | --- | --- | --- |
| 1 | Goal clarification: `README.md`, `AGENTS.md`, `dist_tools/README*.md`, `docs/HISTORICAL.md`, historical banners, `scripts/check-goal-docs.sh` | `bash scripts/check-goal-docs.sh` | A stale active goal, a Docker-as-engine launcher, a moving image tag or an unmarked historical goal doc fails. |
| 2 | Thin release: `release-linux.yml`, `release-windows-alpine.yml`, `webtop-base-image.yml`, `dist_tools/webtop-image.ref` | `bash scripts/check-goal-docs.sh` (YAML parse plus forbidden-step scan) | A packager build, `alpine-rootfs.tar`, ELF rewrite, generated entrypoint or floating tag in a release fails. |
| 3 | Raw OCI library: `litebox_packager` (rustls, portable, raw layers + image config, no binary/mutators) | `cargo build --release -p litebox_runner_linux_userland`; raw `--oci-image` alpine smoke | Any rewritten ELF, flattened rootfs, materialized symlink or synthesized entrypoint fails. |
| 4 | Linux runner raw `--oci-image`, `--publish`, image ENTRYPOINT/CMD/ENV, pid 1 | `cargo build --release -p litebox_runner_linux_userland`; raw OCI smoke booting the image's own `/init` | A rewritten ELF, a host-path program load on the OCI path, or a missing image ENV/ENTRYPOINT fails. |
| 5 | Shared rootless gateway: `litebox_userspace_net`, Linux platform hooks, Windows parity wrapper | live `curl 127.0.0.1:<published>` against a guest HTTP server with no root/TUN | A TUN/root requirement, a Docker daemon dependency, or a refused published port fails. |
| 6 | x86_64 SIGSYS as a guest-syscall trap; release seccomp never returns EINVAL | `cargo check -p litebox_platform_linux_userland --target x86_64-unknown-linux-gnu` | `SeccompAction::Errno` as the x86_64 release default, or no x86_64 SIGSYS guest dispatch, fails. |
| 7 | Windows parity: raw `--oci-image`, image-config defaults, unchanged `--publish` | `cargo check -p litebox_runner_linux_on_windows_userland --target x86_64-pc-windows-gnu` | A rewritten-layer call on the Windows OCI path fails. |
| 8 | Retire legacy launchers/benchmarks; add `tools/webtop/readiness.sh` | `bash scripts/check-goal-docs.sh`; benchmark run | A fixed sleep as readiness, an injected service, a patched image file or a pruned rootfs fails. |
| 9 | Format, lint, tests | `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings` on the changed crates, `cargo nextest run` on the changed crates | Any warning or failing test fails. |
| 9a | Pre-existing botched merge repair (`litebox/src/net/mod.rs`, `litebox_shim_linux/src/syscalls/{file,net}.rs`, four Windows platform files) | `cargo fmt --all -- --check` parses every file; `cargo build --release -p litebox_runner_linux_userland` | A duplicated statement, missing delimiter or dangling API reference fails. |
| 10 | End-to-end measurement on the final tree | `bash tools/webtop/readiness.sh` with exact wall time, peak RSS, image bytes, process count | A skipped live check, unexplained warning, flaky result or an unmeasured under-5-second claim fails. |
| 11 | Guarded gate launcher `scripts/gate-window.sh` | `bash scripts/check-gate-guard.sh` | A passing label/tree that queues again, a failed label/tree that reruns without a reason, or a missing `<PART>_DONE` line fails. |
| 12 | Gate audit record and rules: `docs/GATE-AUDIT.md`, `AGENTS.md`, `scripts/check-goal-docs.sh` | `bash scripts/check-goal-docs.sh`; `bash -n` on both new scripts | A missing CACHED/GATE_TREE contract, or a missing reason for the historical rerun, fails. |
