# STATUS (2026-08-31, sub-session 30): INDEPENDENT VERIFICATION of a claimed `VirtualFree(DECOMMIT)`/xwayland.so-dlopen host-crash fix -- fix's own narrow claim (no panic) holds, but its headline "9 repaints, weston survives past the old crash point" evidence does NOT reproduce; exact-repro re-run this session shows repaint count 0 (not >1), DRM ioctl count back down to the pre-crash-era baseline of 2 (1 SetCrtc + 1 PageFlip), and the screenshot is still solid black -- XFCE desktop content is NOT confirmed onscreen

A fix was submitted claiming to resolve the `litebox_platform_windows_userland/src/lib.rs`
`allocate_pages` Replace-mode `VirtualFree(MEM_DECOMMIT)` panic (`ERROR_INVALID_PARAMETER`) that
was crashing the host process while weston's musl `ld.so` `dlopen()`'d `xwayland.so`, via a new
`decommit_bisecting` helper that retries in page-granularity halves on that specific error. The
fix is present as an uncommitted working-tree change to `lib.rs` (57 lines, +50/-7), confirmed via
`git diff --stat`. This session re-ran the exact repro this project's AGENTS.md documents as
current-working (`.wfgy/xfce-build/xfce_launch.sh`, invoked as `sh /xfce_launch.sh` against
`xfce-layer19.tar` -- the sub-session-29 fix for the PowerShell argv-quoting crash and the
`bind()`-never-creates-`S_IFSOCK` gap, both already committed at `c56e405a`), rebuilt release,
with `LITEBOX_LOG=debug` and `--gui`, full log at `.wfgy/xfce-build/verify_run1.log` (144,334 lines).

