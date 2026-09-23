# litebox — current state (2026-09-23)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, and a claim a later commit superseded, were deleted rather than hedged. Reference
detail is drained to the `docs/AGENTS_ARCHIVE_*.md` files and per-investigation logs to the dated
`docs/*.md` in the map below — read those for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not a separate memory file. **Compacted past ~30KB** — newest: 2026-09-23,
64th pass (pass-history section below; 26th-64th full narrative, including this pass's own complete
evidence and fix rationale: `docs/AGENTS_ARCHIVE_2026-09-22.md`).

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

**Log level**: default is `warn,litebox_platform_windows_userland::fork_verify=error` (`fork_verify`
pinned to `error` since it warns per single-stepped instruction). Do **not** add `LITEBOX_LOG=error`
by reflex; use `fork_verify=warn` when a fork heal is the subject. A bare `LITEBOX_LOG=debug` rules
out "which module's silent early-return ate my decision" fast but is too noisy for a full desktop
boot — target `litebox_shim_linux::syscalls::{process,unix}=debug,litebox_diag::stderr_capture=debug`
on the `de_only.sh` isolation harness instead.

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
  `cross_process_children`-tracked pid returns `ESRCH` unconditionally (pass 141, a documented gap,
  not a bug: the pid is real and reachable via `wait4`, just not signalable yet).
- **A `socketpair(2)`-originated fd (both ends `Unnamed`) is NOT safe to drop as CLOEXEC across a
  cross-process fork, unlike a named-peer CLOEXEC client socket** — real processes (`dbus-daemon`'s
  babysitter) use it for pre-`exec()` bookkeeping; dropping it makes the child's peer look
  instantly gone to the parent. `raw_fd_is_addressless_unix_socket_pair` (`net.rs`) now refuses
  (falls back to thread-based fork) this narrow case instead of silently dropping it (54th pass).
- **A `TypedFd`'s index is only valid against the SAME `Descriptors` instance that `insert()`ed
  it** — reading one back against a DIFFERENT process's table is out-of-bounds or resolves to an
  unrelated entry; every accessor in `litebox/src/fd/mod.rs` returns `None` rather than panicking
  (`faa74c6`) — the same class as the `Network::queued_for_closure`/`Pipes.litebox`/`FutexManager`
  bugs before it.
- **A `de_only.sh`/`LITEBOX_PROCESS_FORK=1` boot no longer crashes on RAM** — plateaus ~3.1-3.3GB
  free; still check `FreePhysicalMemory` throughout regardless. **Confirm the release binary's mtime
  postdates the newest relevant commit before trusting a boot result** (57th pass caught a ~1hr-stale
  binary this way). See Track B item 1.
- **A guest diagnostic must reach the console through a PIPE or `$( )`, never a bare file redirect**
  — `cmd > /tmp/f` + parent read fails silently under `LITEBOX_PROCESS_FORK=1` (child writes its
  own writable-layer snapshot). `cmd 2>&1 | sed 's/^/[tag] /' &` is the pattern for streaming
  output; `VAR=$(external-cmd)` also genuinely works now (44th-pass fd-carry fix). Full mechanism:
  archive.
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
- **`DIAG_TIMELINE`/`sys_execve` log at `debug!`, NOT `error!`** — use
  `LITEBOX_LOG=warn,litebox_shim_linux::syscalls::process=debug,litebox_platform_windows_userland::
  fork_verify=error`. A cross-process fork child's guest pid IS its real Windows PID
  (`runner…/lib.rs:1673`), so `DIAG_TIMELINE execve`'s `pid=` is directly `cdb -pv -p`-able.
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
`unix-socket`. Fork-child GPR/vmem-adopt cost is small (~1.2s, down from ~3.5-5s); the DOMINANT
per-fork cost on an `--oci-image` boot is the rootfs rebuild, not this (56th pass, below). Still
open: nginx's own SSL-cert generation fails on its first startup attempt, not root-caused
(`docs/track-b-fork-fix-progress.md:146-152`).

