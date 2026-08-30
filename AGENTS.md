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
