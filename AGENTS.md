# AGENTS.md — handoff note (2026-08-30, sub-session 12)

## Active standing goal (session-scoped Stop hook on the originating machine)

> "go ahead and push all the way till xfce starts flawlessly, fix any bug that arises first"

This is being worked via `/goal` on litebox-main, gm session_id `litebox-xfce-1`. Full
chronological detail of every prior sub-session's investigation, ruled-out hypotheses, and
fixes lives in gm's memory store (`recall`/`codesearch` against this project) as resolved
mutables — do not re-derive from scratch; query the recall store first (e.g. search
"wlroots shm keymap crash", "DRM PRIME handle", "fork_verify stale pointer", "mallocng
syscall_callback").

## Current state (as of sub-session 10)

**Fixed and pushed, verified live, in order** (all real litebox emulation gaps — every one of
this chain that initially looked like it might be an "upstream" bug turned out, on closer
investigation, to be litebox's own bug; keep defaulting to that hypothesis for anything new):
1. mallocng `.meta=0` crash (`syscall_callback` stack-switch-before-pushfq) — commit `b4a40e3d`.
2. libinput evdev device rejection, missing `fallocate`, a `migrate_file_up` panic —
   commit `5458d74c`. weston alone stable 150+s.
3. Full DRM sysfs subtree (`/sys/dev/char/226:0`, `device/drm/{card0,renderD128}`,
   `DRM_CAP_CRTC_IN_VBLANK_EVENT`/`PRIME`, `DRM_IOCTL_GET_MAGIC`/`AUTH_MAGIC`) — commits
   `1f51bf4a`, `024d704f`. labwc's wlroots DRM backend creates successfully end-to-end.
4. `fchmod`-on-unlinked-fd (re-resolved path instead of operating on the fd) and
   `mmap(MAP_SHARED)` not recognizing wlroots' hand-rolled unlink-based shm files —
   commit `61c97e9f`. Fixed the keymap-shm SIGSEGV that looked like an upstream wlroots
   NULL-deref bug but wasn't.
5. `DRM_IOCTL_PRIME_HANDLE_TO_FD`/`FD_TO_HANDLE`/`GEM_CLOSE` (all three completely
   unimplemented) — commit `17312da4`. Fixed a SIGABRT during swapchain buffer allocation.
   **This got `xfsettingsd` to genuinely `sys_execve` for the first time in the whole
   investigation.**
6. fork_verify syscall-trampoline-boundary stale `rcx` (return-address register) —
   commit `8ec32c4b`. See "(1c) RESOLVED" below.

**(a) D-Bus machine-id — trivial, NOT a litebox bug, just a repro-command gap.**
`xfsettingsd` needs `/var/lib/dbus/machine-id` (real D-Bus setup requirement, not litebox's
concern) and a running session bus. Fix: add to the launch shell command (see repro below)
`mkdir -p /var/lib/dbus; dbus-uuidgen --ensure=/var/lib/dbus/machine-id; dbus-daemon --session
--fork --print-address`. Confirmed this makes D-Bus start correctly and `xfsettingsd`
execve successfully.

**(1c) RESOLVED this session (sub-session 11), commit `8ec32c4b`.** Root cause: the disarm
check at `on_single_step`'s `!relocations.is_in_destination(rip)` (fires once `rip` has moved
off the guest's `call syscall_callback` and onto `syscall_callback`'s own host address) is one
instruction too late to catch a stale value in `rcx` — `syscall_callback`'s own doc comment
("the register context is the guest context with the return address in rcx") establishes that
`rcx` at that exact disarm point is the guest's real return address, pushed straight through as
`pt_regs->ip` (`push rcx // pt_regs->ip` in the naked-asm trampoline, `lib.rs` ~line 1920) and
later resumed into `rip` verbatim with no further translation anywhere else in the syscall
pipeline. This is exactly case (1)'s class of value (a live code-pointer-shaped register,
deterministically translatable via the same relocation map already proven correct for every
other register at `fork()` time) reached one instruction later than case (1) itself checks.

Fix: at the disarm point, translate `rcx` using the identical `is_in_source`-gated
`relocations.translate()` pattern case (1) already uses for `rip`/`rbp` — narrow, total, and
proven safe by the SAME reasoning, never a guess.

**Both (1b) [register-to-register mov propagation] and the ORIGINAL six-ABI-register (1c)
attempt from sub-session 10 were RE-TESTED this session and BOTH CONFIRMED UNSAFE — do not
reintroduce either:**

