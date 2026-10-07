# AGENTS.md archive — detail drained 2026-09-15

Two drain passes today, both because `AGENTS.md` crossed its 30KB compaction threshold. This file holds
what was drained out in each: detail that is still true and still occasionally useful, but that a session
does not need in order to know the current state. Claims that a later commit *superseded or refuted* were
deleted outright rather than archived here — keeping a refuted claim anywhere is how the "one genuinely
open host crash" survived five days past its fix (`271cbb5`).

**First pass** (32,268 bytes) drained the sections immediately below, through "Superseded framings
deleted in this pass." **Second pass** (78,590 bytes, after a full-day multi-session hunt for the
browser-stream disconnect bug added the ACK-stall-kill investigation) drained the "Second drain pass, same
day" and "Guest-reachable-code panic fixes" sections below that.

Everything below carries its proving commit sha or `file:line`. Earlier drains:
`docs/AGENTS_ARCHIVE_2026-09-10.md`, `_2026-09-05.md`, `_2026-09-03.md`.

## Reading a cross-process fork log

On an identity fork, `fork_verify` emits "stale CODE pointer detected, translating and resuming" with
`translated_rip == rip` — 84,319 of ~90,400 lines in one run, zero real translations, bounded per child by
`MAX_IDENTITY_VERIFICATION_STEPS = 4096`. Wasteful, not corrupting; without knowing this you will chase it.

## Trampoline-extension poisoning: the blow-by-blow (`6311f74`)

`AGENTS.md` keeps the mechanism and the fix in one paragraph; this is the live evidence behind it.

The runtime patcher's initial trampoline allocation was a flat one-page guess. When a segment needed
more stub space than one page held, the extension path tried to map at exactly one fixed adjacent
address with `MAP_FIXED_NOREPLACE` and no fallback — unlike the *initial* allocation a few lines above
it, which already had a try-fixed-then-let-the-VM-choose path. Any unrelated mapping already occupying
that one address therefore failed the extension outright, and `apply_trap_fallback` then poisoned
**every** `syscall` site in the whole segment with `ICEBP;HLT`, regardless of how many were otherwise
patchable. The guest died on the first syscall it executed after load.

Witnessed live by pulling and booting `docker.io/edgelevel/alpine-xfce-vnc:latest` fresh through
`--oci-image` (no packager step): busybox `/bin/sh` took SIGILL within 3s of exec
(`[diag-ud-entry] raw_code=0xc0000096`), with 480 sites poisoned by a single failed 4KiB extension.

The fix sizes the initial allocation from a cheap `0F 05` byte-pair count over the segment — a sound
upper bound on patchable syscall sites, the same technique the rewriter's own fast-reject scan already
relies on — capped at 4MiB. Re-run clean, zero fatal signals. The exposure generalizes: any real binary
with a few hundred `syscall` sites (ordinary, not a busybox peculiarity) was affected.

## `edgelevel/alpine-xfce-vnc`, verified against its canonical registry layer

Alpine 3.16.0. Ships `Xvfb`, `x11vnc` and `novnc_server` plus the full `xfce4-session`/`xfwm4` set — a
noVNC-over-browser pipeline. Same X-server category (Xvfb, not Xorg/DRM) as the already-verified
`alpine-mate` selkies boot, so there is no DRM/KMS+wgpu path for this image to misalign with; it lands
squarely in the already-settled Xvfb/browser pipeline, not the `--gui` one.

## The three cross-process-fork correctness bugs the perf work exposed

`AGENTS.md` names the three shas in one line. The mechanisms:

- **`060ccc3`** — no `SIGCHLD` reached a parent from a cross-process fork child, so any parent using the
  race-free mask-then-`sigsuspend` wait hung forever rather than waking on the child's exit.
- **`6e86a40`** — `sys_wait4(pid=-1)` consulted the cross-process child registry only when the
  thread-based registry was already empty, so a process with both kinds of child never reaped the
  cross-process one. This is also the fix for the curl-self-test stall that `1f30ab4` narrowed; do not
  cite `1f30ab4` as live open work.
- **`d5cc744`** — a redundant claim release deleted a coalesced `CLAIMED_RANGES` slot, and with it the
  collision coverage for a loaded library.

## Why the "one genuinely open host crash" claim outlived its fix

Recorded because the failure mode is procedural, not technical, and will recur.

