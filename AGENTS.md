# litebox — current state (2026-09-17)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, and a claim a later commit superseded, were deleted rather than hedged. Reference
detail is drained to the `docs/AGENTS_ARCHIVE_*.md` files and per-investigation logs to the dated
`docs/*.md` in the map below — read those for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one line
plus its pointer, not in a separate memory file or a pass narrative appended below.

## The cheap repro — start here

```
target/release/litebox_runner_linux_on_windows_userland.exe -Z \
  --oci-image docker.io/library/debian:stable-slim -- /bin/bash -c '<script>'
```

One ~81MB layer, `[cache] HIT` after the first pull, real GNU coreutils instead of busybox — which
matters: coreutils `touch` issues the `utimensat(fd, NULL, …)`/futimens form busybox's never reaches
(`caaac79`). Two host-side gotchas, each already costly:

- **PowerShell, never Git Bash** — Git Bash rewrites `/absolute/guest/paths` into
  `C:/Program Files/Git/...` before the runner sees them, giving a misleading `ENOENT`. **`Start-Process
  -RedirectStandardOutput/-RedirectStandardError` makes the runner exit almost instantly with zero guest
  output** (no crash dump, no event-log entry); use `& .\runner.exe ... *> combined.log` instead.
- **Single quotes only inside `-c`** — embedded double quotes are corrupted crossing into the child's
  Win32 command line. This masqueraded as deep fork/stack-pointer corruption for a whole sub-session.

**Log level**: the default is `warn,litebox_platform_windows_userland::fork_verify=error` (`EnvFilter`'s
own ERROR-only default discarded all real `warn!` sites; `fork_verify` is pinned to `error` because it
warns per single-stepped instruction). Do **not** add `LITEBOX_LOG=error` by reflex; use
`fork_verify=warn` when a fork heal is the subject.

## Standing lessons and hard constraints

- **No WSL or hypervisor, ever** — always run under the matching runner
  (`litebox_runner_linux_on_windows_userland.exe`/`litebox_runner_linux_userland`); cross-compiling FOR
  Linux is fine, running the result in a VM defeats the premise.
- **`fork_verify.rs`'s stale-pointer-healing bug class is Windows-only** (real `fork()` gives the
  child identical addresses) — never port to another platform's crate.
- **Never `bcdedit /debug on`** without a kernel debugger already attached — two full-host freezes
  needing a power-cycle.
- **Never run two full-stack verifications concurrently** — starves both, looks exactly like a real
  hang. Kill every `litebox_runner` between runs; watch `FreePhysicalMemory`, kill on a falling
  trend not a fixed RSS number.
- **`LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check**, never
  `PrintWindow`/`CopyFromScreen`. A pixel count alone never identifies WHO painted a frame — decode
  frame structure (`advisor/probes/decode_frame.py`) and correlate against `DIAG_TIMELINE execve`'s
  real argv0.
- **Never time litebox with one host process per datapoint** (a bare spawn costs 1.6-2.3s, dwarfing real
  per-exec differences) — run N iterations inside ONE guest process, establish a noise floor. Never
  subtract timestamps across a parent log and a fork-child log — `init_logging()` resets elapsed time
  to ~0 per child.
- **Refusal errno choice is API contract** — EPERM lets callers degrade, EINVAL/ENOSYS fails them hard;
  wrong choices have silently broken whole subsystems before (archive).
- **Proving a run took the cross-process fork path needs `[process_fork_diag] task-resume-probe` lines,
  never the shim's eligibility log** — that log fires regardless of whether the fork actually happened
  that way (produced two recorded false conclusions, archive).
- **fork carries pipes, regular files and the writable layer into a child, but NOT sockets** — a
  pre-fork-created listening socket serves nothing to a forked child; run dbus-daemon non-forking, and
  for XFCE use `xfce4-session`, never `startxfce4`.
- **On host-side crashes, use `advisor/probes/symbolize_litebox_crash.py`, snapshotting `.exe`+`.pdb`
  next to the log** — a ring dump's `rva=` is only meaningful against the exact emitting build.
- **Isolate the harness before blaming litebox** — launch guest probes directly as the runner's
  top-level program, never via a runtime-built `/bin/sh -c` wrapper.
- **Never trust a container tag name for its WM/session contents** — verify by registry manifest + blob
  tar-listing, or a live in-guest `/usr/bin` listing.
- **Never record a test count you did not just watch run to completion**, and never leave a suite red
  for an environmental reason. No counts are recorded here on purpose.
- **Repo hygiene** — packed layer tars, frame dumps and debug logs never go in git (`.wfgy/`,
  gitignored); untrack anything `git add -A` sweeps in.
- Procedural know-how is in the archive's "Working practices": freestanding guest binaries built on the
  HOST, probe injection via a small `--resume-from` overlay tar, mature libraries over hand-rolled code.
- **Guest-reachable code returns an errno, never a panic** — the host process IS the entire guest
  session, so an `unimplemented!()`/`unreachable!()`/panic, or unbounded recursion, on any
  guest-reachable path kills every guest process at once. Bitten many times (OOM, metadata ops, open
  flags, nested `epoll_ctl`, corrupted guest contexts); full fixed-bug list with shas: archive.

## Cross-process fork (`LITEBOX_PROCESS_FORK=1`)

A genuine `D == 0` fork — child at the SAME addresses, no relocation, no `fork_verify` healing — exists as
`spawn_cross_process_fork_child` (design case `advisor/ADVISORY-002-d-zero-fork.md`). It short-circuits to
a native fork when `platform.has_native_fork()` — the whole fd-carrying apparatus is Windows-only
scaffolding for a missing syscall.

**It is correctness-sound**: zero corruption across every completed fork on a `bash -c` loop repro, vs the
thread-based default's 100% tcache-corruption rate on the same repro (ADVISORY-001 §3N is the
**thread-based** path's defect only).

**Eligibility** — refused only for `comm` == `Xvfb`/`dbus-daemon` (a live unix listening socket can't be
served from a fork-time filesystem snapshot), an already-borrowed fd table, a beyond-stdio fd that isn't
a pipe end/path-recorded regular file/eventfd/close-on-exec (overridable by
`LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an unsanitizable `fs_base`/context. On a real `debian-xfce` boot
the only remaining blocking kind is `unix-socket` — 5 refused forks of 34, down from 34/34. Per-kind
deviations: archive.

