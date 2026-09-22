# litebox — current state (2026-09-22)

The authoritative CURRENT-STATE picture of what works, what is broken, and what to do next. Every claim
carries a commit sha or `file:line` so the next session re-verifies instead of re-deriving; a claim
nobody could point at, and a claim a later commit superseded, were deleted rather than hedged. Reference
detail is drained to the `docs/AGENTS_ARCHIVE_*.md` files and per-investigation logs to the dated
`docs/*.md` in the map below — read those for a trail, never as a starting point.

Also the single source of truth for standing rules. A future "remember this" belongs here as one
line plus its pointer, not a separate memory file. **Compacted past ~30KB** — newest: 2026-09-22,
51st pass (trimmed the 49th-pass narrative to make room for the finding below).

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
  leave a suite red for an environmental reason.
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
var plus the per-fork fd-kind scan. On a real `debian-xfce` boot the only remaining blocking kind
is `unix-socket`. Per-fork cost was ~3.5-5s, now ~1.2s (`LITEBOX_DIAG_FORK_TIMING=1`). Still open:
nginx's own SSL-cert generation fails on its first startup attempt, genuinely not root-caused
(`docs/track-b-fork-fix-progress.md:146-152`).

**Pass history (4th-47th, 2026-09-17/22)**: full narrative for every pass is in the dated archives
(see "Docs and tooling map" below). The CURRENT STATE those passes converged on:

- **The ORIGINAL Xvfb SIGSEGV is FIXED — 43rd pass, `01f8532`** (`get_unmmaped_area`'s top-down
  search forecloses the whole upper region once the topmost VMA reaches `high_limit`, crowding
  libraries into a gap-starved low window; a fix written+`if false`d in the 6th pass, `linux.rs`
  step 1.5, was finally live-tested and closes it). Same-session A/B, control crashes at the
  bit-identical wild pointer `0x7feffecdd400` every time, fix shows zero SIGSEGV. Do NOT reopen
  DISPLAY/`getenv()`/loader-stack (proven correct, 30th pass) for THIS crash. A SECOND, later Xvfb
  crash (same signature, different trigger, a SysV-shm cross-process-attach bug) is ALSO now fixed
  — 51st pass, below.
- **Xvfb's crash backtrace was corrupted PROJECT-WIDE, ROOT-CAUSED (38th) and FIXED (39th,
  `7d66935`)**: `litebox_syscall_rewriter` overwrote the 9 bytes libunwind's x86_64 signal-frame
  detection matches at `__restore_rt`. Fixed via a litebox-synthesized trampoline holding the real
  glibc bytes verbatim.
- **Live `cdb`/`WaitForDebugEvent` capture of a timing-sensitive guest crash was proven infeasible
  38th-42nd pass** (sustained AV-interception rate perturbs the crash's own wall-clock pacing; a
  six-bug-fixed conditional-breakpoint session ran an entire normally-100%-crashing window with
  zero hits) — **narrower than it sounds**: this is specifically about `cdb`, not about live capture
  in general — `LITEBOX_DIAG_FATALDUMP=1` (VEH-based, in-process, no debugger) DOES capture this
  crash family without reliably suppressing it, confirmed 46th pass (below).
- **"Fork-after-Xorg PERMANENT freeze" is CONFIRMED GONE** (35th pass, a full
  `LITEBOX_PROCESS_FORK=1` release boot reached `HOLD` with zero freeze/SIGSEGV/corruption).
  `LITEBOX_PROCESS_FORK=1` is RECOMMENDED for `.wfgy/webtop_stack.sh`.
- **`pty_registry`/`daemon_pty_masters` cross-process redesign is DONE, genuine cross-process pty
  I/O LIVE-PROVEN** (36th: `syscalls::pty::SharedPtyTable`; 37th: `advisor/probes/pty_fork_probe.c`
  proved a separate Windows fork child opening `/dev/pts/<id>` fresh post-`fork()`). Mechanism:
  "Shared-memory foundations" below.
- **44th-45th passes (2026-09-22)** — `xfce4-session` reached real pre-session setup for the first
  time (`iceauth`/`ssh-agent`/`gpg-agent`/`xfconfd`), then hit a second Xvfb SIGSEGV (`0x4000400`,
  same signature as the 43rd-pass-fixed crash) before `xfwm4`. 45th pass fixed two real host-process
  bugs (`5d63ec6`, `32dd3d5`) chasing a separate host-process panic (`lib.rs:7396`, next bullet).
  Full narrative: archive.
