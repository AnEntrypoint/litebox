# Webtop guest rebuilt from host packages; rungs 1-4 status (2026-10-12)

The container was recreated on 2026-10-11 and `/tmp/lbx` (rootfs tarballs, harness, runner copy) was
lost. The old `rootfs-x11.tar` seed does not survive. This is the rebuild that replaced it.

## Rebuild recipe (no network, no debootstrap)

The host already has the webtop packages installed, so the guest is assembled from them:

1. Binaries + library closure: `Xvfb xfwm4 xfce4-session xfce4-panel xfdesktop xfsettingsd xclock xwd
   xdpyinfo xwininfo dbus-daemon dbus-launch glycin-thumbnailer` + chromium + dash/base64/coreutils,
   and `ldd` of all of them (plus `xfce4/`, `gdk-pixbuf-2.0/`, `gio/modules/` ELF files).
2. Map files to packages with `dpkg -S`, then take `dpkg-query -L` for those packages. Drop
   `/usr/share/{doc,man,lintian,info}`, `/usr/share/locale` except en, `gcc`, `cpp`, `include`.
   Result: ~2.2 GB. Copy with `cp -a --parents` (keeps symlinks), then resolve dangling links from the host.
3. Add explicitly: `/usr/share/{fonts,X11,icons,themes,glib-2.0,xfce4,dbus-1,mime,applications,
   glycin-loaders,gtk-3.0,gdk-pixbuf-2.0}`, `/etc/{fonts,xdg}`, `/usr/libexec/glycin-loaders`,
   `/usr/lib/chromium` (minus `extensions/`), `/usr/lib/xorg/modules` (from `xserver-xorg-core`),
   `xkbcomp` (`x11-xkb-utils`), `xkb-data`, the Mesa DRI dir `/usr/lib/x86_64-linux-gnu/dri` (48 MB,
   from `libgl1-mesa-dri`) with its ldd closure, and `/usr/share/drirc.d`.
4. Guest basics: `/etc/{passwd,group,hosts,nsswitch.conf,resolv.conf,localtime,machine-id}`,
   `/opt/lib` (the host `libudev` is a FAKE stub: ship `libudev.so.1` -> `libudev.so.1.0.0-fake`),
   `/tmp`, `/tmp/home`, `/tmp/xdg`, `/tmp/.X11-unix` all 1777, `/bin/sh -> /usr/bin/dash`.
5. Coreutils the boot script needs: `cat ls rm head tr wc mkdir sleep base64 chmod grep awk`
   (+ closures). `awk` is `/etc/alternatives/awk` -> mawk: copy the resolved file.
   **`timeout` and `cut` are NOT safe to rely on**: `timeout` returned 0-byte captures; `cut` was missing.
6. `chmod -R go+rX` drops the sticky/world-write bits on `/tmp*` — re-apply `chmod 1777` last.
7. Tar with `--owner=0 --group=0 --numeric-owner`. Check for dangling links (0 expected).

Script: `docs/tools/webtop-wt.sh` (rung 1-4, `<rung> <hold> [chromium flags]`, watchdogged X clients).
Decode: `docs/tools/xwdhist.py <out-file>` (handles 24/32 bpp, `XWD`/`CXWD`/`PXWD` blocks).
Run it with the gated wrapper: `PROG="/bin/sh /wt.sh" TAR=<wt.tar> RUNNER=<lbxrun> run-wt.sh <tag> <secs> <rung> <hold> [flags]`
(one runner at a time; sweep orphans by argv — `timeout -s KILL` leaves fork children alive).

## Results

| rung | result |
|---|---|
| 1 Xvfb | PASS. root captures 3,148,907 B; decoded 2 colours (589,824 black + 196,608 white) = root weave |
| 2 +xfwm4 | root unchanged (no clients), xfwm4 window present |
| 3 +xfce4-session | PASS for the desktop: root 201 colours incl. the panel bar and desktop grey `cececeff` |
| 4 +chromium | chromium window `litebox chromium - Chromium` mapped 1024x744+0+24 in the xfwm4 frame; grabs are uniform `cececeff` = desktop grey showing through: **MAPPED, NEVER DRAWN** |

