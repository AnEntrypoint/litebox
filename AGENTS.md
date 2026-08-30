# AGENTS.md — handoff note (2026-08-30, sub-session 4)

## Session update (2026-08-30, sub-session 4): labwc DRM backend now creates successfully; new blocker in wlroots keymap shm allocation

Picked up sub-session 3's exact next step (extend `SysClassDrm` to serve a synthetic
`device/drm` subtree). Found the fix was already partially drafted uncommitted in the working
tree at session start (`litebox/src/fs/devices.rs`'s `SysDrmDirHandle::DeviceOf`/`DeviceDrmOf`
variants, `litebox_common_linux`'s `DRM_CAP_PRIME` constants) -- completed and extended it, then
iterated live through FIVE further real, previously-unreached bugs one at a time (same
read-log-find-next-failure discipline as prior sessions), each confirmed via a fresh
`LITEBOX_LOG=debug` repro:

1. **`device/drm/{card0,renderD128}` self-referencing synthetic subtree** -- completed the
   pre-existing draft (`SysClassDrm`'s `DeviceOf`/`DeviceDrmOf` handle variants, proper
   `NodeInfo` entries, `walk_directories`/`list_dir_at`/`dir_status` wiring). Fixed
   `/sys/dev/char/226:0/device/drm` resolving.
2. **`DRM_CAP_CRTC_IN_VBLANK_EVENT` wrong value** -- the uncommitted draft's `DRM_CAP_PRIME`
   support was correct, but a fresh capability wlroots also queries
   (`backend/drm/drm.c`'s `check_drm_features()`) needed adding. Initially guessed the real
   kernel value as `0x9`; live capability-value tracing (added temporary debug logging,
   removed after) showed the actual queried value was `18` (`0x12`), not `9` (`0x9` is
   `DRM_CAP_CURSOR_HEIGHT`) -- corrected. This device's page-flip completion event already
   always stamps `crtc_id`, so reporting `1` is simply true, not fabricated.
3. **`DRM_CAP_PRIME` get_cap wiring incomplete** -- the constants existed uncommitted but
   `drm.rs`'s `get_cap()` never actually checked for `DRM_CAP_PRIME` and returned the
   import+export bitmask. Wired it.
4. **`226:128` (render node) `/sys/dev/char` reverse lookup missing** -- `SysDevCharEntry`
   only had `226:0`; wlroots' `types/wlr_drm.c`'s `drmGetDevice2()` opens the render node too
   and needs its own reverse lookup for the identical reason `226:0`'s was needed. Added
   `SysDevCharEntry::DrmRender` (`226:128 -> ../../class/drm/renderD128`).
5. **`<card>/device/subsystem` and `<card>/device/uevent` missing at the synthetic `device`
   level** (distinct from the already-existing `<card>/subsystem`/`<card>/uevent` one level
   up) -- `drmGetDevice2()`'s bus-type classification reads `<device>/subsystem` (added as a
   symlink to `../../../bus/platform`, since litebox's virtual DRM device has no real PCI/USB
   parent -- `platform` is the real kernel's own choice for a DRM device with no discrete bus,
   e.g. `simpledrm`/`vkms`) and `<device>/uevent` (added as a regular file, `DRIVER=litebox`).
6. **`DRM_IOCTL_GET_MAGIC`/`DRM_IOCTL_AUTH_MAGIC` completely unimplemental** -- wlroots' render
   allocator (`render/allocator/allocator.c`) does the legacy DRI `drmGetMagic()`/
   `drmAuthMagic()` authentication handshake before allocating; unimplemented ioctls fell
   through to `ENOTTY`->`EINVAL`, surfaced as `"drmGetMagic failed: Invalid argument"` ->
   `"unable to create allocator"`. Implemented both (`struct drm_auth`, one `__u32 magic`
   field) with a single fixed magic value -- this device has exactly one possible client, so
   there is no real per-client auth state to track.

**Verified live, cumulatively**: with all six fixes plus `WLR_RENDERER=pixman` (or
`WLR_RENDERER_ALLOW_SOFTWARE=1`; litebox's GPU is virtual/software, and wlroots' own real gate
refuses software rendering by default -- NOT a litebox bug, same category as the alread-tried
`WLR_RENDERER=pixman` sub-session-3 attempt, except this time tried AFTER the DRM backend could
actually open rather than before), labwc's DRM backend creation now fully succeeds:
`DrmModeGetResources`/`GetCrtc`/`GetPlaneResources`/`GetPlane`/`ObjGetProperties`/`GetProperty`
all resolve correctly, no more `"Failed to create DRM backend"`/`"Could not successfully create
backend on any GPU"`/`"Failed to open any DRM device"`/`"unable to create backend"` -- this was
the session's original stated blocker and it is now conclusively fixed.

**New blocker found, NOT yet fixed**: past DRM backend creation, wlroots crashes with a real
`SIGSEGV` (`signal=Signal(11)`) inside its own keymap-shm-allocation code path
(`types/wlr_keyboard.c:222`, `"Failed to allocate shm file for keymap"`) shortly after. Also
discovered along the way: `/dev/shm` does not exist by default in this rootfs and litebox has no
synthetic tmpfs mount there -- `mkdir -p /dev/shm && chmod 1777 /dev/shm` in the launch shell
command (added to the documented repro below) fixes that specific sub-gap for free (it is backed
by the ordinary writable rootfs layer, not `devices.rs`, so a plain `mkdir` just works, no code
change needed) and pushes the crash further (past an earlier, now-fixed `/dev/shm/wlroots-XXXXXX`
open failure for the DRM format table). With `/dev/shm` pre-created, the SAME `openat` path
(`/dev/shm/wlroots-<random6>`) is called TWICE for the keymap allocation and **both calls
succeed** (`fd=Some(18)` then `fd=Some(19)`, identical filename, zero intervening
`fcntl`/`ftruncate`/`mmap`/`unlink` syscalls logged) immediately before the error and crash --
this is suspicious: real `os_create_anonymous_file()`'s `O_CREAT|O_EXCL` retry-loop shape expects
a SECOND open of the SAME just-created name to fail `EEXIST` (prompting a new random suffix), not
succeed silently. `sys_ftruncate` is fully implemented in litebox but is never actually invoked
anywhere in this whole repro run (confirmed via full-log grep) -- meaning wlroots itself never
gets far enough to call it, i.e. the crash happens between the second successful `open()` and
whatever wlroots does next, entirely in guest userspace with no syscall trace to follow further
without either real wlroots/musl `os_create_anonymous_file`/`allocate_shm_file` source to
cross-reference the exact expected call sequence, or finer instrumentation than what
`LITEBOX_LOG=debug` currently emits (e.g. a temporary trace on `sys_close`/`sys_write` too, or on
litebox's own `O_EXCL` existence-check codepath in `do_open`/the core `fs` layer, to confirm
whether the second `open()` really should have raced/collided and didn't). Registered as gm
mutable `labwc-wlroots-shm-keymap-allocation-still-fails-after-dev-shm-fix` with full detail (see
`.gm/mutables.yml` / gm's own recall) rather than abandoned silently.

Repro command (extends sub-session 3's shape with `mkdir -p /dev/shm`/`chmod 1777` and
`WLR_RENDERER=pixman`):
```
target/release/litebox_runner_linux_on_windows_userland.exe --initial-files .wfgy/xfce-build/alpine-pinned2.tar --resume-from .wfgy/xfce-build/xfce-layer16.tar -- /bin/sh -c "mkdir -p /run/user/1000 /dev/shm; chmod 700 /run/user/1000; chmod 1777 /dev/shm; export XDG_RUNTIME_DIR=/run/user/1000; export XKB_CONFIG_ROOT=/usr/share/X11/xkb; export WLR_RENDERER=pixman; seatd -l debug & for i in 1 2 3 4 5 6 7 8 9 10; do [ -S /run/seatd.sock ] && break; sleep 1; done; labwc -s \"xfsettingsd & xfce4-panel & xfdesktop &\""
```
with `LITEBOX_LOG=debug`, `MSYS_NO_PATHCONV=1` in Git Bash. Rebuild
`cargo build --locked --release -p litebox_runner_linux_on_windows_userland` first.

Regression-tested: `cargo test -p litebox_shim_linux --lib -- --skip test_mremap` 177/177 pass;
`cargo test -p litebox_platform_windows_userland` all pass -- both clean after all six fixes
above.

**Next step for whoever picks this up**: isolate the exact guest-userspace failure between the
second `open("/dev/shm/wlroots-XXXXXX")` success and the `"Failed to allocate shm file for
keymap"` log line. Prime suspects in order of likelihood: (1) litebox's `O_CREAT|O_EXCL` open
path in the core `fs` layer (not the shim) not actually rejecting a second open of a name that
should already exist -- check `litebox/src/fs`'s `do_open`/equivalent for how `OFlags::EXCL` is
enforced against an already-open (not yet closed/unlinked) name; (2) a `write()`/`pwrite64()` on
the fd (Wayland keymap content is written before mmap in some wlroots versions) not being logged
because `sys_write`'s own debug-log call is missing or gated differently than `sys_openat`'s; (3)
an `mmap()` call on the shm fd silently failing (its own debug line might be filtered by a
narrower grep than used this session -- re-check with an unfiltered full-file read around the
exact line range, not a `grep` for `sys_mmap` which was checked and came up empty, but a raw
`sed`/`Read` of a wider window is more reliable). Once fixed, verify per this session's own
already-proven-working repro command above; then check for xfsettingsd/xfce4-panel/xfdesktop
process launches (`sys_execve` log lines) and hold the window 90-150s+ per the standing
completion criterion.

# AGENTS.md — handoff note (2026-08-30, sub-session 3)

## Session update (2026-08-30, sub-session 3): labwc DRM backend blocker found, partial fix

Picked up sub-session 2's remaining gap directly: found `xfce-layer16.tar` (via `tar -tf`) is
the layer tar that actually ships a real `usr/bin/labwc` binary (`xfce-layer-FINAL.tar`, used
by prior sessions, does not -- only labwc config files). Ran the full documented repro
(`alpine-pinned2.tar` + `--resume-from xfce-layer16.tar`, `seatd -l debug &` then
`labwc -s "xfsettingsd & xfce4-panel & xfdesktop &"`, `XKB_CONFIG_ROOT=/usr/share/X11/xkb` set)
against the now-fixed libinput/fallocate/migrate_file_up path from sub-session 2.

**New blocker found**: labwc uses `wlroots`' DRM backend (distinct from weston's own DRM
backend code, which was already proven stable in sub-session 2) -- wlroots calls
`drmGetDeviceNameFromFd2()` which litebox failed with `No such file or directory`, aborting
backend creation before any DRM ioctl. Root cause: litebox's `/sys/dev/char/<major>:<minor>`
reverse-lookup backend (`SysDevChar` in `litebox/src/fs/devices.rs`) only had an entry for the
virtual input device (`13:64`), not the DRM device (`226:0`) -- its own doc comment explicitly
(and, it turns out, wrongly) scoped DRM as unnecessary, reasoning weston's DRM backend doesn't
need this reverse lookup. wlroots does.

**Partial fix applied and committed** (this session): added a `226:0 -> ../../class/drm/card0`
entry to `SysDevCharEntry`/`SysDevChar` (same pattern as the existing `13:64` entry). Verified
live this DOES fix the shallow lookup -- `sys_readlinkat`/`sys_stat` on
`/sys/dev/char/226:0` itself now succeed and correctly resolve into `/sys/class/drm/card0`
(confirmed via fresh `LITEBOX_LOG=debug` capture).

**Still blocking, NOT yet fixed**: immediately after that shallow resolution succeeds,
wlroots' `drmGetDeviceNameFromFd2()` (or a related libdrm call inside it -- exact function not
isolated, no local libdrm/wlroots source was available this session to cross-reference like
sub-session 2 had for libinput) does `sys_stat` on the DEEPER path
`/sys/dev/char/226:0/device/drm` and gets `ENOENT`, which is immediately followed by the
`drmGetDeviceNameFromFd2() failed` error and backend abort. `SysDevChar`'s backend design is a
flat namespace (`walk_directories` stops at any single-component match,
`WalkStopReason::StoppedAtNonDirectory`, confirmed in its own source) -- it has no support for
resolving a further path component past the symlink target, so this deeper `device/drm`
sub-path can never resolve today regardless of what `226:0` points to.

Real kernel sysfs shape being emulated: `/sys/class/drm/card0/device` is normally a symlink to
the card's parent PCI device directory, which itself contains a `drm/` subdirectory listing
sibling DRM nodes (`card0`, `renderD128`, etc) -- i.e. `/sys/dev/char/226:0/device/drm/card0`
resolves back to the same `card0` directory via a real device's actual PCI topology. litebox's
virtual DRM device has no real PCI parent to model, so this needs a synthetic self-referencing
structure: `SysClassDrm`'s `card0` entry needs a `device` sub-entry (symlink to a synthetic
device directory) which itself needs a `drm` sub-entry (directory containing `card0`, symlinked
or directory-listed back to the real `/sys/class/drm/card0` this whole tree originates from).