**Verified TRUE (fix's narrow claim holds):**
- `grep -c "panicked at"` -> 0. `grep -c "VirtualFree(DECOMMIT) failed"` -> 0. The host process does
  not crash loading `xwayland.so` this run (`sys_openat .../xwayland.so` at t=15.99s, no panic
  follows). This part of the fix-phase claim is genuine and reproducible.
- `xfsettingsd` (tid=26), `xfce4-panel` (tid=27), `xfdesktop` (tid=28) all `sys_execve` successfully
  at t=18.12/18.17/18.20s.
- `TRACE unix_connect`/`TRACE unix_accept` (sub-session-29's instrumentation, already committed):
  all 3 XFCE clients connect to `/run/user/1000/wayland-1` with `ok=true` at t=32.01s/32.26s/33.10s,
  matched by 3 corresponding `TRACE unix_accept: result ok=true` on weston's side (plus one earlier
  `ok=true` for `/run/seatd.sock` at t=15.51s) -- 4 total accepts, 4 total successful connects. This
  item from the task's verification checklist is confirmed still holding.
- `WESTON_ALIVE=1` at the scripted 60s liveness check; run reaches `DONE_SLEEPING` cleanly.
- `sys_write` count from any tid, anywhere in the run: 0. Matches the fix-phase report's own stated
  "next bug" -- XFCE clients still write zero Wayland protocol bytes after connecting.

**Verified FALSE (the fix's headline repaint-progress claim does not reproduce):**
- `grep -c "\[repaint\] Beginning repaint"` -> **0**, not the claimed 9. In fact the string
  `repaint` (any case) appears **zero times anywhere in the entire 144K-line log**, despite
  `--logger-scopes=log,drm-backend,compositor-backend,wayland-protocol,xwayland` being passed
  (the same flag set the fix-phase claim says it used). Either weston's actual repaint-loop log
  line differs from what was grepped for, or the repaint loop never runs multiple times this rerun
  -- but the specific evidence cited (9 occurrences) is not reproducible as stated.
- `DrmModeSetCrtc`/`DrmModePageFlip` ioctl count: exactly **2** total (1 SetCrtc + 1 PageFlip, both
  at t=21.13s) -- this is the SAME count every prior sub-session back to sub-session 26 has measured
  for "weston paints its own empty-desktop startup frame once and never repaints again," not an
  improved count. No DRM ioctl activity occurs after XFCE's clients connect at t=32-33s.
- Real screenshot taken this session (PowerShell `EnumWindows`+exact-title-match+`GetClientRect`+
  `ClientToScreen`+`CopyFromScreen`, the documented working technique; a `PrintWindow`-based capture
  was also tried as a cross-check but produced an unreliable half-black/half-white GDI artifact
  typical of GPU-composited swapchain windows, and was discarded in favor of the `CopyFromScreen`
  result). Pixel-sampled at 20px intervals across the window's own client-area bounds
  (`GetWindowRect` confirmed L=554,T=12,R=1523,B=575; client capture origin (561,42) w=954 h=525 is
  entirely inside those bounds): **728/756 sampled pixels are exactly RGB(0,0,0), the rest
  (32,32,32)** -- uniform solid black. No XFCE panel, taskbar, desktop icons, wallpaper, or any
  window content is visible. Screenshots: `.wfgy/xfce-build/verify_screenshot2.png` (CopyFromScreen,
  trustworthy), `.wfgy/xfce-build/verify_printwindow.png` (PrintWindow, discarded/unreliable).

**Verdict: the fix made a real, narrow, reproducible improvement (host no longer panics on
`xwayland.so` dlopen) but did NOT make the progress its own report claimed (no repaint-count
increase, no DRM ioctl-count increase, screenshot still solid black, same as every prior
sub-session back to #26).** The standing goal -- XFCE rendering normal desktop content on screen --
is NOT met. Two independent gaps remain open and unresolved: (1) weston's repaint scheduler still
never fires a second frame even once all three XFCE clients are alive and Wayland-connected (this
session found NO log evidence at all of weston's repaint-loop activity, worth re-checking whether
`--logger-scopes` is actually taking effect, since its total absence rather than a stuck-at-1 count
is itself a new, narrower observation this session adds); (2) `sys_write` from any XFCE client tid
is still 0 -- no Wayland protocol bytes ever flow over the successfully-connected+accepted sockets
in either direction, so weston has nothing to composite regardless of (1). Whoever continues:
first re-check weston's actual `--logger-scopes` output format/line text against the litebox debug
log (the total absence of any "repaint" string is a new, sharper anomaly than "stuck at 1" and may
point at logger-scope wiring rather than the compositor's own scheduler); then resume the
already-identified next step of tracing why GTK's Wayland client library never writes after
`connect()` (SO_PEERCRED/getsockopt correctness on connected AF_UNIX sockets, and confirming
`GDK_BACKEND`/`WAYLAND_DISPLAY` actually reach the child via `/proc/<pid>/environ` at the point of
`execve`, not just the parent shell's own `env` output before forking).

Repro used this session: `.wfgy/xfce-build/xfce_launch.sh` against `xfce-layer19.tar`, release
binary rebuilt at 07:18 (already included the uncommitted `lib.rs` fix, `cargo build --locked
--release -p litebox_runner_linux_on_windows_userland` reported "Finished" with no recompile
needed). Full log: `.wfgy/xfce-build/verify_run1.log`.

---


**Two real, load-bearing bugs found and fixed this sub-session, both with direct before/after evidence (not log-absence inference):**

1. **The PowerShell/Windows argv-quoting corruption bug that silently killed every prior sub-session's XFCE launch.** Every repro script in this project passed its multi-line guest shell script as one embedded-double-quote-containing string via `sh -c "<script>"` through a PowerShell array element to the runner's argv. Windows command-line reconstruction (`std::env::args()`/`GetCommandLineW`) mishandles the embedded `"` characters PowerShell 5.1 does not re-escape correctly when building a native child process's command line, corrupting the string in transit. Direct evidence: `LITEBOX_LOG=debug` showed the outer guest shell (tid=1000) successfully sequencing every command up to and including forking weston (`clone: spawned new task parent_tid=1000 child_tid=20`) -- immediately followed by `/bin/sh: syntax error: unterminated quoted string` and `sys_exit_group(status=Exit(2))`, terminating the ENTIRE launch script before the `WAYLAND_DISPLAY` discovery loop or any XFCE client ever ran. This silently explained every previous sub-session's "zero Wayland connect traffic" and "Xwayland never spawns" findings: nothing downstream of weston's fork ever executed, at all -- not a Wayland/DRM/Xwayland-layer bug, a Windows-side argv-marshaling one. **Fix**: write the guest script to a real file inside the rootfs tar (`xfce_launch.sh`, appended as a tar member onto `xfce-layer18.tar` via Python's `tarfile.open(..., 'a')` -- no extraction needed, since Windows `tar` cannot recreate this rootfs's many symlinks) and invoke it as `sh /xfce_launch.sh`, a two-element argv with no embedded quotes anywhere to corrupt. Confirmed live: zero syntax errors, all three XFCE components (`xfsettingsd`/`xfce4-panel`/`xfdesktop`) genuinely `sys_execve` at t≈13.3-17.9s across two independent reruns.

2. **`litebox_shim_linux/src/syscalls/unix.rs`'s pre-existing, previously-documented `bind()`-never-creates-`S_IFSOCK` gap (see the "Separable real bug" section below) was the actual reason XFCE's clients could never find weston's socket, even with fix #1 applied.** With fix #1 alone, the repro script's `[ -S /run/user/1000/wayland-N ]` discovery loop still failed all 10 polling iterations (confirmed via `sys_stat` entries for `wayland-0`/`wayland-1`/`wayland-2` at every poll, `-S` always false even though weston really did bind `wayland-1.lock` at t=12.4s), so `WDISP` fell back to the hardcoded `wayland-0` default -- a socket weston never created. Direct evidence: added real `TRACE unix_connect`/`TRACE unix_accept` debug-log instrumentation to `litebox_shim_linux/src/syscalls/unix.rs`'s stream-socket `connect()` (~line 861) and `accept()` (~line 884) -- previously **zero** logging existed anywhere in the unix-socket or `net.rs` connect/accept/bind path, meaning every earlier sub-session's "zero connect traffic" claims from grepping the log were unfalsifiable, not real negative evidence (this gap is now closed for future sessions too). With this instrumentation, the WDISP-resolution-with-`-S` run showed all three XFCE clients' `connect()` calls to `/run/user/1000/wayland-0` failing with `ECONNREFUSED` -- direct, unambiguous proof. **Fix**: changed the repro script's discovery-loop test from `[ -S $f ]` to `[ -e $f ]` (the documented workaround for the untyped-bind-socket gap). Confirmed live: `RESOLVED_WAYLAND_DISPLAY=wayland-1` (correctly matching weston's real bind), and all three XFCE clients' `TRACE unix_connect`/`TRACE unix_accept` pairs now show `ok=true` -- genuine, successful Wayland socket connections, a first for this entire multi-session investigation.

**New, more precise finding, not yet fixed**: even with both real connect-path bugs fixed, `weston`'s own `[repaint] Beginning repaint` count is STILL exactly 1 (never more), and **zero `sys_write` syscalls occur from any of the three XFCE client tids (25/26/27) at any point in the entire run** -- not even the very first Wayland protocol message (`wl_display.get_registry`) that any working Wayland client must send immediately after `connect()` succeeds. So the socket-level connection is now genuinely real and accepted (weston's `TRACE unix_accept: result ok=true` × 3, confirmed), but no protocol traffic ever flows over it in either direction. Also observed: `xfce4-panel` and `xfdesktop` both log a GTK `cannot open display:` warning (an X11-path error) around the same timestamp as their successful Wayland `connect()` -- suggesting GTK's Wayland backend either fails silently very early (before writing anything) and falls back toward an X11 path that also fails (Xwayland is never actually forked by weston despite `xserver listening on display :0` being logged -- weston's real lazy-Xwayland-launch semantics: it defers the actual `fork()`/`exec()` of the `Xwayland` binary until a client attempts a genuine X11 connection, and this trace shows nothing ever does). **Next step for whoever continues**: determine why GTK's Wayland client library writes zero bytes after a successful `connect()` -- candidates worth checking first: (a) whether litebox's `SO_PEERCRED`/`getsockopt` support on connected Unix-domain sockets is correct (many Wayland client libraries validate the peer credential immediately after connecting, silently aborting if it looks wrong), (b) whether `xfsettingsd`/`xfce4-panel`/`xfdesktop`'s actual runtime `GDK_BACKEND` value took effect (verify via `/proc/<pid>/environ` read or similar, since env-inheritance itself was never directly confirmed at the point of `execve`, only that the parent shell's own `env | grep` output showed the right values before forking).

**Instrumentation kept, not reverted**: the `TRACE unix_connect`/`TRACE unix_accept` debug logging added to `litebox_shim_linux/src/syscalls/unix.rs` is genuinely useful for any future connect/accept-path investigation (this file's connect/accept/bind functions had zero logging before this sub-session) and should stay landed.

---

*Everything below this line predates sub-session 29's fixes and is preserved for its historical evidence trail; its "Xwayland never spawns"/"zero Wayland connect traffic" conclusions are now known to have been artifacts of the PowerShell argv-corruption bug (fix #1 above), not real litebox/weston/Xwayland defects.*

---
---

# STATUS (2026-08-31, sub-session 29): XFCE content NOT confirmed onscreen -- task's premise (Xwayland never fork/execve's) did not hold under fresh repro; true blocker turned out to be a pre-existing, already-documented litebox argv/stack-pointer corruption bug killing the parent shell right after `fork()`, before any XFCE/X11 client code runs at all

This sub-session set out to fix "weston logs `xserver listening on display :0` then never
fork/execve's Xwayland," per the standing task description. Reproducing with `LITEBOX_LOG=debug`
and cleanly extracted, un-ANSI-mangled, tid-tracing logs showed that description was itself
stale/wrong: the actual failure is one layer earlier and unrelated to Xwayland.

**What actually happens, precisely traced:**
- Parent shell `tid=1000` forks weston (`clone: spawned new task parent_tid=1000 child_tid=20`) at
  t≈11.96s.
- On the very next syscall, the **parent shell itself** hits `/bin/sh: syntax error: unterminated
  quoted string` and calls `sys_exit_group(status=Exit(2))` -- the whole launch script dies right
  there.
- Every subsequent line of the repro script (the `WAYLAND_DISPLAY` discovery loop, `xfsettingsd`/
  `xfce4-panel`/`xfdesktop` launches) never runs -- not because Xwayland's lazy-spawn wasn't
  triggered, but because nothing downstream of weston's fork ever executes, XFCE's GTK/X11 clients
  included. No X11 client connection attempt ever happens, so the earlier "no Xwayland fork/exec"
  observation was a downstream symptom of this crash, not an Xwayland-layer cause.
- Reproducible on 4 independent runs regardless of exact shell text used afterward (confirmed by
  simplifying the socket-discovery loop to a trivial fixed-candidate `[ -S ]` check, and separately
  by inserting `sleep 1` before the failing point) -- ruling out this session's own script edits as
  the cause. The extracted embedded shell body passes `sh -n` cleanly outside litebox, ruling out
  an actual shell syntax bug in the repro script itself.

**Root cause: litebox's own pre-existing, extensively self-documented argv/stack-pointer
corruption bug**, in `litebox_shim_linux/src/syscalls/process.rs`'s `fixup_stale_stack_pointers`
(lines ~1188-1450+) and its Windows counterpart `litebox_platform_windows_userland/src/fork_verify.rs`.
That code's own doc comments describe multiple already-fixed rounds of exactly this corruption
class (a parent/child stack slot that numerically resembles a stale pointer gets misidentified and
"healed," corrupting live shell-arena/argv string data -- previously root-caused to a mallocng
heap-pointer misfire, "verified 40/40 clean" for short payload lengths 1-40). This session's repro
hits the same symptom class (`ash`'s `stalloc` arena corrupted right after `fork()`) but at a
scale/code path not fully bisected against those prior fixes -- most likely the corruption now
strikes the **parent** thread's continuation after a heavier fork (weston, not the earlier
lightweight `mkdir`/`chmod`/`seatd`/`dbus-daemon` forks that succeeded fine), a case the existing
scan window (bounded to the **child's** `rsp`, per the code's own comments) does not cover.

This is the same bug class already flagged as an open, cross-session blocker in project memory
(`npx casey goal status`: "fork()+pre-execve mallocng `.meta=0` null-deref crash, proven
litebox-specific"). It is materially different from, and deeper than, the Xwayland lazy-spawn
framing this sub-session started from, and was judged not safely fixable as a narrow in-session
change: the code's own history shows three prior narrowing attempts at this exact heuristic, each
requiring precise live-repro-driven bisection before any constant/guard change, and explicitly
warning against speculative edits without new pinned-down repro data. That bisection was not done
this session.

**What was actually changed:** `run_repro_fix_apply.ps1` -- replaced the hardcoded
`WAYLAND_DISPLAY=wayland-0` assumption/polling with dynamic discovery of whichever `wayland-N`
socket weston actually binds (confirmed real: no `wayland-0`/`wayland-1` baked into
`xfce-layer18.tar`). This fix is applied and correct but its effect could not be observed, because
the script now dies from the pre-existing corruption bug before ever reaching that code. No
`litebox_shim_linux`/`litebox_runner_linux_on_windows_userland` source changes were made this
sub-session -- no Xwayland-specific gap was found anywhere in litebox to fix; the real blocker sits
one layer earlier, in already-existing, not-yet-fully-resolved core litebox fork/exec code.

**Verification performed:** `cargo check -p litebox_shim_linux` clean; `cargo test -p
litebox_shim_linux --lib -- --skip test_mremap` -> 177/177 passed (baseline maintained, no
regression, since no shim code was touched this sub-session).

**Screenshot taken during a live run:** solid black content area (1523x825px client area) under
the "litebox virtual display" title bar -- matching weston's single `kiosk-shell-background` solid
color surface, no XFCE panel/taskbar/desktop content visible. **XFCE content is NOT confirmed
onscreen.**

**Flip count:** `DrmModeSetCrtc`/`DrmModePageFlip` occurred exactly **once** in every run (original
and all 4 re-runs) -- unchanged from prior sub-sessions' count of 2 total calls (1 SetCrtc + 1
PageFlip = the single startup repaint). No repeated repainting observed, because the parent shell
dies before any X11/XFCE client ever connects to trigger further compositor activity.

**Concrete next step:** this needs its own dedicated, bisection-heavy investigation session against
`fixup_stale_stack_pointers`/`fork_verify.rs`, using the same length-sweep/executable-range-filter
methodology already used to fix the prior 3 rounds of this bug class, scoped specifically to the
**parent** thread's post-fork continuation (not just the child's) -- an apparently-uncovered case.
This is new, real scope beyond an Xwayland-specific fix and should be tracked as its own item
rather than folded into further weston/DRM/Wayland-protocol work, none of which can be reached
until the parent shell survives past `fork()`.

---

# STATUS (2026-08-30, sub-session 28, updated): DRM epoll-readiness gap FOUND AND FIXED (real, landed), but re-verification shows it was NOT the actual blocker -- weston still repaints exactly once even with the fix in place; the true remaining gap is one level deeper, in the Wayland protocol traffic between weston and its clients (Xwayland/XFCE), not in DRM readiness signaling

**Real fix landed this sub-session** (kept, verified, not reverted): `DrmSubsystem::pending_flip_events`
had zero `epoll`/`poll` readiness wiring -- a real, structural gap, not a guess. Added
`DrmSubsystem::has_pending_flip_events()`, a `DriFd` marker mirroring the existing `EvdevFd`
pattern (tagged onto `/dev/dri/card0` at `open()` time in `syscalls::file`), and wired it into
`syscalls::epoll::EpollDescriptor::poll`'s `File` arm exactly the same way `EvdevFd`/
`EvdevSubsystem::has_pending` already works -- a genuinely idiomatic, in-tree-precedented fix, not
invented from nothing. `cargo test -p litebox_shim_linux --lib -- --skip test_mremap`: 177/177
(unchanged from baseline, confirmed both before and after this edit). This closes a real DRM-fd
readiness gap regardless of the finding below, and should stay landed.

**However, re-running the full XFCE repro with this fix in place shows NO CHANGE in weston's own
repaint behavior**: all three XFCE components again genuinely `sys_execve` (confirmed via debug
trace), zero fatal signals, weston stays alive -- but `DrmModeSetCrtc`/`DrmModePageFlip` ioctl
count is STILL exactly 2 (weston's own single startup modeset+flip), even well after all three
XFCE processes are alive and running. **The DRM-readiness hypothesis is refuted by this direct
re-test** -- fixing the readiness signal did not change weston's own decision about whether to
schedule a new frame, meaning the actual blocker is upstream of DRM entirely: weston never decides
new content needs painting in the first place, regardless of whether it would correctly observe a
flip-complete event if it looked.

**New, more precise finding, not yet fixed**: grepped the same run's full log for any Wayland
socket connect/traffic activity involving `wayland-0` -- found ZERO matches. Despite weston
successfully creating the Wayland listening socket, launching Xwayland as its own child, and all
three XFCE GTK/X11 applications staying alive and running real syscalls, **no evidence exists in
this session's logs that Xwayland (or anything else) ever actually establishes real Wayland
protocol traffic with weston as a client** -- which would fully explain zero further repaints:
weston has nothing to composite because nothing new is actually arriving over the Wayland
protocol, not because of any DRM-emulation gap. This narrows the investigation to a genuinely
different layer than every hypothesis tried so far this multi-session investigation (labwc-style
output-management stall, missing XKB data, missing `/tmp/.X11-unix`, DRM epoll readiness) --
whoever continues should trace whether Xwayland's own `wl_display_connect()`/registry-bind
sequence to weston's Wayland socket ever completes at all (a real AF_UNIX `connect()`/`sys_write`
trace on the `wayland-0` socket path specifically, not just its listening `bind()`), since this
session's evidence suggests it may not be reaching that point despite Xwayland itself staying
alive as a process.

---

*Everything below this line is sub-session 28's original (partially superseded) write-up,
preserved for the detailed evidence trail it still documents correctly (the readiness gap's
precise code-level root cause, the exact `EpollDescriptor`/`IOPollable` architecture read this
session) -- only its CONCLUSION (that fixing DRM readiness would resolve the black-window symptom)
is now known to be incomplete, per the update above.*

Sub-session 27's own Fix-phase agent iterated its way to a working set of launch-env fixes
(`XKB_CONFIG_ROOT=/usr/share/X11/xkb`, pre-creating `/tmp/.X11-unix`) but its OWN repro script had
regressed relative to this project's long-established working command -- it dropped the D-Bus
SESSION bus entirely (only `dbus-daemon --system` was started, no `--session`), which xfsettingsd/
xfce4-panel genuinely need. That is why its final screenshot was still black and its report
concluded "XFCE clients still fail to connect to Wayland" -- a real symptom, but from a broken
repro, not a persisting litebox/weston defect.

**Re-ran the ORIGINAL, long-proven-working repro command** (documented throughout this file,
`dbus-daemon --nofork --session` with an explicit `DBUS_SESSION_BUS_ADDRESS`) with sub-session 27's
two real fixes folded in (`XKB_CONFIG_ROOT`, pre-created `/tmp/.X11-unix`) at `LITEBOX_LOG=debug`:

- All three XFCE components genuinely `sys_execve` (`xfsettingsd` tid=21 t=17.71s, `xfce4-panel`
  tid=22 t=17.72s, `xfdesktop` tid=23 t=17.73s).
- Weston (`tid=1000`) is confirmed alive for the ENTIRE run -- grepped every `sys_exit_group` in a
  1,039,308-line capture; weston's own tid never appears among them.
- Zero `fatal signal` lines, zero `cannot open display` lines, anywhere in the full capture.
- **Grepped the entire run for `DrmModeSetCrtc`/`DrmModePageFlip` ioctls: exactly TWO calls total,
  both at t=14.47s -- one `SetCrtc` immediately followed by one `PageFlip` -- and NOTHING else for
  the rest of the run, including the ~3+ seconds AFTER all three XFCE clients had already
  `sys_execve`'d and presumably created real Wayland surfaces.** Weston composites its own initial
  empty-desktop frame exactly once, at startup, and never repaints again -- not because of a
  crash, not because of a config-apply-triggered stall, but because weston's own repaint scheduler
  genuinely never decides to schedule a second frame, regardless of live client windows existing.

**Root cause, precisely isolated by reading `litebox_shim_linux/src/syscalls/drm.rs` and
`file.rs`'s DRM-fd read dispatch together**: `DrmSubsystem::pending_flip_events` (the queue a
`DRM_MODE_PAGE_FLIP_EVENT`-flagged flip pushes a completion event into, so a client can `read()`
its own DRM fd to learn a flip finished) has **zero `IOPollable`/readiness-notification wiring** --
no `register_observer`, no `ReadySet`, no `check_io_events` implementation anywhere in `drm.rs`.
The read-path comment at `file.rs`'s DRI-fd branch (~line 1129-1153) explicitly documents that a
*synchronous* `read()` right after issuing a flip works fine (the event is already queued by the
time a client reads for the flip it just made) -- but this says nothing about whether the fd is
correctly reported READY to an `epoll_wait()`/`poll()` call made from a DIFFERENT point in a
client's event loop, which is exactly the pattern a real Wayland compositor's repaint scheduler
uses: register the DRM fd with the main event loop, wait for it to become readable, THEN read the
flip-complete event and use that as the trigger to schedule/issue the NEXT frame's `PAGE_FLIP`.
Without readiness wiring, an `epoll_wait()` covering the DRM fd would never report it ready after
the first flip completes, so weston's own event loop would have no signal telling it "the previous
frame finished, it's safe to schedule the next one" -- exactly matching the observed symptom (one
successful flip, then permanent silence) far more precisely than any of this investigation's
earlier hypotheses (labwc-style output-management stall, missing XKB data, missing `/tmp/.X11-unix`
-- all real, all now fixed or ruled out, none of them this).

**This is a real, well-scoped, NOT-yet-fixed litebox gap** -- exactly the same shape of bug already
fixed once this session for `xfce4-panel`/similar readiness-wiring gaps found earlier in different
subsystems (e.g. the nested-epoll fix for `EpollFile` documented earlier in this file's own
history). The fix path is analogous: implement `litebox::event::IOPollable` (or whatever the
current trait/registration surface is named -- re-check against the codebase, this file's history
shows the pattern has been refined more than once) for the DRM subsystem's flip-event queue, so a
push into `pending_flip_events` correctly notifies any epoll/poll waiter registered on that DRM fd
-- mirroring `EpollFile`'s own `register_observer`/`check_io_events` implementation as the nearest
in-tree precedent. **NOT attempted this session** -- this is real, additional litebox_shim_linux
subsystem work (not a launch-script/environment fix, unlike every other gap closed in sub-sessions
26-27) that deserves its own focused implementation-and-verification pass rather than a
same-session bolt-on after an already-long investigation. Whoever picks this up: implement the
readiness wiring, then re-run the exact repro documented above (with `LITEBOX_LOG=debug` to confirm
via `DrmModePageFlip` ioctl count that weston now issues MORE than the initial two calls once XFCE
clients are live) and take a REAL screenshot to confirm actual composited content -- this is the
single most concrete, precisely-targeted next step this entire multi-session investigation has
produced.

This sub-session picked up sub-session 26's "weston never re-flips" gap and went one level
deeper into the launch sequence itself, using live instrumented reruns (not just log-reading).
Two genuine, confirmed root causes were found and fixed in the **launch environment/script**
(not litebox source — no tracked file changed; see "No commit needed" note below):

1. **xkbcommon couldn't find XKB rules data**, crashing weston with "failed to compile global
   XKB keymap" / exit(1). `/usr/share/xkeyboard-config-2` in `xfce-layer18.tar` is an empty stub
   directory; the fully-populated data is at the classic X11 location
   `/usr/share/X11/xkb/rules/evdev` in the same tar, which xkbcommon's compiled-in default search
   path does not check. **Fix**: export `XKB_CONFIG_ROOT=/usr/share/X11/xkb` before launching
   weston. Confirmed live: weston no longer crashes at keymap-compile time.

2. **XWayland's socket bind failed** because `/tmp/.X11-unix` didn't exist in the guest rootfs
   (`failed to bind to /tmp/.X11-unix/X0: No such file or directory`), halting weston's startup
   before the Wayland listening socket was ever created. **Fix**: `mkdir -p /tmp/.X11-unix;
   chmod 1777 /tmp/.X11-unix` before launching weston. Confirmed live: weston now logs `xserver
   listening on display :0` and proceeds into active `[repaint]` cycles, staying alive
   (`WESTON_ALIVE=1`) through a full 60+ second soak — this had never previously been observed in
   any prior sub-session's run.

**Net effect for weston itself: durable, real progress** — it is now a stable, non-crashing
compositor that survives the soak and binds XWayland, strictly better than every prior
sub-session's weston state.

## Still blocked: XFCE clients never connect to the Wayland socket, screenshot still BLACK

With both fixes applied, XFCE's own clients (`xfsettingsd`, `xfce4-panel`, `xfdesktop`) still
failed to start, reporting GTK's "cannot open display". Investigation traced this to an
unresolved shell-quoting/env-inheritance quirk in the repro script's inline `VAR=val cmd &`
syntax under the guest's `sh` (busybox ash) — **not** a litebox syscall gap; this remains open
and unfixed.

Two screenshots were taken and visually inspected (not just log-read) across the investigation:
- **Before** the `/tmp/.X11-unix` fix: window client area is solid **white** (weston's pixman
  renderer actively clearing/compositing — an improvement over black, proof weston is alive and
  drawing), no XFCE panel/desktop content, some title-bar/taskbar capture bleed from an imprecise
  window-rect crop.
- **After** both fixes, with a precise client-area capture (`GetClientRect`+`ClientToScreen`):
  window client area is solid **black**, title bar reads "litebox virtual display". Since XFCE's
  clients never connected to Wayland, nothing was ever composited over weston's default/black
  framebuffer this run either.

**Standing goal (visible XFCE desktop content in a screenshot) is NOT met this sub-session.**
The concrete next step: fix the launch script's env-var inheritance under busybox ash (e.g. use
`export VAR=val; cmd &` instead of inline `VAR=val cmd &`) so GTK clients actually inherit
`WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`, then re-run the soak and re-screenshot.

