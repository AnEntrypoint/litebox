# litebox — current state (2026-09-21)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, and a claim a later commit superseded, were deleted rather than hedged. Reference
detail is drained to the `docs/AGENTS_ARCHIVE_*.md` files and per-investigation logs to the dated
`docs/*.md` in the map below — read those for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not in a separate memory file.

## The cheap repro — start here

```
target/release/litebox_runner_linux_on_windows_userland.exe -Z \
  --oci-image docker.io/library/debian:stable-slim -- /bin/bash -c '<script>'
```

One ~81MB layer, `[cache] HIT` after the first pull, real GNU coreutils instead of busybox — which
matters: coreutils `touch` issues the `utimensat(fd, NULL, …)`/futimens form busybox's never reaches
(`caaac79`). Two host-side gotchas, each already costly:

- **PowerShell, never Git Bash** — Git Bash rewrites `/absolute/guest/paths` into
  `C:/Program Files/Git/...` before the runner sees them, giving a misleading `ENOENT`. `Start-Process
  -RedirectStandardOutput/-RedirectStandardError` makes the runner exit almost instantly with zero guest
  output (no crash dump, no event-log entry); use `& .\runner.exe ... *> combined.log` instead.
- **Single quotes only inside `-c`** — embedded double quotes are corrupted crossing into the child's
  Win32 command line. This masqueraded as deep fork/stack-pointer corruption for a whole sub-session.

**Log level**: default is `warn,litebox_platform_windows_userland::fork_verify=error` (`EnvFilter`'s
own ERROR-only default discarded all real `warn!` sites; `fork_verify` is pinned to `error` because
it warns per single-stepped instruction). Do **not** add `LITEBOX_LOG=error` by reflex; use
`fork_verify=warn` when a fork heal is the subject.

## Standing lessons and hard constraints

- **No WSL or hypervisor, ever** — always run under the matching runner
  (`litebox_runner_linux_on_windows_userland.exe`/`litebox_runner_linux_userland`); cross-compiling FOR
  Linux is fine, running the result in a VM defeats the premise.
- **`fork_verify.rs`'s stale-pointer-healing bug class is Windows-only** (real `fork()` gives the
  child identical addresses) — never port to another platform's crate.
- **Never `bcdedit /debug on`** without a kernel debugger already attached — two full-host freezes
  needing a power-cycle.
- **A process spinning inside a dead-locked allocator/spinlock resists `Stop-Process -Force`** —
  use `Invoke-CimMethod -MethodName Terminate` (WMI) instead. `cdb -p <pid>` must use `-pv`/`qd`,
  never a bare `q` (kills the target).
- **Never run two full-stack verifications concurrently** — starves both, looks exactly like a real
  hang. Kill every `litebox_runner` between runs; watch `FreePhysicalMemory`, kill on a falling
  trend not a fixed RSS number.
