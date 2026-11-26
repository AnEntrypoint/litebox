# End-to-end measurement (2026-11-26)

Host: Linux 7.0.0-1012-oracle, aarch64, 4 CPUs, 24 GiB RAM, uid 1000 (no root, no TUN, no
Docker daemon). Toolchain `stable` = rustc 1.98.1. Tree `46b79af8`.

Image: `docker.io/linuxserver/webtop@sha256:8bb9f1ac0bcf64d6e0be56bcd65dbfa985ee8c2b09c16820ddae29d3e44aeaa3`
(OCI index digest carrying `linux/amd64` and `linux/arm64`). The runner pulled the arm64
manifest's own config: `Entrypoint ["/init"]`, `Cmd null`, 25 image ENV entries including
`DISPLAY=:1` and `SELKIES_ENABLE_BASIC_AUTH=false`. Nothing in the image was edited.

Command: `bash tools/webtop/readiness.sh` in an exclusive gate window, with all 18 raw layers
already cached (the Docker comparison also assumes a local image). It starts
`litebox_runner_linux_userland --oci-image <pin> --publish 3000:3000`, polls for a real HTTP 200
and the first real encoded frame, samples VmRSS and process/thread counts every 0.25 s, and
prints one result block. No fixed sleep is used as readiness.

## Result (verbatim numbers)

```
BENCH_RESULT image_ref=docker.io/linuxserver/webtop@sha256:8bb9f1ac... status=NOT_READY target_s=5
  cold_or_warm_cached_layers_before=18 rootfs_bytes_before=3557082624
  wall_s=3.575 http_200_s=NOT_REACHED first_frame_s=NOT_REACHED readiness_s=NOT_REACHED gap_to_target_s=NOT_REACHED
  peak_rss_mb=696.8 rootfs_bytes=3557082624 layer_count=18 host_procs=1 guest_threads=6
  last_http_code=000 frame_evidence=NONE runner_rc=11
```

- Wall time to the runner's exit: **3.575 s**. Wall time to GUI/readiness: **NOT_REACHED**.
- Peak RSS: **696.8 MiB** (0.25 s sampling). Image bytes: **1,225,750,713** compressed across 16
  layers; rootfs bytes: **3,557,082,624** decompressed raw layer tars across 18 cached files
  (16 Webtop layers + 2 smoke-image layers).
- Process count: **1 host process, 6 guest threads**.
- Cold image acquisition (first run, outside the startup window): **32 s** to pull and
  decompress all 16 layers; a warm-cache boot re-reads them in **5 s**.

## Gap to the under-5-second target

The target is **not met**, and the shortfall is not a few seconds of slowness: the unmodified
image's own `/init` does not reach a GUI on this host. The runner boots the image's own
`ENTRYPOINT` as pid 1 with its own ENV, then the guest's first process faults:

```
WARN litebox_shim_linux: diag-guest-exception: pre-signal snapshot pid=1 tid=1 comm=sh
     exception=Exception(37) kernel_mode=false pc=0xfffffffc138c sp=0xffffffef0550 fault_address=0x8
ERROR litebox_shim_linux::syscalls::signal: fatal signal: terminating task signal=Signal(11)
runner_rc=11
```

The same `Exception(37)`/`Signal(11)` reproduces with the image's own `/bin/echo` and
`/bin/true`, so it is a LiteBox guest-execution gap with this glibc/aarch64 image, not a
Webtop-specific service failure. HTTP 200 and the first real frame are therefore NOT_REACHED,
and `gap_to_target_s` is NOT_REACHED rather than a fabricated number. Fixing it belongs in the
in-memory syscall/ELF shim; changing the image is out of contract. The user's under-5-second
target remains the acceptance criterion for that work.

The published-port path itself is proven end-to-end: the gateway injected
`tcp 10.0.0.1:49152 -> 10.0.0.2:3000 S` during the benchmark, and a separate guest HTTP server
(a host-compiled static diagnostic binary, no image mutation) answered
`published_http_code=200` through `--publish 18086:8080` as uid 1000 with no TUN and no Docker
daemon. Raw OCI boot of unmodified layers is proven by `raw-oci-smoke-ok` from the pinned alpine
smoke image.

## Witnesses (all green on `46b79af8`)

| Witness | Result |
| --- | --- |
| `bash scripts/check-goal-docs.sh` | PASS (docs consistency, YAML parse, digest pin, forbidden active paths) |
| `cargo fmt --all -- --check` | PASS |
| `cargo build --release -p litebox_runner_linux_userland` | `BUILD_DONE 0` |
| `cargo clippy -p <changed crates> --all-targets --all-features -- -D warnings` | `TEST_DONE 0` |
| `cargo nextest run <changed crates> --lib` | 8 passed, 0 skipped |
| `cargo check -p litebox_platform_linux_userland --target x86_64-unknown-linux-gnu` | `CROSS_DONE 0` |
| `cargo check -p litebox_runner_linux_on_windows_userland --target x86_64-pc-windows-gnu` | `CROSS_DONE 0` |
| raw OCI smoke / cache provenance / rootless published port | `SMOKE_DONE 0` |
| exclusive end-to-end readiness measurement | `LIVE_DONE 0` (measurement complete; status NOT_READY with evidence above) |

Integration tests under `litebox_runner_linux_userland/tests/` are not in the nextest witness:
they require a packaged rootfs, host C fixtures and a TUN device, all of which the goal path
deliberately removes. The unit tests covering every changed crate are in the witness.

## Pre-existing condition found and repaired

`origin/main` at `5b0e42a5` did not compile: a botched merge left unclosed delimiters, duplicated
statements and stale API calls in `litebox/src/net/mod.rs`,
`litebox_shim_linux/src/syscalls/{file,net,process}.rs` and four Windows platform files. Those
were repaired by deleting duplicate merge sides and restoring the Windows files to their last
parse-clean revision (`5b7c3ef6`), which already contains the sparse/odd-sized shared-group
carry. `cargo fmt --all -- --check` now parses every file.