**Concrete next step**: extend either `SysDevChar` to support nested walks past its symlink
targets (bigger, more general fix), or more narrowly, extend `SysClassDrm`'s existing
`card0`/`renderD128` subtree to serve a `device/drm/card0` (and `device/drm/renderD128`)
sub-path that resolves back to itself, then re-test whether `sys_stat` on
`/sys/dev/char/226:0/device/drm` succeeding is sufficient to unblock
`drmGetDeviceNameFromFd2()`, or whether a further sub-path is needed after that (iterate: fix
one level, re-run the exact repro below, read the next `sys_stat`/`sys_openat`/`sys_readlinkat`
call immediately preceding the next error, if any). `WLR_RENDERER=pixman labwc` (labwc's own
suggested software-rendering fallback, printed in its own error output) was TRIED and TESTED
LIVE this session -- does NOT help, identical `drmGetDeviceNameFromFd2()` failure, since that
call happens during DRM device OPENING, before renderer selection is ever reached. Do not
re-try this as a shortcut; the sysfs `device/drm` sub-path fix is the only real path forward.

Repro command (unchanged shape from sub-session 2, just swap the layer tar and drop
`udevd --daemon` which sub-session 2's own AGENTS.md section below already confirmed makes zero
difference):
```
target/release/litebox_runner_linux_on_windows_userland.exe --initial-files .wfgy/xfce-build/alpine-pinned2.tar --resume-from .wfgy/xfce-build/xfce-layer16.tar -- /bin/sh -c "mkdir -p /run/user/1000; chmod 700 /run/user/1000; export XDG_RUNTIME_DIR=/run/user/1000; export XKB_CONFIG_ROOT=/usr/share/X11/xkb; seatd -l debug & for i in 1 2 3 4 5 6 7 8 9 10; do [ -S /run/seatd.sock ] && break; sleep 1; done; labwc -s \"xfsettingsd & xfce4-panel & xfdesktop &\""
```
with `LITEBOX_LOG=debug`, `MSYS_NO_PATHCONV=1` in Git Bash. Rebuild
`cargo build --locked --release -p litebox_runner_linux_on_windows_userland` first.