`0473cc3` closed the `RtlpUnwindPrologue` crash on 2026-09-08 by fixing `VEH_FRAME_STRIDE`. The claim
that it was still open survived to 2026-09-10 because `8fc102a` recompiled `AGENTS.md` from
pre-`0473cc3` archive text without re-running the repro, and then to 2026-09-15 because each subsequent
pass carried the section forward unchallenged. `271cbb5` closed it by bisecting live rather than by
reading. The lesson is the compaction rule itself: a recompile that copies a claim forward without a
witness launders a stale claim into a fresh-looking document.

## Retired: the 2026-09-09 test-suite status note

`AGENTS.md` keeps only the standing rule ("never record a test count you did not just watch run to
completion, and never leave a suite red for an environmental reason"). The counts it was derived from
are deliberately not carried anywhere — a suite is not evidence of anything and this project does not
keep test files. The underlying findings that were *real* are already recorded elsewhere on their own
merits: the panic-becomes-host-crash handler defect (`78dda05`, in `AGENTS.md`'s crash-machinery
section), and `lstat` failing to walk an intermediate symlink on any usrmerge layout.

Full original text: memory `mem-14fccd59cddec385-2382`.

## Second drain pass, same day — the ACK-stall-kill investigation blow-by-blow

`AGENTS.md` grew back to 78.6KB after the first drain above, entirely from a multi-session hunt for the
browser-stream disconnect bug (the "ACK-stall-kill"). `AGENTS.md` now keeps only the one-line verdict per
candidate plus the still-open thread; this section holds the full evidence and methodology behind each,
in the order investigated. The root cause remains UNIDENTIFIED after all ten — do not treat this section
as containing a fix; it is a record of what was ruled out and how, so nobody re-runs the same measurement.

**Candidate: client JS/transport (refuted).** A hand-rolled, protocol-only WS client (raw `socket`, no
canvas/decode, reflects every incoming `PING` as an immediate `PONG`, `reflect_latency_ms=0.00`) connected
directly to `ws://127.0.0.1:3000/websocket` and stayed open cleanly for a 150s test window, surviving 7
consecutive 20s ping cycles with zero disconnects — while real Chrome tabs on the identical endpoint at
the identical time died on their usual ~20-60s cycle. A client physically incapable of "a busy JS main
thread missing its ACK timer" survives fine; real browsers do not.

**Candidate: nginx config / frontend "dual-connect" (refuted, retracted).** PRD row
`selkies-dual-connect-per-pageload-pending-frontend-trace` was mis-framed. Reproduced `webtop_stack.sh`'s
exact `sed` pipeline against the stock `default.conf` template and diffed the result: `location
/websocket` is nginx's longest-prefix-wins match even for a `/websockets` (plural) request, so the
trailing `s` some client paths use does not 404 by itself — no config bug. The frontend is the stock,
unmodified selkies dashboard (instrumented live via a `window.WebSocket` wrapper): it opens exactly ONE
`data_websocket` per page load, same as any stock deployment. The real, still-live mechanism is the
ACK-stall-kill itself: `sk.log` shows a continuous cycle of `Legacy client ... connected` then, ~tens of
seconds later, `Data WS closed with error ...: sent 1011 ... keepalive ping timeout` — the dashboard's own
reload-on-disconnect logic then reloads and reconnects, and because a page navigation never sends a clean
WS close frame, the pre-reload socket lingers server-side until the fresh one preempts it (`Killing old
client for 'primary' ... reason: a new primary client connected`) or times out — which is the entire "two
`Legacy client` registrations per reload" pattern. That pairing is the *expected* by-design behavior of
any selkies reconnect; real `docker run` deployments show the identical pairing on every reload. What's
deployment-specific here is only that the keepalive/ACK-stall fires every reload cycle instead of never.

**Candidate: `/proc/<pid>/cmdline` ENOENT syscall cost (refuted).** The original 19/19-correlated marker:
all 19/19 `keepalive ping timeout` closes in one 100+min session's `sk.log` had a burst (1-8 lines,
mode=8) of litebox's `[diag-proc-sys-open-miss] unregistered path opened: /proc/<pid>/cmdline errno=2`
landing in the handful of lines immediately before it — 100% correlation. Measured directly (a temporary
`litebox::platform::Instant` wrapper around `do_open_resolved`'s `do_open()` and
`diag_raw_print_proc_sys_open_miss`, reverted after measuring): `do_open()`'s ENOENT resolution costs
3800-5800ns; the diagnostic print costs 1200-4200ns; ~5-10us total per occurrence, ~40-80us for a full
8-line burst — 5-6 orders of magnitude below the ~20s `keepalive_ping_timeout` window.
`Procfs::walk_directories` (`litebox/src/fs/procfs.rs:344-365`) rejects an unrecognized first path
component via a linear scan over a fixed 7-entry `ProcfsEntry::ALL` array — no process-table walk, no
lock, no host syscall, immediate `ENOENT`, confirming why it's cheap. The correlation is real but is a
marker, not the cause.

