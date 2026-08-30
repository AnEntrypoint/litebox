# AGENTS.md — handoff note (2026-08-30, sub-session 20)

## Active standing goal (session-scoped Stop hook on the originating machine)

> "go ahead and push all the way till xfce starts flawlessly, fix any bug that arises first"

This is being worked via `/goal` on litebox-main, gm session_id `litebox-xfce-1`. Full
chronological detail of every prior sub-session's investigation, ruled-out hypotheses, and
fixes lives in gm's memory store (`recall`/`codesearch` against this project) as resolved
mutables — do not re-derive from scratch; query the recall store first (e.g. search
"fork_verify AV path stale pointer", "DRM PRIME handle", "wlroots shm keymap",
"step bound exhaustion", "keep relocations alive").

## Current state (as of sub-session 20)

**Fixed and pushed, verified live** (every one of this chain that initially looked like it
might be an "upstream" bug turned out to be litebox's own gap — keep defaulting to that
hypothesis for anything new):
1. mallocng `.meta=0` crash — commit `b4a40e3d`.
2. libinput evdev rejection, missing `fallocate`, `migrate_file_up` panic — commit `5458d74c`.
3. Full DRM sysfs subtree, `DRM_CAP_*`, `DRM_IOCTL_GET_MAGIC`/`AUTH_MAGIC` — commits `1f51bf4a`,
   `024d704f`. labwc's wlroots DRM backend creates successfully.
4. `fchmod`-on-unlinked-fd + `mmap(MAP_SHARED)` on unlink-based shm files — commit `61c97e9f`.
5. `DRM_IOCTL_PRIME_HANDLE_TO_FD`/`FD_TO_HANDLE`/`GEM_CLOSE` — commit `17312da4`. Got
   `xfsettingsd` to genuinely `sys_execve` for the first time.
6-9. Four fork_verify AV-bypass/register-healing extensions (`rcx`, `rdi`, AV-path CODE `rip`,
   AV-path DATA memory-operand registers) — commits `8ec32c4b`, `c3182da7`, `4bf0acac`,
   `a9895bec`.
10. fork_verify: chain ancestor relocations across NESTED fork generations (a real architecture
    gap — a grandchild fork's map only covered its immediate parent, not the grandparent) —
    commit `ca7408e0`. This got `xfsettingsd` to genuinely reach its D-Bus `connect()` attempt,
    the furthest this whole investigation has ever gotten.

**Current blocker**: `xfsettingsd` still fails ("Could not connect: Connection refused")
because `dbus-daemon`'s real long-running daemon (a SINGLE-generation fork child of the
`--fork` parent, confirmed via `clone: spawned new task parent_tid=<dbus-daemon>`) crashes
before it ever calls `bind()`/`listen()` on its Unix socket — confirmed via `ls -la` on the
socket path showing a plain empty regular file (`-rwxrwxrwx ... 0 ...`), never an actual
listening socket.

**Root cause, precisely confirmed (sub-session 19)**: this is `MAX_THREAD_VERIFICATION_STEPS`
(16384, `fork_verify.rs`) tripping — the step bound that stops continuous single-stepping on a
long-running post-fork thread. Live `LITEBOX_VEH_TRACE=1` capture showed the exact sequence:
stale-pointer healing WARN lines firing right up until `"step bound 16384 exceeded ... ending
verification early"`, then the SAME thread taking an unverified, unhealed raw
`EXCEPTION_ACCESS_VIOLATION` moments later — disproving the bound's own doc-comment claim that
post-fork staleness is front-loaded (it can still occur well past 16384 steps on a real,
long-running daemon).

**THREE independent fix attempts for extending coverage past the bound have now failed,
across two sessions — do not attempt a fourth mechanical variant without first understanding
WHY extending coverage specifically breaks things:**
1. Raise `MAX_THREAD_VERIFICATION_STEPS` 2x (32768) — avoided this crash, caused a DIFFERENT,
   worse host-level segfault (exit 139, `rip=0x2`, `.meta=0`-shaped null-deref).
