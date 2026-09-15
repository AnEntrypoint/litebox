# AGENTS.md archive — detail drained 2026-09-16

One drain pass today: `AGENTS.md` crossed the 30KB threshold again after two commits were appended
directly on top of yesterday's `f1c17b8` recompaction without compacting — the eleventh popup-menu/
Terminal-Emulator investigation (`3a312f0`) and the `spawn_exec_collision_child` selkies-boot-hang
root-cause-and-fix (`42d8ced`). This file holds the full repro/methodology/log detail drained out of
both. Nothing still-open lives only here: the popup-menu/Terminal-Emulator symptom's still-unconfirmed
status, the still-open Track B single-shared-address-space collision, and the still-separate
ADVISORY-001 §3N tcache class all stay stated in `AGENTS.md` itself.

Everything below carries its proving commit sha or `file:line`. Earlier drains:
`docs/AGENTS_ARCHIVE_2026-09-15.md`, `_2026-09-10.md`, `_2026-09-05.md`, `_2026-09-03.md`.

## Popup-menu/Terminal-Emulator re-test: architecture read + boot-reliability blow-by-blow (2026-09-15 session, `3a312f0`)

**2026-09-15, later session: could NOT re-confirm the popup-menu symptom live — blocked before reaching
it by two boot-reliability regressions, one found and fixed, one found and NOT fixed (at the time —
see the next section for the fix that landed the next day).** Architecture read first, no code changed
on guesswork: `webtop_stack.sh`'s guest is plain `Xvfb` (`+extension XTEST`) + XFCE + `selkies`, and
selkies' own `input_handler.py` (`WebRTCInput.send_x11_mouse`/`send_mouse`) drives mouse position/clicks
through `pynput.mouse.Controller`'s Xlib backend, which is `Xlib.ext.xtest.fake_input` under the hood —
an ordinary X11 client issuing real XTest protocol requests over its own AF_UNIX socket to Xvfb, exactly
like any other `XTestFakeMotionEvent`/`FakeButtonEvent` caller. **litebox implements no grab-specific or
menu-specific code anywhere in this path** — `litebox_shim_linux::syscalls::evdev` (`/dev/input/event0`,
`EV_REL`-only push model) is a DIFFERENT subsystem for a native-DRM/uinput desktop shape this webtop
deployment never uses; the generic `Pollee`/`Observer` notifier (`litebox/src/event/polling.rs`) and the
AF_UNIX socket implementation (`litebox_shim_linux::syscalls::unix`) both call `notify_observers`/
`register_observer` correctly on every state transition (unlike the already-fixed "only wakes on the
first event" bug class `evdev.rs`/`drm.rs` document), and `sys_ppoll` (`litebox_shim_linux::syscalls::
file.rs:5671`) rebuilds a fresh `PollSet` and rescans real fd state on every call — level-triggered, not
edge/observer-only, so it cannot exhibit a "the byte arrived but nobody woke up to read it" gap for a
GLib/GTK poll()-based main loop. **If a real litebox defect explains the menu symptom, by this reading it
has to be in generic syscall-emulation correctness (AF_UNIX ordering, timestamp/clock semantics X11 grabs
validate against, or something not yet identified) — not a menu-specific code path, because litebox has
none.** This narrows future search; it does not confirm or refute the symptom itself, which still needs a
live re-test.

Live re-test was blocked before reaching the Applications menu at all, across 15 full boot cycles of
`--resume-from .wfgy/webtop_stack_seed.tar` this session:

1. **Fixed**: `.wfgy/webtop_stack.sh`'s nginx supervisor retry block only recreates
   `/var/lib/nginx/{tmp,logs,body,proxy,fastcgi,uwsgi,scgi}` when `/etc/nginx/sites-enabled/default` is
   missing — but nginx's OWN first-launch `mkdir() "/var/lib/nginx/body" failed (2: No such file or
   directory)` reproduced 22/22 times regardless (sites-enabled/default already existed from the
   top-of-script setup, so the gated recreate never fired to paper over it), forcing every boot into the
   supervisor's fork-heavy respawn loop (mkdir/openssl/nginx/curl×20) before Xvfb ever started — and that
   fork storm hit the already-documented ADVISORY-001 §3N glibc safe-linked tcache/fastbin double-free
   (`double free or corruption (out)` → `SIGABRT`/`SIGSEGV`, killing the whole guest) on 22 of 22 boots
   this session, far above this bug's historically-documented "sporadic, once in several cycles" rate.
   Root cause of the mkdir ENOENT itself not identified (a real candidate: a forked `mkdir` utility's
   directory creation not yet visible to a separately-forked `nginx` process under litebox's thread-based
   fork — worth a follow-up, not chased further this session), but the fix doesn't need that: recreating
   those dirs unconditionally right before every nginx launch attempt (not gated on sites-enabled/default)
   made nginx succeed on attempt=1 and skip the fork storm entirely. This is a `.wfgy/`-local repro-script
   fix, not a litebox source change (`.wfgy/` is gitignored, confirmed via `git ls-files` — nothing to
   commit), but it took boot success (reaching `SELKIES_LAUNCHED_LAST` with zero crashes) from 0/22 to
   3/5 on the patched seed. Also swapped the script's `tail -F /tmp/sk.log` (retry+inotify) for `tail -f`
   (polling): litebox has no `inotify_init`/`inotify_init1` (`unsupported syscall`, confirmed live on
   every boot), and GNU `tail -F` silently never notices new data when that syscall is refused rather than
   falling back to polling — this made every prior session's `[sk]`-tagged selkies log tee a silent no-op.
2. **NOT fixed at the time of this session, later root-caused and fixed — see the next section**: even
   on a clean boot (`XVFB_UP`, `DE_UP`, no crash), selkies itself never served. `curl`'s own
   WebSocket-upgrade probe against `http://127.0.0.1:3000/websockets` (the dashboard's real,
   confirmed-correct endpoint — verified via the browser's own console: `WebSocket connection to
   'ws://127.0.0.1:3000/websockets' failed ... 404`) returned `404` every time, with ZERO `[sk]`-tagged
   output ever appearing even with the `tail -f` fix active, across all 3 clean boots reached this
   session. One boot's stderr trace showed the mechanism: at t≈175s (selkies apparently still
   initializing) a `spawn_exec_collision_child` event fired (matching this project's own documented
   collision class — most likely selkies' `gst_app_resize`/xfconf-query DPI-set fork, already implicated
   elsewhere for a different, sporadic SIGSEGV), fork_verify logged a burst of stale CODE/DATA-pointer
   heals in response, and selkies exited `rc=1` roughly 15s later with no logged reason — the supervisor
   then respawned it into the same failure. `docs/webtop-debian-selkies-2026-09-06.md` documents an
   apparently-related, 100%-reproducible prior bug (a proxied `location` deterministically gets
   `connect() refused` — masked as a `404` by a missing `50x.html` — if and only if the ORIGINAL client
   request arrived via the `-p`-published NAT path, never via a same-guest loopback probe); this session
   could not distinguish "same bug, still unfixed" from "a new, DPI-fork-triggered selkies startup
   failure" without a working internal-vs-external curl comparison (attempted once via an in-script
   background probe subshell; it never printed even its first iteration in 300+s and was reverted rather
   than trusted or chased further).
3. Xvfb's own `XVFB_FAILED` rate was also unusually high this session (4 of the last 6 boot attempts) —
   consistent with, but not confirmed as, the already-documented Mesa llvmpipe/`cc1` fixed-address
   collision race (`LIBGL_ALWAYS_SOFTWARE=1`/`GALLIUM_DRIVER=softpipe` already applied); not investigated
   further since it was not this session's blocker (the 3 clean boots reached DE_UP fine).

**Net effect at the time**: the popup-menu/Terminal-Emulator symptom was UNCHANGED from the prior
session's finding — neither newly confirmed nor refuted live that session — and no litebox source code
was changed, per this project's own standing discipline against forcing an unverified fix. The one real
fix that landed (nginx dir-recreate race) is in `.wfgy/webtop_stack.sh` only and measurably improved boot
reliability, but did not itself touch litebox. Item 2 above was root-caused and fixed the next day — see
below.

## `spawn_exec_collision_child`: selkies-boot-hang root cause and fix, blow-by-blow (2026-09-16, `42d8ced`)

**The t≈175s `spawn_exec_collision_child` + selkies-never-binds mechanism, root-caused and fixed — the
trigger was NOT `gst_app_resize`/xfconf-query as guessed in the prior session's investigation above; it
is selkies' own interpreter re-exec, and the real defect was an unbounded blocking wait with no
fallback.** Live-reproduced (`.wfgy/boot_repro2.*`, `.wfgy/fix_verify_run1.log`) with
`LITEBOX_LOG=warn,…fork_verify=warn`, correlating `path=` on every `spawn_exec_collision_child` line
against the guest's own `[sk]`-tagged stdout:

- The collision is `path=/lsiopy/bin/python3` — selkies' shebang (`/lsiopy/bin/selkies` → `#!/lsiopy/bin/
  python3`) re-execs an `ET_EXEC` python3 at selkies' own launch, ~70-180s into boot, once nginx/Xvfb/dbus/
  xfce4-session's cumulative fork/exec history has filled enough of litebox's ONE shared host address
  space to collide with python3's fixed link address. The EARLIER, already-documented `cc1`/Mesa-llvmpipe
  collision (`~t=76s`, harmless, resolves in ~5s) is a separate, unrelated event that just happens to
  precede it by design (`.wfgy/webtop_stack.sh` starts nginx/Xvfb before selkies specifically to dodge that
  one) — do not conflate the two `spawn_exec_collision_child` events in a boot's log.
- `spawn_exec_collision_child` (`litebox/src/platform/mod.rs:1131`, impl `litebox_platform_windows_userland/
  src/lib.rs:9972`, called from `sys_execve` at `litebox_shim_linux/src/syscalls/process.rs:6075` on
  `Map(EEXIST)` after the point of no return) correctly avoids crashing the guest by spawning a fresh,
  genuinely separate `litebox_runner` process to run the colliding program and adopting its exit status —
  but it did so via a **plain blocking `cmd.status()` with no timeout**. That nested child is a completely
  isolated OS process with no shared AF_UNIX namespace with the ORIGINAL guest's already-running Xvfb/
  D-Bus (the same gap `docs/fork-fs-veh-2026-09-08.md:128-144` already documents for cross-process FORK
  children, now confirmed to also apply here) — selkies inside it cannot actually reach the desktop it's
  supposed to serve. Observed live consequences, both real, both reproduced: (a) the nested attempt can
  exit quickly with a real but degraded-environment failure (its own gcc/collect2 sub-step, itself another
  nested collision, returning `raw_status=1`); or (b) — the actual mechanism behind this row's original
  "selkies never binds, 404s forever" symptom — the nested child can sit at 0% CPU forever (most likely
  blocked on a `connect()`-then-`ppoll(timeout=-1)` against an unreachable socket path, the exact
  `dbus-daemon --fork` hang class this project already knows), and since the calling guest thread blocks on
  it UNCONDITIONALLY, this wedges the ENTIRE guest boot — the top-level shell's own supervisor loop never
  sees `selkies` exit, so it never respawns, and the whole `.wfgy/webtop_stack.sh` `HOLD` loop just ticks
  forever over a dead boot. Live-witnessed: 7+ minutes at 0.06s total CPU, `Get-Process` confirmed, until
  manually killed.
- **Fixed** (`litebox_platform_windows_userland/src/lib.rs`, `spawn_exec_collision_child`): replaced the
  blocking `cmd.status()` with `cmd.spawn()` + a poll loop using the SAME "genuinely wedged, not just slow"
  CPU-progress check `process_fork::run_external_fault_watchdog_child` already uses and this project already
  trusts for the identical judgment call (measured on the CHILD's handle from a genuinely different
  process, never the self-measurement that function's own doc comment already found unreliable) — a 20s
  no-CPU-progress stall grace, and a 120s absolute cap regardless of progress. Timing out kills the child
  and returns `None`, which is exactly the existing, already-correct spawn-failure fallback (kill this ONE
  guest process with `SIGSEGV`; its own supervisor loop already respawns it) — changes nothing for the
  overwhelmingly common case where the child actually exits.
- **Live-verified the fix actually fires and recovers**, not just compiles: re-ran the identical repro on
  the patched binary (`.wfgy/boot_repro3.*`). The SAME `/lsiopy/bin/python3` collision occurred (t=161.7s
  this run — this class is inherently non-deterministic run to run, expected), its nested child again made
  some CPU progress but never exited; at t=281.9s (exactly 120.1s later) the absolute cap fired —
  `spawn_exec_collision_child: replacement process exceeded the absolute time cap … killing it` — the
  original guest thread then took the pre-existing `killing process with SIGSEGV tid=211 path=/lsiopy/
  bin/python3` fallback, and the boot's own supervisor loop printed `SELKIES_SUPERVISOR: attempt=… exited
  rc=… -- respawning` and kept going instead of hanging. `Get-Process` after the fix's cap fired showed only
  the one main runner process alive (no orphaned/zombie nested child), confirming `child.kill()`+`child.
  wait()` clean up correctly.
- **What this fix does NOT close**: the underlying single-shared-address-space collision itself (Track B,
  already extensively documented, multi-session-scale infra work) is unchanged — selkies' python3 re-exec
  can still collide, and when it does, the nested recovery attempt still cannot reach Xvfb/D-Bus, so it
  still very likely fails or times out (now bounded at ≤120s instead of forever). A separate, pre-existing,
  already-documented bug (ADVISORY-001 §3N glibc tcache/fastbin corruption, `[sk] Segmentation fault`
  `rc=139`) also still fires independently on some selkies (re)launches, unrelated to this fix. **The fix's
  scope is precisely**: convert an unbounded, unrecoverable, whole-boot hang into a bounded failure the
  existing supervisor-respawn loop already knows how to recover from — a real, live-confirmed reliability
  improvement, not a claim that the collision itself no longer happens.
- Boot-reliability numbers, repeated live boots, `.wfgy/webtop_stack.sh --resume-from .wfgy/
  webtop_stack_seed.tar`: pre-fix, one live run hit the unbounded hang directly (0/1 that run, needed a
  manual kill after 7+ minutes with zero progress) — consistent with this row's own prior-session 3/5
  "clean boot but selkies never binds" characterization, since an unbounded hang and a `404`-forever boot
  are the same underlying defect, just differing in whether the specific run's nested child fully wedges or
  merely fails slowly. Post-fix, two live runs both avoided the hang: one recovered via the bounded fallback
  and kept cycling through the supervisor loop (never reached a fully clean `Data WebSocket Server
  listening` state in the observation window, blocked by the separate, pre-existing tcache-corruption
  respawn loop above); commit `42d8ced` has the fix. This is a real, measured improvement (a boot that used
  to need a manual process kill now self-recovers), not yet a claim of 100% clean-boot reliability — the
  tcache/fastbin corruption class remains this project's next blocker for a fully clean boot, tracked
  separately (ADVISORY-001 §3N).
