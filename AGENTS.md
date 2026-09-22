# litebox — current state (2026-09-22)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, and a claim a later commit superseded, were deleted rather than hedged. Reference
detail is drained to the `docs/AGENTS_ARCHIVE_*.md` files and per-investigation logs to the dated
`docs/*.md` in the map below — read those for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not in a separate memory file. **This file is compacted whenever it grows
past ~30KB** — newest compaction: 2026-09-22, 42nd pass (46.2KB → under 30KB), full pre-compaction
41st-pass and 38th-40th-pass narratives drained verbatim to `docs/AGENTS_ARCHIVE_2026-09-22.md`.

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

**Log level**: default is `warn,litebox_platform_windows_userland::fork_verify=error` (`fork_verify`
pinned to `error` since it warns per single-stepped instruction). Do **not** add `LITEBOX_LOG=error`
by reflex; use `fork_verify=warn` when a fork heal is the subject. A bare `LITEBOX_LOG=debug`
(blanket) is the fastest way to rule out "which module's silent early-return ate my decision" —
cheap for a small repro, too noisy for a full desktop boot.

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
  close-on-exec and (37th pass) pty fds are safely DROPPED and the fork proceeds; only genuinely
  un-recoverable kinds (unix-socket) still refuse. Check `try_cross_process_fork`'s match arms
  (`litebox_shim_linux/src/syscalls/process.rs`) before assuming a new kind needs old treatment.
  A pty fd, unlike a socket, is safely RE-OPENABLE by id afterward via `SharedPtyTable` — run
  dbus-daemon non-forking, and for XFCE use `xfce4-session`, never `startxfce4`.
- **A guest diagnostic must reach the console through a PIPE or `$( )`, never a bare file redirect**
  (38th pass). `cmd > /tmp/f` + parent read fails silently under `LITEBOX_PROCESS_FORK=1` (child
  writes its own writable-layer snapshot). `VAR=$(external-cmd)` used to return EMPTY (the
  fork-carry scan hard-cut fds 0/1/2 at `raw >= 3` — **FIXED 44th pass**,
  `raw_fd_is_plain_stdio_device`, `process.rs`/`file.rs`) — both `cmd | reader &` and
  `VAR=$(external-cmd)` now genuinely work. `cmd 2>&1 | sed 's/^/[tag] /' &` remains the pattern to
  use for streaming output. Full mechanism: archive.
- **`.wfgy/webtop_stack.sh` is NOT what boots — `.wfgy/webtop_seed.tar` embeds a FROZEN COPY**
  (`--resume-from`), so editing the host script alone changes nothing. Re-tar after every edit
  (`tar -xf` to a stage dir, overwrite, `tar -cf webtop_seed.tar webtop_stack.sh tmp config`) and
  verify with `tar -xOf ... | grep`. Found the hard way: the 35th pass's `/dev/tcp` rewrite was
  still absent from the tar on the 38th pass — never once ran in a guest.
- **A boot whose log stops is usually a DEAD ROOT RUNNER, not a hang** — the root process hosts
  the top-level shell, so when it dies the `[s]` markers stop while orphaned cross-process children
  (Xvfb, selkies) keep running/burning CPU, reading exactly like a stall. Diagnose in one command:
  `Get-CimInstance Win32_Process -Filter "Name='litebox_runner…'" | ForEach-Object {
  $_.CommandLine.Length }` — every cross-process CHILD has the bare 77-char exe-only command line
  (`process_fork.rs:1594-1599`), so **if no survivor carries the full `--oci-image …` arg list, the
  root is gone** (RAM pressure → OOM-kill).
- **Before ANY `cdb` attach, set `LITEBOX_DIAG_NO_EXTERNAL_FAULT_WATCHDOG=1` and
  `LITEBOX_DIAG_NO_FAULT_WATCHDOG=1`.** Every runner spawns a watchdog CHILD (`process_fork.rs:4391`)
  that `TerminateProcess`es after 15s of <10ms CPU delta with no "was a fault armed" precondition —
  a debugger-frozen process makes zero progress, so it gets killed ~15s in. Kill the target's
  watchdog first on an already-running boot (small ~10-20MB, same parent).
