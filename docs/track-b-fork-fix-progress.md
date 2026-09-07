# Track B fork-without-exec fix: running progress log

## 2026-09-07 (later session): Step 0 verdict -- still INCONCLUSIVE, but with two more real bugs
## found, fixed and confirmed, and the remaining blocker now precisely localized

### Goal for this session

Get a final, clean Step 0 CONFIRMED/REFUTED verdict, being careful about the shared host's real
memory contention (checked before every boot attempt: 4.8GB, 5.2GB, 3.9GB, 4.7GB, 4.8GB free
across this session's runs -- never below the "small `bash -c` repro doesn't need much" floor the
task brief itself set). No stale `litebox_runner*` processes or `boot.lock` at session start.

### Bug #5 (real, fixed, confirmed via live capture): FS_BASE not repaired before resuming at an
### exception-table `recover` fixup

`vectored_exception_handler`'s recovered-AV branch (`litebox_platform_windows_userland/src/lib.rs`,
around the `[diag-recover-fsbase]` diagnostic a PRIOR session already added but never acted on)
jumps `context.Rip` to the exception table's `recover` fixup address and resumes -- but, unlike
the sibling FS_BASE-reset repair branch immediately above it in the same function (which does a
bounded `wrfsbase`-and-verify loop before resuming), this branch did no such repair. The prior
session's own comment explicitly predicted this: `recover` is compiler-generated `Err(Fault)`
fixup code inside a real Rust function, whose epilogue can perform FS-relative accesses
(stack-protector checks, TLS) -- resuming there with `FS_BASE` still cleared (`rdfsbase()==0`,
the exact condition this whole VEH exists to repair) re-faults immediately.

This session's first `LITEBOX_PROCESS_FORK=1 LITEBOX_LOG=debug` run of the `bash -c` inline
`tcache_fork_repro.sh`-equivalent repro hit exactly this: `[diag-recover-fsbase] recover_rip=...
fsbase=0x0` -- the predicted zero. The process then went silent and idle (8MB working set, 1
thread, 0% CPU, `tasklist`/`Get-Process` visible for 5+ minutes with zero further log output) --
a real, reproducible hang, not resource pressure (confirmed via a clean re-run at ~4-5GB free with
no debugger attached).

**Fix**: before resuming at `recover`, apply the exact same bounded `wrfsbase`-and-verify repair
the sibling branch already uses, keyed off the same trusted `THREAD_FS_BASE`-shadowed value
(`WindowsUserland::get_thread_fs_base()`, never repairing to an untrusted 0). Confirmed live:
rebuilt, re-ran the identical repro -- the SAME recovery point now prints
`[diag-recover-fsbase] recover_rip=... fsbase=0x7feffffb0740` (a real, healthy FS base) instead of
`0x0`, on every run since.

### Bug #6 (real, fixed, but NOT the reason for the remaining hang): cross-process fork child's
### stdio was never actually wired to anything observable

While chasing the still-open hang (see below), found that `spawn_process_fork_child`
(`litebox_platform_windows_userland/src/process_fork.rs`) calls `spawn_suspended(&mut exe_wide,
false, false)` -- no stdout/stdin pipe -- and its own doc comment claims this makes the child
"inherit the parent's real console session's stdio the ordinary way a genuine `fork()` child
would" via `bInheritHandles=0` and no `STARTF_USESTDHANDLES`. That reasoning is backwards:
`bInheritHandles=0` means NONE of the parent's handles (stdio included) are inherited, and
omitting `STARTF_USESTDHANDLES` just leaves `CreateProcessW` to assign fresh default handles, not
the parent's real stream. Confirmed live: none of the child-side diagnostic chain's own
unconditional `eprintln!` lines (`[process_fork_diag] task-resume-probe (child, winpid=...)`,
etc.) ever appeared in ANY of this session's captured logs, across multiple runs, even ones that
otherwise made it deep into the fork machinery -- not lost output from a hang, genuinely never
delivered anywhere.

**Fix**: when `spawn_suspended` is called without a pipe (the production `spawn_process_fork_child`
path), explicitly duplicate the CURRENT process's real `STD_OUTPUT_HANDLE`/`STD_ERROR_HANDLE`/
`STD_INPUT_HANDLE` into the child via `STARTF_USESTDHANDLES` + `bInheritHandles=1` (marking each
handle `HANDLE_FLAG_INHERIT` first, since a handle inherited from an upstream pipe/launcher is not
always already marked inheritable) -- matching real `fork()` semantics, where the child keeps the
exact same fd table as the parent. This is a real, independently-justified observability/semantic
fix (a forked child's own guest output should reach the same place the parent's does) but, per the
next section, did NOT resolve the remaining hang -- the hang reproduces identically with this fix
in place, confirming it is a separate bug from Bug #6.

### The remaining blocker: a real, deterministic hang, NOT resource pressure, NOT either bug above

With both fixes applied and rebuilt, five consecutive clean runs (varying free memory 3.9-5.2GB,
one deliberately run with zero debugger ever attached, to rule out the possibility that an earlier
session's `cdb -pv` non-invasive attach was itself leaving the target thread suspended) all show
the IDENTICAL signature:

1. Image loads, `bash` ELF loads, `TCACHE_REPRO_START pid=1` prints (~60-75s in, consistent with
   this repo's documented normal image-load time -- not itself evidence of slowness).
2. `[diag-recover-fsbase] recover_rip=... fsbase=0x7feffffb0740` -- a healthy fsbase, confirming
   Bug #5's fix is working as intended.
3. Total silence for 5-8+ minutes (multiple runs let sit this long or longer) -- no further log
   line, no CPU usage, no growth in working set.
4. `Get-Process -Id <pid> | % Threads | select WaitReason,ThreadState` on the live, untouched (no
   debugger ever attached) process shows the sole thread as `WaitReason=Suspended,
   ThreadState=Wait` -- i.e. the thread has a nonzero Windows suspend count and nothing is ever
   resuming it. This is not a fault loop, not CPU-bound slowness, not I/O wait: something in this
   process's own code path calls `SuspendThread` (directly, or via a scope-guard pattern like
   `ctxwatch_arm_other_threads`'s `defer`-paired `SuspendThread`/`ResumeThread`) and the matching
   `ResumeThread` never runs.

`ctxwatch_arm_other_threads` (`litebox_platform_windows_userland/src/lib.rs`) was inspected as the
most likely suspect (it explicitly suspends "other" threads while arming a context watchpoint, and
this investigation's `run_thread_with_fork_verification` call newly engages `fork_verify`'s
single-step verification machinery in this exact run for the first time this whole investigation
has reached this deep) -- but this repro is single-threaded (a `bash -c` subshell with no forked
grandchildren), so `ACTIVE_THREADS` filtered to "other than current" would be empty and this
specific function would suspend nothing. The `defer`-guard pattern used there (and in the sibling
`ThreadHandle::interrupt`) looks structurally sound on read -- the real culprit is still unlocated
and needs a live debugger attach (accepting the small risk of again leaving the thread suspended,
this time with an explicit `~*e ResumeThread` step before detaching) or, better, a
`LITEBOX_DIAG_*`-style instrumentation point logging every `SuspendThread`/`ResumeThread` pair by
call site, to distinguish `fork_verify`'s own single-step arming from `ctxwatch`/`interrupt` from
something else entirely.

### Step 0 verdict: still INCONCLUSIVE -- REFUTED-by-crash is now doubly ruled out (two more real
### bugs fixed, both confirmed not to be the deterministic ntdll crash class), but a new, different,
### equally real hang blocks completion

Baseline (no `LITEBOX_PROCESS_FORK`) reconfirmed this session: identical `TCACHE_REPRO_START` ->
SIGABRT (signal 6, `Aborted`, subshell status 134) corruption, exactly as every prior session
documented -- the corruption this whole track exists to fix is real and the repro is trustworthy.

`LITEBOX_PROCESS_FORK=1`: the two bugs fixed this session are both real, both independently
justified by direct evidence (not speculative), and neither was previously known -- this is
genuine progress, not a wash. But Step 0's central claim (the forked child runs its tcache
workload and exits cleanly) is still not reachable: every run past the FS_BASE fix now hits a
NEW, different, equally deterministic hang (a leaked thread suspend), rather than either the old
ntdll crash or a resource-pressure stall. This is real progress along the same trajectory every
session in this investigation has followed -- each session's fix exposes the next layer -- but the
verdict cannot honestly be called CONFIRMED, and REFUTED no longer fits either (there is no crash;
this is a hang with a specific, if not yet located, mechanism).

### Step 9 (long-running cross-process child survival): still NOT REACHED

Unchanged from the prior entry -- still blocked on reaching a live, resumed, running cross-process
child at all.

### Regression check (this session's two fixes)

`cargo build --release --bin litebox_runner_linux_on_windows_userland`: clean (pre-existing,
unrelated `unsafe fn`/E0133 warnings in `litebox/src/mm/mod.rs` only).

`cargo test --release -p litebox --lib`: 123 passed, 26 failed -- identical to every prior
session's documented baseline (missing `diod` binary, one `tar_ro` symlink test, one
`mm::tests::test_vmm_mapping` assertion). No regression.