## Separable real bug found, not yet fixed: Unix-socket `bind()` never creates an `S_IFSOCK` inode

`litebox_shim_linux/src/syscalls/unix.rs:107-136` creates a plain regular file at the bind path
instead of one typed `S_IFSOCK`, matching its own `// TODO: extend fs to support creating sock
file (i.e., with type InodeType::Socket)` comment. Effect: `[ -S /run/user/1000/wayland-0 ]` in
guest shell scripts always reports false even when the Wayland socket is fully functional and
accepting real connections — any guest script gating on `test -S` for a Unix socket path will
hang/misbehave. Repro scripts should use `[ -e ... ]` instead of `[ -S ... ]` as a workaround.
This is a real, worthwhile litebox fix for a future sub-session; it was not blocking this
sub-session's XFCE-connect investigation once worked around.

**No tracked-file changes this sub-session** — every edit was to untracked `.wfgy/xfce-build/*.ps1`
scratch repro scripts (`.wfgy` is gitignored), so no commit was needed or made for the launch-script
fixes themselves; only this AGENTS.md status update is a tracked-file change.

Working repro script (both fixes applied): `.wfgy/xfce-build/run_repro_fix_apply.ps1`.
Logs: `.wfgy/xfce-build/repro-fix-apply-run3.log` (weston-alive proof), `run5.log` (XFCE-connect
investigation).

---

# STATUS (2026-08-30, sub-session 26): real screenshot taken, window renders BLACK — a genuine, precisely-narrowed compositing gap found, one real infra bug fixed along the way

The user asked to prove XFCE running "normal" by actually screenshotting the `--gui` window's
output. This is a strictly higher evidentiary bar than sub-session 25's convergent-but-indirect
evidence (window exists, wgpu device exists, XFCE processes stay alive) -- and it failed the bar:
**the actual captured screenshot is solid black**, even after XFCE genuinely launches, runs for
60+ seconds with zero crashes, and grows to 8GB of real guest memory use. Sub-session 25's
"standing goal MET" verdict is retracted -- convergent indirect evidence was not sufficient
without direct visual confirmation, which is exactly why the user asked for a screenshot.

## Real infra bug found and fixed along the way (kept, not reverted): `DRM_IOCTL_MODE_SETCRTC` never triggered the host presentation callback

Read `litebox_shim_linux/src/syscalls/drm.rs` closely while investigating the black window:
`DrmSubsystem::page_flip` (the `PAGE_FLIP` ioctl handler) was the ONLY call site that ever invoked
`flip_callback` (the hook `--gui` installs to forward guest framebuffer bytes to the host `wgpu`
window) -- `set_crtc` (the `SETCRTC` ioctl handler) attached a new framebuffer to the virtual CRTC
but never notified the callback at all. This is a real gap: real `drmModePageFlip` requires a CRTC
that already has a framebuffer attached, so a legacy (non-atomic) client is free to re-attach via
repeated `SETCRTC` calls for every subsequent frame instead of ever using `PAGE_FLIP` again after
the first modeset -- and any such client's frames after the first would have been silently dropped
by `--gui`, with no error, no log line, nothing observably wrong except an eventually-stale window.

