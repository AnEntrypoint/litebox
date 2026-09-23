# litebox — current state (2026-09-23)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, or one a later commit superseded, is deleted rather than hedged. Reference
detail is drained to `docs/AGENTS_ARCHIVE_*.md` and dated `docs/*.md` in the map below — read those
for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not a separate memory file. Compacted at the 65th, 70th, 72nd, 75th, 76th,
81st, 83rd and 85th passes (pass-history section below; 26th-69th full narrative:
`docs/AGENTS_ARCHIVE_2026-09-22.md`; 70th-85th full narrative, including each pass's own complete
evidence and fix rationale: `docs/AGENTS_ARCHIVE_2026-09-23.md`). Re-compacted 85th pass (drained
the 83rd/84th passes' own full bug-by-bug writeups to the archive, ~50KB→~41KB, now that this file
was well past its own 30KB threshold).

## The cheap repro — start here

```
target/release/litebox_runner_linux_on_windows_userland.exe -Z \
  --oci-image docker.io/library/debian:stable-slim -- /bin/bash -c '<script>'
```

One ~81MB layer, `[cache] HIT` after the first pull, real GNU coreutils instead of busybox (coreutils
`touch` issues the `utimensat`/futimens form busybox's never reaches, `caaac79`). Host-side gotchas:

- **PowerShell, never Git Bash** — Git Bash rewrites `/absolute/guest/paths` into `C:/Program
  Files/Git/...` before the runner sees them (misleading `ENOENT`). `Start-Process
  -RedirectStandardOutput/-RedirectStandardError` makes the runner exit almost instantly with zero
  guest output; use `& .\runner.exe ... *> combined.log` instead.
- **Single quotes only inside `-c`** — embedded double quotes are corrupted crossing into the
  child's Win32 command line (masqueraded as deep fork/stack-pointer corruption for a whole session).
- **`LITEBOX_PROCESS_FORK=1` is a HOST env var, not a guest `--env`** —
  `spawn_cross_process_fork_child` (`litebox_platform_windows_userland/src/lib.rs`) reads it via a
  bare `std::env::var_os` on the HOST side; via `--env` it silently no-ops with ZERO log output
  (looks like "not eligible" but isn't even attempted) — 37th pass.
- **Boot logs from PowerShell redirection (`*> file.log`) are UTF-16LE, not UTF-8** — a plain
  `grep`/`Select-String` silently returns zero matches even when the text is there. Always `iconv
  -f UTF-16LE -t UTF-8` (or `Get-Content -Encoding Unicode`) first — 53rd pass.
- **`*> file.log` WORD-WRAPS any tracing line longer than ~116-119 chars across MULTIPLE physical
  lines, no continuation marker** — data isn't lost, only split (repro: `python -c
  "sys.stderr.write('A'*300)" *> f` → 3 physical lines for 1 logical write). A naive line-based
  grep/regex over a long DEBUG line sees only the first ~116-119 chars (70th). **Fix**: a physical
  line NOT starting with the `<float>s` timestamp prefix is a continuation — rejoin before regex.

**Log level**: default is `warn,litebox_platform_windows_userland::fork_verify=error` (`fork_verify`
pinned to `error` since it warns per single-stepped instruction). Don't add `LITEBOX_LOG=error` by
reflex; use `fork_verify=warn` for a fork heal. Bare `LITEBOX_LOG=debug` is useful but too noisy for
a full boot. **Prefer the two dedicated low-overhead targets over any blanket module target**:
`litebox_diag::process_timeline=debug` (five `DIAG_TIMELINE` lines, system-wide, cheap) and
`litebox_diag::socket_read=debug` (read()/recvfrom() payload previews, narrow with
`LITEBOX_DIAG_SOCKET_READ_TARGET=<comm>`, unset = every process, 74th) — both far cheaper than the
old `litebox_shim_linux::syscalls::{process,net,file,unix}=debug` recipe (~70+ call sites/module
flooded across every concurrently-forked process during a boot's fork storm).

## Standing lessons and hard constraints

- **No WSL/hypervisor ever** — run under the matching runner (`litebox_runner_linux_on_windows_
  userland.exe`/`litebox_runner_linux_userland`); cross-compiling FOR Linux is fine, running the
  result in a VM defeats the premise.
- **`fork_verify.rs`'s stale-pointer-healing bug class is Windows-only** (real `fork()` gives
  identical child addresses) — never port it to another platform's crate.
- **Never `bcdedit /debug on`** without a kernel debugger attached — two full-host freezes so far.
- **A process spinning in a dead-locked allocator/spinlock resists `Stop-Process -Force`** — use
  `Invoke-CimMethod -MethodName Terminate` (WMI). `cdb -p <pid>` must use `-pv`/`qd`, never bare `q`
  (kills the target).
- **Never run two full-stack verifications concurrently** — starves both, looks like a real hang.
  Kill every `litebox_runner` between runs; watch `FreePhysicalMemory`, kill on a falling trend.
- **`LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check**, never `PrintWindow`/
  `CopyFromScreen` — decode via `advisor/probes/decode_frame.py`, correlate against `DIAG_TIMELINE
  execve`'s real argv0.
- **Never time litebox with one host process per datapoint** (bare spawn costs 1.6-2.3s) — run N
  iterations in ONE guest process; never subtract timestamps across a parent log and a fork-child
  log (`init_logging()` resets elapsed time to ~0 per child).
- **Release-binary `cdb` reads are unreliable** (MSVC ICF folds distinct functions into one symbol)
  — build `cargo build -p litebox_runner_linux_on_windows_userland` (no `--release`) for any `cdb`
  session needing a trustworthy stack.
- **Refusal errno choice is API contract** — EPERM lets callers degrade, EINVAL/ENOSYS fails them
  hard; wrong choices have silently broken whole subsystems before (30th-pass AF_UNIX
  `EAGAIN`-vs-`EINPROGRESS` fix is the newest instance).
- **Proving a run took the cross-process fork path needs `[process_fork_diag] task-resume-probe`
  lines, never the shim's eligibility log** — the latter fires regardless of outcome (archive).
- **An fd subsystem being "uncarriable" across a cross-process fork doesn't mean the fork must be
  refused** — pipes/regular files/eventfds ARE carried; close-on-exec and pty fds are safely
  DROPPED and the fork proceeds (a pty, unlike a socket, is RE-OPENABLE by id via `SharedPtyTable`);
  only genuinely unrecoverable kinds (unix-socket) refuse. Check `try_cross_process_fork`'s match
  arms (`litebox_shim_linux/src/syscalls/process.rs`) before assuming a new kind needs old
  treatment. Run dbus-daemon non-forking; for XFCE use `xfce4-session`, never `startxfce4`.
- **`wait4()`/`kill()` to a cross-process fork child are asymmetric** — `kill()` to a
  `cross_process_children`-tracked pid returns `ESRCH` unconditionally (documented gap: reachable
  via `wait4`, just not signalable yet).
- **A `socketpair(2)`-originated fd (both ends `Unnamed`) is NOT safe to drop as CLOEXEC across a
  cross-process fork** — real processes (`dbus-daemon`'s babysitter) use it for pre-`exec()`
  bookkeeping; `raw_fd_is_addressless_unix_socket_pair` (`net.rs`) refuses it (falls back to
  thread-based fork) rather than silently dropping it (54th).
- **A `TypedFd`'s index is only valid against the SAME `Descriptors` instance that `insert()`ed
  it** — reading one back against a different process's table is out-of-bounds or resolves to an
  unrelated entry; every accessor in `litebox/src/fd/mod.rs` returns `None` rather than panicking
  (`faa74c6`) — same class as the `Network::queued_for_closure`/`Pipes.litebox`/`FutexManager` bugs.
- **A `de_only.sh`/`LITEBOX_PROCESS_FORK=1` boot's RAM floor is NOT a fixed ~3.1-3.3GB plateau — it
  depends on concurrent HOST load, can fall well below 1GB free** (73rd, 4/4 `de_only_xcensus_
  seed2.tar` boots: collapse to 500MB-1.5GB free within ~10-20s of `xfce4-session`'s fork tree
  starting). `taskkill /IM litebox_runner…exe /F /T` reliably recovers RAM even from <500MB-free;
  watch `FreePhysicalMemory` throughout, kill on a FALLING TREND not a fixed number. **Confirm the
  release binary's mtime postdates the newest relevant commit before trusting a boot result** (57th
  caught a ~1hr-stale binary this way). See Track B item 1.
- **`de_only_xcensus_seed2.tar` (NOT `de_only_seed.tar`) reaches `DE_LAUNCHED_DIRECT` in ~10-15s and
  does NOT hit the 71st-pass `gpg-agent`/`iceauth`/`ssh-agent` dead end** (4/4, 73rd) — preferred
  harness, ~10x faster to `xfwm4`-launch than `webtop_stack.sh`. **`de_only_xcensus_seed3.tar`**
  (75th, disk-only) is the same seed with its `/tmp/xcensus.py` round trip rewritten to feed
  `python3` via stdin instead of a `/tmp` file — seed2's census always failed `rc=2` ENOENT
  (writable-layer-visibility gap on `/tmp`); seed3 returns real data (`rc=0`). Use seed3.
- **A bare file redirect (`cmd > /tmp/f` + a later sibling's read) used to fail silently under
  `LITEBOX_PROCESS_FORK=1`, same root cause the 75th pass fixed (`1d449e6`)** — not yet re-verified
  for a literal `>` specifically, so prefer a PIPE or `$( )` when in doubt: `cmd 2>&1 | sed
  's/^/[tag] /' &` for streaming, `VAR=$(external-cmd)` for captured output (44th-pass fix).
- **`.wfgy/webtop_stack.sh` is NOT what boots — `.wfgy/webtop_seed.tar` embeds a FROZEN COPY**
  (`--resume-from`); editing the host script alone changes nothing. Re-tar after every edit (stage,
  overwrite, `tar -cf webtop_seed.tar webtop_stack.sh tmp config`), verify with `tar -xOf ... |
  grep`. Found the hard way: the 35th pass's `/dev/tcp` rewrite was still absent from the tar on
  the 38th pass — never once ran in a guest.
- **A boot whose log stops is usually a DEAD ROOT RUNNER, not a hang** — when the root process dies,
  `[s]` markers stop while orphaned cross-process children (Xvfb, selkies) keep burning CPU, reading
  like a stall. Check `Get-CimInstance Win32_Process -Filter "Name='litebox_runner…'"`'s
  `CommandLine.Length` — a cross-process CHILD has the bare 77-char exe-only command line
  (`process_fork.rs:1594-1599`); if no survivor carries the full `--oci-image…` args, the root is
  gone (RAM pressure → OOM-kill).
- **Before ANY `cdb` attach, set `LITEBOX_DIAG_NO_EXTERNAL_FAULT_WATCHDOG=1` and
  `LITEBOX_DIAG_NO_FAULT_WATCHDOG=1`** — every runner spawns a watchdog (`process_fork.rs:4391`)
  that `TerminateProcess`es after 15s of <10ms CPU delta, killing a debugger-frozen target.
- **Socket read/write tracing (69th-74th; full mechanism: archive)**: `sys_write`/`sys_writev` log
  under `syscalls::file`, not `net` (`file.rs:1847`/`2990`). A socket fd's `read(2)`/`readv(2)` is a
  SEPARATE path from `recvmsg(2)` — real Xlib/XCB Xtrans uses plain `read()`/`write()` (root cause,
  71st, of the 70th pass's X11-reassembly desync). `run_on_raw_fd` (`lib.rs:1702`) splits socket fds
  into `net` (generic TCP) and `unix` (`UnixSocketSubsystem`, what X11/D-Bus use) — the 71st pass's
  `litebox_diag::socket_read` diagnostic only instrumented `net` (fixed 73rd, `fc830d1`, mirrored
  onto `unix`). Blanket `syscalls::file=debug` is unusable on a real boot (50MB+/s of guest time,
  destabilized a boot enough to break `xdpyinfo`, 71st) — use `litebox_diag::socket_read` instead,
  optionally `LITEBOX_DIAG_SOCKET_READ_TARGET=<comm>[,<comm>...]` (74th; unset = every process).
- **`FlushingStderr` (`litebox_runner_linux_on_windows_userland/src/lib.rs`) does one locked
  `write_all`+`flush` per tracing EVENT, in `Drop`** — closes a real interleaving window across
  concurrent guest (=Windows) threads the prior separate lock/write/lock/flush had. Fixed; NOT the
  70th pass's X11-reassembly desync explanation (that gap was the `read()`/`recvmsg()` split above).
- **All five `DIAG_TIMELINE` sites log at `debug!` on their own `litebox_diag::process_timeline`
  target**, not nested under `syscalls::process`/`syscalls::signal` (~70 unrelated sites each; 74th)
  — use `LITEBOX_LOG=warn,litebox_platform_windows_userland::fork_verify=error,litebox_diag::
  process_timeline=debug` for cheap whole-boot coverage. A cross-process fork child's guest pid IS
  its real Windows PID (`runner…/lib.rs:1673`), so `DIAG_TIMELINE execve`'s `pid=` is `cdb -pv -p`-able.
- **On host-side crashes, use `advisor/probes/symbolize_litebox_crash.py`, snapshotting `.exe`+`.pdb`
  next to the log** — a ring dump's `rva=` is only meaningful against the exact emitting build.
- **Isolate the harness before blaming litebox** — launch guest probes directly as the runner's
  top-level program, never via a runtime-built `/bin/sh -c` wrapper. Never trust a container tag
  name for its WM/session contents — verify by registry manifest + blob tar-listing or a live
  in-guest `/usr/bin` listing. Never record a test count not watched run to completion; never leave
  a suite red for an environmental reason. **The 53rd pass's "`xfce4-session` startup depth varies
  run to run" was itself a RAM-exhaustion artifact (fixed 56th/57th)** — 58th pass's 3/3 clean runs
  all reach the identical depth (`iceauth`+`ssh-agent` spawned, then hangs — Track B item 1).
- **Repo hygiene** — packed layer tars, frame dumps and debug logs never go in git (`.wfgy/`,
  gitignored); untrack anything `git add -A` sweeps.
- **Guest-reachable code returns an errno, never a panic** — the host process IS the entire guest
  session, so an `unimplemented!()`/`unreachable!()`/panic, or unbounded recursion, on any
  guest-reachable path kills every guest process at once (OOM, metadata ops, open flags, nested
  `epoll_ctl`, corrupted guest contexts — full fixed-bug list with shas: archive).

## Cross-process fork (`LITEBOX_PROCESS_FORK=1`)

A genuine `D == 0` fork — child at the SAME addresses, no relocation, no `fork_verify` healing —
exists as `spawn_cross_process_fork_child` (`advisor/ADVISORY-002-d-zero-fork.md`), short-circuiting
to a native fork when available (the fd-carrying apparatus is Windows-only scaffolding for a
missing syscall). **Correctness-sound**: zero corruption on a `bash -c` loop repro vs the
thread-based default's 100% tcache-corruption rate (ADVISORY-001 §3N is thread-path-only).

**Eligibility** — an already-borrowed fd table, a beyond-stdio fd that isn't a pipe end/path-recorded
regular file/eventfd/close-on-exec/pty (overridable by `LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an
unsanitizable `fs_base`/context. No by-name gate exists (34th) — only this global opt-in env var
plus the per-fork fd-kind scan; the only remaining blocking kind on a real `debian-xfce` boot is
`unix-socket`. Fork-child GPR/vmem-adopt cost is small (~1.2s, down from ~3.5-5s); the rootfs
index-merge cost the 56th pass fixed is NOT the dominant per-fork cost any more (confirmed 100%
cache-hit, `LITEBOX_DIAG_FORK_TIMING=1`, 75th: real per-fork rootfs cost ~83-140ms). The real
per-fork-count cost is each fork being a separate Windows process with its own ~350MB-1.1GB peak
working set (guest-memory emulation, writable-layer import, rootfs materialization — not yet
decomposed; see Track B item 1). **`live_cross_process_fork_children`** (`GlobalState` field,
`litebox_shim_linux/src/lib.rs`, 76th) is admission control capping concurrent cross-process-fork
children at 6 (`Task::reserve_cross_process_fork_slot`/`release_cross_process_fork_slot`,
`syscalls/process.rs`, fails open ~8s) — real but only PARTIAL mitigation, see Track B item 1.
Still open: nginx's own SSL-cert generation fails on its first startup attempt, not root-caused
(`docs/track-b-fork-fix-progress.md:146-152`).

**Pass history (4th-82nd, 2026-09-17/23)**: full narrative in the dated archives ("Docs and tooling
map" below). Condensed current-state trail:

- **43rd-74th (FIXED/REFUTED, live-verified; archives: `_2026-09-22.md`/`_2026-09-23.md`)**: both
  Xvfb SIGSEGVs; D-Bus activation's dropped-CLOEXEC-fd bug; `fd/mod.rs:422` panic; per-fork
  rootfs-rebuild RAM cost (56th); `ssh-agent`/`xfwm4` permanent freeze
  (`RawMutex::WaiterQueue::with_lock`, 60th/61st); `SharedUnixConnectQueue::cancel`'s slot leak
  (62nd); `DBUS_FAILED` root-caused+fixed (a byte-size regression guard discarding a healthy fresher
  writable-layer export, 67th/68th). REFUTED: `/defaults/xfce/` readdir, dbus-daemon babysitter
  SIGKILL, epoll-readiness, GLX/compositor blocker theories. 70th-74th: root-caused two
  logging/capture gaps hiding `xfwm4`'s own X11 traffic (`do_read`'s socket branch is separate from
  `do_recvmsg`; the `unix` closure, not just `net`, needed the `socket_read` diagnostic); added
  low-overhead `litebox_diag::process_timeline`/`socket_read` targets; independently reproduced a
  `GetAllProperties` D-Bus call re-issuing every ~10.7s forever (mechanism unconfirmed, still open).
  `DE_FAILED`/RAM collapse (host process count peaking ~30) survived all of it.
- **75th**: **`xfwm4` launches for the first time ever.** Root-caused+FIXED (`1d449e6`)
  `take_cross_process_writable_layer_export` requiring an env var unset on every `--oci-image`
  boot — the parent never re-absorbed ANY cross-process fork child's filesystem writes, on ANY
  `--oci-image` boot, ever; fixed by mirroring the child's own OCI-image fallback onto the parent's
  read side. Confirmed via `DIAG_TIMELINE execve`, X11 window count 0→1→11,
  `_NET_SUPPORTING_WM_CHECK` advancing. Also REFUTED the rootfs-cache-miss theory
  (`LITEBOX_DIAG_FORK_TIMING=1`: both caches 100% hit, real per-fork rootfs cost ~83-140ms). `DE_UP`
  not reached — RAM crater (0.3-1.3GB free) around `WM_POLL n=6`, 11+ windows.
- **76th**: first DIRECT host-side capture of the crater process tree
  (`.wfgy/pass76_crater_procsnapshot.txt`) — 33 simultaneous processes, 4 fork-tree generations
  deep, ~10.5GB combined working set/15.25GB host. Landed `live_cross_process_fork_children`
  (shared-arena `AtomicU32` admission control, caps 6 concurrent cross-process children, fails open
  ~8s) — real but PARTIAL: slower growth, same eventual magnitude (28-29 processes, 0.17-0.31GB
  free). Reframed: not a scheduling problem — a real XFCE session needs >6 long-lived daemons alive
  simultaneously, each paying ~350MB-1.1GB peak working set once (ordinary Windows behavior); fix
  rate-limits, doesn't cap. Real fix needs lower PEAK PER-PROCESS RSS. Confirmed not a leak (WMI
  `Terminate` fully recovers RAM).
- **77th**: root-caused+FIXED (`621ee1a`) a real 2x host-allocator commit bug —
  `WindowsUserland::alloc` doubled every commit instead of using `VirtualAlloc2`'s unused
  `MEM_ADDRESS_REQUIREMENTS::Alignment` field (`cdb -pv` traced it to `clap`'s CLI-parsing `Vec`
  growth, ~108MB committed before the OCI pull even begins). Verified correct, measured: Priv
  committed 123.7→76.6MB / 130.2→84.5MB single-process, ~35-40% reduction per process. Real but NOT
  sufficient alone: full XFCE boot still cratered at the same magnitude (28 processes, 0.82GB free)
  — 28×~77MB≈2.15GB vs. ~7-8GB actually consumed. New candidate found by code reading, not fixed:
  BOTH fork paths (`Vmem::duplicate`, `litebox/src/mm/linux.rs`~1679-1719; `copy_one_group`,
  `litebox_platform_windows_userland/src/process_fork.rs`) unconditionally byte-copy every
  non-shared VMA on every fork, including read-only shared-library code/rodata a real `fork()`
  would share for free. Deliberately not attempted — flagged as needing real measurement first.
- **78th**: measured the 77th pass's shared-library-COW theory (`LITEBOX_DIAG_FORK_VMA_BREAKDOWN=1`,
  permanent, off by default) instead of guessing — read-only file-backed bytes are only ~29% of
  copied bytes per fork (real but MODERATE, not dominant); declined the skip-copy fix as a bad trade
  against ADVISORY-001 §3N's bug history for a moderate payoff. Incidentally reconfirmed `ls | wc -l`
  can still SIGSEGV/SIGABRT via the known tcache-corruption class.
- **79th**: measured fork-then-immediately-`execve()` instead of guessing — with the `GLIBC_TUNABLES`
  workaround, 20/20 real `bash -c` forks are plain `fork()` (`CLONE_VM` absent) with `execve()` as
  the literal first syscall, ~85% of cycle wall time is eager-copy, 100% wasted at `execve`. Declined
  to implement a skip/defer-copy fix: reaching a "first syscall" checkpoint needs the child to
  execute real instructions first, which needs the copy already done (no COW/page-fault primitive
  exists) — a real fix needs a new per-page lazy-population primitive, scoped but not attempted.
  Also reconfirmed ADVISORY-001 §3N live (44% per-fork crash rate WITHOUT the tunable, 0% with it).
- **80th**: confirmed by direct live measurement (not prediction) that tightening
  `CROSS_PROCESS_FORK_CONCURRENCY_CAP` further is a dead end — two fresh boots at 6→3 hit the SAME
  WM_POLL n=4 crater ceiling the unmodified cap=6 binary already hit (77th), just slower with fewer
  procs alive at the crater instant. Confirms the crater is driven by CUMULATIVE committed memory
  across the boot's whole fork history, not peak instantaneous concurrency — CLOSES admission-cap
  tuning as a lever.
- **81st**: investigated the 79th pass's declined idea in a narrower "defer the WHOLE copy batch (not
  per-page) until the child's first non-`execve` syscall" framing, via code reading only (no code
  changed, no live boot — nothing to verify). Confirms 79th's obstacle (the child needs real,
  populated memory to execute even its first instruction, so there is no window to defer into) and
  finds a SECOND, independent one: the checkpoint is a syscall event, not a memory-WRITE event, and a
  plain `fork()`ed child may legally write memory (stack/TLS/malloc bookkeeping/`atfork` handlers)
  with zero syscalls before `execve` — sharing pages until the checkpoint would let such a write
  silently corrupt the PARENT. Litebox's own `CLONE_VFORK` path already does share-until-`execve`
  safely (`do_clone`, `process.rs:3893-3925`), but only because real `vfork()` carries a POSIX UB
  contract forbidding exactly that write, plus the parent is blocked (`wait_for_vfork_done`) —
  neither property holds for plain `fork()`, which is what the real `bash` workload uses (79th).
  Whole-batch deferral removes the per-page performance argument only, not the correctness one — the
  only remaining viable path is genuine per-page lazy population (79th's own scoped primitive), no
  shortcut exists at either granularity. Re-confirms 79th's decision not to implement.
- **82nd** — both remaining "quick, safe" Track B levers (skip-copying-zero-bytes; trimming optional
  XFCE session-autostart components) tested and confirmed real but genuinely exhausted — neither
  reduces Windows' own `VirtualAlloc2(MEM_COMMIT)` charge, which is what the crater is measured in.
  Net effect: narrowed the whole investigation down to one remaining candidate, genuine per-page
  lazy population. Full Angle A/B evidence, exact XFCE client list, exact before/after process
  counts: archive.
- **83rd** — IMPLEMENTED the lazy (reserve-then-commit-on-first-fault) primitive
  (`litebox_platform_windows_userland/src/lazy_fork_commit.rs`, `LITEBOX_LAZY_FORK_COMMIT=1`,
  default OFF) the 79th/81st/82nd passes converged on. Real, measured win for the dominant
  fork-then-`execve` case (debug: 103ms eager vs 34ms mixed; both builds 5/5 clean). Found a
  genuine, 100%-reproducible crash for fork-WITHOUT-`execve` (a bash `(...)` subshell) — root cause
  not found this pass. Full design, ordering-bug fix, isolation-POC numbers: archive.
- **84th** — root-caused the 83rd pass's crash to TWO real, general cross-process-fork bugs, both
  FIXED and live-verified (neither is lazy-commit-specific): (1) a forked child's `sys_mmap`
  allocator had no record of `copy_one_group`'s own 64KiB alignment padding, letting a fresh mmap
  silently collide with it (`litebox/src/mm/linux.rs`'s `Vmem::new_adopting_existing_memory`, now
  pre-inserts a `VM_OWN_FORK_PADDING` placeholder per group span); (2) a cross-process-fork child's
  `SignalState` never inherited the parent's `sigreturn_trampoline` address (unlike the thread-based
  path), so `LinuxShimEntrypoints::exception`'s trampoline recognition could never match
  (`adopt_forked_process` now takes it via a new env var). With both landed, the subshell repro runs
  clean 5/5 WITHOUT the lazy flag — but STILL crashed 5/5 WITH it (Bug 3, re-scoped, not fixed this
  pass): the fault reached `vectored_exception_handler` and got redirected toward
  `exception_callback`, but `LinuxShimEntrypoints::exception()` was never observably entered.
  **Methodological finding**: gated, low-volume diagnostics added directly inside
  `vectored_exception_handler` measurably regressed the (otherwise-fixed) non-lazy case too — any
  future instrumentation there needs an A/B test against the non-lazy repro before being trusted.
  Full bug mechanics, exact diffs: archive.
- **85th** — root-caused and FIXED Bug 3 (no debugger needed — exact address arithmetic against a
  `LITEBOX_DIAG_LAZY_FORK_COMMIT=1`/`LITEBOX_DIAG_FATALDUMP=1` capture was sufficient): the child's
  own live `%rsp` at fork time landed INSIDE the same lazily-reserved (never-yet-committed) stack
  group `classify_lazy_eligible_groups` was already making lazy — confirmed via ZERO
  `[lazy_fork_commit] fault #N serviced` lines ever printing before the crash. Windows delivers
  EVERY exception with `CONTEXT.Rsp` set to whatever the CPU held at fault time, not only ones whose
  own fault address is in a lazy range; a fresh child's very first exception is guaranteed to hit
  this. Fix: `classify_lazy_eligible_groups` (`lazy_fork_commit.rs`) now takes the child's fork-time
  `%rsp` (threaded from `spawn_process_fork_child`'s `full_gprs.rsp`) and excludes whichever group
  contains it, leaving every OTHER pure-data group lazy. **Verified**: the subshell repro's ORIGINAL
  crash signature is gone, 5/5, both debug and release; fork-then-`execve` stays clean 5/5, no
  regression. **Bug 4 (OPEN, found while re-verifying the fix)**: the SAME subshell repro now runs
  further (lazy faults ARE serviced, with real, non-zero-filled parent data) but still fails 5/5,
  both builds, with a NEW signature: `Fatal glibc error: malloc.c:2601 (sysmalloc): assertion
  failed` — real heap corruption. Root cause: `lazy_commit_veh` reads the PARENT's LIVE memory
  (`ReadProcessMemory`) at WHATEVER MOMENT the child happens to touch a page, which for a
  fork-without-`execve` child can be long after `fork()` returned control to the parent's own guest
  thread — a genuine TOCTOU gap the eager `copy_one_group` path (synchronous, while the parent is
  still blocked inside its own `fork()` syscall) never had. The parent's continued heap
  mutation (malloc/free) after `fork()` returns can be read mid-mutation by a later lazy fault,
  corrupting the child's heap bookkeeping — exactly the observed assertion shape. No fix attempted
  this pass; needs either a fork-time snapshot into a buffer (not a live parent read) or genuine
  OS-level COW between the two Windows processes — real design work, not a quick patch. Full
  mechanics, exact repro logs, both candidate fix directions: archive and
  `lazy_fork_commit.rs`'s own module doc comment. `LITEBOX_LAZY_FORK_COMMIT` stays default OFF —
  Bug 4 means it is STILL not safe for a real boot (a real desktop forks many long-lived daemons
  that don't `execve()` and run concurrently with a parent still mutating its own heap — exactly
  Bug 4's trigger shape). `DE_UP` not attempted this pass.

Fully DONE (kept only as a marker so a future pass doesn't re-attempt): the minimal isolated
cross-process AF_UNIX repro; the `Network` shared-arena redesign's `socket_set`/
`LocalPortAllocator`/`closing_in_background`/`queued_for_closure` slice; DISPLAY/`getenv()` as the
`DE_FAILED` cause; AF_UNIX `connect()` `EAGAIN`-vs-`EINPROGRESS`; `pty_registry`/
`daemon_pty_masters` (`syscalls::pty::SharedPtyTable`, live-verified cross-process); fork's
fd-eligibility scan dropping a redirected 0/1/2 (`raw_fd_is_plain_stdio_device`);
`SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap (`UnixStreamState::Connecting`);
both Xvfb SIGSEGVs.

**Open, in rough priority order:**

1. **`xfwm4` now launches (75th pass, `1d449e6`) — the blocker is no longer filesystem visibility,
   it is pure host-RAM/process-count exhaustion before `DE_UP`.** CLOSED sub-issues:
   `ssh-agent`/`xfwm4` freeze (60th/61st); `DBUS_FAILED`'s regression-guard cause (67th/68th); the
   writable-layer export-path fallback bug (75th). Admission-control (76th) and the fixed
   per-process alloc floor (77th, ~35-40% reduction) are landed, real, partial mitigations.
   Session-autostart trimming and zero-byte-skip (82nd) are closed as dead-end levers — neither
   touches Windows' own `VirtualAlloc2(MEM_COMMIT)` charge. **83rd-85th passes: genuine per-page
   lazy fork-memory population (`litebox_platform_windows_userland::lazy_fork_commit`,
   `LITEBOX_LAZY_FORK_COMMIT=1`) implemented, a real measured win for fork-then-`execve` (the
   dominant real case), and THREE real correctness bugs found+fixed along the way (a guest-mmap/
   64KiB-padding collision; forked children not inheriting the parent's `sigreturn_trampoline`;
   the lazy classifier making the child's own live stack group lazy, breaking exception delivery
   before its first fault could ever be serviced) — but a FOURTH, structural bug (Bug 4, 85th pass)
   remains OPEN: `lazy_commit_veh` reads the parent's memory LIVE, at whatever moment the child
   touches a page, which races the parent's own continued heap mutation for a fork-WITHOUT-`execve`
   child — confirmed live as a deterministic `glibc malloc.c:2601` heap-corruption SIGABRT, 5/5,
   both builds.** `LITEBOX_LAZY_FORK_COMMIT` stays default OFF — Bug 4 makes it genuinely unsafe for
   any real boot (a real desktop forks many long-lived daemons that don't `execve()` and run
   concurrently with a parent still mutating its own heap, exactly Bug 4's trigger shape). Full
   mechanics of all four bugs: archive and `lazy_fork_commit.rs`'s own module doc comment. **Next
   pickup, precise**: fix Bug 4 (needs either a fork-time snapshot of lazy-eligible bytes into a
   buffer instead of a live parent read, or genuine OS-level COW between the two Windows processes —
   real design work, not a quick patch), then re-verify 5/5 clean on BOTH repros, both builds, before
   ever attempting the full `DE_UP` boot with the flag on. `LITEBOX_DIAG_FORK_VMA_BREAKDOWN=1` (both
   call sites, zero cost when off) remains the permanent tool for measuring any future fix's real
   payoff before landing it. `DE_UP` has not been reached by any pass through the 85th;
   chrome-devtools MCP has been `CONNECT_TIMEOUT` every time it was checked (moot until `DE_UP`
   fires). Lower-priority, still open: (a) decompose
   remaining per-fork cost between rootfs materialization staying resident post its cheap (~83-140ms)
   build vs. Windows loader overhead; (b) once `DE_UP` fires (or stalls again), use
   `de_only_xcensus_seed3.tar`'s working `/tmp/xcensus.py` (`XCENSUS_SELECTION`/`XCENSUS_ROOTPROP`,
   real values not `xprop` heuristic text) + `LITEBOX_DIAG_SOCKET_READ_TARGET=xfwm4` to check whether
   the ~10.7s `GetAllProperties` retrigger (69th/70th, still unconfirmed) recurs, checking
   `MappingNotify`(34)/XKB at that boundary before the 30th-pass `LD_PRELOAD getenv_probe.so`
   technique (`ps`/`/proc` is blind to cross-process-forked siblings, 65th; `gpg-agent`'s fatal
   `malloc.c:3846` assertion, 52nd, is why the OLD `de_only_seed.tar` dead-ends earlier than
   `_xcensus_seed2/3`); (c) unconfirmed: `LITEBOX_LOG` may not reach forked children's own stderr
   (`process_fork.rs`'s env-block construction possibly drops it) — if so, every prior
   diagnostic-logging conclusion past the FIRST fork generation needs re-weighing; see `_2026-09-23.md`.
2. `SharedUnixConnectQueue`'s cancel-on-claim-race slot leak — FIXED 62nd (`unix.rs`); didn't
   resolve item 1's symptom. Other AF_UNIX exhaustion paths still silent (38th, `unix.rs`):
   `SharedUnixAddrPresenceTable` capacity-256 overflow; a key >108 bytes; backlog ignored on
   cross-process accept. Abstract sockets CORRECT.
3. `SafeZoneAllocator`'s `spin::mutex::SpinMutex` still has no dead-holder recovery — lower-urgency
   theoretical risk (the live `ssh-agent`/`xfwm4` freeze once blamed on it was actually `RawMutex`'s
   `WaiterQueue::with_lock`, CLOSED 60th/61st), not tied to any live symptom now.
4. Debugger-root-cause `litebox/src/event/wait.rs:224`'s `unreachable!()` on garbage thread state
   (dozens/boot, most frequent historical panic, NOT yet debugger-confirmed — don't patch blind).
5. `flock_registry`/`drm`/`evdev` (`GlobalState` fields) remain open, same non-POD-payload obstacle
   `SharedPtyTable` is a template for; `timerfd`/`signalfd` are the next-cheapest carriable fd kinds
   before `socket`/`unix-socket`/`epoll`; the writable-layer-visibility gap for LARGE content
   (`/tmp/de.log` etc.) needs its own chunked-publish design, not a widened `SharedFilePublishTable`
   cap. All three lower-urgency, not on the Xvfb/selkies boot path.

## Container images and OCI loading

**`litebox_packager --oci-image <ref> --output <tar>`** pulls, whiteout-merges, rewrites every ELF and
produces a bootable flat tar in one command — supersedes the ad-hoc OCI-pull Python scripts this
project once hand-rolled, retired, do not recreate.

**Runtime in-memory loading** — `--oci-image <ref>` pulls, merges and rewrites every layer in memory;
no host directory is created for the rootfs (a real one hit three Windows-path bugs). Rewritten
layers cache under `.litebox-cache/`, keyed so a rewriter change self-invalidates.
`tar_ro.rs`'s multi-layer index is built ONCE at mount, not per read (was O(entries²), 17.3s →
0.35s fixed) — and, as of the 56th pass, ONCE per boot tree rather than once per fork child too
(`TarRo::live_entries_after_merge`/`from_merged_live_entries`, "Cross-process fork" section below).
Cache internals, the four fixed OOM bugs, tag-verification detail: archive.

Tags verified live, never from the name: `linuxserver/webtop:alpine-mate` ships MATE not XFCE;
`alpine-xfce` doesn't exist; `debian-xfce`/`ubuntu-xfce` ship real XFCE.

**X server choice**: for on-screen DRM/wgpu (`--gui`) use `Xorg` with `modesetting` — litebox's
virtual DRM is legacy-KMS + dumb-buffer + XRGB8888 only (no atomic modeset/GBM/EGL), so a
GBM-first compositor lands on its least-tested fallback and `Xvfb` never touches DRM/KMS at all.
For browser/selkies, `Xvfb` IS correct — its `-shmem` framebuffer works now SysV shm exists.

## A real desktop renders in a browser

**XFCE renders in a real host browser, and MATE too** — full pipeline (Xvfb, selkies/pixelflux
x264, MIT-SHM) inside litebox, reverse proxy host-side only. Working config: selkies
`--addr=0.0.0.0` port **8081**, dashboard over `--publish`, `/websockets` tunnelled to 8081.
Fourteen litebox defects got here, all landed (archive).

**A stock s6-overlay image boots with no flags/stubs**: `/init` runs 16 cross-process children with
zero uncarriable fds. The once-deterministic black XFCE desktop is fixed (runtime rewriter was
corrupting `libLLVM.so.19.1`'s `.dynsym`, mesa `dlopen` failed forever). Rest: archive.

**XFCE also renders on the THREAD-based fork path, gated by one flag** (`docker.io/linuxserver/
webtop:debian-xfce`, `.wfgy/webtop_stack.sh`). Without it, 3/3 boots die ~7s in to ADVISORY-001
§3N's safe-linked-tcache write. Fix: `--env GLIBC_TUNABLES=glibc.malloc.tcache_count=
0:glibc.malloc.mxfast=0` as a GUEST-side `--env` flag (workaround, THREAD-path only).
`LITEBOX_PROCESS_FORK=1` removes that crash class by construction and no longer hits the old
"Fork-after-Xorg" freeze either (35th). A `de_only.sh` boot runs its whole 60s+160s window with
ZERO crash/OOM as of the 57th pass. `DE_FAILED` still fires — NOT "Cannot open display" (refuted,
52nd), NOT RAM exhaustion (57th) — see Track B item 1 for current blockers. Selkies also needs
`--clipboard-enabled=false` on the thread-based path (its clipboard monitor re-triggers the same
corruption every tick) — moot cross-process.

**Open here.** One client per selkies instance, no slot reclaim on reload. A second, distinct
glibc/tcache corruption signature (`double free or corruption (out)` SIGABRT) still sporadically
hits selkies on the THREAD-based fork path under heavy fork load — Track B territory; don't
re-attempt `GLIBC_TUNABLES` without evidence of a third mechanism.

**ACK-stall-kill and port-8081 watchdog — both CLOSED (2026-09-16)**: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

## Host-side crash machinery

A fatal host fault dumps before it dies, ungated (stack walk, `RECENT_FAULTS` ring, `RECOVERY_LOG`,
no env var needed); a real OS minidump comes only from the repeated-identical-fault circuit
breaker. An unexplained `0xC0000005` may be a panic — the VEH handler enters only for the four
codes it triages, registers FIRST in the chain (`docs/veh-exception-handler-design.md`).
Cross-process sync on Windows: every native address/TID-based wait is process-local
(`WaitOnAddress`, keyed events, `NtAlertThreadByThreadId`=ACCESS_DENIED); only a shared kernel
object crosses processes — `RawMutex` (below) is the one that matters; `xproc_sync.rs`'s
named-event primitive is live-verified but still unwired.

## Shared-memory foundations -- all DONE, live-verified 2026-09-16/17/22 (full mechanism: archive)

`RawMutex` no longer calls `WaitOnAddress`/`WakeByAddressSingle` (process-local per MSDN) -- a
manual wait queue + cross-process kernel `Event`s, with `poisoned: AtomicBool` owner-death
recovery. A small 64 MiB `shared_kernel_arena_alloc` backs `SharedArc<T>` for
`LiteBoxX`/`GlobalState` placement (NOT wired to `GlobalAlloc`; `SLAB_ALLOC` stays
private-per-process). **Root cause of the whole `GlobalState`-sharing class**: `SharedArc::new`
shares only `T`'s literal inline bytes -- a `BTreeMap`/similar registry has its NODES on the
private per-process heap, meaningless to an attaching process. Of the original uncarriable-registry
list (`unix_addr_table`/`pty_registry`/`daemon_pty_masters`/`flock_registry`/`fifo_registry`/
`sysv_shm`/`memfds`/`shared_files`): all but `flock_registry` are fixed (per-process-shadowed, a
shared-arena fixed array, or — for `pty_registry`/`daemon_pty_masters` — both a shadow AND a
live-verified cross-process companion, `syscalls::pty::SharedPtyTable`). `sysv_shm` moved to
per-process named-object mapping (51st). Reusable pattern (`SharedUnixAddrPresenceTable`, reused
by AF_UNIX/`SharedPtyTable`): fixed-slot, pure-atomic, lock-free `(kind, key bytes<=108, owner
pid)` side-index. **A mutable-state table on this pattern needs every WRITE path audited for
shared-side mirroring** — `SharedPtyTable`'s own setters originally only reached the local side
(37th-pass live catch). Still open: `flock_registry` (pty's pattern is a template);
`SafeZoneAllocator::alloc`'s spinlock livelock (no dead-holder recovery unlike `RawMutex`).

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, the GUI-protocol
decision, five cheap-wins PRD rows, cross-process-fork stdio-handle bug (`spawn_suspended`'s
clobbered `STARTF_USESTDHANDLES`), presenter-process split (`docs/presenter-process-design.md`)
— all CLOSED, none open. Full detail: archive.

## Docs and tooling map

- **Archives** (newest first) — `_2026-09-23.md` (70th-85th passes, full narrative: the `xfwm4`
  writable-layer-export fix, the live-captured RAM-crater process tree, the 76th-pass
  admission-control fix's honest partial-success evidence, and the full 83rd-85th lazy-fork-commit
  bug-by-bug writeup — all four bugs, exact repro logs, both candidate real fixes for the still-open
  Bug 4), `_2026-09-22.md` (26th-69th passes, full narrative behind every pass-history entry above
  through the 69th), `_2026-09-18.md` (12th-34th), `_2026-09-17.md` (shell-crash, stdio-handle bug),
  `_2026-09-16.md` (Track A audit), `_2026-09-15.md` (ACK-stall-kill), `_2026-09-10.md` (fork fd
  eligibility, OCI cache, s6-boot). Older: `_2026-09-03/05.md`.
- Fork: `docs/track-b-fork-fix-progress.md`, `advisor/ADVISORY-002-d-zero-fork.md`,
  `advisor/ADVISORY-001-fundamentals.md` (§3N tcache). `docs/veh-exception-handler-design.md` —
  read before touching VEH.
- Desktop logs: `docs/webtop-debian-{selkies,xfce}-2026-09-0{6,8}.md`,
  `webtop-xfce-code-vs-data-2026-09-08.md`, `fork-fs-veh-2026-09-08.md`.
- Consult before deriving: `docs/premade-library-research.md`, `docs/drm-dumb-buffer-ioctl-reference.md`,
  `docs/diag-timeline-field-semantics.md` (before any `DIAG_TIMELINE` `comm`-field hypothesis).
- `docs/macos.md` — Apple Silicon guest-execution stub, deferred. NOT implemented:
  `docs/session-daemon-design.md`, `docs/fork-region-grouping-design.md`.
- `advisor/probes/` — diagnostics (`decode_frame.py`, `symbolize_litebox_crash.py`, `dup_probe.c`,
  `drm_flip_probe.c`, `clone_probe.c`, `socketpair_fork_probe.c`, `pty_fork_probe.c`) plus
  `MEASUREMENT-PITFALLS.md`, `DISK-HYGIENE.md`.
- `.gm/memories/` — older per-topic notes, superseded by this file/archives.