**Pass history (4th-64th, 2026-09-17/23)**: full narrative in the dated archives ("Docs and tooling
map" below). The CURRENT STATE those passes converged on:

- **Both Xvfb SIGSEGVs FIXED** (43rd `01f8532` top-down-search library-crowding; 51st `c2112bc`
  `sys_shmat` addr-only-valid-in-creator, now a NAMED `CreateFileMappingW`), confirmed gone on a
  full `webtop_stack.sh` boot (52nd). Do NOT reopen DISPLAY/`getenv()`/loader-stack (proven correct,
  30th). Also DONE: Xvfb backtrace corruption (38th/39th `7d66935`); `LITEBOX_DIAG_FATALDUMP=1`
  (VEH, no debugger) is the only viable capture for this crash family, `cdb` proven infeasible;
  "Fork-after-Xorg PERMANENT freeze" gone (35th); cross-process pty I/O proven (36th/37th).
- **44th-59th** — `xfce4-session` reached real pre-session setup (fd-at-fork-boundary,
  connect-cancel race, `PROT_NONE`-adoption panic, allocator corruption fixed en route); D-Bus
  activation's dropped-CLOEXEC-fd bug FIXED (`66265d9`, `xfwm4` genuinely `execve`d for the first
  time ever right after); `fd/mod.rs:422` panic FIXED (`faa74c6`); per-fork rootfs-rebuild RAM cost
  FIXED (`2d18a4e`); `ps -ef` crash FIXED; task-scoped cross-process exit wait added. Root-caused
  `xfwm4`'s non-launch as 100% reproducible: `xfce4-session` spawns `iceauth`+`ssh-agent` then goes
  silent, `cdb -pv` showing a spinlock retry loop (tentatively, WRONGLY, blamed on
  `SafeZoneAllocator`, see 60th). `gpg-agent`'s fatal glibc `malloc.c:3846` SIGABRT, reproduced
  once, still open. Full narrative for all of the above: archive.
- **60th-61st — the ssh-agent/xfwm4 permanent-freeze class CLOSED for good.** Root cause:
  `RawMutex`'s `WaiterQueue::with_lock` released via a plain statement, not a RAII guard, so a panic
  inside `SafeZoneAllocator` mid-`wake_many` wedged the queue's lock `true` forever (not a dead
  holder — the panicking thread survives `catch_unwind`, so `RawMutex`'s own liveness recovery
  never triggers). Fixed `61c235e` (RAII release via `litebox::utils::defer`). Confirmed on the
  RELEASE binary, 3/3 clean boots, zero hang (vs. 100% pre-fix). `DE_FAILED` still happens.
- **62nd — independently re-verified; narrowed further; one real leak found+fixed, symptom NOT
  resolved.** Fetched real upstream `xfwm4` source; 3 independent live techniques (X11 ground-truth
  census via `XGetSelectionOwner`/`XQueryTree`, guest-stderr capture, mid-hang `cdb -pv`) agree:
  `xfwm4` genuinely claims `WM_S0` and finishes `myScreenInit`, prints ZERO stderr, never reaches
  `setNetSupportedHint` — stuck inside `initSettings()`'s `xfconf_init`/`xfconf_channel_new` D-Bus
  call chain. `xfconfd` independently verified alive via `dbus-send ... Introspect` — refutes
  "xfconfd never starts." Found+fixed a real leak: `SharedUnixConnectQueue::cancel` (`unix.rs`)
  permanently leaked a slot on a claim-race against the listener's `accept()`; fixed via the same
  Drop-based release path a normal closed connection uses. Verified building/running; did NOT
  resolve the `xfwm4` symptom.
- **63rd — `/defaults/xfce/` readdir hypothesis REFUTED; `DE_FAILED` reconfirmed; new lead
  surfaced.** Isolated repro + full production-log re-read: `layered.rs`'s upper/lower `read_dir`
  merge is CORRECT under cross-process fork (the 62nd pass's "total 0" was `ls -la`'s block-count
  header, not an entry count). `DE_FAILED` reconfirmed byte-for-byte: 25 real windows exist, `WM_S0`
  genuinely owned by `xfwm4`, `_NET_SUPPORTING_WM_CHECK` stays empty through 12 `WM_POLL`s (60s).
  New lead surfaced (not yet chased): a second `dbus-daemon` child `SIGKILL`ed ~0.4s after a
  thread-based-fork fallback.