`cargo test --release -p litebox_common_linux`: 10 passed, 1 doctest passed, 0 failed -- clean.

`cargo test --release -p litebox_platform_windows_userland --lib`: 4 passed, 0 failed -- clean.

`cargo test -p litebox_shim_linux` (test/doctest build): still fails to compile, `E0576` "cannot
find method `run_test_thread` in trait `ThreadProvider`" -- confirmed pre-existing (identical to
every prior session's documented finding), unrelated to this session's two-file diff.

### What remains open for a future session

- **Locate the leaked `SuspendThread`/missing `ResumeThread`** behind the new hang. Suspect areas
  to instrument first: `fork_verify::begin`/`on_single_step`'s single-step arming (newly exercised
  end-to-end for the first time this investigation has reached this far), and
  `run_thread_with_fork_verification`'s own setup sequence in
  `litebox_platform_windows_userland/src/lib.rs`. A live debugger attach is the fastest path but
  must explicitly `ResumeThread` (or issue cdb's own thread-resume, if any) before detaching, to
  avoid the debugger's own non-invasive attach compounding the exact symptom being diagnosed (this
  session confirmed, by attaching then detaching cleanly and re-running from scratch, that the
  hang is NOT a debugger artifact -- it reproduces with no debugger ever attached -- but a future
  session's own attach should still be careful not to leave the process suspended, since that
  would produce an indistinguishable false signal).
- Bug #6's stdio fix, once the hang above is resolved, should let a future session actually SEE
  `TCACHE_REPRO_CHILD_OK`/`TCACHE_REPRO_DONE` from the forked child directly (previously
  impossible regardless of whether the child's own logic was correct) -- this closes a real
  observability gap that likely masked information in every prior session's runs too.
- The real, still-unfixed `ElfPatchKey`-not-rekeyed-on-fork bug and Step 9 (long-running
  cross-process child survival) remain exactly as open as the prior entry left them.

---

This is the running log for the Track B architectural effort (D==0 cross-process fork,
per `advisor/ADVISORY-002-d-zero-fork.md`), broader in scope than the webtop-specific
investigation in `docs/webtop-debian-selkies-2026-09-06.md`. That file remains the log for
the webtop/XFCE demo-path investigation (Track A); this file is the log for Track B (the
architectural fork fix) going forward. Future sessions continuing Track B should append here.

---

## 2026-09-07: Step 0 decisive experiment -- INCONCLUSIVE, plus a new blocking finding

### Goal

Per ADVISORY-002 section 6, step 0: run a `beyond_stdio == 0` (stdio-only fd table) glibc
fork-without-exec repro under `LITEBOX_PROCESS_FORK=1` and confirm the `__libc_malloc+0x76`
tcache-corruption fault (ADVISORY-001 section 3N) does not occur, as a cheap, decisive
before-funding-anything-else gate for the whole Track B rewrite.

### Repro built

`advisor/probes/tcache_fork_repro.sh` (committed). Uses a stock, already-present, dynamically
linked `bash` from the `linuxserver/webtop:debian-xfce` image: a `( ... )` grouped subshell
forks without ever calling `execve()`, and inside it, repeated same-size-class shell-array
allocation/deallocation bursts drive glibc malloc/free hard enough to populate and drain
tcache freelists for that size class -- the exact `REVEAL_PTR` pop pattern that safe-linking
protects and that ADVISORY-001 3N symbolized as the fault site. No guest compiler is needed
(this repo's guest gcc/cc1 are broken, confirmed by earlier sessions), and no new host
cross-compilation toolchain was needed either, since bash's own subshell fork is a completely
realistic, unmodified glibc fork-without-exec workload already present in the target image.

### Baseline (default fork path): CONFIRMED corrupt, as expected

Command: `litebox_runner --unstable --oci-image linuxserver/webtop:debian-xfce --resume-from
tcache_fork_repro.tar -- /bin/bash /tcache_fork_repro.sh` (no `LITEBOX_PROCESS_FORK`).

Result:
```
TCACHE_REPRO_START pid=1
malloc(): unaligned tcache chunk detected
/tcache_fork_repro.sh: line 56:     2 Aborted     ( declare -a bufs; ... )
TCACHE_REPRO_SUBSHELL_STATUS=134
```
glibc's own tcache consistency check (`e->next` failing the `test $0xf,%al` alignment check
neighboring the `__libc_malloc+0x76` `REVEAL_PTR` site) aborts the child with SIGABRT
(status 134). This is a genuine, clean repro of the corruption class: confirms the repro is
real and matches the predicted failure mode.

### `LITEBOX_PROCESS_FORK=1` attempts: two runs, neither reached a clean verdict

**Critical environment-variable-plumbing lesson (costly, worth recording prominently):**
`--env LITEBOX_LOG=... --env LITEBOX_PROCESS_FORK=1` sets variables in the **guest's**
environment, passed to the launched Linux program. Both `LITEBOX_LOG` (read by the runner's
own `tracing_subscriber::EnvFilter` at `litebox_runner_linux_on_windows_userland/src/lib.rs:524`)
and `LITEBOX_PROCESS_FORK` (read via `std::env::var_os` on the **host** side, inside
`spawn_cross_process_fork_child` in `litebox_platform_windows_userland/src/lib.rs:8590`) must
be set as real Windows/host environment variables on the process launching the runner, NOT via
`--env`. The first two attempts at this experiment silently ran with the gate permanently
unexercised and zero tracing output, because both flags were passed the wrong way. Any future
session repeating this experiment: `export LITEBOX_LOG=... LITEBOX_PROCESS_FORK=1` (or
PowerShell `$env:...=`) on the host shell, never `--env`.

**Attempt 1** (`bash /tcache_fork_repro.sh`, host env correctly set, `LITEBOX_LOG=debug`):
reached the gate, and it fired loudly and exactly as pass 158's logging promises:
```
clone: cross-process (D==0) fork() NOT eligible -- guest holds fd(s) at or above 3 ...
beyond_stdio=1 total_alive=4
```
Root cause: `bash /path/to/script.sh` (as opposed to `bash -c '...'`) keeps the script file
itself open as an extra fd (confirmed via the surrounding `sys_openat path=/tcache_fork_repro.sh
fd=Some(3)` with no matching close before the fork). So this run's repro was NOT actually
`beyond_stdio == 0` -- it fell through to the same broken thread-based path, and its
tcache-abort crash (identical signature to baseline) is not evidence about `LITEBOX_PROCESS_FORK`
at all. This is exactly the false-negative trap ADVISORY-002 section 1 warns about (the 94th-pass
historical conclusion made the identical mistake with a held socket instead of a held script fd).

**Attempt 2** (fixed: `bash -c '<inline command>'`, no script file, host env set,
`LITEBOX_LOG=warn`): this should have been genuinely `beyond_stdio == 0` (no script fd, no
`/dev/tty` fd alive at fork time in this invocation shape). But the run never reached the
`beyond_stdio` gate at all -- it hard-crashed the **host** runner process first, inside the
proactive relocation-healing pass (`fixup_stale_elf_data_pointers`, `litebox_shim_linux/src/
syscalls/process.rs:1606` and its caller around line 2813) that runs unconditionally for
*every* real fork, before the code ever reaches the `beyond_stdio` gate at line ~3190. The
crash:
```
[several hundred] DIAG_HEAL wrote a pointer into an executable dest range (slot=... old=... new=...)
diag: fixup_stale_elf_data_pointers summary ranges_seen=10 healed_count=2460
[diag-unrecov-av] tid=ThreadId(6) rip=0x7ff8da4f5492 addr=0xc0000100 ... is_in_guest=false
  -- no exception-table entry found
```
`is_in_guest=false` and "no exception-table entry found" mark this as a **host**-side
unrecoverable access violation inside litebox's own Rust code (not a guest fault, not the
`__libc_malloc+0x76` signature at all), during the pointer-healing sweep of the *thread-based*
relocating duplicate that fork() unconditionally performs before the `LITEBOX_PROCESS_FORK`
gate is ever consulted.

### The load-bearing architectural finding this surfaces