Variants at rung 4 (all: window mapped, never drawn):
- default flags: GPU process exits: `ANGLE ... GLX is not present` (Xvfb without GLX).
- `+extension GLX` (Xvfb): `glXQueryExtensionsString returned NULL`; then the X server stops answering `xwd`.
- `--use-angle=swiftshader --enable-unsafe-swiftshader`: stalled before the first capture (not diagnosed).
- `--disable-gpu`: no GPU-process errors, no X11/compositor error in chromium stderr; still never drawn.
- `--disable-gpu` + `-extension MIT-SHM` on Xvfb (XPutImage only): still never drawn. **MIT-SHM is not the cause.**

## litebox defects found and fixed this session

- `5d765ec7` a refused guest `mremap` aborted the host process (`remap_pages` `expect`). Now an errno.
- `bef9d55a` fresh memfd objects take the pool token, not a process-local counter name (silent alias).
  Switch `LITEBOX_MEMFD_TOKEN_OFF`. One ON probe aborted with `shared heap lock stuck`; not reproduced on repeat.

## Open

- `mmap`'s `_ => panic!("unhandled mmap error")` (`litebox_platform_linux_userland/src/lib.rs` ~2908) is the
  same guest-reachable class as the mremap panic. Not changed.
- `pool_table` = 128 slots, never freed (`shared_heap.rs:47`): `shmat` ENOMEM after 128 attached segments.
- Headless `--headless=new` in the minimal (non-webtop) rootfs: no renderer starts, no DOM in 240 s.
- The paint question is now a renderer-to-viz frame problem: the window is mapped, the compositor has
  nothing to show. Next: whether the renderer commits frames at all (Mojo/BeginFrame, timerfd firing in
  the renderer and GPU-less viz), not the X server.

## Paint discriminators (2026-10-12, later runs)

Single-process `--single-process --disable-gpu` in this webtop **PAINTS**: chromium window 910-1113 colours,
BGRA `00d4ff` = RGB `ffd400` = the page's yellow. Multi-process with the same flags does not.
So the blocker is the cross-process path (renderer -> viz), not the X11 presentation.

A/B of the memfd switch (`LITEBOX_MEMFD_TOKEN_OFF`), multi-process, `--disable-gpu`, compositor vmodules:
- token path ON (default, `bef9d55a`): chromium window shows a **striped blue band** (vertical stripes,
  two colours, rows ~216 of 744), not the page. Rendered to PNG and inspected: real pixels reach the
  compositor, but the content is wrong. Not blank.
- token path OFF (`LITEBOX_MEMFD_TOKEN_OFF=1`): chromium window **blank grey** (`cececeff`, no stripes).

Reading: the token path makes frames cross processes (something is painted), but the bytes the viz
side reads are not the bytes the renderer wrote. Candidate: pool-offset mapping or stride/format in the
shared bitmap, under `map_shared_memory` (`litebox_platform_linux_userland/src/lib.rs` ~3125-3145) and
`try_memfd_mmap` (`litebox_shim_linux/src/syscalls/mm.rs:1260`). Not yet diagnosed.

Verbose chromium logging (`--v=1`) makes the guest too slow to finish: do not use it for runs that must
complete; use `--vmodule` only.

Band structure (token path ON, mid-band row y=559 of the 1024x744 chromium window): colour changes EVERY
pixel, repeating every 4 px with a 2:1:1 mix (512 px `ff7f0000`, 256 px `30403fff`, 256 px `b04c3fff`).
That is a stride or pixel-format mismatch between the producer and the reader of the same bitmap, not
random aliasing. Next: find where the viz-side reader takes its row stride/format for a shared software
bitmap, and check it against the producer's size under the pool-token mapping.

Big-message check (`LITEBOX_DIAG_BIGMSG=1`, diagnostic in `unix.rs` write path): with plain
`--disable-gpu` the multi-process run sent **zero** cross-process messages over the 2048-byte ring
(no `EMSGSIZE` refusals, no partial writes). So the ring is not in the paint path. Plain-flag multi-process:
desktop paints (201 colours), chromium window stays uniform grey. The striped band appeared only with
`--vmodule` logging, so it is timing-dependent, not a deterministic stride bug.

Syscall summary (`LITEBOX_STRACE_SUMMARY=1`, plain multi-process): the table reports only the guest
shell (`comm=sh`), not chromium's processes, so it cannot show the browser main-thread poll loop. Per-process
summaries are needed before the busy-loop can be attributed. No `epoll_wait`/`ppoll` appears in this table.

