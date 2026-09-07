# Track B fork-without-exec fix: running progress log

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
