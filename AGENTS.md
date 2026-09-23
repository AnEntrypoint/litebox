# litebox — current state (2026-09-23)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, or one a later commit superseded, is deleted rather than hedged. Reference
detail is drained to `docs/AGENTS_ARCHIVE_*.md` and dated `docs/*.md` in the map below — read those
for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not a separate memory file. Compacted at the 65th, 70th, 72nd and 75th passes
(pass-history section below; 26th-69th full narrative, including each pass's own complete evidence
and fix rationale: `docs/AGENTS_ARCHIVE_2026-09-22.md`; 70th-74th narrative is condensed in-line
below only, not yet migrated to that archive).

## The cheap repro — start here

```
target/release/litebox_runner_linux_on_windows_userland.exe -Z \
  --oci-image docker.io/library/debian:stable-slim -- /bin/bash -c '<script>'
```

One ~81MB layer, `[cache] HIT` after the first pull, real GNU coreutils instead of busybox (coreutils
`touch` issues the `utimensat`/futimens form busybox's never reaches, `caaac79`). Host-side gotchas:

- **PowerShell, never Git Bash** — Git Bash rewrites `/absolute/guest/paths` into
  `C:/Program Files/Git/...` before the runner sees them, giving a misleading `ENOENT`. `Start-Process
  -RedirectStandardOutput/-RedirectStandardError` makes the runner exit almost instantly with zero guest
  output (no crash dump, no event-log entry); use `& .\runner.exe ... *> combined.log` instead.
- **Single quotes only inside `-c`** — embedded double quotes are corrupted crossing into the
  child's Win32 command line (masqueraded as deep fork/stack-pointer corruption for a whole session).
- **`LITEBOX_PROCESS_FORK=1` is a HOST env var, not a guest `--env`** — `spawn_cross_process_fork_child`
  (`litebox_platform_windows_userland/src/lib.rs`) reads it via a bare `std::env::var_os` on the HOST
  side; setting it via `--env` instead silently no-ops the whole cross-process path with ZERO log
  output (looks identical to "not eligible", but isn't even attempted) — confirmed live, 37th pass.
- **Boot logs launched via PowerShell redirection (`*> file.log`) are UTF-16LE, not UTF-8** — a plain
  `grep`/`Select-String` against them silently returns zero matches even when the text is really
  there. Always `iconv -f UTF-16LE -t UTF-8` (or PowerShell's own `Get-Content -Encoding Unicode`)
  first — confirmed live, 53rd pass, on `.wfgy/webtop_release_boot6.log`.
- **`*> file.log` WORD-WRAPS any single tracing line longer than ~116-119 chars across MULTIPLE
  physical lines, with no continuation marker** — PowerShell wraps a captured native-process
  stderr line at its host/console buffer width; data is not lost, only split (confirmed by direct
  repro: `python -c "sys.stderr.write('A'*300)" *> f` produces 3 physical lines for 1 logical
  write). A naive line-based `grep`/regex over any long DEBUG line (hex payload previews
  especially) silently sees only the first ~116-119 chars (70th pass). **Fix**: rejoin first — a
  physical line NOT starting with the `<float>s` timestamp prefix is a continuation of the
  previous logical line, concatenate it back on before applying any other regex.

**Log level**: default is `warn,litebox_platform_windows_userland::fork_verify=error` (`fork_verify`
pinned to `error` since it warns per single-stepped instruction). Do **not** add `LITEBOX_LOG=error`
by reflex; use `fork_verify=warn` when a fork heal is the subject. A bare `LITEBOX_LOG=debug` rules
out "which module's silent early-return ate my decision" fast but is too noisy for a full boot.
**For a process-tree/socket-payload investigation specifically (the common case), prefer the two
dedicated low-overhead targets over any blanket module target**: `litebox_diag::process_timeline=
debug` (five `DIAG_TIMELINE` lines, system-wide, cheap — see Standing lessons) and
`litebox_diag::socket_read=debug` (read()/recvfrom() payload previews; optionally narrow further
with the `LITEBOX_DIAG_SOCKET_READ_TARGET=<comm>` env var, unset = every process, 74th pass) —
both strictly cheaper than the old `litebox_shim_linux::syscalls::{process,net,file,unix}=debug`
recipe, which floods ~70+ unrelated call sites per module across every concurrently-forked process
during a desktop boot's fork storm.

## Standing lessons and hard constraints

- **No WSL or hypervisor, ever** — always run under the matching runner
  (`litebox_runner_linux_on_windows_userland.exe`/`litebox_runner_linux_userland`); cross-compiling
  FOR Linux is fine, running the result in a VM defeats the premise.
- **`fork_verify.rs`'s stale-pointer-healing bug class is Windows-only** (real `fork()` gives
  identical child addresses) — never port to another platform's crate.
- **Never `bcdedit /debug on`** without a kernel debugger attached — two full-host freezes so far.
- **A process spinning inside a dead-locked allocator/spinlock resists `Stop-Process -Force`** —
  use `Invoke-CimMethod -MethodName Terminate` (WMI) instead. `cdb -p <pid>` must use `-pv`/`qd`,
  never a bare `q` (kills the target).
- **Never run two full-stack verifications concurrently** — starves both, looks exactly like a real
  hang. Kill every `litebox_runner` between runs; watch `FreePhysicalMemory`, kill on a falling trend.
- **`LITEBOX_DUMP_FRAMES=1` is the only trustworthy `--gui` visual check**, never
  `PrintWindow`/`CopyFromScreen` — decode frame structure (`advisor/probes/decode_frame.py`) and
  correlate against `DIAG_TIMELINE execve`'s real argv0.
- **Never time litebox with one host process per datapoint** (bare spawn costs 1.6-2.3s) — run N
  iterations inside ONE guest process; never subtract timestamps across a parent log and a
  fork-child log (`init_logging()` resets elapsed time to ~0 per child).
- **Release-binary `cdb` reads are unreliable** — MSVC linker ICF folds distinct functions into one
  symbol. Build `cargo build -p litebox_runner_linux_on_windows_userland` (no `--release`) for any
  `cdb` session needing a trustworthy stack.
- **Refusal errno choice is API contract** — EPERM lets callers degrade, EINVAL/ENOSYS fails them
  hard; wrong choices have silently broken whole subsystems before (30th pass's AF_UNIX
  `EAGAIN`-vs-`EINPROGRESS` fix is the newest instance).
- **Proving a run took the cross-process fork path needs `[process_fork_diag] task-resume-probe`
  lines, never the shim's eligibility log** — the latter fires regardless of outcome (three false
  conclusions so far, archive).
- **An fd subsystem being "uncarriable" across a cross-process fork does not mean the fork must be
  refused over it** — only that fd can't be carried. Pipes/regular files/eventfds ARE carried;
  close-on-exec and pty fds are safely DROPPED and the fork proceeds (a pty, unlike a socket, is
  RE-OPENABLE by id via `SharedPtyTable`); only genuinely un-recoverable kinds (unix-socket) still
  refuse. Check `try_cross_process_fork`'s match arms (`litebox_shim_linux/src/syscalls/process.rs`)
  before assuming a new kind needs old treatment. Run dbus-daemon non-forking; for XFCE use
  `xfce4-session`, never `startxfce4`.
- **`wait4()`/`kill()` to a cross-process fork child are asymmetric** — `kill()` to a
  `cross_process_children`-tracked pid returns `ESRCH` unconditionally (documented gap, not a bug:
  reachable via `wait4`, just not signalable yet).
- **A `socketpair(2)`-originated fd (both ends `Unnamed`) is NOT safe to drop as CLOEXEC across a
  cross-process fork** — unlike a named-peer CLOEXEC client socket, real processes (`dbus-daemon`'s
  babysitter) use it for pre-`exec()` bookkeeping; `raw_fd_is_addressless_unix_socket_pair`
  (`net.rs`) now refuses (falls back to thread-based fork) rather than silently dropping it (54th).
- **A `TypedFd`'s index is only valid against the SAME `Descriptors` instance that `insert()`ed
  it** — reading one back against a DIFFERENT process's table is out-of-bounds or resolves to an
  unrelated entry; every accessor in `litebox/src/fd/mod.rs` returns `None` rather than panicking
  (`faa74c6`) — the same class as the `Network::queued_for_closure`/`Pipes.litebox`/`FutexManager`
  bugs before it.
- **A `de_only.sh`/`LITEBOX_PROCESS_FORK=1` boot's RAM floor is NOT a fixed ~3.1-3.3GB plateau —
  it depends on concurrent HOST load and can fall well below 1GB free** (73rd pass, 4/4 independent
  `de_only_xcensus_seed2.tar` boots: consistent collapse to 500MB-1.5GB free within ~10-20s of
  `xfce4-session`'s own fork tree starting, on a host where unrelated processes — other Claude Code
  sessions, browser, etc. — independently held ~9 of 15GB total RAM). `taskkill /IM
  litebox_runner…exe /F /T` reliably recovers full RAM even from a <500MB-free state; still check
  `FreePhysicalMemory`/`Get-Counter '\Memory\Available MBytes'` throughout and kill on a FALLING
  TREND, not a fixed number. **Confirm the release binary's mtime postdates the newest relevant
  commit before trusting a boot result** (57th pass caught a ~1hr-stale binary this way). See Track
  B item 1.
- **`de_only_xcensus_seed2.tar` (NOT the plain `de_only_seed.tar`) reaches `DE_LAUNCHED_DIRECT` in
  ~10-15s real time and does NOT hit the 71st-pass `gpg-agent`/`iceauth`/`ssh-agent` dead end** —
  confirmed 4/4 clean runs, 73rd pass; still the preferred harness, ~10x faster to `xfwm4`-launch
  than `webtop_stack.sh`. **`de_only_xcensus_seed3.tar`** (75th pass, disk-only, not checked in) is
  the SAME seed with its baked-in `/tmp/xcensus.py` round trip rewritten to feed the census script
  to `python3` via a shell variable + stdin instead of a `/tmp` file — the old seed2's census
  always failed `rc=2` ENOENT (writable-layer-visibility gap on `/tmp`, a different instance of the
  same class the 75th pass's own fix addressed for ordinary files); seed3's census returns real
  data (`rc=0`). Use seed3 for any future census-dependent capture.
- **A bare file redirect (`cmd > /tmp/f` + a later sibling's read) used to fail silently under
  `LITEBOX_PROCESS_FORK=1` for the SAME root cause the 75th pass fixed (`1d449e6`) — a fork
  child's writable-layer export was never re-imported by the parent on any `--oci-image` boot, so
  no later sibling ever saw it.** Not yet re-verified for a literal `>` redirect specifically (only
  `mkdir`+`cp`+`ls`/`cat` was), so still prefer a PIPE or `$( )` when in doubt: `cmd 2>&1 | sed
  's/^/[tag] /' &` for streaming output, `VAR=$(external-cmd)` for captured output (44th-pass
  fd-carry fix). Full mechanism: archive + 75th-pass entry above.
- **`.wfgy/webtop_stack.sh` is NOT what boots — `.wfgy/webtop_seed.tar` embeds a FROZEN COPY**
  (`--resume-from`), so editing the host script alone changes nothing. Re-tar after every edit
  (`tar -xf` to a stage dir, overwrite, `tar -cf webtop_seed.tar webtop_stack.sh tmp config`) and
  verify with `tar -xOf ... | grep`. Found the hard way: the 35th pass's `/dev/tcp` rewrite was
  still absent from the tar on the 38th pass — never once ran in a guest.
- **A boot whose log stops is usually a DEAD ROOT RUNNER, not a hang** — when the root process
  (hosting the top-level shell) dies, `[s]` markers stop while orphaned cross-process children
  (Xvfb, selkies) keep burning CPU, reading exactly like a stall. Diagnose via
  `Get-CimInstance Win32_Process -Filter "Name='litebox_runner…'"` and check `CommandLine.Length`
  — a cross-process CHILD has the bare 77-char exe-only command line
  (`process_fork.rs:1594-1599`); if NO survivor carries the full `--oci-image …` args, the root is
  gone (RAM pressure → OOM-kill).
- **Before ANY `cdb` attach, set `LITEBOX_DIAG_NO_EXTERNAL_FAULT_WATCHDOG=1` and
  `LITEBOX_DIAG_NO_FAULT_WATCHDOG=1`** — every runner spawns a watchdog child (`process_fork.rs:4391`)
  that `TerminateProcess`es after 15s of <10ms CPU delta, killing a debugger-frozen (zero-progress)
  target; kill the already-running target's own watchdog first if attaching mid-boot.
- **Socket read/write tracing, condensed (69th-74th passes; full mechanism: archive)**: `sys_write`/
  `sys_writev` log under `syscalls::file`, not `net` (`file.rs:1847`/`2990`). A socket fd's
  `read(2)`/`readv(2)` is a SEPARATE code path from `recvmsg(2)` — `do_read`'s socket branch calls
  `GlobalState::receive` directly, real Xlib/XCB Xtrans uses plain `read()`/`write()` — root-caused
  (71st) as the true cause of the 70th pass's X11-reassembly desync. `run_on_raw_fd`'s dispatch
  (`lib.rs:1702`) splits socket fds into TWO closures, `net` (generic TCP) and `unix`
  (`UnixSocketSubsystem`, what X11/D-Bus actually use) — the 71st pass's `litebox_diag::socket_read`
  diagnostic only instrumented `net` (fixed 73rd, `fc830d1`, mirrored onto `unix`). `sys_readv`/
  `sys_pread64`/`sys_preadv` all delegate to `sys_read`, no separate instrumentation needed. Blanket
  `syscalls::file=debug` is unusable on a real boot (50MB+/s of guest time, destabilized a boot
  badly enough to break a normally-reliable `xdpyinfo` probe, 71st) — use the dedicated
  `litebox_diag::socket_read` target instead, optionally with `LITEBOX_DIAG_SOCKET_READ_TARGET=
  <comm>[,<comm>...]` (74th pass; unset = every process).
- **`FlushingStderr` (`litebox_runner_linux_on_windows_userland/src/lib.rs`) buffers one whole
  tracing EVENT and does exactly one locked `write_all`+`flush`, in `Drop`** — the prior version's
  separate lock/write/lock/flush left a real interleaving window across concurrent guest threads
  (= Windows threads in this one host process). Matches the guest-visible `STDOUT_WRITE_LOCK`/
  `STDERR_WRITE_LOCK` discipline. Fixed; NOT the 70th pass's X11-reassembly desync explanation
  (reproduced byte-identical post-fix — that gap was the `read()`/`recvmsg()` split above).
- **All five `DIAG_TIMELINE` sites (`exit`/`exit_group`/`clone`/`execve` in `syscalls/process.rs`,
  `exit_signal` in `syscalls/signal/mod.rs`) now log at `debug!` on their OWN dedicated
  `litebox_diag::process_timeline` target, NOT nested under `syscalls::process`/`syscalls::signal`
  (74th pass)** — those two modules' own implicit debug!/trace! targets carry ~70 unrelated call
  sites each, so the historical `litebox_shim_linux::syscalls::process=debug` recipe (still below,
  do not use it just for this) floods every syscall of every concurrently-forked desktop-boot
  process to see five cheap lines. Use `LITEBOX_LOG=warn,litebox_platform_windows_userland::
  fork_verify=error,litebox_diag::process_timeline=debug` instead — same five lines, for the WHOLE
  boot, at a small fraction of the cost. (This target was briefly `error!`/always-on by original
  design, commit `ec7427d`; demoted to `debug!` in `3042980` because `error!` + ~50 OTHER sites
  raised the same way flooded a desktop boot with thousands of lines — a real, cited log-volume
  hazard, not something to simply revert.) A cross-process fork child's guest pid IS its real
  Windows PID (`runner…/lib.rs:1673`), so `DIAG_TIMELINE execve`'s `pid=` is directly `cdb -pv -p`-able.
- **On host-side crashes, use `advisor/probes/symbolize_litebox_crash.py`, snapshotting `.exe`+`.pdb`
  next to the log** — a ring dump's `rva=` is only meaningful against the exact emitting build.
- **Isolate the harness before blaming litebox** — launch guest probes directly as the runner's
  top-level program, never via a runtime-built `/bin/sh -c` wrapper. Never trust a container tag
  name for its WM/session contents — verify by registry manifest + blob tar-listing or a live
  in-guest `/usr/bin` listing. Never record a test count not watched run to completion; never
  leave a suite red for an environmental reason. **The 53rd pass's "`xfce4-session` startup depth
  varies run to run" was itself a RAM-exhaustion artifact (fixed 56th/57th) — 58th pass's 3/3 clean
  runs all reach the IDENTICAL depth** (`iceauth`+`ssh-agent` spawned, then hangs — see Track B item 1).
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
unsanitizable `fs_base`/context. No by-name gate exists (34th pass) — only this global opt-in env
var plus the per-fork fd-kind scan. On a real `debian-xfce` boot the only remaining blocking kind is
`unix-socket`. Fork-child GPR/vmem-adopt cost is small (~1.2s, down from ~3.5-5s); the rootfs
index-merge cost the 56th pass fixed is NOT the dominant per-fork cost any more — confirmed 100%
cache-hit (both the per-layer OCI cache and the 56th pass's own merged-index cache) with
`LITEBOX_DIAG_FORK_TIMING=1` on a real `debian-xfce` boot, 75th pass: real per-fork rootfs-related
cost is ~83-140ms end to end. The real per-fork-count cost is each fork being a genuinely separate
Windows process (guest-memory emulation, its own writable-layer import) — see Track B item 1's
"process-count accumulation" note. Still open: nginx's own SSL-cert generation fails on its first
startup attempt, not root-caused (`docs/track-b-fork-fix-progress.md:146-152`).

**Pass history (4th-75th, 2026-09-17/23)**: full narrative in the dated archives ("Docs and tooling
map" below). Condensed current-state trail:

- **43rd-61st (FIXED, live-verified)**: both Xvfb SIGSEGVs; D-Bus activation's dropped-CLOEXEC-fd
  bug (`66265d9`); `fd/mod.rs:422` panic (`faa74c6`); per-fork rootfs-rebuild RAM cost (`2d18a4e`);
  the `ssh-agent`/`xfwm4` permanent-freeze class via `RawMutex::WaiterQueue::with_lock`
  (`61c235e`). `DE_FAILED` (no `_NET_SUPPORTING_WM_CHECK`) survived all of it. **62nd-66th**:
  narrowed `xfwm4` to an exact fd/protocol step (X11 census, guest-stderr, `cdb -pv`) — claims
  `WM_S0`, zero stderr, never reaches `setNetSupportedHint`; fixed `SharedUnixConnectQueue::
  cancel`'s slot leak (62nd); REFUTED `/defaults/xfce/` readdir, dbus-daemon babysitter SIGKILL,
  epoll-readiness (pid-filtered traces miss cross-process-forked GDBus siblings), GLX/compositor
  blocker theories. **67th-68th**: `DBUS_FAILED` root-caused+FIXED (`publish_as_container_fs_
  snapshot`'s byte-size regression guard discarded a healthy fresher export; gated the veto on
  THIS process's own prior-adoption failure only, `WRITABLE_LAYER_IMPORT_OK`) — 0 fires across 6
  post-fix boots vs 17 pre-fix. Upstream `xfwm4` pre-hint chain confirmed: `initSettings()`→
  `init_compositor_screen`(no-op)→`sn_init_display`→`myDisplayAddScreen`→`getNetCurrentDesktop`→
  `setUTF8StringHint`→`setNetSupportedHint`. **69th**: first byte-level D-Bus decode (`sys_recvmsg`
  payload-preview, `net.rs::do_recvmsg`) proves `initSettings()`'s call chain succeeds end-to-end,
  but its final `GetAllProperties(.../"/xfwm4/custom")` call re-issued identically every ~10.7s
  forever — matching upstream mechanism `cb_keys_changed`→`keymap_reload()` (GDK `keys-changed`),
  theory not proven. Full narrative for all of this range: archive.
- **70th**: retrigger independently reproduced (t=9.22/20.47/31.27/42.19s, deltas 10.8-11.3s);
  "`xfwm4` never writes X11" REFUTED — `sys_writev`/`sys_read` log under `syscalls::file`, not
  `net`. Fixed two logging-infra bugs (Standing lessons: line-wrap rejoin, `FlushingStderr` race).
  Reassembled RECV stream parsed 155 Replies+15 PropertyNotify+2 Errors in <1s, incl. a
  `GetKeyboardMapping`-shaped reply — but bytes after didn't parse as valid framing (real parser
  gap, not corrupted input, explained by 71st). GDK upstream source confirms `keys-changed` fires
  ONLY on genuine XKB `XkbNewKeyboardNotify`/`XkbMapNotify`, no internal timer. Did not reach `DE_UP`.
- **71st**: root-caused the 70th-pass desync — `do_read`'s socket branch (`file.rs`, calls
  `GlobalState::receive` directly) is a separate path from `do_recvmsg`; real Xlib/XCB Xtrans uses
  plain `read()`/`write()`, so a `recvmsg`-only capture missed the whole stream including ConnSetup
  (confirmed: captured stream's first bytes don't parse as a live ConnSetup reply). Fixed: new
  `litebox_diag::socket_read` diagnostic on `do_read`'s socket branch, dedicated low-overhead
  target. Three fresh-capture attempts each hit a different obstacle (blanket `file=debug`
  destabilized the boot; `de_only.sh` hit an unrelated `gpg-agent` dead end; a `webtop_stack.sh`
  boot was killed mid-`SELKIES_PORT_SELFTEST_FAILED` polling). XKB-at-retrigger remained OPEN.
- **73rd**: found+fixed the REAL reason the 71st-pass diagnostic captured zero `xfwm4` traffic — the
  `unix` closure gap above (`fc830d1`); live-confirmed (real AF_UNIX D-Bus SASL handshake traffic
  captured for the first time). Added `litebox_shim_linux::syscalls::process=debug`
  (`DIAG_TIMELINE execve`) to directly identify `xfwm4`'s own guest pid. 4/4 captures that pass
  showed `xfwm4` stopping after EXACTLY 2 reads (its D-Bus SASL handshake), attributed at the time
  to RAM/CPU starvation from the fork storm — **partially superseded by the 75th pass: `xfwm4`
  wasn't merely starved, its own xfconf config was invisible to it at all (the writable-layer
  export-path bug, fixed `1d449e6`); RAM/CPU pressure is real but was not the whole story.** XKB
  question left OPEN, unchanged since.
- **74th**: narrowed the diagnostic scope itself (assignment: cut the 73rd pass's own self-inflicted
  capture overhead). Moved all five `DIAG_TIMELINE` sites onto a dedicated `litebox_diag::
  process_timeline` target and added an optional `LITEBOX_DIAG_SOCKET_READ_TARGET` comm filter to
  `litebox_diag::socket_read` — both verified live. RAM still collapsed hard that run (7.85GB free
  at launch → 479MB at t=60s, host process count peaking at 30) — logging overhead was NOT the
  dominant RAM driver. Read the ever-present `[process_fork_diag] globalstate-probe (child):
  rebuilding rootfs from OCI image …` line (99 of them that boot) as evidence the 56th pass's
  rootfs-index cache was still missing repeatedly — **REFUTED by the 75th pass below with direct
  measurement: that line prints unconditionally on every fork regardless of downstream cache
  status, and the cache was actually hitting 100% of the time.** Also hit the xcensus.py
  writable-layer-visibility gap every attempt (`rc=2`, file not found) — **FIXED, 75th pass.**
- **75th**: two real findings, in order of consequence.
  1. **The 74th pass's rootfs-cache-miss theory is REFUTED, with direct measurement.**
     `LITEBOX_DIAG_FORK_TIMING=1` on a real `de_only_xcensus_seed2.tar` boot (`debian-xfce`, same
     harness) shows the per-layer OCI cache AND the 56th pass's merged-rootfs-index cache both
     hitting 100% of the time (`[cache] HIT`/`[diag-mergedidx] HIT` on every single one of ~28-100
     forks sampled across two boots, zero misses) — real per-fork rootfs-related cost is now
     **~83-140ms end to end** (`rootfs layers ready`→`default_fs_multi_layer returned`), far below
     even the 56th pass's own ~2.3-2.5s cache-hit target. The `globalstate-probe (child):
     rebuilding rootfs from OCI image …` line the 74th pass read as a miss signal fires
     UNCONDITIONALLY before either cache is even consulted — a high count of it is not evidence of
     wasted work. The 56th pass's own caching fix stands, fully vindicated; do not re-investigate
     it without new contrary measurement.
  2. **Root-caused and FIXED a real, deterministic (not racy) writable-layer bug that plausibly
     explains a large share of this whole investigation's "writable-layer-visibility gap"
     symptoms.** `take_cross_process_writable_layer_export`
     (`litebox_platform_windows_userland/src/lib.rs`, called from `sys_wait4`'s cross-process
     branch via `import_cross_process_writable_layer` — the ONLY place a parent ever re-absorbs a
     reaped fork child's filesystem writes) required `FORK_CHILD_TAR_PATH_ENV_VAR`, which is
     deliberately UNSET on every `--oci-image` boot (only ever set for `--initial-files`) — so the
     function returned `None` unconditionally on every OCI-image boot, meaning **a parent NEVER
     imported ANY cross-process fork child's filesystem writes back into its own live state, on
     ANY `--oci-image` boot, ever.** The child's own export-path-naming side (`diag_process_fork_
     task_resume_probe` in the runner crate) already had the correct OCI-image fallback (a
     placeholder `"oci-image"` stem); the parent's read side did not mirror it, so the two sides
     silently computed different export filenames and the parent's read always missed. Confirmed
     live with a minimal, fast repro (`-Z --oci-image ... -- /bin/bash -c 'mkdir -p /tmp/t2; ls -la
     /tmp/t2'`, both `debian:stable-slim` and `linuxserver/webtop:debian-xfce`): before the fix,
     `mkdir` reports exit 0 but the VERY NEXT sibling fork's `ls` deterministically reports `No
     such file or directory` for the identical path — 100% reproducible across 6+ repeated runs,
     not a timing race. **Fix** (`1d449e6`): mirror the child's own `.or_else(FORK_CHILD_OCI_IMAGE_
     ENV_VAR → "oci-image")` fallback on the parent's read side too. Verified: the same minimal
     repro now succeeds 4/4; the real `de_only.sh` harness's own `mkdir -p ~/.config/xfce4/xfconf/
     xfce-perchannel-xml/ && cp /defaults/xfce/*` step (previously invisible to every later
     sibling — `XFCONF_USERDIR`/`XFCONF_XFWM4XML_HEAD` both `No such file or directory`, every
     single pass since this harness existed) now succeeds, and **`xfwm4` launches for the first
     time in this investigation's entire history** — confirmed via `DIAG_TIMELINE execve`
     (`argv0=/usr/bin/xfwm4`), the X11 window count growing 0→1→11 (`XCENSUS_WINDOWS`), and
     `_NET_SUPPORTING_WM_CHECK`'s `xprop` error text advancing from "no such atom on any window"
     to "not found" (the exact 56th-pass forward-progress marker) — reproduced 2/2. Also fixed the
     seed tar's own `/tmp/xcensus.py` visibility gap (a DIFFERENT instance of the same
     export/import-staleness class, still present for a plain `cat > file <<EOF` + later-sibling
     `python3 file` round trip even after the fix above, since that round trip's SOURCE write and
     READ are two more forks either side of the SAME gap) by feeding the census script to `python3`
     via a shell variable + stdin instead of a `/tmp` file (`.wfgy/de_only_xcensus_seed3.tar`,
     disk-only, not checked in) — `XCENSUS_PRE_DE` now returns `rc=0` with real census data instead
     of `rc=2` ENOENT, giving this investigation its first-ever live X11-census ground truth.
     **Not yet reached: `DE_UP`.** Two independent post-fix boots both reached `WM_POLL n=6`
     (`_NET_SUPPORTING_WM_CHECK` still "not found") with 11+ real windows before a RAM crater (free
     RAM fell to 0.3-1.3GB, forcing cleanup) cut the run short — the already-known "process-count
     accumulation" bottleneck (Track B item 1) is now the SOLE remaining blocker on this harness,
     not a filesystem-visibility bug. `xfwm4`'s own X11 traffic capture
     (`LITEBOX_DIAG_SOCKET_READ_TARGET=xfwm4`) still showed only the 2-read D-Bus SASL handshake in
     both post-fix attempts — the RAM crater cut the run before `xfwm4` reached its steady-state
     retrigger loop, so **the XKB-event question remains genuinely open, unchanged, not newly
     answered by this pass.**

### Track B — current pickup list, precise (full pass-by-pass evidence: archive)

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
   `ssh-agent`/`xfwm4` freeze (60th/61st); `DBUS_FAILED`'s regression-guard cause (67th/68th);
   the writable-layer export-path fallback bug that silently discarded every cross-process fork
   sibling's filesystem writes on every `--oci-image` boot (75th, see pass-history above — this is
   almost certainly why `setNetSupportedHint` and the xfconf-config-dependent parts of `xfwm4`
   startup never worked in any earlier pass; the 69th/70th pass's "`GetAllProperties` retriggers
   every ~10.7s forever" symptom was captured on a boot that could never have seen `xfwm4`'s own
   copied `xfwm4.xml` config in the first place). **Pickup, in order**: (a) get RAM/process-count
   down or budget up enough for `de_only_xcensus_seed2/3.tar` under `LITEBOX_PROCESS_FORK=1` to
   survive past `WM_POLL n=6` (two 75th-pass attempts both hit a RAM crater — 0.3-1.3GB free —
   at 11+ live X11 windows, `_NET_SUPPORTING_WM_CHECK` still "not found"; a genuinely quiet host
   with more starting headroom than the ~8GB this pass had, or a reduced-diagnostic-overhead
   config, is the first thing to try — the 75th pass's OWN fix did not touch process-count
   accumulation at all, that remains exactly as the 56th pass characterized it); (b) once `DE_UP`
   fires (or the boot stalls again short of it), use `de_only_xcensus_seed3.tar`'s now-working
   `/tmp/xcensus.py` ground truth (`XCENSUS_SELECTION`/`XCENSUS_ROOTPROP` — real `_NET_SUPPORTING_
   WM_CHECK`/`WM_S0` values, not `xprop`'s heuristic text) plus `LITEBOX_DIAG_SOCKET_READ_TARGET=
   xfwm4` to see whether the ~10.7s `GetAllProperties` retrigger is still real post-fix — it was
   NOT re-observed this pass (both attempts died before `xfwm4` got past its D-Bus SASL handshake,
   same as the 73rd/74th passes, but now for a RAM reason unrelated to the fix); if it recurs,
   check for `MappingNotify`(34)/XKB at the ~10.7s boundaries before falling back to the
   30th-pass `LD_PRELOAD getenv_probe.so` reentrancy-detection technique. `ps`/`/proc` is blind to
   cross-process-forked siblings (65th) — re-weigh any `ps`-based conclusion. `gpg-agent`'s fatal
   glibc `malloc.c:3846` assertion (52nd) is why the OLD `de_only_seed.tar` (not `_xcensus_seed2/3`)
   dead-ends earlier, at `iceauth`+`ssh-agent`+`gpg-agent`.
2. `SharedUnixConnectQueue`'s cancel-on-claim-race slot leak — FIXED, 62nd pass (`unix.rs`); did NOT
   resolve the `xfwm4` symptom above, so a real but insufficient fix for THIS symptom. Other AF_UNIX
   exhaustion paths still silent (38th, `unix.rs`): `SharedUnixAddrPresenceTable` capacity-256
   overflow; a key >108 bytes; backlog ignored on cross-process accept. Abstract sockets CORRECT.
3. `SafeZoneAllocator`'s own `spin::mutex::SpinMutex` still has no dead-holder recovery — kept as a
   lower-urgency theoretical risk (the real, live `ssh-agent`/`xfwm4` freeze this was once blamed
   for was `RawMutex`'s `WaiterQueue::with_lock`, CLOSED 60th/61st, see above), not tied to any live
   symptom now.
4. Debugger-root-cause `litebox/src/event/wait.rs:224`'s `unreachable!()` on garbage thread state
   (dozens per boot, most frequent panic historically, NOT yet debugger-confirmed — do not patch
   blind).
5. `flock_registry`/`drm`/`evdev` (`GlobalState` fields) remain open, same non-POD-payload obstacle
   `SharedPtyTable` gives a template for; `timerfd`/`signalfd` are the next-cheapest carriable fd
   kinds before `socket`/`unix-socket`/`epoll`; the writable-layer-visibility gap for LARGE content
   (`/tmp/de.log` etc.) needs its own chunked-publish design, NOT a widened `SharedFilePublishTable`
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

**A stock s6-overlay image boots with no flags/stubs**: `/init` runs 16 cross-process children
with zero uncarriable fds. The once-deterministic black XFCE desktop is fixed (runtime rewriter was
corrupting `libLLVM.so.19.1`'s `.dynsym`, mesa `dlopen` failed forever). Rest settled in archive.

**XFCE also renders on the THREAD-based fork path, gated by one flag** (`docker.io/linuxserver/
webtop:debian-xfce`, `.wfgy/webtop_stack.sh`). Without it, 3/3 boots die ~7s in to ADVISORY-001
§3N's safe-linked-tcache write. Fix: `--env GLIBC_TUNABLES=glibc.malloc.tcache_count=
0:glibc.malloc.mxfast=0` as a GUEST-side `--env` runner flag (workaround, not a fix, THREAD-path
only). `LITEBOX_PROCESS_FORK=1` removes that whole crash class by construction and no longer hits
the old "Fork-after-Xorg" freeze either (35th pass). A `de_only.sh` boot runs its whole 60s+160s
window with ZERO crash/OOM as of the 57th pass (fixes landed: cross-process fork section above).
`DE_FAILED` still fires — NOT "Cannot open display" (refuted, 52nd), NOT RAM exhaustion (57th) —
see Track B item 1 for the current live blockers. Selkies also needs `--clipboard-enabled=false`
on the thread-based path (its clipboard monitor re-triggers the same corruption every tick) —
moot cross-process.

**Open here.** One client per selkies instance, no slot reclaim on reload. A second, distinct
glibc/tcache corruption signature (`double free or corruption (out)` SIGABRT) still sporadically
hits selkies on the THREAD-based fork path under heavy fork load — Track B territory; do not
re-attempt `GLIBC_TUNABLES` without evidence of a third mechanism.

**ACK-stall-kill and port-8081 watchdog — both CLOSED (2026-09-16)**, detail: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

## Host-side crash machinery

A fatal host fault dumps before it dies, ungated (stack walk, `RECENT_FAULTS` ring, `RECOVERY_LOG`,
no env var needed); a real OS minidump comes only from the repeated-identical-fault circuit
breaker. An unexplained `0xC0000005` may be a panic — the VEH handler enters only for the four
codes it triages, registers FIRST in the chain (`docs/veh-exception-handler-design.md`).
Cross-process sync on Windows is a hard platform constraint: every native address/TID-based wait
is process-local (`WaitOnAddress`, keyed events, `NtAlertThreadByThreadId`=ACCESS_DENIED); only a
shared kernel object crosses processes — `RawMutex` (below) is the one that matters;
`xproc_sync.rs`'s named-event primitive is live-verified but still unwired.

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
live-verified cross-process companion, `syscalls::pty::SharedPtyTable`). `sysv_shm` moved from a
shared-address-table design to per-process named-object mapping (51st pass, see above). Reusable
pattern (`SharedUnixAddrPresenceTable`, reused by AF_UNIX/`SharedPtyTable`): fixed-slot,
pure-atomic, lock-free `(kind, key bytes<=108, owner pid)` side-index. **A mutable-state table on
this pattern needs every WRITE path audited for shared-side mirroring** — `SharedPtyTable`'s own
setters originally only reached the local side (37th-pass live catch). Still open: `flock_registry`
(pty's pattern is now a template); `SafeZoneAllocator::alloc`'s spinlock livelock (no dead-holder
recovery unlike `RawMutex`).

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, the GUI-protocol
decision, five cheap-wins PRD rows, cross-process-fork stdio-handle bug (`spawn_suspended`'s
clobbered `STARTF_USESTDHANDLES`), presenter-process split (`docs/presenter-process-design.md`)
— all CLOSED, none open. Full detail: archive.

## Docs and tooling map

- **Archives** (newest first) — `_2026-09-22.md` (26th-69th passes, full narrative behind every
  pass-history entry above through the 69th; 70th-75th are condensed in AGENTS.md itself only, not
  yet migrated), `_2026-09-18.md` (12th-34th), `_2026-09-17.md` (shell-crash, stdio-handle bug),
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
