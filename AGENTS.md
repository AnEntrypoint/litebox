# litebox — current state (2026-09-22)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, and a claim a later commit superseded, were deleted rather than hedged. Reference
detail is drained to the `docs/AGENTS_ARCHIVE_*.md` files and per-investigation logs to the dated
`docs/*.md` in the map below — read those for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not in a separate memory file. **This file is compacted whenever it grows
past ~30KB** — newest compaction: 2026-09-22, 39th pass (44.5KB → 34.2KB; a further pass should
finish the trim toward 30KB if it grows again before then), full pre-compaction 38th-pass
Xvfb-symbolization and Shared-memory-foundations/pipe-diagnostic narratives, plus the 39th pass's
own findings, drained verbatim to `docs/AGENTS_ARCHIVE_2026-09-22.md`.

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
- **`LITEBOX_PROCESS_FORK=1` is a HOST env var, not a guest `--env`** — `spawn_cross_process_fork_child`
  (`litebox_platform_windows_userland/src/lib.rs`) reads it via a bare `std::env::var_os` on the HOST
  side; setting it via `--env` instead silently no-ops the whole cross-process path with ZERO log
  output (looks identical to "not eligible", but isn't even attempted) — confirmed live, 37th pass.

**Log level**: default is `warn,litebox_platform_windows_userland::fork_verify=error` (`EnvFilter`'s
own ERROR-only default discarded all real `warn!` sites; `fork_verify` is pinned to `error` because
it warns per single-stepped instruction). Do **not** add `LITEBOX_LOG=error` by reflex; use
`fork_verify=warn` when a fork heal is the subject. A bare `LITEBOX_LOG=debug` (blanket) is
sometimes the fastest way to rule out "which module's silent early-return ate my decision" — cheap
for a small repro, too noisy for a full desktop boot.

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
  that way (produced two recorded false conclusions, archive; a THIRD instance this session: an
  `LITEBOX_PROCESS_FORK=1` passed as `--env` instead of a host env var produced an "eligible" shim log
  with no spawn at all and no warning either — see the cheap-repro gotcha above).
- **An fd subsystem being "uncarriable" across a cross-process fork does not mean the fork must be
  refused over it** — only means that ONE fd can't be carried. Pipes/regular files/eventfds are
  carried; close-on-exec and (as of the 37th pass) pty fds are safely DROPPED and the fork proceeds
  anyway; only genuinely un-recoverable kinds (unix-socket, etc.) should still refuse. Check
  `try_cross_process_fork`'s match arms (`litebox_shim_linux/src/syscalls/process.rs`) before
  assuming a new fd kind needs the same treatment as an older, less-understood one.
- **fork carries pipes, regular files and the writable layer into a child, but NOT sockets or ptys
  by fd** — a pre-fork-created listening socket serves nothing to a forked child; a pty fd is
  dropped too, but (unlike a socket) is safely RE-OPENABLE by id afterward via `SharedPtyTable`
  (37th pass) — run dbus-daemon non-forking, and for XFCE use `xfce4-session`, never `startxfce4`.
- **A guest diagnostic must reach the console through a PIPE, never a file, and never command
  substitution** (38th pass; invalidated a large amount of this project's historical "we saw
  nothing, so nothing happened" reasoning). `cmd > /tmp/f` + parent read fails silently under
  `LITEBOX_PROCESS_FORK=1` (child writes its own writable-layer snapshot) — this broke the
  `DE_UP` check itself. `VAR=$(external-cmd)` returns EMPTY (root-caused to
  `litebox_shim_linux/src/syscalls/process.rs:2646-2649`'s fork-carry scan filtering `raw >= 3`,
  so guest fds 0/1/2 are never carried across a cross-process fork — **STILL OPEN, own dedicated
  pass needed**; `cmd | reader` and `$(builtin)` work, since those are ONE fork with the
  redirect already applied before it, unlike comsub's two forks). `cmd 2>&1 | sed 's/^/[tag] /' &`
  WORKS — use it for every guest diagnostic. Full mechanism, exact fix recipe:
  `docs/AGENTS_ARCHIVE_2026-09-22.md`.
- **`.wfgy/webtop_stack.sh` is NOT what boots — `.wfgy/webtop_seed.tar` embeds a FROZEN COPY**
  (`--resume-from`), so editing the host script alone changes nothing. Re-tar after every edit
  (`tar -xf` to a stage dir, overwrite, `tar -cf webtop_seed.tar webtop_stack.sh tmp config`) and
  verify with `tar -xOf ... | grep`. Found the hard way twice: the 35th pass's `/dev/tcp`
  readiness-gate rewrite was still absent from the tar on the 38th pass, so it had never once
  run in a guest and "not yet live-verified" was an understatement.
- **A boot whose log stops is usually a DEAD ROOT RUNNER, not a hang** — the root process hosts
  the top-level shell, so when it dies the `[s]` markers stop while the orphaned cross-process
  children (Xvfb, selkies) keep running and keep burning CPU, which reads exactly like a stall.
  Diagnose in one command: `Get-CimInstance Win32_Process -Filter "Name='litebox_runner…'" |
  ForEach-Object { $_.CommandLine.Length }` — every cross-process CHILD has the bare 77-char
  exe-only command line (`process_fork.rs:1594-1599`), so **if no survivor carries the full
  `--oci-image …` arg list, the root is gone.** Under host-RAM pressure this is an OOM-kill.
- **Before ANY `cdb` attach, set `LITEBOX_DIAG_NO_EXTERNAL_FAULT_WATCHDOG=1`** (and
  `LITEBOX_DIAG_NO_FAULT_WATCHDOG=1`). Every runner spawns an external watchdog CHILD
  (`process_fork.rs:4391`) that polls `GetProcessTimes` and calls `TerminateProcess` after 15s of
  <10ms CPU delta, with **no "was a fault armed" precondition** — and a debugger-frozen process
  makes exactly zero CPU progress, so it is killed ~15s into the session. On an already-running
  boot, kill the target's watchdog child first (it is the small ~10-20MB process parented by the
  target). These watchdogs also inflate the process count: roughly half the identical-command-line
  runner processes are watchdogs, not guests.
- **`DIAG_TIMELINE`/`sys_execve` log at `debug!`, NOT `error!`** — their own adjacent comments
  claim "always visible regardless of the configured log filter", and that is wrong; under the
  default filter they print nothing at all. Use
  `LITEBOX_LOG=warn,litebox_shim_linux::syscalls::process=debug,litebox_platform_windows_userland::fork_verify=error`.
  A cross-process fork child adopts its own **Windows PID as its guest pid**
  (`runner…/lib.rs:1673`), so a large `pid=` on a `DIAG_TIMELINE execve` line is a real host PID
  that `cdb -pv -p` accepts directly; `sys_execve: entry`'s `host_tid` is the other join.
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
- **A freestanding no-libc probe's local `char buf[N] = "literal"` array initializer can crash** —
  clang `-O1` lowers it to an aligned SSE `movaps`, and a hand-written `_start` doesn't always give
  the same alignment guarantee real crt0 does. Use a manual byte-copy loop instead (37th pass,
  `advisor/probes/pty_fork_probe.c`'s own `copy_str`).
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
regular file/eventfd/close-on-exec/pty (overridable by `LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an
unsanitizable `fs_base`/context. No by-name gate exists (confirmed by static reading, 34th pass) — the
only gate is this global opt-in env var plus the per-fork fd-kind scan. On a real `debian-xfce` boot
the only remaining blocking kind is `unix-socket`. Per-kind deviations: archive.

**Per-fork cost** was ~3.5-5s, now ~1.2s; `NGINX_STARTED` in under a minute versus never in 15+.
Use `LITEBOX_DIAG_FORK_TIMING=1` for the next cost question.

**Still open**: nginx's own SSL-cert generation fails on its first real startup attempt — the
original symptom this investigation began from, genuinely not root-caused
(`docs/track-b-fork-fix-progress.md:146-152`).

**Pass history (4th-37th, 2026-09-17/22), full narrative for every pass: `docs/
AGENTS_ARCHIVE_2026-09-17.md` (4th-11th), `_2026-09-18.md` (12th-34th, shared AF_UNIX connection
plane; DBUS_FAILED/DISPLAY-getenv()/AF_UNIX-errno CLOSED; Xvfb's SIGSEGV root-caused to a glibc
memcpy reading an unmapped pointer; ldconfig static-PIE double-relocation SIGSEGV fixed), and
`_2026-09-22.md` (26th-37th, verbatim pre-compaction text: `XVFB_UP` fix, `SharedFilePublishTable`
closing `DBUS_FAILED`, AF_UNIX errno fix, the Xvfb crash characterization, the "Fork-after-Xorg
freeze" risk confirmed gone, and the full `SharedPtyTable` design+verification narrative).** The
CURRENT STATE those passes converged on:

- **`DE_FAILED` IS THE Xvfb SIGSEGV, not a second bug — 38th pass, direct evidence.**
  `xfce4-session`'s real stderr (never readable by any prior pass — see pipe lesson above)
  contains NO "Cannot open display"; it contains an AT-SPI dbind-WARNING GTK only emits AFTER
  `gtk_init` succeeds. Log ordering: `DE_LAUNCHED` → Xvfb `Segmentation fault at
  0x7feffecdd400` → the DE's AT-SPI warning → `DE_FAILED`. A/B-CONTROLLED: the same harness with
  only `xfce4-session`'s launch removed (`.wfgy/de_noDE.sh`) runs zero Xvfb crashes. Do NOT reopen
  DISPLAY/`getenv()`/loader-stack (proven correct, 30th pass) or attach `cdb` to `xfce4-session` —
  it is not the faulting process.
- **Xvfb's crash backtrace was corrupted PROJECT-WIDE, ROOT-CAUSED (38th pass) and FIXED (39th
  pass, `7d66935`).** Cause: libunwind's x86_64 signal-frame detection matches the literal 9 bytes
  `48 c7 c0 0f 00 00 00 0f 05` (`mov $0xf,%rax; syscall`) at `__restore_rt`, and
  `litebox_syscall_rewriter` overwrites exactly those bytes in place to intercept every guest
  `syscall` (no seccomp/ptrace on this platform) — desyncing every guest backtrace through any
  delivered signal, not just Xvfb's. Fix: x86_64 signal delivery now returns through a
  litebox-synthesized trampoline (mirroring aarch64's existing `ensure_sigreturn_trampoline`)
  instead of `action.restorer` — a freshly `mmap`'d, `PROT_READ`-only page holding the REAL glibc
  bytes verbatim (so any non-CFI unwinder, including Xvfb's own `xorg_backtrace()`, still
  recognizes it), while reaching it via `ret` faults on instruction-fetch before the real
  `syscall` opcode decodes (caught in `LinuxShimEntrypoints::exception`,
  `litebox_shim_linux/src/lib.rs`). Verified: signal round-trip works end to end; Xvfb's own
  backtrace post-fix now correctly stops at the signal boundary instead of wandering 11 frames
  into stale RBP-chain garbage. Full narrative: commit `7d66935`, `docs/AGENTS_ARCHIVE_2026-09-22.md`.
- **The Xvfb SIGSEGV itself remains OPEN — the 39th pass's `ProcSELinuxGetClientContext` theory is
  REFUTED, 40th pass, direct empirical evidence, real upstream source now in hand.** Fault is
  `libc+0x162abd` = `vmovdqu (%rsi),%ymm0` inside `__memmove_avx_unaligned_erms`, reading a wild,
  fully-unmapped, bit-identical `0x7feffecdd400` across every capture (now 8+ captures, 40th pass
  included). Real `Xext/xselinux_ext.c`/`xselinux_hooks.c`/libselinux `init.c`/`getpeercon.c`
  fetched this pass (`github.com/XQuartz/xorg-server`, `github.com/SELinuxProject/selinux` —
  gitlab.freedesktop.org is still anti-bot-blocked but this mirror isn't) and read in full:
  `ProcSELinuxGetClientContext` (`xselinux_ext.c:290-305`) does NOT touch `/proc` at all — it looks
  up an already-connected X client by resource ID (`dixLookupClient`) and replies with a SID set at
  CONNECT time by `SELinuxLabelClient` (`xselinux_hooks.c:112-152`), which calls libselinux's
  `getpeercon_raw(fd, &ctx)` (`getsockopt(SOL_SOCKET, SO_PEERSEC)`) and falls back to
  `SELinuxDefaultClientLabel()` on ANY failure — confirmed this pass that litebox's `getsockopt`
  already answers unmapped options (no `SO_PEERSEC`/31 variant exists in `SocketOption`,
  `litebox_common_linux/src/lib.rs:2235-2251`) with `ENOPROTOOPT`
  (`litebox_shim_linux/src/syscalls/net.rs:2569-2572`), which is exactly the failure shape
  `getpeercon_raw` already handles by falling back — NOT a bug. More importantly: the XSELinux
  protocol handler is only ever registered via `AddExtension` inside `SELinuxExtensionInit`
  (`xselinux_ext.c:690-712`) if `is_selinux_enabled()` returns true, which itself requires
  `statfs("/sys/fs/selinux", …).f_type == SELINUX_MAGIC` (`libselinux/src/init.c`
  `verify_selinuxmnt`) — a check litebox's `statfs` could never satisfy (see the fix below), so by
  the real source the extension should never even init. **Decisive test, 40th pass**: reran
  `.wfgy/de_only.sh` under `LITEBOX_PROCESS_FORK=1` (sidesteps the thread-fork tcache-corruption
  class entirely, giving a clean, fast, reproducible path to the crash — 2/2 runs,
  `.wfgy/de_only_statfsfix_pfork_1.log`) with Xvfb launched with `-extension "SELinux"` added
  (explicitly disabling the extension at the protocol level, `.wfgy/de_only_noselinux_seed.tar`):
  **the SAME bit-identical `0x7feffecdd400` SIGSEGV still occurs, 2/2 runs**
  (`.wfgy/de_only_noselinux_1.log`, `.wfgy/de_only_noselinux_2.log`). An explicitly-disabled
  extension cannot be the crash site — the 39th pass's `addr2line`-based attribution to
  `ProcSELinuxGetClientContext`/`xselinux_ext.c:305` was a misattribution (the `[rsp+0]`
  raw-stack-word read, though a sounder method than a frame-pointer walk, was not actually the true
  return address this time — do not re-trust a single raw-stack-word capture without an independent
  cross-check again). **Do not re-open the SELinux/XSELinux/`is_selinux_enabled`/`getpeercon`
  thread — fully closed, 40th pass.** The real crash site is UNKNOWN again; next step needs a
  genuine live debugger attach on Xvfb itself (`cdb -pv`, breaking on `__memmove_avx_unaligned_erms`
  or its caller) or CFI-based unwinding, neither attempted by any pass to date — a raw-stack-word
  scan alone has now produced one confirmed-wrong lead and should not be trusted alone again.
  **Independent, unrelated bug found+FIXED same pass**: `SyscallRequest::Statfs`/`Fstatfs`
  (`litebox_shim_linux/src/lib.rs:2450` pre-fix) ignored `pathname`/`fd` entirely
  (`pathname: _`/`fd: _`) and unconditionally returned a canned tmpfs-shaped success for ANY path
  or fd number, including nonexistent/closed ones — a real, independent correctness bug (statfs on
  a nonexistent path, or fstatfs on a bad fd, must fail) now fixed by validating via `sys_stat`/
  `sys_fstat` first (a real function, `write_tmpfs_statfs`, factored out and reused by both);
  live-verified via an A/B stash/rebuild/rerun that it does NOT regress the pre-existing
  thread-fork tcache-corruption flakiness (bit-identical failure with and without the fix) and does
  NOT (as hoped) eliminate the Xvfb SIGSEGV either, consistent with the magic-number analysis above
  (litebox's `f_type` is `TMPFS_MAGIC`, never `SELINUX_MAGIC`, before or after this fix). Full
  evidence and log paths: `docs/AGENTS_ARCHIVE_2026-09-22.md`'s 40th-pass entry.
- **The "Fork-after-Xorg PERMANENT freeze" risk is CONFIRMED GONE** (35th pass, live evidence: a
  full `LITEBOX_PROCESS_FORK=1` release-binary boot reached its designed idle `HOLD` loop with zero
  freeze/SIGSEGV/tcache-corruption). `LITEBOX_PROCESS_FORK=1` is now the RECOMMENDED flag for
  `.wfgy/webtop_stack.sh`, evidenced-safe-pending-reconfirmation under workable host RAM. That same
  pass rewrote the `SELKIES_PORT_UP`/`NGINX_SELFTEST` readiness polls from a 170x-`curl`-fork loop
  (which was paying the full cross-process rootfs-rebuild cost per iteration, the likely OOM-kill
  cause) to a zero-fork bash `/dev/tcp/HOST/PORT` builtin check. **38th-pass correction: that
  rewrite had never once executed in a guest** — `.wfgy/webtop_seed.tar` still carried the
  pre-rewrite frozen copy (tar mtime 09-21 09:03, `grep -c dev/tcp` = 0), so "not yet
  live-verified" understated it. Tar regenerated 38th pass; the gate now runs, and on its first
  real execution reported `NGINX_SELFTEST_FAILED last_code=` (empty) — the `/dev/tcp` connect
  path returns no HTTP status line, so this gate is **still not proven working** and needs its
  own look, separately from the desktop question.
- **`pty_registry`/`daemon_pty_masters` cross-process redesign is DONE and genuine cross-process
  pty I/O is LIVE-PROVEN** (36th pass: `syscalls::pty::SharedPtyTable`; 37th pass: proved it live
  via `advisor/probes/pty_fork_probe.c` — a genuinely separate Windows fork child opened
  `/dev/pts/<id>` fresh and read a marker the parent wrote to the master after `fork()` — and fixed
  two real bugs local-only testing hadn't surfaced: a fork-eligibility scan that refused the whole
  fork over any open pty fd, and `PtyStateRef::Local` setters that never mirrored lock/termios
  state into the shared slot). Mechanism: "Shared-memory foundations" below. Full narrative, exact
  repro, log evidence: `docs/AGENTS_ARCHIVE_2026-09-22.md`.

### Track B — current pickup list, precise (full pass-by-pass evidence: archive)

Fully DONE (kept only as a marker so a future pass doesn't re-attempt): the minimal isolated
cross-process AF_UNIX repro; the `Network` shared-arena redesign's `socket_set`/
`LocalPortAllocator`/`closing_in_background`/`queued_for_closure` slice; DISPLAY/`getenv()` as the
`DE_FAILED` cause (REFUTED FOR GOOD); AF_UNIX `connect()` `EAGAIN`-vs-`EINPROGRESS`; `pty_registry`/
`daemon_pty_masters` (`syscalls::pty::SharedPtyTable`, now live-verified cross-process, 37th pass).

**Open, in rough priority order:**

1. **Xvfb's SIGSEGV — still the SINGLE top blocker** (`DE_FAILED` is a downstream symptom, not a
   second bug, 38th pass). The backtrace-corruption meta-bug that blocked investigating it is FIXED
   (39th pass, `7d66935`). The 39th pass's `ProcSELinuxGetClientContext` lead is REFUTED (40th
   pass, real source read + the crash reproduces 2/2 with the `SELinux` X extension explicitly
   disabled — see above); do not re-open it. Pickup, in order: (a) live `cdb -pv` attach on Xvfb
   itself under `LITEBOX_PROCESS_FORK=1` (use `.wfgy/de_only.sh`/`de_only_seed.tar` for a fast,
   clean ~2-minute repro to the crash — no `LITEBOX_DIAG_FATALDUMP` needed once a debugger is
   attached), breaking on `__memmove_avx_unaligned_erms` or catching the access violation directly,
   to get a REAL call stack instead of another raw-stack-word guess — not attempted by any pass to
   date; (b) failing that, CFI-based unwinding against `.wfgy/xvfb.debug`'s DWARF `.eh_frame`; (c)
   once a real caller is known, re-derive which litebox-emulated syscall (if any) it depends on
   before assuming a litebox bug — the SELinux lead's own lesson this pass.
2. ~~`xfce4-session`'s `DE_FAILED` as an independent bug~~ — **REFUTED, 38th pass.** It is the
   Xvfb crash seen from downstream. Do not spend a pass attaching `cdb` to `xfce4-session`.
2b. **AF_UNIX cross-process tables have FOUR silent exhaustion paths, none logging anything**
   (38th-pass static audit, `litebox_shim_linux/src/syscalls/unix.rs`): `SharedUnixAddrPresenceTable`
   capacity 256 (`unix.rs:2585`) whose `insert` return is DISCARDED at `unix.rs:275-277` (an
   over-capacity `listen(2)` still returns success, later clients get `ECONNREFUSED`); a key >108
   bytes (`UNIX_ADDR_KEY_MAX`) silently bails in `insert`/`post`/`try_claim`/`has_pending`;
   `SharedUnixConnectQueue` capacity 64 returns `EAGAIN`; `SharedUnixConnTable` capacity 64 leaves a
   request `REQ_CLAIMED` FOREVER (`unix.rs:430-435`, `cancel` only CASes `REQ_PENDING→REQ_EMPTY`) —
   a monotonic slot leak for the fork family's life. Also: `SHARED_UNIX_CONN_BUF` is only 2048
   bytes/direction, and the cross-process accept path ignores the listener backlog entirely
   (`unix.rs:420-452` never reads `state.limit`). Abstract sockets checked and CORRECT (not a
   suspect).
3. `SafeZoneAllocator`'s `spin::mutex::SpinMutex` (`litebox/src/mm/allocator.rs`) needs the same
   dead-holder-recovery treatment `RawMutex` already has — live-caught spinning forever in
   `dealloc`, high blast radius, own dedicated pass.
4. Debugger-root-cause `litebox/src/event/wait.rs:224`'s `unreachable!()` on garbage thread state
   (dozens per boot, most frequent panic historically, NOT yet debugger-confirmed — do not patch
   blind).
5. `SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap (`UnixStreamState::
   Connecting(request_idx)` needed) — scoped, not yet fixed.
6. `flock_registry`/`drm`/`evdev` (`GlobalState` fields, eighteenth-pass audit) remain open — same
   non-POD-payload (Arc-based state, `Pollee` observer lists) obstacle the pty fix's own
   `PtyEnd::Shared{Master,Slave}`/`SharedPtyTable` pattern now gives a concrete template for, not
   yet applied to any of the three; not on the Xvfb/selkies boot path so lower urgency.
7. After the above, `timerfd`/`signalfd` are the next-cheapest carriable fd kinds before attempting
   `socket`/`unix-socket`/`epoll` (`pty` itself is no longer purely uncarriable — a cross-process
   opener can re-acquire a pty by id via `pts_open`'s shared fallback even though the fd itself
   still isn't carried across `fork()`).
8. The general writable-layer-visibility gap for LARGE/unbounded content (`/tmp/de.log`/
   `/tmp/de2.log`/`/tmp/xvfb.log`, `/tmp/wm1`/`/tmp/wm2`) remains open —
   `SharedFilePublishTable`'s 256-byte cap must NOT be widened to try to cover it; a real fix needs
   its own design (chunked publish, or a genuine shared-arena ring rather than a snapshot table).

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
webtop:debian-xfce`, `.wfgy/webtop_stack.sh`). Without it, 3/3 boots die ~7s in to ADVISORY-001
§3N's safe-linked-tcache write. Fix: `--env GLIBC_TUNABLES=glibc.malloc.tcache_count=
0:glibc.malloc.mxfast=0` as a GUEST-side `--env` runner flag (a bare host `$env:`/`export` does NOT
reach the guest — same class of mistake as the `LITEBOX_PROCESS_FORK` gotcha above). Workaround,
not a fix, for the THREAD-based path specifically. `LITEBOX_PROCESS_FORK=1` removes that whole
crash class by construction (no relocation) and no longer hits the old "Fork-after-Xorg" freeze
either (35th pass) — the real current blocker on EITHER fork path is the Xvfb SIGSEGV (see above),
not AF_UNIX. Selkies also needs `--clipboard-enabled=false` on the thread-based path (its clipboard
monitor re-triggers the same corruption every tick) — moot on the cross-process path.

**Open here.** One client per selkies instance, no slot reclaim on reload. **A SECOND, distinct
glibc/tcache corruption signature** (`double free or corruption (out)` SIGABRT) still sporadically
hits selkies on the THREAD-based fork path under heavy fork load — Track B territory, not a
tunable-coverage gap; do not re-attempt a `GLIBC_TUNABLES` fix without evidence of a THIRD
mechanism. Detail: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

### The ACK-stall-kill and port-8081 watchdog — both CLOSED (2026-09-16)

Real blocker was the guest-side patcher silently crashing on `shutil.copy2()`'s `copystat()` (no
`listxattr` shim) before ever patching `selkies.py`; fixed (`478e640`). Port-8081 double-bind fix
live-verified over 17 boot cycles + a 6000-connection stress test, zero recurrence. Detail: `docs/
AGENTS_ARCHIVE_2026-09-16.md`.

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
`LiteBoxX`/`GlobalState` placement (NOT wired to `GlobalAlloc`; `SLAB_ALLOC` stays private-per-process).
**Root cause of the whole `GlobalState`-sharing class**: `SharedArc::new` shares only `T`'s literal
inline bytes -- any registry that was a `BTreeMap`/similar has its NODES on the private per-process
heap, meaningless to an attaching process. Of the original uncarriable-registry list
(`unix_addr_table`/`pty_registry`/`daemon_pty_masters`/`flock_registry`/`fifo_registry`/`sysv_shm`/
`memfds`/`shared_files`): all but `flock_registry` are now fixed (per-process-shadowed, a real
shared-arena fixed array, or — for `pty_registry`/`daemon_pty_masters`, which need genuine
cross-process visibility per real devpts semantics — both a per-process shadow AND a live-verified
cross-process companion, `syscalls::pty::SharedPtyTable`). The reusable pattern
(`SharedUnixAddrPresenceTable` in `syscalls/unix.rs`, reused by `sysv_shm`/the AF_UNIX
connection-data layer/`SharedPtyTable`): fixed-slot, pure-atomic, lock-free `(kind, key
bytes<=108, owner pid)` side-index, or a POD-control-state variant for mutable per-slot fields.
**A mutable-state table on this pattern needs every WRITE path individually audited for
shared-side mirroring** — `SharedPtyTable`'s own setters were originally wired only into the local
side and silently never reached the shared slot until a live cross-process test caught it (37th
pass); a read-only/publish-once table doesn't have this failure mode, any ongoing-mutable-state
table does. Still open: `flock_registry` (same non-POD-payload obstacle, pty's own pattern is now
a concrete template); `SafeZoneAllocator::alloc`'s spinlock livelock (no dead-holder recovery
unlike `RawMutex`).

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, the GUI-protocol
decision, five cheap-wins PRD rows, cross-process-fork stdio-handle bug (`spawn_suspended`'s
clobbered `STARTF_USESTDHANDLES`), presenter-process split (`docs/presenter-process-design.md`)
— all CLOSED, none open. Full detail: archive.

## Docs and tooling map

- **Archives** (newest first) — `_2026-09-22.md` (26th-40th passes: `XVFB_UP`/`SharedFilePublishTable`/
  `DBUS_FAILED`/AF_UNIX-errno fixes, the full Xvfb-crash symbolization narrative + the
  sigreturn-trampoline fix and call-site narrowing (39th), the `ProcSELinuxGetClientContext` lead
  REFUTED + the `statfs`/`fstatfs` path-blind-success bug fixed (40th), "Fork-after-Xorg freeze"
  confirmed gone, the full `SharedPtyTable` design+verification, and the drained
  Shared-memory-foundations/pipe-diagnostic
  full-mechanism writeups), `_2026-09-18.md` (12th-34th, shared AF_UNIX connection plane, ldconfig
  static-PIE fix), `_2026-09-17.md` (shell-crash investigation, stdio-handle bug, writable-layer-race
  fix), `_2026-09-16.md` (Track A audit, RawMutex/presenter), `_2026-09-15.md` (ACK-stall-kill),
  `_2026-09-10.md` (fork fd eligibility, OCI cache, s6-boot, crash-dump/VEH). Older: `_2026-09-03.md`,
  `_2026-09-05.md`.
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
  `drm_flip_probe.c`, `clone_probe.c`, `socketpair_fork_probe.c`, `pty_fork_probe.c` — cross-process
  pty I/O verification) plus `MEASUREMENT-PITFALLS.md`, `DISK-HYGIENE.md`.
- `.gm/memories/` — older per-topic notes, superseded by this file/archives.
