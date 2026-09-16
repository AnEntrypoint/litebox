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

## Masked-502/404 root-caused live: a startup race + ADVISORY-001 §3N, NOT a NAT/net.rs bug (2026-09-16, later session)

**Task**: get the REAL error under the masked `404` on `/websockets` (previously only inferred from a
2026-09-06 capture, unconfirmed since), correlate its timing, and determine litebox-vs-script-vs-other.
Re-read `litebox_platform_windows_userland/src/net.rs` in full (1068 lines, still structurally clean, matches
the 2026-09-07 finding) before touching anything live.

**Live capture, this session (`.wfgy/webtop_stack_natdiag.sh` variants — nginx `error_log` tailed live,
never done before): a genuine, real `connect() failed (111: Connection refused)` to `127.0.0.1:8081`,
captured twice, on two independent boots, from BOTH a guest-internal probe (`client: 10.0.0.2`) and a real
`-p`-published external probe (`client: 10.0.0.1`, matching `net.rs`'s documented gateway-side ephemeral
endpoint).** This is decisive: `net.rs`'s own `send_ip_packet` loops any `127.0.0.0/8`-destined packet
straight back into the guest's receive queue, bypassing the NAT gateway/real-socket-bridge code entirely —
confirmed by this exact line (`litebox_platform_windows_userland/src/net.rs:1028-1037`) and now independently
proven live: the identical `ECONNREFUSED` occurred via a path (guest-internal curl) that never touches the
gateway at all. The `-p`-vs-loopback framing every prior session (2026-09-06 through today) carried is
**retired** — there is no NAT-path-dependent bug, and there never was; `net.rs` is cleared for the third
time, now with live proof instead of code-reading alone.

**What the ECONNREFUSED actually is**: a plain startup race, plus the already-tracked ADVISORY-001 §3N crash
class hitting selkies itself:
- Boot 1: probed `/websockets` within ~seconds of `SELKIES_LAUNCHED_LAST` (before selkies' own Python
  interpreter/import cost could possibly have reached `bind()`/`listen()`) — genuine refusal, both internal
  and external.
