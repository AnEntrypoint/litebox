# AGENTS.md — handoff note (2026-08-30, sub-session 22)

## Active standing goal (session-scoped Stop hook on the originating machine)

> "go ahead and push all the way till xfce starts flawlessly, fix any bug that arises first"

This is being worked via `/goal` on litebox-main, gm session_id `litebox-xfce-1`. Full
chronological detail of every prior sub-session's investigation, ruled-out hypotheses, and
fixes lives in gm's memory store (`recall`/`codesearch` against this project) as resolved
mutables — do not re-derive from scratch; query the recall store first (e.g. search
"fork_verify AV path stale pointer", "DRM PRIME handle", "wlroots shm keymap", "step bound
exhaustion", "keep relocations alive false positive", "dbus-daemon nofork").

## MAJOR FINDING (sub-session 21): `dbus-daemon --nofork` avoids the fork-verify crash entirely

The step-bound-exhaustion crash blocking D-Bus (see the extensive investigation trail below,
sub-sessions 13/19/20) is a real, still-unfixed litebox gap in `fork_verify`. But it is
**entirely avoidable at the repro-command level**: `dbus-daemon`'s crash only happens in its
own daemonizing self-fork (the `--fork` path, its default without an explicit flag). Running
`dbus-daemon --nofork` (stay in the foreground, no self-fork at all) sidesteps the whole bug
class — confirmed live, a full run with `--nofork` shows **zero** `fatal signal` lines from
`dbus-daemon` or its descendants, and **`xfsettingsd` genuinely connects to D-Bus for the first
time this entire investigation** (no more "Could not connect: Connection refused" for
`xfsettingsd` itself), spawning real children (`at-spi-bus-launcher`, `xfconfd`).

**Use `--nofork` in the repro command going forward** (see updated repro command below). The
underlying `fork_verify` step-bound bug remains open and real (a genuine litebox limitation
that will resurface for any OTHER guest program that daemonizes via a long-running
post-`fork()` self-fork) but is no longer the immediate blocker for THIS goal.

## Blocker found with `--nofork` (sub-session 21) — ROOT-CAUSED sub-session 22, genuine upstream wlroots gap, NOT litebox

Execution with `--nofork` progresses much further and hits:
```
Assertion failed: width > 0 && height > 0 (render/swapchain.c: wlr_swapchain_create: 21)
```
(`fatal signal: ... signal=Signal(6)` — SIGABRT) on labwc itself (`tid=1000`), BEFORE either
`xfce4-panel` or `xfdesktop` ever `sys_execve`.

**Sub-session 22 root-caused this precisely, using `labwc -d` for full wlroots debug logging (the
`-d`/`--debug` flag, not a `WLR_*_LOG_LEVEL` env var — `labwc --help` in-guest confirms the
correct flag).** Sequence, quoted from a live `LITEBOX_LOG=debug labwc -d` capture:

1. ~21.26s: FIRST modeset for output `Virtual-1` succeeds completely via real DRM ioctls
   (`DrmModeCreateDumb`/`DrmModeMapDumb`/`DrmPrimeHandleToFd`/`DrmModeAddFb2`). wlroots logs
   `[types/output/swapchain.c:96] Testing swapchain for output 'Virtual-1'` →
   `[render/swapchain.c:103] Allocating new swapchain buffer` →
   `[render/allocator/drm_dumb.c:105] Allocated 1920x1080 DRM dumb buffer` — all succeed.
2. ~25.64s (right after `xfsettingsd` connects to D-Bus and its built-in display-management code
   issues a `wlr-output-management` config-apply request): labwc runs `output_test_auto` a SECOND
   time for `Virtual-1`. Logs: `[../src/output.c:421] testing modes for Virtual-1` →
   `[../src/output.c:437] testing requested mode 1920x1080@60000` (the requested mode itself is
   NOT zero) → `[types/output/render.c:123] Attaching empty buffer to output for modeset` →
   `[types/output/swapchain.c:27] Choosing primary buffer format XR24 for output 'Virtual-1'` →
   immediately `Assertion failed: width > 0 && height > 0` — critically, `Testing swapchain for
   output` (the log line from the successful first pass) never appears this second time.
3. **Zero DRM ioctls of any kind occur on tid=1000 in the entire ~4.4s window between the first
   successful modeset (last DRM ioctl at 21.2647s) and the crash (25.6438s)** — confirmed via full
   grep of the debug log. This proves litebox's DRM emulation cannot be the cause: there is no
   ioctl call in this window for litebox to answer incorrectly. The crash is wlroots reprocessing
   a second output-management commit purely from its own in-memory state.

Cross-referenced against wlroots' real upstream source (`github.com/swaywm/wlroots`, fetched
live this session): `output_pending_resolution()` (`types/output/output.c`) falls back to
`output->width`/`output->height` (persistent fields, distinct from the per-commit
`pending.mode`) whenever `WLR_OUTPUT_STATE_MODE` is not set on the CURRENT commit's state.
wlroots' **legacy (non-atomic) DRM backend**'s connector-test function, `legacy_crtc_test()`
(`backend/drm/legacy.c`), runs **purely on cached state with zero ioctls** (confirmed via live
fetch of its actual source) and is documented by its own comment as only reliably validating a
buffer commit against a PRIOR `queued_fb`/`current_fb` it already has cached — a second
output-management-triggered commit arriving without a fresh mode-probe is exactly the gap this
cached-only test function is weak against.

