# AGENTS.md — handoff note (2026-08-30, sub-session 8)

## Active standing goal (session-scoped Stop hook on the originating machine)

> "go ahead and push all the way till xfce starts flawlessly, fix any bug that arises first"

This is being worked via `/goal` on litebox-main, gm session_id `litebox-xfce-1`. Full
chronological detail of every prior sub-session's investigation, ruled-out hypotheses, and
fixes lives in gm's memory store (`recall`/`codesearch` against this project) as resolved
mutables — do not re-derive from scratch; query the recall store first (e.g. search
"wlroots shm keymap crash", "DRM backend creation", "mallocng syscall_callback",
"libinput evdev unhandled device").

## Current state (as of sub-session 7)

**Fixed and pushed, verified live, in order**:
1. mallocng `.meta=0` crash (`syscall_callback` stack-switch-before-pushfq) — commit `b4a40e3d`.
2. libinput evdev device rejection (missing `EVIOCGKEY`/`EVIOCGLED`/`EVIOCGSW`), missing
   `fallocate`, a `migrate_file_up` panic — commit `5458d74c`. weston alone now stable 150+s.
3. `/sys/dev/char/226:0` DRM reverse-lookup for wlroots — commit `1f51bf4a`.
4. Full `device/drm` synthetic sysfs subtree, `DRM_CAP_CRTC_IN_VBLANK_EVENT`/`PRIME`, `226:128`
   render-node lookup, `DRM_IOCTL_GET_MAGIC`/`AUTH_MAGIC` — commit `024d704f`. **labwc's wlroots
   DRM backend now creates successfully end-to-end** (this was the session's original stated
   blocker).
5. Debug instrumentation (`sys_openat` flags, `sys_unlinkat`, `sys_ftruncate` entry logs,
   permanent, harmless) — commit `96acaf17`.
6. **wlroots keymap-shm SIGSEGV, sub-session 7 — FULLY FIXED AND LIVE-VERIFIED, was NOT an
   upstream wlroots bug after all** (sub-session 6's diagnosis above turned out to be an
   incomplete read of the crash — the NULL deref was real, but its trigger was two stacked
   litebox gaps, not upstream wlroots): (a) `sys_fchmod` re-resolved `fd` back to a path and
   called path-based `chmod`, which always failed `ENOENT` once wlroots' `unlink()` removed the
   directory entry — fixed via a new `FileSystem::chmod_fd` trait method operating on the open
   fd directly (`litebox/src/fs/{mod,backend,in_mem,layered,resolver,composer,devices,tar_ro,
   nine_p/mod}.rs`, `litebox_shim_linux/src/syscalls/file.rs`); (b) once (a) let `fchmod`/
   `ftruncate` succeed, `sys_mmap(MAP_SHARED|PROT_WRITE)` on wlroots' plain (non-`memfd_create`)
   shm file still hit litebox's deliberate `ENODEV` rejection for ordinary file-backed writable
   shared mappings — fixed by tagging any fd still open on a just-unlinked regular file with the
   same `MemfdMarker` `sys_memfd_create` applies at creation (new `sys_unlinkat` helper
   `tag_unlinked_regular_file_as_shm_like`, `litebox_shim_linux/src/syscalls/file.rs`), since
   wlroots'/weston's hand-rolled shm-file recipe (`open`+`open`+`unlink`+`fchmod`+`ftruncate`) is
   structurally identical to what `sys_memfd_create` does internally — this makes the existing
   memfd real-shared-memory-backing machinery (`try_memfd_mmap`/`resize_memfd_shared_backing`)
   cover it with zero changes to `sys_mmap` itself. Commit: see `git log` (this sub-session).

