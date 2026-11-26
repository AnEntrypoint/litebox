# Pending validation

Phase A records each change with the witness that proves it and the red check that must fail if
the change is wrong. Phase B runs every witness once on the final tree, fixes failures, and
pushes once.

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