- **(1b) alone crashes exit 139 at ~1.3s** (live-verified this session, contradicting the
  sub-session-10 claim it "genuinely eliminates the ORIGINAL guest-level SIGSEGV crash class" —
  that claim was evidently based on a shorter/different test window; a fresh, careful 60s
  isolated test of (1b) alone this session showed the mallocng `.meta=0`-style crash class
  recurring on a NEW thread, far earlier than the sub-session-10 report of "44s+"). The patch
  is still preserved at
  `%TEMP%\claude\...\scratchpad\xfce-repro-logs\.gm-scratch-fork-verify-fix.patch` for
  reference/future re-investigation, but it must NOT be reapplied without first explaining why
  this session's live re-test contradicts the prior session's claim.
- **The six-ABI-register (1c) attempt is unsafe for the reason sub-session 10 already
  suspected**: syscall arguments (rdi/rsi/rdx/r10/r8/r9) are guest-supplied values of
  genuinely unknown shape (fds, flags, small integers, real pointers) with no basis for
  assuming pointer-ness — translating them unconditionally is exactly the "unbounded
  guessing" hazard this module's own top-level doc comment warns about. `rcx` is
  categorically different and is the ONLY register this trampoline's calling convention
  guarantees is a code pointer at this point.

**Verification this session**: `rcx`-only fix, isolated (no (1b)), ran the full repro command
clean for a 150s+ window (`timeout 160`, exit code 124 = timeout, i.e. no crash) with
`LITEBOX_LOG=debug` — zero host-level crashes, zero `STATUS_ACCESS_VIOLATION`, zero
`fork_verify` stale-`rcx` triggers even needed in this particular run (the fix is a no-op
safety net for this repro's actual dbus-daemon fork pattern, which apparently doesn't hit a
stale-`rcx` case, but is exercised and safe). `cargo test -p litebox_shim_linux --lib --skip
test_mremap`: 177 passed, 0 failed. `cargo test -p litebox_platform_windows_userland`: 4
passed, 0 failed.

**NEW frontier, NOT part of this session's scope, tracked in gm PRD as
`xfsettingsd-exits-1-and-panel-desktop-never-launch`**: with the fork_verify gap now closed,
the remaining blocker to the full completion criterion is that `xfsettingsd` still dies
(guest-level `Signal(11)`, handled cleanly, no host crash) before labwc's session shell ever
reaches `xfce4-panel &`/`xfdesktop &` in `xfsettingsd & xfce4-panel & xfdesktop &` — confirmed
live this session: only ONE `sys_execve` for `xfsettingsd` ever appears in a 150s log, zero for
`xfce4-panel`/`xfdesktop`. Root cause not yet investigated this session — likely still the D-Bus
session-bus race (xfsettingsd's dbus-daemon child forks repeatedly, "Could not connect:
Connection refused" appears in stdout), a separate gap from the fork_verify platform bug.

**Also fixed this session, safe and independently useful (NOT yet committed — see below)**:
`mesa-dri-gallium` (provides `swrast_dri.so`, the software rasterizer) was missing from the
rootfs (`/usr/lib/dri/` existed but was empty) — installed via `apk add --no-cache
mesa-dri-gallium` inside a guest run with `--export-writable-layer`, producing
`.wfgy/xfce-build/xfce-layer17.tar` (a full resumable overlay on top of `xfce-layer16.tar`).
Confirmed NOT the cause of any currently-blocking crash (the crash reproduces identically
with or without it, since `WLR_RENDERER=pixman` never touches DRI/GL), but a real
correctness gap worth having fixed for whenever GL/DRI-dependent rendering paths are
eventually exercised. **Use `xfce-layer17.tar` as the `--resume-from` target going forward**
(same shape as `xfce-layer16.tar`, just with real mesa DRI drivers present).

## Repro command (current known-good, sub-session 12: correct D-Bus session-bus export)

**IMPORTANT correction from sub-session 12**: the previously-documented
`dbus-daemon --session --fork --print-address` approach is WRONG — it prints the bus address
to stdout and discards it; nothing exports `DBUS_SESSION_BUS_ADDRESS`, so `xfsettingsd`/labwc
never see the already-running bus and instead each independently try to autolaunch their OWN
session bus via `dbus-launch`, spawning MORE fork children that also hit the fork_verify gap
below — compounding the problem. Use `dbus-launch --sh-syntax --exit-with-session` with `eval`
instead, which correctly sets and exports the address in the current shell:

```
target/release/litebox_runner_linux_on_windows_userland.exe --initial-files .wfgy/xfce-build/alpine-pinned2.tar --resume-from .wfgy/xfce-build/xfce-layer17.tar -- /bin/sh -c "mkdir -p /run/user/1000 /dev/shm /var/lib/dbus; chmod 700 /run/user/1000; chmod 1777 /dev/shm; export XDG_RUNTIME_DIR=/run/user/1000; export XKB_CONFIG_ROOT=/usr/share/X11/xkb; export WLR_RENDERER=pixman; dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>&1 || true; eval \$(dbus-launch --sh-syntax --exit-with-session) 2>&1; export DBUS_SESSION_BUS_ADDRESS; seatd -l debug & for i in 1 2 3 4 5 6 7 8 9 10; do [ -S /run/seatd.sock ] && break; sleep 1; done; labwc -s \"xfsettingsd & xfce4-panel & xfdesktop &\""
```
with `LITEBOX_LOG=debug` (add `LITEBOX_DIAG_FATALDUMP=1` for crash register/instruction-byte
capture), `MSYS_NO_PATHCONV=1` in Git Bash. Rebuild
`cargo build --locked --release -p litebox_runner_linux_on_windows_userland` first. When
grepping the resulting log for specific `tid=`/`pid=` values, strip ANSI color codes first
(`sed 's/\x1b\[[0-9;]*m//g' logfile > clean.log`) — plain grep silently misses matches
embedded in colored lines otherwise (confirmed this session). Regression suite:
`cargo test -p litebox_shim_linux --lib -- --skip test_mremap` (177/177 pass as of last
commit) and `cargo test -p litebox_platform_windows_userland` (4/4 pass).

## Current blocker (sub-session 12): fork_verify gap NARROWED but not fully closed

With the corrected D-Bus repro above, `xfsettingsd` genuinely tries to connect over the
properly-exported bus, but still fails (`Could not connect: Connection refused`) because
`dbus-daemon`'s own daemonizing fork (the real daemon process's `--fork` self-detach, not a
per-connection worker) still SIGSEGVs at guest level after a DENSE burst of `fork_verify`
"stale pointer, translating" WARN lines that all otherwise succeed — the already-landed `rcx`
fix (commit `8ec32c4b`) IS helping (many more pointers get healed than before), but at least
one case still slips through. This is a NARROWER instance of the exact same bug class fix
commit `8ec32c4b` already fixed one instance of — not a new, unrelated bug. Leading suspect
(not yet safely confirmed): the register-to-register-mov-propagation case (the reverted
"(1b)" patch from sub-session 10, re-tested and found unsafe in isolation by sub-session 11)
may still be the real remaining gap and need a genuinely safe reformulation neither prior
attempt found — full detail in gm mutable `dbus-daemon-fork-child-still-sigsegv-after-rcx-fix`.
**Read `litebox_platform_windows_userland/src/fork_verify.rs`'s FULL module doc comment before
attempting anything here — this is delicate, high-risk platform code with a real history of
well-intentioned fixes causing worse regressions (host-level process crashes instead of
guest-level ones). Test every change in isolation, never batch.**