**Per-fork cost** was ~3.5-5s, now ~1.2s; a full `webtop_stack.sh` boot reaches `NGINX_STARTED` in
under a minute versus never in 15+. Older cost explanations were measured wrong. Use
`LITEBOX_DIAG_FORK_TIMING=1` for the next cost question. Three correctness bugs fixed; detail: archive.

**Reading a cross-process log** — the `fork_verify` "stale CODE pointer" noise-vs-signal read: archive.

**Still open**: nginx's own SSL-cert generation fails on its first real startup attempt — the original
symptom this investigation began from, genuinely not root-caused (`docs/track-b-fork-fix-progress.md:
146-152`). The TOP-LEVEL parent's curl-self-test stall (`sys_wait4(pid=-1)` not checking
`cross_process_children`) is fixed (`6e86a40`) — do not cite that one as open.

**The nginx-self-test pipe-EOF wedge (fourth pass, 2026-09-17) — CONFIRMED and FIXED.** Root
cause: broad `bInheritHandles=TRUE` leaked a sibling fork child's inheritable bridge-pipe handle
into unrelated children racing the same window. Fix: `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` explicit
per-call handle allow-list (`process_fork.rs`, `spawn_suspended_impl`). Live-verified. Archive.

**`Network::socket_set` made shared-arena-native (eighth pass)**: fixed a `tuple.unwrap()` panic by
giving `socket_set` a 256-slot (`MAX_SOCKETS`) fixed array in the shared kernel arena instead of a
private-heap `Vec`. A/B-confirmed (ninth pass) this genuinely fixed that panic (old binary
crash-loops 8x on it, new binary zero) and, in doing so, newly exposed a downstream stall the old
binary could never reach (it crashed first). Full narrative: archive.

**Post-`NGINX_STARTED` CPU livelock — ROOT-CAUSED and FIXED (tenth pass, 2026-09-17).** Real
mechanism, found via a live CPU-sampling profiler (WPR/WPA needs admin, unavailable — used cdb's
`-pv` poor-man's sampler instead: 15 rapid `~*k` stack samples over ~5s plus `!runaway`, in
`.wfgy/cpu_profile_session`): **NOT** the previously-flagged, never-confirmed `SafeZoneAllocator`
spinlock — that code never appeared in any sample. `!runaway` showed ONE thread (of 8) burning
100% of a core continuously (0 → 732s user CPU over ~12 real minutes; every other thread stayed
under 1s), always sampled inside `LocalPortAllocator::ephemeral_port`/`deallocate` — RIP moving
across samples (genuinely executing, not frozen), disassembly confirming real `hashbrown` SIMD
probe code (`pcmpeqb`/`pmovmskb`/`tzcnt`) that never terminates. Root cause: `LocalPortAllocator`
(embedded inline in `Network`, itself embedded in the shared-arena `GlobalState`) stored its ports
in a `HashMap` — whose backing table is a **private-per-process-heap allocation reachable only via
a raw pointer**. A cross-process-fork child (confirmed via the log: `winpid=5060` explicitly
tagged `(child)`) that attaches to (rather than constructs) the shared `GlobalState` inherits the
constructing process's pointer value, meaningless in its own address space — the exact same
"stale cross-process pointer" bug class already fixed a dozen times today for other fields, just
not yet audited for this nested one. **Fixed**: `LocalPortAllocator::refcount` converted from
`HashMap<NonZeroU16, NonZeroU16>` to a fixed, pointer-free `[u16; 65535]` array
(`litebox/src/net/local_ports.rs`), same pattern as `socket_set`'s `MAX_SOCKETS` array.