2. Raise it 16x (262144) — same worse host-level segfault, identical signature.
3. (Sub-session 20, this session) Keep `tls.fork_verify`'s `Arc<AddressRelocations>` alive past
   the bound (so the already-landed, already-proven-safe AV-path reactive healing could still
   fire), while adding a SEPARATE flag to stop `entry_eflags_tf` from re-arming `TF` — i.e. zero
   additional single-stepping cost, purely a passive safety net. This ALSO caused a host-level
   segfault (exit 139) on the full repro. Reverted cleanly (`git status` clean, baseline tests
   177/4 pass).

**Leading hypothesis for #3's failure, NOT yet verified**: keeping the relocation map's `Arc`
alive indefinitely past the bound may let the AV-path healing incorrectly fire on a LATER,
UNRELATED fault whose address coincidentally satisfies `is_in_source` against a now-very-stale
map — the tracked source ranges were captured at ONE `fork()` moment; by the time a daemon has
run thousands of steps past the bound, the guest's OWN legitimate memory layout may have grown
enough (new mmaps, stack growth, etc.) that a coincidental range overlap becomes newly possible
in a way it never was during case (1)/(2)'s originally-designed narrow window. **Needs
verification via minimal diagnostic instrumentation (log every AV-path heal attempt's address
and whether it was a genuine hit) BEFORE any further fix attempt, not another mechanical
variant.**

**Also landed, safe and independently useful**: the corrected D-Bus repro command
(`dbus-launch --exit-with-session`, not `--print-address`) and `mesa-dri-gallium` installed
into `.wfgy/xfce-build/xfce-layer17.tar`.

## Repro command (current known-good)

```
target/release/litebox_runner_linux_on_windows_userland.exe --initial-files .wfgy/xfce-build/alpine-pinned2.tar --resume-from .wfgy/xfce-build/xfce-layer17.tar -- /bin/sh -c "mkdir -p /run/user/1000 /dev/shm /var/lib/dbus; chmod 700 /run/user/1000; chmod 1777 /dev/shm; export XDG_RUNTIME_DIR=/run/user/1000; export XKB_CONFIG_ROOT=/usr/share/X11/xkb; export WLR_RENDERER=pixman; dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>&1 || true; eval \$(dbus-launch --sh-syntax --exit-with-session) 2>&1; export DBUS_SESSION_BUS_ADDRESS; seatd -l debug & for i in 1 2 3 4 5 6 7 8 9 10; do [ -S /run/seatd.sock ] && break; sleep 1; done; labwc -s \"xfsettingsd & xfce4-panel & xfdesktop &\""
```
with `LITEBOX_LOG=debug` (add `LITEBOX_DIAG_FATALDUMP=1 LITEBOX_VEH_TRACE=1` for crash register
capture and step-by-step trace), `MSYS_NO_PATHCONV=1` in Git Bash. Rebuild
`cargo build --locked --release -p litebox_runner_linux_on_windows_userland` first. Strip ANSI
color codes before grepping (`sed 's/\x1b\[[0-9;]*m//g' logfile > clean.log`). Regression
suite: `cargo test -p litebox_shim_linux --lib -- --skip test_mremap` (177/177) and
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
- **Push safety**: stage ONLY the specific files you changed (never `git add -A`/`.`).
- **fork_verify's `MAX_*_VERIFICATION_STEPS` bound is load-bearing in a way not yet fully
  understood.** THREE independent attempts to extend coverage past it (raise the bound 2x, 16x,
  or keep the relocation map passively alive without re-arming `TF`) have all caused a
  DIFFERENT, worse host-level crash than the one being fixed. Do not attempt a fourth mechanical
  variant without first adding diagnostic instrumentation to understand exactly why extending
  coverage breaks things (leading hypothesis: a stale relocation map's `is_in_source` producing
  false-positive hits against the guest's own legitimately-evolved later memory layout — see
  "Current blocker" above for detail). Every OTHER fix in this investigation (#6-10 above) was
  narrow, single-register/single-transition-point, and safe — this specific step-bound gap is
  the one exception that has resisted three honest attempts.

## Rootfs/artifact locations

- `.wfgy/xfce-build/xfce-layer17.tar` — current furthest-progressed rootfs (layer16 + mesa DRI).
  Use as `--resume-from`.
- `.wfgy/xfce-build/alpine-pinned2.tar` — base tar, paired with a layer-N overlay via `--resume-from`.
- Large scratch artifacts (`target-myfork/`, `alpine-fresh-test.tar`, `.agentplug/`) are local
  build/test byproducts, gitignored, safe to ignore or regenerate.
