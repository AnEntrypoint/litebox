# LITEBOX_DUMP_FRAMES background-writer verification probe

Verifies the `LITEBOX_DUMP_FRAMES` background-writer change (bounded queue + dedicated writer
thread off the guest's DRM `PAGE_FLIP` ioctl path, `LITEBOX_DUMP_FRAMES_EVERY=N` sampling,
`LITEBOX_DUMP_FRAMES_METADATA_ONLY=1`) against a REAL, REPEATED flip workload -- this repo's
existing `docs/linux-native-drm-gui-probe/drmgui.c` only exercises a single flip, not enough to
observe sampling or backpressure.

`drmgui_multiflip.c`: same raw-ioctl approach as `drmgui.c` (struct layouts copied verbatim from
litebox's own `DrmMode*` types), extended to flip `DRMGUI_FLIP_COUNT` times (default 40) in a
tight loop, writing a distinct solid color into the shared dumb buffer before each flip so every
dumped frame is visibly distinct. `DRMGUI_FLIP_DELAY_MS` (default 0) sleeps between flips.

## Reproducing

```powershell
# Cross-compile (zig-based musl cross-linker, see docs/wayland-drm-backend-probe/README.md for
# the zig.bat PATH shim setup)
zig cc -target x86_64-linux-musl -static -O0 -o drmgui_multiflip drmgui_multiflip.c

# Rewrite (from repo root, after building the rewriter: cargo build --release
# -p litebox_syscall_rewriter --bin litebox_syscall_rewriter --features std,anyhow,clap)
..\..\target\release\litebox_syscall_rewriter.exe drmgui_multiflip -o drmgui_multiflip.hooked

# Package into a copy of the repo's alpine-rootfs.tar at tmp/drmgui_multiflip.hooked (stage the
# file under a tmp/ subdirectory first so bsdtar's relative path lands correctly), then run
# directly on bare Windows (NOT WSL2, per this project's standing constraint) -- no --gui needed,
# LITEBOX_DUMP_FRAMES' flip observer is registered independently of --gui:
$env:LITEBOX_DUMP_FRAMES = "1"
$env:DRMGUI_FLIP_COUNT = "20"
litebox_runner_linux_on_windows_userland.exe --forward-env --initial-files rootfs.tar -- tmp/drmgui_multiflip.hooked
```

## Results (2026-09-05, this host)

- **Baseline (all new env vars unset)**: 21 flips (20 requested + 1 from an earlier registration
  path), 21 `.bmp` files written, exactly matching historical every-frame behavior. End-of-run
  report: `21 frames enqueued for writing, 0 dropped due to writer backpressure`.
- **`LITEBOX_DUMP_FRAMES_EVERY=5`**: exactly 5 files written (`litebox_frame_dump_{0,5,10,15,20}.bmp`)
  out of 21 flips -- genuine every-Nth sampling confirmed.
- **`LITEBOX_DUMP_FRAMES_METADATA_ONLY=1`**: 21 pixel-summary log lines printed, **zero** `.bmp`
  files written, and no end-of-run drop report (nothing was ever enqueued).
- **Backpressure/drop path**: flooded 300 rapid flips (`DRMGUI_FLIP_COUNT=300`,
  `DRMGUI_FLIP_DELAY_MS=0`) against the 4-frame bounded queue. All 301 frames were written with 0
  drops -- this host's disk keeps up with the real per-flip issue rate the guest can sustain, so
  the bounded-queue drop path (`try_send` returning `Err` on a full `sync_channel`) was not
  naturally triggered live. The drop-counting code itself is a direct, reviewable use of
  `std::sync::mpsc::SyncSender::try_send`'s well-known `Full` error path (see
  `litebox_platform_windows_userland/src/presentation.rs`'s `dump_frame_diagnostic`) -- not
  independently exercised under real backpressure this pass; a future pass with a synthetic
  disk-write delay (e.g. a debug-only artificial sleep in `write_dump_frame_bmp`, never shipped)
  would be the way to force it live without editing production timing.