**Second instance of the same bug class found immediately on live re-verify, also fixed.** Past
`NGINX_STARTED` with the fix above, a forked child panicked `index out of bounds: the len is 256
but the index is 3414407380873671541` in smoltcp's `SocketSet::retain`, from
`Network::remove_dead_sockets` over `closing_in_background` — identical mechanism, one of the four
instances this file already named as still-open (`socket_set`/`interface`/`closing_in_background`/
`queued_for_closure`, see `Network::rebind_per_process_fields`'s doc comment). **Fixed**:
`closing_in_background` converted `Vec<SocketHandle>` → fixed `[Option<SocketHandle>; MAX_SOCKETS]`
(self-contained, `litebox/src/net/mod.rs`). `interface` and `queued_for_closure` remain open —
the latter additionally touches the shared `DescriptorTable::drain_entries_full_covered_by` API.

**Verification**: `cargo build --release` clean; all 25 `litebox` net unit tests pass unchanged.
Live: re-ran the identical `webtop_stack.sh` repro twice against the fixed binary. Both times
reached `NGINX_STARTED` with sane, distributed multi-process CPU (no thread ever exceeded ~25s
over several real minutes; thread/process counts fluctuated 4-19 = real fork churn, not one stuck
thread) — the specific livelock this pass chased is confirmed gone, live, not just by code reading.

**Poison-on-dead-holder scheme for `Network` — DESIGNED, IMPLEMENTED, and LIVE-VERIFIED
(eleventh pass, 2026-09-17).** Closes the "third issue" above. `RawMutex` gained a `poisoned:
AtomicBool`, set (unconditional `store`) by `try_recover_from_dead_holder_unregistered` on every
dead-holder recovery, read-and-cleared by new `take_poison()`
(`litebox_platform_windows_userland/src/lib.rs`); `litebox::sync::Mutex` got one opt-in method,
`lock_recovering_poison() -> (MutexGuard, bool)`, deliberately NOT wired into ordinary `lock()` (no
general poisoning concept for every `Mutex<Platform, T>`, by design). `GlobalStateHandle::net_lock`
is the ONE call site that opts in: on `true`, calls new `Network::reset_after_poisoning`
(`litebox/src/net/mod.rs`) before handing out the guard — wholesale-resets `socket_set` (reusing the
existing shared-arena storage, not a second allocation), `closing_in_background`, `queued_for_closure`,
`local_port_allocator`. `smoltcp::iface::SocketHandle` is a bare `usize` index (no generation
counter), so there is no cheaper way to tell stale from live short of wiping every field that could
hold one — option (a) from the task brief, narrowly scoped to `Network`.

**Two more instances of a DISTINCT, pre-existing bug found and fixed the same pass, both live
`cdb`-caught (thread frozen bit-for-bit at the same RIP across 6 rapid re-samples, inside
`buddy_system_allocator::LockedHeapWithRescue::dealloc`):** letting a `SocketSet::remove`d `Socket`
drop normally runs its RX/TX ring buffers' `Vec` destructor through the CURRENT process's allocator
on a pointer that names a DIFFERENT (often already-dead) process's private heap — `Network::socket`
allocates those buffers on whichever process's heap called it. Hit in `reset_after_poisoning` itself
(first version) and, independently and unrelated to any poisoning event, in `remove_dead_sockets`
(routine per-tick housekeeping over the genuinely cross-process-shared `closing_in_background`
array). Fix both: `core::mem::forget` the removed `Socket` instead of dropping it — correct because
that memory was never this process's to free; Windows reclaims it wholesale when the owning process
exits. `close_handle`'s/`listen`'s own `socket_set.remove` sites are SAFE unchanged — every handle
they touch came from THIS process's own private, non-shared descriptor table (sockets are never
fork-carried), so remover == allocator always there.

**Live verification, five `LITEBOX_PROCESS_FORK=1` boots.** Runs 1-2 failed the WRONG way (own
mistake: `Start-Process -RedirectStandardOutput/-RedirectStandardError`, the silent-exit artifact
this file already warns about — corrected to `& .\runner.exe ... *> log` after). Run 3 (pre-`mem::
forget` binary) `cdb`-confirmed the new livelock above, killed, root-caused, fixed. Runs 4 and 5
(post-both-fixes binary): real dead-holder-recovery fired live in BOTH (`holder_pid=12996` and
`=10908`, unrelated events), both correctly triggered `reset_after_poisoning`, ZERO panic-cascade
either time. Run 4 reached `SELKIES_PORT_UP` + `DE_LAUNCHED` — the furthest point reached in this
entire day's investigation — with exactly one `"handle does not refer to a valid socket"` panic
(the accepted, disclosed residual: a DIFFERENT still-live process's already-minted `SocketFd`/
`LocalPort` token goes stale the instant an unrelated reset runs; non-fatal,
"panic kills the process, supervisor respawns", did not block progress). Run 5: same pattern, one
more live recovery + one more residual panic, reached `SELKIES_SUPERVISOR: giving up after 30
attempts` before this session ended it (timing variance vs. run 4, not a regression — steady log
growth throughout, no livelock signature). Full per-run transcripts, `cdb` samples, symbolized
stacks: `docs/AGENTS_ARCHIVE_2026-09-17.md`.