ppoll spin (`LITEBOX_SPIN_STACK=271`, every 8192nd ppoll): multi-process 13 entries, single-process
(which paints) 12 entries. The hot ppoll loop is present in both, so it is NOT the paint blocker. It is a
separate defect; the multi-process renderer's missing frames are a different problem.

IO-thread timeline (`LITEBOX_DIAG_SYSCALL_TIMELINE=Chrome_ChildIOT`, plain multi-process, 170 s):
four processes' IO threads (pids 1443, 1825, 1850, 1867). Of 4,170 `epoll_pwait` exits, 2,930 returned
0 events; timeouts are either 0 (bursts) or about 170 s (idle). `sendto` returned EAGAIN 118 times of 2,146
(not a send-retry spin). The zero-timeout bursts show work pending without a ready fd, so the spin is
a task-reposting loop. The attribution to a task source needs chromium's own task traces, not yet done.

Attribution blockers (2026-10-12): `/usr/lib/chromium/chromium` has NO `.symtab` and no debug info
(only 1878 dynamic symbols), so the spin's rip values cannot be mapped to chromium functions. No
chromium debug package is in the local apt cache (xtradeb PPA build 154.0.8037.92-1xtradeb1). The
`--vmodule` runs for message_pump and sequence_manager produced no task-level log lines in this build.
Remaining routes: a symbolized build (needs apt index update, root, network), or litebox-side per-task
instrumentation.

Candidate refuted (2026-10-12): background subsystems off (`--disable-background-networking
--disable-component-update --disable-sync --disable-extensions --disable-default-apps` plus
translate/optimization-hints/media-router features). IO zero-event polls 2806 of 3956 (71%), baseline 70%;
chromium window still uniform grey. Background work is not the spin or the paint blocker.

Candidate refuted (2026-10-12): `--disable-features=VizDisplayCompositor`, plain multi-process, 150 s hold.
The desktop paints (203 colours) and chromium's window stays uniform grey (`cececeff`, 9 captures). The
compositor-selection flag does not change the paint result. (A first run at a 60 s hold showed no desktop
yet, so only the 150 s run is the valid comparison.)

## PAINT ACHIEVED in the webtop, multi-process, no --no-sandbox (2026-10-12)

Flags: `--disable-gpu --in-process-gpu` (no `--single-process`, no `--no-sandbox`). The renderer is a
separate chromium process; the GPU process is folded into the browser.
- Run `ingpu` (10 captures): chromium window painted, page yellow `ffd400` (BGRA `00d4ff`), 633-884 colours.
- Run `ig3` (9 captures): same, 632-773 colours, stable. Evidence: `docs/evidence/webtop-chromium-paint-in-process-gpu.png`.
- Run `ingpu2`: stalled after one capture in the guest (a reliability problem, not a paint failure).
- Process count is NOT a discriminator: the single-process run also reached 27-33 live runner processes.

Repeat tally for `--disable-gpu --in-process-gpu` (full 150 s hold): painted in `ingpu`, `ig3`, `ig5`;
stalled in the guest before any capture in `ingpu2`, `ig4`. The paint result is consistent when it completes.

Stall tally (`--disable-gpu --in-process-gpu`, full 150 s hold): 7 of 9 full runs painted and completed
(`ingpu`, `ig3`, `ig5`, `st1`-`st4`). The 2 stalls (`ingpu2`, `ig4`) stopped in the guest before any capture.
The four `st*` runs had `LITEBOX_DIAG_STUCK_SECS=45` and produced no stuck-thread records, so the stall
trigger is still unidentified. Reliability is 7/9, not yet "perfect".

Reliability tally, `--disable-gpu --in-process-gpu`, full runs (2026-10-12): 13 painted and completed of 19.
Failures (6): 4 guest stalls before the first capture (`ingpu2`, `ig4`, `sa3`, `sa4`; no stuck-thread record
with `LITEBOX_DIAG_STUCK_SECS=45`) and 2 completed runs where the XFCE desktop never came up (`sa2`, `sa5`:
root stays the bare weave, chromium window 0 bytes). The session-log dump was added to `wt.sh`; the four
`lg*` runs all painted, so no failure was captured with its logs yet.