litebox's DRM device **deliberately and correctly** implements only the legacy `SETCRTC`/
`PAGE_FLIP` API — `litebox_shim_linux/src/syscalls/drm.rs:536` (`set_client_cap`) explicitly
rejects `DRM_CLIENT_CAP_ATOMIC` with `EINVAL` ("claiming atomic support here would be a lie a
client could act on"), matching real minimal/software DRM hardware. This correctly and
necessarily forces wlroots onto the legacy backend path system-wide. **There is no litebox-side
fix available that doesn't mean fabricating fake atomic-modesetting support litebox's design
explicitly and correctly refuses to lie about.**

**Conclusion: this is a genuine upstream wlroots legacy-DRM-backend limitation (weak state
caching in `legacy_crtc_test`/`output_ensure_buffer`'s empty-buffer fallback across a second
output-management commit), NOT a litebox emulation gap** — the first time in this whole
investigation a blocker is confirmed NOT litebox's own, breaking the pattern of fixes 1-10 below
(all of which were genuinely litebox's own gaps).

**Two workaround avenues investigated, both currently blocked by hard project constraints:**
- (a) Suppress `xfsettingsd`'s display-management code so it never issues the triggering
  output-management config-apply request: NOT POSSIBLE without recompiling/patching
  `xfsettingsd` — its display-management logic is compiled directly into the single
  `xfsettingsd` binary (confirmed via `xfsettingsd --help`, which offers no plugin-disable flag,
  and via filesystem search — no separate loadable plugin file for it exists to omit). The
  project's hard constraint ("never recompile, binary-patch, or otherwise modify any guest
  package/binary") rules this out.
- (b) Configure labwc itself to reject/ignore incoming `wlr-output-management` client requests:
  NOT POSSIBLE — labwc's full documented `rc.xml` schema (fetched live, `docs/rc.xml.all`) has no
  `<outputs>` section or any option controlling wlr-output-management protocol exposure.

No safe, non-speculative fix is available this session on either the litebox side or the
guest-config side. Full evidentiary trail recorded as gm mutable
`labwc-swapchain-zero-crash-is-genuine-upstream-wlroots-legacy-drm-gap-not-litebox` (session
`litebox-xfce-1-sub22`). **Standing goal is NOT complete.** Genuine next options for a future
session: patch wlroots itself (outside litebox's own source tree — a different kind of change
than every prior fix in this investigation, needs explicit user sign-off since it means carrying
a local wlroots patch/fork rather than using the guest's unmodified official package); or find a
config path inside XFCE's `xfconfd`/`xsettings.xml` that pre-seeds a saved display profile so
`xfsettingsd` never needs to issue a runtime config-apply request in the first place (untested,
worth trying first — is guest-config-only, no binary changes).

## Prior fixed-and-pushed chain (verified live, in order)

Every one of this chain that initially looked like it might be an "upstream" bug turned out to
be litebox's own gap — keep defaulting to that hypothesis for anything new:
1. mallocng `.meta=0` crash — commit `b4a40e3d`.
2. libinput evdev rejection, missing `fallocate`, `migrate_file_up` panic — commit `5458d74c`.
3. Full DRM sysfs subtree, `DRM_CAP_*`, `DRM_IOCTL_GET_MAGIC`/`AUTH_MAGIC` — commits `1f51bf4a`,
   `024d704f`. labwc's wlroots DRM backend creates successfully.
4. `fchmod`-on-unlinked-fd + `mmap(MAP_SHARED)` on unlink-based shm files — commit `61c97e9f`.
5. `DRM_IOCTL_PRIME_HANDLE_TO_FD`/`FD_TO_HANDLE`/`GEM_CLOSE` — commit `17312da4`.
6-9. Four fork_verify AV-bypass/register-healing extensions (`rcx`, `rdi`, AV-path CODE `rip`,
   AV-path DATA memory-operand registers) — commits `8ec32c4b`, `c3182da7`, `4bf0acac`,
   `a9895bec`.
10. fork_verify: chain ancestor relocations across NESTED fork generations — commit `ca7408e0`.

**Unfixed, real, open litebox limitation (do not re-attempt blindly)**: `fork_verify`'s
`MAX_THREAD_VERIFICATION_STEPS` bound (16384) disarms verification (and clears the relocation
map) for a long-running post-fork thread, and a stale pointer reaching an unverified path after
that point can crash the guest task. THREE independent attempts to extend coverage past the
bound (raise it 2x, raise it 16x, keep the relocation map alive passively without re-arming
`TF`) have all failed — the first two caused a DIFFERENT worse host-level crash; the third was
caught in the act via direct diagnostic instrumentation producing a FALSE-POSITIVE `is_in_source`
hit (a coincidental address-range overlap that only becomes possible once the guest's own
legitimate memory layout has evolved far past the map's original narrow validity window) that
"healed" to a wrong address and crashed anyway. **Do not attempt "keep the map alive" again in
any form** — the map's precision is fundamentally time-bounded. A grace window shorter than
16384 (tens of steps, matching the doc-commented expected real staleness window) is the one
remaining untested design point, but is now moot for THIS specific blocker since `--nofork`
avoids it entirely; it would still be worth fixing properly for other programs that hit it.

## Repro command (current known-good, sub-session 22: `--nofork` + `labwc -d`)

Add `-d` to the `labwc` invocation (not a `WLR_*_LOG_LEVEL` env var — confirmed via `labwc
--help` in-guest) to get full wlroots-internal debug logging (`[file.c:line] message` lines
interleaved with litebox's own `LITEBOX_LOG=debug` output), essential for diagnosing
compositor-internal crashes like the swapchain assertion above.

```
target/release/litebox_runner_linux_on_windows_userland.exe --initial-files .wfgy/xfce-build/alpine-pinned2.tar --resume-from .wfgy/xfce-build/xfce-layer17.tar -- /bin/sh -c "mkdir -p /run/user/1000 /dev/shm /var/lib/dbus; chmod 700 /run/user/1000; chmod 1777 /dev/shm; export XDG_RUNTIME_DIR=/run/user/1000; export XKB_CONFIG_ROOT=/usr/share/X11/xkb; export WLR_RENDERER=pixman; dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>&1 || true; export DBUS_SESSION_BUS_ADDRESS='unix:path=/tmp/mybus'; dbus-daemon --nofork --nopidfile --nosyslog --address=\"\$DBUS_SESSION_BUS_ADDRESS\" --session & sleep 2; seatd -l debug & for i in 1 2 3 4 5 6 7 8 9 10; do [ -S /run/seatd.sock ] && break; sleep 1; done; labwc -d -s \"xfsettingsd & xfce4-panel & xfdesktop &\""
```
with `LITEBOX_LOG=debug` (add `LITEBOX_DIAG_FATALDUMP=1 LITEBOX_VEH_TRACE=1` for crash register
capture), `MSYS_NO_PATHCONV=1` in Git Bash. Rebuild
`cargo build --locked --release -p litebox_runner_linux_on_windows_userland` first. Strip ANSI
color codes before grepping (`sed 's/\x1b\[[0-9;]*m//g' logfile > clean.log`). Regression
suite: `cargo test -p litebox_shim_linux --lib -- --skip test_mremap` (177/177) and
`cargo test -p litebox_platform_windows_userland` (4/4).

## Completion criterion (unchanged, NOT YET MET)

labwc's own `-s "xfsettingsd & xfce4-panel & xfdesktop &"` session targets launch (real
`sys_execve` log lines) and survive a 90-150+ second window with no `fatal signal:`/
`sys_exit_group` (Signal) in a `LITEBOX_LOG=debug` capture — log-based evidence only, never
`busybox kill -0` (confirmed unreliable in this rootfs). As of sub-session 22, execution reaches
real D-Bus service activation (further than ever) but labwc itself aborts on a swapchain
assertion before `xfce4-panel`/`xfdesktop` ever launch — root-caused (see above) as a genuine
upstream wlroots legacy-DRM-backend gap triggered by `xfsettingsd`'s runtime
wlr-output-management config-apply request, not a litebox emulation gap; no safe fix found this
session on either side of the boundary.

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
- **fork_verify's `MAX_*_VERIFICATION_STEPS` bound is load-bearing.** See "Unfixed, real, open
  litebox limitation" above — three independent extension attempts all failed for related but
  distinct reasons. Do not attempt a fourth without new diagnostic evidence.

## Rootfs/artifact locations

- `.wfgy/xfce-build/xfce-layer17.tar` — current furthest-progressed rootfs (layer16 + mesa DRI).
  Use as `--resume-from`.
- `.wfgy/xfce-build/alpine-pinned2.tar` — base tar, paired with a layer-N overlay via `--resume-from`.
- Large scratch artifacts (`target-myfork/`, `alpine-fresh-test.tar`, `.agentplug/`) are local
  build/test byproducts, gitignored, safe to ignore or regenerate.