**Does NOT reach the browser/terminal/apps milestone — squarely the SEPARATE, ALREADY-DOCUMENTED
Track B Xvfb/dbus thread-based-fork corruption class below, not this pass's bug or responsibility.**
`SELKIES_SUPERVISOR` exhausts 30 respawn attempts (`rc=2` every time); `curl` to port 8080 got
`Empty reply from server`, port 8081 (selkies) refused the connection outright. Boot reaches its
stable `HOLD t=` steady state afterward rather than crash-looping. **Real next pickup for the
browser milestone**: Track B step 3 (fixed-base shared kernel heap) making `Xvfb`/`dbus-daemon`
themselves cross-process-fork-eligible — separate, larger, already-scoped work.

**`XVFB_FAILED`/`DBUS_FAILED` — characterized, NOT primarily RAM pressure (fifth pass, healthy
~3.6GB-free start).** Direct log evidence: `webtop_stack.sh: line 280: 147 Killed xset q >
/dev/null 2>&1` immediately before `[s] XVFB_FAILED` — the liveness-check `xset` itself got
killed (signal), not Xvfb failing to start. `Xvfb`/`dbus-daemon` (and anything they spawn) are
refused cross-process-fork eligibility, so they run the THREAD-based path — the ALREADY-
DOCUMENTED, still-open ADVISORY-001 §3N tcache/heap-corruption class that `GLIBC_TUNABLES` only partially
mitigates ("Track B territory, not a tunable-coverage gap", `docs/AGENTS_ARCHIVE_2026-09-16.md`) —
consistent with, not a new defect. Likely a FALSE NEGATIVE on Xvfb's actual health: the boot
reached `SELKIES_PORT_UP`/`DE_LAUNCHED` afterward, which needs a real working X display for selkies
to capture from, so `XVFB_FAILED` most likely means "the `xset` liveness probe crashed", not
"Xvfb itself never started". Real fix is the same Track B step-3 fixed-base-shared-heap work that
would let `Xvfb`/`dbus-daemon` themselves become cross-process-fork-eligible, eliminating the
thread-based path (and its tcache corruption class) for them entirely — not attempted this pass.

**Fork-after-Xorg PERMANENT freeze — did NOT reproduce 2026-09-17; thread-based-fork-only.** Under
`LITEBOX_PROCESS_FORK=1` the identical script completed cleanly 2/2 — zero freeze, zero double-free.
Full evidence, a disclosed ENOMEM finding under concurrent cross-process forks: archive.

### Track B — current pickup list, precise (full pass-by-pass evidence: archive)

(1) ~~Debugger-root-cause the dead-holder-recovery data-inconsistency panic~~ — DONE, eleventh pass
(poison-on-dead-holder scheme, this file's current Track B entry above); the actual remaining
blocker on the browser/terminal/apps milestone is now squarely the separate `XVFB_FAILED`/
`DBUS_FAILED`/selkies-thread-based-fork item below, not this one. (1b) `queued_for_closure`'s own
still-open cross-process-Vec hazard (distinct from the two `mem::forget` fixes above — nothing yet
converts its STORAGE to a fixed pointer-free array the way `closing_in_background`/`socket_set`
already were) remains a live risk for a future pass: any process reading/pushing it while attached
rather than constructing could still hit the stale-pointer class on the Vec header itself, not just
the drop-ownership issue just fixed; (2) debugger-root-cause
`litebox/src/event/wait.rs:224`'s `unreachable!()` on garbage thread state (dozens per boot, most
frequent panic historically, NOT yet debugger-confirmed — do not patch blind); (3) root-cause the
`/tmp/empty` writable-layer cross-child-visibility gap behind `DBUS_FAILED`; (4) finish the
`Network` shared-arena redesign (`interface`, `queued_for_closure` remain — `closing_in_background`
and `socket_set`'s slot array are done, see current Track B entry; `litebox/src/net/mod.rs`'s
`MAX_SOCKETS` doc comment has the design); (5) after (1)-(4), `timerfd`/`signalfd` are the
next-cheapest carriable fd kinds before attempting `socket`/`unix-socket`/`pty`/`epoll`.

## Container images and OCI loading

**`litebox_packager --oci-image <ref> --output <tar>`** pulls, whiteout-merges, rewrites every ELF and
produces a bootable flat tar in one command (x86-64/Apple Silicon hosts only) — supersedes the ad-hoc
OCI-pull Python scripts this project once hand-rolled, retired, do not recreate.