- **`LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check**, never
  `PrintWindow`/`CopyFromScreen`. A pixel count alone never identifies WHO painted a frame — decode
  frame structure (`advisor/probes/decode_frame.py`) and correlate against `DIAG_TIMELINE execve`'s
  real argv0.
- **Never time litebox with one host process per datapoint** (bare spawn costs 1.6-2.3s) — run N
  iterations inside ONE guest process. Never subtract timestamps across a parent log and a
  fork-child log — `init_logging()` resets elapsed time to ~0 per child.
- **Release-binary `cdb` reads are unreliable** — MSVC linker ICF folds distinct functions into
  one symbol (no `[profile.release]` override exists, so LTO is off but ICF still runs). Build
  `cargo build -p litebox_runner_linux_on_windows_userland` (no `--release`) for any `cdb` session
  needing a trustworthy stack — confirmed live, eighteenth pass, refuted two release-build leads.
- **Refusal errno choice is API contract** — EPERM lets callers degrade, EINVAL/ENOSYS fails them
  hard; wrong choices have silently broken whole subsystems before (archive; 30th pass's AF_UNIX
  `EAGAIN`-vs-`EINPROGRESS` connect() fix is the newest instance).
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
- **Never trust a container tag name for its WM/session contents** — verify by registry manifest +
  blob tar-listing, or a live in-guest `/usr/bin` listing.
- **Never record a test count not watched run to completion**; never leave a suite red for an
  environmental reason.
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

**Per-fork cost** was ~3.5-5s, now ~1.2s; `NGINX_STARTED` in under a minute versus never in 15+.
Use `LITEBOX_DIAG_FORK_TIMING=1` for the next cost question.

**Still open**: nginx's own SSL-cert generation fails on its first real startup attempt — the
original symptom this investigation began from, genuinely not root-caused
(`docs/track-b-fork-fix-progress.md:146-152`).

**Passes 4-25 (2026-09-17/20), all FIXED/REFUTED, live-verified — full narrative: `docs/
AGENTS_ARCHIVE_2026-09-17.md`/`_2026-09-18.md`.** Landed: nginx pipe-EOF handle
allow-list; `Network::socket_set`/`LocalPortAllocator`/`closing_in_background` made shared-arena
fixed arrays; `RawMutex` poison-on-dead-holder scheme; by-name Xvfb/dbus-daemon fork exclusion
relaxed; shared cross-process AF_UNIX connection plane (`SharedUnixConnTable`/
`SharedUnixConnectQueue`) designed+implemented+leak-fixed; a platform-wide smoltcp
stale-`SocketHandle` panic fixed via `catch_unwind`+`force_reset_network_after_panic()`;
`webtop_stack.sh`'s `[ -S "$XSOCK" ]`→`-e` fix; `xset q`'s silent kill fixed (`memfds`/
`shared_files` per-process-shadowed); a `GlobalState` field audit (`unix_addr_table`/
`fifo_registry` per-process-shadowed, `sysv_shm` fixed array); `sys_wait4`'s `pid==-1`/`pid>0`
no-repoll gaps both fixed; the `$XSOCK` stall closed for good via `SharedUnixAddrPresenceTable`
consulted on `ENOENT`. REFUTED along the way (recorded so they are never re-tried): the
`wait_on_tun` two-holder-deadlock theory, `has_pending`/queue logic as the ppoll-stall cause, the
AF_UNIX rendezvous "livelock" theory (24th pass: live per-request instrumentation proved the
mechanism itself sound, one real connect per boot, ~23ms, no timing/address mismatch). Still open
from this range: `SafeZoneAllocator::dealloc`'s spinlock livelock (no dead-holder recovery, unlike
`RawMutex`); `pty_registry`/`daemon_pty_masters`/`flock_registry`/`drm`/`evdev` need a deeper
redesign than a flat fixed array (non-POD payload); `UnixInitStream::check_io_events`'s static
`Init`-state report (unconfirmed).

**Twenty-sixth pass (2026-09-20)** — FIXED `EpollFile::repoll_stdin_and_timerfd_interests` reading
`is_still_ready` (always false for `EPOLLET`), not `event.is_some()`; `XVFB_UP` printed for
the first time ever. **Twenty-seventh pass (2026-09-20)** — surgical pre-create of `/tmp/empty`/
`config/`/`config/.Xresources` into `webtop_seed.tar` landed; a periodic writable-layer-sync-thread
alternative was tried, PROVEN to regress `NGINX_SUPERVISOR`, fully removed (do not re-add without
an import-lock). **Twenty-eighth pass (2026-09-21)** — `SharedFilePublishTable`
(`litebox_shim_linux/src/syscalls/file.rs`) closed `DBUS_FAILED` for good; five further live-caught
`Network`/`fd` cross-process stale-index bugs fixed one at a time (`close`/`close_handle`'s
unguarded `socket_set` touches, `queued_for_closure` fixed-array conversion + `None`-tolerant
lookups, `LocalPortAllocator`'s `unreachable!()`s, a full `net/mod.rs` `socket_set` guard sweep,
`drain_entries_full_covered_by`'s `matches_subsystem` guard) — two consecutive full release-binary
boots ran panic-free to `DE_LAUNCHED` for the first time ever. **Twenty-ninth pass (2026-09-21)** —
three more real bugs fixed live (`sys_execve`'s `copy_vector` reading `argv`/`envp` after
`end_fork_child_verification()` already cleared the translation map it needs,
`litebox_shim_linux/src/syscalls/process.rs`; `Network::reset_after_poisoning`'s own unguarded
`socket_set.remove`, `litebox/src/net/mod.rs`; a `STATUS_STACK_OVERFLOW` in two more bare
`std::thread::spawn` sites needing the same 32 MiB `INITIAL_GUEST_THREAD_STACK_SIZE` treatment) but
**none were the `DE_FAILED` cause** — a live `execve` trace
(`litebox_shim_linux::syscalls::process=trace`) proved `xfce4-session`'s own envp holds correct
`DISPLAY=:1` bytes pre-loader, the ELF loader's stack-building code was inspected line by line and
found sound, and `/usr/bin/printenv` (a real `getenv()` call) reads `DISPLAY` correctly in the
identical invocation shape — narrowing the failure to something inside `xfce4-session`'s own
process. Full narrative, every repro command, every ruled-out alternative for all four passes:
archive.

**Thirtieth pass (2026-09-21) — the DISPLAY/getenv hypothesis DEFINITIVELY REFUTED with direct
live evidence; one real POSIX errno-contract bug found+FIXED in the AF_UNIX cross-process connect
path; a NEW, likely-more-fundamental lead found: Xvfb itself SIGABRTs deterministically ~190-197s
into every full boot.** Built an LD_PRELOAD interposer (`getenv_probe.so`, host-cross-compiled via
`clang --target=x86_64-unknown-linux-gnu -shared -fPIC -nostdlib
-Wl,--unresolved-symbols=ignore-all`, no sysroot needed) that overrides `getenv()` process-wide and
logs every call (name + real result, read directly from `environ`) to `/tmp/getenv_trace.log`,
injected via `LD_PRELOAD=/tmp/getenv_probe.so` exported near the top of `webtop_stack.sh` (so every
process forked after `export DISPLAY=:1` loads it). Live result: 19
separate `GETENV query name=DISPLAY` calls across the full boot, ALL returning `result=[:1]`,
including two complete `GDK_BACKEND` → `WAYLAND_DISPLAY` → `DISPLAY` backend-probe sequences (real
GDK/GTK backend-selection logic, matching xfce4-session's own process count — one per launch
attempt) — `getenv(DISPLAY)` is proven correct at every single call site, including deep inside
GDK. This closes the DISPLAY/`getenv()`/`environ` line of investigation for good; do not re-open it
without new information. Enabling `litebox_shim_linux::syscalls::unix=debug` on the same boot then
found the real mechanism: `[unix_addr_presence] ECONNREFUSED but address IS bound, by a DIFFERENT
guest pid` fires repeatedly during the `xfce4-session` launch window, and tracing
`connect_cross_process` end to end showed BOTH real bugs it masks. (1) **FIXED**: a non-blocking
AF_UNIX `connect()` that has not yet been claimed by the listener's `accept()` loop surfaced
`Errno::EAGAIN` (via the blanket `TryOpError::TryAgain -> Errno::EAGAIN` conversion in
`litebox_common_linux/src/errno/mod.rs`) instead of the POSIX-mandated `EINPROGRESS` — the exact
signal real client libraries (libdbus among them) branch on to mean "poll for writability, this is
not a failure"; `litebox_shim_linux::syscalls::net::connect` already carries the identical
`TryAgain -> EINPROGRESS` override for the analogous TCP path, so this mirrors an already-proven
pattern. Fixed in both `UnixStream::connect` and `UnixStream::connect_cross_process`
(`litebox_shim_linux/src/syscalls/unix.rs`), live-verified: the boot's own new
"request not yet claimed (non-blocking) ... returning EINPROGRESS" log line fires as designed, and
a live `connect_cross_process` outcome tally on the post-fix boot showed 22 posted / 20 completed /
2 EINPROGRESS / 0 timeouts / 0 hard failures — materially cleaner than the pre-fix boot's outright
failure on the same call shape. (2) **NOT yet fixed, precisely scoped**: that same branch also
unconditionally cancels the just-posted `SharedUnixConnectQueue` request on a non-blocking
not-yet-ready outcome, so a connection attempt that does not complete synchronously within the one
syscall can never complete later no matter how long the caller polls — needs a
`UnixStreamState::Connecting(request_idx)`-shaped state so a repeat `connect()`/poll on the same fd
reattaches to the SAME pending request instead of abandoning it; left as its own scoped follow-up
rather than risked half-implemented. **DE_FAILED still fires after fix (1) alone** — expected, since
fix (2) is unresolved and a wholly separate, likely more fundamental issue was found: `Xvfb` itself
hits a real SIGABRT at ≈190-197s into every full boot — see the thirty-first pass immediately below
for its REAL abort text (caught live for the first time) and root mechanism.

**Thirty-first pass (2026-09-21)** — Xvfb's real abort text caught live for the first time
(`litebox_diag::stderr_capture` in `file.rs`'s `sys_write`): the SIGABRT chased since the
thirtieth pass is the TAIL of Xorg's own crash handler after a real SIGSEGV(11), guest address
`0x7feffecdd400`, bit-identical across boots. A live `cdb` attach caught an access violation
(`sub rax, fs:[0x28]`) but it was a SEPARATE, already-handled "FS_BASE-reset" class (Windows
clears FS_BASE on its own initiative, silently repaired by the existing VEH), firing 22s into
Xvfb's life, not the real crash.

**Thirty-second pass (2026-09-21) — cdb REFUTED as viable for this crash (attaching it starves
the boot of the very X11 traffic the crash needs); a low-overhead in-process diagnostic caught
the REAL fault clean, twice, with hardware ground truth; exact Xvfb call site still open.**
Finishing the 31st pass's own cdb pickup (auto-continue via `gn` on every first-chance AV,
`.wfgy/xvfb_live_cdb_orchestrator3.ps1`) worked exactly as scripted, but attaching cdb to Xvfb's
own pid measurably perturbs it: two consecutive boots hit `XVFB_FAILED`/`DE_FAILED` immediately
after attach (vs. clean boots with no debugger) — Windows freezes the whole debugged process
during each first-chance-exception script (confirmed via `!analyze -v`'s own elapsed-time field),
and the FS_BASE-reset class fires often enough that this loses the already-narrow `xset q`
liveness race, which then starves `DE_LAUNCHED`'s own X11 traffic the crash is byte-volume-
correlated with. **cdb is structurally unable to observe this bug.** Instead, broadened
`litebox_platform_windows_userland/src/lib.rs`'s existing `diag_fataldump_enabled()` gate
(`26fe95c`): it already did a full register/stack/code-bytes dump as a synchronous `eprintln!`
inside the VEH handler (no debugger, no freeze), just gated to a small address-magnitude range
from an unrelated older investigation — dropping that restriction (the real overhead guard,
`faulting_instruction_has_fs_override`, is independent of address magnitude) let it also catch
this crash. `LITEBOX_DIAG_FATALDUMP=1`, no debugger: two independent boots reached the real
fault with `XVFB_UP`/`DBUS_UP`/`DE_LAUNCHED` all firing normally first, at ~207s into Xvfb's own
life, bit-identical `rip=0x7fefede9dabd` and fault address `0x7feffecdd400` both times.
Byte-matched (`objdump -d` against the same-BuildID runtime `libc.so.6`) to glibc's own AVX2
memcpy/memmove multiarch routine (`vmovdqu (%rsi),%ymm0`, file offset `0x162abd`) reading 64+
bytes from a source pointer that is COMPLETELY UNMAPPED on the host (`type=0x0 alloc_base=0x0`)
— not FS_BASE-related (no `0x64` prefix), not a litebox transport bug (`SharedByteRing` re-read
as sound), a genuine wild pointer Xvfb itself computed. `0x7feffecdd400` = `TASK_ADDR_MAX -
0x1312C00`, exactly ~19.2MB below the top of the guest address space. Xorg's own self-printed
backtrace was found NOT trustworthy past frame 0 (frames 1-9 of 13 fail `dladdr()` entirely; live
evidence, don't re-trust `(EE) Backtrace:` addresses without cross-checking a real section
table). Root cause of WHICH Xvfb call site feeds the bad pointer remains open — needs real
CFI-based unwinding (no tool for this readily available this pass) or upstream Xvfb source
cross-reference; full evidence, every ruled-out hypothesis, exact repro, and the flakiness
profile (2 clean captures / 6 attempts, unrelated to the fix): archive.

### Track B — current pickup list, precise (full pass-by-pass evidence: archive)

(-1) ~~Build the minimal isolated cross-process AF_UNIX repro~~ — DONE, fourteenth pass. (0)

(0b) `SafeZoneAllocator`'s `spin::mutex::SpinMutex` (`litebox/src/mm/allocator.rs`) needs the same
dead-holder-recovery treatment `RawMutex` already has — live-caught spinning forever in `dealloc`,
high blast radius, own dedicated pass. (2) debugger-root-cause `litebox/src/event/wait.rs:224`'s
`unreachable!()` on garbage thread state (dozens per boot, most frequent panic historically, NOT
yet debugger-confirmed — do not patch blind). **Current top blocker, thirty-second pass — hardware
ground truth in hand (bit-identical `rip`/fault-address across two independent boots, glibc AVX2
memcpy/memmove reading 64+ bytes from a fully-unmapped `0x7feffecdd400`), exact Xvfb call site
still open**: pickup is real CFI-based stack unwinding (no tool for this was readily available
this pass — `.wfgy/xvfb.debug`'s DWARF `.eh_frame`/`.debug_line` data is already fetched and
matches the exact running `BuildID sha1=6440f00c805782c9a39a5acd92855079e9fffc92`, a naive
raw-stack-word scan surfaced 4 plausible-but-probably-stale candidates) or upstream Xvfb source
cross-reference (network access confirmed working — `debuginfod.debian.net` answered in one
request) to determine whether this is a litebox-shim bug or a genuine upstream Xvfb defect. Use
`LITEBOX_DIAG_FATALDUMP=1` (no debugger — attaching one was proven this pass to starve the boot
of the traffic the crash needs), NOT cdb, for the next capture.
~~DISPLAY/getenv() as the DE_FAILED cause~~ — REFUTED FOR GOOD, thirtieth pass (see
that pass's own entry above for the LD_PRELOAD-interposer evidence). ~~AF_UNIX connect()
EAGAIN-vs-EINPROGRESS~~ — FIXED, thirtieth pass; that same code path's
request-cancellation-on-first-non-blocking-miss behavior is its own separate, precisely-scoped, NOT
yet fixed follow-up — see that pass's own entry above for the exact
`UnixStreamState::Connecting(request_idx)` reasoning. (4) finish the `Network` shared-arena redesign (`interface`
remains — `queued_for_closure` is fixed, twenty-eighth pass); (4b) `pty_registry`/
`daemon_pty_masters`/`flock_registry`/`drm`/`evdev` (`GlobalState` fields, eighteenth-pass audit)
genuinely need cross-process visibility per their own doc comments but hold non-POD payload
(Arc-based state, `Pollee` observer lists), so need a deeper redesign than `sysv_shm`'s
flat-Copy-slot-array fix — not yet touched by any pass, not on the Xvfb/selkies boot path so lower
urgency; (5) after (0)-(4), `timerfd`/`signalfd` are the next-cheapest carriable fd kinds before
attempting `socket`/`unix-socket`/`pty`/`epoll`. (6) The general writable-layer-visibility gap for
LARGE/unbounded content (`/tmp/de.log`/`/tmp/de2.log`/`/tmp/xvfb.log`, `/tmp/wm1`/`/tmp/wm2` —
reconfirmed real, thirtieth pass: neither `[de]` nor `[de2]`-tagged output ever appeared in either
thirtieth-pass boot log despite `DE_FAILED` firing both times) remains open —
`SharedFilePublishTable`'s 256-byte cap must NOT be widened to try to cover it; a real fix needs its
own design (chunked publish, or a genuine shared-arena ring rather than a snapshot table).

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

A trampoline-extension failure used to poison a whole segment's syscalls, now fixed (archived).
Tags verified live, never from the name (archived): `linuxserver/webtop:alpine-mate` ships MATE not
XFCE; `alpine-xfce` doesn't exist; `debian-xfce`/`ubuntu-xfce` ship real XFCE.

**X server choice**: for the DRM/wgpu on-screen (`--gui`) path use `Xorg` with `modesetting` — litebox's
virtual DRM device is legacy-KMS + dumb-buffer + XRGB8888 only, no atomic modeset/GBM/EGL, so a GBM-first
compositor lands on its least-tested fallback, and `Xvfb` never touches DRM/KMS at all (zero page-flips,
indistinguishable from "never drew"). For browser/selkies, `Xvfb` IS correct and verified — its
`-shmem` framebuffer works now that SysV shared memory exists.

**Durable artifacts**: `C:\dev\litebox-webtop\webtop_seatd.tar` (stock MATE webtop); the
`.wfgy/xfce-build/` weston+XFCE tar is superseded by the stock-image path.

## A real desktop renders in a browser

**XFCE renders in a real host browser, and MATE too** — full pipeline (Xvfb, selkies/pixelflux
x264, MIT-SHM) inside litebox, reverse proxy host-side only. Working config: selkies
`--addr=0.0.0.0` port **8081**, dashboard over `--publish`, `/websockets` tunnelled to 8081.
Fourteen litebox defects got here, all landed (archive).

**A stock s6-overlay image boots with no flags/stubs**: `/init` runs 16 cross-process children
with zero uncarriable fds. **The black XFCE desktop was deterministic, now fixed**: the runtime
rewriter corrupted `libLLVM.so.19.1`'s `.dynsym`, so mesa `dlopen` failed forever. Rest settled
in the archive (PI futexes, labwc SIGABRT, gdk-pixbuf, network fixes).

**XFCE also renders on the THREAD-based fork path, gated by one flag** (`docker.io/linuxserver/
webtop:debian-xfce`, `.wfgy/webtop_stack.sh`). Without it, 3/3 boots die ~7s in — ADVISORY-001 §3N's
safe-linked-tcache write (the `fork_verify: stale CODE pointer` warning right before it is a red
herring; translation is correct). Fix: `--env GLIBC_TUNABLES=glibc.malloc.tcache_count=
0:glibc.malloc.mxfast=0` as an `--env` runner flag (a bare host-shell prefix does NOT reach the guest).
Glibc-only workaround, not a fix (PRD `glibc-tunables-workaround-pending-zero-fork`);
`LITEBOX_PROCESS_FORK=1` removes the class properly but can't run a desktop until the AF_UNIX gap
below closes. Selkies also needs `--clipboard-enabled=false` (its clipboard monitor re-triggers the
same corruption every tick).

**Sixth/seventh pass (closed)** — writable-layer-adoption race fixed via the existing
atomic-rename primitive; live-verified 5/5 boots, zero recurrence. Detail: archive.

**Open here.** One client per selkies instance, no slot reclaim on reload. The AF_UNIX
connection-data-plane gap this paragraph used to describe is now DONE (thirteenth pass) and
crash-free through `DE_LAUNCHED` (twenty-eighth pass) — see the Cross-process fork section above
for the current sole blocker (`DISPLAY` loss across fork).

**The glibc/tcache crash class still sporadically hits selkies** on the THREAD-based fork path, a
SECOND corruption signature under heavy fork load (`double free or corruption (out)` SIGABRT).
Track B territory, not a tunable-coverage gap — do not re-attempt a `GLIBC_TUNABLES` fix without
evidence of a THIRD mechanism. Detail: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

### The ACK-stall-kill and port-8081 watchdog — both CLOSED (2026-09-16)

Real blocker was the guest-side patcher silently crashing on `shutil.copy2()`'s `copystat()` (no
`listxattr` shim) before ever patching `selkies.py`; fixed (`478e640`). Port-8081 double-bind fix
live-verified over 17 boot cycles + a 6000-connection stress test, zero recurrence. Detail: `docs/
AGENTS_ARCHIVE_2026-09-16.md`.

## Host-side crash machinery

A fatal host fault dumps before it dies, ungated (stack walk, `RECENT_FAULTS` ring, `RECOVERY_LOG`,
no env var needed); a real OS minidump comes only from the repeated-identical-fault circuit breaker
(`error_code` is synthesized, `is_in_guest` is tri-state — exact semantics: archive). An unexplained
`0xC0000005` may be a panic — the VEH handler enters only for the four codes it triages, registers
FIRST in the chain, sizes per-depth frames from disassembly not guesswork (`docs/veh-exception-
handler-design.md`). Cross-process sync on Windows is a hard platform constraint: every native
address/TID-based wait is process-local (`WaitOnAddress`, keyed events, `NtAlertThreadByThreadId`=
ACCESS_DENIED); only a shared kernel object crosses processes — `RawMutex` (below) is the one that
matters; `xproc_sync.rs`'s named-event primitive is live-verified but still unwired.

## Shared-memory foundations -- all DONE, live-verified 2026-09-16/17 (detail: those two archives)

`RawMutex`: no longer calls `WaitOnAddress`/`WakeByAddressSingle` (process-local per MSDN) -- manual
wait queue + one auto-reset kernel `Event` per OS thread, cross-process half real
(`DuplicateHandle`-based); gained `poisoned: AtomicBool` + owner-death recovery (eleventh pass,
above); `resolve_waiter_event`'s stale-pid panic fixed via a fixed-32-slot pointer-free
`WaiterQueue`. Shared kernel heap: a small **64 MiB, standalone, bounded**
`shared_kernel_arena_alloc` (`lib.rs`, NOT wired to `GlobalAlloc`) backs `SharedArc<T>`
(`value`+`strong: AtomicUsize`) for `LiteBoxX`/`GlobalState` placement; `SLAB_ALLOC` stays on the
private per-process path (a shared bump allocator exhausted an 8 GiB pool in 45-90 execs).
**Root cause of the whole `GlobalState`-sharing class, precisely characterized**: `SharedArc::new`
shares only `T`'s literal inline bytes -- any REGISTRY that was a `BTreeMap`/similar has its NODES
on the private per-process heap, meaningless to an attaching process (PRD:
`globalstate-nested-collections-not-actually-shared`). Of the original list (`unix_addr_table`,
`pty_registry`, `daemon_pty_masters`, `flock_registry`, `fifo_registry`, `sysv_shm`, `memfds`,
`shared_files`): `unix_addr_table`/`fifo_registry`/`memfds`/`shared_files` are now
per-process-shadowed on `GlobalStateHandle`, `sysv_shm` is a real shared-arena-native fixed array.
`pty_registry`/`daemon_pty_masters`/`flock_registry` remain open (pickup list).
`SharedUnixAddrPresenceTable` (`syscalls/unix.rs`) established the reusable flat-table PATTERN
(`sysv_shm` and the AF_UNIX connection-DATA layer both reused it next): fixed-256-slot,
pure-atomic, lock-free `(kind, key bytes<=108, owner pid)` side-index. `Pipes.litebox`'s stale
pointer (fixed via `GlobalStateHandle::pipes()`) and `FutexManager`'s stack-allocated `LoanList`
sharing hang (fixed: each process gets its own fresh `FutexManager`) were two more instances of
the same raw-pointer-frozen-into-shared-bytes root pattern. `SafeZoneAllocator::alloc`'s spinlock
livelock (distinct mechanism, no dead-holder recovery unlike `RawMutex`) is still open.

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, the GUI-protocol
decision, five cheap-wins PRD rows, cross-process-fork stdio-handle bug (`spawn_suspended`'s
clobbered `STARTF_USESTDHANDLES`), presenter-process split (`docs/presenter-process-design.md`)
— all CLOSED, none open. Full detail: archive.

## Docs and tooling map

- **Archives** (newest first) — `_2026-09-18.md` (12th-32nd passes, full trace/repro detail for
  everything AGENTS.md's own pass entries above summarize — shared AF_UNIX connection plane;
  DBUS_FAILED/DISPLAY-getenv()/AF_UNIX-errno all CLOSED; Xvfb's real SIGSEGV root-caused to a
  glibc memcpy reading an unmapped pointer, cdb refuted as a viable capture method, exact Xvfb
  call site still open (32nd)), `_2026-09-17.md` (shell-crash investigation, stdio-handle
  bug, 12 registry/pointer/lock fixes, writable-layer-race fix), `_2026-09-16.md` (popup-menu
  re-test, Track A audit, RawMutex/presenter), `_2026-09-15.md` (ACK-stall-kill), `_2026-09-10.md`
  (fork fd eligibility, OCI cache, s6-boot, browser config, crash-dump/VEH, CoW). Older:
  `_2026-09-03.md`, `_2026-09-05.md`.
- Fork: `docs/track-b-fork-fix-progress.md`, `advisor/ADVISORY-002-d-zero-fork.md`,
  `advisor/ADVISORY-001-fundamentals.md` (§3N tcache). `docs/veh-exception-handler-design.md` —
  read before touching VEH.
- Desktop logs: `docs/webtop-debian-selkies-2026-09-06.md`, `webtop-alpine-mate-2026-09-07.md`,
  `webtop-debian-xfce-2026-09-08.md`, `webtop-xfce-code-vs-data-2026-09-08.md`, `fork-fs-veh-2026-09-08.md`.
- Consult before deriving: `docs/premade-library-research.md`, `docs/drm-dumb-buffer-ioctl-reference.md`,
  `docs/diag-timeline-field-semantics.md` (before any `DIAG_TIMELINE` `comm`-field hypothesis).
- `docs/macos.md` — Apple Silicon guest-execution context switch is a stub, deferred. Designs NOT
  implemented: `docs/session-daemon-design.md`, `docs/fork-region-grouping-design.md`.
- `advisor/probes/` — diagnostics (`decode_frame.py`, `symbolize_litebox_crash.py`, `dup_probe.c`,
  `drm_flip_probe.c`, `clone_probe.c`, `socketpair_fork_probe.c`) plus `MEASUREMENT-PITFALLS.md`,
  `DISK-HYGIENE.md`.
- `.gm/memories/` — older per-topic notes, superseded by this file/archives.