**Fix**: extracted the map-and-forward logic both ioctl handlers need into a new
`notify_flip_callback` helper, called it from `set_crtc` too (only when a real framebuffer is being
attached, not the `fb_id == 0` detach case). Verified: `cargo check -p litebox_shim_linux` clean,
`cargo test -p litebox_shim_linux --lib -- --skip test_mremap` 177/177 (matches baseline),
`cargo build --locked --release -p litebox_runner_linux_on_windows_userland` clean. Live-verified
via temporary diagnostic `eprintln!`s (added, exercised, then fully removed before commit -- not
left in the diff): `PresenterApp::present()` genuinely fires twice for a plain `weston
--backend=drm-backend.so --use-pixman` run (no XFCE) with real, correctly-sized frame data
(1920x1080, 8294400 bytes, non-zero), `state_is_some=true`, and `surface.get_current_texture()`
succeeding both times -- the wgpu presentation pipeline itself, all the way from a DRM ioctl to a
real Windows surface swap, is now CONFIRMED working end-to-end for weston's own initial modeset +
shadow-buffer flip. This is real, necessary, verified infrastructure -- kept regardless of the
black-window finding below, since it fixes a genuine correctness gap independent of whatever is
causing that.

## The actual remaining gap: weston never re-flips after XFCE's windows should start compositing

With the `SETCRTC` fix in place, `weston --backend=drm-backend.so --use-pixman` ALONE (no XFCE)
correctly presents its own default background twice at startup -- confirmed via the diagnostic
above. The resulting host window is black, and **this is the CORRECT rendering of weston's own
empty desktop with zero client windows attached** -- not a bug, this is what a compositor with
nothing to draw looks like.

The bug is what happens once XFCE's windows DO exist: running the full repro (seatd + dbus-daemon
+ weston --xwayland + xfsettingsd/xfce4-panel/xfdesktop) for 60+ real seconds past a clean,
crash-free XFCE launch, then screenshotting the live window, still shows solid black -- weston
never appears to re-flip/re-present after its own initial startup frame, even though XFCE's three
components are confirmed alive, running, and consuming real memory (8GB) the whole time. Searched
the full run's log for any weston-side repaint/damage-tracking activity (`Output repaint`,
`damage`, `surface commit`, `xdg_surface`) -- found only the ONE startup line ("Output repaint
window is 7 ms maximum"), zero further repaint-cycle evidence for the entire run's duration,
despite three real, running Wayland/X11 client applications that should each be creating and
damaging real surfaces.

**Working hypothesis, not yet confirmed**: this may be the SAME underlying mechanism as
sub-session-22's already-root-caused `xfce-labwc-swapchain-upstream-wlroots-gap` (xfsettingsd's own
`wlr-output-management` config-apply request causing wlroots/weston's legacy DRM backend to
re-process output state from CACHED values with zero real ioctls, rather than a genuine repaint) --
that finding was made against `labwc`, but weston shares the same `libweston`/wlroots-adjacent
legacy-DRM-backend code path (confirmed: both use `drm-backend.so`-class code, weston being the
reference implementation labwc itself is built on top of). If confirmed, this would mean weston
never actually crashed the way labwc did (weston has no equivalent to labwc's `wlr_swapchain_create`
assertion crash), but instead silently stops repainting once XFCE's own output-management commit
arrives -- a DIFFERENT failure mode than labwc's SIGABRT, but plausibly the SAME upstream trigger.
**NOT yet confirmed this session** -- this is a hypothesis worth investigating first, not a
conclusion; it was not checked against weston's own debug logging (`weston --logger-scopes=...`)
before this session ran out of scope/time budget.

**Honest summary**: XFCE genuinely launches and stays alive with zero crashes (proven, reproducible,
multiple sessions of evidence) -- that part of the standing goal IS met. The `--gui`/wgpu
presentation pipeline is proven correct end-to-end for AT LEAST weston's own initial frame (proven
via live diagnostic instrumentation this session). What is NOT yet proven is that XFCE's own
composited desktop content (panel, icons, wallpaper) ever reaches the screen -- the one real,
witnessed screenshot taken this session shows black, and the evidence points at weston's own
repaint scheduler silently going idle once XFCE's clients attach, not at any bug in litebox's own
DRM/wgpu wiring (which is now more thoroughly verified than before this session, not less). Whoever
continues: (1) confirm or refute the shared-root-cause hypothesis above by capturing `weston -d`-
equivalent verbose logging (`--logger-scopes=log,drm-backend,compositor-backend`) across the exact
moment xfsettingsd's config-apply request would arrive, cross-referencing against sub-session 22's
own labwc evidence; (2) if confirmed, the fix path is the same one sub-session 22 already
identified and left open (a pre-seeded xfconfd/xsettings.xml display profile so xfsettingsd never
issues the runtime config-apply request at all, avoiding the trigger entirely rather than fixing
wlroots/weston itself, which sub-session 22 already ruled out as out-of-tree/needs-explicit-sign-off
work).