**Candidate: selkies' own psutil tick (refuted).** Pulled the exact deployed selkies source
(`docker-baseimage-selkies` pins `selkies-project/selkies@348bc4f61da66198573e7e57db9a266aca1991d5`, a
pre-refactor single-file `selkies.py`, NOT current upstream `main` which already wraps this in
`asyncio.to_thread`). At the pinned commit, `_collect_system_stats_ws` (`selkies.py:3273-3291`,
`interval_seconds=1`) calls exactly two psutil functions directly on the coroutine, no
`run_in_executor`/`to_thread`: `psutil.cpu_percent()` and `psutil.virtual_memory()` — confirmed by
grepping the whole 3757-line file, no `process_iter`/`Process()`/per-pid calls exist anywhere. Measured
live under litebox (isolated one-shot boot, 200-iteration loop, no fork so the tcache class is
side-stepped): `psutil.cpu_percent()` = 26.0us, `psutil.virtual_memory()` = 48.9us, combined = 74.8us/tick
— ~5-6 orders of magnitude below the timeout window.

**Candidate: `GPUtil.getGPUs()` (refuted).** Pinned source
(`/lsiopy/lib/python3.13/site-packages/GPUtil/__init__.py`) confirms the call at `selkies.py:1701` is
genuinely synchronous on the event loop, firing once per client (re)connect plus once at
`_collect_gpu_stats_ws` task start (`selkies.py:3300`, then returns immediately since `gpus` is empty).
`nvidia-smi` is NOT absent as assumed — `GPUtil`'s `Popen(["nvidia-smi", ...])` really forks and attempts
to exec `/usr/bin/nvidia-smi` every call (confirmed: that path does not exist, `ENOENT`), so every call is
a genuine fork()+failed-exec()+cleanup cycle. Measured live (isolated one-shot boot, same
`--oci-image docker.io/linuxserver/webtop:debian-xfce`, N=20 timing loop): first call 0.975ms, avg=0.644ms,
median=0.608ms, min=0.588ms, max=0.852ms — confirmed by litebox's own process-tree dump showing exactly 21
real fork attempts. Still ~4-5 orders of magnitude below the 20s window. Also confirmed while here: the
`keepalive ping timeout` message comes from the `websockets` library's own default protocol-level keepalive
(`ping_interval=20, ping_timeout=20`, `selkies.py:2708-2709`, not a selkies customization), and
`_run_frame_backpressure_logic` (the `Client stall for 'primary': No ACK in N.Ns` warning's source,
`backpressure_check_interval_s=0.5`, `selkies.py:1204`) is pure arithmetic on frame-id/timestamp state with
zero subprocess calls — a downstream symptom detector, not a cause.

**Candidate: pixelflux capture/encode hot path (refuted).** `selkies.py`'s own capture call site
(`_start_capture_for_display`, `:3091-3200`) invokes `capture_module.start_capture` exactly ONCE per
display via `run_in_executor`, never a per-frame call on the coroutine. `pixelflux` (PyO3 0.29.2, fetched
live from its real upstream source, `pixelflux/src/lib.rs`) has an explicit three-thread architecture —
Capture/Compositor, Encode, and a dedicated Delivery thread that owns the Python callback. Grepping the
8381-line source: zero pre-0.29 `allow_threads`/`with_gil`, 20+ `py.detach()`/`Python::attach()` calls
wrapping capture/encode work; the callback invocation (`cb.call1`, `lib.rs:2316`/`6832`) is the ONLY place
the GIL is reacquired, inside `Python::attach` on the delivery thread specifically. Source comments state
the design intent directly: "the Python frame callback runs on a dedicated delivery thread so its GIL
never stalls calloop input/control dispatch" (`lib.rs:4345-4346`), "one GIL acquisition per frame with all
stripes batched" (`lib.rs:6710`). The callback itself (`queue_data_for_display`, `selkies.py:3140-3162`)
does only a `memoryview` wrap, a small dict literal, and `call_soon_threadsafe` — no encode/capture work
inside the GIL-held window. Live corroboration: reused an already-running stack, connected a real Chrome
tab, observed continuous rendering for a multi-minute window before a keepalive-timeout-style disconnect
fired spontaneously **while the desktop was idle** (a Thunar window open, mouse motion only, no
pixel-diff/encode load) — the opposite correlation a genuine encode-hot-path cause would predict.