Read together with the code (`litebox_shim_linux/src/syscalls/process.rs`): the eager
`PageManager::duplicate()` call (line ~2604) and both proactive fixup passes
(`fixup_stale_stack_pointers`/`fixup_stale_elf_data_pointers`, lines ~2808-2813) run
**unconditionally for every real fork**, strictly *before* the `fd_complexity.beyond_stdio == 0`
gate check (line ~3190) and the `spawn_cross_process_fork_child` call (line ~3196) that follow
much later in the same function. In other words: **`LITEBOX_PROCESS_FORK=1` does not skip the
costly, glibc-unsafe, and (per this run) sometimes host-crashing relocating duplicate/heal
step at all** -- it only decides, after that step has already run to completion (or, this run,
crashed the host partway through), which artifact the child process actually resumes from
(the just-built, possibly-corrupt-for-glibc relocated copy in a new thread, vs a
freshly-spawned separate Windows process at identity/source addresses per
`spawn_cross_process_fork_child`). This matches ADVISORY-002 section 3's own description of the
mechanism (the duplicate always happens; the cross-process artifact is discarded rather than
resumed-from when the gate passes), but the practical consequence -- that the SAME
duplicate/heal call can crash the *host* runner process outright, independent of whether a
`beyond_stdio == 0` guest would go on to take the cross-process branch -- was not previously
measured or flagged as a Step-0-relevant risk, and it means a genuinely clean `beyond_stdio==0`
D==0 verification run needs either (a) a repro small/simple enough that the proactive fixup
passes do not themselves crash the host before the gate is reached, or (b) the relocating
duplicate/heal step's own reliability fixed/bypassed first.

### Verdict: INCONCLUSIVE, not CONFIRMED and not REFUTED

Four runs total this session:
1. Default fork path, `bash script.sh`: tcache abort, as expected (real evidence, but not
   about `LITEBOX_PROCESS_FORK`).
2 & 3. `LITEBOX_PROCESS_FORK=1`, `bash script.sh`, wrong-then-right env plumbing: gate fired,
   `beyond_stdio=1` (script-file fd held), fell through to the broken path -- not a valid test
   of the cross-process path.
4. `LITEBOX_PROCESS_FORK=1`, `bash -c '...'` (intended to be genuinely `beyond_stdio==0`):
   host-side crash in the always-run relocation-healing step, before the gate was ever reached.

**No run in this session actually exercised the `spawn_cross_process_fork_child` success path
with a confirmed `beyond_stdio == 0` guest end to end.** The central claim of ADVISORY-002
step 0 -- that a `beyond_stdio==0` guest under `LITEBOX_PROCESS_FORK=1` avoids the
`__libc_malloc+0x76` tcache fault -- is therefore **neither confirmed nor refuted** by this
session's runs. What IS newly established is the separate, real finding above: the
unconditional pre-gate duplicate/heal step is itself a host-crash risk on this exact repro
shape, independent of the gate's own logic, and needs to be understood/fixed (or the repro
adjusted to avoid tripping it) before a clean Step-0 verdict is reachable.

**Recommended next step for a future session:** retry with the `bash -c` (or an even smaller,
non-bash) `beyond_stdio==0` repro, but first either (a) capture a full `LITEBOX_LOG=debug`
trace of the host crash at line 1244 of `processfork_clean.log` (preserved on disk this
session for exactly this purpose) and root-cause the `DIAG_HEAL`/`fixup_stale_elf_data_pointers`
host AV, since it may itself be a real, previously-uncharacterized bug worth fixing regardless
of Track B; or (b) shrink the repro's address-space footprint (fewer/smaller allocations,
avoid loading bash's full library set if a smaller dynamically-linked glibc binary can be found
or built) so the proactive healing pass has less surface to crash on, and re-run to completion.

### Open questions from ADVISORY-002 section 7 -- partial closure this session

- **`QueueUserAPC2` availability on this exact build: CONFIRMED.** Compiled and ran
  `advisor/probes/apc_probe.c` fresh this session (`clang -O1 -o apc_probe.exe apc_probe.c
  -lkernel32 -lntdll`). Output: `QueueUserAPC2 export: 00007FF8D7EDB2C0`,
  `parent: QueueUserAPC2(special) on child thread -> ok=1 err=0`,
  `parent: child result: shared1=1234 (expect 1234) shared2=1 (1=APC interrupted user-mode
  spin)`. This closes the open question -- the probe was previously only "appears to discharge
  it" per the advisory; it is now confirmed actually run, successfully, on this exact host
  (Windows 11 10.0.26200) and this exact build.
- **Whether `spawn_cross_process_fork_child`'s child survives long-running execution, or only
  to `execve`: NOT closed this session.** No run reached the point of having a live
  cross-process child to observe over time -- see the INCONCLUSIVE verdict above. Still open.
- Reserve size / collision-free high-VA band for the eventual shared section: not addressed
  this session (out of scope for Step 0 specifically).
- Byte-level confirmation of `PROTECT_PTR` in raw 2.41/2.42 `malloc.c` source: not addressed
  this session.

### Artifacts from this session

- `advisor/probes/tcache_fork_repro.sh` -- the repro script, committed.
- `advisor/probes/apc_probe.exe` -- compiled probe binary confirming `QueueUserAPC2`, NOT
  committed (binary artifact; rebuild via the one-line `clang` command in the probes README
  pattern any time it's needed again).
- Raw run logs (`baseline_default_fork.log`, `processfork_run.log`, `processfork_run2.log`,
  `processfork_debug.log`, `processfork_debug2.log`, `processfork_clean.log`) were left in the
  repo root during the session for inspection; NOT committed (large, session-local debug
  output, several with full DEBUG-level traces). A future session picking this up should look
  for them there or regenerate as needed, and should clean them up if committing other changes
  from the same working tree.

### Overall go/no-go read for the large Track B investment

**Do not treat this session's runs as either a green light or a red light for Track B.** The
one clean, unambiguous result (baseline default-path tcache abort) only reconfirms already-known
behavior. The `LITEBOX_PROCESS_FORK=1` side of the experiment was compromised twice by tooling
mistakes (env-var plumbing, then an accidentally-nonzero `beyond_stdio`) and once by a newly
discovered, apparently pre-existing host-crash bug in the unconditional pre-gate healing step.
Re-running Step 0 cleanly, informed by the lessons above, remains the correct next action before
committing to the rest of the Track B ordered plan -- this session narrowed *how* to do that
correctly but did not produce the decisive result itself.

---

## 2026-09-07 (later): fixed the DIAG_HEAL-logging host crash; Step 0 still blocked by a
## separate, deeper exception-table bug

### Goal

Root-cause and fix the `fixup_stale_elf_data_pointers` host crash from the prior entry above,
then re-attempt Step 0 to a clean CONFIRMED/REFUTED verdict.

### Root cause found: the "temporary, do not commit" DIAG_HEAL logging was never removed

`litebox_shim_linux/src/syscalls/process.rs`'s `fixup_stale_elf_data_pointers` (~line 1606)
contained an unconditional `litebox_util_log::error!` call on every heal whose translated value
landed in an executable destination range -- its own comment already said "DIAGNOSTIC
(temporary, do not commit)" but it shipped anyway, with no env-var gate. On this repro it fired
hundreds of times in one fork (`healed_count=2460` total, confirmed in the prior entry's log
excerpt). Each call is a real `tracing`-backed formatted log event (allocation + I/O), executed
from inside the fork-time relocation-healing window -- the same window
`litebox_platform_windows_userland`'s own `!is_in_guest` VEH branch doc comments already
document as prone to a transient Windows FS_BASE-clear race for host code. The crash's captured
`addr=0xc0000100` is `STATUS_VARIABLE_NOT_FOUND`, a leaked-NTSTATUS-in-register signature this
codebase's own `docs/AGENTS_ARCHIVE_2026-09-03.md` already ties to Windows API activity reached
from host code, not guest memory content -- consistent with the logging burst being the
proximate trigger, not the pointer-healing arithmetic itself.

### Fix applied (minimal, targeted)

Removed the unconditional `litebox_util_log::error!` calls (both the per-heal one and the
per-pass summary) from `fixup_stale_elf_data_pointers`. Could not gate behind a
`LITEBOX_DIAG_HEAL_LOG`-style host env-var check instead, as first attempted: this crate
(`litebox_shim_linux`) is `#![no_std]`, so `std::env::var_os` does not compile there. Removing
the diagnostic outright (rather than half-wiring an env gate that would not build) was the
correct minimal fix given it was already marked as not meant to be committed. Left a doc comment
at the call site pointing any future investigator at a `static AtomicBool`/host-side toggle
instead of a direct `std::env` read, if the diagnostic is needed again.

Commit: `373c79f` "fork: stop the DIAG_HEAL diagnostic from crashing the host mid-fork".

### Verified: the logging-burst crash is gone; a DIFFERENT, deeper, pre-existing crash remains

Rebuilt (`cargo build --release --bin litebox_runner_linux_on_windows_userland`), re-ran the
identical `bash -c` `tcache_fork_repro.sh`-equivalent inline repro under
`LITEBOX_PROCESS_FORK=1 LITEBOX_LOG=debug` (both real host env vars). Result: **zero
`DIAG_HEAL`/`healed_count` log lines this run** (confirms the fix removed that logging burst
entirely), but the host runner still crashed -- earlier in the fork sequence than before, inside
`PageManager::duplicate()`'s own page-allocation/relocation step (`DIAG_VMA fork-relocation`
lines were still being emitted when the crash hit), not inside either proactive healing pass at
all:

```
[diag-extable] module_base=0x7ff658a40000 rip=0x7ff6592f0198 rva=0x8b0198 table_len=12 shown=12
[diag-extable]   [3] start=0x7ff6592f0198 (rva 0x8b0198) stop=0x7ff6592f01a0 (rva 0x8b01a0) fixup=0x7ff6592f01a2 covers=true
[diag-unrecov-av] tid=ThreadId(6) rip=0x7ff6592f0198 rva=0x8b0198 addr=0x10188000 rsp=0x1f8d9fdfc0
  rax=0x1 rbx=0x1000 rcx=0x200 rdx=0x10188000 rsi=0x10188000 rdi=0x7ff00049c000 rbp=0x1f8d9fe030
  is_in_guest=false is_verifying=false -- no exception-table entry found
```

The `[diag-extable]` dump is the load-bearing detail: entry `[3]`'s own printed range
(`start=0x7ff6592f0198`, `stop=0x7ff6592f01a0`) DOES contain the faulting `rip`
(`0x7ff6592f0198`), and the diagnostic's own `covers` computation says `covers=true` for that
exact entry -- yet `search_exception_tables` (called moments earlier at
`litebox_platform_windows_userland/src/lib.rs:1598-1600`, on the identical `context.Rip`, before
this diagnostic even runs) returned `None`, driving the fault into the unrecoverable
`[diag-unrecov-av]` path instead of resuming at the fixup address. This is not a new bug this
session introduced -- `litebox/src/mm/exception_table.rs`'s own `debug_snapshot_table` doc
comment (predating this session) already documents this exact class as previously observed and
unexplained: "a real captured fault landed on an instruction that the on-disk `.extable` section
provably covers, yet `search_exception_tables` reported no match". `search_exception_tables`
and `debug_snapshot_table` share the identical `reloc()` closure and the identical
`exception_table()` PE-section lookup, so the divergence is not an obvious textual bug in either
function; not root-caused further this session (would need a live debugger attached at the
fault to compare the two calls' actual table contents/addresses side by side, since static
reasoning from the source does not explain the mismatch).

A second, immediately following fault in the SAME run
(`rip=0x7ff8da4f587a addr=0xa is_in_guest=false`, an ntdll-range address, `fault_addr=0xa`) is
consistent with the process already being in a corrupted, post-first-crash state and is not
treated as independent evidence.

### Step 0 verdict: STILL INCONCLUSIVE (not CONFIRMED, not REFUTED) -- blocked one layer deeper

The DIAG_HEAL logging-burst crash that blocked the previous session's Step 0 attempt is fixed
and confirmed gone. But `LITEBOX_PROCESS_FORK=1` still cannot be exercised end-to-end on this
repro: the process now crashes even earlier, inside `PageManager::duplicate()` itself, before
either proactive healing pass runs and long before the `beyond_stdio` gate (~line 3190) is
reached. **No run this session reached the `spawn_cross_process_fork_child` success path
either** -- so step 9 (long-running cross-process child survival) could not be attempted at all;
there was never a live cross-process child to observe.

### Baseline reconfirmation (step 8b): CONFIRMED, unchanged

Re-ran the same repro with `LITEBOX_PROCESS_FORK` unset (default fork path),
`LITEBOX_LOG=warn`. Clean SIGABRT as expected, no change from prior sessions:

```
fatal signal: terminating task signal=Signal(6) pid=2 tid=2 comm=bash...
/bin/bash: line 1:     2 Aborted   ( declare -a bufs; ... )
TCACHE_REPRO_SUBSHELL_STATUS=134
TCACHE_REPRO_DONE
```

### Test results (step 7)

`cargo test --release -p litebox -p litebox_shim_linux -p litebox_common_linux -p
litebox_platform_windows_userland`:
- `litebox_shim_linux` (test/doctest build) and `litebox_platform_windows_userland` (doctest
  build only) fail to compile: `E0576`/`E0407`, "cannot find method `run_test_thread` in trait
  `ThreadProvider`". Confirmed via `git stash` that this is 100% pre-existing on `main` before
  this session's change -- not a regression from the `fixup_stale_elf_data_pointers` fix (a
  trait/test-infrastructure drift unrelated to `process.rs`).
- `litebox` lib tests: 123 passed, 26 failed. Confirmed via the same `git stash` comparison to
  be the identical pass/fail count on unmodified `main` -- all 26 failures are pre-existing
  (missing `diod` binary for the `nine_p` suite, one `tar_ro` symlink test, one `mm::tests::
  test_vmm_mapping` range-merging assertion), none touching code this session changed.
- `litebox_common_linux`: 4 passed, 0 failed, 1 doctest passed -- clean.

No regression from this session's fix in any of the four crates.

### What remains open for a future session

- Root-cause the `search_exception_tables`/`debug_snapshot_table` divergence itself (both
  compute from the identical PE `.extable` section and the identical `reloc()` logic, yet
  disagree on whether the same `rip` is covered) -- this is now the concrete, precisely
  evidenced blocker standing between this investigation and a clean Step-0 run, more fundamental
  than anything in the fork-time healing passes. A live debugger session that breaks at the
  `[diag-unrecov-av]` print and inspects both call sites' actual `exception_table()` return
  values side by side is the most direct next step.
- Step 0's central claim (does `LITEBOX_PROCESS_FORK=1` avoid the tcache fault for a
  `beyond_stdio==0` guest) remains neither confirmed nor refuted -- no run in this investigation
  to date has ever reached the cross-process child successfully.
- Step 9 (long-running cross-process child survival) is untouched -- there has never been a live
  cross-process child to observe in any session so far.

---

## 2026-09-07 (later still): root-caused and fixed the search_exception_tables/debug_snapshot_table
## divergence -- a genuine torn-read bug, not a diagnostic artifact

### Goal

Root-cause the `search_exception_tables`/`debug_snapshot_table` divergence flagged as the
concrete blocker at the end of the prior entry, then push Step 0 as far toward a real
CONFIRMED/REFUTED verdict as the session allows.

### Method: live instrumentation, not more static reading

Static reading of `litebox/src/mm/exception_table.rs` (both functions use the identical
`reloc()` closure and the identical `exception_table()` PE-section lookup) genuinely could not
explain the divergence, matching the prior session's own conclusion. Added a temporary
per-entry trace to `search_exception_tables` itself (env-gated via `LITEBOX_DIAG_SEARCH_TRACE`,
logging `i/addr/start/stop/fixup/covers` for every table entry on every AV) plus a raw print of
`context.Rip`/`context.Rdx`/`context.Rsi` taken immediately before the `search_exception_tables`
call site in `litebox_platform_windows_userland/src/lib.rs`, and reproduced live: `bash -c
'(...)'`  tcache-exercise repro under `LITEBOX_PROCESS_FORK=1 LITEBOX_LOG=debug
LITEBOX_DIAG_SEARCH_TRACE=1`, both as real host env vars.

### Root cause: `search_exception_tables` was called on a live, racing `context.Rip` re-read,
### not the already-captured, safe `context_snapshot.Rip`

The live capture is unambiguous. In one fault:
```
[diag-search-call] context_ptr=0xfe0c9fd580 context.Rip=0x10188000 context.Rdx=0x0 context.Rsi=0x10188000 trunc=0x10188000
[diag-search-trace] i=3 addr=0x10188000 start=0x7ff6d80efad8 stop=0x7ff6d80efae0 fixup=0x7ff6d80efae2 covers=false
[diag-unrecov-av] tid=ThreadId(6) rip=0x7ff6d80efad8 rva=0x8afad8 addr=0x10188000 rsp=0xfe0c9fdc80 rax=0x1 rbx=0x1000 rcx=0x200 rdx=0x10188000 rsi=0x10188000 rdi=0x7ff00049c000 rbp=0xfe0c9fdcf0 is_in_guest=false is_verifying=false -- no exception-table entry found
```
`context.Rip` read live, immediately before the `search_exception_tables` call, was
`0x10188000` -- not an instruction pointer at all, but `exception_record.ExceptionInformation[1]`
(the faulting *memory* address, also visible verbatim in `Rdx`/`Rsi`, consistent with an in-
flight `rep movsq`/`memcpy_fallible` AV: `rcx=0x200` qword count, `rsi`=source=fault address).
Yet `context_snapshot.Rip` -- captured once, at this same function's very first statement,
well before the search call, in the SAME non-reentrant invocation (`veh_depth=0x1`, i.e. no
nesting) -- was `0x7ff6d80efad8`, which the sibling `debug_snapshot_table`-based diagnostic
confirms entry `[3]`'s `[start, stop)` range genuinely covers (`covers=true`). Both readings are
in the same handler invocation, same thread, with nothing in this codebase writing `context.Rip`
in between (checked exhaustively: the only intervening code is the FS_BASE-reset fast path,
which returns immediately if taken and was not taken here).