Regression-tested: `cargo test -p litebox_shim_linux --lib -- --skip test_mremap` 177/177 pass
after the `226:0` addition.

# AGENTS.md — handoff note (2026-08-30, sub-session 2)

## Session update (2026-08-30, sub-session 2): libinput EVDEV_UNHANDLED_DEVICE blocker CONFIRMED FIXED

The libinput/evdev blocker documented in the section immediately below (mallocng-fix session)
is now root-caused and fixed, commit `5458d74c` (pushed to `main`). Real cause: `libevdev_new_from_fd()`
itself was failing during `evdev_device_create()` (`evdev.c:2314-2316`), BEFORE the udev-tag
classification code the prior session was staring at ever ran -- which is exactly why neither
candidate diagnostic log line ("not tagged as supported input device" / "is tagged by udev as")
ever appeared: both are downstream of a call that never happened. `libevdev`'s internal
`sync_key_state()`/`sync_led_state()`/`sync_switch_state()` issue `EVIOCGKEY`/`EVIOCGLED`/`EVIOCGSW`
during setup; litebox implemented none of the three, so they fell through to the `Raw` ioctl
catch-all's `EINVAL`. That catch-all's own warning (`log_unsupported_fmt` in
`litebox_shim_linux/src/lib.rs`) is gated by `cfg!(debug_assertions)` and is silently swallowed in
every `--release` build -- which is the real reason this failure produced zero diagnostic output
across many prior sessions' repro runs, independent of which of the two evdev.c branches anyone
was trying to disambiguate. Fixed by implementing all three ioctls as real all-zero-bitmap
responses (`litebox_common_linux/src/lib.rs`, `litebox_shim_linux/src/syscalls/file.rs`).

