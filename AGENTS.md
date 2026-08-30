# AGENTS.md — handoff note (2026-08-30)

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
