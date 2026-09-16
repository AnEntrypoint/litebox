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

## Track A fork-without-exec audit: dbus-daemon/nginx already fixed, boot script's own supervisor subshells found and fixed, evidence inconclusive on host RAM exhaustion (2026-09-16, later session)

**Task**: `advisor/ADVISORY-002-d-zero-fork.md` §6 Track A recommends avoiding fork-without-exec entirely
for XFCE session daemons (`dbus-daemon --fork`, `xfsettingsd`, `Thunar --daemon`) and nginx's own
master/worker model, without touching litebox's architecture, as the fastest route to real desktop
stability. This session audited `.wfgy/webtop_stack.sh` (the gitignored local boot script) against that
recommendation, daemon by daemon.

**Per-daemon findings**:
- **dbus-daemon**: already fixed, predates this session. `webtop_stack.sh` starts the session bus with
  `dbus-daemon --session --nofork --print-address` directly (a foreground, non-self-daemonizing
  invocation) and shims `dbus-launch` to `exec` its argument against the already-running bus rather than
  letting the image's own `startwm.sh` invoke real `dbus-launch` (which internally forks and previously
  SIGSEGV'd, see `AGENTS.md`'s "fork carries pipes... but NOT sockets" lesson). No change needed.
- **nginx**: already fixed, predates this session. Started with `-g 'master_process off; daemon off;'`,
  removing both nginx's own daemonizing self-fork AND its worker-process fork (which the script's own
  comment already documents as unreliable under litebox's thread-based relocating fork: "the worker
  crashed silently... while the master itself kept running"). No change needed.
- **xfsettingsd, Thunar**: launched inside `xfce4-session`'s own client-launch chain (via
  `/defaults/startwm.sh`), not directly invoked by `webtop_stack.sh`. `xfce4-session` forks+execs each
  session client once (safe, ordinary fork+exec) — the open question ADVISORY-002 raises is whether these
  binaries THEMSELVES call fork() again after being exec'd (genuine self-daemonization), which needs an
  interactive guest shell (`xfsettingsd --help`/`Thunar --help`, or a live `ps` tree check for
  reparenting) to verify empirically. **Not independently re-verified this session** — every boot attempt
  was killed for RAM safety before a stable interactive guest shell was reached (see below). Status
  unchanged from ADVISORY-002's own claim that these fork-without-exec by design; if a real substitute
  foreground flag exists for either, it was not found or tested this session.
- **selkies**: not a boot-script daemon-invocation question (no separate fork-avoidance flag applies to
  selkies' own process model) — its relevant fork risk is its `xclip`-polling clipboard monitor, already
  disabled via `--clipboard-enabled=false` (pre-existing fix, predates this session).

**New finding, not anticipated by the daemon-by-daemon framing: the boot script's OWN supervisor loops
were themselves an uninvestigated instance of the exact crash class.** Both the nginx and selkies
supervisor loops were implemented as `( ... ) &` bash subshells — reproducing s6-supervise's respawn
behavior, added in an earlier session specifically because a bare `&` with no restart let a crashed nginx/
selkies silently vanish. But a `(...)&` subshell is fork() with NO exec() after it: bash forks a child
that keeps running the SAME interpreter image (running the while-loop, `$n` arithmetic, string
substitutions for path construction, `case`/`if` evaluation) for the rest of the boot, allocating heap
memory as it goes — structurally identical in shape to `dbus-daemon --fork`'s self-daemonizing fork the
advisory names as unsafe, just spelled as a shell construct instead of a C `fork()` call. This was not
hypothetical: re-reading this same archive's own "GLIBC_TUNABLES propagation" section above shows the
mechanism already caught red-handed — the `SELKIES_SUPERVISOR` subshell (`supervisor_pid=170` from
`SELKIES_LAUNCHED_LAST`) SIGABRT'd on a bare `double free or corruption (out)` line ~2 minutes after a
python3 exec-collision event, while it was still alive as exactly this kind of long-lived
forked-without-exec bash process.

**Fix applied** (`.wfgy/webtop_stack.sh`, gitignored, no litebox source change): both the nginx and
selkies supervisor loop bodies were extracted verbatim into standalone scripts (`/tmp/nginx_supervisor.sh`,
`/tmp/selkies_supervisor.sh`, written via a quoted heredoc so nothing is expanded early) and launched via
`/bin/sh /tmp/<name>.sh &` instead of a bare `( ... ) &` subshell. This is an ordinary fork()+execve() of a
fresh `/bin/sh` image — per ADVISORY-002 §1.5's own mechanism ("a child that execs promptly discards the
whole inherited heap... before allocating again"), the new supervisor process starts with a clean heap
regardless of what corruption state the parent script's own heap was in at that moment, exactly mirroring
what the pre-existing `dbus-daemon --nofork`/`nginx daemon off` fixes already do for THEIR processes.
nginx's supervisor needed five previously-local (non-exported) shell variables (`NGINX_CONFIG`, `CPORT`,
`CWS`, `SFOLDER`, `FILE_MANAGER_PATH`) exported before the new script is launched, since a freshly-exec'd
process only inherits the environment, not the parent shell's local variables; selkies' supervisor needed
no such export (no external variable references in its body).

**Evidence gathered — six live boots this session** (`docker.io/linuxserver/webtop:debian-xfce`,
`--env GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0`, `--publish 3000:3000`, log level
`warn,litebox_platform_windows_userland::fork_verify=error`):

