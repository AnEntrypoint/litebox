# AGENTS.md — handoff note (2026-08-30, sub-session 10)

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

**Current blocker (two parts, both real, both confirmed this session)**:

**(a) D-Bus machine-id — trivial, NOT a litebox bug, just a repro-command gap.**
`xfsettingsd` needs `/var/lib/dbus/machine-id` (real D-Bus setup requirement, not litebox's
concern) and a running session bus. Fix: add to the launch shell command (see repro below)
`mkdir -p /var/lib/dbus; dbus-uuidgen --ensure=/var/lib/dbus/machine-id; dbus-daemon --session
--fork --print-address`. Confirmed this makes D-Bus start correctly and `xfsettingsd`
execve successfully.

**(b) fork_verify stale-pointer gap — REAL litebox bug, confirmed root cause, NOT YET
SAFELY FIXED, high platform-layer risk.** With (a) fixed, `xfsettingsd` execve's but its
`dbus-daemon` (a plain fork, not exec) repeatedly forks per-connection child processes, and
EVERY forked child SIGSEGVs shortly after `open(/dev/null)`, following a dense burst of
`fork_verify` "stale pointer, translating" WARN lines — i.e. fork_verify (litebox's
post-fork pointer-corruption healer, same bug family as fix #1 above) is catching MOST but
not ALL stale pointers in this fork's post-resume execution.

Two sub-causes identified, tried, and both currently REVERTED (working tree is clean at
commit `17312da4` — do not assume either fix below is live):

- **(1b) Register-to-register propagation** (`litebox_platform_windows_userland/src/fork_verify.rs`):
  no case covers a plain `mov reg, reg`/`movzx`/`movsx` with zero memory operands copying a
  stale value between registers. A drafted, narrow fix (translate ONLY the source register,
  never the destination — an earlier two-register-translating draft caused its own
  corruption) genuinely eliminates the ORIGINAL guest-level SIGSEGV crash class when tested
  in isolation. Patch preserved at
  `%TEMP%\claude\...\scratchpad\xfce-repro-logs\.gm-scratch-fork-verify-fix.patch`
  (also copy this into a durable project location if picking this up — the scratchpad may not
  survive across machines/sessions).
- **(1c) Syscall-argument translate at the syscall-callback disarm boundary**: (1b) ALONE
  does not fully fix things — it delays the crash much further (confirmed: dbus-daemon writes
  its session-bus address file successfully, 8800+ single-step traps survived vs. dying
  almost immediately) but then hits a DIFFERENT, WORSE failure: once traced execution reaches
  a real `syscall` instruction, fork_verify correctly disarms single-stepping
  (`!is_in_destination(rip)` at line ~697) and the syscall's host-side implementation runs
  UNVERIFIED — a stale pointer in an ABI argument register at that exact instant is never
  healed, producing a HOST-level `STATUS_ACCESS_VIOLATION` (whole-process crash, exit 139),
  strictly worse than the original guest-level SIGSEGV. **This session attempted a fix for
  this exact gap (translate the six Linux x86-64 syscall ABI registers — rdi/rsi/rdx/r10/r8/r9
  — right at the disarm point) and it made things WORSE, not better: the process crashed
  MUCH earlier (at ~1.26s, before even reaching labwc) instead of at ~44s+.** The attempted
  fix's exact code is NOT preserved (reverted without saving) — whoever picks this up should
  treat it as a known-bad approach shape to avoid repeating verbatim, but the underlying goal
  (heal syscall-argument registers at the disarm boundary) is still the right target; the
  bug is likely in exactly HOW the translate-and-write is done (register selection, write
  ordering relative to `rax`/`rcx`/`r11` — which x86-64 `syscall` itself clobbers and which
  this file's own module docs may have guidance on that a rushed attempt missed — or a subtle
  ordering issue with when EFlags/TF gets cleared relative to the register writes).
  **This needs the same level of careful, register-semantics-aware investigation the (1b) fix
  clearly had — do not attempt a quick patch without first reading the ENTIRE
  `fork_verify.rs` module doc comment (its "why this design" reasoning is extensive and
  directly relevant) and understanding exactly what `syscall_callback`'s host-side dispatch
  does with each argument register immediately after this disarm point.**

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

## Repro command (current known-good, includes the D-Bus fix from (a) above)

```
target/release/litebox_runner_linux_on_windows_userland.exe --initial-files .wfgy/xfce-build/alpine-pinned2.tar --resume-from .wfgy/xfce-build/xfce-layer17.tar -- /bin/sh -c "mkdir -p /run/user/1000 /dev/shm /var/lib/dbus; chmod 700 /run/user/1000; chmod 1777 /dev/shm; export XDG_RUNTIME_DIR=/run/user/1000; export XKB_CONFIG_ROOT=/usr/share/X11/xkb; export WLR_RENDERER=pixman; dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>&1 || true; dbus-daemon --session --fork --print-address 2>&1 || true; seatd -l debug & for i in 1 2 3 4 5 6 7 8 9 10; do [ -S /run/seatd.sock ] && break; sleep 1; done; labwc -s \"xfsettingsd & xfce4-panel & xfdesktop &\""
```
with `LITEBOX_LOG=debug` (add `LITEBOX_DIAG_FATALDUMP=1` for crash register/instruction-byte
capture), `MSYS_NO_PATHCONV=1` in Git Bash. Rebuild
`cargo build --locked --release -p litebox_runner_linux_on_windows_userland` first. When
grepping the resulting log for specific `tid=`/`pid=` values, strip ANSI color codes first
(`sed 's/\x1b\[[0-9;]*m//g' logfile > clean.log`) — plain grep silently misses matches
embedded in colored lines otherwise (confirmed this session). Regression suite:
`cargo test -p litebox_shim_linux --lib -- --skip test_mremap` (177/177 pass as of last
commit) and `cargo test -p litebox_platform_windows_userland` (4/4 pass).

## Completion criterion (unchanged, NOT YET MET)

labwc's own `-s "xfsettingsd & xfce4-panel & xfdesktop &"` session targets launch (real
`sys_execve` log lines) and survive a 90-150+ second window with no `fatal signal:`/
`sys_exit_group` (Signal) in a `LITEBOX_LOG=debug` capture — log-based evidence only, never
`busybox kill -0` (confirmed unreliable in this rootfs). As of sub-session 10, `xfsettingsd`
genuinely `sys_execve`'s (real progress) but exits(1) or crashes (via its dbus-daemon fork
children's SIGSEGV) before `xfce4-panel`/`xfdesktop` ever launch.

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
