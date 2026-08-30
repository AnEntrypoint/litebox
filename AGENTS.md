# AGENTS.md — handoff note (2026-08-30, sub-session 6)

## Active standing goal (session-scoped Stop hook on the originating machine)

> "go ahead and push all the way till xfce starts flawlessly, fix any bug that arises first"

This is being worked via `/goal` on litebox-main, gm session_id `litebox-xfce-1`. Full
chronological detail of every prior sub-session's investigation, ruled-out hypotheses, and
fixes lives in gm's memory store (`recall`/`codesearch` against this project) as resolved
mutables — do not re-derive from scratch; query the recall store first (e.g. search
"wlroots shm keymap crash", "DRM backend creation", "mallocng syscall_callback",
"libinput evdev unhandled device").

## Current state (as of sub-session 6)

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

**Current blocker (NOT a litebox bug, confirmed)**: past DRM backend creation, wlroots
SIGSEGVs in its own keymap-shm-allocation code (`types/wlr_keyboard.c:222`). Exhaustive live
forensics (`LITEBOX_DIAG_FATALDUMP=1`) captured the actual crash: `rip` resolves inside
`libwlroots-0.20.so`'s own text section, crashing instruction is `mov rax,[rdi+0x80]` with
`rdi=0x0` — a genuine NULL-pointer struct dereference in real wlroots code, not litebox
emulation (litebox's `open`/`unlink` syscall sequence for this exact path was independently
re-verified correct against real wlroots `util/shm.c` semantics). Deterministic and
environment-independent: unaffected by `WLR_RENDERER=pixman` vs `WLR_RENDERER_ALLOW_SOFTWARE=1`,
`HOME`, or `XKB_DEFAULT_*` env vars (all tried live this session). All layer tars
(`xfce-layer13` through `xfce-layer16`) ship the same `usr/bin/labwc`/wlroots build.

**Next step for whoever picks this up**: this looks like a genuine upstream wlroots 0.20.x
defensive-programming gap, hit specifically because litebox's virtual GPU forces the
software-rendering + no-DRM-render-node code path most real hardware never exercises (a
DIFFERENT shm allocation earlier in the same run, for `wlr_linux_dmabuf_v1.c`'s format table,
fails identically but is tolerated gracefully — proving wlroots' own NULL-checking is
inconsistent across call sites). Litebox project rules forbid patching guest binaries. Real
options: (a) source a newer `libwlroots-0.20.so` point release with this NULL-check fixed, if
one exists, and get it into the rootfs via an official Alpine package upgrade (never a manual
binary patch); (b) live single-step/register-dump deeper into the exact wlroots call graph
between the second `open()` succeeding and the crash to find precisely which internal call
returns NULL unchecked (candidates: `xkb_keymap_get_as_string()` returning NULL, or the size
computation itself); (c) check whether a newer Alpine `wlroots`/`labwc` package version is
available that isn't yet in this rootfs.

## Repro command (unchanged shape, current known-good)

```
target/release/litebox_runner_linux_on_windows_userland.exe --initial-files .wfgy/xfce-build/alpine-pinned2.tar --resume-from .wfgy/xfce-build/xfce-layer16.tar -- /bin/sh -c "mkdir -p /run/user/1000 /dev/shm; chmod 700 /run/user/1000; chmod 1777 /dev/shm; export XDG_RUNTIME_DIR=/run/user/1000; export XKB_CONFIG_ROOT=/usr/share/X11/xkb; export WLR_RENDERER=pixman; seatd -l debug & for i in 1 2 3 4 5 6 7 8 9 10; do [ -S /run/seatd.sock ] && break; sleep 1; done; labwc -s \"xfsettingsd & xfce4-panel & xfdesktop &\""
```
with `LITEBOX_LOG=debug` (add `LITEBOX_DIAG_FATALDUMP=1` for crash register/instruction-byte
capture), `MSYS_NO_PATHCONV=1` in Git Bash. Rebuild
`cargo build --locked --release -p litebox_runner_linux_on_windows_userland` first. Regression
suite: `cargo test -p litebox_shim_linux --lib -- --skip test_mremap` (177/177 pass as of
sub-session 6) and `cargo test -p litebox_platform_windows_userland`.

## Completion criterion (unchanged)

labwc's own `-s "xfsettingsd & xfce4-panel & xfdesktop &"` session targets launch (real
`sys_execve` log lines) and survive a 90-150+ second window with no `fatal signal:`/
`sys_exit_group` in a `LITEBOX_LOG=debug` capture — log-based evidence only, never
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
- **Push safety**: stage ONLY the specific files you changed (never `git add -A`/`.`) — a prior
  sub-session's push hung badly after accidentally staging a 254MB scratch tar and a full
  `target-myfork/` build-cache tree. `.gitignore` already covers `target-myfork/`,
  `alpine-fresh-test.tar`, `.agentplug/`, `.wfgyxfce-*.ps1`.

## Rootfs/artifact locations

- `.wfgy/xfce-build/xfce-layer16.tar` — current furthest-progressed rootfs, has real `usr/bin/labwc`.
- `.wfgy/xfce-build/alpine-pinned2.tar` — base tar, always paired with a layer-N overlay via `--resume-from`.
- Large scratch artifacts in the working tree (`target-myfork/`, `alpine-fresh-test.tar`,
  `.agentplug/`) are local build/test byproducts, gitignored, safe to ignore or regenerate.