7. **`DRM_IOCTL_PRIME_HANDLE_TO_FD`/`DRM_IOCTL_PRIME_FD_TO_HANDLE`/`DRM_IOCTL_GEM_CLOSE` — sub-
   session 8, FULLY FIXED AND LIVE-VERIFIED**: the swapchain-buffer-acquire SIGABRT (`render/
   allocator/drm_dumb.c:90 Failed to get PRIME handle from GEM handle: Invalid argument` →
   `Assertion failed: width > 0 && height > 0` → SIGABRT ~29.5s) is gone. Implemented three real
   ioctl handlers in `litebox_shim_linux/src/syscalls/drm.rs`/`file.rs` (consts/structs/`IoctlArg`
   variants in `litebox_common_linux/src/lib.rs`): `DRM_IOCTL_PRIME_HANDLE_TO_FD` (`0xc00c642d`)
   re-opens `/dev/dri/card0` for a fresh real fd and tags it (new per-fd `DrmPrimeFdMarker`,
   `file.rs`) with the exported dumb buffer's `MAP_DUMB` offset, so `syscalls::mm::
   try_dri_dumb_buffer_mmap` resolves the guest's later `mmap(prime_fd, ..., 0)` back onto the
   SAME real shared-memory handle the original `CREATE_DUMB` established (no real dma-buf
   subsystem needed — single-client device, see the new code's own doc comments);
   `DRM_IOCTL_PRIME_FD_TO_HANDLE` (`0xc00c642e`) resolves the tagged fd back to the originating
   GEM handle (same-handle self-import round-trip); `DRM_IOCTL_GEM_CLOSE` (`0x40086409`) is a
   real no-op-success for any still-live handle (this device has no per-handle refcounting — real
   teardown stays solely `DRM_IOCTL_MODE_DESTROY_DUMB`'s job). All three were discovered
   sequentially via live iteration against the real repro (each fixed gap unmasked the next
   `Raw{cmd:...}` ioctl fallthrough down the same real wlroots call chain). Commit: see `git log`
   (this sub-session).

**Current state**: labwc now survives past ALL of DRM backend creation, keyboard/keymap setup,
AND swapchain/output buffer allocation — `xfsettingsd` genuinely `sys_execve`'s at ~27.2s (real
log line: `sys_execve: entry tid=19 path=/usr/bin/xfsettingsd`) and runs for ~1.3s (reads its own
ELF/shared libs, real `sys_read`/`sys_fstat` activity, no fault) before exiting cleanly via
`sys_exit_group status=Exit(1)` — an ordinary application-level exit(1), NOT a signal/crash/
assertion (confirmed: no `fatal signal:`, no `Assertion failed`, no `[ERROR]` line anywhere
between its execve and its exit in a full `LITEBOX_LOG=debug` capture). `xfce4-panel`/`xfdesktop`
are never observed to launch in this run — labwc's `-s` session command only ever spawned the one
`xfsettingsd` process, never the `& xfce4-panel & xfdesktop &` continuation, in this capture.
**This is a NEW blocker outside this sub-session's DRM/PRIME scope** — not yet root-caused (likely
either xfsettingsd itself failing on a missing D-Bus/config dependency, or labwc's `-s` argument
shell not actually chaining the three `&`-joined commands the way `/bin/sh -c` would) — left for
the next sub-session. PRD row `drm-prime-handle-to-fd-not-implemented-blocks-xfce-launch` is
RESOLVED; this new gap needs its own fresh PRD row and investigation.

## Repro command (unchanged shape, current known-good)

```
target/release/litebox_runner_linux_on_windows_userland.exe --initial-files .wfgy/xfce-build/alpine-pinned2.tar --resume-from .wfgy/xfce-build/xfce-layer16.tar -- /bin/sh -c "mkdir -p /run/user/1000 /dev/shm; chmod 700 /run/user/1000; chmod 1777 /dev/shm; export XDG_RUNTIME_DIR=/run/user/1000; export XKB_CONFIG_ROOT=/usr/share/X11/xkb; export WLR_RENDERER=pixman; seatd -l debug & for i in 1 2 3 4 5 6 7 8 9 10; do [ -S /run/seatd.sock ] && break; sleep 1; done; labwc -s \"xfsettingsd & xfce4-panel & xfdesktop &\""
```
with `LITEBOX_LOG=debug` (add `LITEBOX_DIAG_FATALDUMP=1` for crash register/instruction-byte
capture), `MSYS_NO_PATHCONV=1` in Git Bash. Rebuild
`cargo build --locked --release -p litebox_runner_linux_on_windows_userland` first. Regression
suite: `cargo test -p litebox_shim_linux --lib -- --skip test_mremap` (177/177 pass as of
sub-session 8) and `cargo test -p litebox_platform_windows_userland` (4/4 pass).

## Completion criterion (unchanged, NOT YET MET)

labwc's own `-s "xfsettingsd & xfce4-panel & xfdesktop &"` session targets launch (real
`sys_execve` log lines) and survive a 90-150+ second window with no `fatal signal:`/
`sys_exit_group` in a `LITEBOX_LOG=debug` capture — log-based evidence only, never
`busybox kill -0` (confirmed unreliable in this rootfs). As of sub-session 8, labwc itself now
survives past DRM backend creation, keyboard/keymap setup, AND output/swapchain buffer allocation
(all three previously-fatal walls); `xfsettingsd` genuinely `sys_execve`'s and runs briefly before
its own clean (non-signal) `exit(1)`, and `xfce4-panel`/`xfdesktop` are never observed to launch
in the same run — closer than any prior sub-session (the DRM buffer-allocation SIGABRT that
previously ended every run before ANY session target could execve is now fully gone), but the
full three-process session still has not survived a 90-150s window in a captured run.

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

## Rootfs/artifact locations

- `.wfgy/xfce-build/xfce-layer16.tar` — current furthest-progressed rootfs, has real `usr/bin/labwc`.
- `.wfgy/xfce-build/alpine-pinned2.tar` — base tar, always paired with a layer-N overlay via `--resume-from`.
- Large scratch artifacts in the working tree (`target-myfork/`, `alpine-fresh-test.tar`,
  `.agentplug/`) are local build/test byproducts, gitignored, safe to ignore or regenerate.