- Boot 2: selkies logged its own `INFO:data_websocket:Data WebSocket Server listening on port 8081` at
  guest t≈130-140s, i.e. selkies genuinely bound — and an external `-p` probe issued shortly after still got
  a real `502` (nginx's error log showed the identical `connect() failed (111: Connection refused)`,
  `client: 10.0.0.1`). Selkies bound, then died, before that specific request landed. This is the SAME
  fork-corruption class already tracked project-wide (ADVISORY-001 §3N; the same class that killed the
  Selkies-supervisor subshell and the boot's own top-level `sh` pid 2 elsewhere this session — see below),
  not a new mechanism.
- A live process-table dump captured mid-boot2 (this session's own crash-time snapshot) confirmed
  `pid=184 ppid=164 comm=/lsiopy/bin/selkies` genuinely alive at that moment, and separately confirmed a
  `fatal signal: terminating task signal=Signal(11) pid=2` (the top-level guest shell) at t≈208s — a NEW
  witness of the standing tcache-corruption class hitting the boot script's own pid 2, not just selkies/
  nginx as previously documented.

**Real fixes landed, both `.wfgy/webtop_stack.sh`-only (gitignored; no litebox source change, nothing to
commit for these two)**:
1. **The masking itself, fixed**: `error_page 500 502 503 504 /50x.html` was firing correctly on every real
   upstream failure, but `/usr/share/selkies/web/50x.html` never existed in this script's setup (flagged as
   a "cosmetic, lower priority" fix back on 2026-09-06, never actually done until now) — so the real 502's
   own error page 404'd, and THAT was the status code every session since 2026-09-06 was chasing as if it
   were the primary symptom. Added a `printf`-written placeholder at setup time (no external fork). Live
   effect, confirmed this session: the exact same underlying `ECONNREFUSED` now surfaces as an honest
   `curl`-visible `502`, not a `404` — verified directly (`external_http_code=502`, not `404`, after the
   fix; `open() ".../50x.html" failed` no longer appears in nginx's error log after the fix, where it did
   before). This alone resolves the "confusing 404" framing that drove ten-plus sessions' worth of
   `-p`-path suspicion.
2. **A `SELKIES_PORT_UP` gate** after `SELKIES_LAUNCHED_LAST`: polls selkies' own port directly (bypassing
   nginx and the 50x.html masking) via `curl`'s EXIT CODE (7 = couldn't connect; anything else means
   something is genuinely listening) rather than `%{http_code}` — the first version of this gate used
   `http_code` and looped all the way to its cap every time even after selkies was confirmed listening,
   because selkies' raw WebSocket server does not necessarily answer a plain HTTP GET with a
   curl-parseable response before timeout, so `%{http_code}` reads "000" for BOTH "nobody home" and
   "connected fine, no HTTP reply" — a real, self-inflicted diagnostic bug, caught live (the `http_code`
   version ran all 60 iterations, ~150s, ~120 extra forks, and that specific extra fork pressure is
   plausibly what pushed the boot into the pid=2 SIGSEGV above — measure-changed-the-outcome, this file's
   own recurring lesson, striking its own diagnostic this time). Fixed to the exit-code check; cap raised to
   170s to match the real, live-measured ~100-140s selkies bind latency (a 20s cap, tried first, elapsed
   every time before selkies ever bound). **Scope, stated plainly in the script's own comment**: this gate
   closes the FIRST-bind race only. It cannot and does not close the separate ADVISORY-001 §3N crash class
   that can kill selkies (or nginx, or the script's own shell) moments after a successful bind — that
   remains this project's open, multi-session architectural blocker, unchanged by this session.

**Not reached this session**: a browser-verified stable connection long enough to retest the Terminal
Emulator/Applications-menu click path. Both live boots run to gather the above evidence were themselves
eventually lost to the ADVISORY-001 §3N class (one killed manually on a 5GB+-RSS/stalled-stdout pattern
matching this file's own already-documented bad sign; one ended in the pid=2 SIGSEGV above) before a
sufficiently long clean window opened for a real `claude-in-chrome`/`chrome-devtools` browser session. The
Terminal Emulator re-test via the real browser click path remains blocked on the SAME standing blocker
(ADVISORY-001 §3N boot reliability), not on the masked-502/404 investigation this session closes out.

**Bottom line for the next session**: stop treating `/websockets` 404s/502s as a networking investigation —
`net.rs` is cleared for good, live-proven twice more. Every remaining instance of this symptom is either (a)
a request that landed before selkies bound (now mitigated, not eliminated, by `SELKIES_PORT_UP`), or (b) a
selkies crash from the standing ADVISORY-001 §3N tcache/fastbin class. Fixing (b) at the root needs Track
B's cross-process kernel-state infrastructure (already the standing recommendation for the unrelated vfork
row) or a from-scratch investigation of why `GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0`
is an incomplete workaround under this much concurrent fork load — not a masked-502 question anymore.

## `GLIBC_TUNABLES` propagation through `spawn_exec_collision_child`: no gap, live-verified; the recurring crash is a second corruption signature (2026-09-16, later session)

**Task**: the from-scratch investigation the previous section above named as needed. Specifically: does
`42d8ced`'s new nested-litebox_runner collision-recovery path (added the same day as the tunable
workaround was first believed sufficient) silently drop `GLIBC_TUNABLES` somewhere in its fork/exec
chain — a very plausible regression vector, since it spawns a genuinely separate host OS process — or
does the workaround reach every process correctly and the crash class is simply not fully closed by it?