- **64th — the dbus-daemon `SIGKILL` lead REFUTED as the `DE_FAILED` cause; one real, independent,
  low-severity bug found instead; `DE_FAILED` UNCHANGED.** Rebuilt the debug binary,
  `syscalls::{process,signal}=trace,net=debug,unix=debug`, pid-level execve/wait4 trace across the
  exact kill window. **Identity**: the killed pid is dbus-daemon's own activation "babysitter"
  (`comm` stays `dbus-daemon\0`, never execs) — its own fork's `argv0` is
  `/usr/lib/x86_64-linux-gnu/xfce4/xfconf/xfconfd`, i.e. this IS the `org.xfce.Xfconf` activation
  `xfwm4`'s `initSettings()` depends on. **Cause of death** (`pid=39` babysitter/`pid=40`
  xfconfd/`pid=19372` real daemon): babysitter forks+execs xfconfd cleanly (t=83.334s), then goes
  completely silent (zero syscalls) for ~1.28s — a real thread-based-fork-fallback scheduling
  stall, not corruption (it answers correctly the moment it resumes). The real daemon does
  `wait4(babysitter,WNOHANG)` (still alive) then `wait4(babysitter,0)` BLOCKING ~4ms later; 43
  MICROSECONDS after that the babysitter's own signal check finds `SIGKILL` already queued and
  dies. `sys_kill`/`sys_tgkill` (`signal/mod.rs:852-862`) log nothing, so the kill itself is
  invisible in any trace — but this exact WNOHANG-then-kill-then-blocking-reap sequence is upstream
  `dbus-spawn-unix.c`'s OWN documented `_dbus_babysitter_unref()` cleanup fallback (fetched and read
  live this pass): normal successful-activation cleanup expects the babysitter to self-exit via a
  pipe-close `poll()` HUP, and explicitly `kill(sitter_pid, SIGKILL)`s it if it's too slow to notice
  — exactly what's observed. **Real, correctly-emulated dbus behavior, not a litebox
  signal-delivery bug.** **Definitively NOT the `DE_FAILED` cause**: xfconfd (`pid=40`) is a
  SEPARATE OS process from its babysitter, unaffected by the babysitter's SIGKILL — its own trace
  shows a complete D-Bus SASL handshake (`AUTH EXTERNAL`→`OK`→`NEGOTIATE_UNIX_FD`→`AGREE_UNIX_FD`→
  `BEGIN`, t=84.35-84.41s, BEFORE the babysitter's own death at t=84.621s) then idles healthily 10+
  more seconds — a fully live, fully-registered `org.xfce.Xfconf` throughout, matching the
  62nd/63rd passes' own `dbus-send` confirmation. **Real independent low-severity bug found, not
  fixed** (cosmetic, zero functional impact, lower priority than `DE_FAILED`): a
  thread-based-fork-fallback child can go fully unscheduled for over a second right after its own
  fork+exec, missing a real upstream fast-shutdown window and hitting a defensive-kill path instead
  — flagged as a new Track B candidate (root cause — genuine host CPU contention vs. a real
  fallback-path scheduling defect — not isolated). **Pickup**: this lead is now a dead end for
  `DE_FAILED` — proceed to a byte-level trace of `xfwm4`'s own D-Bus fd specifically, cross-
  referenced against real `xfce-mirror/xfconf` source for `xfconf_channel_new`'s exact GDBus call
  sequence, or a `cdb -pv` attach on `xfwm4` itself once RAM is quiet. Full trace
  (`.wfgy/pass64_dbus_trace_run1.log`, gitignored) and line numbers: archive.

### Track B — current pickup list, precise (full pass-by-pass evidence: archive)

Fully DONE (kept only as a marker so a future pass doesn't re-attempt): the minimal isolated
cross-process AF_UNIX repro; the `Network` shared-arena redesign's `socket_set`/
`LocalPortAllocator`/`closing_in_background`/`queued_for_closure` slice; DISPLAY/`getenv()` as the
`DE_FAILED` cause (REFUTED FOR GOOD); AF_UNIX `connect()` `EAGAIN`-vs-`EINPROGRESS`; `pty_registry`/
`daemon_pty_masters` (`syscalls::pty::SharedPtyTable`, live-verified cross-process, 37th pass);
fork's fd-eligibility scan dropping a redirected 0/1/2 (44th, `raw_fd_is_plain_stdio_device`);
`SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap (44th, `UnixStreamState::Connecting`);
both Xvfb SIGSEGVs (43rd/51st, confirmed on the full stack by the 52nd).

**Open, in rough priority order:**

1. **`DE_FAILED`'s real chain — the `ssh-agent`/`xfwm4` permanent-freeze class is CLOSED for good**
   (60th/61st). Current blocker (62nd-64th): `xfwm4` genuinely claims `WM_S0`, finishes
   `myScreenInit`, prints ZERO stderr, never returns from `initSettings()`'s `xfconf`/D-Bus call
   chain — `xfconfd` independently confirmed alive and fully registered on the bus throughout
   (64th's own SASL-handshake trace), so this is NOT "xfconfd never starts" and NOT the 63rd/64th
   passes' dbus-daemon-babysitter `SIGKILL` (both REFUTED, see pass history above). Needs a
   BYTE-LEVEL trace of `xfwm4`'s own D-Bus fd specifically (not the whole boot's `unix=debug`,
   150-300MB/minute, impractical past ~15s) plus real `xfce-mirror/xfconf` source for its exact
   GDBus call/signal-subscribe sequence, or a `cdb -pv` attach on `xfwm4` itself. PER-PROCESS RAM is
   the hard ceiling on any attempt (~28-29 live host processes, 8.5GB→<1GB in <90s on `de_only.sh`
   alone) — budget for it, kill fast, never run two attempts back-to-back. Secondary, lower
   priority: `gpg-agent`'s fatal glibc `malloc.c:3846` assertion (52nd, maybe same mechanism,
   recheck once `DE_UP`); high `VM_SHARED` fork-child region count (52nd); the 64th pass's own
   thread-based-fork-fallback scheduling-stall finding (new Track B candidate, zero functional
   impact so far).
2. **`SharedUnixConnectQueue`'s cancel-on-claim-race slot leak — FIXED, 62nd pass** (`unix.rs`,
   `cancel()` now drains an abandoned `REQ_ACCEPTED` slot via the same Drop-based release path a
   normal closed connection uses, rather than leaking it until the whole fork family exits); a
   `WARN` on `post()`'s queue-full return was also added (previously silent). Verified building and
   running; did NOT resolve the `xfwm4` symptom above, so likely a real but insufficient fix for
   THIS symptom — may still matter under `webtop_stack.sh`'s strictly higher connect load. Other
   AF_UNIX exhaustion paths still silent (38th, `unix.rs`): `SharedUnixAddrPresenceTable`
   capacity-256 overflow (`unix.rs:275-277`); a key >108 bytes; backlog ignored on cross-process
   accept. Abstract sockets checked and CORRECT.
2b. **REFUTED, 63rd/64th passes**: the 62nd pass's `/defaults/xfce/`-appears-empty-to-a-later-fork
   hypothesis (63rd — `layered.rs` merge is correct, `ls -la`'s "total 0" was its block-count
   header) AND the dbus-daemon-babysitter-`SIGKILL` hypothesis (64th — real, correctly-emulated
   upstream dbus cleanup behavior; xfconfd itself fully registers on the bus independent of its
   babysitter's fate, see 64th pass entry above). Neither is the `DE_FAILED` mechanism. The 64th
   pass's own real-but-cosmetic side finding (a thread-based-fork-fallback child going unscheduled
   for ~1.3s) is a new, lower-priority Track B candidate, not yet root-caused.
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
no host directory is ever created for the rootfs (a real one hit three Windows-path bugs).
Rewritten layers cached under `.litebox-cache/`, keyed so a rewriter change self-invalidates.
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
the old "Fork-after-Xorg" freeze either (35th pass). BOTH Xvfb SIGSEGVs are fixed and CONFIRMED on a
full `webtop_stack.sh` boot too (51st/52nd passes, zero crashes). D-Bus service-activation
false-"exited" (54th), the `fd/mod.rs:422` panic (55th, `faa74c6`), and per-fork RAM cost (56th,
`2d18a4e`) are all FIXED — a `de_only.sh` boot now runs the entire 60s+160s window with ZERO
crash/OOM (57th pass). `DE_FAILED` still fires — NOT "Cannot open display" (refuted, 52nd), NOT RAM
exhaustion any more (57th) — see Track B item 1 for the current live blockers. Selkies also needs
`--clipboard-enabled=false` on the thread-based path (its clipboard monitor re-triggers the same
corruption every tick) — moot cross-process.

**Open here.** One client per selkies instance, no slot reclaim on reload. A SECOND, distinct
glibc/tcache corruption signature (`double free or corruption (out)` SIGABRT) still sporadically
hits selkies on the THREAD-based fork path under heavy fork load — Track B territory, not a
tunable-coverage gap; do not re-attempt `GLIBC_TUNABLES` without evidence of a THIRD mechanism.

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

- **Archives** (newest first) — `_2026-09-22.md` (26th-64th passes, full narrative for everything
  this file's own entries summarize: the 62nd pass's xfwm4/xfconf X11-census+stderr-capture+cdb
  evidence, the `SharedUnixConnectQueue::cancel` leak fix, the 63rd pass's readdir-hypothesis
  refutation, and the 64th pass's dbus-daemon-babysitter-SIGKILL root cause+refutation),
  `_2026-09-18.md` (12th-34th,
  shared AF_UNIX plane, ldconfig static-PIE fix), `_2026-09-17.md` (shell-crash, stdio-handle bug,
  writable-layer-race fix), `_2026-09-16.md` (Track A audit, RawMutex/presenter), `_2026-09-15.md`
  (ACK-stall-kill), `_2026-09-10.md` (fork fd eligibility, OCI cache, s6-boot, crash-dump/VEH). Older:
  `_2026-09-03.md`, `_2026-09-05.md`.
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