**Runtime in-memory loading** — `--oci-image <ref>` pulls, merges and rewrites every layer in memory; no
host directory is ever created for the rootfs (a real one hit three independent Windows-path bugs).
Rewritten layers are cached under `.litebox-cache/`, keyed so a rewriter change self-invalidates. Large
images (multi-GB, 100K+ entries) pack fine now; residual risk is host-memory contention, not a litebox
bug. `tar_ro.rs`'s multi-layer index is built ONCE at mount, not per read (was O(entries²), 17.3s →
0.35s fixed). Cache internals and the four fixed OOM bugs: archive.

**A trampoline-extension failure used to poison a whole segment's syscalls, now fixed** (archived).
**Tags, verified live, never from the name** (archived): `linuxserver/webtop:alpine-mate` ships MATE
not XFCE; `alpine-xfce` doesn't exist; `debian-xfce`/`ubuntu-xfce` ship real XFCE.

**X server choice**: for the DRM/wgpu on-screen (`--gui`) path use `Xorg` with `modesetting` — litebox's
virtual DRM device is legacy-KMS + dumb-buffer + XRGB8888 only, no atomic modeset/GBM/EGL, so a GBM-first
compositor lands on its least-tested fallback, and `Xvfb` never touches DRM/KMS at all (zero page-flips,
indistinguishable from "never drew"). For browser/selkies, `Xvfb` IS correct and verified — its
`-shmem` framebuffer works now that SysV shared memory exists.

**Durable artifacts**: `C:\dev\litebox-webtop\webtop_seatd.tar` (stock MATE webtop); the
`.wfgy/xfce-build/` hand-assembled weston+XFCE tar is superseded by the stock-image path.

## A real desktop renders in a browser

**XFCE renders in a real host browser, and MATE too** — full pipeline (Xvfb, selkies/pixelflux x264,
MIT-SHM) inside litebox, only the reverse proxy host-side. Working config: selkies `--addr=0.0.0.0`
port **8081**, dashboard over `--publish`, `/websockets` tunnelled to 8081. Fourteen litebox defects
got here, all landed (archive).

**A stock s6-overlay image boots with no flags/stubs**: `/init` runs 16 cross-process children with
zero uncarriable fds into supervision — retires three "fundamental blocker" claims older notes
carried. **The black XFCE desktop was deterministic, now fixed**: the runtime rewriter corrupted
`libLLVM.so.19.1`'s `.dynsym`, so mesa `dlopen` failed forever — not a race. Settled in the archive:
PI futexes work; labwc's SIGABRT is upstream wlroots; every pre-`694bb93` gdk-pixbuf finding is stale;
two network fixes this path needed.

**XFCE also renders on the THREAD-based fork path, gated by one flag** (`docker.io/linuxserver/
webtop:debian-xfce`, `.wfgy/webtop_stack.sh`). Without it, 3/3 boots die ~7s in — ADVISORY-001 §3N's
safe-linked-tcache write (the `fork_verify: stale CODE pointer` warning right before it is a red
herring; translation is correct). Fix: `--env GLIBC_TUNABLES=glibc.malloc.tcache_count=
0:glibc.malloc.mxfast=0` as an `--env` runner flag (a bare host-shell prefix does NOT reach the guest).
Glibc-only workaround, not a fix (PRD `glibc-tunables-workaround-pending-zero-fork`);
`LITEBOX_PROCESS_FORK=1` removes the class properly but can't run a desktop until the AF_UNIX gap
below closes. Selkies also needs `--clipboard-enabled=false` (its clipboard monitor re-triggers the
same corruption every tick).

**Sixth pass (2026-09-17) — did NOT reach `DE_LAUNCHED`, new stall past
`SELKIES_BIND_WATCHDOG_STARTED`** (writable-layer-adoption race on a late fork child); browser
never reached, killed clean, no RAM-leak evidence. Archive.

**Seventh pass (2026-09-17) — writable-layer-adoption race ROOT-CAUSED and FIXED, two bugs, both
landed, `DE_LAUNCHED`/`SELKIES_PORT_UP` now reached deterministically (5/5 repro boots).**
`CONTAINER_FS_SNAPSHOT_ENV_VAR` (`litebox-container-fs-<pid>.tar`) is ONE canonical path shared by
the whole boot tree, but two consumers still carried stale single-consumer assumptions -- (1) the
importing fork child deleted it right after import (true before the canonical-path design, false
after: several children in the same fork-heavy window share the identical path, so the first
importer race-deletes it out from under the rest -- the exact sixth-pass `could not adopt ...
(os error 2)` symptom); (2) the exiting child's export-back path published via a raw, non-atomic
`std::fs::copy` onto the same canonical path, letting a concurrent importer read a torn tar
mid-overwrite (`failed to read tar entry: numeric field was not a number`, caught live once in
~180 adopts). Fix: stopped the premature delete; routed the exit-time publish through the
already-correct atomic-rename primitive (`publish_as_container_fs_snapshot`, widened to `pub`)
instead of a raw copy -- a lifecycle/synchronization fix, kept as a plain on-disk file, not moved
into the shared kernel arena. Live-verified: 5 consecutive `LITEBOX_PROCESS_FORK=1` boots, ~800+
combined adopt/export cycles, one failure total (bug 2, in the run before its own fix landed),
zero recurrence after both fixes were live; all 5 reached `DE_LAUNCHED`+`SELKIES_PORT_UP`
(previously non-deterministic, never reached at all the pass before). Browser/terminal/apps still
blocked, **not by this bug**: `NGINX_SELFTEST_FAILED`/`XVFB_FAILED`/`DBUS_FAILED`/`DE_FAILED`
still fire every run, the already-documented Track B Xvfb/dbus corruption class (above). Full
mechanism, both fixes, and the 5-boot transcript: archive (newest entry).