**Code-level read first, before touching anything live**: `sys_execve`
(`litebox_shim_linux/src/syscalls/process.rs:6058-6059`) clones `argv_vec`/`envp_vec` into
`argv_for_collision_retry`/`envp_for_collision_retry` BEFORE `load_program` consumes the originals, purely
for this hand-off — this clone is *the exact envp the failing `execve()` call itself carried*, not some
separately-reconstructed or ambient-host-env substitute. `spawn_exec_collision_child`
(`litebox_platform_windows_userland/src/lib.rs:9972` impl) then loops over every `envp` entry and adds it
as a `--env KEY=VALUE` flag to the nested `litebox_runner` invocation (`lib.rs:10035-10047`), with an
explicit doc comment already distinguishing this from the HOST process's own ambient environment (which
`Command` inherits by default, unconditionally, unrelated to this loop). Structurally, there is no gap: if
`GLIBC_TUNABLES` was present in the colliding process's own envp (which normal guest-level fork/exec
inheritance from `webtop_stack.sh`'s `export` on line 45 should guarantee, since that part is ordinary
Unix env inheritance, not exec-collision machinery), it reaches the nested child.

**Added a permanent diagnostic to convert this from a code-reading argument into a live fact on every
occurrence** (`litebox_platform_windows_userland/src/lib.rs`, `spawn_exec_collision_child`): a
`glibc_tunables_forwarded: Option<bool>` tracked across the `--env` loop, logged via one `warn!` per
collision (`path=`, `glibc_tunables_forwarded=true|false`). Rebuilt release (`cargo build --release -p
litebox_runner_linux_on_windows_userland`, 27.7s incremental).

**Live boot** (`.wfgy/envcheck_launch.ps1`, `--resume-from .wfgy/webtop_stack_seed_natdiag3.tar`,
`--env GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0`, `--publish 3000:3000`):
`.wfgy/envcheck_run1.log` recorded FOUR collision events, checked live via `Get-CimInstance Win32_Process`
parent/child confirmation (a real nested `litebox_runner.exe` child process, PID 16908, parented to the
main runner PID 14744) plus the new diagnostic line, for every one:

```
path=/usr/libexec/gcc/x86_64-linux-gnu/14/collect2  glibc_tunables_forwarded=true   (t=3.07s, nested clock)
path=/usr/bin/gcc                                    glibc_tunables_forwarded=true   (t=4.58s, nested clock)
path=/usr/libexec/gcc/x86_64-linux-gnu/14/cc1        glibc_tunables_forwarded=true   (t=76.27s)
path=/lsiopy/bin/python3                             glibc_tunables_forwarded=true   (t=116.20s)  <- selkies' own shebang re-exec, the exact case this investigation targeted
```

**Every single collision forwarded the tunable correctly, including the critical selkies case.** This
closes the "does `42d8ced` leak the workaround" question definitively: it does not. No fix was needed or
applied to the propagation path itself.

**The crash still happened anyway, same boot, ~2 minutes after the confirmed-forwarded python3
collision** — and this is the real finding. Sequence from `.wfgy/envcheck_run1.log`/`.out.log`:

1. `t=116.20s`: `/lsiopy/bin/python3` collision, nested child spawned (PID 16908 confirmed via
   `Win32_Process`), `glibc_tunables_forwarded=true`.
2. The nested child sat at ~0.05s total CPU (confirmed via `Get-Process -Id 16908`, unchanged across
   repeated checks) — the same "wedged, no shared AF_UNIX namespace with Xvfb/D-Bus" pattern `42d8ced`
   already documents.
3. `t=236.30s` (elapsed=120.0935317s after the collision, matching `42d8ced`'s absolute cap to the
   millisecond-scale): `spawn_exec_collision_child: replacement process exceeded the absolute time cap …
   killing it` — the `42d8ced` fix firing exactly as designed.
