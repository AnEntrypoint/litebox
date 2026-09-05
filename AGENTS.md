# litebox — current state (2026-09-05)

This file is the authoritative, CURRENT-STATE picture of what works, what's broken, and what to
do next — consolidated, not historical. It does not narrate how each conclusion was reached; it
states what is true right now. The full pass-by-pass forensic history (every investigation,
every retracted hypothesis, every dead end) is preserved in `docs/AGENTS_ARCHIVE_2026-09-03.md`
(up to 2026-09-03) and `docs/AGENTS_ARCHIVE_2026-09-05.md` (2026-09-03 through 2026-09-05) — read
those only if you need the detailed reasoning trail behind something stated here, not as a
starting point for new work.

This file is also the single source of truth for standing rules, hard constraints, and durable
lessons. Any future "remember this" belongs here, not in a separate memory file or a new pass
narrative appended to the bottom.

## Standing lessons and hard constraints (read before doing anything)

- **No WSL or hypervisor, ever, for anything litebox-related** (building, running, verifying) --
  litebox's whole premise is running unmodified Linux ELF binaries directly on bare Windows via
  syscall rewriting; reaching for WSL2/Hyper-V/any VM undermines that. Cross-compiling FOR Linux
  from any host is fine; RUNNING the result inside a VM/WSL is not -- always run it as a real
  litebox guest process via the matching runner (`litebox_runner_linux_on_windows_userland.exe`
  on Windows, `litebox_runner_linux_userland` on native Linux).
- **`fork_verify.rs` and its whole stale-pointer-healing bug class are Windows-only** -- real
  Linux/macOS `fork()` gives the child identical virtual addresses, so this bug class structurally
  cannot occur there. Never assume a `fork_verify`-attributed crash needs Linux/macOS work, never
  port a `fork_verify.rs` fix to another platform's crate.
- **Never enable `bcdedit /debug on`** without a real kernel debugger already attached and
  confirmed working first -- caused two genuine full-host freezes (hard power-cycle required)
  combined with litebox's exception-heavy workload.
- **For `--gui` visual verification, always use `LITEBOX_DUMP_FRAMES=1`** (numbered `.bmp` +
  non-black-pixel count to stderr), never Windows `PrintWindow`/`CopyFromScreen` (unreliable,
  interfered with by overlapping windows).
- **A pixel/non-black count never identifies WHO painted a frame.** Decode frame structure
  (`advisor/probes/decode_frame.py`) and correlate against `DIAG_TIMELINE execve` log lines
  showing the real argv0 that actually ran, before attributing rendered content to a specific
  component. Cost this project real time repeatedly: weston's own built-in panel was mistaken for
  XFCE's more than once, and real panel-shaped pixels were once attributed to `xfce4-panel` on an
  image that turned out to ship MATE (`xfdesktop`/`xfce4-panel` never existed in that layer).
- **Never run two full-stack litebox verifications concurrently on this host** (including across
  sessions/peers) -- they starve each other, and the failure (log truncated mid-line, no crash, no
  exit) is indistinguishable from a real hang or regression. Check
  `Get-Process litebox_runner_linux_on_windows_userland` (or `tasklist | grep litebox_runner`) and
  coordinate with any peer session before booting.
- **Never time litebox with one host process per datapoint.** A bare process spawn costs
  1.6-2.3s on this host -- that dwarfs real per-exec differences. Hold host-process count
  constant: run N iterations inside ONE guest process and take the delta (n0 vs n_large), never
  compare separate runner invocations. Also hold host load constant (check for a concurrent boot
  or heavy pull skewing the baseline). Establishing a real noise floor (10+ repeated runs of one
  fixed config) before comparing anything is worth the time -- this project measured a ~20-36%
  run-to-run spread on this host that made several earlier single-shot "findings" retract.