- **`DIAG_TIMELINE`/`sys_execve` log at `debug!`, NOT `error!`** despite their own comments
  claiming otherwise — use `LITEBOX_LOG=warn,litebox_shim_linux::syscalls::process=debug,
  litebox_platform_windows_userland::fork_verify=error`. A cross-process fork child adopts its own
  **Windows PID as its guest pid** (`runner…/lib.rs:1673`), so a large `pid=` on a `DIAG_TIMELINE
  execve` line is a real host PID `cdb -pv -p` accepts directly.
- **On host-side crashes, use `advisor/probes/symbolize_litebox_crash.py`, snapshotting `.exe`+`.pdb`
  next to the log** — a ring dump's `rva=` is only meaningful against the exact emitting build.
- **Isolate the harness before blaming litebox** — launch guest probes directly as the runner's
  top-level program, never via a runtime-built `/bin/sh -c` wrapper. Never trust a container tag
  name for its WM/session contents — verify by registry manifest + blob tar-listing or a live
  in-guest `/usr/bin` listing. Never record a test count not watched run to completion; never
  leave a suite red for an environmental reason.
- **Repo hygiene** — packed layer tars, frame dumps and debug logs never go in git (`.wfgy/`,
  gitignored); untrack anything `git add -A` sweeps.
- **A freestanding no-libc probe's local `char buf[N] = "literal"` array initializer can crash** —
  clang `-O1` lowers it to an aligned SSE `movaps`, and a hand-written `_start` doesn't always give
  the same alignment guarantee real crt0 does. Use a manual byte-copy loop instead (37th pass,
  `advisor/probes/pty_fork_probe.c`'s own `copy_str`).
- **Guest-reachable code returns an errno, never a panic** — the host process IS the entire guest
  session, so an `unimplemented!()`/`unreachable!()`/panic, or unbounded recursion, on any
  guest-reachable path kills every guest process at once. Bitten many times (OOM, metadata ops, open
  flags, nested `epoll_ctl`, corrupted guest contexts); full fixed-bug list with shas: archive.

## Cross-process fork (`LITEBOX_PROCESS_FORK=1`)

A genuine `D == 0` fork — child at the SAME addresses, no relocation, no `fork_verify` healing —
exists as `spawn_cross_process_fork_child` (`advisor/ADVISORY-002-d-zero-fork.md`), short-circuiting
to a native fork when available (the fd-carrying apparatus is Windows-only scaffolding for a
missing syscall). **Correctness-sound**: zero corruption on a `bash -c` loop repro vs the
thread-based default's 100% tcache-corruption rate (ADVISORY-001 §3N is thread-path-only).

**Eligibility** — an already-borrowed fd table, a beyond-stdio fd that isn't a pipe end/path-recorded
regular file/eventfd/close-on-exec/pty (overridable by `LITEBOX_PROCESS_FORK_IGNORE_FDS`), or an
unsanitizable `fs_base`/context. No by-name gate exists (34th pass) — only this global opt-in env
var plus the per-fork fd-kind scan. On a real `debian-xfce` boot the only remaining blocking kind
is `unix-socket`. Per-fork cost was ~3.5-5s, now ~1.2s (`LITEBOX_DIAG_FORK_TIMING=1`). Still open:
nginx's own SSL-cert generation fails on its first startup attempt, genuinely not root-caused
(`docs/track-b-fork-fix-progress.md:146-152`).

**Pass history (4th-43rd, 2026-09-17/22)**: full narrative for every pass is in the dated archives
(see "Docs and tooling map" below). The CURRENT STATE those passes converged on:

- **The Xvfb SIGSEGV is FIXED — 43rd pass, `01f8532`.** Root cause: `get_unmmaped_area`'s top-down
  search forecloses the WHOLE upper address region when the topmost existing VMA already reaches
  past `high_limit`, packing an execve'd process's libraries into a crowded low window with
  inter-library gaps as small as one page — a fix for exactly this (`linux.rs:2439-2510` step 1.5,
  walk down from `high_limit` past occupying mappings instead of giving up) was written and
  deliberately left `if false`d back in the 6th pass (`75eb781`), never live-tested against this
  crash until now. Same-session A/B (`.wfgy/xvfb_pass43_*`, `LITEBOX_PROCESS_FORK=1`): control
  (step 1.5 disabled, every prior pass's real baseline) crashes at the bit-identical wild pointer
  `0x7feffecdd400` within one `WM_POLL` cycle, matching all 8+ prior captures; fix enabled, two
  isolated `de_only.sh` runs (one the full 60s window to `DE_FAILED after 60s`) plus two full
  release-binary `webtop_stack.sh` boots (`NGINX_STARTED`→...→`DE_LAUNCHED`→`DE_FAILED`) all show
  ZERO `Segmentation fault`/SIGSEGV. **`DE_FAILED` itself is UNCHANGED and is now the SOLE remaining
  blocker** (see its own bullet below) — fixing the crash was never expected to also fix it (38th
  pass's own A/B already showed one bug seen from two ends). Do NOT reopen DISPLAY/`getenv()`/
  loader-stack (proven correct, 30th pass) or attach `cdb` to `xfce4-session` for the CRASH — it was
  never the faulting process. Full A/B evidence, every log path: archive.
- **`DE_FAILED` (the WM never announcing `_NET_SUPPORTING_WM_CHECK`) is now cleanly isolated from
  the (fixed) crash and is the current top blocker** — narrowed since the 30th pass to "something
  inside `xfce4-session`'s own process" (envp/`getenv()`/ELF-loader-stack all proven correct by
  direct evidence). **Never attempted by any pass**: a live `cdb -pv` attach on `xfce4-session`
  itself, breaking on `getenv`/`XOpenDisplay`/`_XConnectXCB` — now finally safe to try without the
  Xvfb-crash confound racing it.
- **Xvfb's crash backtrace was corrupted PROJECT-WIDE, ROOT-CAUSED (38th) and FIXED (39th,
  `7d66935`).** `litebox_syscall_rewriter` overwrote the exact 9 bytes libunwind's x86_64
  signal-frame detection matches at `__restore_rt`, desyncing every guest backtrace through any
  delivered signal. Fix: signal delivery now returns through a litebox-synthesized trampoline
  holding the real glibc bytes verbatim. Verified end to end.
- **38th-42nd passes' live-cdb-capture saga is now MOOT** (the fix landed via non-debugger
  same-session A/B instead of resolving the crashing call) but its own hard-won conclusion stands
  as a standing lesson: **live debugger capture of a real litebox guest crash can be fundamentally
  infeasible even fully fixed** — the 42nd pass's six-bug-fixed conditional-breakpoint session ran
  an entire normally-100%-crashing window with zero hits, because sustained AV-interception rate
  (not per-event cost) perturbs whatever real wall-clock pacing the crash needed. Do not re-attempt
  live-cdb capture of a suspected-timing-sensitive crash without a non-`WaitForDebugEvent`
  mechanism (untried: kernel ETW). Full six-bug writeup, pointer-provenance derivation: archive.