4. `t=236.41s`: the guest thread's existing fallback — `killing process with SIGSEGV tid=172
   path=/lsiopy/bin/python3` — fired correctly, matching `EEXIST`/point-of-no-return handling.
5. Immediately after: a bare `double free or corruption (out)` line (glibc's `malloc_printerr` message,
   unprefixed since it comes from the guest's own stdout/stderr, not a litebox log line), followed by
   `t=237.52s ERROR … fatal signal: terminating task signal=Signal(6) pid=170 tid=170 comm=[the raw byte
   sequence for "sh"]` — i.e. **the SELKIES_SUPERVISOR subshell itself (`supervisor_pid=170` from
   `SELKIES_LAUNCHED_LAST` in the stdout log) aborted via SIGABRT**, not SIGSEGV.

**This is NOT the same fault signature ADVISORY-001 §3N originally symbolized.** §3N's own
symbolization (`advisor/ADVISORY-001-fundamentals.md` section 3N) is specific:
`__libc_malloc+0x76`'s `xor (%rax),%rsi` — `tcache_get`'s `REVEAL_PTR` of a safe-linked `next` pointer,
raising **SIGSEGV** because the revealed "address" is a XOR-masked non-pointer, not a dereferenceable
address. `tcache_count=0`/`mxfast=0` exist specifically to take this exact instruction out of the picture
by forcing every free/alloc through bins that don't safe-link. `double free or corruption (out)` is a
categorically different glibc code path: it is `malloc_printerr`'s own message, raised by `_int_free`'s
(or `malloc_consolidate`'s) explicit consistency checks on a chunk's size/prev-size fields or a detected
duplicate free — a **SIGABRT**, not a page-fault SIGSEGV, and one that fires on the unsorted/small/large
bins specifically (the ones `tcache_count=0`/`mxfast=0` deliberately leave active, per §3N's own original
reasoning that those use "ordinary unmangled `fd`/`bk` pointers, which DO land in a source range and
which the existing relocation healing handles").

**Conclusion, evidence-based, not forced**: the `GLIBC_TUNABLES` workaround has no propagation gap
anywhere, including through `42d8ced`'s new nested-spawn recovery path, and is doing exactly the job it
was designed for (eliminating the specific safe-linked-pointer SIGSEGV). The crash class recurring today
is real, but it is a SECOND, related mechanism: under this much concurrent fork/exec pressure (nginx +
Xvfb/D-Bus + nested gcc/collect2/cc1 collisions + the selkies-supervisor retry loop, several of these
forking near-simultaneously), litebox's own thread-based relocating fork-healing does not reliably heal
every plain (non-safe-linked) heap pointer either — the exact "structurally unhealable... known,
documented, architecturally-understood gap" this dispatch's own brief named, just now confirmed to extend
beyond the safe-linked-pointer case specifically. There is no additional `GLIBC_TUNABLES` setting to reach
for (disabling the unsorted/small/large bins too is not an available tunable, and would defeat malloc's
own free-list reuse broadly, likely trading one failure mode for a worse one). **This is Track B territory
(`ADVISORY-002-d-zero-fork.md`'s cross-process `D==0` fork)** — the real fix is removing thread-based
relocating fork as the mechanism, not a bigger or different memory-allocator workaround. No unverified fix
was forced onto this; the diagnostic (`glibc_tunables_forwarded`) is left in place as a permanent,
near-zero-cost live check for the next session that touches this class.

**Host RAM note**: this boot's process tree (main runner ~1.9GB RSS + repeated nested collision children)
took host free RAM from ~7.3GB to ~1.5GB over roughly 4 minutes before being killed — consistent with this
project's other standing RAM-pressure warnings for this exact scenario (heavy concurrent fork/collision
load). All `litebox_runner_linux_on_windows_userland` processes were force-killed immediately upon
observing this; host free RAM recovered to ~6.8GB within seconds of the kill. Only one boot was run this
session, per this project's own "never run two full-stack verifications concurrently" rule.

**Not reached this session**: the Terminal Emulator/Applications-menu browser click-path retest. The one
live boot run was fully consumed by the tunable-propagation/RAM investigation above and ended in the same
standing crash class before a clean window opened — unchanged from every other session's experience this
week. This remains blocked on the same standing ADVISORY-001 §3N / Track B blocker, not on anything new.