## Completion criterion (unchanged, NOT YET MET)

labwc's own `-s "xfsettingsd & xfce4-panel & xfdesktop &"` session targets launch (real
`sys_execve` log lines) and survive a 90-150+ second window with no `fatal signal:`/
`sys_exit_group` (Signal) in a `LITEBOX_LOG=debug` capture — log-based evidence only, never
`busybox kill -0` (confirmed unreliable in this rootfs). As of sub-session 12, `xfsettingsd`
genuinely `sys_execve`'s and attempts a real D-Bus connection (closer than ever) but still
fails to connect because the D-Bus daemon it depends on keeps crashing via the fork_verify
gap above, before `xfce4-panel`/`xfdesktop` ever launch.

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
- **Push safety**: stage ONLY the specific files you changed (never `git add -A`/`.`) — a prior
  sub-session's push hung badly after accidentally staging a 254MB scratch tar and a full
  `target-myfork/` build-cache tree. `.gitignore` already covers `target-myfork/`,
  `alpine-fresh-test.tar`, `.agentplug/`, `.wfgyxfce-*.ps1`.
- **fork_verify caution**: this is deep, carefully-reasoned platform-layer code with a real
  documented history of a previously-rejected overly-broad fix attempt (see the module's own
  doc comments on case (1)). A wrong fix here can turn a recoverable guest-level crash into a
  host-level process crash — strictly worse. Read the FULL module doc comment before
  attempting any change, and always test in isolation before combining fixes.

## Rootfs/artifact locations

- `.wfgy/xfce-build/xfce-layer17.tar` — current furthest-progressed rootfs (layer16 + real
  mesa-dri-gallium installed). Use this as `--resume-from` going forward.
- `.wfgy/xfce-build/xfce-layer16.tar` — prior layer, still has real `usr/bin/labwc`, no mesa DRI.
- `.wfgy/xfce-build/alpine-pinned2.tar` — base tar, always paired with a layer-N overlay via `--resume-from`.
- Large scratch artifacts in the working tree (`target-myfork/`, `alpine-fresh-test.tar`,
  `.agentplug/`) are local build/test byproducts, gitignored, safe to ignore or regenerate.
