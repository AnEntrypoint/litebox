# AGENTS.md — handoff note (2026-08-30, sub-session 14)

## Active standing goal (session-scoped Stop hook on the originating machine)

> "go ahead and push all the way till xfce starts flawlessly, fix any bug that arises first"

This is being worked via `/goal` on litebox-main, gm session_id `litebox-xfce-1`. Full
chronological detail of every prior sub-session's investigation, ruled-out hypotheses, and
fixes lives in gm's memory store (`recall`/`codesearch` against this project) as resolved
mutables — do not re-derive from scratch; query the recall store first (e.g. search
"fork_verify stale pointer rcx rdi", "DRM PRIME handle", "wlroots shm keymap",
"D-Bus session bus export", "proactive fixup_stale_stack_pointers").

## Current state (as of sub-session 14)

**Fixed and pushed, verified live, in order** (every one of this chain that initially looked
like it might be an "upstream" bug turned out to be litebox's own gap — keep defaulting to
that hypothesis for anything new):
1. mallocng `.meta=0` crash — commit `b4a40e3d`.
2. libinput evdev rejection, missing `fallocate`, `migrate_file_up` panic — commit `5458d74c`.
3. Full DRM sysfs subtree, `DRM_CAP_*`, `DRM_IOCTL_GET_MAGIC`/`AUTH_MAGIC` — commits `1f51bf4a`,
   `024d704f`. labwc's wlroots DRM backend creates successfully.
4. `fchmod`-on-unlinked-fd + `mmap(MAP_SHARED)` on unlink-based shm files — commit `61c97e9f`.
5. `DRM_IOCTL_PRIME_HANDLE_TO_FD`/`FD_TO_HANDLE`/`GEM_CLOSE` — commit `17312da4`. Got
   `xfsettingsd` to genuinely `sys_execve` for the first time.
6. fork_verify: stale `rcx` (return address) at the syscall-trampoline disarm boundary —
   commit `8ec32c4b`.
7. fork_verify: stale `rdi` (first-arg register) at the same case-(1) indirect-call-landing
   boundary — commit `c3182da7`. Narrowed the remaining dbus-daemon fork-child crash further.

**Non-code fix, but real and necessary**: the repro command must use `dbus-launch --sh-syntax
--exit-with-session` + `eval` + explicit `export DBUS_SESSION_BUS_ADDRESS` — `dbus-daemon
--print-address` alone discards the address, causing every D-Bus client to autolaunch its own
session bus (compounding fork_verify crash exposure). See repro command below.