- **Launch-mechanism gotcha found and fixed first**: `Start-Process -RedirectStandardOutput/
  -RedirectStandardError` made the runner exit almost instantly (`HasExited=True` within 3-10s) with a
  peak working set of only ~50MB and ZERO guest-side output — not even the script's own leading `echo
  GUESTSTART`, no crash dump, no Windows Application-Error event, no low-virtual-memory event. Confirmed
  reproducible with BOTH the fixed script's tar AND the known-good original `webtop_stack_seed.tar`
  (ruling out the tar/script content as the cause) and confirmed NOT a resource issue (working set never
  grew, no `Get-WinEvent` crash/OOM record at the matching timestamp). Switching to the call operator with
  `*>` file redirection (`& .\runner.exe ... *> combined.log`) made the exact same invocation run
  normally end to end. Root cause not fully instrumented, but consistent with ADVISORY-002 §3.1's own
  documented risk notes about the runner's console-handle assumptions (`SetConsoleCtrlHandler`, a
  `ConsoleStdinReader` thread) not being satisfied by `Start-Process`'s redirected-pipe handles. This
  gotcha would have produced a false "the fix broke booting entirely" conclusion if not caught early by
  testing the SAME redirection method against the known-good original script first.
- **1 control run** (unmodified original `webtop_stack.sh`/`webtop_stack_seed.tar`): reached
  `NGINX_STARTED`→`NGINX_SELFTEST 200`→`XVFB_UP`→`DBUS_UP`→`DE_LAUNCHED`→`DE_VIA_STARTWM=no`→
  `DE_FALLBACK_LAUNCHED`→`DE_UP via direct xfce4-session`→`SELKIES_LAUNCHED_LAST`→`SK_TAIL_BEGIN`→
  `HOLD t=20s` — a full clean boot. One `fork_verify` AV-path stale-CODE-pointer livelock (8 repeats at
  the same rip, matching the already-documented, already-bounded `b6ddf43` livelock-counter behavior)
  triggered the existing sacrifice-one-task fallback: `fatal signal ... Signal(11) pid=143 comm=gpg-agent`
  — a single, known, non-fatal-to-the-boot task kill, NOT the whole-guest tcache/double-free class (the
  boot's own log continued normally for many more seconds afterward with no further disruption). RSS
  climbed to ~5GB+ by `HOLD t=20s`, forcing a manual kill for RAM safety (free RAM had fallen to
  ~1.07GB); RAM recovered to ~7GB within 2s of the kill, confirming the runner process itself (not a
  leak elsewhere on the host) was the consumer.
- **5 fixed-script attempts**, `webtop_stack_seed_fixed.tar` (a freshly-built tar, same `ustar` header
  structure verified byte-identical to the known-good original via `xxd`, embedding only the updated
  script at `config/webtop_stack.sh`):
  - Attempts 1-2: killed prematurely by this session's own misreading of the script's BY-DESIGN quiet
    60-second Xvfb-socket poll loop (`while [ $i -lt 60 ]; do [ -S "$XSOCK" ] && break; ...; sleep 1;
    done` — pure shell builtins, deliberately forks nothing while waiting, per the script's own comment)
    as an unresponsive stall. Both showed `NGINX_STARTED`/`NGINX_SELFTEST 200` (i.e., the fixed nginx
    supervisor worked identically to the original) before being killed with no crash signature observed
    in either. Inconclusive as boot-completion data points, but corroborate that the fix introduces no
    immediately-visible regression in the part of the boot both attempts covered.
  - Attempt 3: killed mid-transition (right as `XVFB_FAILED`→`DBUS_UP`→`DE_LAUNCHED` were written,
    likely already buffered before the kill took effect) after the same premature-stall misreading — no
    crash signature.
  - Attempt 4: full clean run, patient this time — `NGINX_STARTED`→`NGINX_SELFTEST 200`→`XVFB_UP`→
    `DBUS_UP`→`DE_LAUNCHED`→`DE_VIA_STARTWM=no`→`DE_FALLBACK_LAUNCHED`→`DE_UP via direct xfce4-session`,
    zero crash signature, killed for RAM safety (free RAM ~2.25GB and falling) right at/after `DE_UP`.
  - Attempt 5: `NGINX_STARTED`→`NGINX_SELFTEST 200`→`XVFB_UP`→`DBUS_UP`→`DE_LAUNCHED`→`DE_VIA_STARTWM=no`→
    `DE_FALLBACK_LAUNCHED`, zero crash signature, killed for RAM safety (free RAM ~2GB) before the DE
    fallback verdict resolved.

**Conclusion, stated honestly in both directions**: the fix is mechanistically sound and directly
addresses a live-documented crash instance (the exact `SELKIES_SUPERVISOR` SIGABRT this same archive
already recorded), and caused no observed regression across five attempts — every fixed-script boot
progressed at least as far as the unmodified control run, on the same milestones, at comparable timing.
**But this session cannot claim a measured reduction in crash frequency for the target tcache/double-free
class**, because that class did not occur in EITHER arm (control or fixed) within the boot-age this
session's host RAM allowed — every single boot, six for six, had to be manually killed for RAM safety
between `DE_UP` and `SELKIES_LAUNCHED_LAST`, consistently 130-170s into the boot. The archive's own prior
examples of the target crash class (the `SELKIES_SUPERVISOR` SIGABRT this fix targets, the pid=2 SIGSEGV
elsewhere in this file) occurred several minutes further into the boot's `HOLD`-loop steady state, under
sustained concurrent fork pressure this session never reached before RAM forced a kill. This host's free
RAM was materially more constrained today than the archive's own earlier "~800MB free" baseline assumes —
starting each boot with only ~5.8-6.2GB free (of 15.6GB total) and watching it fall below 1-2GB within
150s of a single `debian-xfce`+selkies boot, well above the documented 650MB-1GB steady-state RSS this
project's own standing lesson names. A re-run on a host with more sustained free RAM (or a lighter guest
image) is needed to actually measure the fix's effect on crash frequency; this session's result is
honest negative evidence (no regression, no confirmed improvement) rather than a positive confirmation.

**Not reached this session**: a live browser-verified stable connection to retest the Terminal Emulator/
Applications-menu click path (task step 4) — no boot held a stable serving window long enough, for the
same RAM reason above. Track B architectural work (cross-process `RawMutex`, presenter-process split,
etc.) was explicitly out of scope for this dispatch and was not started, per ADVISORY-002 §6's own
recommendation that it is multi-session-scale work.

## Track A crash-frequency finally measured with an adequate sample: selkies itself crashes on ~100% of launch attempts, ~120s MTBF, unchanged by the fix; xfsettingsd/Thunar cleared (2026-09-16, later session)

**Task**: the follow-up this file's own prior section called for — re-run `webtop_stack_seed_fixed.tar`
past the historical crash window with real host RAM headroom (this session's host recovered to 6-9GB free,
unlike the ~1.7GB reading at session start, which never recurred once boots were underway — likely a
transient dip from unrelated host activity, not a real constraint), and independently re-verify
`xfsettingsd`/Thunar's own fork behavior.

**Boot 1** (`.wfgy/crashval1.*`, `LITEBOX_LOG=warn,…fork_verify=error`, `--resume-from
webtop_stack_seed_fixed.tar`, i.e. the Track A forkless-supervisor fix in effect): ran **1215s (~20.25
min) live**, killed manually only after the finding below was unambiguous — never RAM-forced (free RAM
stayed 6.3-9.1GB throughout, confirmed by continuous polling; peak runner RSS ~3.1GB). Every one of 17
`spawn_exec_collision_child` events logged `glibc_tunables_forwarded=true` (0 false) — the propagation
finding from this file's earlier "no gap" section reconfirms on a second, much longer-running boot.

**The result is a clean, real answer, not another inconclusive RAM-limited sample**: the `SELKIES_SUPERVISOR`
(the Track A forkless-fix subprocess itself) survived all 6 of its own respawns over the full 20 minutes
without ever dying — direct, positive confirmation that the fix does exactly what it was designed to do
(a fresh `/bin/sh` exec discards whatever corrupted heap state the parent script's shell carried). But
**selkies itself — the process the supervisor launches — segfaulted (`rc=139`, `[sk] Segmentation fault`,
`SIGSEGV`) on attempts 1 through 6, one per launch, at a strikingly consistent **~120-second** interval
measured from the script's own `HOLD t=Ns` ticks (attempt=3 at HOLD~140s, attempt=4 at HOLD~260s,
attempt=5 at HOLD~380s, attempt=6 at HOLD~500s — four consecutive 120s±5s gaps). This period is not a
script artifact: `selkies_supervisor.sh`'s body (`.wfgy/webtop_stack.sh:387-410`) has no delay besides
`sleep 1` between attempts, so ~120s is genuinely how long selkies' own process takes, every single time,
to reach whatever internal operation collides/corrupts and kills it — consistent with (not yet proven to
be) a periodic internal timer of selkies' own (a resize/DPI-recheck candidate, matching this file's
existing "unconfirmed lead" about xfwm4/xfdesktop re-layout events, still not isolated). **In 1215s and 7
total launch attempts, selkies never once logged reaching `Data WebSocket Server listening` — 0/7 successful
binds.** Attempt 7 (launched after attempt 6's crash) did not crash again within the remaining ~700s of
this boot, but also never bound: a live `chrome-devtools` browser check against `http://localhost:3000`
mid-attempt-7 got the dashboard shell (nginx serving fine) but its own console logged `WebSocket connection
to 'ws://localhost:3000/websockets' failed: … Unexpected response code: 502` — the exact
already-documented `connect() failed (111: Connection refused)` to `127.0.0.1:8081` signature, confirming
selkies was simply not listening at that moment either, ~200-300s into its own run. This is a **third**
distinct outcome for a launch attempt (crash / never-crash-but-never-bind), not previously distinguished
from each other in this file's own earlier, RAM-truncated samples.

**Boot 2** (`.wfgy/crashval2_xfdiag.*`, same fixed tar and `--env`, `LITEBOX_LOG` additionally carrying
`litebox_shim_linux::syscalls::process=debug` to get `DIAG_TIMELINE` visibility): independently reproduced
the identical crash signature — `SELKIES_SUPERVISOR: attempt=1 exited rc=139` — on its very first launch,
confirming boot 1's finding is not a one-boot fluke. This boot reached the desktop via the REAL
`startwm.sh` path (`DE_UP via startwm.sh`, not boot 1's fallback), and hit only the already-documented,
already-benign fatal signals along the way: `SIGKILL`→`dbus-daemon` ×2 (ordinary transient-bus teardown)
and one `SIGSEGV`→`gpg-agent` (byte-for-byte the same known, accepted `fork_verify` livelock
single-task-sacrifice this file's own "1 control run" paragraph already recorded) — no new fatal-signal
class. Killed deliberately at 349s (RAM stayed a healthy ~4-4.5GB free throughout; not a RAM kill) once its
two jobs were done, because `syscalls::process=debug` measurably slows guest wall-clock progress (far more
`DIAG_TIMELINE`/`resolve_shebang` lines than a normal boot) and continuing it further was low value once
xfsettingsd/Thunar were answered.

**`xfsettingsd`/Thunar, independently re-verified live for the first time this week (ADVISORY-002 §6's one
open item, closed)**: `DIAG_TIMELINE` in boot 2 shows both launched by ordinary, safe fork+exec —
`comm=xfce4-session` execve'ing `argv0=/usr/bin/xfsettingsd` (pid=146, t=114.8s guest-time, after the
expected `ENOENT`-then-succeed `PATH` search through `/lsiopy/bin`, `/usr/local/sbin`, `/usr/local/bin`,
`/usr/sbin`), and `comm=xfce4-session` execve'ing `argv0=/usr/bin/Thunar` (a wrapper script) which itself
then execs `argv0=/usr/bin/thunar-real` from a `bash` comm — both ordinary parent-forks-child-execs-once
chains, exactly the safe shape ADVISORY-002 §6 already assumes for images that don't self-daemonize these
binaries. **No fatal signal was ever attributed to xfsettingsd's or Thunar's pids in this boot.** The only
`pid=146` "exit" events seen afterward were `comm=pool-9`-style GLib thread-pool worker threads exiting
cleanly (`status=0`) — ordinary intra-process thread churn, not the process dying and not a
fork-without-exec self-daemonization event. **Conclusion: xfsettingsd and Thunar do NOT need their own
forkless-daemon fix — this closes the one item ADVISORY-002 §6's Track A audit left unverified last
session.** All four daemons named by Track A (dbus-daemon, nginx, xfsettingsd, Thunar) are now confirmed
either already fixed or never at risk in this image.

**What this means for the dispatch's core question ("did the forkless-daemon fix reduce crash frequency,
eliminate it, or make no difference")**: **no difference to selkies' own crash rate.** The fix's scope was
always precisely the supervisor script's own heap (confirmed working, 6/6 respawns survived, 2 boots, 0
regressions) — it was never going to touch selkies' own process-internal corruption, and it doesn't.
Selkies' crash rate in this environment right now is effectively **100% per launch attempt** (7 attempts
across 2 independent boots, 7 failures — 6 outright `SIGSEGV` crashes plus 1 silent no-bind hang), a
materially WORSE measured rate than this project's older "sporadic, once in several cycles" characterization
— though that older figure predates today's heavier concurrent-fork-pressure conditions (nested
`gcc`/`collect2`/`cc1` exec collisions, the exec-collision recovery path itself, and the supervisor
fix's own extra fork+exec) and the two are not measured under identical conditions, so this is not
claimed as a regression, only as the first real measurement under current conditions.