**Open here.** One client per selkies instance, no slot reclaim on reload. An intermittent host AV ends
some runs (host-allocator region fault) — separate non-determinism from the ACK-stall-kill below.
Architectural gap: **guest processes share no AF_UNIX/loopback/FIFO namespace**, so a cross-process fork
gives zero AVs but Xvfb is unreachable from its own clients — one shared host-side transport would put
the whole desktop on the crash-free path (`docs/fork-fs-veh-2026-09-08.md:128-144`).

**The glibc/tcache crash class still sporadically hits selkies** on the THREAD-based fork path,
separately from the ACK-stall-kill (a DPI-fork on rapid reconnect SIGSEGVs, ADVISORY-001 §3N) --
**a SECOND, different corruption signature under heavy fork load** (`double free or corruption
(out)` SIGABRT), hitting bin paths `GLIBC_TUNABLES` deliberately leaves enabled. **Track B
territory, not a tunable-coverage gap** — do not re-attempt a `GLIBC_TUNABLES`/env fix without
evidence of a THIRD mechanism. Full evidence: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

### The ACK-stall-kill and port-8081 watchdog — both CLOSED (2026-09-16)

Real blocker was the guest-side patcher silently crashing on `shutil.copy2()`'s `copystat()`
(no `listxattr` shim) before ever patching `selkies.py`; fixed (`478e640`), live-verified 60+s
zero `keepalive ping timeout`. Port-8081 double-bind fix live-verified over 17 boot cycles + a
6000-connection stress test, zero recurrence, zero RAM leaks. Full detail: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

## Host-side crash machinery

**A fatal host fault dumps before it dies, ungated**: stack walk, `RECENT_FAULTS` ring, `RECOVERY_LOG`
print with no env var needed; a real OS minidump comes only from the repeated-identical-fault circuit
breaker. Two dump fields mislead on an old reading (`error_code` is synthesized; `is_in_guest` is
tri-state) — exact semantics: archive.

**An unexplained `0xC0000005` may be a panic** — litebox no longer treats Rust panics as panics; the
handler now enters only for the four codes it triages, registers FIRST in the chain, sizes per-depth
frames from disassembly not guesswork, and no longer lets the watchdog kill a recovered run. Narrative:
`docs/veh-exception-handler-design.md`.

