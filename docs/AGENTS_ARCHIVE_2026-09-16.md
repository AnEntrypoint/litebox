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