- **`log_unsupported!`/refusal errno choice is part of the API contract, not incidental.** EPERM
  ("you may not") lets callers degrade gracefully; EINVAL/ENOSYS ("this is broken/unknown") makes
  them fail hard. Getting this wrong for a legitimately-unsupported-but-refusable capability can
  break unrelated features entirely (a `clone()` namespace-flag EINVAL once silently broke ALL
  PNG/JPEG decoding via glycin's own bwrap-sandboxing fallback logic). Report what's actually
  true, never a fake success or an overly-broad failure.
- **Always build general debug/observability tooling proactively while investigating**, not just
  enough to explain the current bug -- e.g. separating guest stdout from litebox's own log
  stream, capturing a component's own stderr instead of letting it get redirected to an unread
  file (a real, repeated blind spot this project hit more than once: weston's, Xwayland's, and
  xfdesktop's own stderr each went unread for a long stretch before someone finally checked it).
- **Prefer premade, mature libraries over hand-rolled code for well-known problem classes.**
  Standing project mantra, not a one-off: before writing or iterating further on custom logic for
  a solved problem (OCI/registry clients, binary-format parsing, Windows unwind-info construction,
  crash/minidump handling, CLI parsing, serialization, etc.), research what mature, well-maintained
  library already exists. See `docs/premade-library-research.md` for the concrete audit findings so
  far (headline: `litebox_packager --oci-image` already implements the full pull-rewrite-package
  pipeline correctly via `oci-client`; the session's own hand-rolled Python scripts duplicating it
  have been retired). Only hand-roll when research confirms no good existing solution fits a
  genuinely litebox-specific constraint.
- **Isolate the harness before blaming litebox.** Launch guest test probes directly from their own
  minimal tar layer as the runner's top-level program, never through a runtime-built `/bin/sh -c`
  wrapper -- harness bugs (MSYS2 path-mangling, a shell SIGILL) have each produced a false
  "litebox is fundamentally broken" claim that disappeared once the harness variable was removed.
- **Build freestanding guest test binaries on the HOST**, not the guest toolchain (both the
  guest's clang and gcc are broken as of this writing) -- `clang --target=x86_64-unknown-linux-gnu
  -nostdlib -nostdinc -ffreestanding -fno-stack-protector -static -O1`, producing a static
  `ET_EXEC` with raw `syscall` instructions, no libc.
- **Inject a new probe/script into a multi-GB layer via a small `--resume-from` overlay tar**
  (just the new file, `tar cf overlay.tar -C <dir> file`), never by rebuilding the whole layer.
  Needs a real Windows path (not MSYS `/tmp/...`) and `MSYS2_ARG_CONV_EXCL='*'`/
  `MSYS_NO_PATHCONV=1` set, or it fails in two different misleading ways (an ENOENT that looks
  like a missing shebang resolver, or a stack-overflow panic).
- **`linuxserver/webtop:alpine-mate` ships MATE desktop, not XFCE** -- confirmed via direct tar
  listing, zero `xfdesktop`/`xfce4-panel`/`xfsettingsd`/real-`xfwm4` anywhere in the layer.
  `linuxserver/webtop:alpine-xfce` does NOT exist as a tag (404) -- Alpine-based webtop flavors are
  mate/icewm/kde/openbox only, no xfce. Real XFCE tags that do exist: `arch-xfce`, `debian-xfce`,
  `ubuntu-xfce`, `fedora-xfce`. Prefer `debian-xfce` (or `ubuntu-xfce`) over `arch-xfce`: Arch's
  pacman-built layers are the most Windows-path-hostile of the four (colons in package-db dir names,
  case-variant sibling paths), which matters only if extracting to a real host directory -- with
  runtime in-memory OCI loading (see "Container images" below) this stops mattering, but there is no
  reason to prefer arch's layer shape over debian's/ubuntu's regardless. The MATE path's own
  remaining blocker, if MATE support is still wanted, is the unresolved `mate-session`
  `RtlpUnwindPrologue` crash (see below), not an xfconf/wallpaper gap.
- **Repo hygiene**: large binary artifacts (packed layer tars, frame dumps, debug logs) never
  belong in git -- keep them in `.wfgy/` (gitignored) or a durable-but-untracked sibling directory
  like `../litebox-webtop/`. Root-level scratch files (`probe_*.tar`, `*.bmp`, `*.log`) are
  gitignored; if `git add -A`/`git add .` sweeps one in by accident, untrack it rather than leave
  it committed.

## Container images

**Use `litebox_packager --oci-image <ref> --output <tar>` to pull and package any Docker/OCI
image.** This is a complete, working, already-tested Rust tool (`oci-client` + `oci-spec` +
`litebox_syscall_rewriter`) that pulls the image, correctly handles whiteout/opaque-whiteout
files, rewrites every ELF, and produces a bootable tar in one command. It supersedes every
ad-hoc Python script this project previously hand-rolled for the same purpose (`pull_oci_image.py`,
`batch_rewrite_layer.py`, `fetch_container.py` -- all retired, do not recreate them).

**`linuxserver/webtop:alpine-mate` ships MATE, not XFCE** (see standing lessons above).
`linuxserver/webtop:alpine-xfce` does NOT exist (404) -- do not pull it. **`debian-xfce` DOES ship
a real, plain xfce4-session/xfwm4/xfdesktop stack** -- an earlier pass's tar-listing claiming zero
`xfwm4`/`xfdesktop`/`xfsettingsd`/`xfce4-panel`/`xfce4-session`/`startxfce4` anywhere was WRONG
(likely an incomplete/truncated pull, per this project's own repeated "truncated pull looks like a
missing binary" lesson) and has since been corrected by a live `/usr/bin` listing inside a fully
booted guest: all of the above are present, plus 26 `xfce4-*` binaries, `xfwm4`, `xfdesktop`,
`xfsettingsd`, and `startxfce4` itself, alongside `labwc`/`openbox`/`Xwayland`/`selkies-desktop`
(the image supports multiple session types, not just one). Still, **always verify a desktop
image's actual WM/session binaries by direct registry manifest + blob tar-listing, or a live
`/usr/bin` listing inside a fully booted guest, BEFORE assuming either way** -- never trust the tag
name, and never trust a single tar-listing pass without confirming the pull actually completed in
full. A registry pull token + manifest/blob fetch via plain `curl` (no docker/skopeo needed) is
enough to check this in seconds, at zero litebox-side cost, before committing to a 20-minute pull.

**Runtime OCI images are structurally the wrong shape for this project's actual DRM device.**
`litebox`'s virtual DRM device (`litebox_shim_linux/src/syscalls/drm.rs`) is legacy-KMS + dumb-
buffer + XRGB8888 only, one fixed 1920x1080 mode, no atomic modeset, no GBM/EGL. A modern
Selkies/Wayland/GBM-first compositor (what `debian-xfce`/`alpine-openbox` actually ship) is
pushed onto its least-tested software-rendering fallback path. **`Xorg` with the `modesetting`
driver is the correct target, not `Xvfb`** -- `Xvfb` renders into a private buffer and NEVER
touches DRM/KMS at all, so it produces zero page-flips and zero frames, indistinguishable from "the
guest never drew" (confirmed by reading `drm.rs`'s own flip-callback wiring: it fires only from
`DRM_IOCTL_MODE_PAGE_FLIP`/`SETCRTC`, no fbdev fallback exists). `modesetting` drives DRM/KMS dumb
buffers directly -- the exact shape this device implements. Also: **don't boot the container's own
s6-overlay/selkies entrypoint** -- launch the target binary (`Xorg`, `openbox`, whatever) directly
as the runner's top-level program via `--oci-image ... -- /usr/bin/Xorg :0 ...`, per this project's
own harness-isolation lesson one level up; the container's init pulls in Python, WebRTC, hardware
video encoders, and process supervision that contribute zero pixels and add hundreds of syscalls
of irrelevant surface.

**Runtime, in-memory OCI loading (implemented, working)**: extracting an OCI image's layers onto a
real host directory before packaging was fundamentally the wrong approach on Windows -- NTFS case-
insensitivity, reserved colons in pacman package-db paths, and dir/file type collisions across
layers each independently broke `litebox_packager`'s real-directory extraction (three distinct
bugs, patched one at a time, until the pattern itself was recognized as the problem). Fixed:
`litebox_runner_linux_on_windows_userland --oci-image <ref>` pulls, merges (OCI whiteout-aware,
hard-link-aware), and syscall-rewrites every layer entirely in memory via `litebox::fs::tar_ro::
TarRo::from_layers` + `litebox_packager::oci::{pull_layers_in_memory, rewrite_layer_elfs}` -- NO
real host directory is ever created for the rootfs. Verified live: `docker.io/library/busybox:
latest` pulled, merged, rewritten, booted, real guest output. The ahead-of-time
`litebox_packager --oci-image <ref> --output <tar>` path still exists unchanged for callers that
want a pre-built, reusable flat tar (used e.g. by `litebox_runner_linux_on_windows_userland
--initial-files`).

**Real, load-bearing perf constraint already satisfied**: `tar_ro.rs` is on the hot path for every
guest file read -- the multi-layer index is built ONCE at mount time (a single path ->
(layer, offset) map with whiteouts resolved at index-build time), not re-scanned per lookup.

**Memory: pulling a large image's largest layer can OOM -- four real, independent, now-fixed bugs,
plus a genuine remaining constraint that ISN'T a litebox bug.** All four were found via a real
`RUST_BACKTRACE=1`/live-process-monitoring investigation on a real large image
(`linuxserver/webtop:debian-xfce`, largest layer ~910MB compressed / ~2.56GB decompressed):
1. `rewrite_layer_elfs`'s output buffer was sized to the INPUT's exact byte length
   (`Vec::with_capacity(layer_tar.len())`), but the rewritten output is always slightly larger
   (trampoline code, 512-byte tar block rounding) -- the first overflow triggered `Vec`'s
   capacity-DOUBLING policy on an already-multi-GiB buffer (reproduced exactly:
   2,565,094,400 -> 5,130,188,800 bytes, precisely 2x). Fixed: reserve proportional headroom
   (5%, 1MiB floor) instead of an exact-fit capacity.
2. The initial network-pull buffer and the gzip-decompression output buffer both grew from
   `Vec::new()` via the same doubling pattern. Fixed: pre-size both using the manifest's already-
   known compressed size (`layer_desc.size`) and a 4x gzip-ratio estimate.
3. Even with (1)+(2), the decompressed layer (a full ordinary heap `Vec`, ~2.56GB) and
   `rewrite_layer_elfs`'s own output buffer (~2.56GB) were BOTH resident simultaneously during the
   rewrite call -- a genuine ~5.1GB peak of ordinary, non-page-cache-evictable heap memory. Fixed:
   decompression now streams via `std::io::copy` into a temp file, which is then mmap'd and passed
   as the rewrite's `&[u8]` input (zero signature change needed) -- the input is now page-cache-
   evictable under memory pressure instead of hard-pinned.
4. `rewrite_layer_elfs`'s OUTPUT was still a full in-memory `Vec` even after (3). Fixed: it now
   writes its output tar directly to a file on disk (streaming, via a generic `<W: Write>` sink)
   which becomes the on-disk cache file directly (atomic rename into place) -- **the success path
   now holds no whole-layer-sized heap buffer at all**, only a small `BufWriter` chunk plus one
   file's worth of per-entry rewrite data at a time (already true before this fix). A digest+
   rewriter-version-keyed on-disk cache (`.litebox-cache/<digest>_v<version>.tar`, gitignored) also
   means a repeat pull of the same image+layer skips network+decompress+rewrite entirely (verified
   live: cold pull ~7.3s, warm cache-hit ~4.4s for a small image; wins scale with layer size).
   All four fixes verified via real busybox boots on both the runtime and ahead-of-time paths.

**What's left is NOT a litebox bug -- it's host memory exhaustion, confirmed by direct evidence.**
Even with all four fixes landed and active (confirmed via log lines: "rewritten directly to disk,
... streamed directly, no in-memory copy"), pulling `debian-xfce`'s and `alpine-openbox`'s largest
layer still gets the whole process killed by an external low-memory watchdog -- but the killed
process itself showed smooth, unremarkable linear memory growth (not a runaway allocation), and,
decisively, a completely unrelated trivial PowerShell monitoring script running concurrently was
ALSO killed by the same watchdog at the same moment -- ruling out a Rust-specific allocation
failure and confirming genuine physical-memory-threshold contention from OTHER processes. Measured
directly: Chrome (under `chrome-devtools-mcp` browser-automation control) grew ~1.9GB in one
20-minute window while every other process stayed flat, which alone accounted for the entire
free-memory decline on a 16GB host. **Do not chase this as a litebox bug** -- if a large-image pull
gets killed and free host memory is already low (check `Get-CimInstance Win32_OperatingSystem |
Select FreePhysicalMemory` before assuming a regression), the fix is closing memory-hungry
unrelated host processes (browser automation, stale sessions) or waiting for load to clear, not
further litebox-side memory work. A small image (e.g. `busybox:latest`, or any image whose largest
layer is under a few hundred MB) remains reliable regardless of host load.

**Every `linuxserver/webtop:*` flavor shares the same ~519MB blocking layer** (digest
`sha256:22809eee...`, confirmed identical across `alpine-openbox` and `alpine-icewm`'s manifests --
almost certainly the common base rootfs all flavors build on). This means picking a different
desktop flavor within the webtop family does NOT avoid the memory constraint; only a genuinely
different, non-webtop image (or waiting for host memory to clear) does.

**Canonical layer for the (older, hand-assembled, weston-based) XFCE path**:
`.wfgy/xfce-build/layer31_direct_fixed.tar` -- superseded in priority by the stock-image path
above, but still the one confirmed-working weston/XFCE combination from earlier in this project
(see "XFCE/MATE desktop" below for exactly what's confirmed and what isn't).

**Durable copy of the stock MATE webtop image**: `C:\dev\litebox-webtop\webtop_seatd.tar` (2.6GB,
moved out of scratch temp specifically so it survives cleanup). This is what the `mate-session`
crash investigation (see below) reproduces against.

## XFCE/MATE desktop status

**Canonical hand-assembled layer (`layer31_direct_fixed.tar`, weston + XFCE)**: a real XFCE
session comes up and stays alive -- `weston`, `xfwm4`, `xfconfd`, `xfsettingsd`, `xfdesktop`,
`xfce4-panel` all run without crashing, and `xfce4-panel`'s clock genuinely ticks (real time
progression confirmed across decoded frames). Root causes found and fixed along the way, all
landed and verified: `do_kill` used to reject any `tkill`/`tgkill` targeting a remote thread with
a silent ESRCH, deadlocking glibc's `SIGSETXID`/TLS-update handshake (the actual reason multiple
GTK clients hung indefinitely on startup) -- fixed, real cross-thread signal delivery now works.
`sys_setitimer` used to reject any repeating timer (`it_interval != 0`) with ENOSYS, breaking
every GLib main-loop timer (panel clock, animations, cursor blink) -- fixed, timers now rearm
correctly. Missing `gschemas.compiled` (never generated for this layer) caused `at-spi-bus-launcher`
to abort repeatedly -- fixed by compiling schemas at launch. `inotify` unavailability was
investigated and confirmed NOT load-bearing (dbus works fine without it) -- do not re-chase it.

**Real, unresolved gap on this same layer**: the desktop *background* (not just the panel)
renders inconsistently across runs -- sometimes a full filled background, sometimes it drops to
a much sparser state partway through a run and doesn't recover. This was investigated extensively
(shared memory ruled out, scanout/DRM buffer corruption ruled out, the guest's own framebuffer
content is confirmed already-degraded at the source before litebox's capture path ever touches
it) but never fully root-caused; multiple runs of the identical command produce visibly different
outcomes (clean full desktop / a single drop that never recovers / occasional hard hangs /
occasional Xwayland `SIGABRT`), suggesting a real, non-deterministic guest-side or litebox-side
race rather than one deterministic bug. Not resolved as of this writing -- treat any single run's
outcome as one data point, not proof, per the noise-floor lesson above.

**Real, unresolved gap, separately**: real image decoding via `gdk-pixbuf`/GTK was investigated at
length. Current understanding: PNG and JPEG decode correctly once `GDK_PIXBUF_MODULE_FILE` is set
and the loader cache is compiled (both built-in decoders, no sandboxing needed) -- confirmed live,
stock wallpaper and app icons decode fine. SVG has no loader shipped in this layer at all (a
packaging gap, not a litebox bug -- would need librsvg added). XPM is the one genuine anomaly: it
fails specifically when read from litebox's tar-RO filesystem backend combined with the dlopen'd
XPM loader module (`libpixbufloader-xpm.so`) -- the same bytes copied to a writable directory
decode fine, so this is a real, narrow, litebox-facing filesystem-backend interaction, not a
general tar-RO read bug (PNG reads from the same tar-RO backend work fine) and not a general image-
decoding bug. Not yet root-caused to the exact mechanism. A separate, earlier investigation into
sandboxed decoding via `glycin`+`bwrap` found litebox has zero real Linux namespace support
(`unshare`/`clone` with namespace flags are accepted but don't actually isolate anything) --
`bwrap`'s sandbox creation genuinely cannot work without it. That's a real, large, unimplemented
feature (not attempted), but turned out to be moot once the non-sandboxed PNG/JPEG path was
confirmed working directly.

**Stock MATE webtop image (`webtop_seatd.tar`)**: real DRM/wgpu rendering confirmed working
(`labwc`, the real Wayland/DRM compositor this image uses, reaches a healthy idle state with the
window manager genuinely owning the display -- root cause of an earlier "window manager conflict"
symptom was `weston.ini`'s own `xwayland=true` setting launching a competing internal Xwayland/WM;
fixed by disabling it). `TEST_DONE` reached cleanly with a real panel confirmed rendering. Further
progress on this image (MATE-native session, actual desktop content) is blocked by the
`RtlpUnwindPrologue` crash below, since `mate-session`'s own launch sequence is exactly the shape
that triggers it.

**Stock XFCE webtop image**: see "Container images" above (`debian-xfce`/`ubuntu-xfce` preferred,
`alpine-xfce` does not exist, `arch-xfce` deprioritized) -- check `git log` for the latest status
before assuming a result either way.

## The `RtlpUnwindPrologue` crash (genuinely unresolved, do not attempt a fix without new evidence)

A real, host-side (not guest-code, not litebox's syscall-emulation logic) Windows platform bug.
Deterministically triggered by 3 consecutive execs of a large binary (`mate-session --version`
x3 against `webtop_seatd.tar` is the exact, reproducible repro -- no compositor needed, ~90s).
30+ archived investigation passes (`docs/AGENTS_ARCHIVE_2026-09-03.md`) plus several fresh
attempts this session, one root-cause theory already retracted with hard evidence (`.fnent`
proved the suspected missing unwind metadata is actually complete and valid).

**Current, most precise understanding** (2026-09-05, via a genuinely new diagnostic capture that
finally worked -- a prior bug in the crash-diagnostic code itself, an alignment-unsafe read, had
been silently destroying the evidence needed in every earlier attempt; that diagnostic bug is now
fixed): the repeated fault is a real access violation inside `ntdll.dll` itself (confirmed via
page-state introspection: the faulting `rip` sits in real, valid, executable module memory).
Cross-referenced against Microsoft's own current x64 exception-handling documentation: when
Windows' SEH unwind dispatch walks a stack frame with no `RUNTIME_FUNCTION` entry, its own
documented fallback treats whatever is at `[RSP]` as a return address and keeps walking -- this
is intentional "leaf function" behavior, not a bug in the unwinder. `switch_to_guest_sysret`
(litebox's own guest-entry trampoline, a naked `jmp` into guest code with no real Windows call
frame) means that once guest execution is running on the guest's own stack, there is no real
Windows call chain to unwind at all. If some LATER, unrelated fault triggers SEH dispatch while
`RIP` happens to be deep inside guest execution, and the unwind walk reaches this frame-less
boundary, the "leaf function" fallback dereferences ordinary guest data (in the observed case, a
small integer, `0x42a`) as if it were a code pointer, landing on unallocated memory and faulting.
This is confirmed to be a genuinely different mechanism from the already-retracted theory (that
one was about one specific function's own missing metadata; this one is structural -- no
`RUNTIME_FUNCTION` entry could ever describe "the guest's own stack contents at an arbitrary,
unpredictable depth").

**Correction (2026-09-05, cross-session review found this by re-reading the code's own comments,
not by new investigation)**: the SEH-unwind theory above has the SAME SHAPE as three already-
retracted theories (`0x4e12c0`, `0xfefefefefefefeff`, "-libcalls") -- it describes where the
repeated symptom is observed, not the true first fault. `litebox_platform_windows_userland/src/
lib.rs:869`'s own comment records that live captures identified the real causative first fault as
`is_in_guest=true, addr=usize::MAX`, and that the reason it kept getting misattributed is that
`eprintln!` inside the trace block re-faults on an already-corrupted thread before the print
completes -- so a LATER fault in the resulting cascade gets logged as "the" crash instead. `addr=
usize::MAX` (`0xFFFF_FFFF_FFFF_FFFF`) is not a stack-unwind artifact (an unwinder dereferencing
guest data would fault on an arbitrary small integer, e.g. the `0x42a` observed elsewhere) --
it's the sentinel Windows uses when `ExceptionInformation[1]` (faulting address) is genuinely
unavailable, and `lib.rs:1014` already special-cases exactly that value. Also: `is_in_guest` is a
`Cell<bool>` field on `TlsState`, not a function -- "the existing `is_in_guest` guard" in earlier
revisions of this section was never a real function name; fixed here to avoid sending a future
pass looking for one.

**Second correction (2026-09-05, live evidence this time, not code-reading)**: the `lib.rs:869`
comment cited just above -- asserting the causative first fault is `is_in_guest=true, addr=
usize::MAX` -- is now itself suspect. A live repro (`mate-session --version` x3 against
`webtop_seatd.tar`, `LITEBOX_DIAG_FAULT_VQ=1` widened to fire on any guest-mode access violation
regardless of `rip == cr2`, no `LITEBOX_VEH_TRACE`) captured 154 real access-violation events
across two runs. EVERY one showed `is_in_guest=false, addr=0x2` -- the opposite of what that
comment claims, and none in `HOST_ALLOCATOR_REGION_MIN`'s range (ruling out a separate,
now-refuted "corrupt guest FS-base reads host heap" hypothesis this same investigation raised and
tested). `is_in_guest=false` means the fault is in HOST-side code, not guest code -- consistent
with the SEH-unwind-cascade theory (a small-integer deref like `0x2` fits an unwinder walking
through a garbage pointer), but flatly inconsistent with `lib.rs:869`'s own claimed evidence.
Either that comment describes a genuinely different fault than the one that reproduces via this
repro, or it's stale/wrong. Treat `lib.rs:869`'s specific claim as unverified until someone
re-examines it directly against fresh evidence -- do not build further theory on top of it without
first resolving this contradiction.

**Concrete next step, not yet attempted, and cheaper than either of the two below**: capture
depth-0 (`is_in_guest=true`) via the already-existing allocation-free `RECENT_FAULTS` ring
(`lib.rs:587`) or `diag_raw_regdump`, against the exact `mate-session --version` x3 repro --
**do NOT run this under `LITEBOX_VEH_TRACE=1`**, the archive already records ~12 consecutive
traced runs where the crash never reproduced, meaning tracing's own overhead dodges the race this
bug depends on. Also worth a one-line experiment first: `AddVectoredExceptionHandler(0, ...)` at
`lib.rs:2418` registers LAST in the process's VEH chain (`0` means last, `1` means first) --
confirm whether that's deliberate; if another VEH in the process (CRT, a loaded DLL, a nested
litebox fork child) registers with `1`, it sees the exception first and can
`EXCEPTION_CONTINUE_EXECUTION` out from under this trace, silently hiding exactly the fault being
hunted.

**Two older next-step options, lower priority now that the above is known**: (a) generic tracing
of "the true original fault" via new diagnostics (largely superseded by the depth-0-capture step
above, which needs no new code); (b) integrate `minidump-writer` (Mozilla's crash-reporting crate,
confirmed capable of genuine in-process x86_64 Windows minidump capture with no live-debugger
attach needed -- see `docs/premade-library-research.md`) for real symbol resolution and full
call-stack reconstruction. Do not guess at a fix (e.g. blindly registering `RtlAddFunctionTable`
entries) without first getting real evidence from the depth-0 capture -- this bug has already
produced multiple retracted theories from acting on incomplete understanding, and the user's own
explicit standard for this bug is a genuinely root-caused fix, not one that merely stops the
observed symptom.

## Windows CoW-mmap performance (investigated thoroughly, not worth pursuing further)

`try_allocate_cow_pages` (avoiding a page-by-page `sys_read`+memcpy of the whole binary on every
guest exec, ~27ms/exec on busybox) is now implemented for Windows, live-VMA-safety-checked
(structurally cannot repeat an earlier untracked-host-memory regression by construction), and
fully tested -- but has **zero practical effect** on real tar-packed execs. Root cause,
conclusively established: Windows' `MapViewOfFile3` requires 64KiB file-offset alignment; real
ELF `PT_LOAD` segment file-offsets are only page-aligned (linker-controlled, not fixable by
repacking); and because real ELF binaries pack their segments back-to-back with zero gap, only
the FIRST segment (by `p_vaddr`) could ever benefit from any padding/realignment trick -- but the
first segment always starts at file offset 0, which is already aligned and never needed help in
the first place. The segments that actually need padding structurally have no free space before
them (confirmed via `readelf` on real binaries: the observed gaps are 400-3356 bytes against a
need of ~28-57KB, short by one to two orders of magnitude). Tar-file-start alignment (a separate,
also-investigated angle) has zero effect on this either -- confirmed via three real layers at
1%/90%/100% file-start alignment, byte-identical CoW-attempt/success counts on all three. **This
is a closed, well-evidenced negative result -- do not re-attempt without a genuinely new
approach** (e.g. per-segment ELF relayout, which was considered and rejected as too risky for
`ET_EXEC` binaries with linker-fixed addresses).

## Input latency (fixed, shipped)

Three real bugs found and fixed in the Windows presentation/input path, all verified live:
1. Slow mouse movement was silently dropped entirely (a float-to-i32 truncation discarded
   sub-pixel remainders instead of accumulating them) -- fixed, motion is now lossless.
2. Every physical mouse movement was delivered to the guest as TWO separate evdev reports
   (`REL_X`+`SYN_REPORT`, then `REL_Y`+`SYN_REPORT`) instead of one grouped report, causing
   double pointer-motion processing -- fixed, one `SYN_REPORT` per physical movement now.
3. The window was originally locked to the guest's fixed virtual resolution to keep 1:1 pixel
   mapping, but that didn't fit smaller host screens; the window is now resizable and a
   `Resized` handler keeps the surface configuration truthful, with mouse deltas scaled by the
   ratio of visible-guest-pixels-to-window-pixels so cursor tracking stays correct at any window
   size without overshooting into the invisible guest region.

Not yet done: no framerate baseline exists (an idle compositor with no client legitimately
produces zero page flips, so there's nothing to measure against yet) -- needs a real, moving,
on-screen client first.

## Repo/tooling notes

- `docs/premade-library-research.md` -- the ongoing library-vs-hand-rolled-code audit (see the
  standing lesson above). Check it before writing new infrastructure code in any of the areas it
  covers.
- `advisor/probes/` holds various diagnostic scripts and probes accumulated across this
  investigation (`decode_frame.py`, `run_xfce_xwm.sh` and variants, `drm_flip_probe.c`, etc.) --
  useful, keep using them, but don't assume every script there is still the current recommended
  path (e.g. the OCI-pull Python scripts are retired, see "Container images" above).
- Pre-existing, unrelated test-suite gaps as of this writing (not blocking, not this project's
  fault to fix unless picked up deliberately): `cargo test -p litebox --lib` has 26 failing tests
  out of 150 (missing `diod` binary for 9P tests, plus two separate pre-existing logic bugs
  unrelated to anything in this file). `cargo test -p litebox_shim_linux --lib` is clean, 181/181.