- **The `lib.rs:7396` host-process panic (`process_memory_range_by_regions`'s `assert!`) is FIXED —
  46th pass, `c5a8884`.** `Vmem::new_adopting_existing_memory` (`litebox/src/mm/linux.rs`) adopted
  `PROT_NONE` regions into a cross-process fork child's `vmas` even though `do_clone`'s own
  copy-plan loop already excludes them from every copy group by design — a later guest
  `munmap`/`mprotect` on the untracked-but-adopted region routed into a real `VirtualFree` call
  against memory the child never committed. Same fix shape as the 45th pass's `VM_SHARED` skip:
  excluded from adoption entirely. **Live-verified: 0 panics across 5 post-fix boots.** Diagnostic
  pitfall worth recording: a cross-process fork CHILD re-execs this binary but takes `main()`'s
  `is_diagnostic_resume_child()` branch and NEVER reaches `run()` — a runtime toggle wired only at
  the top of `run()` silently never takes effect in any fork descendant.
- **The `buddy_system_allocator` free-list corruption panic ("len is 34, index is 53") is FIXED —
  49th pass, `8b64698`.** `Network`/`Pipes::rebind_per_process_fields` (`litebox/src/net/mod.rs`,
  `litebox/src/pipes.rs`) plain-assigned their shared-arena `litebox` field, dropping the OLD value
  in place — always some OTHER process's private-heap `Arc<LiteBoxX>` pointer frozen into shared
  bytes, so `Arc::drop` read a bogus refcount and freed an unrelated, still-live FD-table `Vec`
  through THIS process's allocator with a garbage `Layout`, corrupting `Heap::free_list`. Fix:
  `mem::forget` the stale value, mirroring `reset_after_poisoning`'s established pattern for the
  identical hazard on `SocketSet::remove`. **Verified 7/7 clean `de_only.sh` boots**, zero
  recurrence vs. 100% reproduction pre-fix. Full mechanism/backtrace: archive.
- **50th pass** — the second Xvfb SIGSEGV's full register state proven bit-identical across
  independent boots: `rip=0x7fefedeababd` (glibc `__memmove_avx_unaligned_erms`'s 32-64-byte AVX2
  path), `rdx=0x40`. `rdi` (dest) is a real per-process heap chunk (offset fixed, base moves with
  ASLR, as expected); `rsi` (src, fault address `0x37f0400`) does NOT move with ASLR at all — the
  key clue the 51st pass root-caused (next bullet). Live `cdb` was NOT re-attempted this pass
  (skipped, not merely deferred: the 41st-42nd passes already proved sustained `cdb`
  AV-interception fully suppresses this whole crash *family* — same dispatch point, adjacent `rip`
  — regardless of how cheap each breakpoint handler is, and static+`LITEBOX_DIAG_FATALDUMP=1`
  evidence alone was sufficient to root-cause without paying that cost).
- **51st pass (2026-09-22) — SysV shm cross-process-attach bug ROOT-CAUSED and FIXED, no `cdb`
  needed.** `sys_shmat` (`litebox_shim_linux/src/syscalls/mm.rs`) handed back a bare `SysvShmSegment.addr`
  — a real mapping ONLY in the CREATING process — to every attacher; under
  `LITEBOX_PROCESS_FORK=1`, a genuinely different real Windows process (e.g. Xvfb attaching a
  segment an X11 MIT-SHM CLIENT created, never fork-related to Xvfb) got that numeric value with
  ZERO backing memory of its own, exactly matching `rsi`'s "fixed, non-ASLR'd" signature (a value
  copied verbatim out of the shared `GlobalState` table, not derived from this process's own mmap
  placement) and `LITEBOX_DIAG_FATALDUMP=1`'s own `[veh] fault addr=0x37f0400 protect=0x0` (reserved,
  never committed, in THIS process). Old design assumed the pre-cross-process-fork "one shared host
  address space" model, stale since `LITEBOX_PROCESS_FORK=1` existed. **Fix**: `shmget` no longer
  maps anything (matches real Linux); every `shmat`, including the creator's own first one, opens a
  NAMED platform shared-memory object (`create_named_shared_memory`, new `PageManagementProvider`
  method, Windows impl = `CreateFileMappingW(name="Local\litebox_sysvshm_<shmid>")`, idempotent
  create-or-open exactly like `xproc_sync::CrossProcessEvent`'s existing named-kernel-object
  pattern) and maps it into ITS OWN address space via the existing `map_existing_shared_pages`
  primitive (same one `syscalls::file`'s memfd/`wl_shm` bridging already uses) — the returned
  address is per-process, matching real Linux `shmat` (never cross-process-identical there either).
  `shmdt`'s reverse lookup moved to a new per-process `FilesState::shm_attachments` table (shares
  correctly via the existing `CLONE_FILES`-vs-`fork_duplicate` split `FilesState` already has).
  **Verified**: 10/10 clean `de_only.sh` boots (`LITEBOX_PROCESS_FORK=1`, `LITEBOX_DIAG_FATALDUMP=1`,
  debug build; 7 short + 3 reaching WM_POLL n=7/60s with zero truncation-related ambiguity), ZERO
  recurrence of either Xvfb SIGSEGV signature (`0x7feffecdd400` or `0x37f0400`) — a background
  6-more-run batch was killed mid-flight by the harness's own low-host-RAM safety reaper (not a
  litebox issue; do not re-run unprompted per that reaper's own guidance). **NOT yet re-verified**:
  a full `webtop_stack.sh` boot (nginx+selkies, heavier RAM) past this fix, and whether `DE_FAILED`
  (a SEPARATE, already-narrowed-to-"inside xfce4-session's own process" issue, 30th/38th passes)
  now resolves on its own or needs its own still-never-attempted live `cdb -pv` attach on
  `xfce4-session` itself (43rd-pass pickup item 1, still open, not touched this pass either).