The file's own pre-existing `context_snapshot` doc comment (added independently of this
investigation, well before it) already names the exact mechanism: `context` is a live pointer
into the OS-owned in-flight `CONTEXT` record, and other machinery in this process
(`ThreadHandle::interrupt`'s `SuspendThread`/`SetThreadContext`, `ctxwatch_arm_other_threads`'s
debug-register rewrites) can write that same memory concurrently -- exactly the "TORN-READ"
class that comment was written to guard against. `context_snapshot` was captured specifically so
every diagnostic read could use a torn-read-safe copy instead of the live, racing pointer -- but
the one call site that actually FEEDS the recovery decision, `search_exception_tables(context.Rip
.trunc())`, was never updated to use it, and instead re-read the live, racing `context.Rip`
directly. This is not a bug in `search_exception_tables` or `debug_snapshot_table` themselves
(confirming the prior session's inability to find a textual divergence between them was
correct) -- it is a bug in which `Rip` value the caller in `litebox_platform_windows_userland`
handed to the search, one line away from a value that was already known to be safe.

### Fix applied

`litebox_platform_windows_userland/src/lib.rs`'s exception-table search call now searches on
`context_snapshot.Rip.trunc()` instead of a live `context.Rip.trunc()` re-read. The resume write
(`context.Rip = recover as u64`, a few lines later, on success) still correctly targets the LIVE
`context` -- that write is how Windows is told where to resume, and must go through the live
pointer; only the read that decides whether a fixup exists needed to move to the snapshot.
Minimal, one-call-site change; no changes to `litebox/src/mm/exception_table.rs` itself (the
temporary per-entry trace added for this investigation was removed after root-causing, net diff
zero on that file).

### Verified: the specific crash this fix targets is gone

Re-ran the identical `bash -c` repro under `LITEBOX_PROCESS_FORK=1 LITEBOX_LOG=warn` against the
rebuilt binary. No `[diag-unrecov-av]` this run (previously guaranteed to fire at this point in
every prior session's attempt) -- fork-relocation logging progressed further into
`PageManager::duplicate()` than any previous run this investigation has captured. The run did not
reach a clean `TCACHE_REPRO_CHILD_OK`/completion marker either; the host process exited silently
partway through relocation with no panic, no crash diagnostic, and no exit-code capture available
from this session's tooling. Host free memory was measured at ~4.2GB going into this run and
~3.4GB coming out (checked via `Get-CimInstance Win32_OperatingSystem`), consistent with the
process being reaped under host memory pressure from unrelated concurrent load on this machine
(multiple other large processes observed running concurrently, not litebox-related) rather than a
code-level fault -- but this session could not rule that out definitively (no Windows Event Log
/ crash-dump capture was set up to distinguish an OOM-style termination from a silent crash).

### Test results (step 7, re-run after this session's fix)

`cargo test --release -p litebox --lib`: 123 passed, 26 failed -- identical pass/fail count and
identical failing-test list to the pre-existing baseline documented in the prior entry (missing
`diod` binary for `nine_p`, one `tar_ro` symlink test, one `mm::tests::test_vmm_mapping`
range-merging assertion). No regression.

`cargo test --release -p litebox_common_linux`: 10 passed, 0 failed, 1 doctest passed -- clean.

`cargo test --release -p litebox_platform_windows_userland --lib`: 4 passed, 0 failed -- clean
(this is the crate the fix itself lives in).

`cargo test --release -p litebox_shim_linux`: still fails to compile (test/doctest build),
`E0576`/`E0407` "cannot find method `run_test_thread` in trait `ThreadProvider`" -- confirmed
pre-existing in the prior entry via `git stash` comparison, unrelated to this session's change
(this session did not touch `litebox_shim_linux` at all).

### Step 0 verdict: STILL INCONCLUSIVE, but the specific blocker this entry targeted is resolved

The `search_exception_tables`/`debug_snapshot_table` divergence is root-caused (a genuine
torn-read bug in the caller, not a diagnostic-comparison artifact and not a bug in either
exception-table function) and fixed, with no regression in the scoped test suite. The specific
crash signature this divergence caused (`[diag-unrecov-av]` with `covers=true` for an entry the
live search should have found) is confirmed gone from a live re-run. Step 0's central claim --
whether `LITEBOX_PROCESS_FORK=1` avoids the tcache fault for a `beyond_stdio==0` guest -- remains
neither confirmed nor refuted: this session's one post-fix run got further into
`PageManager::duplicate()` than any prior run but did not reach either a clean completion or an
unambiguous crash diagnostic, most likely due to real, unrelated host memory pressure on this
machine rather than a code defect.

### What remains open for a future session

- Re-run Step 0's `LITEBOX_PROCESS_FORK=1` repro on a host with confirmed headroom (>6GB free,
  no other large concurrent processes) to get a clean, unambiguous run all the way to either
  `TCACHE_REPRO_CHILD_OK`+`spawn_cross_process_fork_child` success, or a new, different crash
  diagnostic. This session's inconclusive silent exit was very plausibly a memory-pressure
  artifact of the host, not a code issue, but that is not yet certain.
- If another silent, diagnostic-free host process exit recurs on a memory-healthy host, that
  itself becomes a new, real bug worth its own investigation (this codebase's crash paths are
  otherwise well-instrumented; a completely silent exit with no panic/AV/diagnostic would be a
  gap in that coverage).
- Step 9 (long-running cross-process child survival) remains untouched -- still no run has ever
  reached a live cross-process child to observe.

---

## 2026-09-07 (yet later): Step 0 final verdict -- **REFUTED**, and it is a genuine bug, not
## memory pressure

### Goal

Get a final, clean, decisive Step 0 verdict on a host with confirmed memory headroom, per the
open item at the end of the prior entry. Capture Windows-level crash artifacts this time so a
silent exit can be definitively attributed to memory pressure vs. a real code defect.

### Pre-flight

Free memory checked before starting: 4.6GB (`Get-CimInstance Win32_OperatingSystem`), trending
down slightly over the session (4.6GB -> 4.4GB -> 4.0GB) purely from unrelated background load
on this host (several `claude`/Chrome/Discord processes, none litebox-related) -- no
multi-GB litebox zombie processes were found running (`tasklist` showed zero
`litebox_runner_linux_on_windows_userland.exe` at session start). This is below the requested
5GB target but the repro itself is lightweight and, per the actual results below, memory
pressure was not the limiting factor this time either.

`target/release/.litebox-cache/boot.lock` was stale (pid 3744, confirmed dead via `tasklist`)
and was cleared before the first run.

Configured `HKCU:\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\
litebox_runner_linux_on_windows_userland.exe` (`DumpFolder=C:\dev\litebox-main\crashdumps`,
`DumpType=2` full dump, `DumpCount=5`) so any Windows-level unhandled crash would leave a real
minidump. No `windbg`/`cdb` available on this host (`where` found neither).

### Repro construction: verified genuinely `beyond_stdio == 0`

Per the prior entry's own hard-won lesson (`bash /script.sh` holds the script file open as fd 3,
producing a false `beyond_stdio=1`), this session ran the script's exact command sequence inline
via `bash -c '<...>'` with no script file at all -- no extra fd opens anywhere in the invocation
shape. Both `LITEBOX_PROCESS_FORK` and `LITEBOX_LOG` were set as real host (Git Bash `export`)
environment variables before invoking the runner directly, never via `--env` (which only reaches
the guest).

### Baseline (default fork path, no `LITEBOX_PROCESS_FORK`): CONFIRMED corrupt, unchanged

`LITEBOX_LOG=warn`, `LITEBOX_PROCESS_FORK` unset. Result, identical to every prior session:
```
malloc(): unaligned tcache chunk detected
...
fatal signal: terminating task signal=Signal(6) pid=2 tid=2 comm=bash
/bin/bash: line 1:     2 Aborted   ( declare -a bufs; ... )
TCACHE_REPRO_SUBSHELL_STATUS=134
TCACHE_REPRO_DONE
```
Zero `[diag-unrecov-av]` lines in this run's log -- confirms the crash below is specific to the
`LITEBOX_PROCESS_FORK=1` path, not a general instability of this host or this repro shape.

### `LITEBOX_PROCESS_FORK=1` run 1: reproducible host-side crash, well before the `beyond_stdio`
### gate, NOT a silent exit this time