**Terminal Emulator/Applications-menu browser click-path retest: still not reached, but for a newly and
precisely diagnosed reason.** It is no longer "every boot lost to RAM before a clean window opened" — host
RAM was healthy (4-9GB free) for the full ~26 minutes of combined boot time this session. The actual and
only blocker is that **selkies (the streaming layer) did not reach a stable bound-and-serving state even
once, in 7 attempts across 2 boots** — there was no browser-visible desktop stream to click into at any
point. This is a stronger, more decisive negative result than any prior session reached (all of which were
cut off by RAM before this clarity was possible). The real fix remains Track B
(`ADVISORY-002-d-zero-fork.md`'s cross-process `D==0` fork, removing thread-based relocating fork as
selkies' own execution mechanism) — no new workaround was attempted or warranted here.

**Not reached this session**: the ACK-stall-kill investigation (task step 5) — explicitly lower priority
per this dispatch, and its relevance is superseded for now: selkies never reached a connected state to
stall FROM in either boot this session, so there was nothing live to correlate against a packet capture.

**Host RAM, final state**: both runners killed cleanly and manually (never by the RAM-safety threshold);
free RAM recovered to ~9.3GB within 2s of each kill, confirming the runner process itself was the only
consumer and the host has no other leak. `Get-Process litebox_runner_linux_on_windows_userland` returns
zero matches at the end of this session.

## The ~120s selkies-crash cadence is `spawn_exec_collision_child`'s own absolute cap, direct causal proof, not a watchdog regression (2026-09-16, later session)

**Task**: the previous section's own `SIGSEGV`/`rc=139` ~120s cadence was measured via the script's `HOLD`
ticks, never directly correlated against `spawn_exec_collision_child`'s internal log lines in the SAME
boot — this dispatch's hypothesis was that `42d8ced`'s own 120s absolute cap might be killing a
legitimately-still-progressing (not genuinely wedged) nested recovery, i.e. a real defect in yesterday's
fix, not a coincidence of timing.

**Code read first** (`litebox_platform_windows_userland/src/lib.rs:10156-10213`): the poll loop checks
`elapsed >= EXEC_COLLISION_ABSOLUTE_CAP` (120s) UNCONDITIONALLY, before the progress check each iteration
— it kills and returns `Err` regardless of whether the child is making CPU progress, per its own log text
("exceeded the absolute time cap even while making CPU progress"). The 20s stall-grace is a SEPARATE,
earlier-firing branch that only trips on zero measurable CPU delta for a full 20s. Structurally: reaching
the 120s branch at all is only possible if the child's CPU delta cleared `MEANINGFUL_CPU_DELTA_100NS` at
least once every <20s throughout — i.e. the absolute cap firing is itself proof the child was NOT flatlined
the whole time (contrast the earlier "GLIBC_TUNABLES propagation" section's own instance, which measured
~0.05s CPU pinned via `Get-Process` polling and still only hit the SAME 120s branch — see below for how
both are reconciled).

**Live boot 1** (`.wfgy/watchdogcheck_launch1.ps1`, `--resume-from webtop_stack_seed_fixed.tar`, `LITEBOX_LOG=
warn,…fork_verify=error`): died at t=105.9s to the standing, already-documented, UNRELATED tcache/
double-free class hitting the top-level guest shell directly (`fatal signal: …Signal(11) pid=2 comm=sh`,
`Segmentation fault`) — before ever reaching selkies or triggering `spawn_exec_collision_child` even once.
Independent, additional live confirmation that this second corruption class is real and can fire on
ordinary fork/exec churn (this run's own `mkdir`/`cp`/`which` calls for the xfce4-session fallback path),
with zero relationship to the watchdog under investigation.

**Live boot 2** (`.wfgy/watchdogcheck_launch2.ps1`, identical config): reached `SELKIES_LAUNCHED_LAST` and
produced the DIRECT causal chain this dispatch needed, verbatim from `.wfgy/watchdogcheck2.log`/`.out.log`:

```
119.721726900s WARN spawn_exec_collision_child: GLIBC_TUNABLES … glibc_tunables_forwarded=true
119.722260600s WARN spawn_exec_collision_child: this process's own address space cannot load this image …
  [path=/lsiopy/bin/python3 -- selkies' shebang re-exec, confirmed by the error= line below]
239.817279400s WARN spawn_exec_collision_child: replacement process exceeded the absolute time cap … killing it
239.946159400s WARN spawn_exec_collision_child: the replacement process did not exit normally …
  path=/lsiopy/bin/python3 error=spawn_exec_collision_child: absolute time cap exceeded
  killing process with SIGSEGV tid=160 path=/lsiopy/bin/python3 error=LoadError(Map(Errno(17 = EEXIST)))
```
…and in the SAME boot's guest-side stdout, immediately: `[s] SELKIES_SUPERVISOR: attempt=1 exited rc=139
-- respawning`. Elapsed collision-to-cap: 120.095s — matching the earlier "GLIBC_TUNABLES propagation"
section's own 120.0935317s/120.1s measurements to the same decimal precision, on a DIFFERENT boot, DIFFERENT
day-session, confirming this is deterministic mechanism behavior, not noise. This repeated 3x total in this
one boot (`cap`-line count 6, `rc=139` count 3) before the run was killed for RAM safety (free RAM fell
9.1GB→2.8GB over the run; recovered to 8.8GB within 3s of `Stop-Process`).

**Verdict, both directions honestly stated**:
- **The absolute cap IS the direct, proven cause of the ~120s SIGSEGV cadence** — not an independent tcache
  coincidence landing on a similar timescale. This closes the timing-correlation-vs-causation gap the prior
  section's own measurement left open.
- **This is NOT the hypothesized defect** ("the fix kills a slow-but-otherwise-fine recovery"). Two lines of
  evidence: (1) reaching the 120s branch at all requires periodic CPU progress (see code-read above) — this
  boot's nested child was not idle; (2) regardless, the nested recovery is a genuinely separate OS process
  with no shared AF_UNIX/loopback namespace to the ORIGINAL guest's already-running Xvfb/D-Bus
  (`lib.rs:10100-10128`'s own doc comment, and `docs/fork-fs-veh-2026-09-08.md:128-144`'s identical gap for
  the sibling cross-process FORK case) — selkies inside that nested child cannot reach the desktop it needs
  to serve NO MATTER HOW LONG it runs. An uncapped/longer-cap re-run was deliberately NOT attempted: it would
  only reproduce the pre-`42d8ced` unbounded whole-boot hang (already proven, at cost, in that commit's own
  investigation) for zero new information, since the blocker is structural, not a timing threshold.
- **No code change made to `spawn_exec_collision_child`, and none is warranted** — raising the cap would
  strictly worsen effective boot behavior (longer hangs before an already-guaranteed-failed attempt gets
  respawned), with no corresponding chance of success. The fix remains correctly scoped exactly as `42d8ced`
  and the "selkies-boot-hang root cause and fix" section above already concluded.
- **The real, still-open blocker is Track B** (`ADVISORY-002-d-zero-fork.md`'s cross-process `D==0` fork,
  extended to cover `spawn_exec_collision_child`'s own nested children too): giving cross-process children a
  shared AF_UNIX/loopback namespace with the parent guest is the only change that could let selkies' own
  collision-recovery attempt actually succeed instead of deterministically timing out every ~120s.

**Not reached this session**: a successful `Data WebSocket Server listening` bind (0/2 this session, 0/9
combined with the prior section's 0/7) and, consequently, the Terminal Emulator/Applications-menu browser
click-path retest — still blocked on the same standing Track B blocker, unchanged.

**Host RAM, final state**: both boots killed manually (boot 1 for an unrelated crash, boot 2 for RAM safety
at 2.8GB free); free RAM recovered to 8.8GB within 3s of the final kill. `Get-Process
litebox_runner_linux_on_windows_userland` returns zero matches at the end of this session.

## ET_EXEC confirmed live, no PIE swap-in, boot-reorder mitigation tried and found insufficient (2026-09-16, later still)

**Task**: don't fix `spawn_exec_collision_child`'s recovery path (structurally blocked, Track B, see above) -- instead try to prevent the `/lsiopy/bin/python3` collision from happening at all, since AGENTS.md already suspected the interpreter is a fixed-address `ET_EXEC` binary rather than a normal ASLR'd PIE one.

**1. Direct live verification (not re-derived from the archive's own prior claim).** Booted the runner against `docker.io/linuxserver/webtop:debian-xfce` with a single non-interactive `/bin/sh -c` command (no Xvfb/desktop needed just to inspect a file):

```
target/release/litebox_runner_linux_on_windows_userland.exe -Z \
  --env GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0 \
  --oci-image docker.io/linuxserver/webtop:debian-xfce --resume-from .wfgy/webtop_stack_seed_fixed.tar \
  -- /bin/sh -c 'readlink -f /lsiopy/bin/python3 && readelf -h /lsiopy/bin/python3'
```

Result: `/lsiopy/bin/python3` -> `/usr/bin/python3` -> `/usr/bin/python3.13`. `readelf -h` on that real file: `Type: EXEC (Executable file)`, `Entry point address: 0x67b0d0`. Genuinely non-PIE, confirmed live, not assumed.

**2. Is this a custom lsiopy-built interpreter (as AGENTS.md previously guessed) or the stock system one?** Listed `/usr/bin`, `/lsiopy/bin` and ran `dpkg -l | grep -i python` in the same guest. Every python-named path under both directories resolves to the SAME single `python3.13` binary (`/lsiopy/bin` is a plain symlink farm to `/usr/bin`, not a separate venv/build); dpkg lists exactly one interpreter package, `python3.13 3.13.5-2+deb13u4`, the ordinary Debian 13 (trixie) system python3. This REFUTES the prior guess that linuxserver.io built a custom "lsiopy" python toolchain for this image -- it is the stock distro package, non-PIE for whatever reason Debian's own python3.13 build made that choice (not investigated further; out of scope -- the fact, not the why, is what mattered for the mitigation decision).

**3. PIE swap-in candidate: ruled out.** No second python3 install of any kind exists in this image to symlink in place of the ET_EXEC one -- confirmed by the same listing above. This closes that mitigation angle with direct evidence rather than "didn't find one."

**4. Binary-patch-to-PIE: ruled out on architectural grounds, not attempted.** Converting `ET_EXEC` to `ET_DYN` after the fact is not a link-level/header patch -- PIE requires the compiler to have emitted position-independent code (relative addressing for globals/GOT/PLT throughout), which a non-PIE compile does not produce; there is no relocation information to retrofit onto already-fixed-address machine code. Only a full rebuild from Debian's `python3.13` source with `-fPIE -pie` added would produce a PIE alternative, which is a guest-image-build-system change, out of scope for this dispatch (and this project has no Dockerfile/image-build script of its own for this stock upstream image -- it consumes `linuxserver/webtop:debian-xfce` as-is via `litebox_packager`).

**5. Boot-reorder mitigation, attempt 1 (launch-only, no wait).** Moved the `selkies_supervisor.sh` launch block in `.wfgy/webtop_stack.sh` from after `startwm.sh`/DE_FALLBACK to immediately after the `dbus-launch` shim setup (before `startwm.sh` runs at all), on the theory that fewer prior forks/execs in litebox's one shared address space by the time python3 first execs should lower the odds something is already sitting at its fixed load address. Rebuilt `.wfgy/webtop_stack_seed_fixed.tar` from the edited script and re-booted (`.wfgy/reorder_boot1.*`, `.wfgy/reorder_boot2.*`).

- Boot 1: `XVFB_FAILED` (a separate, already-tracked intermittent issue -- see "Xvfb's own XVFB_FAILED rate" note elsewhere in this archive) -- uninformative for this specific question, killed and retried.
- Boot 2: `XVFB_UP`, `DBUS_UP`, `SELKIES_LAUNCHED_LAST` (now firing well before `DE_LAUNCHED`), then `DE_UP via startwm.sh` (desktop came up fine) -- but selkies' own supervisor loop crashed on EVERY observed attempt: `SELKIES_SUPERVISOR: attempt=1..6 exited rc=139` (SIGSEGV), 6/6. Cross-checked against `litebox_platform_windows_userland`'s own diagnostic log (`LITEBOX_LOG=warn,...fork_verify=error`): `spawn_exec_collision_child: GLIBC_TUNABLES ... path=/lsiopy/bin/python3 glibc_tunables_forwarded=true` fired once per attempt (6 collision events matching 6 crashes), and `absolute time cap exceeded` (the existing `42d8ced` fallback) fired 6/6 times too -- i.e. every single respawn hit the same structurally-doomed nested-recovery path, not a fresh bug. This is a HIGHER per-attempt collision rate than the archive's own baseline (roughly one collision per whole boot, not one per respawn). The proposed explanation: launching selkies right after dbus but with NO gate on it succeeding meant its 30-attempt respawn loop now ran CONCURRENTLY with `startwm.sh`'s own heavy fork/exec tree (`xfwm4`, `xfce4-panel`, `xfdesktop`, `xfsettingsd`, Thunar, per-app `xfconf-query` children) for the whole desktop-launch window, instead of sequentially after it as in the original ordering -- two independently fork-heavy subsystems racing for the same fixed addresses at the same time, rather than one settling before the other starts. Host RAM during this boot: fell from ~7GB free to a low of ~2.3GB free around `DE_UP`, then stabilized (not still falling) -- consistent with the already-documented "one webtop+selkies boot's RSS passed 4.5GB by DE_UP" note; killed manually once the pattern was clear, RAM recovered to ~8.4GB free within seconds.

**6. Boot-reorder mitigation, attempt 2 (gate the desktop launch on selkies binding first).** Edited `.wfgy/webtop_stack.sh` again: right after the moved selkies-launch block, added a bounded (260s) curl poll against selkies' own port (same `CURLE_COULDNT_CONNECT`-exit-code technique the existing `SELKIES_PORT_UP` gate already uses), logging `SELKIES_PORT_UP_PREDE` on success or `SELKIES_PORT_PREDE_TIMEOUT` after 260s either way -- so `startwm.sh` only starts once selkies has already bound, or after a bounded wait if it hasn't (never blocks forever). Rebuilt the seed tar, re-booted three more times (`.wfgy/reorder_boot3.*` through `reorder_boot5.*`):

- Boots 3 and 4: `XVFB_FAILED` again (3/5 total boots this session hit this pre-existing, unrelated issue -- consistent with the already-documented "4 of the last 6" rate elsewhere in this archive; not investigated further, not this dispatch's blocker).
- Boot 5: `XVFB_UP`, `DBUS_UP`, then `SELKIES_PORT_PREDE_TIMEOUT after 260s` -- selkies did NOT bind within the isolated 260s window even with no desktop-session competition at all. `startwm.sh` then launched per the bounded-timeout design, `DE_UP` succeeded, and selkies' supervisor loop proceeded to crash on subsequent attempts anyway: `attempt=1` rc=139, `attempt=2` rc=139, `attempt=3` rc=1 (a DIFFERENT failure mode -- likely the already-documented "nested gcc/collect2 sub-step, itself another nested collision, returning raw_status=1" case, not the absolute-cap SIGSEGV), `attempt=4` rc=139. Killed manually after ~13 total minutes on this boot with no successful bind observed. Host RAM: dipped to ~2.0-2.1GB free around `DE_UP`/early selkies-crash-loop, stabilized in the low 2GB range (not still falling), recovered to ~8.1GB free within seconds of the final kill.

**Conclusion.** Neither reorder variant produced a clean, repeatable bind. The one new, reasonably solid finding: collision rate is driven by concurrent fork PRESSURE from whichever OTHER subsystem is actively forking/execing at the same wall-clock moment, not simply by how much fork/exec HISTORY has accumulated before selkies' first exec -- moving the launch earlier only helps if nothing else is concurrently doing the same thing, and gating on a successful bind doesn't guarantee one happens inside a bounded window either, since selkies' own first-attempt collision can occur regardless of how isolated its own launch window is (boot 5's `SELKIES_PORT_PREDE_TIMEOUT` obtained zero successful binds even with the desktop session held back). This is consistent with, not a refutation of, the standing conclusion elsewhere in this archive: the real fix is Track B (`advisor/ADVISORY-002-d-zero-fork.md`) -- a shared AF_UNIX/loopback namespace across the fork boundary -- not anything reachable from boot-script ordering or an interpreter substitution. The reorder change was left in `.wfgy/webtop_stack.sh` (it is not harmful -- selkies now starts earlier in wall-clock terms regardless of outcome, and the pre-existing failure mode is unchanged, not worsened, once the 260s gate is accounted for) but is explicitly NOT claimed as a fix.

Terminal Emulator/Applications-menu click-path retest: still blocked, for the same reason as every prior session today -- selkies never reached `Data WebSocket Server listening` in either of the two `XVFB_UP` boots obtained (0/2), so there was never a live stream to click into. No new evidence for or against the menu/terminal mechanisms themselves; they remain independently verified healthy from the direct-guest-driving test earlier in this archive.

Evidence: `.wfgy/elfcheck3.out.log` (readelf), `.wfgy/elfcheck4.out.log` (dpkg/symlink listing), `.wfgy/reorder_boot1.out.log` through `reorder_boot5.err.log`, `.wfgy/webtop_stack.sh` (current, reordered) and `.wfgy/webtop_stack.sh.bak-preselkiesreorder` (pre-change copy).

**Host RAM, final state this pass**: all `litebox_runner_linux_on_windows_userland` processes killed manually after boot 5; free RAM recovered to ~8.1GB (of ~15.6GB total) within seconds, `Get-Process` confirms zero matches.

## Trimmed from AGENTS.md 2026-09-16 (presenter-process-split session, kept full detail here)

**A trampoline-extension failure used to poison a whole segment's syscalls, now fixed** (`6311f74`). A
one-page initial allocation guess meant a segment needing more stub space (ordinary for a real binary)
extended at one fixed adjacent address with no fallback; any unrelated mapping there made
`apply_trap_fallback` poison **every** syscall in the segment with `ICEBP;HLT` on first use. Now sized
from a cheap `0F 05` byte-pair count (sound upper bound), capped at 4MiB. Witnessed live: `edgelevel/
alpine-xfce-vnc:latest` SIGILL'd within 3s before, zero fatal signals after.

**Tags, verified live, never from the name**: `linuxserver/webtop:alpine-mate` ships MATE, not XFCE;
`alpine-xfce` does not exist (404); `debian-xfce`/`ubuntu-xfce` DO ship real XFCE (`34da133`, `c65ab93`,
`1ea5203`; only the debian/ubuntu/fedora/arch bases carry it, `8c07f51`). `alpine-*` flavors share one
~519MB base layer (`9c7ea2b`); `debian-xfce` is a 17-layer Debian 13 image sharing nothing with them.
`edgelevel/alpine-xfce-vnc` is Alpine 3.16.0, Xvfb/browser pipeline. `ubuntu-xfce` packs fine
but its rust-coreutils aborted in rustix auxv handling (`sleep`/`tail`/DE launch) — `bb46f1a` has since
implemented `/proc/self/auxv`/`AT_EXECFN`, so that's a re-test, not a fresh investigation.

## Presenter-process split: full live-verification narrative (2026-09-16 follow-up session)

Prior session's build (commits `4e848d9`, `1ca3da0`, `831a35d`, `71c76b9`, `1e55830`) had verified
scenarios 2 and 6 from `docs/presenter-process-design.md` section 6 plus the `show` TIMEOUT path,
but explicitly left scenario 1's byte-identical dump check, scenario 4's SUCCESS path, and
scenario 5's crash-recovery-with-content unverified for lack of a real DRM-flip-producing guest.
This follow-up built one and closed all three.

**Recipe used**: `docs/dump-frames-writer-verify-probe/README.md`'s exact steps -- a Python-hosted
zig (`pip install ziglang`, `python -m ziglang` via a tiny shell shim on `PATH`) cross-compiled
`drmgui_multiflip.c` for `x86_64-linux-musl`, `litebox_syscall_rewriter` hooked its syscalls, and
the result was appended (`tar -rf`, staged under a local `tmp/` first) into a working copy of
`alpine-rootfs.tar`. Two additional environment knobs beyond the README's own baseline recipe:
`DRMGUI_FLIP_COUNT`/`DRMGUI_FLIP_DELAY_MS` set high (`600`/`1000`) to keep a guest alive and
flipping for several minutes so `show`/`hide`/kill/respawn could all be exercised interactively
against ONE long-lived run, forwarded via the runner's existing `--forward-env`.

**Scenario 1 (byte-identical regression)**: `LITEBOX_DUMP_FRAMES=1`, `DRMGUI_FLIP_COUNT=20`, no
`--gui` -> 21 `.bmp` files, `non_black_pixels=2073600`/`distinct_colors_capped64=1` every frame
(the guest fills the whole 1920x1080 buffer with one solid color per flip), end-of-run
`21 frames enqueued for writing, 0 dropped due to writer backpressure`. Matches the pre-existing
2026-09-05 baseline in `docs/dump-frames-writer-verify-probe/README.md`'s own "Results" section
(`21 flips, 21 .bmp files, exactly matching historical every-frame behavior`) exactly.

**Scenarios 3/4/5 tooling**: a small PowerShell `NamedPipeClientStream` script
(`pipe_client.ps1`, scratch) sent one command per invocation and printed the raw reply line --
same shape the prior session used. The runner spawns TWO OS processes for one guest run (only one
hosts the `ControlServer`'s named pipe; the other is an internal helper) -- when locating the live
pipe, try both candidate `litebox-<pid>` names and use whichever one's `presenter?` actually
replies, don't assume the first-listed `Get-Process` result is the right PID.

**The crash and its diagnosis** (full blow-by-blow, compacted out of the main AGENTS.md entry):
issuing `show` against a presenter that had a real guest actively flipping caused
`litebox-presenter.exe` to disappear within a few seconds, every time, reproduced 3+ times
independently of whether the presenter was auto-spawned by `--gui=hidden` or manually run in the
foreground for visibility into its own stderr. Bisection process: (1) added a diagnostic print to
`litebox_presenter/src/main.rs`'s pipe-reader thread's `Ok(None)|Err(_)` exit arm -- confirmed it
was hitting a clean `Ok(None)` (broken pipe), not a panic, not the process's main-thread event
loop legitimately returning. (2) added matching diagnostics to
`litebox_runner_linux_on_windows_userland::control_server`'s own `handle_connection`/
`push_to_presenter` -- confirmed the SERVER's own read on the presenter's connection independently
saw the identical `Ok(None)` at the same moment `push_to_presenter`'s `WriteFile` (through the
`duplicate_into_current_process`-duplicated handle) had JUST reported success. (3) Traced this to
`litebox_presenter_protocol::pipe`'s `CreateNamedPipeW`/`CreateFileW` calls: `FILE_FLAG_OVERLAPPED`
set on every handle, but every single `ReadFile`/`WriteFile` call (both client and server sides)
passed a NULL `OVERLAPPED` pointer -- a documented-unsound combination once more than one thread
has I/O in flight on handles referring to the same pipe object at once, which is exactly this
module's own `show`/`hide` design (one thread blocked reading a presenter's connection waiting for
rare `key`/`rel`, a different thread writing `show`/`hide` through a duplicate handle of the same
object). First fix attempt: removed `FILE_FLAG_OVERLAPPED` entirely (reasoning: if truly
synchronous I/O was intended, make the handle actually synchronous). This "fixed" the crash but
introduced a WORSE, previously-latent bug: the write then hung forever (confirmed via a targeted
`eprintln` bracketing the raw `WriteFile` call, which printed "starting" but never "returned") --
a pending synchronous read on one duplicate handle can starve a synchronous write on another
duplicate of the same file object at the kernel level, with no way to avoid it once queued. This
explains why NO prior session had ever seen this: the ORIGINAL bug (corruption/crash) always fired
before a session could stay connected long enough to trip the SECOND, deadlock bug hiding behind
it. The real fix: keep `FILE_FLAG_OVERLAPPED`, and give EVERY `ReadFile`/`WriteFile`/
`ConnectNamedPipe` call its own fresh, private `OVERLAPPED` structure with its own manual-reset
event (`litebox_presenter_protocol::pipe::overlapped_call`, `windows-sys`'s
`Win32_System_Threading` feature newly enabled in that crate's `Cargo.toml` for `CreateEventW`),
waited on via `GetOverlappedResult(..., bWait=TRUE)`. This is the intended, standard way to allow
multiple simultaneously-pending I/O operations on one named pipe object -- concurrent pending
read+write across duplicate handles is explicitly what `FILE_FLAG_OVERLAPPED` exists to support,
and per-call private synchronization objects mean the two operations never share any completion
state to race over. Applied uniformly to both the server's `create_and_accept_one_instance`/
`handle_connection` path and the client's `connect_client` path (which gained
`FILE_FLAG_OVERLAPPED` on its own `CreateFileW` too, closing the same latent hazard for its own
future input-forwarding key/rel writes racing its blocked reader thread, even though that path
wasn't exercised this session since no real keyboard/mouse input was injected).

**Post-fix confirmation, timed**: `show` on an already-connected, ready presenter now replies in
~300-370ms (previously: either silent crash within ~1-3s, or -- during the deadlock half-fix --
no reply ever). The presenter survives indefinitely afterward; `presenter?` correctly transitions
`ok hidden` -> `ok visible`; `EnumWindows`/`GetWindowThreadProcessId`/`IsWindowVisible` (run from
a PowerShell process in the SAME desktop session, `SessionId` matched against the presenter's own)
finds a real, visible top-level window titled `"litebox virtual display"` for the presenter's own
PID -- this is the FIRST session in this feature's history where that Win32 introspection produced
a real, correct positive result, because it's the first session where the presenter survived long
enough for there to be anything real to find. A `PrintWindow(hwnd, hdc, PW_RENDERFULLCONTENT)`
capture of that live window was saved and visually inspected: it shows the guest's solid fill
color as a large diagonal-edged shape rather than a clean full rectangle -- a known
`PrintWindow`-vs-hardware-accelerated-DXGI-flip-model capture artifact (PrintWindow is well known
to render DirectComposition/DXGI swapchain content incompletely/incorrectly on many hosts), NOT a
litebox rendering regression: the control-pipe `screenshot` command, which reads the scanout
SECTION directly rather than compositing the on-screen window, reported the correct
`non_black_pixels=2073600` (the full 1920x1080 frame) at the same moment. Do not use `PrintWindow`
captures as a correctness signal for this feature going forward -- `screenshot`'s own pixel counts
already are, and remain, the project's standing reliable verification method (matches the
already-standing rule against trusting `IsWindowVisible` alone).

**Scenario 5, timed**: `Stop-Process -Id <presenter-pid> -Force` while the presenter was
displaying live content -> `presenter?` immediately reported `ok none`, `screenshot` kept
returning `non_black_pixels=2073600` with no interruption (guest/runner never touched). A
follow-up `show` spawned a brand-new `litebox-presenter.exe` (new PID, confirmed via
`Get-Process`/`StartTime`), which reconnected, registered, and had its own real visible
`"litebox virtual display"` window within ~370ms, with `screenshot` immediately reflecting current
scanout content -- true respawn-and-resume, matching the design's own claim, not merely "a new
process now exists" as the prior session could only partially argue from the timeout path.

**Cleanup**: all `litebox_runner_linux_on_windows_userland.exe`/`litebox-presenter.exe` processes
started during this verification pass were killed (`taskkill /F`) before the session ended; none
were left running. Scratch build artifacts (`drmgui_multiflip`/`.hooked`, the working rootfs copy,
`run-*` test directories) were deleted from `.wfgy/` afterward; the pre-existing large `.wfgy/`
accumulation from earlier, unrelated sessions was left untouched (out of scope for this pass).

## Second drain pass, same day — RawMutex-work session, AGENTS.md crossed 30KB again

Relocated verbatim (not summarized) from `AGENTS.md`; still-open status/pointers for each stay in
`AGENTS.md` itself, per this file's own stated principle above.

### The ACK-stall-kill — root cause still unidentified after ten investigations, the one genuinely open bug in this project

**Symptom**: streams fine, then `sk.log`'s `Client stall for 'primary'... Forcing backpressure` →
`Data WS closed ...: sent 1011 ... keepalive ping timeout` — selkies' own stall-detector kills the data
channel, and the dashboard's frontend auto-reloads. A distinct second way to land there: a fresh tab's
first connection sometimes 404s on `/websockets`, tripping the same auto-reload.

**Eight candidates investigated; seven refuted by live measurement or architecture read** (client JS/
transport, nginx config/frontend dual-connect, `/proc/<pid>/cmdline` ENOENT cost, selkies' psutil tick,
`GPUtil.getGPUs()`, pixelflux capture/encode, litebox's own NAT/`--publish` gateway). **One, fork_verify
thread-based healing starving selkies' event loop, is NOT confirmed, NOT cleanly refuted** — a real
livelock-protection gap in `on_single_step` case (1) WAS found and fixed (`b6ddf43`), stress-tested clean
11+ minutes with heals firing continuously, but no unfixed-vs-fixed A/B was possible and a disconnect has
never once co-occurred with active fork-heal traffic in ten sessions. **Do not re-reach for GLIBC_TUNABLES
here; do not re-open the frontend/nginx angle** (both byte/log-verified clean). Per-candidate evidence:
`docs/AGENTS_ARCHIVE_2026-09-15.md`.

**New, unconfirmed lead**: a live disconnect coincided with an open Thunar window closing, suggesting an
xfwm4/xfdesktop re-layout event might trigger one of `selkies.py`'s untested subprocess spawns
(`resize_display`/xrandr/xfconf-query) — untested, not ruled out.

**A follow-up needs**: a real host-TCP packet capture (Wireshark/pktmon on `127.0.0.1:3000`) correlated
against a guest-side timing instrument on selkies' `websockets`-library pong-receive path, plus a working
trusted-input path into the canvas and >1.8-2GB free memory for a rebuild-and-restart window.

**Separate open complaint, distinct from the ACK-stall-kill: Terminal Emulator/Applications-menu popup.**
Architecture read found no litebox grab-/menu-specific code on this path; driving the guest DIRECTLY
(bypassing selkies/browser) proved **both `xfce4-terminal` and the `xfce4-popup-applicationsmenu` popup
mechanism are independently healthy** (open correctly twice each, no crash). `net.rs` is fully cleared
(live-proven twice); the masked-502/404 bug was a startup race (fixed) plus the crash class below hitting
selkies moments after bind (still open). `spawn_exec_collision_child`'s hang is fixed and reconfirmed
(`42d8ced`, 20s/120s bounded). **2026-09-16: the click-path retest is STILL blocked, now for a precisely
diagnosed reason, not RAM** — 2 boots, ~26 min combined, RAM healthy 4-9GB free throughout, selkies reached
`Data WebSocket Server listening` **0 times in 7 launch attempts**; a live `chrome-devtools` probe got a
real `502` (`ws://localhost:3000/websockets` refused to selkies' own port), confirming no stream was ever
up to click into.

### Track A fork-without-exec audit and ET_EXEC finding (ADVISORY-002 §6)

**Track A fork-without-exec audit: crash-frequency measured, root-caused to
`spawn_exec_collision_child`'s own 120s absolute cap firing on selkies' python3 collision — confirmed not
a bug, not a watchdog regression.** All four Track A daemons (dbus-daemon/nginx/xfsettingsd/Thunar)
cleared; selkies itself: 0/7 binds, `SIGSEGV`/`rc=139` on 6/7 at a ~120s cadence — a direct causal log
line (not correlation) proves the nested recovery child makes real CPU progress yet structurally cannot
succeed (no shared AF_UNIX/D-Bus namespace to the original guest). Raising the cap only prolongs an
already-guaranteed failure; real fix stays Track B.

**2026-09-16, later still: ET_EXEC directly confirmed (not assumed) — real Debian `python3.13`, no PIE
swap-in exists, boot-reorder mitigation tried and insufficient.** Live `readelf -h` on the guest's actual
interpreter (`/lsiopy/bin/python3` → `/usr/bin/python3` → `python3.13`) shows `Type: EXEC`, entry
`0x67b0d0` — genuinely non-PIE, and the stock dpkg `python3.13 3.13.5-2+deb13u4` package, not a custom
lsiopy build as previously assumed. No alternate PIE python3 exists anywhere in the image to swap in, and
patching `ET_EXEC`→`ET_DYN` in place isn't viable without a full source rebuild — both ruled out live, not
assumed. Moved selkies' launch earlier in `.wfgy/webtop_stack.sh` (before startwm.sh) two ways; both
insufficient — **new finding: concurrent fork PRESSURE from another active subsystem (not just cumulative
history) drives the collision rate** (launch-only made it WORSE, 6/6 respawns collided once it raced
xfce4-session's own fork tree; gating on selkies binding first, 260s bounded, still didn't get a clean
bind in the one Xvfb-up boot obtained). Net 0/2 XVFB-up boots reached `Data WebSocket Server listening`
this pass; Terminal Emulator retest still blocked. **Confirms Track B is the only real fix at this
layer.** Reorder kept (harmless) but not claimed as a fix.

### Presenter-process split — full live-verification narrative (headline + still-open items stay in AGENTS.md)

Built and committed: `litebox_presenter_protocol` crate (newline-delimited scanout/screenshot/
show/hide/presenter?/key/rel/abs/ps/strace/frames grammar + named-pipe transport), runner-side
`ControlServer` (`litebox_runner_linux_on_windows_userland/src/control_server.rs` --
`DuplicateHandle`-based zero-copy scanout handoff; a header-section polling thread, NOT a
`DrmSubsystem` flip-callback, keeps headless-with-no-observers exactly as cheap as before per
section 4.4, since that callback mechanism unconditionally maps the whole pixel buffer once ANY
observer exists), and `litebox-presenter.exe` (new crate `litebox_presenter`, links only
`litebox_platform_windows_userland::presentation` verbatim + the protocol crate, zero shim/kernel
dependency). `--gui` is now `Option<GuiMode>` (`--gui`/`--gui=hidden`); old `--gui-hidden` kept as
a deprecated alias. `DrmSubsystem` gained `frame_seq` (bumped unconditionally, covers
SETCRTC/PAGE_FLIP/DIRTYFB alike) and `scanout_snapshot()` (a plain generic query, not a boxed flip
callback -- that mechanism can't carry `Platform::SharedMemoryHandle` across a trait object, the
real pre-existing `E0277` `flip_callbacks`'s own doc comment already names). `litebox_shim_linux::
diag::set_strace_summary_enabled` added (the real runtime toggle -- `init_strace_summary` is a
one-shot latch despite its own doc comment's "idempotent" phrasing suggesting otherwise).

**Live-verified this session** (release build, real named pipe, no test files): `advisor/probes/
dup_probe.c` reconfirmed live (mingw gcc) -- `DuplicateHandle` into a same-user non-admin sibling
still works, matches ADVISORY-001 §5's 2026-09-03 finding. Headless (no `--gui`, local tar,
`bin/sleep`): `presenter?`→`ok none`, `strace query`→`ok off`, `frames on/off`→`ok`, `scanout`→
`err bad_state` (no fb attached), `key`/`rel`→`ok`, `abs`→`err unsupported` -- all live over the
real pipe via a PowerShell `NamedPipeClientStream` script (design doc §3's own suggested
debug-tooling shape). This is scenario 2 AND 6 from §6's plan. `--gui=hidden`: `litebox-presenter.exe`
spawns (confirmed via `Get-Process`, several runs). `show` with no drawing guest: blocks ~5.08s
then `err io_error presenter did not start` -- exactly §5 risk 3's 5s contract, live-timed.
Presenter cleanup: found live that a panic on a non-main Rust thread only kills that thread, not
the process -- an orphaned zombie `litebox-presenter.exe` resulted when its scanout-retry thread
hit "runner closed the connection" while the main thread's winit loop kept running. Fixed with a
process-wide panic hook (`litebox_presenter/src/main.rs`) that exits after the default hook
prints; reconfirmed live afterward -- presenter now exits the instant the runner's pipe breaks.

**Live-verified in a follow-up session (2026-09-16, real flip-producing guest)**: built
`drmgui_multiflip.hooked` per `docs/dump-frames-writer-verify-probe/README.md`'s exact recipe and
ran all three previously-open scenarios against it. **Scenario 1**: `LITEBOX_DUMP_FRAMES=1`, 21
flips -> 21 `.bmp` files, `non_black_pixels=2073600`, `0 dropped` -- byte-identical to the
2026-09-05 baseline. **Scenario 3/4**: `--gui=hidden` + `show` against a real flip-producing guest
-- presenter registers, `show` replies `ok`, presenter survives, `presenter?`->`ok visible`,
`EnumWindows` finds a real visible `"litebox virtual display"` window, `PrintWindow` capture shows
real rendered content (not blank). **Scenario 5**: killed the presenter mid-display -- guest/
`screenshot` unaffected (`non_black_pixels=2073600` throughout), a follow-up `show` spawned a
fresh presenter that got its own real visible window with current content within ~370ms -- true
respawn-and-resume.

**Real bug found and fixed this pass**: the first-ever live `show` against a REAL content-producing
guest (every earlier session's `show` test used a guest with no drawn framebuffer, hitting only
the timeout path) made `litebox-presenter.exe` silently `exit(0)` moments after `show`, no panic.
Root cause in `litebox_presenter_protocol::pipe` (shared client+server named-pipe I/O): every
handle had `FILE_FLAG_OVERLAPPED` set but every `ReadFile`/`WriteFile` passed a NULL `OVERLAPPED`
pointer -- unsound once more than one thread has I/O in flight on the same pipe object at once,
which is exactly this module's own `show`/`hide` design (one thread blocked reading a presenter's
connection while a different thread writes `show`/`hide` through a `duplicate_into_current_process`
duplicate of the same handle). Live effect: the pending read spuriously saw `ERROR_BROKEN_PIPE`
right after the concurrent write succeeded. Fix: `litebox_presenter_protocol::pipe::overlapped_call`,
a private per-call `OVERLAPPED` + manual-reset event for every `ReadFile`/`WriteFile`/
`ConnectNamedPipe` (new `Win32_System_Threading` feature on that crate's `windows-sys` dep), which
is what `FILE_FLAG_OVERLAPPED` is actually for -- applied to both server and client (client's
`CreateFileW` also gained `FILE_FLAG_OVERLAPPED`, closing the same latent hazard for future
input-forwarding writes). Simply removing `FILE_FLAG_OVERLAPPED` instead (tried first) "fixes" the
crash but deadlocks the write forever behind the permanently-pending read -- do not retry that
half-fix; the earlier "First fix attempt" paragraph above (this same file, ~180 lines up) has the
full kernel-level reasoning for why. Live-reconfirmed: `show` now replies in ~300ms, presenter
survives indefinitely.

## Ninth ACK-stall-kill candidate: write-side backpressure through the video pipeline (2026-09-16)

No boot attempted this pass -- selkies' 0/7 recent data-socket binds (ET_EXEC finding above)
already made a live repro unlikely before starting, so this was a pure code-level audit of all
three write-path layers between pixelflux's encoder output and the browser: litebox's `--publish`
NAT gateway, pixelflux's delivery thread, and selkies' own websocket send path. Sources for the
latter two aren't vendored in this repo -- fetched live: `selkies.py` from
`selkies-project/selkies@348bc4f61da66198573e7e57db9a266aca1991d5` (`src/selkies/selkies.py`, the
exact pin `docker-baseimage-selkies` uses, confirmed by the 3757-line count matching the
2026-09-15 investigation's own count), `lib.rs` from `linuxserver/pixelflux` (`pixelflux/src/
lib.rs`, master), and `connection.py` from `python-websockets/websockets` (`src/websockets/
asyncio/connection.py`, main -- the real library selkies imports, confirmed via `selkies.py:61`'s
`import websockets.asyncio.server as ws_async`).

**All three layers are individually correct; none blocks an event loop or a hot capture/encode
path on network state.**

- **`net.rs`'s write side** (`litebox_platform_windows_userland/src/net.rs`): `pump_tcp_flows`
  (`:526-602`) buffers at most one ~4096B chunk in `pending_to_real`/`pending_to_guest` on
  `WouldBlock` (real sockets are nonblocking on both the outbound-connect path, `:488`, and the
  inbound-accept path, `:820`) and retries it on the next 5ms tick -- and critically STOPS calling
  `socket.recv_slice()` on the guest-facing smoltcp socket while that pending buffer is nonempty
  (`:541`), so the smoltcp socket's own 256KB RX ring fills and its advertised TCP window correctly
  shrinks toward zero, propagating real backpressure all the way to the guest's own kernel TCP
  stack -- exactly the "does it apply real backpressure or drop/corrupt" question this investigation
  needed answered, and the answer is real backpressure, correctly. `LoopbackQueue` (`:112-117`,
  flagged in an earlier pass as "unbounded and cloned in full every 5ms tick") is structurally an
  uncapped `VecDeque`, but nothing pushes into it without first passing through a bounded (256KB)
  smoltcp socket buffer above it, so its practical growth is bounded by that, not itself a leak or
  backpressure hazard -- the earlier "flagged but not proven causal" note is now resolved: not
  causal, and not effectively unbounded either.
- **pixelflux's delivery thread** (`lib.rs`, fetched from upstream `master`, 4232 lines --
  smaller than the 2026-09-15 session's "8381-line" count, consistent with upstream having moved
  on since; mechanism below unaffected by the size difference): the X11 capture path's
  `on_frame` closure does a REAL blocking `deliver_tx.send()` into a 1-slot `sync_channel`
  (`:3691,3723-3727`) -- but this can only block the dedicated pixelflux capture OS thread, never
  selkies' Python/asyncio thread, because the delivery thread's own `cb.call1(py, (f,))` invokes
  `queue_data_for_display` (`selkies.py:3130-3149`), which does only a `memoryview` wrap and
  `self.capture_loop.call_soon_threadsafe(do_put)` -- a fixed-cost, always-immediate,
  network-state-independent handoff (the actual `asyncio.Queue.put_nowait`/`QueueFull` check
  happens later, inside `do_put`, scheduled to run ON the event loop, not inside this call). The
  GPU/Wayland encode path (`:2784-2807`) is even more conservative and explicitly comments on
  exactly this hazard: it never blocks the calloop thread at all, using `try_send` and parking one
  pending frame (dropping no encoded data, since an encoded frame is part of the H.264 reference
  chain) rather than risk freezing input/Wayland dispatch on a stalled Python consumer.
- **selkies' own websocket send path**: both `send()` and the keepalive `ping()` route through
  the real `websockets.asyncio` library's `send_context()` (`connection.py:860-927`), which does
  `self.send_data(); await self.drain()` (`:914-915`) -- genuine per-connection flow-control-aware
  backpressure (`pause_writing`/`resume_writing`/high-water-mark, `:1049-1078`), never a raw
  blocking socket call. Critically, `keepalive()` (`:803-849`) only starts the `ping_timeout`
  countdown AFTER `await self.ping()` returns (`:822-828`) -- and `ping()` itself goes through the
  same drain-aware `send_context()` -- so a momentarily-full send buffer at the moment a ping is
  due does NOT by itself cause a spurious "keepalive ping timeout": the ping-send call absorbs
  whatever backpressure exists first, and only then does the 20s pong-wait clock start. Selkies
  also has its own application-level defense independent of all of this: a bounded
  `asyncio.Queue(maxsize=120)` per display (`BACKPRESSURE_QUEUE_SIZE`, `selkies.py:3176-3177`)
  between the capture callback and `_video_chunk_sender`, with `QueueFull` silently dropping the
  new frame (`:3143-3147`) rather than ever blocking anything upstream.

**The real, remaining, evidence-backed mechanism -- ruled IN as plausible, not confirmed live.**
Ping and video-frame bytes share ONE ordered per-connection TCP byte stream and ONE asyncio
transport buffer; WebSocket has no separate control-frame channel at the transport level.
`send_data()` (`connection.py:914`) writes an ENTIRE frame's bytes into that buffer unconditionally
BEFORE the drain/high-water check that follows it on the next line -- so a single oversized
`await websocket.send(data_chunk)` call for one IDR/keyframe (explicitly triggerable on demand via
`request_idr_frame()`, `selkies.py:3113`, e.g. on reconnect or a display resize) can push the
transport buffer far past its flow-control threshold in one shot, before any drain-based pushback
has a chance to apply. If the real, achievable throughput from server to browser stays low enough
for long enough afterward -- for any reason: this same day's own independently-documented host
memory-pressure instability (AGENTS.md's "watch `FreePhysicalMemory` live... less stable than that
baseline implies"), a throttled/backgrounded browser tab, or genuine network/loopback contention --
that the backlog cannot physically drain within the 20s `ping_timeout` window, then the ping's own
on-wire delivery, and therefore the pong's return, genuinely cannot make the deadline. This is not
a bug in litebox, pixelflux, or `websockets` individually; it is an emergent property of a single
shared-stream WebSocket connection's keepalive under SUSTAINED backpressure, and it precisely fits
the symptom's own "20-60s", not-exactly-periodic timing (load-dependent delay stacked on the fixed
20s interval, rather than a fixed-interval bug).

**Precise repro condition for when the stack is next bootable, not yet attempted**: throttle
host->browser bandwidth (Windows QoS policy, or read the client side of the websocket slowly/
pause reads to simulate a slow consumer) to below pixelflux's realistic encoder output rate,
sustained for >20s, ideally while forcing an IDR (resize or reconnect) partway through the
throttle window to inject one oversized single-frame write -- watch for `keepalive ping timeout`
appearing well inside that window rather than only at a `ping_interval` boundary. Do not re-chase
this by reading `net.rs` or `pixelflux` again without new evidence -- both are now confirmed
correct for backpressure specifically (not just "nonblocking," which was the prior pass's scope);
the open question is purely about ACHIEVABLE THROUGHPUT under real load, not a code defect in any
of the three audited layers.

## Ping-starvation: sharper root cause found and fixed (2026-09-16, follow-up session)

Re-fetched the same pinned sources (`selkies.py`@`348bc4f61da66198573e7e57db9a266aca1991d5`,
3757 lines, matching count; `connection.py` from `python-websockets/websockets@main`) to build a
concrete fix rather than only characterize the gap. Found a cleaner, upstream-documented mechanism
that supersedes the prior pass's "one oversized IDR frame beats drain to the punch" framing -- same
bug CLASS (video-frame backlog can starve the ping), but a sharper, more directly fixable cause.

**`_video_chunk_sender`'s `'primary'` branch never actually respected backpressure, by any layer.**
`selkies.py:3063` (pinned commit) sends via `websockets.broadcast(primary_viewers, data_chunk)`.
`websockets.asyncio.connection.broadcast()`'s own docstring (`connection.py:1172-1178`) is explicit:
"pushes the message synchronously to all connections even if their write buffers are overflowing.
There's no backpressure. If you broadcast messages faster than a connection can handle them,
messages will pile up in its write buffer until the connection times out." Confirmed in the
implementation (`connection.py:1235-1239`): `getattr(connection.protocol, send_method)(message);
connection.send_data()` -- no `await self.drain()`, ever, for a broadcast. This is a deliberate
library tradeoff for many-viewers-at-once efficiency, not a bug in `websockets` -- but selkies calls
it for `'primary'`, the ONLY display mode a single-client webtop deployment like this one ever
actually uses (confirmed against `webtop_stack.sh`'s single-Xvfb-display setup and AGENTS.md's
"one client per selkies instance" note), so it is the actual production send path, not an edge
case.

**Worse: selkies' OWN app-level backpressure system is silently disconnected from that path.**
`_run_frame_backpressure_logic` (`selkies.py:1196-1267`) is a real, working, fast-reacting detector
-- `BACKPRESSURE_CHECK_INTERVAL_S = 0.5` (`:9`), `STALLED_CLIENT_TIMEOUT_SECONDS = 4.0` (`:14`) --
that computes frame desync from client-ACKed vs server-sent frame IDs (RTT-adjusted) and sets
`display_clients[id]['backpressure_enabled'] = False` on either a >4s ACK stall or an
allowed-desync breach, logging `"Backpressure TRIGGERED for '{display_id}'"` /
`"Client stall ... Forcing backpressure"`. The **secondary**-display branch of
`_video_chunk_sender` (`selkies.py:3069-3072`) correctly gates its send on this flag: `if not
client_info or ... or not client_info.get('backpressure_enabled', True): continue`. The
**primary** branch (`:3053-3061`, a few lines above the broadcast call) reads the exact same flag
per viewer -- but only to decide whether to update `sent_timestamps`/`last_sent_frame_id`
bookkeeping, never to skip the send. The broadcast call two lines later
(`websockets.broadcast(primary_viewers, data_chunk)`) unconditionally includes every viewer in
`primary_viewers` regardless of their `backpressure_enabled` state. This reads as a copy-paste/
refactor asymmetry (the primary branch clearly USED to intend the same gating, given it computes
the identical flag) rather than an intentional design difference -- and it means the one
production-relevant display mode had a real backpressure system whose signal was computed but
never consumed by the send path, while the actually-executed path (`broadcast()`) additionally has
zero library-level backpressure of its own. Two independent safety nets, both absent for the path
that matters.

**Consequence, precisely**: a primary client that falls behind (stalled ACKs, or growing frame
desync) keeps receiving every dequeued frame from the bounded `asyncio.Queue(maxsize=120)`
(`BACKPRESSURE_QUEUE_SIZE`, `selkies.py:3176-3177`) via `broadcast()`, each one written directly
into that connection's transport buffer with no drain wait -- so the backlog can grow to the full
120-frame queue depth (potentially several MB of H.264 data at typical webtop bitrates) before the
upstream queue's own `QueueFull`-drop even engages. A ping due during that window queues its own
tiny frame behind that backlog on the SAME ordered TCP byte stream (WebSocket has no separate
control-frame channel), and if the backlog can't drain within `ping_timeout` (20s), the pong
genuinely can't return in time -- killing an otherwise-healthy connection. This is a strictly
worse (larger, more directly forced) version of the prior pass's "one big frame" mechanism, now
tied to a concrete, provable code asymmetry instead of a timing coincidence.

**The fix** (two changes, both confined to `_video_chunk_sender`'s `'primary'` branch, applied via
`advisor/patches/selkies_primary_backpressure_patch.py`, committed to this repo; see that file's
own docstring for the full text-level diff):

1. Actually gate the `broadcast()` call on `backpressure_enabled`, matching the secondary branch's
   existing correct behavior. This alone lets the already-working 0.5s/4s-reacting ACK-desync
   detector stop feeding a falling-behind client before its backlog can grow unbounded -- for the
   documented "sustained backpressure for >20s" symptom, this detector fires within 0.5-4s, an
   order of magnitude before `ping_timeout` could ever be threatened.
2. Defense in depth for the 0.5-4s gap before that detector reacts: check each viewer's real
   `transport.get_write_buffer_size()` (a live `asyncio.Transport` method --
   `Connection.transport` is a genuine `asyncio.Transport` per `connection_made()`,
   `connection.py:1013`) and skip that one frame for that one client if already backlogged past
   `VIDEO_BACKLOG_DROP_THRESHOLD_BYTES` (default 256KiB, env-tunable via
   `SELKIES_VIDEO_BACKLOG_LIMIT_BYTES`). 256KiB was chosen, not measured live: it drains in ~2s at
   a modest 1Mbps and ~16s even at a barely-functional 128kbps -- comfortably inside `ping_timeout`
   for any connection that isn't already effectively dead, while staying well above a typical
   1280x800 H.264 keyframe so ordinary IDR frames aren't spuriously dropped under merely transient
   jitter. Neither change touches `websockets`' own `send_data()`/`drain()`/`ping()`/`keepalive()`
   code (third-party, pinned, correct on its own terms) -- only selkies' own choice of which bytes
   to hand it.

**Applied**, not yet live-verified: selkies' 0/7 recent data-socket binds (the unrelated ET_EXEC
finding above) made a live repro unlikely before starting, so this was built and verified
offline against the real fetched source: the exact `OLD_BLOCK`/`CONST_ANCHOR` text match was
confirmed unique (`grep -c` on the distinguishing `websockets.broadcast(primary_viewers,
data_chunk)` line = 1) against the pinned `selkies.py`, the substitution was applied and the
result round-tripped through `ast.parse()` successfully, and a diff of the patched file against
the original showed exactly the intended, minimal change (two hunks: one new module constant,
one rewritten branch body) with no incidental drift elsewhere in the 3757-line file. The patch
script's own guest-side file-location step (`import selkies.selkies as m; m.__file__`) could not
be fully exercised on this Windows host (selkies' real dependency chain -- `pixelflux`, `pcmflux`,
`GPUtil`, `aiohttp`, `PIL`, etc. -- isn't installed here), but that step is a standard, low-risk
Python idiom; the load-bearing correctness claim (the text transform itself) was verified directly
against the real file.

**Wiring**: `.wfgy/webtop_stack.sh` (gitignored, local-only) embeds an inline copy of this exact
patch and runs it right after `DBUS_UP`/the `dbus-launch` shim, before the selkies supervisor loop
first launches `selkies` -- i.e. before the target file is ever imported by a running process. The
patcher is idempotent (a `PING-STARVATION FIX (2026-09-16)` marker short-circuits a re-run) and
refuses to touch the file at all if the exact pinned block isn't found verbatim (reports
`SELKIES_PATCH_SKIPPED reason=source_mismatch` rather than risk corrupting a drifted version).

**Verification once a stable boot exists again** (see the ET_EXEC/Track-B blocker above for why
none was attempted this pass): throttle host->browser bandwidth below pixelflux's realistic
encoder output rate (Windows QoS policy, or read the client side of the websocket slowly to
simulate a stalled consumer), sustained for >20s, ideally forcing an IDR (resize or reconnect)
partway through to inject one oversized single-frame write. Pre-fix, expect `keepalive ping
timeout` in `sk.log` inside that window. Post-fix, expect to see `Backpressure TRIGGERED for
'primary'` (now load-bearing on the actual send, not just a log line) and/or
`SELKIES_VIDEO_BACKLOG_LIMIT_BYTES`-driven frame drops, with NO ping timeout while the throttle
holds -- frame drops and a visibly stalled/frozen video during the throttle window are expected
and correct in that state, not a regression.

## Cross-process-capable `RawMutex`: full mechanism internals (Track B step 2, 2026-09-16)

Compacted out of the main `AGENTS.md` entry for space; that entry keeps the done/verified summary
and the Track B step 3 pointer, this is the internals a maintainer needs before touching the code.

`litebox_platform_windows_userland/src/lib.rs`'s `RawMutex` (~5905-6300) replaced
`WaitOnAddress`/`WakeByAddressSingle` (process-local per MSDN) with a manual wait queue
(`waiters: Mutex<Vec<WaiterRecord>>` per `RawMutex` instance) plus one auto-reset kernel `Event`
per OS THREAD, not per mutex (`thread_waiter_event`, a new `thread_local!`, cached for that
thread's whole lifetime -- so a thread that waits on many different mutexes over its life reuses
one event rather than allocating a fresh kernel object per wait). Same trait, same
`underlying_atomic()`/`INIT` contract; no caller changed.

**Lock-ordering / lost-wakeup avoidance**: register (push into the queue) and check
(`underlying_atomic() != val`) happen under the SAME lock `wake_many` takes to pop waiters --
closing the lost-wakeup window the same way `xproc_sync.rs`'s swap-based protocol does (a waiter
can never miss a wake that happens between its check and its registration, because both steps and
the wake are serialized through one lock).

**Timeout-race resolution**: a wait that times out just as `wake_many` pops that same waiter is
resolved by re-acquiring the queue lock: still-queued means genuinely timed out (remove self, no
signal was ever sent); already popped means `wake_many` already committed to `SetEvent` on this
waiter's event, so the recovery path does one more bounded wait to consume that pending signal
rather than leaving a stray `SetEvent` on a per-thread event this thread will reuse on its next
wait (a leaked signal there would cause the NEXT unrelated wait on this thread to return
immediately with a false "woken" result).

**`wake_many` return value**: now returns the real count of waiters it popped and signaled
(previously always `0` -- Windows genuinely couldn't observe this via `WakeByAddress*`, which has
no return value). The trait contract allows either 0-or-real-count, and every existing caller
(`sync/mutex.rs`, `sync/rwlock.rs`) was already written to be correct under the old always-`0`
behaviour, so returning the real count is a pure improvement, not a behaviour requirement change
-- no caller needed updating.

**Ratchet**: `dev_tests/src/ratchet.rs`'s bare-static count for this crate bumped 18->19 for the
one new `thread_local!` (`THREAD_WAITER_EVENT`). Deliberately a plain `thread_local!` rather than a
`TlsState` field (unlike `codewatch`/`ctxwatch`, which deliberately avoid adding to this ratchet)
because `RawMutex` is reachable from host-only threads that never call `install_tls`, so a
`TlsState`-backed field would be unreachable/panic on exactly the threads this code needs to run on.