That unblocked device creation but surfaced weston's next real, previously-unreached dependency:
`fallocate(2)` was completely unimplemented (same silent-`ENOSYS`-in-release gap), breaking
`os_create_anonymous_file()`'s `posix_fallocate()` call for the shared Wayland keymap memfd.
Implemented `fallocate` (`mode=0` grow-only, the only mode litebox needs to support) by reusing
the existing truncate/memfd-shared-backing-resize machinery.

`fallocate`'s truncate call then exposed a genuine, unrelated, previously-unreached panic in
litebox core: `litebox/src/fs/layered.rs`'s `write()`/`truncate()` migration path
(`migrate_file_up`) treated `ReadError::NotForReading` on a Lower-layer fd as `unreachable!()`,
when it's a real condition -- reachable whenever a path classified `Lower` for migration turns out
to name a directory rather than a regular file. Folded it into the existing `NotAFile` handling,
which every caller already handles as a real, non-panicking error, instead of adding a new panic
class.

**Verified live**: with all three fixes, `weston --backend=drm-backend.so --use-pixman
--shell=kiosk-shell.so` (this rootfs ships `kiosk-shell.so`, not `desktop-shell.so` -- the bare
`weston` invocation with no `--shell` flag fails to load the default shell module and exits, a
separate, expected, non-litebox packaging gap, not a bug) reaches stable DRM page-flip rendering
(`DrmModeSetCrtc`/`DrmModePageFlip` succeed) and holds with **no fatal/panic through 150+ seconds**
of a real repro run, confirmed twice. Also needed: `XKB_CONFIG_ROOT=/usr/share/X11/xkb` in the
launch env -- this rootfs ships `xkeyboard-config` data at the legacy X11 path, not
`/usr/share/xkeyboard-config-2` where xkbcommon looks by default; without it weston fails XKB
keymap compilation before ever reaching the memfd/fallocate code path.