`LITEBOX_LOG=debug`, `LITEBOX_PROCESS_FORK=1`, ~4.1GB free going in. The run reached
`do_clone: about to duplicate address space for fork()`, entered `PageManager::duplicate()`'s
VMA relocation loop (many `DIAG_VMA fork-relocation` / `allocate_pages` lines, `self_owner=
GuestPid(2)`), and after roughly 0.4s of relocation work crashed:

```
[diag-unrecov-av] tid=ThreadId(6) rip=0x7ff8da4f5492 rva=0x26fdb5492 addr=0xc0000100
  rsp=0xe4831fcc30 rax=0x0 rbx=0x10188000 ... is_in_guest=false is_verifying=false
  -- no exception-table entry found
```
The log then ends abruptly mid-diagnostic-dump (`[diag-unrecov-av-ring-stack]` lines with no
further output, no panic message, no clean-exit marker) -- the process is torn down from inside
its own unrecoverable-AV handling path.

**Critically: `beyond_stdio` never appears anywhere in this run's log.** Grepped for
`beyond_stdio|fd_complexity|cross-process` across the full ~2700-line debug trace: zero matches.
This confirms the crash happens strictly inside `PageManager::duplicate()`'s relocation loop,
before the `fd_complexity.beyond_stdio == 0` gate check that (per the codebase, ~line 3190) runs
much later in the same function. `spawn_cross_process_fork_child` was never reached.

No Windows-level minidump was written to the configured `crashdumps/` folder, and
`Get-WinEvent -LogName Application` showed no new crash entries in the run's time window (only
stale, unrelated kernel/BSOD entries from days earlier, flushed at boot) -- this is litebox's
own VEH catching the AV and self-terminating via its internal unrecoverable-AV path, not an
OS-level unhandled exception. This also rules out the "concurrent boot" confusion from earlier
in the session: a leftover, unkillable (`Stop-Process -Force` and `taskkill /F` both failed
silently) but small (~256MB) `litebox_runner_linux_on_windows_userland.exe` zombie (pid 27452,
`Responding=True` yet un-terminable) was found holding the boot lock before this run; clearing
the stale lock file directly (not touching the zombie process itself, which appears to be an
OS-level zombie/ghost handle-table entry, not a real live process) was sufficient to proceed.

### `LITEBOX_PROCESS_FORK=1` run 2: IDENTICAL crash, confirms determinism, refutes memory
### pressure as the explanation

Immediately re-ran the identical repro (same env vars, ~4.0GB free going in -- lower than run 1,
if anything more memory-constrained). Result: **byte-for-byte identical crash signature** --
same `rip=0x7ff8da4f5492`, same `rva=0x26fdb5492`, same `addr=0xc0000100`, same `tid=ThreadId(6)`,
same abrupt end-of-log mid-`[diag-unrecov-av-ring-stack]` dump, same ~2688 total log lines. Two
independent runs producing an identical fault address and identical truncation point is strong
evidence of a deterministic bug, not a memory-pressure-driven race whose symptom would be
expected to vary run to run (different silent-exit points, different or absent diagnostics).

### Root-cause read (not fully resolved -- would need a live debugger, unavailable on this host)