- **The "Fork-after-Xorg PERMANENT freeze" risk is CONFIRMED GONE** (35th pass: a full
  `LITEBOX_PROCESS_FORK=1` release-binary boot reached its `HOLD` loop with zero
  freeze/SIGSEGV/tcache-corruption). `LITEBOX_PROCESS_FORK=1` is now RECOMMENDED for
  `.wfgy/webtop_stack.sh`. That pass also rewrote `SELKIES_PORT_UP`/`NGINX_SELFTEST` from a
  170x-`curl`-fork loop (the likely OOM-kill cause) to a zero-fork `/dev/tcp/HOST/PORT` check —
  **38th-pass correction: the rewrite had never once executed** (`webtop_seed.tar` carried the
  stale copy); regenerated, the gate now runs but reports `NGINX_SELFTEST_FAILED` (empty
  `last_code`) — **still not proven working**, separate from the desktop question.
- **`pty_registry`/`daemon_pty_masters` cross-process redesign is DONE, genuine cross-process pty
  I/O LIVE-PROVEN** (36th: `syscalls::pty::SharedPtyTable`; 37th: `advisor/probes/pty_fork_probe.c`
  proved a genuinely separate Windows fork child opening `/dev/pts/<id>` fresh and reading a marker
  the parent wrote post-`fork()`; fixed two real bugs: a fork-eligibility scan refusing the whole
  fork over any open pty fd, and `PtyStateRef::Local` setters never mirroring lock/termios state
  into the shared slot). Mechanism: "Shared-memory foundations" below.