Regression-tested: `cargo test -p litebox_shim_linux --lib -- --skip test_mremap` 177/177 pass;
`cargo test -p litebox_platform_windows_userland` all pass. (`cargo test -p litebox --lib` has
TWO PRE-EXISTING, UNRELATED compile errors on Windows host -- `litebox/src/fs/nine_p/tests.rs`
uses `std::os::unix` directly, and `litebox/src/mm/tests.rs`'s `DummyVmemBackend` is missing
`TASK_ADDR_MIN`/`TASK_ADDR_MAX` trait items -- confirmed via `git diff --stat` that neither file
was touched this session; do not attribute these to the fix above.)

**Next step for whoever picks this up**: the standing goal is XFCE (not bare weston) starting
flawlessly. This session's repro used `--shell=kiosk-shell.so` with no client apps launched, which
only proves the compositor itself is stable -- it does NOT launch labwc/xfsettingsd/xfce4-panel/
xfdesktop. Two gaps to close next: (1) `.wfgy/xfce-build/xfce-layer-FINAL.tar` has no `labwc`
binary in it at all (only `usr/share/xfce4/labwc/*` config files) -- confirm which layer tar
actually ships a real `labwc` binary (check `xfce-layer15.tar`/`xfce-layer16.tar`, named in the
2026-08-28 section below as already having labwc installed) and re-run the FULL documented
`labwc -s "xfsettingsd & xfce4-panel & xfdesktop &"` command through this now-fixed libinput/
fallocate/migrate_file_up path; (2) re-verify the mallocng fix (`b4a40e3d`) and this session's
three fixes all coexist cleanly against whichever layer tar actually has labwc, since none of
that combination has been tested together yet -- this session only tested against
`xfce-layer-FINAL.tar` (which has the XKB/DRM/libinput fixes' prerequisites but not labwc itself).

## Session update (2026-08-30): mallocng blocker CONFIRMED FIXED, new blocker found

The `.meta=0` mallocng crash blocking npx/casey/weston is confirmed fixed on current HEAD
(commit `b4a40e3d`, `syscall_callback` stack-switch-before-pushfq fix) -- verified live this
session via the documented repro (`alpine-pinned2.tar` + `xfce-layer-FINAL.tar` via
`--resume-from`, `seatd -l debug & weston --backend=drm-backend.so --use-pixman`). No crash;
seatd session negotiation succeeds cleanly through to opening `/dev/dri/card0` and
`/dev/input/event0`.

**New blocker, next in the critical path**: weston's `libinput` backend logs
`event0 - not using input device '/dev/input/event0'` -> `warning: no input devices on
entering Weston` -> `failed to create input devices` -> `fatal: failed to create compositor
backend`. seatd opens then immediately closes both devices in the same tick (this is
`libinput_udev_create_context`'s own `device_added()` in `src/udev-seat.c` calling
`close_restricted` after `evdev_device_create()` returns `EVDEV_UNHANDLED_DEVICE`, not a
litebox-side close race -- confirmed by full libinput 1.31.3 source cross-reference at
`.wfgy/xfce-build/libinput-src/libinput-1.31.3/`).

Root cause NOT yet found despite exhaustive static tracing against the real libinput source:
- `evdev_device_create()` returns `EVDEV_UNHANDLED_DEVICE` specifically when
  `device->seat_caps == EVDEV_DEVICE_NO_CAPABILITIES` after configuration (`evdev.c:2380`).
  This can happen via TWO different code paths that produce the IDENTICAL log line, and the
  log capture so far cannot distinguish which one fires:
  1. The udev-tag gate at `evdev.c:2354` (`(udev_tags & EVDEV_UDEV_TAG_INPUT) == 0 ||
     (udev_tags & ~EVDEV_UDEV_TAG_INPUT) == 0`) rejecting the device outright before any
     capability configuration -- would ALSO log "not tagged as supported input device" via
     `evdev_log_info`, which was NEVER observed in any repro run's captured output. This
     absence is evidence AGAINST this path, but not proof (that specific log line's
     visibility through weston's own log forwarding was not independently confirmed).
  2. `evdev_configure_device()` running fully (tag check passes) but ending up with zero
     `seat_caps` bits set anyway -- would ALSO log "is tagged by udev as: ..." (`evdev.c:1608`),
     which was ALSO never observed. Same ambiguity.
- Verified CORRECT by direct source read (litebox's own, real semantics match real kernel):
  `litebox/src/fs/devices.rs`'s `UdevDb` backend content (`E:ID_INPUT=1\nE:ID_INPUT_MOUSE=1\n
  E:ID_INPUT_KEYBOARD=1\n`, exactly 54 bytes, confirmed read in full via live log
  `sys_read fd=16 ... result=Ok(54)`), `SysClassInput`'s `uevent` content
  (`MAJOR=13\nMINOR=64\nDEVNAME=input/event0\nSUBSYSTEM=input\n`), `is_input_device`'s rdev
  match (`rdev=Some((13,64))` confirmed live), and the full `EvdevGetBits`/`GetId`/`GetName`/
  `GetVersion`/`GetProp` ioctl sequence (all succeed, all return real, correctly-shaped data
  matching a keyboard+mouse device per `litebox_common_linux`'s real `EV_KEY`/`EV_REL`/
  `BTN_LEFT` etc constant values).
- TESTED AND RULED OUT this session: (a) an `I:0\n` initialization-timestamp line prepended to
  the udev db content -- no behavior change, reverted; (b) `EVIOCGBIT`/`EVIOCGPROP` returning
  a bare `Ok(0)` success code instead of the real-kernel byte-count return value -- this WAS a
  genuine bug (real `ioctl(EVIOCGBIT)` returns bytes written, litebox was returning a bare 0)
  and IS FIXED AND COMMITTED (`litebox_shim_linux/src/syscalls/file.rs`,
  `IoctlArg::EvdevGetBits`/`EvdevGetProp` handlers), confirmed correct by kernel semantics and
  177/177 `litebox_shim_linux` tests still passing -- but empirically confirmed via identical
  ioctl-call-count before/after (15 calls both runs) that libinux/libevdev does not even
  consult this specific return value in its actual code path taken here, so this fix, while
  real and worth keeping, is NOT what's blocking the input-device rejection.

**Concrete next step for whoever picks this up**: get direct evidence of WHICH of the two
`EVDEV_UNHANDLED_DEVICE` code paths fires -- either patch a local libinput build with extra
eprintf tracing at `evdev.c:2354` and `evdev.c:2380` and get it into the guest rootfs (real
source modification of a LOCAL DEBUG BUILD, not the shipped Alpine package -- keep separate
from the guest's real `/usr/lib/weston/libinput.so.10`), or set `WESTON_LOG_LEVEL`/build
weston+libinput with `-Ddebug-gui=true`/`meson -Dbuildtype=debug` for real per-line source
tracing, then re-run the exact repro in `.wfgy/xfce-build/run_repro_final.ps1`-style invocation
documented below. Once the exact rejection line is captured, the fix is almost certainly a
small, targeted litebox change (either the udev tag properties need a currently-missing
property libinput's tag table doesn't obviously require based on source alone, e.g. a stray
different property name check earlier in `evdev_configure_device` that gates BEFORE reaching
the `EVDEV_UDEV_TAG_KEYBOARD`/`MOUSE` branches, or the sysfs/`is_input_device` check has a
subtle real-vs-litebox mismatch not caught by this session's source-level comparison).

# AGENTS.md — handoff note (2026-08-28)

## Active standing goal (session-scoped Stop hook on the originating machine)

> "go ahead and push all the way till xfce starts flawlessly, fix any bug that arises first"

This is being worked via `/goal` on litebox-main.

## Architecture & Subsystem Mapping Status

- **XFCE & Wayland/DRM in Stock Alpine**:
  - Wayland compositor (`labwc`) launches XFCE session tools (`xfsettingsd & xfce4-panel & xfdesktop &`).
  - Command: `udevd --daemon && labwc -s "xfsettingsd & xfce4-panel & xfdesktop &"`.
- **DRM-to-wgpu Mapping**:
  - Virtual DRM device (`/dev/dri/card0`) handled in `litebox_shim_linux/src/syscalls/drm.rs`.
  - Implements dumb buffer creation (`DRM_IOCTL_MODE_CREATE_DUMB`), mmap offsets (`DRM_IOCTL_MODE_MAP_DUMB`), framebuffer attachment (`DRM_IOCTL_MODE_ADDFB2`), and page flips (`DRM_IOCTL_MODE_PAGE_FLIP`).
  - Flipped frames pass to host `wgpu` surface presentation (`litebox_platform_windows_userland::presentation::Presenter`).

Two hypotheses tested and refuted so far:
- **Not** a missing/non-daemonized `udevd` — installing `eudev` + starting `udevd` (with and
  without `--daemon`) made zero difference, identical error both ways.
- The earlier claim that "`sys_socket` is never called in the log" (which would suggest the
  bad fd is inherited/dup'd rather than freshly socket()'d) is **suspect, not confirmed** —
  a later fork found there's no literal `"sys_socket"` string logged anywhere in the
  codebase, so a naive `grep -c sys_socket` on the debug log trivially returns 0 regardless
  of whether the syscall actually happened. litebox DOES have a real, purpose-built
  `AF_NETLINK` socket shim (`litebox_shim_linux/src/syscalls/netlink.rs`, explicitly built
  "enough for `udev_monitor_new_from_netlink()` to succeed"), and `do_socket` correctly
  routes `AddressFamily::NETLINK` there (`net.rs:1131`). **This needs to be re-checked
  properly**: find out what litebox's real debug-log tag/format is for socket syscalls
  (probably logged by syscall number or a generic dispatch trace, not the string
  "sys_socket"), then re-run the repro and check honestly whether the netlink socket call
  is actually happening or not.

## Linux-leg status update (2026-08-28, after the above hypotheses)

Native Linux shim baseline is fully green (186 passed / 0 failed, both serial and parallel) and
the SIG_IGN/register_exception_handlers + netlink-test fixes are committed+unch as lanmower
`dba81ae`. Full detail drained to memory `mem-7dee60fe537615a1-2114` (rs-learn). Net effect for
the blocker: the udev netlink sockaddr path is real and non-panicking, so the "Bad file
descriptor" investigation is about *downstream* fd handling (dup/fcntl/close/fork-inheritance,
epoll registration), not the netlink parse. Full labwc/DRM repro still needs the Windows host
(this container has no `/dev/dri`/`/dev/net/tun`/cargo).

## Concrete next step

1. Reuse `.wfgy/xfce-build/xfce-layer15.tar` + `alpine-pinned2.tar` (already has eudev
   installed, already has a real official labwc `rc.xml` at `/.config/labwc/rc.xml`, exact
   `0.20.0` tag from labwc's own GitHub repo). Do not rebuild unless something is missing.
2. Repro: `udevd --daemon && labwc -s "xfsettingsd & xfce4-panel & xfdesktop &"` with
   `LITEBOX_LOG=debug`, real litebox guest process via
   `litebox_runner_linux_on_windows_userland.exe` (never WSL/Hyper-V/any hypervisor).
3. Find litebox's real log format for socket-family syscalls (grep the source for what gets
   logged in `do_socket`/the syscall dispatch trace, not just the literal word "sys_socket"),
   then check the fresh debug log for whether `AF_NETLINK`/`SOCK_RAW` is actually requested.
4. If it is: the bug is downstream of the netlink socket handshake, not its absence — trace
   what fd number the socket got, then what happens to that fd right before the
   "Bad file descriptor" error (dup/fcntl/close/fork-inheritance across
   litebox's fd-table handling — this investigation has fixed several fd-table and
   fork/execve-related bugs already, so a similar bug here is plausible).
5. If it is genuinely never called: trace backward for `sys_dup`/`sys_dup2`/`sys_dup3`/
   `sys_fcntl`/`sys_open`/`sys_openat` activity by the same tid leading up to the failure,
   to find where the fd seatd/wlroots is already holding actually came from.
6. Fix minimally and surgically in litebox's own source only (or official, unmodified Alpine
   packages/config — never binary-patch/recompile a guest package). Run
   `cargo test -p litebox_shim_linux --lib -- --skip test_mremap` and
   `cargo test -p litebox_platform_windows_userland` for real pass/fail counts, reproduce
   fresh 10+ times, then commit/push.
7. If labwc gets past this and its own `-s` startup targets (`xfsettingsd`/`xfce4-panel`/
   `xfdesktop`) launch (real `sys_execve` log lines) and survive an extended window
   (90-150+ seconds, log-based only — search for `fatal signal:`/`sys_exit_group`, **never**
   `busybox kill -0`, confirmed unreliable in this rootfs) — that is the actual, final
   completion of the standing goal. Triple-check with real quoted evidence before claiming it.

## Hard constraints (non-negotiable, apply on any machine)

- Never use WSL2/WSL1/Hyper-V/any hypervisor — real litebox guest process on bare Windows
  via `litebox_runner_linux_on_windows_userland.exe` only.
- Never take a full-screen screenshot — crop-capture via `GetWindowRect`, or log-only
  evidence.
- Never recompile, binary-patch, or otherwise modify any guest package/binary — fixes go in
  litebox's own source, or use official unmodified Alpine packages/config/env-vars as-is.
- Commits authored **only** as `lanmower <657315+lanmower@users.noreply.github.com>` — never
  attribute Claude anywhere.
- Zero branches/worktrees — work directly on `main`. If a stray branch exists or `main` is
  named `master`, consolidate/rename to `main`.
- **Evidentiary discipline**: this investigation had one real fabrication incident (a fork
  invented a commit hash and false "stable" claims), caught via independent `git log`/file
  verification. Since then every claim — by any agent, on any machine — must be backed by
  real, quoted tool output. Never invent a fix, a passing test, or a "confirmed running"
  claim. Report honest negative results. A near-zero `tool_uses` count relative to claimed
  work volume is the cheapest, most reliable fabrication tell — check it first on any
  handoff report before trusting it.
- `busybox kill -0 $PID` is confirmed unreliable in this rootfs (false "dead" signals for
  live processes) — use absence of `fatal signal:`/`sys_exit_group` in a full
  `LITEBOX_LOG=debug` capture, or a real `$!`-captured PID's `kill -0`, as liveness evidence
  instead.

## Rootfs/artifact locations

- `.wfgy/xfce-build/xfce-layer15.tar` — current furthest-progressed rootfs (weston's
  `kiosk-shell.so` config fix + official labwc `rc.xml`). Use this directly.
- `.wfgy/xfce-build/alpine-pinned2.tar` — base tar always paired with the layer-N overlay.
- Large scratch artifacts in the working tree (`target-myfork/`, `alpine-fresh-test.tar`,
  `.agentplug/`) are local build/test byproducts, intentionally untracked — not needed to
  continue this work, safe to ignore or regenerate.