**Candidate: litebox's own NAT/`--publish` gateway (refuted).** Full read of `net.rs` (all 1068 lines):
the whole gateway (outbound NAT and every `--publish` flow) runs on ONE dedicated thread
(`litebox-nat-gateway`, `:902-915`), fixed loop `drive()` then `sleep(5ms)` (`:912`). For a `--publish`
flow, `pump_tcp_flows` (`:507-665`) processes guest→real (video out) then real→guest (pong in) per flow,
but every operation on both sides is nonblocking — `real` is `set_nonblocking(true)` at accept (`:820`),
every `smoltcp` buffer op is nonblocking by construction, and a `WouldBlock` on either side breaks that
side's loop immediately (`:537`, `555-557`, `596`) rather than stalling the thread. A full 256KB
`SOCKET_BUFFER_SIZE` receive buffer of queued video costs at most ~64 nonblocking `write()` calls before
the same function reaches the real→guest block for that flow, all within one 5ms tick. Worst-case added
latency for an inbound pong: one ~5ms tick plus a handful of sub-ms syscalls — not the seconds-to-tens-of-
seconds this symptom needs. One real but unconfirmed-causal inefficiency, flagged for cleanup not fixed:
`LoopbackQueue` (`:112-117`) has no size cap and `ensure_listeners_for_queued_packets` (`:391-417`) clones
the entire pending `to_gateway` backlog every single 5ms tick — pure in-memory O(n) `Vec<u8>` clone
overhead, not a blocking-read-behind-write coupling. Live corroboration: 3+ continuous minutes with zero
new console lines on an already-running stack (passive/idle-level load only, since this session's
synthetic input never reached the canvas).

**Candidate: fork_verify thread-based healing (NOT confirmed, NOT cleanly refuted — real bug found and
fixed within it).** Grepped the whole `litebox_platform_windows_userland` crate for
`SetThreadPriority`/`SetPriorityClass`/`SetThreadAffinityMask`/`SetProcessAffinityMask`: zero hits — every
guest thread is an ordinary `std::thread::Builder`-spawned OS thread under Windows' own preemptive
scheduler. The only process-wide lock reachable from healing, `VIRTUAL_PROTECT_LOCK` (`lib.rs:8562`), is a
microsecond-scale critical section per heal, not stop-the-world. `ThreadHandle::interrupt`/`ctxwatch`
suspend only one explicitly targeted thread each, never system-wide. The real defect: `on_single_step`
case (1) (`fork_verify.rs:1048-1060`) translates a stale `rip` and resumes on every single-step trap but,
unlike the AV-path's sibling cases (3)/(4) which patch the stale slot in memory, case (1) never patches the
underlying code/pointer — a guest loop re-entering the same unhealed address pays a fresh ~600us
single-step trap every iteration. Measured live: one thread hit the identical `rip`/`translated_rip` pair
357 consecutive times across 199.330s-199.546s (216ms, ~605us/heal). At the `MAX_THREAD_VERIFICATION_
STEPS=16384` cap this is ~9.8s of single-thread CPU-bound trap handling for ONE fork — a real "pro rata"
violation with no counterpart under real Linux `fork()`, regardless of whether it explains the ACK-stall
symptom. Fixed `b6ddf43`: case (1) now tracks `(rip, translated_rip)` repeats per-thread
(`TlsState::fork_verify_step_rip_repeat`) and, past `STEP_RIP_LIVELOCK_THRESHOLD=8`, invokes the same
deeper healers the AV path uses (`translate_stale_source_indirect_call_target`,
`translate_stale_source_register_indirect_call_target`). Honest limitation: one thread hit 46 consecutive
identical heals where the escalation ran but none of the three deeper healers found a patchable slot for
that shape — no worse than pre-fix, just not fully closed for every case. Stress test (`b6ddf43`'s own
verification pass, same day): booted the real webtop stack with `fork_verify=warn` and a concurrent
guest-side `/bin/sh -c` loop fork+exec'ing `/bin/true` every 0.3s for the ENTIRE session, connected a real
Chrome tab — 11+ minutes of continuous, disconnect-free streaming (one `[websockets] Connection opened!`
line for the whole window) while fork_verify logged 124k+ warn lines and the livelock counter visibly
engaged. Two earlier sessions with `fork_verify=warn` also produced zero disconnects to correlate against
at all (one 5+ minute clean run, heal activity boot-scoped only, host had 16 logical processors/8 cores,
2.1-8GB free — noted as weakening, not ruling out, the "too few cores" framing on a genuinely low-core
host). Across all ten sessions, a disconnect and active fork-heal traffic have never co-occurred in the
same observation window either way.

