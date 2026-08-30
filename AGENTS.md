# AGENTS.md — handoff note (2026-08-30, sub-session 7)

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

**Current blocker (NEW, only reachable now that #6 above is fixed — labwc gets much further
than ever before)**: `render/allocator/drm_dumb.c:90 Failed to get PRIME handle from GEM handle:
Invalid argument`, twice, immediately followed by `types/output/swapchain.c:109 Swapchain for
output 'Virtual-1' failed test` and a fatal `Assertion failed: width > 0 && height > 0
(render/swapchain.c: wlr_swapchain_create: 21)` → SIGABRT at ~29.5s, well before
`xfsettingsd`/`xfce4-panel`/`xfdesktop` are ever `sys_execve`'d. Root cause confirmed by direct
code read: `litebox_common_linux/src/lib.rs`'s `DRM_CAP_PRIME` handling unconditionally reports
BOTH `DRM_PRIME_CAP_IMPORT` and `DRM_PRIME_CAP_EXPORT` set via `DRM_IOCTL_GET_CAP` (to satisfy
wlroots' backend-creation-time capability gate), but no `DRM_IOCTL_PRIME_HANDLE_TO_FD` ioctl is
actually implemented in `litebox_shim_linux/src/syscalls/drm.rs` — previously believed
unreachable (per that code's own now-stale doc comment), now proven reachable live: wlroots'
`render/allocator/drm_dumb.c` calls it for real once buffer allocation is attempted, gets
rejected by the generic ioctl catch-all, and the whole swapchain-buffer-acquire path fails.
PRD row `drm-prime-handle-to-fd-not-implemented-blocks-xfce-launch` has full detail and a
concrete fix sketch (implement a real handler near `map_dumb`, `drm.rs` ~line 747, handing back
an fd over the same dumb-buffer host memory `CREATE_DUMB`/`MAP_DUMB` already backs). NOT
attempted this sub-session (a new ioctl surface needs its own design+verification pass).

## Repro command (unchanged shape, current known-good)

```
target/release/litebox_runner_linux_on_windows_userland.exe --initial-files .wfgy/xfce-build/alpine-pinned2.tar --resume-from .wfgy/xfce-build/xfce-layer16.tar -- /bin/sh -c "mkdir -p /run/user/1000 /dev/shm; chmod 700 /run/user/1000; chmod 1777 /dev/shm; export XDG_RUNTIME_DIR=/run/user/1000; export XKB_CONFIG_ROOT=/usr/share/X11/xkb; export WLR_RENDERER=pixman; seatd -l debug & for i in 1 2 3 4 5 6 7 8 9 10; do [ -S /run/seatd.sock ] && break; sleep 1; done; labwc -s \"xfsettingsd & xfce4-panel & xfdesktop &\""
```
with `LITEBOX_LOG=debug` (add `LITEBOX_DIAG_FATALDUMP=1` for crash register/instruction-byte
capture), `MSYS_NO_PATHCONV=1` in Git Bash. Rebuild
`cargo build --locked --release -p litebox_runner_linux_on_windows_userland` first. Regression
suite: `cargo test -p litebox_shim_linux --lib -- --skip test_mremap` (177/177 pass as of
sub-session 7) and `cargo test -p litebox_platform_windows_userland` (4/4 pass).

## Completion criterion (unchanged, NOT YET MET)

labwc's own `-s "xfsettingsd & xfce4-panel & xfdesktop &"` session targets launch (real
`sys_execve` log lines) and survive a 90-150+ second window with no `fatal signal:`/
`sys_exit_group` in a `LITEBOX_LOG=debug` capture — log-based evidence only, never
`busybox kill -0` (confirmed unreliable in this rootfs). As of sub-session 7, labwc itself now
survives past DRM backend creation AND past keyboard/keymap setup (previously the wall) but
still SIGABRTs during output/swapchain buffer allocation (see "Current blocker" above) before
ever `sys_execve`-ing `xfsettingsd`/`xfce4-panel`/`xfdesktop` — closer than any prior
sub-session, but the session targets have still never actually launched in a captured run.

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