### Track B — current pickup list, precise (full pass-by-pass evidence: archive)

Fully DONE (kept only as a marker so a future pass doesn't re-attempt): the minimal isolated
cross-process AF_UNIX repro; the `Network` shared-arena redesign's `socket_set`/
`LocalPortAllocator`/`closing_in_background`/`queued_for_closure` slice; DISPLAY/`getenv()` as the
`DE_FAILED` cause (REFUTED FOR GOOD); AF_UNIX `connect()` `EAGAIN`-vs-`EINPROGRESS`; `pty_registry`/
`daemon_pty_masters` (`syscalls::pty::SharedPtyTable`, live-verified cross-process, 37th pass);
fork's fd-eligibility scan dropping a redirected 0/1/2 (44th, `raw_fd_is_plain_stdio_device`);
`SharedUnixConnectQueue`'s cancel-on-first-non-blocking-miss gap (44th, `UnixStreamState::Connecting`).

**Open, in rough priority order:**

1. **The SECOND Xvfb SIGSEGV (`0x37f0400`) is FIXED — 51st pass, see "Cross-process fork" above.**
   10/10 clean `de_only.sh` verification, not yet re-verified on a full `webtop_stack.sh` boot.
   `DE_FAILED` (`xfce4-session`'s own "Cannot open display", narrowed to inside its own process
   since the 30th/38th passes, DISPLAY/envp/loader-stack all independently proven correct) is now
   the priority: confirm whether it was purely downstream of this crash all along (in which case a
   fresh boot may already reach a real WM) or is a genuinely separate defect still needing the
   never-yet-attempted live `cdb -pv` attach on `xfce4-session` itself (43rd-pass pickup, still
   open). Host-side `curl http://localhost:8081/` connecting-but-HTTP-timing-out (43rd/49th passes)
   recheck depends on reaching a real WM first.
2. **AF_UNIX cross-process tables have FOUR silent exhaustion paths, none logging anything**
   (38th-pass static audit, `litebox_shim_linux/src/syscalls/unix.rs`): `SharedUnixAddrPresenceTable`
   capacity-256 overflow is silently discarded (`unix.rs:275-277`); a key >108 bytes silently bails;
   `SharedUnixConnectQueue`/`SharedUnixConnTable` (both capacity 64) leak a `REQ_CLAIMED` slot
   forever on cancel (`unix.rs:430-435`); `SHARED_UNIX_CONN_BUF` is 2048 bytes/direction; backlog is
   ignored entirely on cross-process accept. Abstract sockets checked and CORRECT.
3. `SafeZoneAllocator`'s `spin::mutex::SpinMutex` (`litebox/src/mm/allocator.rs`) needs the same
   dead-holder-recovery treatment `RawMutex` already has — live-caught spinning forever in
   `dealloc`, high blast radius, own dedicated pass.
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
0.35s fixed). Cache internals, the four fixed OOM bugs, tag-verification detail: archive.

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
the old "Fork-after-Xorg" freeze either (35th pass). BOTH Xvfb SIGSEGVs are now fixed (43rd/51st
passes); the current sole blocker to an interactive desktop is `DE_FAILED` itself (`xfce4-session`'s
own "Cannot open display", see Track B item 1 above) — not yet re-checked post-51st-pass on a full
`webtop_stack.sh` boot. Selkies also needs `--clipboard-enabled=false` on the thread-based path
(its clipboard monitor re-triggers the same corruption every tick) — moot cross-process.

**Open here.** One client per selkies instance, no slot reclaim on reload. A SECOND, distinct
glibc/tcache corruption signature (`double free or corruption (out)` SIGABRT) still sporadically
hits selkies on the THREAD-based fork path under heavy fork load — Track B territory, not a
tunable-coverage gap; do not re-attempt `GLIBC_TUNABLES` without evidence of a THIRD mechanism.

**ACK-stall-kill and port-8081 watchdog — both CLOSED (2026-09-16)**: real blocker was the
guest-side patcher silently crashing on `shutil.copy2()`'s `copystat()` (no `listxattr` shim)
before ever patching `selkies.py` (`478e640`); port-8081 double-bind fix live-verified over 17
boot cycles + a 6000-connection stress test. Detail: `docs/AGENTS_ARCHIVE_2026-09-16.md`.

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

- **Archives** (newest first) — `_2026-09-22.md` (26th-50th passes: full pass-by-pass narrative for
  everything this file's own pass entries above summarize), `_2026-09-18.md` (12th-34th, shared
  AF_UNIX plane, ldconfig static-PIE fix), `_2026-09-17.md` (shell-crash, stdio-handle bug,
  writable-layer-race fix), `_2026-09-16.md` (Track A audit, RawMutex/presenter), `_2026-09-15.md`
  (ACK-stall-kill), `_2026-09-10.md` (fork fd eligibility, OCI cache, s6-boot, crash-dump/VEH).
  Older: `_2026-09-03.md`, `_2026-09-05.md`.
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