**Process-launch harness note (PowerShell), incidental finding from this investigation**:
`Start-Process -RedirectStandardOutput/-RedirectStandardError` silently produced EMPTY output files for
this runner when launched with a longer `/bin/sh -c '<script>'` argument via `-ArgumentList` (confirmed
repeatable) — process starts, pulls cached OCI layers, then appears to exit with nothing further logged,
looking exactly like a crash. Native invocation (`& $exe ... > out.log 2> err.log`, or a `.ps1` file via
`Start-Process powershell.exe -ArgumentList "-File",<script>`) reliably captured all output for the
identical command. Root cause not fully isolated (plain `/bin/echo` and the unwrapped `/bin/sh
/config/webtop_stack.sh` form both worked fine via `Start-Process`).

**Terminal Emulator / app-launch-latency complaint, investigated same session as the fork_verify stress
test.** `MAX_THREAD_VERIFICATION_STEPS` exhaustion is not a hang mechanism by code inspection
(`fork_verify.rs` ~980-1006): once the per-thread step bound is hit, single-stepping just disarms (`TF`
cleared) and the thread resumes at full guest speed — `is_verifying` stays true so AV-path healing (now
livelock-protected too) stays armed for anything single-stepping would have caught later. Direct evidence:
double-clicking the `Home`/`Desktop` desktop icons (native `SendInput`-level clicks, coordinate-mapped and
pixel-verified against the live screenshot, `.wfgy/click_helper.ps1`) opened a real Thunar window within a
few seconds each time, WHILE the fork-stress loop ran continuously. Terminal Emulator specifically could
not be opened because every dropdown/popup-menu interaction (XFCE panel's "Applications" menu, Thunar's
"File" menu) systematically failed to open despite the same native clicks working reliably for every
non-popup target tried (desktop icons, window buttons, a dialog's Close button) — candidates: X11
pointer-grab semantics for `GtkMenu` popups not surviving whatever layer forwards clicks through selkies,
vs. a timing/coordinate issue specific to menu widgets.

## Guest-reachable-code panic fixes: mechanisms drained to here

`AGENTS.md` keeps only the sha list; the shapes:

- **`6e14b71`** — `litebox/src/fs/layered.rs`'s `chmod`/`chown`/`set_times` mapped `migrate_file_up`'s
  `MigrationError::NotAFile` to `unimplemented!()`. `migrate_file_up` migrates byte *contents*, so every
  lower-only directory or character device took that arm: `touch /usr/share` and `chmod` on any
  read-only-layer directory were deterministic HOST panics at `layered.rs:1538`, as was `touch
  /dev/null`. All three also ended in an unbounded `self.<op>(path, …)` tail call that overflowed the
  host thread stack whenever the upper layer still reported the path missing. Fixed by
  `migrate_entry_up_for_metadata` (recreates a lower-only directory in the upper layer with the lower's
  own mode, carrying `node_info` so the inode is stable across the copy-up — measured 17 → 17 → 17),
  `EROFS` for kinds that cannot be carried up (chardev, FIFO), and a bounded two-pass loop in the three
  callers. `2ce9ba8` then swept the last two host-killing arms off `layered.rs`'s guest-reachable paths.
- `133d3d4` — `O_NOATIME` panicked the host; `in_mem.rs:282` is the same class.
- `62e3c79`/`44189c7` — OOM panicked instead of `ENOMEM`.
- `ab2393d` — `listen()` panicked on `backlog=0` or re-listen, both of which real nginx does.
- `a473048` — a guest open flag could kill the host.
- `8aa05af` — a corrupted guest context kills that guest, not the host.
- `5ec1ee4` — a debug-only diagnostic crashed the host in the exact scenario it existed to debug.
- `1e1da7c` — nested `epoll_ctl(EPOLL_CTL_ADD)` hit `unimplemented!()`; calloop does exactly this.

## Superseded framings deleted in this pass (do not resurrect)

- "Host-side crash machinery, and the one crash still open" as a section title, and the paragraph
  explaining why the stale claim persisted, both refuted by `271cbb5` — see above.
- The `evdev-emits-two-syn-reports-per-mouse-move` PRD row cited in the input-latency section: that row
  no longer exists, and `5683a4e` closed its validation gap with a live-counted witness. The remaining
  open row is `linux-macos-userland-presentation-still-emits-two-syn-reports-per-move`.
- The separate "Two sets of seven independent litebox defects" paragraph, folded into the browser-desktop
  section as one clause; the 2026-09-07 set is memory `mem-f17269d5777055d3-3326`, the 2026-09-08 set is
  enumerated in `docs/AGENTS_ARCHIVE_2026-09-10.md`.