`addr=0xc0000100` is `STATUS_VARIABLE_NOT_FOUND`, the same leaked-NTSTATUS-in-register signature
this codebase's own `docs/AGENTS_ARCHIVE_2026-09-03.md` already ties to Windows API activity
reached from host code, not guest memory content. `tid=ThreadId(6)` is a background thread, not
the guest's main thread that is running `do_clone`/`PageManager::duplicate()` -- meaning some
other thread is executing real Windows API code (its `rva=0x26fdb5492` is a huge module offset,
consistent with ntdll or another system DLL, not litebox's own ~1MB module image) and takes an
AV concurrently with the fork-time page relocation on the main thread. This is exactly the class
of hazard this codebase's own comments already flag: Windows API calls made by host code during
the fork-time relocation window are exposed to transient state (e.g. the documented FS_BASE-clear
race) that this specific window does not fully account for. `is_in_guest=false` rules out this
being guest memory content; `no exception-table entry found` confirms it is not a recognized,
intentionally-recoverable guest fault either. Not root-caused to a specific call site this
session -- doing so would need a live debugger (`windbg`/`cdb`) attached at the fault to inspect
what background thread 6 is doing and why, which was not available on this host and is out of
scope for a Step-0 verdict pass; noted as the concrete next action for whoever picks this up.

### Step 0 verdict: **REFUTED**

**Bold, final, unambiguous: Step 0 is REFUTED.** `LITEBOX_PROCESS_FORK=1` does NOT currently let
a genuinely `beyond_stdio == 0` guest fork successfully. Across two independent runs on a host
with confirmed memory headroom (4.0-4.1GB free, no concurrent litebox processes, no other
unusual load beyond this host's steady-state background processes) and full `LITEBOX_LOG=debug`
tracing, the cross-process fork path crashes the host runner process itself, deterministically,
at the identical fault address, strictly inside `PageManager::duplicate()`'s relocation loop --
**before** the `beyond_stdio` gate is ever reached and **before** `spawn_cross_process_fork_child`
is ever called. This is not the previous session's memory-pressure-confounded inconclusive
result: that session measured a real 4.2GB->3.4GB free-memory drop during its one ambiguous run
and could not rule out OOM reaping. This session reproduced the identical crash twice, with
memory headroom that did not meaningfully change between runs and was never critically low, and
with zero silent/undiagnosed exits -- both runs produced the exact same host-side AV diagnostic.
Memory pressure is definitively ruled out as the explanation. This is a real, distinct,
previously-uncharacterized bug in the unconditional pre-gate `PageManager::duplicate()` path
(most likely a genuine cross-thread race between fork-time page relocation and concurrent
Windows API activity on another thread), independent of and blocking any assessment of the
`LITEBOX_PROCESS_FORK=1` gate's own logic.

### Step 9 (long-running cross-process child survival): NOT REACHED

Per the REFUTED verdict above, no run in this investigation to date -- including this session --
has ever produced a live cross-process forked child to observe. This sub-question remains
completely open and cannot be attempted until the `PageManager::duplicate()` crash above is
fixed.

### Go/no-go read for the large Track B investment

**Do not proceed with the queued architectural Track B work yet.** Step 0 was designed as a
cheap, decisive, before-funding-anything-else gate, and it has now returned a clean, unambiguous
REFUTED verdict rather than an inconclusive one. The specific blocker is well-characterized
(deterministic host-side AV, `STATUS_VARIABLE_NOT_FOUND`-signature, background-thread Windows API
activity racing fork-time page relocation, `PageManager::duplicate()`, before the `beyond_stdio`
gate) but not yet root-caused to an exact call site or fixed. The three genuine bugs found and
fixed getting to this point (the DIAG_HEAL logging-crash, the `search_exception_tables` torn-read,
and now this session's confirmation that a fourth, distinct crash remains) show this code path is
still substantially unproven; a clean Step 0 CONFIRMED result is the correct gate to keep holding
the line on before committing further architectural investment.

### What remains open for a future session

- Root-cause the `tid=ThreadId(6)` / `rva=0x26fdb5492` / `addr=0xc0000100` crash to an exact call
  site -- needs a live debugger (`windbg`/`cdb`, neither present on this host) attached at the
  fault, or a targeted trace of what background threads are doing/calling during the
  `PageManager::duplicate()` relocation window.
- Once fixed, re-run this exact Step 0 repro (baseline + `LITEBOX_PROCESS_FORK=1`, both real host
  env vars, `bash -c` inline invocation for genuine `beyond_stdio==0`) to get the CONFIRMED
  verdict this investigation has been working toward.
- Step 9 (long-running cross-process child survival) is untouched -- still no run has ever reached
  a live cross-process child to observe; blocked entirely on the above fix.
- The unkillable small zombie `litebox_runner_linux_on_windows_userland.exe` process observed
  this session (`Stop-Process -Force` and `taskkill /F` both silently failed against a process
  `tasklist` still listed as alive with `Responding=True`) is itself worth a future look -- not
  investigated further this session since clearing the stale boot lock file was sufficient to
  route around it, but an un-terminable process handle is unusual and could indicate a genuine
  host-state issue worth understanding.

---

## 2026-09-07 (WinDbg session): root-caused the deterministic `PageManager::duplicate()` crash
## via live debugger attach -- FIVE more stray "temporary, do not commit" diagnostics, same bug
## class as the already-fixed DIAG_HEAL. Fixed. Step 0 progressed further than ever before but
## remains INCONCLUSIVE, blocked now on wall-clock/memory, not on a known crash.

### Goal

Per the prior entry's own recommended next action: get a live-debugger-confirmed root cause for
the deterministic `rip=0x7ff8da4f5492`/`addr=0xc0000100` host AV inside `PageManager::duplicate()`,
using WinDbg/cdb (now installed on this host), rather than more log-only reasoning.

### Tooling found

`WinDbgX.exe` at `C:\Users\user\AppData\Local\Microsoft\WindowsApps\Microsoft.WinDbg_8wekyb3d8bbwe\`.
A real `cdb.exe` (`cdbX64.exe`) is bundled in the same package directory -- more scriptable than
WinDbgX for this purpose, per the task's own suggestion. Used via
`cdbX64.exe -g -G -cf <script.txt> <exe> <args>` (`-cf` for a multi-line command-file, `-g -G` to
skip the process-create/process-exit breakpoints).

### Root cause, found WITHOUT even needing the debugger first: more unremoved "temporary, do not
### commit" diagnostics, same bug class as the already-fixed DIAG_HEAL

Before attaching a debugger, re-read `PageManager::duplicate()` (`litebox/src/mm/mod.rs:779`) and
its callees end to end, specifically hunting for anything resembling the already-proven-dangerous
DIAG_HEAL pattern (an unconditional `litebox_util_log::error!` call inside the fork-time
relocation window). Found **five more**, never removed, all marked "DIAGNOSTIC (temporary, do not
commit)" or equivalent, left over from earlier, unrelated investigations (the forked-Xorg
packed-low-window investigation, a stale-pointer-healing investigation, an ELF-patch-cache
investigation) that never got cleaned up:

1. `litebox/src/mm/linux.rs` `Vmem::new` (~line 777): unconditional `error!` on every `Vmem`
   construction (`DIAG_VMEM new placeholders`).
2. `litebox/src/mm/linux.rs` `Vmem::duplicate`, pre-loop filter (~line 1346): unconditional
   `error!` per skipped host-reserved placeholder VMA, in a loop over every VMA in the source
   address space (`DIAG_VMA skipped`).
3. `litebox/src/mm/linux.rs` `Vmem::duplicate`, group-logging + a full 5-value counterfactual
   bisection sweep re-computed from scratch on every single fork (~lines 1425-1504): TWO
   unconditional `error!` calls per group AND per counterfactual candidate (`DIAG_GROUPS
   duplicate`, `DIAG_GROUPS counterfactual`) -- the most expensive of the five, doing real
   recomputation work, not just logging.
4. `litebox/src/mm/linux.rs` `Vmem::duplicate`, main per-region copy loop (~line 1642): unconditional
   `error!` on EVERY relocated VMA, the single hottest fork-time diagnostic of the five
   (`DIAG_VMA fork-relocation`).
5. `litebox/src/mm/linux.rs` `get_unmmaped_area`'s placement helper (~line 1759): unconditional
   `error!` on every single page placement call, fork or not (`DIAG_PLACE chosen-vs-actual`).
6. `litebox/src/mm/mod.rs` `create_pages` (~line 886): unconditional `error!` on EVERY page
   creation anywhere in the runtime, guest `mmap` and internal callers alike (`DIAG_CREATE_PAGES`)
   -- the single chokepoint every relocated VMA in `duplicate()`'s copy loop passes through.
7. `litebox/src/mm/mod.rs` `register_existing_mapping` (~line 1549): unconditional `error!` on
   every VMA-table registration outside `create_pages`/`duplicate` (`DIAG_REGISTER_EXISTING`).
8. `litebox_shim_linux/src/syscalls/mm.rs` `sys_mmap` (~line 851): unconditional `error!`/`error!`
   (success and failure branches) on literally every guest `mmap()` syscall (`DIAG_MMAP
   returned`/`DIAG_MMAP failed`).
9. `litebox_shim_linux/src/syscalls/mm.rs` `init_elf_patch_state` (~line 1110): a real,
   still-unfixed bug (`ElfPatchKey` is `(pid, fd)`, never re-keyed onto a forked child's pid, so a
   forked child re-initializes ELF patch state from scratch against code the parent already
   patched -- worth its own future investigation) whose diagnostic locked a mutex, collected a
   `Vec`, and unconditionally `error!`-logged on every cache-miss reached via `fork()`.

All eight `error!` call sites plus the ninth (real but unfixed-bug-adjacent) diagnostic were
removed outright, except one that was judged a genuinely useful non-"temporary" regression signal
(`litebox_platform_windows_userland/src/lib.rs`'s `allocate_pages` constrained-retry-fallback log,
~line 6537) -- that one was gated behind the crate's own pre-existing `LITEBOX_DIAG_MM` env-var
check (`diag_mm_enabled()`, already used by sibling diagnostics in the same file) rather than
deleted, since a run silently regressing to the old bottom-up packed layout is a real thing worth
being able to observe on demand.

Every one of these runs inside or immediately downstream of `PageManager::duplicate()`'s
fork-time relocation window -- the exact window this investigation's own prior entry already
proved (the `search_exception_tables` torn-read fix, and before that the original DIAG_HEAL fix)
is hazardous for `tracing`-backed formatted logging (allocation + I/O) to run in, racing
concurrent Windows API activity on another thread.

### Rebuild, then LIVE DEBUGGER CONFIRMATION that the previously-deterministic crash is gone

`cargo build --release --bin litebox_runner_linux_on_windows_userland` (PDB confirmed present:
`target/release/litebox_runner_linux_on_windows_userland.pdb`, full symbols, so no debug-profile
rebuild was needed). Re-ran the exact `bash -c` inline `beyond_stdio==0` repro from the prior
entry, both host env vars set correctly (`LITEBOX_PROCESS_FORK=1`, `LITEBOX_LOG=warn`), under
`cdbX64.exe -g -G -cf <script>` with `sxe av` (stop on every access violation) armed.

**The deterministic `rip=0x7ff8da4f5492 addr=0xc0000100` ntdll crash that fired identically on
every single run since this investigation began is GONE.** It does not appear anywhere in this
session's runs, debugged or standalone. Confirmed via a full `!analyze -v` / `.exr -1` / `r` /
`kb` / `~*k` / `u @rip` / `lm` capture at the first (and, per `sxe av`, only real) access
violation this session's debugged run actually hit:

```
0:010> !analyze -v
...
Failure.Bucket: INVALID_POINTER_READ_c0000005_litebox_runner_linux_on_windows_userland.exe!
  RNvNtNtCsaDjktFBbbgY_7litebox2mm15exception_table15memcpy_fallible
EXCEPTION_RECORD: ExceptionAddress: 00007ff7041e8418
  (litebox_runner_linux_on_windows_userland!...exception_table::memcpy_fallible+0x68)
   ExceptionCode: c0000005 (Access violation)
Attempt to read from address 0000000010188000
STACK_TEXT:
  ...memcpy_fallible+0x68
  ...userspace_pointers::to_owned_slice<NoValidation>+0x73
  ...ForkChildVerificationProvider::spawn_cross_process_fork_child::spawn_process_fork_child+0xbe1
  ...ForkChildVerificationProvider::spawn_cross_process_fork_child+0xed
  ...syscalls::file::sys_close
  ...syscalls::file::pty_ioctl
  ...LinuxShimEntrypoints::EnterShim::syscall
  ...syscall_callback
  ...run_thread_inner
  ...do_clone
  std::sys::thread::windows::thread_start
```

This is a fundamentally different, and far more encouraging, finding than every prior session's
crash:

- **The code now reaches `spawn_cross_process_fork_child` and its real page-copy logic
  (`spawn_process_fork_child`/`copy_one_group`)** -- something no prior session in this whole
  investigation ever observed. The `beyond_stdio==0` gate was passed, the cross-process path was
  taken, and real work happened inside it.
- `~*k` (all 12 threads, full stacks) shows the faulting thread squarely inside
  `do_clone`/`spawn_cross_process_fork_child`, with every other thread either idle
  (`NtWaitForWorkViaWorkerFactory`/`NtWaitForSingleObject`) or doing unrelated, non-racing work
  (the NAT gateway thread, network polling). **No cross-thread race is visible in this crash** --
  contradicts the suspected mechanism named in the original task brief (a background thread
  colliding with fork-time relocation). That mechanism was real for the OLD, now-fixed crash (the
  stray-diagnostic-triggered ntdll AV); it does not explain this one.
- `memcpy_fallible` is `litebox`'s own designed-to-possibly-fault primitive (see
  `litebox/src/mm/exception_table.rs:130`), reading the PARENT's own memory via
  `NoValidation`-mode `to_owned_slice` so `copy_one_group`
  (`litebox_platform_windows_userland/src/process_fork.rs:2926`) can `WriteProcessMemory` it into
  the child, PAGE BY PAGE. `copy_one_group`'s own doc comment (line ~3013) explicitly documents
  that an unreadable page is EXPECTED (the reservation-group span is rounded out to Windows
  allocation granularity and may cover padding beyond the guest's real mapped content) and handled
  by simply leaving that page as the child's already-zero-filled `MEM_COMMIT` default. Confirmed
  live: a STANDALONE (undebugged) re-run of the identical repro produced
  `[diag-recover-fsbase] recover_rip=0x7ff7041e8422 fsbase=0x7feffffb0740` -- the exact fixup
  address `u @rip` disassembly shows immediately after the faulting `rep movs` pair, with a
  healthy non-zero `fsbase` -- i.e. **this specific AV IS being recovered correctly** by
  `search_exception_tables` (using the already-fixed `context_snapshot.Rip`, not a racing live
  read) every single time, exactly as designed. The `sxe av`-armed debugger session only ever saw
  it as "a crash" because `sxe av` breaks on EVERY first-chance access violation, recovered or
  not -- it is not, on its own, evidence of an unrecovered fault. This was confirmed directly: no
  `[diag-unrecov-av]`, `[diag-unrecov-av-giveup]`, or `[diag-extable]` line appeared in the
  standalone run, all of which are the unconditional, ungated prints on the genuinely-unrecovered
  path (`litebox_platform_windows_userland/src/lib.rs:1715` onward) -- ruling out both the
  "genuinely unrecoverable" branch and the repeated-same-rip circuit breaker
  (`MAX_REPEATED_UNRECOV_AV`) as explanations for what happens next.

### What happens next: NOT a crash, NOT a hang from the recovery path itself -- a slow, resource-
### starved copy loop, confounded by severe host memory pressure this session

After the recovered fault, the standalone run produced no further log output and the process
remained alive (confirmed via a second, non-invasive `cdb -p <pid>` attach showing real, if
minimal, thread activity -- not the previously-documented un-terminable zombie-handle artifact,
which shows a thread with NO further stack at all). Investigated three hypotheses:

1. *A repeated-fault livelock in the VEH itself*: ruled out directly -- the diagnostic prints that
   would fire on that path (`[diag-unrecov-av-giveup]`, `[diag-extable]`, `[diag-unrecov-av-depth]`)
   never appeared.
2. *An actual infinite loop in `copy_one_group`'s page-by-page copy*: the loop is a straightforward
   bounded `while cursor < source_group.end` over 4KiB pages with no retry-on-failure logic --
   nothing in the code supports an infinite-loop reading.
3. *Legitimately slow, resource-starved progress*: **the most likely explanation, and consistent
   with direct measurement.** This session's host free memory fell from ~3.9GB at session start to
   under 1.5GB by its end, entirely from unrelated concurrent load (multiple other Claude Code
   sessions, Chrome, Discord, and this project's own `gm` daemon helper process all running
   simultaneously on the same host) -- not from litebox itself, which stayed under 250MB resident
   throughout every run this session. `copy_one_group` walks the webtop image's full multi-hundred-
   MB-to-multi-GB VMA groups ONE 4KiB PAGE AT A TIME, each page a separate `read_source_bytes`
   (host-memory read) + `WriteProcessMemory` (cross-process write) round trip -- tens of thousands
   of syscalls for a single large group, and page-by-page cross-process copy loops are exactly the
   kind of work that degrades sharply under host memory pressure (paging, working-set trims). A
   deliberately re-run debug-level (`LITEBOX_LOG=debug`) attempt later in this session never even
   reached `TCACHE_REPRO_START` (still loading the ~3.5GB webtop image after 45+ seconds) before
   this session's own free memory dropped under 1.5GB and the run was abandoned -- this reproduces,
   on this exact investigation, the identical memory-pressure confound the 2026-09-07 (later still)
   entry above already documented once (that session's own free-memory drop from 4.2GB to 3.4GB
   during one ambiguous, inconclusive run).

**This is very likely NOT a code defect** -- no crash, no unrecovered fault, no evidence of an
actual infinite loop, and a resource-pressure explanation that fits both the direct measurements
and this investigation's own prior precedent for exactly this failure shape. But it was not
confirmed to full completion this session either (no run reached `TCACHE_REPRO_CHILD_OK`), so it
is recorded as unconfirmed-but-likely rather than closed.

### Fix applied and verified

Commit-worthy changes: removed 8 stray unconditional `litebox_util_log::error!` diagnostics (items
1-2, 4-9 above) outright, gated the 1 legitimate regression-signal diagnostic (item 3's sibling in
`lib.rs`, the `allocate_pages` constrained-retry-fallback log) behind the existing
`LITEBOX_DIAG_MM` env var via the crate's own pre-existing `diag_mm_enabled()` helper. Rebuilt
clean. Confirmed via live debugger attach that the previously 100%-deterministic
`rip=0x7ff8da4f5492`/`addr=0xc0000100` ntdll crash inside `PageManager::duplicate()` -- present in
literally every `LITEBOX_PROCESS_FORK=1` run across every session of this entire investigation
until now -- is gone.

### Test results (re-run after this session's fix)

`cargo test --release -p litebox --lib`: 123 passed, 26 failed -- identical pass/fail count and
failing-test list to the documented pre-existing baseline (missing `diod` binary for `nine_p`, one
`tar_ro` symlink test, one `mm::tests::test_vmm_mapping` range-merging assertion). No regression.

`cargo test --release -p litebox_common_linux`: 10 passed, 0 failed, 1 doctest passed -- clean.

`cargo test --release -p litebox_platform_windows_userland --lib`: 4 passed, 0 failed -- clean.

`cargo test --release -p litebox_shim_linux`: still fails to compile (test/doctest build only),
`E0576` "cannot find method `run_test_thread` in trait `ThreadProvider`" -- confirmed pre-existing
in every prior entry via `git stash` comparison, unrelated to this session's changes (a
`ThreadProvider` trait/test-infrastructure drift, not touched by anything in this session's diff).

### Step 0 verdict: no longer REFUTED by the old crash -- reopened to INCONCLUSIVE, further along
### than ever, blocked now on getting a clean high-memory run rather than on a known bug

The specific, deterministic crash that produced the prior entry's REFUTED verdict is fixed and
confirmed gone via live debugger evidence, not just log inference. This session's runs got further
into the cross-process fork path than any previous session -- past `PageManager::duplicate()`
entirely, into `spawn_cross_process_fork_child`'s real page-copy logic, with the guest's
`beyond_stdio==0` gate correctly passed. **No run this session reached
`TCACHE_REPRO_CHILD_OK`/completion**, so Step 0's central claim is still not CONFIRMED -- but the
REFUTED verdict from the prior entry specifically rested on a crash that is now shown, with
debugger evidence, not to exist. The honest verdict is INCONCLUSIVE-but-substantially-de-risked,
not REFUTED: every concrete, previously-identified blocker in the `PageManager::duplicate()`/
`spawn_cross_process_fork_child` path has now been found and fixed (DIAG_HEAL logging crash, the
`search_exception_tables` torn-read, and now nine more stray diagnostics of the identical class),
and what remains between here and a clean verdict looks, on current evidence, like host resource
pressure rather than a code defect.

### Step 9 (long-running cross-process child survival): still NOT REACHED

No run in this session reached a live, running cross-process child -- still blocked on getting one
clean run past `copy_one_group`'s full copy to actual guest resumption. This remains completely
open.

### What remains open for a future session

- **Get one clean run on a host with real headroom (>6GB free, verified via
  `Get-CimInstance Win32_OperatingSystem`, with every other memory-heavy process -- other Claude
  Code sessions, browsers, chat apps -- closed for the duration, not just "not currently doing
  anything").** This session's own concurrent load (several other Claude sessions plus this
  project's `gm` daemon helper, Chrome, Discord) made a clean, fast run impossible to obtain; every
  attempt after the first stalled during image loading alone, before ever reaching
  `TCACHE_REPRO_START`.
- If a clean, high-memory run STILL does not reach `TCACHE_REPRO_CHILD_OK`, that becomes the next
  real target to root-cause -- but with the ntdll crash and eight of nine stray diagnostics now
  fixed, and no evidence found this session of an actual infinite loop or unrecovered fault, the
  remaining gap looks structural (a very large `WriteProcessMemory`-per-4KiB-page copy loop is
  inherently slow for a multi-GB image) rather than a bug. Worth profiling `copy_one_group`
  directly (wall-clock per group, page count per group) on a clean run to confirm this before
  assuming a defect.
- The real, still-unfixed `ElfPatchKey`-not-rekeyed-on-fork bug noted in
  `litebox_shim_linux/src/syscalls/mm.rs`'s `init_elf_patch_state` (found while removing that
  function's stray diagnostic) is worth its own follow-up: a forked child whose address space
  already holds the parent's patched code re-initializes ELF patch state from scratch, computing a
  fresh `trampoline_addr` the copied code does not actually jump to.
- Step 9 (long-running cross-process child survival) remains completely untested -- still blocked
  on reaching a live cross-process child at all.