- **44th pass (2026-09-22) — two real bugs FIXED+verified (fd 0/1/2 dropped at the cross-process
  fork boundary; a premature AF_UNIX connect-request cancellation that stalled `xfce4-session`
  forever on its own D-Bus connect).** With both fixed, `xfce4-session` genuinely progresses for
  the first time ever -- `iceauth`/`ssh-agent`/`gpg-agent`/`xfconfd`/
  `dbus-update-activation-environment` all `execve` (none appear in any prior pass's log). Before
  `xfwm4` is reached, Xvfb SIGSEGVs again at fault address `0x4000400` (backtrace frame0 offset
  `0x1b20ed`, bit-identical to the 43rd-pass-fixed crash's own signature) --
  `.wfgy/de_only_pass44_run2.log:46383-46392`. Full detail, exact fix diffs, GLib-CRITICAL noise
  analysis: archive.
- **45th pass (2026-09-22) — TWO real host-process bugs found+fixed (`5d63ec6`, `32dd3d5`); the
  Xvfb `0x4000400` SIGSEGV was NOT reproduced across 2 post-fix boots (was present in 1 of 2
  pre-fix boots), suggestive but NOT proven fixed; a separate, still-OPEN third host-process-panic
  mechanism now blocks full confidence either way.** Root-caused and fixed a DIFFERENT,
  previously-undiagnosed bug hit in the SAME 44th-pass logs: a HOST-process Rust panic
  (`litebox_platform_windows_userland/src/lib.rs:7396`, `process_memory_range_by_regions`'s own
  `assert!`) firing inside short-lived cross-process-fork children (`ssh-agent`, `xprop`, etc.) at
  the bit-identical region `0x7fef60030000-0x7fef64000000`, Windows reporting `MEM_FREE`. (1)
  `allocate_pages`'s collision checks never accounted for the shared kernel heap's `SEC_RESERVE`
  fallback view -- fixed, but tested+REFUTED as the cause of this specific address (every process
  in the crashing fork tree lands its shared kernel heap at the FIXED base, not the fallback); kept
  as a real, independent fix. (2) `Vmem::new_adopting_existing_memory` adopted a `VM_SHARED` region
  into a cross-process fork child's `vmas` with `shared_handle: None`, which
  `Vmem::remove_mapping`'s `shared_overlaps` check (keyed on `VmArea::view_extent()`, `None`
  whenever `shared_handle` is `None`) then misclassified as ordinary PRIVATE memory, routing a
  later guest `munmap`/`mprotect` straight into `deallocate_pages`/`update_permissions`'s real
  Windows calls against an address this child never actually committed real memory at
  (`copy_one_group`/`group_relocations` never recreates `VM_SHARED` backing for a Windows
  cross-process-fork child at all). Fixed by skipping `VM_SHARED` regions at adoption entirely --
  real cross-process content sharing for them remains unimplemented. **After BOTH fixes, the
  bit-identical `lib.rs:7396` panic on the SAME address STILL recurred** (during `ssh-agent`'s own
  exit) -- a THIRD, still-unidentified mechanism also produces it; a follow-up diagnostic boot
  (`LITEBOX_DIAG_PROCESS_FORK_EXEC_FIXUP=1`/`LITEBOX_DIAG_MM=1`, to compare
  `0x7fef60030000` against real `copy_one_group` reservation-group boundaries) failed to even
  launch this pass (Windows file-lock contention against a leftover process, host RAM down to
  ~3-4.5GB free) and was not re-attempted. **DE_FAILED was reached in every run this pass (4/4); no
  run reached a working window manager; no browser/app verification was possible.** Full evidence,
  log line numbers, exact repro commands: archive. Pickup, in order: (1) re-run the diagnostic boot
  once RAM is quiet to find whether `0x7fef60030000` falls inside any printed reservation-group
  range (if not: a second, more general `group_relocations()`/`vma_layout()` coverage gap, same
  shape as the `VM_SHARED` one but not limited to shared regions); (2) once that panic is fully
  closed, re-verify from scratch whether the Xvfb `0x4000400` SIGSEGV is actually gone (2 clean
  runs is not enough given this crash class's own documented high determinism -- do not declare it
  fixed on this evidence alone); (3) only then does real browser/app verification become
  meaningful.

### Track B — current pickup list, precise (full pass-by-pass evidence: archive)

Fully DONE (kept only as a marker so a future pass doesn't re-attempt): the minimal isolated
cross-process AF_UNIX repro; the `Network` shared-arena redesign's `socket_set`/
`LocalPortAllocator`/`closing_in_background`/`queued_for_closure` slice; DISPLAY/`getenv()` as the
`DE_FAILED` cause (REFUTED FOR GOOD); AF_UNIX `connect()` `EAGAIN`-vs-`EINPROGRESS`; `pty_registry`/
`daemon_pty_masters` (`syscalls::pty::SharedPtyTable`, now live-verified cross-process, 37th pass);
cross-process fork's fd-eligibility scan dropping a redirected 0/1/2 (44th pass,
`raw_fd_is_plain_stdio_device`); `SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap
(44th pass, `UnixStreamState::Connecting`, item 5 below — DONE, not just scoped).

**Open, in rough priority order:**

1. **`DE_FAILED`'s ORIGINAL cause (the D-Bus non-blocking-connect stall) is FIXED — 44th pass** (see
   its own entry above). A SECOND, deeper blocker was exposed by that fix and is now the single top
   blocker: a fresh Xvfb SIGSEGV (fault `0x4000400`, backtrace frame0 offset `0x1b20ed` —
   bit-identical to the 43rd-pass-fixed crash, so almost certainly the SAME wild-pointer-read defect
   via a trigger condition step 1.5's crowded-top-down-packing fix doesn't cover) that kills Xvfb
   while `xfce4-session`'s pre-session setup (`iceauth`/`ssh-agent`/`gpg-agent`/`xfconfd`, all
   reached for the first time ever) is still running, before `xfwm4` is ever attempted. Needs its
   own root-cause pass, same non-debugger A/B methodology as the 43rd pass's own (`cdb` attach is
   the SAME "sustained AV-interception perturbs crash timing" trap the 41st-42nd passes already
   proved infeasible for this crash family — do not re-attempt without a non-`WaitForDebugEvent`
   capture mechanism).
2. A new, unscoped observation from the 43rd pass, not yet investigated: host-side
   `curl http://localhost:8081/` failed on both full-stack boots despite the GUEST's own
   `/dev/tcp` self-test succeeding — possibly just the same transient RAM dip the 35th pass already
   documented at this exact script stage, possibly a real `--publish` gap. Reproduce with RAM held
   above ~2GB throughout before concluding either way.
2b. **AF_UNIX cross-process tables have FOUR silent exhaustion paths, none logging anything**
   (38th-pass static audit, `litebox_shim_linux/src/syscalls/unix.rs`): `SharedUnixAddrPresenceTable`
   capacity 256's `insert` return DISCARDED at `unix.rs:275-277` (over-capacity `listen(2)` still
   succeeds, clients later get `ECONNREFUSED`); a key >108 bytes silently bails in
   `insert`/`post`/`try_claim`/`has_pending`; `SharedUnixConnectQueue` capacity 64 returns `EAGAIN`;
   `SharedUnixConnTable` capacity 64 leaves a request `REQ_CLAIMED` FOREVER (`unix.rs:430-435`,
   `cancel` only CASes `REQ_PENDING→REQ_EMPTY`) — a monotonic slot leak for the fork family's life.
   Also `SHARED_UNIX_CONN_BUF` is only 2048 bytes/direction; cross-process accept ignores the
   listener backlog entirely (`unix.rs:420-452`). Abstract sockets checked and CORRECT.