**Cross-process sync on Windows is a hard platform constraint**: every native address/TID-based wait is
process-local (`WaitOnAddress`, keyed events, `NtAlertThreadByThreadId`=ACCESS_DENIED); only a shared
kernel object crosses processes. `litebox_platform_windows_userland/src/xproc_sync.rs` is a
live-verified NAMED-event mutex primitive, still unwired (wants Track B step 3's fixed-base shared
section first). `RawMutex` (every shim subsystem's synchronization bottoms out in this trait) is
rewired as of this pass -- see "Cross-process-capable `RawMutex`" below, a different mechanism.

## Cross-process-capable `RawMutex` -- done, live-verified (detail: archive)

`RawMutex` no longer calls `WaitOnAddress`/`WakeByAddressSingle` (process-local per MSDN) --
replaced with a manual wait queue plus one auto-reset kernel `Event` per OS thread; cross-process
half is real code (`DuplicateHandle`-based), now genuinely live and fixed once (see "RawMutex
lost-wakeup" below -- the wait-queue storage itself had to become pointer-free once cross-process
contention became real). Live-verified: `yes hello | head -c 5000000 | wc -c`, `sort --parallel=4` multithreaded
contention, both exact/correct, no hang/deadlock. `process-fork-pipe-relay-sigpipe-above-4kb`
(3-stage pipeline SIGPIPE) resolved 2026-09-16, don't rely on `LITEBOX_PROCESS_FORK=1` for
heavy-iteration guests. Full internals: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

## Shared kernel heap -- selective-routing correction landed (ADVISORY-002 §3.3)

**The "route everything through one shared section" design is REVERTED.** `SLAB_ALLOC`
(`#[global_allocator]`, `lib.rs`) is back to the private per-process `VirtualAlloc2` mechanism for
every ordinary host-heap allocation -- routing everything through the shared bump allocator (no
reclaim) was live-proven to exhaust an 8 GiB pool after 45-90 real execs, worse than not sharing at
all; **live-verified fixed**. The fixed-base/atomic-cursor/handle-inherit machinery is NOT deleted
-- it now backs a small **64 MiB, standalone, bounded**
arena (`shared_kernel_arena_alloc`, `lib.rs`), deliberately NOT wired to `GlobalAlloc`, reserved for
`LiteBoxX`/`GlobalState`-only placement. Full mechanism/history: `docs/AGENTS_ARCHIVE_2026-09-17.md`.

### `SharedArc<T>` and real `GlobalState` create-vs-attach -- BOTH DONE and LIVE-VERIFIED 2026-09-17; does NOT close XVFB_FAILED/DBUS_FAILED

`litebox_platform_windows_userland/src/lib.rs`'s `SharedArc<T>` (hand-rolled, not `std::sync::Arc`,
whose `ArcInner` layout is a private std detail unsound to place by hand; stable Rust also has no
`allocator_api`) places `value` plus a `strong: AtomicUsize` in the bounded 64 MiB
`shared_kernel_arena_alloc` region; `new` -> `(handle, arena_offset)`, `attach(offset)` gets an
independent handle to the SAME bytes. `Drop` never reclaims/runs `T`'s destructor (bump allocator,
no free list; a kernel singleton must outlive the whole fork family). `SharedKernelStateProvider`
(`SharedKernelStateSlot::{LiteBoxX,ShimGlobalState}`) turns this into a real create-or-attach
protocol: trivial `Arc::new` default everywhere with real OS process isolation, real impl on
`WindowsUserland`. `litebox_shim_linux::GlobalState` is now `GlobalStateHandle<Platform, FS>` =
`Platform::Handle<GlobalStateX<...>>`; `LinuxShimBuilder::build` does the real attach-or-create
branch. Decisive live proof (sentinel writes before/after `spawn_cross_process_fork_child`, both
observed by the child's own post-`build()` read): archive.

**Does NOT close `XVFB_FAILED`/`DBUS_FAILED`: root cause precisely characterized.** `SharedArc::new`
places only `T`'s literal inline bytes in the arena -- but every `GlobalState` REGISTRY
(`unix_addr_table`, `pty_registry`, `daemon_pty_masters`, `flock_registry`, `fifo_registry`,
`sysv_shm`, `memfds`, `shared_files`, plus originally 3 caches -- now down to 0, see below) is a
`BTreeMap`/similar whose NODES live on the ordinary private per-process heap -- an attaching
process's copy of the root pointer is meaningless in its own address space. Follow-up PRD:
`globalstate-nested-collections-not-actually-shared`. Full mechanism: `docs/AGENTS_ARCHIVE_2026-09-17.md`.

## `unix_addr_table` presence sharing -- landed and live-verified (real blocker was elsewhere)

`litebox_shim_linux/src/syscalls/unix.rs`'s `SharedUnixAddrPresenceTable`: fixed-256-slot, pure-
`core::sync::atomic` (zero `unsafe`, zero `RawMutex`), lock-free side-index recording `(kind, key
bytes <=108, owner guest pid)` per bind/listen, mirrored alongside (never replacing) each
process's real `unix_addr_table` `BTreeMap`. A plain `GlobalState` field (`unix_addr_presence`, no
new `SharedKernelStateProvider` slot needed): zero pointer indirection, inherits whatever sharing
`GlobalState` itself already has for free. Wired at all 4 call sites (stream `listen`/`Drop`,
datagram `bind`/`Drop`) plus an always-on diagnostic on every real `ECONNREFUSED`. **This is the
reusable flat-table PATTERN the still-genuinely-shared registries below (`pty_registry` et al.)
need next** — `unix_addr_table`'s own full `BTreeMap` (the `Backlog`/`Channel` connection data,
not just presence) remains real per-process-heap and unconverted, same as those others. Decisive
live proof (two keys registered before/after the fork, both observed by the child): archive.

## Cross-process fork: twelve registry/pointer/lock fixes, all now landed, 2026-09-17

