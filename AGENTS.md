# litebox — current state (2026-09-18)

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

**Eligibility** — an already-borrowed fd table, a beyond-stdio fd that isn't a pipe end/path-recorded
regular file/eventfd/close-on-exec (overridable by `LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an
unsanitizable `fs_base`/context. The old unconditional-by-`comm`-name refusal for `Xvfb`/
`dbus-daemon` is REMOVED as of the twelfth pass (below) — both now go through this same scan like
everything else. On a real `debian-xfce` boot the only remaining blocking kind is `unix-socket` — 5
refused forks of 34, down from 34/34 (pre-twelfth-pass baseline). Per-kind deviations: archive.

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

**Post-`NGINX_STARTED` CPU livelock — ROOT-CAUSED and FIXED (tenth pass, 2026-09-17).** cdb `-pv`
poor-man's sampler (WPR/WPA needs admin, unavailable): one thread burning 100% of a core
continuously (0 → 732s user CPU over ~12 real minutes), always inside `LocalPortAllocator::
ephemeral_port`/`deallocate` — genuinely executing `hashbrown` SIMD probe code that never
terminates, **not** the then-unconfirmed `SafeZoneAllocator` spinlock (live-caught for real in a
different call path in the twelfth pass, see above). Root cause: `LocalPortAllocator::refcount`
was a `HashMap` whose backing table is private-per-process-heap, reachable only via a raw pointer
meaningless to an attaching fork child — the same stale-cross-process-pointer class fixed a dozen
times today, not yet audited for this nested field. **Fixed**: converted to a fixed, pointer-free
`[u16; 65535]` array (`litebox/src/net/local_ports.rs`), same pattern as `socket_set`'s
`MAX_SOCKETS` array. **Second instance found immediately on re-verify**: `closing_in_background`
(`Vec<SocketHandle>`, same mechanism) hit an `index out of bounds` panic in smoltcp's
`SocketSet::retain` — fixed the same way, converted to `[Option<SocketHandle>; MAX_SOCKETS]`
(`litebox/src/net/mod.rs`). `interface`/`queued_for_closure` remain open (the latter also touches
`DescriptorTable::drain_entries_full_covered_by`). Verified: `cargo build --release` clean, 25
net unit tests pass; two live re-runs reached `NGINX_STARTED` with sane distributed CPU, livelock
confirmed gone.

**Poison-on-dead-holder scheme for `Network` — DESIGNED, IMPLEMENTED, and LIVE-VERIFIED
(eleventh pass, 2026-09-17).** `RawMutex` gained a `poisoned: AtomicBool`, set by
`try_recover_from_dead_holder_unregistered` on every dead-holder recovery, read-and-cleared by
`take_poison()`; `litebox::sync::Mutex::lock_recovering_poison()` is an opt-in method (NOT wired
into ordinary `lock()`). `GlobalStateHandle::net_lock` is the one call site that opts in: on
poison, `Network::reset_after_poisoning` wholesale-resets `socket_set`, `closing_in_background`,
`queued_for_closure`, `local_port_allocator` before handing out the guard — `SocketHandle` is a
bare `usize` with no generation counter, so wiping every field that could hold one is the only way
to tell stale from live. **Two more instances of a distinct, pre-existing bug found and fixed the
same pass** (live `cdb`-caught, frozen bit-for-bit across 6 re-samples in
`buddy_system_allocator::LockedHeapWithRescue::dealloc`): a `SocketSet::remove`d `Socket`'s normal
`Drop` ran its RX/TX ring buffers' `Vec` destructor through the CURRENT process's allocator on a
pointer naming a DIFFERENT (often dead) process's private heap. Fix: `core::mem::forget` the
removed `Socket` instead of dropping it — that memory was never this process's to free; Windows
reclaims it when the owning process exits. `close_handle`/`listen`'s own `socket_set.remove` sites
are unaffected (sockets are never fork-carried, remover == allocator always there).

**Live verification, five `LITEBOX_PROCESS_FORK=1` boots.** Runs 1-2 failed the WRONG way (`Start-
Process` silent-exit artifact, already warned about above). Run 3 `cdb`-confirmed the new livelock
above, killed, root-caused, fixed. Runs 4-5 (post-fix binary): real dead-holder-recovery fired live
in both, zero panic-cascade; run 4 reached `SELKIES_PORT_UP`+`DE_LAUNCHED` (furthest point that
day) with one disclosed non-fatal residual panic (stale `SocketFd`/`LocalPort` token after an
unrelated reset); run 5 reached `SELKIES_SUPERVISOR: giving up after 30 attempts`, same pattern,
no livelock. Full per-run transcripts/`cdb` samples: `docs/AGENTS_ARCHIVE_2026-09-17.md`.

**Did NOT reach the browser/terminal/apps milestone (runs 4-5, eleventh pass)** — `SELKIES_SUPERVISOR`
exhausted 30 respawn attempts, port 8081 refused connections, boot settled into stable `HOLD t=`
rather than crash-looping. At the time this was attributed to the Xvfb/dbus by-name exclusion
below; **that attribution is now superseded** — see the twelfth-pass entry just below, which
removed the exclusion and found the real remaining blocker is one layer deeper.

**`XVFB_FAILED`/`DBUS_FAILED` under the OLD by-name exclusion (fifth pass) — historical, exclusion
since removed.** `xset q` itself got killed (signal) immediately before `[s] XVFB_FAILED`, not
Xvfb failing to start — likely a false negative, since the boot still reached `SELKIES_PORT_UP`/
`DE_LAUNCHED` after, which needs a real X display. Cause at the time: `Xvfb`/`dbus-daemon` were
refused cross-process-fork eligibility by name, so they ran the THREAD-based path (ADVISORY-001
§3N tcache class). Superseded by the twelfth-pass entry below, which removed that exclusion.

**By-name exclusion relaxed and re-tested (twelfth pass, 2026-09-17/18) — new, precisely-characterized blocker found.** `try_cross_process_fork`
(`litebox_shim_linux/src/syscalls/process.rs`) unconditionally refused any `comm` matching
`Xvfb`/`dbus-daemon` before the fd-eligibility scan even ran (added `4bad287`, when `Network`
internals were still private-per-process-heap, so a cross-process-forked Xvfb would have been
unreachable regardless). That precondition is now false (`d1ff9d2`, `6fc102c`), so the by-name
block was removed, letting both comms fall through to the SAME fd-eligibility gate as everything
else (the `unix-socket` fd-kind refusal itself is untouched). Live-verified, `LITEBOX_PROCESS_FORK=1`
+ `.wfgy/webtop_stack.sh`: Xvfb DOES now genuinely cross-process-fork (direct log proof, not
inferred: a same-run WARN shows a DIFFERENT guest pid than the connecting client owning the bound
X11 socket — `[unix_addr_presence] ECONNREFUSED but address IS bound, by a DIFFERENT guest pid ...
self_pid=17392 owner_pid=16756`). `XVFB_FAILED`/`DBUS_FAILED` still fire, but for a NEW, DIFFERENT,
now-precisely-characterized reason, not the old thread-based tcache class: `unix_addr_table`'s
`Backlog`/`Channel` connection DATA (as opposed to the presence side-index already shared per
"`unix_addr_table` presence sharing" above) is still real per-process-heap, so a client in a
DIFFERENT cross-process-forked guest process gets ECONNREFUSED even though the listener is
genuinely alive and bound — the exact gap this file's "Open here" section already named
("guest processes share no AF_UNIX/loopback/FIFO namespace"), now hit by name for the first time.
Safety: zero crash/corruption from the relaxation itself — boot reached its stable `HOLD t=`
steady state both after `XVFB_FAILED`+`DBUS_FAILED`+`DE_FAILED` (run 1) and separately in a second
boot (run 2, independently confirmed safe, though that run's own progress was gated by an unrelated
finding below). **Next real pickup for the browser milestone**: extend the `unix_addr_table`
presence-sharing PATTERN (flat, fixed-slot, lock-free) from presence-only to the actual
`Backlog`/`Channel` connection data — separate, larger, not attempted this pass.

**`SafeZoneAllocator::dealloc` spinlock livelock — LIVE-CAUGHT for the first time (twelfth pass,
run 2), previously only theorized ("Previously-recorded allocator livelock (`SafeZoneAllocator::
alloc`) not re-investigated this pass -- still open" — RawMutex section above).** Unrelated to the
Xvfb/dbus relaxation above (hit deep in a `[process_fork_diag] globalstate-probe (child)`
diagnostic's own `std::process::exit()` call, present since before this pass). Two live `cdb -pv`
samples ~27s apart, symbolized against the matching same-timestamp `.pdb` (`-y <dir>`, required —
raw offsets alone mis-suggested `ntdll!RtlFreeActivationContextStack`/`ntdll!LdrShutdownProcess`
internals until symbolized), showed a single thread bit-identical at the same leaf instruction
(`test al,al` in `SafeZoneAllocator::<WindowsUserland as GlobalAlloc>::dealloc+0x59`, disassembly
confirms a classic `lock cmpxchg`+`pause`-backoff spin loop) while its User Mode CPU time climbed
continuously (9:22 → 9:49 and counting) — genuinely spinning, not blocked. Call chain:
`diag_process_fork_globalstate_probe_inner` → `std::process::exit` → Rust's own TLS-destructor
cleanup (`std::sys::thread_local::guard::windows::cleanup`/`destructors::list::run`) → freeing a
TLS-held `Vec<String>`/`Option<..>` → `SafeZoneAllocator::dealloc` spins forever acquiring its
internal `spin::mutex::SpinMutex` (`litebox/src/mm/allocator.rs`) — a raw external-crate spinlock
with NO dead-holder recovery, unlike `RawMutex` (which got exactly this recovery mechanism earlier
today). Consistent with a thread/process elsewhere dying while holding this global-allocator lock,
permanently starving every future `alloc`/`dealloc` in that process. **Notable operational
side-effect**: the stuck process resisted `Stop-Process -Force`/`taskkill /F` for roughly two
minutes (repeated attempts, `Get-Process` kept reporting it alive with climbing CPU); only
`Invoke-CimMethod -MethodName Terminate` (WMI) actually killed it. Not root-caused further this
pass (out of scope for the Xvfb/dbus task) — real fix is giving `SafeZoneAllocator`'s spinlock the
same dead-holder-recovery treatment `RawMutex` already has, or routing it through `RawMutex`
itself; high blast radius (global allocator, every allocation in every process) — deserves its own
dedicated, carefully-scoped pass, not a rushed change here.

**Fork-after-Xorg PERMANENT freeze — did NOT reproduce 2026-09-17; thread-based-fork-only.** Under
`LITEBOX_PROCESS_FORK=1` the identical script completed cleanly 2/2 — zero freeze, zero double-free.
Full evidence, a disclosed ENOMEM finding under concurrent cross-process forks: archive.

**Thirteenth pass, 2026-09-18 — shared cross-process AF_UNIX connection data plane DESIGNED and
IMPLEMENTED (`SharedUnixConnTable`/`SharedUnixConnectQueue`, `syscalls/unix.rs`'s own module doc
comment has the full design), `XVFB_FAILED`/`DBUS_FAILED` NOT yet closed.** Three real bugs found
live and fixed along the way (each independently significant, not just this feature's own
teething problems): (1) a `GlobalState`-embedded fixed array passed BY VALUE through
`create_shared_kernel_state`/`SharedArc::new` overflowed the constructing thread's stack at 8 MiB
— shrunk to ~32 KiB, same order of magnitude as `SharedUnixAddrPresenceTable`'s already-proven-safe
size; (2) `WaitContext::remaining_timeout()`'s `None` is ambiguous between "no deadline" and
"deadline expired", which silently turned a 3-second bounded cross-process `connect()` timeout
into an infinite poll loop once live-tested — fixed by capturing `cx.deadline().is_some()` once
before the retry loop, matching `epoll.rs`'s own already-correct pattern; (3) `Backlog::
check_io_events` never checked the new shared connect queue, so a real event-driven listener
(Xvfb, dbus-daemon) blocked in `poll`/`epoll_wait` never even got to `accept()` for a cross-process
client — fixed, then further extended the pre-existing "bounded 15ms repoll for an unwakeable fd
kind" mechanism (built for stdin/evdev/timerfd) to cover AF_UNIX sockets too, since no genuine
cross-process wake exists anywhere in this codebase. **Did NOT reach the browser/terminal/apps
milestone**: `XVFB_FAILED`/`DBUS_FAILED` persisted in every one of seven live boots that reached a
decision; the last run (epoll fix included) hadn't reached a decision at all after 8 real minutes
(vs ~1.5-3 min every earlier run) when killed for time — not root-caused, possibly the broadened
repoll scope's own added latency, needs a timed A/B. No isolated minimal repro was built this pass
(a real process deviation, owed as the next pass's first step). Full narrative, exact log
evidence, and files touched: `docs/AGENTS_ARCHIVE_2026-09-18.md`.

### Track B — current pickup list, precise (full pass-by-pass evidence: archive)

(-1) **NEW TOP PRIORITY, thirteenth pass**: build the minimal isolated cross-process AF_UNIX repro
(one parent listens, one genuinely-cross-process-forked child connects, both directions exchange
real bytes) that should have come BEFORE the full-boot attempts above — this pass skipped it under
time pressure and paid for it (three iterations of boot-then-diagnose instead of a fast, cheap,
isolated loop). Once that passes, re-run `webtop_stack.sh` under run 7's binary (epoll/`PollSet`
fix included) for a clean, uncontaminated read on whether `XVFB_FAILED`/`DBUS_FAILED` finally
close. If run 7's timed A/B (above) shows the broadened bounded-repoll scope is the slowdown
culprit, narrow it (e.g. only a Unix socket in `Listen` state, or only a `Shared`-transport
connected socket, not every same-process Unix socket fd) before re-testing. (0)

(0) **~~TOP PRIORITY, twelfth pass~~ — superseded by thirteenth-pass entry above.** (0b) `SafeZoneAllocator`'s `spin::mutex::SpinMutex` (`litebox/src/mm/allocator.rs`)
needs the same dead-holder-recovery treatment `RawMutex` already has — live-caught spinning forever
in `dealloc` this pass, high blast radius, own dedicated pass. (1) ~~Debugger-root-cause the
dead-holder-recovery data-inconsistency panic~~ — DONE, eleventh pass. (1b) `queued_for_closure`'s
own still-open cross-process-Vec hazard (nothing yet converts its STORAGE to a fixed pointer-free
array the way `closing_in_background`/`socket_set` already were) remains a live risk: any process
reading/pushing it while attached rather than constructing could still hit the stale-pointer class
on the Vec header itself; (2) debugger-root-cause `litebox/src/event/wait.rs:224`'s
`unreachable!()` on garbage thread state (dozens per boot, most frequent panic historically, NOT
yet debugger-confirmed — do not patch blind); (3) root-cause the `/tmp/empty` writable-layer
cross-child-visibility gap; (4) finish the `Network` shared-arena redesign (`interface`,
`queued_for_closure` remain); (5) after (0)-(4), `timerfd`/`signalfd` are the next-cheapest
carriable fd kinds before attempting `socket`/`unix-socket`/`pty`/`epoll`.

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

**Sixth pass** — writable-layer-adoption race on a late fork child stalled past
`SELKIES_BIND_WATCHDOG_STARTED`, never reached `DE_LAUNCHED`. **Seventh pass — ROOT-CAUSED and
FIXED, two bugs** (a shared `CONTAINER_FS_SNAPSHOT_ENV_VAR` path race-deleted by the first of
several importers, and a non-atomic `std::fs::copy` export letting a concurrent importer read a
torn tar) — fix: stopped the premature delete, routed the export through the existing atomic-rename
primitive (`publish_as_container_fs_snapshot`, widened to `pub`). Live-verified 5/5 boots,
~800+ adopt/export cycles, zero recurrence, all reaching `DE_LAUNCHED`+`SELKIES_PORT_UP`
deterministically for the first time. Full mechanism/transcript: archive.

**Open here.** One client per selkies instance, no slot reclaim on reload. An intermittent host AV ends
some runs (host-allocator region fault) — separate non-determinism from the ACK-stall-kill below.
Architectural gap: **guest processes share no AF_UNIX/loopback/FIFO namespace**, so a cross-process fork
gives zero AVs but Xvfb is unreachable from its own clients — precisely confirmed and named
(`unix_addr_table`'s `Backlog`/`Channel` connection data) in the twelfth-pass entry above; one
shared host-side transport, or extending that table's presence-sharing pattern to real connection
data, would put the whole desktop on the crash-free path (`docs/fork-fs-veh-2026-09-08.md:128-144`).

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
constructs `GlobalState` first, frozen into cross-process-shared bytes, meaningless/dangling in
every other attaching process. Fix pattern: shadow with a fresh per-process copy (`litebox`,
`proc_self_info`/`pts_registry`, `elf_patch_cache`/`exec_ranges_cache`/`segment_scan_cache`,
`futex_manager`), rebind via a locking accessor (`Network` via `net_lock`, `Pipes.litebox` via
`pipes()`), or replace a private-heap `Vec` with a fixed-slot pointer-free array. Instance 12
(`net_lock` left permanently locked by an exiting fork-child) was a lock-liveness/owner-death gap,
not a stale pointer — fixed below. Does NOT close `XVFB_FAILED`/`DBUS_FAILED`; `pty_registry`/
`flock_registry`/etc. remain real, still-open. Full detail: archive.

## RawMutex lost-wakeup, Pipes stale-pointer, FutexManager sharing gap, and cross-process-fork lock-orphaning -- ALL FOUR FIXED 2026-09-17

Four fixes, live-verified, landed: (A) `RawMutex::resolve_waiter_event`'s cross-process branch
panicked on a stale pid instead of signaling the real waiter — `waiters` is now a fixed-32-slot
pointer-free `WaiterQueue`. (B) `Pipes.litebox`'s stale pointer crashed a killed fork child's
stdio teardown — now rebound via `GlobalStateHandle::pipes()` same as `net_lock`. (C)
`FutexManager` cross-process sharing hung on stack-allocated `LoanList` entries — resolved by
giving each process its own fresh `FutexManager`. (D) A cross-process-fork child's un-shutdown
`net_worker` thread could be killed mid-hold of `net_lock`, orphaning it — fixed with `RawMutex`
owner-death recovery (`OpenProcess`/`GetExitCodeProcess`-confirmed-dead force-recovery). Full
mechanism: archive. `SafeZoneAllocator::alloc`'s spinlock livelock (flagged here as still-open,
distinct from D) was LIVE-CAUGHT for the first time in the twelfth pass — see that entry above.

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

- **Archives** — `docs/AGENTS_ARCHIVE_2026-09-18.md` (thirteenth pass: shared cross-process AF_UNIX
  connection data plane, three real bugs found+fixed, seven-boot verification, newest),
  `_2026-09-17.md` (shell-crash investigation, stdio-handle bug, twelve registry/pointer/lock
  fixes, writable-layer-race fix + 5-boot verification), `_2026-09-16.md` (popup-menu re-test,
  `spawn_exec_collision_child` fix, Track A audit, RawMutex/presenter detail), `_2026-09-15.md`
  (ACK-stall-kill), `_2026-09-10.md` (fork fd eligibility, cost history, OCI cache, s6-boot,
  browser config, crash-dump/VEH, CoW, practices). Older: `_2026-09-03.md`, `_2026-09-05.md`.
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