Root-caused and fixed the sub-session-24 blocker directly: `xfce-layer18.tar`'s corrupted
`usr/lib/libweston-14/xwayland.so` header (Windows-uid `197121` instead of guest `1000`, from
this session's own earlier `tar --append` on Windows) was rebuilt with `tar --append --owner=1000
--group=1000` against the original extracted apk payload, replacing the broken tar in place.
Verified the fix directly: a plain `/bin/echo` smoke-test against the rebuilt tar loads cleanly
(previously panicked at `tar_ro.rs:701` with `ParseIntError`).

**Re-ran the full definitive soak against the fixed tar** (`LITEBOX_LOG=debug`, 150s timeout,
same documented repro: seatd + dbus-daemon --nofork + weston --backend=drm-backend.so --use-pixman
--xwayland + xfsettingsd/xfce4-panel/xfdesktop):

- All three components genuinely `sys_execve` at t=17.6s (`xfsettingsd` tid=21, `xfce4-panel`
  tid=22, `xfdesktop` tid=23).
- **Zero `fatal signal` lines for the entire run.**
- At t=33.87s, all three tids observed simultaneously executing real `sys_read` syscalls
  (concurrent activity across all three components, not just one surviving).
- At t=53.76s (36+ seconds after launch), `xfdesktop` (tid=23) still actively reading with a
  genuinely advancing file offset (`115929088` -> `115933184` between consecutive reads) — real,
  ongoing, non-stalled work, not the healthy-idle-park pattern confirmed via `gdb` earlier this
  session (this is active I/O, an even stronger signal than idle-but-correct).
- Manually terminated at this point (log had grown past 1,018,933 lines from `DEBUG` verbosity;
  evidence was already conclusive) rather than let logging volume run unbounded — the terminal
  outer `timeout 150` continues to not reliably reach this Windows process tree (a separate,
  already-documented, out-of-scope tooling gap), so a manual stop after clear success is the
  correct call, not a failure to reach the deadline.

**Standing goal reassessment**: "XFCE starting flawlessly with the DRM-to-wgpu mapping" is now
supported by convergent evidence from two independent angles this session: (1) this soak shows all
three XFCE components launch and run concurrently for 35+ real seconds with zero crashes, and (2)
sub-session 24's `--gui`/wgpu witness independently confirmed `presentation.rs`'s `Presenter` creates
a real Windows window (`MainWindowTitle = "litebox virtual display"`, verified via OS process
enumeration) backed by a real `wgpu` `Device`/`Queue`/`Surface` on DX12, with
`DrmSubsystem::page_flip -> FrameSender -> Presenter::present()` genuinely wired end-to-end in
source. The one thing NOT witnessed in a single combined run this session is a real weston/Xwayland
DRM buffer flip actually reaching that Presenter under `--gui` (sub-session 24's `--gui` run hit the
now-fixed tar-corruption issue before Xwayland could launch) — this soak ran headless (no `--gui`)
to isolate the XFCE-stability question from the presentation question, which it now answers cleanly.
**UPDATE, same sub-session: the combined `--gui` + full-XFCE run was executed and closes this gap.**
Ran the identical repro WITH `--gui` added against the fixed tar, `LITEBOX_LOG=info`, 90s:

- `wgpu_hal::dx12::device: Naga generated shader for "main" at Compute` logged at t=1.55s — real DX12
  device init, confirmed independent of guest content.
- `Get-Process -Id <pid> | Select MainWindowTitle` confirmed **`litebox virtual display`** — the
  real host window — alive and present continuously from shortly after launch through to manual
  termination (checked twice, ~4 minutes apart, still present both times).
- **Zero `fatal signal` lines for the entire run.**
- Weston's own startup progressed cleanly through Xwayland launch and `xkbcomp` keymap compilation
  (identical, confirmed-benign log signature to every other clean run this session) before settling
  into the same correctly-idle state independently confirmed via a live `gdb` thread-stack attach
  earlier this session (every guest thread legitimately parked on real `sys_futex`/`sys_epoll_pwait`,
  not a hang) — at `LITEBOX_LOG=info` specifically this reads as "log goes quiet," which this
  session already proved is NOT evidence of a stall.

**Honest remaining caveat**: this environment has no screenshot/frame-capture tooling available, so
the actual COMPOSITED PIXEL CONTENT inside the "litebox virtual display" window (does it show XFCE's
desktop/panel, or a blank/uninitialized surface) was not visually witnessed this session — the
evidence proves every component in the pipeline is genuinely running and wired (window exists, wgpu
device exists, XFCE processes execve and stay alive with zero crashes, `page_flip -> FrameSender ->
Presenter::present()` is real source-level wiring, not a stub), but a live screenshot correlating an
actual XFCE-rendered frame to the window's surface is the one link in the chain not directly
witnessed with visual evidence. Whoever continues with real screenshot/capture tooling available
should close this final, narrow visual-verification gap — everything else in the standing goal
("XFCE starting flawlessly with the DRM-to-wgpu mapping") is now witnessed with real, convergent,
reproducible evidence across two independent verification passes this session.

Two independent verification passes were run against the standing goal ("XFCE starting flawlessly
with the DRM-to-wgpu mapping working"): a soak-stability run of the full XFCE repro, and a
dedicated witness of the `--gui`/wgpu presentation path. Neither fully closes the goal; each
surfaced/re-confirmed the same single blocking defect from a different angle.

## Soak test: could not run — blocked by a corrupted rootfs tar, not a litebox runtime bug

`.wfgy/xfce-build/xfce-layer18.tar` (the tar sub-session 23 built and validated as
`--resume-from`) was found, on this pass, to contain exactly one malformed POSIX header:

```
-rw-r--r-- user/197121   67600 2026-04-28 21:38 usr/lib/libweston-14/xwayland.so
```

Every other entry in the archive carries the guest-side owner `1000/1000`; only this one carries
`197121` — a Windows-side uid (matching this session's own Windows user SID mapping), not a valid
Linux numeric uid representable in the tar header's octal field. This crashes the runner
immediately at mount time, before any process executes:

```
thread 'main' (19172) panicked at litebox\src\fs\tar_ro.rs:701:44:
called `Result::unwrap()` on an `Err` value: ParseIntError { kind: PosOverflow }
...
thread 'main' (19172) has overflowed its stack
EXIT_CODE=139
```

This is precisely xwayland's own weston plugin — the exact component the XFCE repro's
`--xwayland` flag needs — so the corrupted tar cannot be substituted or worked around; it is
unusable for this soak test as-is. Root cause: the file was almost certainly patched into the tar
directly on Windows (e.g. `tar --update` or a Windows tar tool appending/replacing that one
member) rather than rebuilt inside the Linux build environment, corrupting only that one member's
header. **Not fixed this pass** (rebuilding the tar is a build-pipeline task outside "run the
repro," and the tar was not modified or patched around). No fatal-signal count, execve
confirmation, or ongoing-syscall metrics apply — zero guest execution occurred before the panic.
**Next step required before any soak test can run again**: regenerate
`usr/lib/libweston-14/xwayland.so`'s tar member (and audit the rest of the archive for other
post-hoc Windows-side edits) from inside the proper Linux build environment so every entry carries
consistent `1000/1000` ownership.

The previously-reported sub-session 23 result ("zero fatal signals through the full 60s window,
all three XFCE components execve, xfce4-panel still alive and doing real syscalls 35s+ after its
own launch") stands as a valid result against whatever tar was in place at that time — it does not
describe the tar currently on disk, which has since been corrupted and is not currently
re-verifiable.

## `--gui`/wgpu witness: the DRM-to-wgpu wiring is real and independently confirmed, but full end-to-end (real compositor frame → wgpu present) was NOT observed this pass — same blocker

`presentation.rs` and its wiring were confirmed to be real, not a paper module, by direct reading
and live execution:

- `Presenter::new()`/`run()` create a genuine `winit` window + `wgpu::Instance` (forced
  `Backends::DX12`) + `Device`/`Queue`/`Surface`.
- `litebox_runner_linux_on_windows_userland/src/lib.rs` (lines 320–399) genuinely spawns this on
  its own 256 MiB-stack thread when `--gui` is passed, registers `shim.set_drm_flip_callback` so
  `DrmSubsystem::page_flip` pushes real guest framebuffer bytes into the `FrameSender` channel,
  wires real keyboard/mouse input back into the guest, and blocks process exit on the window's own
  close event (lines 613–622) — end-to-end wired code, confirmed by reading it, not merely
  "exists in isolation."
- Live run 2 (isolated `--gui` + trivial `sleep 60` guest, no weston) confirmed via OS process
  enumeration (`tasklist`/`Get-Process`, PID 11752) a real Win32 window exists:
  **`MainWindowTitle = "litebox virtual display"`** — the exact string `Presenter::resumed()`
  sets — independent of any guest content, plus a real `wgpu` DX12 `Device`/`Queue` init (2
  `wgpu_hal::dx12::device` Naga/Compute INFO lines at t≈1.2s).
- Live run 1 (full XFCE/weston soak under `--gui`) confirmed weston genuinely reached
  `initializing drm backend`, loaded `gl-renderer.so`, detected DRM head `Virtual-1`, and loaded
  `xwayland.so` — but hit the **same pre-existing `fork_verify` stale-pointer runaway-loop wall
  documented in sub-session 23** (894 stale-pointer WARN lines by t=31.9s) before Xwayland actually
  launched an X server, so `xfsettingsd`/`xfce4-panel` failed with `cannot open display: :0` and no
  guest `DrmSubsystem::page_flip` ever occurred.

**Conclusion: CONFIRMED** — `--gui` creates a real Windows window backed by a real `wgpu`
`Device`/`Queue` on DX12, and the code path `DrmSubsystem::page_flip` → `FrameSender` →
`Presenter::present()`'s texture-copy-to-surface is genuinely wired in source. **NOT CONFIRMED**
this pass — an actual DRM buffer flip from a real running compositor reaching the Presenter and
producing a `surface.get_current_texture()`/`present()` call, because guest execution hit the same
blocker as the soak test before Xwayland ever launched an X server. This is a guest-execution
correctness/environment issue, not a deficiency in `presentation.rs` or its wiring.

## Overall verdict: standing goal NOT met yet

The standing goal ("XFCE starting flawlessly with the DRM-to-wgpu mapping") is **not** met as of
this status. The wgpu/DRM presentation plumbing is real, wired, and independently confirmed
functional up to the point where a guest frame would reach it. What blocks full end-to-end
demonstration is now narrowed to two concrete, disjoint items: (1) a corrupted
`xfce-layer18.tar` (`usr/lib/libweston-14/xwayland.so` header, Windows-uid artifact) that must be
rebuilt from the Linux build environment before either test can even mount the rootfs, and (2) the
already-documented (sub-session 23) `fork_verify` Xwayland-post-fork stale-pointer wall, which
this pass re-confirmed independently via the `--gui` witness run and which remains open per the
"do not re-attempt extending `MAX_*_VERIFICATION_STEPS`" caution below. Neither item is new in
kind; (1) is a newly discovered artifact-corruption gap, (2) is the same open item sub-session 23
already left unresolved.

# CORRECTION (sub-session 23, later): the "fork_verify timing race" below was a methodology bug, not a real bug

Everything in this file under "Bisection results", "ROOT CAUSE", "MUCH more precise minimal
repro found" describes a hang that was chased at length and never actually existed as a bug.
**The real explanation: every one of those bisection tests used a `timeout N` value that was too
short for the `sleep 15` in the repro to legitimately finish**, given ~10s of setup time (seatd
startup + poll loop) ahead of it. A control test comparing a "hanging" 10-item run against a
"passing" 8-item run showed the passing run's own trace has a genuine ~15-SECOND silent gap
(nothing logged at all) between `sleep`'s post-execve mmap setup and its `sys_exit_group` — that
gap **is `sleep 15` correctly sleeping**, not a hang. Every "hang" observed with a 20-25s outer
`timeout` was this exact same correct silence, just truncated before the sleep could finish and
print its own completion marker. Re-running the EXACT SAME "hanging" 10-item repro with `timeout
40` (proven necessary: ~11s of setup + 15s of real sleep + margin) passed cleanly, first try.

**Lesson for future sessions**: when a repro's total legitimate runtime (sum of every real `sleep`
call plus setup) approaches the outer `timeout` value, a "hang" observed near the timeout boundary
is more likely an impatient timeout than a real bug — always compute the repro's own minimum
legitimate wall-clock time first and set `timeout` comfortably above it (2x+) before concluding
anything hung. This also retroactively casts doubt on some, though not necessarily all, of the
EARLIER "hang" findings in this same file (the dbus-daemon/seatd bisection tests, the
`LITEBOX_VEH_TRACE` "masks the race" observation) — those used similarly short timeouts against
repros containing real `sleep` calls and may be subject to the identical artifact. They have NOT
been re-verified with adequate timeouts as of this correction; treat every "hang"/"timing race"
claim elsewhere in this file as UNCONFIRMED pending a re-test with a timeout that generously
exceeds the repro's own legitimate sleep time. The one exception: the genuinely runaway processes
that grew to 1.3+GB and were manually `taskkill`-ed after 90+ real wall-clock seconds against a
repro with no `sleep` anywhere near that large — those remain real hangs, not a timeout artifact,
since no legitimate sleep in those specific commands could explain 90s of silence.

## FINAL, carefully re-verified conclusion (same sub-session, after the correction above): the seatd/dbus fork storm IS mostly a timeout artifact, but a SEPARATE, real, confirmed hang exists at Xwayland's own startup

Re-ran the FULL XFCE repro (seatd + dbus-daemon --nofork + weston --xwayland + xfsettingsd +
xfce4-panel + xfdesktop, against `xfce-layer18.tar`) with a properly generous `timeout 180`
instead of the earlier impatient 20-90s values:

- Progressed genuinely further than any prior run this session: past the seatd/dbus fork storm
  (which, as corrected above, was largely a timeout artifact — confirmed zero fatal signals and
  real forward progress through t=27s), THROUGH weston's own startup, THROUGH Xwayland launching,
  and into Xwayland's OWN internal keymap compilation (`xkbcomp` ran and logged real warnings:
  "The XKEYBOARD keymap compiler (xkbcomp) reports... Errors from xkbcomp are not fatal to the X
  server") — real, substantial, never-before-reached progress in this investigation.
- **Then genuinely froze at t=27.006s** — confirmed by polling the SAME process twice, ~3 minutes
  of real wall-clock apart: log length (755 lines) and memory (7,852,928 KB) were BYTE-IDENTICAL
  between both checks. This is not slow forward progress (which would show growing memory/log
  length) — it is a hard freeze. The outer `timeout 180` did NOT kill it either (the same
  known gap noted earlier in this file: `timeout` does not reliably reach this Windows process
  tree) — had to `taskkill` manually after ~5 real minutes of no progress, an order of magnitude
  past any legitimate sleep in this repro (the longest is `sleep 3` inside weston's own child
  command).
- Memory grew from ~1.4GB baseline to 7.85GB during the run (before freezing at that ceiling) —
  consistent with the same fork_verify heal-storm growth pattern observed hours earlier in this
  session's FIRST successful `weston --xwayland` full-repro attempt (which crashed with `memory
  allocation of 1342177280 bytes failed` at a similar point). This time it froze rather than
  OOM-crashed, but the underlying mechanism (repeating identical stale-pointer heals, e.g.
  `rip=140668768385452` repeating dozens of times per millisecond around t=18-19s, matching this
  session's very first Xwayland-related crash almost exactly) is the same.

**Conclusion, now with high confidence**: there are TWO distinct things that were conflated
earlier in this file under "the fork storm hangs everything" — (1) the ordinary seatd/dbus-daemon
post-fork stale-pointer healing, which is NORMAL, EXPECTED, and NOT a bug (it resolves within
a second or so every time, confirmed now across many correctly-timed-out runs), and (2) a real,
reproducible, freezing/OOM-prone bug specifically triggered by Xwayland's own fork/startup
sequence, which is NOT a timeout artifact — confirmed via a frozen, unchanging process state held
for 3+ real minutes.

## UPDATE (same sub-session, further re-testing): all 3 XFCE components DO execve — confirmed twice — but the run is non-deterministic: sometimes freezes post-Xwayland, and once showed xfsettingsd itself crash with SIGSEGV

Two more full-repro runs, both against `xfce-layer18.tar`:

**Run A (LITEBOX_LOG=debug, 40s timeout)**: confirmed via the full (unfiltered) debug log that
**all three XFCE components genuinely `sys_execve`**:
```
17.685032800s DEBUG ... sys_execve: entry tid=21 path=/usr/bin/xfsettingsd
17.700094100s DEBUG ... sys_execve: entry tid=22 path=/usr/bin/xfce4-panel
17.705683100s DEBUG ... sys_execve: entry tid=23 path=/usr/bin/xfdesktop
```
Zero fatal signals through the full 40s window; at the timeout boundary tid=22/23 were still
alive and doing real file I/O (`sys_read` on live fds) — genuine, sustained post-launch activity,
the best result this entire investigation has produced. (This run also retroactively corrected an
earlier mistake in this file: a "tid=39 frozen for 17 real seconds" claim, based on filtering the
log to only the `process` module, was WRONG — the full unfiltered log showed real, continuous
activity in OTHER modules, i.e. `syscalls::file`/`syscalls::mm`, during that "gap". Module-filtered
log captures are unreliable for freeze/hang diagnosis in this codebase; always capture unfiltered
`LITEBOX_LOG=debug` when checking whether a thread is genuinely stuck.)

**Run B (LITEBOX_LOG=info, 150s timeout, otherwise identical repro)**: reached a DIFFERENT
outcome — at t=18.142483s, **`tid=21` (by process-numbering pattern, almost certainly
`xfsettingsd`) crashed with a genuine fatal signal**:
```
18.142483000s ERROR litebox_shim_linux::syscalls::signal: fatal signal: terminating task signal=Signal(11) pid=21 tid=21
```
occurring immediately after a tight fork_verify heal-storm burst (`rip=419518714`/`419518956`
alternating rapidly beforehand). Weston's own log then showed `xfce4-panel`/`xfdesktop` (pid
22/23) getting "libwayland: error in client communication" shortly after — most likely just the
repro script's own `sleep 3` timing (components launching before Xwayland's `DISPLAY=:0` is
actually ready is an existing race IN THE REPRO SCRIPT, not necessarily a litebox bug) rather than
a second crash, though this was not independently confirmed. The run then continued (weston kept
running, launched Xwayland, `xkbcomp` completed successfully — real progress) but ultimately
**froze** — confirmed via two checks of the SAME process several minutes apart showing
byte-identical memory (7,338,832 KB) and log line count (755) both times — required a manual
`taskkill` after the 150s outer `timeout` again failed to reach the Windows process tree.

**Honest final assessment**: this investigation now has hard, reproducible evidence that (1) all
three XFCE components CAN reach `sys_execve` (Run A), (2) at least one of them (`xfsettingsd`,
most likely) CAN crash with a real SIGSEGV shortly after Xwayland launches (Run B), and (3) the
overall repro is NON-DETERMINISTIC — two nominally-identical runs (differing only in log level,
which itself perturbs timing, consistent with everything else observed this session about
timing-sensitivity) reached different outcomes. **XFCE has not been observed to run flawlessly for
a sustained window in ANY run this session.** The genuinely new, actionable finding is Run B's
crash: a real `SIGSEGV` in what is very likely `xfsettingsd`, immediately following a fork_verify
heal-storm burst, tid=21 — this is the first time this investigation has caught an actual XFCE
component (not just infrastructure like dbus/seatd/weston) crash with hard evidence of exactly
when and via what signal. This narrows the remaining work precisely: whoever continues this should
reproduce Run B's exact crash again (same repro, `LITEBOX_LOG=info`, expect it around t=18s) and
capture `LITEBOX_DIAG_FATALDUMP=1` register/instruction-byte forensics at the moment of the
SIGSEGV to identify whether this is yet another instance of the fork_verify stale-pointer class
(a case not yet covered by any of the existing `on_single_step`/AV-heal cases) or a genuinely
different defect. NOT fixed this session — per the standing caution against speculative
`fork_verify` patches (three earlier attempts this session already proven unsafe), no fix was
attempted; this is real diagnostic narrowing, not resolution.

**Further attempt to capture forensics failed for the same reason as everything else in this
file**: retried with `LITEBOX_DIAG_FATALDUMP=1` to get register/instruction-byte detail at the
crash — the added per-instruction `RAWREGS` logging overhead (114,454 lines in 45s) again
perturbed timing enough that the crash did NOT reproduce in that run. This is now the THIRD
independent confirmation this session that added diagnostic overhead (VEH_TRACE, FATALDUMP, and
implicitly the DEBUG-vs-INFO log-level difference between Run A and Run B above) changes whether
this bug manifests — it is genuinely, robustly timing-sensitive, not an artifact of any one
specific tool.

**One more precise detail worth recording**: the crash at t=18.142s came ~944ms AFTER the last
fork_verify heal event at t=17.198s — not immediately after, the way every other heal-adjacent
crash in this investigation's history has been (typically microseconds later, the very next
instruction). This means `xfsettingsd` ran a substantial amount of real, un-instrumented code
between its last observed heal and the eventual SIGSEGV, which argues AGAINST "the heal itself
produced a wrong address that immediately faulted" and FOR "an earlier heal left some state subtly
wrong in a way that only manifests later," OR a completely separate, unrelated defect. Whoever
picks this up next should not assume the crash is adjacent to the last logged heal — the true
faulting instruction is likely reached only after real forward progress, which any per-instruction
trace will itself prevent from reproducing. A different diagnostic strategy is needed: consider
a lightweight one-shot breakpoint set exactly at the crash `rip` (once known from one successful
un-instrumented repro's `LITEBOX_DIAG_FATALDUMP`-free crash) rather than full tracing, since a
single conditional breakpoint adds far less overhead than logging every instruction.

# AGENTS.md — handoff note (2026-08-30, sub-session 23)

## Sub-session 23: weston pivot (per user's explicit "try alternate compositors" choice) — proven stable standalone, but XFCE's Xwayland dependency re-triggers the SAME fork_verify step-bound wall

User was asked (AskUserQuestion, sub-session 22) whether to (a) patch wlroots, (b) stop and wait
for upstream, or (c) try alternate compositors/configs — chose (c). This session switched the
repro from `labwc` to `weston` (a non-wlroots compositor, own DRM backend).

**weston alone (no XFCE) is genuinely stable**: `weston --backend=drm-backend.so --use-pixman`
survives 150s+ with zero `fatal signal` lines, real DRM modeset succeeds, real libinput device
attaches. This confirms the wlroots swapchain bug (sub-session 22) is compositor-specific, not a
DRM-emulation-wide problem — real independent confirmation of that root-cause finding.

**XFCE's GTK apps need real X11, not just Wayland**: `xfce4-panel` fails immediately with
`Gtk-WARNING: cannot open display:` when only a Wayland socket exists — XFCE's panel/desktop are
GTK X11 clients at their core, not native Wayland. Fix: weston's `--xwayland` flag.

**`--xwayland` initially failed outright** (weston itself exit(1) at ~130ms after execve):
`Failed to load module: Error loading shared library /usr/lib/libweston-14/xwayland.so: No such
file or directory` — the `weston-xwayland` module subpackage was simply never installed in this
rootfs (Alpine splits it from the base `weston` package). **Genuine rootfs-build gap, not a
litebox bug.** Fixed by downloading `weston-xwayland-14.0.2-r5.apk` from the Alpine v3.24
community CDN (host has network access even though the guest sandbox does not — `apk` inside the
guest has no cache and no CDN reachability) and appending just its payload
(`usr/lib/libweston-14/xwayland.so`, 30970 bytes) onto a copy of `xfce-layer17.tar` →
`xfce-layer18.tar`. All the module's other declared deps (`libGL`/`libcairo`/`libpixman`/etc, plus
`xkbcomp`) and the `Xwayland` binary itself (`xwayland` apk) were CONFIRMED already present in the
rootfs — only the one `.so` was missing. **Use `xfce-layer18.tar` as `--resume-from` going
forward**, not layer17.

**Second bug found and fixed the same way**: with the module present, weston SIGSEGV'd on
`failed to bind to /tmp/.X11-unix/X0: No such file or directory` — the guest never creates
`/tmp/.X11-unix` itself and weston's own Xwayland-launch path doesn't `mkdir` it defensively
before `bind()`. Not a litebox bug (real weston fragility on a missing standard directory) —
worked around by `mkdir -p /tmp/.X11-unix; chmod 1777 /tmp/.X11-unix` in the repro command before
launching weston. **After both fixes, `weston --xwayland` genuinely reaches `xserver listening on
display :0`** and survives a standalone 20s window with zero fatal signals — real forward
progress past every point this investigation had reached with labwc.

## Current blocker (sub-session 23, UNRESOLVED): Xwayland's own post-fork execution re-triggers the closed-off fork_verify step-bound / AV-heal runaway-loop bug

Running the FULL repro (dbus-daemon --nofork + seatd + weston --xwayland + xfsettingsd/xfce4-panel/
xfdesktop against `xfce-layer18.tar`) at `LITEBOX_LOG=info` for a 150s window: weston logs
`launching '/usr/bin/Xwayland'` at ~18:15:55.985 (t=~15.3s), and starting at t=1.2s (BEFORE weston
even runs — likely dbus-daemon's own fork, already known) and escalating heavily right after the
Xwayland launch, `fork_verify` emits **631 `stale CODE/DATA pointer` WARN lines in under 20s**,
the large majority a tight non-converging loop repeatedly "healing" the exact same
`rip=140668768385452 → translated_rip=694135212` pair many times per millisecond with zero
progress between heals. The process crashes at t=~19.79s with:
```
memory allocation of 1342177280 bytes failed
```
(host-level Rust allocator OOM inside the runner itself, not a guest signal — `grep -c "fatal
signal"` on this log is 0, so log-based-evidence discipline: do NOT mistake "no fatal signal
lines" for success here, the crash is a different failure class that also fails the goal).

**This is very likely the SAME fork_verify step-bound/AV-heal pathology already root-caused and
explicitly closed off as unsafe-to-extend in sub-sessions 13/19/20** (see "CLOSED DEAD END" section
below — three separate fix attempts at extending step-bound coverage all caused worse crashes,
including one proven via diagnostic instrumentation to heal to a WRONG address and crash anyway).
Xwayland forks internally (X servers commonly fork a helper/logging or become session-daemon-like)
much the same way `dbus-daemon --fork` did — but unlike dbus-daemon, there is no `--nofork`-style
flag for Xwayland to sidestep its own fork. **Do not re-attempt extending
`MAX_THREAD_VERIFICATION_STEPS` or keeping `AddressRelocations` alive past the bound in any form —
this has been tried three times already and is proven unsafe** (false-positive `is_in_source` hits
against an expired map). This needs either (a) a fundamentally different fix to fork_verify's
architecture that doesn't share that failure mode (not yet designed), or (b) avoiding whatever
Xwayland-internal fork triggers it (not yet identified — unlike dbus-daemon, no obvious `--nofork`
equivalent flag is documented for Xwayland), or (c) reporting this precise, narrower blocker back
to the user: XFCE's Wayland-only components (xfsettingsd, possibly xfdesktop in Wayland-native
mode) may be reachable without Xwayland; only the GTK/X11 rendering path (xfce4-panel, and
xfdesktop's own X11-drawn desktop icons) strictly requires it.

**UPDATE (same sub-session, tested): there is no Wayland-only fallback.** Ran `xfsettingsd` +
`xfdesktop` (no `xfce4-panel`, no `--xwayland` at all — plain `weston --backend=drm-backend.so
--use-pixman`) for 90s. Result: BOTH fail immediately —
```
xfsettingsd: Unable to open display.
(xfdesktop:22): Gtk-WARNING **: cannot open display:
```
So this is not an `xfce4-panel`-only requirement — the entire XFCE stack tested (xfsettingsd,
xfdesktop) is built GTK/X11-first and requires a real `DISPLAY`, i.e. Xwayland, unconditionally.
There is no partial-XFCE-without-Xwayland path available with this rootfs/XFCE build.

**Also confirmed: the fork_verify heal-storm is NOT Xwayland-specific.** It reproduces in this
Wayland-only run too (313 stale-pointer WARN lines in the first ~17s), starting at t=1.2s —
BEFORE weston even launches. So the trigger is `dbus-daemon` and/or `seatd`'s own startup, not
anything Xwayland does internally. (`dbus-daemon --nofork` was already applied in this repro and
does NOT prevent it here — contradicts the sub-session-21 finding that `--nofork` "avoids the
fork-verify crash entirely"; more likely `--nofork` avoided ONE specific instance of the bug
[dbus-daemon's own daemonize-fork] but `seatd` or another descendant has its own unrelated fork
hitting the same underlying step-bound gap.) In this specific run the process did not crash via
OOM this time — it went permanently silent at t=17.04s (log stops mid-heal-storm, memory usage
flat ~1.4GB, PID still alive) and the outer `timeout 90` did not kill the Windows-native child
process (confirmed: PID was still running well past 90s wall-clock, had to be force-killed
manually via `taskkill`). This is a SEPARATE, also-unresolved reliability gap: `timeout N` +
`litebox_runner...exe` does not reliably enforce N seconds when the guest is wedged — worth a
`prd-add` row of its own (likely `timeout`'s SIGTERM not reaching the actual Windows process tree,
or the runner process ignoring/not translating it) but out of scope for the immediate goal.

**Conclusion for whoever picks this up next**: the real remaining blocker is `fork_verify`'s
step-bound gap itself — general, not compositor- or Xwayland-specific, and already proven (3
independent attempts, this session) unsafe to patch by extending step bounds or keeping the
relocation map alive past the bound. Reaching "XFCE starts flawlessly" requires either (a) a
genuinely different fork_verify architecture (not yet designed — the AV-path healing mechanism
itself is sound for ITS narrow cases, the problem is specifically the unbounded case once
single-stepping disarms), or (b) precisely identifying which single fork (dbus-daemon post-
`--nofork`? seatd? something else in the chain?) is hitting it in THIS repro and finding a
targeted avoidance for that one process the way `--nofork` avoided dbus-daemon's daemonize-fork —
NOT yet done for whatever is triggering it now. Do not attempt a 4th step-bound-extension patch;
it will very likely fail the same way the first 3 did.

## Bisection results (same sub-session, later): the trigger is UNIVERSAL, not process-specific — and the loop is per-process-lifetime, not per-fork

Isolated each of the three candidates individually against a clean repro:
- `seatd` alone (no dbus at all): heal storm fires (263 events), process hangs indefinitely
  (never reached its own 30s completion echo, force-killed after 90s+ wall clock).
- `dbus-daemon --nofork` alone (no seatd): heal storm ALSO fires (133 events, identical repeating
  `rip=30257409`/`translated_rip=31299834` pattern every run), process ALSO hangs indefinitely.
  **This directly contradicts the sub-session-21 "`--nofork` avoids the fork-verify crash
  entirely" finding** — re-tested against BOTH `xfce-layer18.tar` and the original
  `xfce-layer17.tar` (ruling out a layer18/weston-fix regression) with byte-identical results on
  both. Sub-session 21's success was very likely evaluated on a shorter/less-scrutinized run, or
  the specific downstream symptom it checked (xfsettingsd's D-Bus connection succeeding) can occur
  even while this heal storm is silently ongoing in the background.
- `dbus-uuidgen --ensure=...` ALONE (no dbus-daemon at all — just the one-shot helper binary that
  runs BEFORE dbus-daemon in every repro so far): heal storm fires too (56 events) — same
  mechanism, definitively proving this is not dbus-daemon-specific either. **Critically, this run
  actually COMPLETED** (reached its own echo'd completion marker) rather than hanging.

**Refined understanding**: the fork_verify AV-heal mechanism fires on essentially any fork+exec in
this rootfs (confirmed now: dbus-uuidgen, dbus-daemon, seatd — 3 for 3) and is NOT inherently fatal
— `dbus-uuidgen`, a short-lived one-shot binary, forks, heals, and exits cleanly within under a
second with zero lasting harm. The catastrophic outcomes (OOM / permanent hang) only appear with
`dbus-daemon` and `seatd`, both LONG-RUNNING daemons that stay resident after forking. This
strongly suggests the heal overhead or some related resource (likely the relocation map itself, or
per-step trap/exception-handling cost) is not bounded by the fork event but continues accruing for
the entire remaining lifetime of the forked process — consistent with, but more precisely scoped
than, the original step-bound hypothesis from sub-sessions 13/19/20. A daemon that forks once and
then runs for the rest of the session's duration pays this cost forever; a one-shot helper that
forks and exits in under a second does not live long enough to hit the wall.

**Implication for next steps**: this makes the underlying bug MORE tractable, not less — the
question is no longer "which process triggers it" (all of them do) but "why does the AV-heal cost
never terminate for a long-lived forked process, when it clearly resolves fine for a short-lived
one." That is a real, scoped question for whoever redesigns fork_verify next, but per explicit
user instruction this session did not attempt a 4th patch to the mechanism itself — this section
only narrows the diagnosis.

## ROOT CAUSE, confirmed by reading `on_single_step`/`begin()` directly (same sub-session, no code changed)

Read `fork_verify.rs`'s actual step-bound logic (lines ~620-639, 2005-2011) to explain the
bisection results precisely, without patching anything:

- `tls.fork_verify_step_count` resets to 0 in `begin()`, called fresh on every `fork()`.
- `on_single_step` increments it every trap and, once it exceeds `MAX_THREAD_VERIFICATION_STEPS`
  (16384) or `MAX_IDENTITY_VERIFICATION_STEPS` (4096), sets `tls.fork_verify = None` and clears
  `EFLAGS.TF` — this correctly, deliberately ends verification (both the single-step path AND the
  AV-heal path in `lib.rs`, which also gates on `tls.fork_verify.borrow().as_ref()`) rather than
  looping forever in the fork_verify machinery itself.
- BUT the module's own doc comment for this bound already says plainly: "ending verification early
  is NOT known to make such a loop itself terminate" — it only stops the *verification overhead*
  from compounding an already-hung/broken child, it does not un-stick the child.

**This is exactly what the bisection observed**: the repeating identical `rip`/`translated_rip`
pairs (hundreds of times, always the SAME pair, e.g. `140668768385452 → 694135212`) are a real
guest-level infinite loop — the child keeps re-executing the same faulting instruction because
whatever it's looping on never resolves, not because fork_verify is failing to heal it (it heals
the SAME slot successfully every single time, that's why the same "success" line repeats
verbatim). At roughly hundreds-of-microseconds per single-step Windows-exception round-trip,
16384 steps takes on the order of several seconds to ~10s — consistent with every observed hang
(silent stop between t=6s and t=20s across all bisection runs) — after which verification ends
itself cleanly, but the child is already permanently wedged in its own loop and never recovers,
which is why the process goes silent forever instead of crashing OR completing.

**This means the real bug is NOT in fork_verify's step-bound logic at all** — that logic is
already working exactly as designed and documented. The real bug is a genuine LiteBox-emulation
gap causing the CHILD to enter an infinite loop after a stale pointer heals "successfully" but
something about the guest's subsequent state is still wrong (a value fork_verify has no case for:
neither a stale code pointer nor a stale memory operand, but something else entirely — a stale
FD, a stale futex/synchronization primitive's value, a signal mask, or similar non-pointer state
`PageManager::duplicate`/`fork_verify` were never designed to fix, since fork_verify's own module
doc explicitly says it repairs ONE narrow class of bug and nothing else). **This is a NEW,
previously-unrecognized class of post-`fork()` corruption, distinct from the stale-pointer class
fork_verify already handles** — likely specific to long-running daemons that fork and then loop
(dbus-daemon's/seatd's event loops) rather than fork-then-immediately-execve (the case this
module was designed and proven correct for).

**Next real step for whoever picks this up**: identify what specific non-pointer guest state is
wrong post-fork by live-debugging ONE of the repeating loop iterations directly (e.g.
`LITEBOX_VEH_TRACE=1` plus manually decoding the loop body at the repeating `rip` to see what
condition it's testing and why it never becomes false) rather than assuming it's another
stale-pointer case fork_verify's existing mechanisms could heal — the healing IS succeeding on
every iteration; the loop's exit condition itself is what's broken.

**CONFIRMED (same sub-session, further testing): this is a genuine indefinite hang, not just
slow verification.** Re-ran the `seatd`-only bisection with a 60s timeout (vs. the original 20-
30s) specifically to rule out "it just needs more time" — the process was STILL alive and STILL
stuck emitting the identical repeating heal pair 85+ seconds into wall-clock time (well past
where `MAX_THREAD_VERIFICATION_STEPS`=16384 should already have fired and ended verification
long ago), had to be `taskkill`-ed manually; `timeout 60` never killed it either (same
outer-timeout-doesn't-reach-the-Windows-process-tree gap noted earlier). This is real, not an
artifact of an impatient bisection window.

**Also notable and possibly a real clue**: a parallel `LITEBOX_VEH_TRACE=1` capture of the SAME
`seatd -l debug` repro (with the extra per-instruction eprintln overhead VEH_TRACE adds) did NOT
reproduce the stuck loop at all in 8s/~19500 traps — `rip` advanced steadily through real code and
seatd printed `"seatd started"` (success!). This strongly suggests the underlying bug is
timing/scheduling-sensitive: the extra host-side overhead VEH_TRACE adds per single-step
(eprintln, syscalls) changes the relative timing enough to avoid whatever race or non-deterministic
condition the bug depends on — consistent with the earlier hypothesis of stale non-pointer guest
state (a futex, condvar, or similar synchronization primitive) rather than a pure pointer issue,
since synchronization bugs are exactly the class of bug that timing changes can mask. Reproducing
under `LITEBOX_VEH_TRACE=1` reliably is therefore NOT a safe way to "test" a fix — a fix must be
verified with tracing OFF, at realistic timing, or it may appear to work while the underlying race
is merely being timing-masked again.

## MUCH more precise minimal repro found (same sub-session, continued) — narrowed from "seatd hangs" to an exact shell construct

Bisected further by stripping the repro down piece by piece (`LITEBOX_LOG=info`, no VEH_TRACE, in
every test below — matters, see above):

- `sleep 10` alone: completes fine (17 heal events, one fork).
- `seatd -l debug &` then `sleep 8`: completes fine (121 heal events, ~3 forks).
- `seatd -l debug &` then a `for i in 1 2 3; do sleep 1; done` loop, no test command: completes
  fine (175 heals).
- `seatd -l debug &` then `for i in 1 2 3 4 5; do [ -S /run/seatd.sock ] && break; sleep 1; done`
  (5 iterations, `[` test present): completes fine (212 heals) — breaks out on iteration 1 since
  the socket is already up.
- `seatd -l debug &` then the SAME loop with `1 2 3 4 5 6 7 8 9 10` (10 iterations available, but
  should still break after iteration 1 since the socket appears fast) followed by `echo LOOP_DONE;
  sleep 15; echo TRAIL_DONE`: **`LOOP_DONE` prints, but the subsequent `sleep 15` hangs
  indefinitely — `TRAIL_DONE` never prints.** (291 heal events before going silent.)

**Exact minimal reproducing shell command** (everything before this is confirmed NOT sufficient
on its own):
```sh
seatd -l debug & for i in 1 2 3 4 5 6 7 8 9 10; do [ -S /run/seatd.sock ] && break; sleep 1; done; echo LOOP_DONE; sleep 15; echo TRAIL_DONE
```
Note this is IDENTICAL in shape to the `1 2 3 4 5` variant that works — the only difference is the
loop's iteration LIST going up to 10 instead of 5, even though the loop still only actually runs
once (breaks immediately both times, since the socket is up well before either loop's first
`sleep 1`). This means the bug is NOT about how many times the loop body executes — it's
triggered by something in how `ash` sets up/tears down the `for i in <a long literal list>` word
list itself, or a subtly different fork/exec count for the longer literal argument list, before
the loop even runs its body. Confirmed reproducible twice in a row with the same exact command
(not a one-off fluke).

**Correction after further bisection (word-list length is NOT a strict threshold)**: tested 7 items
(242 heals, `TRAIL_DONE` printed, fine) and 8 items (`TRAIL_DONE` printed, fine) — both pass. Then
RE-RAN the exact 10-item command a third time: hung again (`LOOP_DONE` only, no `TRAIL_DONE`),
confirming it is reproducible specifically at 10 items across 3/3 runs while 5, 6, 7, and 8 items
are 1/1 clean each. This is NOT a strict "N items breaks it" threshold — no fork ever executes the
loop body more than once in any of these tests (the socket is always already up, so `[ -S ... ] &&
break` fires on iteration 1 regardless of list length) — so the bug is not about loop iteration
count at all. The most likely remaining explanation: `ash`'s parse/exec setup cost for a longer
literal word list is itself slightly larger (more argv strings to allocate/copy before the loop's
first iteration even runs), and that small extra amount of work is enough to shift timing into
whatever race window the bug depends on — consistent with the earlier `LITEBOX_VEH_TRACE` masking
observation (more host-side overhead === more likely to avoid the race, in both directions: a
LONGER list gives more real opportunity for the race to fire, while VEH_TRACE's per-instruction
logging overhead is enough to consistently avoid it entirely).

**Assessment**: this is a genuinely timing/scheduling-sensitive race, not a deterministic logic
bug triggered by a specific shell construct — the shell construct only matters insofar as it changes
timing. It is independent of seatd/dbus/weston as subject matter (any long-enough sequence of
forks appears sufficient) and most likely lives in `fork()`/`clone()`'s interaction with something
scheduling-sensitive: a genuine host-side race between fork_verify's single-step/AV-heal machinery
and the guest thread's own progress, OR corrupted/leaked bookkeeping in litebox's own SIGCHLD/reap
path (`litebox_shim_linux/src/syscalls/process.rs`, "reap_cross_process_child" and related, grepped
but not yet read in full this session) that only manifests once enough fork+exit cycles have
accumulated. Per the user's explicit "we wouldn't expect battle-tested alpine to have huge issues"
skepticism-of-upstream-blame standard from earlier this session, litebox's own emulation remains
the correct default hypothesis, not `ash`. NOT yet fixed — this session stopped at this precise,
mechanically-reproducible-3/3-times-at-10-items repro (safe to hand to a future session or a fresh
diagnostic pass) rather than risk a 4th speculative patch to `fork_verify` itself, since the actual
defect may not even be in that module (it could be upstream, in `sys_clone`/`sys_wait4`'s own
bookkeeping, or a genuine host-side scheduling race in the single-step/exception-handling path
itself, which `fork_verify`'s heals would then just be a symptom of, not the cause).

---

# (below: prior sub-session 22 handoff, preserved verbatim)

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

## FIX APPLIED (sub-session 23, final): case (1b), register-to-register stale-pointer propagation — resolves the xfsettingsd SIGSEGV

Found that a previously-drafted-but-never-applied fix (`.gm-scratch-fork-verify-fix.patch`, from
an earlier sub-session's dbus-daemon SIGSEGV investigation, saved to scratch but never landed)
matches the exact bug class behind the `xfsettingsd` SIGSEGV documented above: a bare `mov reg,
reg`/`movzx`/`movsx` with NO memory operand propagates a stale source-range pointer from one
register to another with nothing to trip any existing case (1)/(2)/(2b)/(2c)/(2d)/(3)/(4), ALL of
which require either `rip` itself to be stale or an `OpKind::Memory` operand on the instruction.
This precisely explains the ~944ms gap observed between the last logged heal and the SIGSEGV: the
stale register sits unused and unobserved until a later, unrelated instruction dereferences it.

Applied as case (1b) in `on_single_step` (`litebox_platform_windows_userland/src/fork_verify.rs`),
positioned AFTER instruction decode (the saved patch's line numbers assumed an older file layout
and did not compile as-is — had to move the block from before decode to after decode/validity
check, then verified via `cargo check -p litebox_platform_windows_userland`, clean). Narrow and
safety-gated identically to the original patch's own reasoning: only `Mov`/`Movzx`/`Movsx` (not
`Test`/`Cmp`/`Xor`), only `op1` (source) ever read/translated (never `op0`, the write-only
destination), requires `MIN_POINTER_ALIGN` on the source value, requires NO memory operand
anywhere on the instruction (so it never double-fires with a case below that already handles the
memory-operand form).

**Post-fix verification**: `cargo build --locked --release -p litebox_runner_linux_on_windows_userland`
succeeded. Full repro re-tested at `LITEBOX_LOG=debug`, 60s timeout (same log level as the run that
originally found the crash): **all three XFCE components genuinely `sys_execve`** at t=17.4-17.42s
(`xfsettingsd` tid=21, `xfce4-panel` tid=22, `xfdesktop` tid=23) — **zero fatal signals for the
entire 60s run** (process ended via `exit 124`, killed by the outer `timeout`, NOT a crash) — and
at t=53.6s, ~36 seconds after its own execve, **`xfce4-panel` (tid=22) was still alive and
actively executing real syscalls** (dynamic library loading, `mprotect` calls) — genuine, sustained
post-launch activity, not a stall. This is the cleanest, furthest-progressed, longest-surviving
result this entire investigation has produced, and the exact SIGSEGV this fix targets has not
recurred in any post-fix run.

## RESOLVED (same sub-session, final): the "freeze" was never a bug — it was misdiagnosed idle state; the real remaining defect was a second stale-pointer gap (`lea`), now also fixed

Investigated the `LITEBOX_LOG=info`/`warn` "freeze" directly with `gdb` (Windows-native, attached
to the live frozen process) instead of more in-guest tracing, specifically to break the pattern of
every diagnostic tool this session tried perturbing the very timing being investigated. Built a
`x86_64-pc-windows-gnu`-target release binary (DWARF debug info gdb reads natively — the default
MSVC-target build only carries a `.pdb`, which gdb cannot resolve, hence every earlier attempt at
symbolizing the frozen stacks failed with `??`).

**`thread apply all bt` on the "frozen" process showed every single guest thread legitimately
blocked in real Linux syscalls** — `sys_futex` (`FutexManager::wait`), `sys_epoll_pwait`
(`EpollFile::wait`), `sys_ppoll` (`PollSet::wait`) — all via the correct `WaitOnAddress` path, and
the runner's own `main` thread was simply doing an ordinary `std::thread::Thread::join()` on a
guest worker thread (the normal "wait for workers to finish" pattern, not a deadlock indicator).
**This is not a hang. It is the system correctly reaching a quiescent idle state** — exactly what a
real, successfully-started desktop session looks like once every component has started and is
waiting for an event (D-Bus message, X11 input, a timer) that never arrives in this headless,
input-free sandbox. The "log goes silent" observation that drove the entire "freeze" investigation
this session was a correct observation of an INCORRECT conclusion: no new syscalls happen because
there is genuinely nothing new to do, not because anything is stuck.

**However, this same gdb session's host-side log (kept running throughout, `LITEBOX_LOG=warn`)
showed the fix above did NOT fully resolve the SIGSEGV** — the exact same `tid=21`/`rip=419518714`
`/419518956` crash signature recurred once more, proving case (1b) closed only part of the gap.
Root-caused precisely: **`lea dest, [base+disp]` never dereferences memory** (confirmed via
`iced_x86::InstructionInfoFactory::used_memory()`, which reports zero memory access for `lea`), so
`memory_write_address` (which requires a real memory access) always returns `None` for it, forcing
it into case (2b)'s branch -- but case (2b)'s own gate checks `is_in_source` on the COMPUTED
`base+disp` effective address, not on the base register's raw value. Whenever `disp` is nonzero
(the common shape: `lea rdi, [rbx+0x18]`, indexing into a struct field from a stale base), the
computed address need not itself land in a tracked source range even though `base` genuinely does
(`AddressRelocations`' source ranges are the parent's real, bounded pre-`fork()` mappings, not an
unbounded span) -- so case (2b) silently never fires for this exact shape, and the stale value
`lea` computes from the untranslated base propagates onward uncaught, exactly reproducing case
(1b)'s own "delayed by real execution time" symptom.

**Fix**: added case (1c) to `on_single_step` -- gates on the `lea` instruction's BASE register's
own raw value being a genuine, aligned `is_in_source` hit (not the computed effective address),
translates just that base register, and retries so the CPU recomputes `base+disp` itself with the
corrected base. Narrow and safety-gated identically to every other case in this file (only the
named base register read/translated, `MIN_POINTER_ALIGN` required, no index-register handling
since no such shape has been observed).

**Post-fix verification, definitive**: `cargo build --locked --release` succeeded;
`cargo test -p litebox_platform_windows_userland` passes (4/4, baseline unaffected). Full repro at
`LITEBOX_LOG=debug`, 60s timeout: **all three XFCE components genuinely `sys_execve`** at
t=17.35-17.36s (`xfsettingsd` tid=21, `xfdesktop` tid=23, `xfce4-panel` tid=22) — **zero fatal
signals for the entire 60-second run** (`exit: 124`, killed by timeout, not a crash) — and at
t=52.47s, ~35 seconds after its own execve, **`xfce4-panel` (tid=22) was still alive and actively
executing real syscalls** (dynamic library `mprotect` calls, real ongoing work) — reproducibly
clean, no crash, sustained multi-component survival. A companion `LITEBOX_LOG=info` 150s run also
showed zero fatal signals for the full duration before being manually terminated (confirmed
correctly idle via `gdb`, not stuck).

**Status**: the specific SIGSEGV chased across this entire sub-session (dbus-daemon's original
manifestation, then `xfsettingsd`'s) is fixed by the combination of case (1b) (register-to-register
propagation) and case (1c) (`lea` base-register propagation) -- two related but distinct gaps in
the same class of bug, both now closed with real evidence, no code left un-verified. The
"freeze"/"hang" framing that dominated much of this sub-session's middle section was a genuine
misdiagnosis (confirmed via live `gdb` inspection, not assumed) -- future sessions should default
to attaching a debugger to an apparently-stuck LiteBox process BEFORE concluding it is hung, since
"log went quiet" and "genuinely deadlocked" are trivially confused without doing so, and this
session lost significant time to that exact confusion.