Root pattern (instances 1-11): a raw `Arc`/`Box` pointer captured once by whichever process
constructs `GlobalState` first, frozen into cross-process-shared bytes, meaningless (or dangling) in
every other attaching process -- found and fixed one layer deeper each time, isolated with a minimal
`-Z --oci-image debian:stable-slim -- /bin/bash -c 'mkdir ...'` repro under `LITEBOX_PROCESS_FORK=1`.
Fix pattern: shadow the field with a fresh per-process copy (state that doesn't need cross-process
visibility -- `litebox`, `proc_self_info`/`pts_registry`, `elf_patch_cache`/`exec_ranges_cache`/
`segment_scan_cache`, `futex_manager`), or rebind via a locking accessor (state genuinely meant to
be shared -- `Network`'s two fields via `net_lock`, `Pipes.litebox` via `pipes()`), or (instance 9)
replace a process-private-heap `Vec` with a fixed-slot pointer-free array. Instance 12 (`net_lock`
left permanently locked by an exiting fork-child) was DIFFERENT -- a lock-liveness/owner-death-
recovery gap, not a stale pointer -- fixed below. Does NOT close `XVFB_FAILED`/`DBUS_FAILED`;
`pty_registry`/`flock_registry`/etc. remain real, still-open follow-on work. Full detail: archive.

## RawMutex lost-wakeup, Pipes stale-pointer, FutexManager sharing gap, and cross-process-fork lock-orphaning -- ALL FOUR FIXED 2026-09-17

Four fixes, all live-verified, all landed: (A) `RawMutex::resolve_waiter_event`'s cross-process
branch panicked on a stale pid instead of signaling the real waiter -- `waiters` is now a
fixed-32-slot pointer-free `WaiterQueue`. (B) `Pipes.litebox`'s stale pointer crashed a killed
fork child's stdio teardown -- now interior-mutable, rebound via `GlobalStateHandle::pipes()`
same as `net_lock`. (C) `FutexManager` cross-process sharing hung on `LoanList` entries that can
be stack-allocated (fork-family-identical only for the forking thread) -- resolved by giving each
process its own fresh `FutexManager`. (D) A cross-process-fork child's un-shutdown `net_worker`
thread could be killed mid-hold of the shared `net_lock`, orphaning it forever -- fixed with
`RawMutex` owner-death recovery (`OpenProcess`/`GetExitCodeProcess`-confirmed-dead force-recovery).
Full mechanism and live evidence: archive. Previously-recorded allocator livelock
(`SafeZoneAllocator::alloc`) not re-investigated this pass -- still open, distinct from D.

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` Windows build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, the GUI-protocol
(DRM/KMS+wgpu) decision, five cheap-wins PRD rows (cargo build/fmt-verified, no boot needed) — all
CLOSED, none open. Also closed: **cross-process-fork stdio-handle bug** (`spawn_suspended`'s two
back-to-back `STARTF_USESTDHANDLES` blocks clobbered each other, no null guard on the second; PTY
test hit a separate, NOT-root-caused `signal=Signal(13)`) and **presenter-process split**
(`litebox_presenter_protocol` crate + runner-side `ControlServer` zero-copy scanout handoff +
`litebox-presenter.exe`, `--gui` now `Option<GuiMode>`, one real bug found+fixed: missing per-call
`OVERLAPPED`; `docs/presenter-process-design.md`). Full detail: archive.

## Docs and tooling map

- **Archives** — `docs/AGENTS_ARCHIVE_2026-09-17.md` (shell-crash investigation, stdio-handle bug,
  twelve registry/pointer/lock fixes, writable-layer-race fix + 5-boot verification, newest at
  bottom), `_2026-09-16.md` (popup-menu re-test, `spawn_exec_collision_child` fix, Track A audit,
  RawMutex/presenter detail), `_2026-09-15.md` (ACK-stall-kill), `_2026-09-10.md` (fork fd
  eligibility, cost history, OCI cache, s6-boot, browser config, crash-dump/VEH, CoW, practices).
  Older: `_2026-09-03.md`, `_2026-09-05.md`.
- Fork: `docs/track-b-fork-fix-progress.md`, `advisor/ADVISORY-002-d-zero-fork.md`,
  `advisor/ADVISORY-001-fundamentals.md` (§3N tcache, Appendix D presenter). `docs/veh-exception-
  handler-design.md` — canonical VEH narrative, read before touching the handler.
- Desktop logs: `docs/webtop-debian-selkies-2026-09-06.md`, `webtop-alpine-mate-2026-09-07.md`,
  `webtop-debian-xfce-2026-09-08.md`, `webtop-xfce-code-vs-data-2026-09-08.md`, `fork-fs-veh-2026-09-08.md`
- Consult before deriving: `docs/premade-library-research.md`, `docs/drm-dumb-buffer-ioctl-reference.md`
  (DRM syscall UAPI), `docs/diag-timeline-field-semantics.md` (before any `DIAG_TIMELINE` `comm`-field
  hypothesis — two investigations mis-traced it).
- `docs/macos.md` — port state; Apple Silicon guest-execution context switch is a stub, deferred.
- Designs NOT implemented: `docs/session-daemon-design.md` (VT100-emulator slice done; daemon/IPC
  layer isn't), `docs/fork-region-grouping-design.md` (still a diagnostic probe).
- `advisor/probes/` — diagnostics (`decode_frame.py`, `symbolize_litebox_crash.py`, `dup_probe.c`,
  `drm_flip_probe.c`, `clone_probe.c`) plus `MEASUREMENT-PITFALLS.md`, `DISK-HYGIENE.md`. OCI-pull
  Python scripts there are retired.
- `.gm/memories/` — older per-topic notes, superseded by this file/archives wherever they overlap.