3. `SafeZoneAllocator`'s `spin::mutex::SpinMutex` (`litebox/src/mm/allocator.rs`) needs the same
   dead-holder-recovery treatment `RawMutex` already has — live-caught spinning forever in
   `dealloc`, high blast radius, own dedicated pass.
4. Debugger-root-cause `litebox/src/event/wait.rs:224`'s `unreachable!()` on garbage thread state
   (dozens per boot, most frequent panic historically, NOT yet debugger-confirmed — do not patch
   blind).
5. ~~`SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap~~ — **FIXED, 44th pass**
   (`UnixStreamState::Connecting`, see its own entry above).
6. `flock_registry`/`drm`/`evdev` (`GlobalState` fields, eighteenth-pass audit) remain open — same
   non-POD-payload obstacle pty's own `PtyEnd::Shared{Master,Slave}`/`SharedPtyTable` pattern gives
   a concrete template for, not yet applied; not on the Xvfb/selkies boot path, lower urgency.
7. `timerfd`/`signalfd` are the next-cheapest carriable fd kinds before `socket`/`unix-socket`/
   `epoll` (`pty` is no longer purely uncarriable — a cross-process opener can re-acquire one by id
   via `pts_open`'s shared fallback even though the fd itself isn't carried across `fork()`).
8. The writable-layer-visibility gap for LARGE/unbounded content (`/tmp/de.log`/`/tmp/de2.log`/
   `/tmp/xvfb.log`, `/tmp/wm1`/`/tmp/wm2`) remains open — `SharedFilePublishTable`'s 256-byte cap
   must NOT be widened to cover it; needs its own design (chunked publish or a shared-arena ring).

## Container images and OCI loading

**`litebox_packager --oci-image <ref> --output <tar>`** pulls, whiteout-merges, rewrites every ELF and
produces a bootable flat tar in one command — supersedes the ad-hoc OCI-pull Python scripts this
project once hand-rolled, retired, do not recreate.

**Runtime in-memory loading** — `--oci-image <ref>` pulls, merges and rewrites every layer in memory;
no host directory is ever created for the rootfs (a real one hit three Windows-path bugs).
Rewritten layers cached under `.litebox-cache/`, keyed so a rewriter change self-invalidates.
`tar_ro.rs`'s multi-layer index is built ONCE at mount, not per read (was O(entries²), 17.3s →
0.35s fixed). Cache internals, the four fixed OOM bugs, tag-verification detail: archive.

Tags verified live, never from the name: `linuxserver/webtop:alpine-mate` ships MATE not XFCE;
`alpine-xfce` doesn't exist; `debian-xfce`/`ubuntu-xfce` ship real XFCE.

**X server choice**: for the DRM/wgpu on-screen (`--gui`) path use `Xorg` with `modesetting` —
litebox's virtual DRM device is legacy-KMS + dumb-buffer + XRGB8888 only, no atomic modeset/GBM/EGL,
so a GBM-first compositor lands on its least-tested fallback, and `Xvfb` never touches DRM/KMS at all
(zero page-flips). For browser/selkies, `Xvfb` IS correct — its `-shmem` framebuffer works now that
SysV shared memory exists.

**Durable artifacts**: `C:\dev\litebox-webtop\webtop_seatd.tar` (stock MATE webtop); the
`.wfgy/xfce-build/` weston+XFCE tar is superseded by the stock-image path.

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
the old "Fork-after-Xorg" freeze either (35th pass) — the Xvfb SIGSEGV that used to be the real
current blocker on EITHER fork path is FIXED (43rd pass, see "Cross-process fork" above);
`DE_FAILED` is now the sole remaining blocker to an interactive desktop. Selkies also needs
`--clipboard-enabled=false` on the thread-based path (its clipboard monitor re-triggers the same
corruption every tick) — moot cross-process.

**Open here.** One client per selkies instance, no slot reclaim on reload. A SECOND, distinct
glibc/tcache corruption signature (`double free or corruption (out)` SIGABRT) still sporadically
hits selkies on the THREAD-based fork path under heavy fork load — Track B territory, not a
tunable-coverage gap; do not re-attempt `GLIBC_TUNABLES` without evidence of a THIRD mechanism.

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
`LiteBoxX`/`GlobalState` placement (NOT wired to `GlobalAlloc`; `SLAB_ALLOC` stays
private-per-process). **Root cause of the whole `GlobalState`-sharing class**: `SharedArc::new`
shares only `T`'s literal inline bytes -- a `BTreeMap`/similar registry has its NODES on the
private per-process heap, meaningless to an attaching process. Of the original uncarriable-registry
list (`unix_addr_table`/`pty_registry`/`daemon_pty_masters`/`flock_registry`/`fifo_registry`/
`sysv_shm`/`memfds`/`shared_files`): all but `flock_registry` are fixed (per-process-shadowed, a
shared-arena fixed array, or — for `pty_registry`/`daemon_pty_masters` — both a shadow AND a
live-verified cross-process companion, `syscalls::pty::SharedPtyTable`). Reusable pattern
(`SharedUnixAddrPresenceTable`, reused by `sysv_shm`/AF_UNIX/`SharedPtyTable`): fixed-slot,
pure-atomic, lock-free `(kind, key bytes<=108, owner pid)` side-index. **A mutable-state table on
this pattern needs every WRITE path audited for shared-side mirroring** — `SharedPtyTable`'s own
setters originally only reached the local side, silently missing the shared slot until a live
cross-process test caught it (37th pass). Still open: `flock_registry` (same obstacle, pty's
pattern is now a template); `SafeZoneAllocator::alloc`'s spinlock livelock (no dead-holder
recovery unlike `RawMutex`).

## Closed — do not re-attempt without a genuinely new approach

VEH_FRAME_STRIDE canary guard, `dev_bench`/`litebox_runner_snp` build failures, CoW-mmap
performance, input-latency bugs, presenter-split duplicate-`SYN_REPORT`, the GUI-protocol
decision, five cheap-wins PRD rows, cross-process-fork stdio-handle bug (`spawn_suspended`'s
clobbered `STARTF_USESTDHANDLES`), presenter-process split (`docs/presenter-process-design.md`)
— all CLOSED, none open. Full detail: archive.

## Docs and tooling map

- **Archives** (newest first) — `_2026-09-22.md` (26th-43rd passes: full pass-by-pass narrative for
  everything this file's own pass entries above summarize, including the 43rd pass's Xvfb-SIGSEGV
  fix A/B evidence, the 42nd pass's six-bug conditional-breakpoint capture writeup and the
  38th-40th passes' Xvfb-crash-characterization narrative), `_2026-09-18.md` (12th-34th, shared
  AF_UNIX connection plane, ldconfig static-PIE fix), `_2026-09-17.md` (shell-crash investigation,
  stdio-handle bug, writable-layer-race fix),
  `_2026-09-16.md` (Track A audit, RawMutex/presenter), `_2026-09-15.md` (ACK-stall-kill),
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