**Current blocker**: `xfsettingsd` genuinely attempts its D-Bus connection but still fails
(`Could not connect: Connection refused`) because `dbus-daemon`'s own daemonizing self-fork
(and other, unrelated forked threads, e.g. `dbus-launch`'s own children) still occasionally
SIGSEGV at guest level (litebox handles these cleanly — no host crash) via the SAME bug class
as fixes #6/#7 above: a stale pointer copied between registers by a plain `mov`-shaped
instruction with no memory operand, which none of fork_verify's existing per-trap cases cover
generically. Fixes #6/#7 each closed ONE specific register at ONE specific transition point
(the case-(1) indirect-call-landing trap); fresh forensics after landing #7 show a DIFFERENT
thread hitting the SAME general pattern via a DIFFERENT register pairing (`rbp`/`rcx` both
holding a stale value, likely from an unrelated `mov rbp, rcx`-shaped instruction elsewhere).

**Recommended next approach — SUCCESSIVE per-register/per-site patches have diminishing
returns; there is likely an unbounded number of distinct sites.** The more scalable fix is
almost certainly PROACTIVE, not reactive: strengthen `fixup_stale_stack_pointers` (in
`litebox_shim_linux`, runs ONCE at `fork()`-resume time, before the child executes anything)
to catch register-to-register-mov-derived staleness patterns before the child ever resumes,
rather than continuing to add one more reactive per-trap case to `fork_verify.rs` for each
newly-discovered site. A genuinely different investigation angle than sub-sessions 10-14's
incremental case-by-case approach is needed here — read `fixup_stale_stack_pointers`'s current
implementation and doc comments first to understand what it already proactively scans and
why, before deciding how to extend it.

**Also landed, safe and independently useful**: `mesa-dri-gallium` (software rasterizer)
installed into `.wfgy/xfce-build/xfce-layer17.tar` (a full resumable overlay on
`xfce-layer16.tar`). Not the cause of any current crash, but a real correctness gap fixed for
whenever GL/DRI-dependent paths are exercised. Use `xfce-layer17.tar` as `--resume-from`.

## Repro command (current known-good)

```
target/release/litebox_runner_linux_on_windows_userland.exe --initial-files .wfgy/xfce-build/alpine-pinned2.tar --resume-from .wfgy/xfce-build/xfce-layer17.tar -- /bin/sh -c "mkdir -p /run/user/1000 /dev/shm /var/lib/dbus; chmod 700 /run/user/1000; chmod 1777 /dev/shm; export XDG_RUNTIME_DIR=/run/user/1000; export XKB_CONFIG_ROOT=/usr/share/X11/xkb; export WLR_RENDERER=pixman; dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>&1 || true; eval \$(dbus-launch --sh-syntax --exit-with-session) 2>&1; export DBUS_SESSION_BUS_ADDRESS; seatd -l debug & for i in 1 2 3 4 5 6 7 8 9 10; do [ -S /run/seatd.sock ] && break; sleep 1; done; labwc -s \"xfsettingsd & xfce4-panel & xfdesktop &\""
```
with `LITEBOX_LOG=debug` (add `LITEBOX_DIAG_FATALDUMP=1` for crash register/instruction-byte
capture), `MSYS_NO_PATHCONV=1` in Git Bash. Rebuild
`cargo build --locked --release -p litebox_runner_linux_on_windows_userland` first. Strip ANSI
color codes before grepping for `tid=`/`pid=` (`sed 's/\x1b\[[0-9;]*m//g' logfile > clean.log`)
or plain grep silently misses matches. Regression suite:
`cargo test -p litebox_shim_linux --lib -- --skip test_mremap` (177/177) and
`cargo test -p litebox_platform_windows_userland` (4/4).

## Completion criterion (unchanged, NOT YET MET)

labwc's own `-s "xfsettingsd & xfce4-panel & xfdesktop &"` session targets launch (real
`sys_execve` log lines) and survive a 90-150+ second window with no `fatal signal:`/
`sys_exit_group` (Signal) in a `LITEBOX_LOG=debug` capture — log-based evidence only, never
`busybox kill -0` (confirmed unreliable in this rootfs).

## Hard constraints (non-negotiable, apply on any machine)

- Never use WSL2/WSL1/Hyper-V/any hypervisor — real litebox guest process on bare Windows via
  `litebox_runner_linux_on_windows_userland.exe` only.
- Never take a full-screen screenshot — crop-capture via `GetWindowRect`, or log-only evidence.
- Never recompile, binary-patch, or otherwise modify any guest package/binary — fixes go in
  litebox's own source, or use official unmodified Alpine packages/config/env-vars as-is.
- Commits authored **only** as `lanmower <657315+lanmower@users.noreply.github.com>` — never
  attribute Claude anywhere.
- Zero branches/worktrees — work directly on `main`.
- **Evidentiary discipline**: every claim must be backed by real, quoted tool output. Never
  invent a fix, a passing test, or a "confirmed running" claim. Report honest negative results.
- `busybox kill -0 $PID` is confirmed unreliable in this rootfs — use log-based liveness
  evidence instead.
- **Push safety**: stage ONLY the specific files you changed (never `git add -A`/`.`).
  `.gitignore` already covers `target-myfork/`, `alpine-fresh-test.tar`, `.agentplug/`,
  `.wfgyxfce-*.ps1`.
- **fork_verify caution**: deep, carefully-reasoned platform-layer code. A broad/blanket fix
  (e.g. translating every register-to-register mov unconditionally) has been tried twice and
  found unsafe both times (trades a recoverable guest crash for a worse host-level process
  crash, or crashes even earlier). Every safe fix landed so far (`rcx`, `rdi`) was narrow: one
  specific, well-understood register, at one specific, well-understood transition point, using
  the same proven `is_in_source`+`translate()` pattern case (1) already established. Read the
  FULL module doc comment before attempting anything; test every change in isolation.

## Rootfs/artifact locations

- `.wfgy/xfce-build/xfce-layer17.tar` — current furthest-progressed rootfs (layer16 + mesa DRI).
  Use as `--resume-from`.
- `.wfgy/xfce-build/xfce-layer16.tar` — prior layer, has real `usr/bin/labwc`, no mesa DRI.
- `.wfgy/xfce-build/alpine-pinned2.tar` — base tar, paired with a layer-N overlay via `--resume-from`.
- Large scratch artifacts (`target-myfork/`, `alpine-fresh-test.tar`, `.agentplug/`) are local
  build/test byproducts, gitignored, safe to ignore or regenerate.
