# The Windows VEH guest-fault handler: why it does what it does

This documents the reasoning behind several non-obvious decisions in
`litebox_platform_windows_userland/src/lib.rs`'s vectored exception handler (the function that
receives every `AddVectoredExceptionHandler`-routed fault, both guest-mode and host-mode). The
inline comments at each site now just point here; this is the full reasoning, current as of
2026-09-09/10. The `RtlpUnwindPrologue` crash this handler was long implicated in is closed — it was
`VEH_FRAME_STRIDE` being 168 bytes short of the two frames it must cover, fixed by `0473cc3` and
bisected live 2026-09-15; see `AGENTS.md`'s "Closed" section. `fork_verify.rs`'s own doc comments
cover the stale-pointer-healing machinery referenced below.

## Unrecovered access violations now terminate instead of `EXCEPTION_CONTINUE_SEARCH`

A WER minidump (AGENTS.md pass 246) proved that returning `EXCEPTION_CONTINUE_SEARCH` for an
unrecovered `is_in_guest` fault is not merely unproductive -- it is actively harmful.
`switch_to_guest`'s trampoline (`switch_to_guest_sysret`) enters guest code via a bare `jmp`,
never a `call`, deliberately adopting the guest's own `rsp` with no host stack frame set up at
all -- so there is no legitimate call chain for Windows' SEH machinery to walk back through once
it takes over. The captured dump showed `ntdll!RtlVirtualUnwind2` itself faulting while
attempting exactly this: reading a stale `UWOP_ALLOC_SMALL`-accumulated stack-offset value out of
its own internal unwind-context struct and dereferencing it as a pointer, because no real
`RUNTIME_FUNCTION`/`UNWIND_INFO` entry describes this jump-based guest frame.
`EXCEPTION_CONTINUE_SEARCH` was previously reached unconditionally on the very first unrecovered
AV (before the sibling repeat-count circuit breaker has a chance to intervene, since that only
fires from the 65th identical repeat onward) -- meaning this exact `ntdll` corruption was hit on
every single genuine first-chance unrecovered guest fault, not just a rare repeated-fault edge
case. Fix: terminate cleanly instead for this case -- a guest-mode fault with no recognized
exception-table entry is not something Windows' own unwind path can ever safely process, so
handing it onward can only make things worse.

**Track-B extension (this also applies to host-mode faults, not just guest-mode).** The original
fix above only covered `is_in_guest == true`, on the reasoning that `switch_to_guest`'s bare-`jmp`
frame is the only one with no legitimate unwind chain. Live evidence from the fork-without-exec
investigation (five consecutive deterministic `LITEBOX_PROCESS_FORK=1` repros, reproduced with no
debugger ever attached to rule out an attach artifact, and reproduced identically with Windows
Error Reporting fully disabled -- both `HKCU\...\Windows Error Reporting\Disabled` and
`DontShowUI` set -- to rule out a WER-specific hang) proved the identical failure mode also
happens for `is_in_guest == false` (host-mode) faults: the `[diag-unrecov-av]` branch fires with
`is_in_guest=false` (confirmed in every capture), falls through unmodified to
`EXCEPTION_CONTINUE_SEARCH`, and the target thread is subsequently observed via
`Get-Process`/`cdb -pv` (non-invasively) parked forever at `WaitReason=Suspended,
ThreadState=Wait`, CPU pinned at 0 (a real kernel suspend, not a spin loop -- confirmed by
sampling `Process.CPU` twice, three seconds apart, with zero delta), inside `ntdll.dll` with no
other thread in the process ever alive to have called `SuspendThread` a second time (exhaustively
confirmed: `ThreadHandle::interrupt` and `ctxwatch_arm_other_threads`, this file's only two other
`SuspendThread` call sites, were both instrumented behind `LITEBOX_DIAG_INTERRUPT=1` and never
fired during any hang). This matches the Windows Application event log's own record of prior
`LITEBOX_PROCESS_FORK=1` runs on this exact host crashing with exception code `0xc0000409`
(`STATUS_STACK_BUFFER_OVERRUN`, Windows' fast-fail code) at a fixed `ntdll.dll` offset -- the same
"Windows' own unwind/exception path cannot safely continue past this fault" hazard, just
reachable from host-mode fault sites too (this crate's own host-mode call chains -- the VEH
trampoline's per-depth scratch-stack frames, `memcpy_fallible`/`write_u32_fallible`-class fallible
accessors, the `recover` fixup's own compiler-generated epilogue resumed via a raw `context.Rip`
write with no corresponding `call`, and the `switch_to_guest*` family generally -- are exactly as
unwind-info-hostile as the guest-mode jump already fixed). So: extend the same clean,
deterministic termination to every unrecovered AV, not just guest-mode ones.

**Why `RaiseFailFastException`, not a self-`TerminateProcess`.** Live evidence this session: a
self-`TerminateProcess(GetCurrentProcess(), ...)` call made from exactly this position (inside the
VEH, on the very thread that is mid-exception-dispatch for the fault being handled) does not
reliably terminate the process on this host/Windows build -- confirmed directly: the diagnostic
print immediately before the call DID appear in the captured log (so the code path was genuinely
reached and ran), yet the process was independently observed via `Get-Process` 30+ seconds later
still alive, its sole thread still parked at `WaitReason=Suspended`/`HasExited=False`. An external
`Stop-Process -Force` (a `TerminateProcess` call from a *different* process) against the same PID
succeeded immediately with no error. This is consistent with a documented Windows caveat: a
thread already inside kernel-mode exception/debug-port delivery for its own fault cannot always
complete a self-`TerminateProcess` of that same process, because the call itself can block behind
the very kernel-mode exception protocol this code is trying to escape. `RaiseFailFastException` is
Windows' purpose-built "abandon immediately, no unwind, no SEH second-chance dispatch" primitive
(the same mechanism `__fastfail`/heap-corruption detection uses) -- unlike `EXCEPTION_CONTINUE_SEARCH`
(returns to the same exception dispatcher already failing to make progress) or `TerminateProcess`
(blocked behind that same dispatcher per the evidence above), a fail-fast exception is delivered
through an entirely separate, always-fatal kernel path that does not wait on the ordinary
exception-port protocol.

## FS_BASE-reset repair (both host-mode and guest-mode)

Windows clears the current thread's `FS_BASE` MSR back to 0 on its own initiative, apparently as
part of ordinary scheduling (observed to recur many times per second under load, e.g. during
`apk add nodejs`'s guest dynamic-linking/TLS-heavy startup). A guest `mov %fs:...` hit while
`FS_BASE` is 0 reads/writes through linear address `0 + offset` instead of the real TLS block,
which is (almost always) unmapped and therefore an ordinary `#PF` here, reported as
`EXCEPTION_ACCESS_VIOLATION` -- indistinguishable, without an explicit check, from a genuine guest
segfault. Detection and repair happen *before* any other exception-code-specific handling (in
particular before the `EXCEPTION_SINGLE_STEP` triage that hands off to
`fork_verify::on_single_step`, which has no notion of FS_BASE at all and would misclassify the
trap or waste the step if run against a thread whose FS_BASE is transiently wrong).

Repair happens in place, without ever leaving guest mode: just `wrfsbase` the stored value back
and retry the exact same faulting instruction via `EXCEPTION_CONTINUE_EXECUTION`. This used to
instead route through `interrupt_callback` (`set_context_to_interrupt_callback`), which is far
more expensive -- it leaves guest mode, saves the full guest context, and takes a `NtContinue`
round-trip through host Rust code before `switch_to_guest` gets back around to restoring FS_BASE
and re-entering the guest. Under the same scheduler pressure that causes FS_BASE to be cleared in
the first place, that round-trip reliably took long enough for FS_BASE to be cleared *again*
before the guest completed even one more instruction, producing an unbounded livelock: thousands
of access violations in a row, forward progress permanently stalled -- the reported indefinite
hang (`LITEBOX_VEH_TRACE=1` traces from hung runs showed exactly this pattern: repeated
`EXCEPTION_ACCESS_VIOLATION` with `rdfsbase() == 0` at a different `rip` each time, `is_verifying
== false`, never reaching a third occurrence of the same instruction). Fixing FS_BASE directly in
the handler removes every one of those host round-trip's kernel transitions from the recovery
path: a single MSR write plus a `CONTINUE_EXECUTION` return, no syscalls, no context save, no
scheduling-visible event of its own to compound the problem.

This does forgo the old comment's stated rationale for going through `interrupt_callback` ("avoid
missing a real interrupt that arrives while resuming the guest"): a pending interrupt is not
inspected before resuming. This is safe: interrupt/signal delivery to a running guest is already
only ever "eventually", never guaranteed at a specific instruction boundary (true on real hardware
too), and `ThreadHandle::interrupt` does not depend on this path at all -- it suspends the target
thread directly and rewrites its context itself, which works correctly regardless of whether this
handler happens to run in between. A real interrupt is caught at the next point that already
checks for one (the next syscall, or the next `ThreadHandle::interrupt` inspection), exactly as it
would be if this access violation had not happened to occur at all.

Two guards gate the guest-mode repair specifically (`faulting_instruction_has_fs_override`,
`context_snapshot.Rip != 0`): without them a real guest fault coinciding with `rdfsbase() == 0`
(e.g. a null-pointer dereference with no FS-segment prefix, or `Rip == 0` meaning this isn't a
real instruction at all) gets misdiagnosed as an FS_BASE reset and retried forever. Confirmed live
via a `process.title = <string>` repro under Node.js, where a plain `mov rdx, [rdx+0x788]` (no FS
override) with `rdx` already null was being "repaired" and retried unboundedly on a background
guest thread. A single `wrfsbase` write can also lose a race against another Windows-initiated
FS_BASE reset under high-frequency triggering (AGENTS.md pass 302) -- both repair sites verify and
retry (bounded, 8 attempts) before resuming rather than writing once and hoping.

**Single-step path needs the same repair, and matters far more there.** A single-step trap while
in guest mode belongs to the post-`fork()` verification machinery (`EFLAGS.TF` is masked out of
every guest-visible eflags value, so the guest can never arm it itself) -- either a clean step
(re-arm TF and resume without leaving guest mode) or a catch of the child executing/writing
through a stale pointer into the parent's address space (fall through to the normal exception path
with a synthesized access violation so the child dies exactly as on real hardware). Single-
stepping means every guest instruction is its own kernel round-trip through this handler -- exactly
the kind of scheduling-visible event the FS_BASE-reset behavior above is keyed off of -- so a
`fork()` child under verification hits the reset on very nearly every single instruction
(confirmed via `LITEBOX_VEH_TRACE=1`: >99% of `on_single_step` calls during a real `apk add
nodejs` run observed `rdfsbase() == 0`). Before this fix, this path had no FS_BASE repair of its
own: `on_single_step` only *logged* the corruption and proceeded with its rip/instruction
classification regardless (safe, since it never reads `%fs:`-relative memory itself), then
re-armed `TF` and resumed the *original* guest instruction with FS_BASE still zero. If that
instruction touched `%fs:`, it then took a *second* trap (`EXCEPTION_ACCESS_VIOLATION`), which the
repair above fixes and retries via `CONTINUE_EXECUTION`, but with `TF` still armed the whole time,
so the very next instruction immediately single-steps again -- and if FS_BASE has already been
reset yet again by then (the common case), the two traps alternate in an extremely tight loop:
thousands of round trips for a handful of instructions of real forward progress, exactly the
"quadratic-ish" slowdown reported as an apparent hang. Repairing FS_BASE in place here, before
`on_single_step` runs, means the guest instruction that resumes after this step always sees
correct FS_BASE the first time: one MSR rewrite replaces two full VEH dispatches.

## A stale source-range `rip` can arrive as a raw AV instead of `EXCEPTION_SINGLE_STEP`

`fork_verify::on_single_step` already heals a stale, untranslated source-range `rip` (case (1) in
its own doc comment), but that case does not always announce itself as `EXCEPTION_SINGLE_STEP`:
whether Windows delivers a clean `#DB` trap (the page the stale address names is still resident,
so the CPU can fetch and execute it under `TF` before this handler ever sees it) or a raw
`EXCEPTION_ACCESS_VIOLATION` (the page is not resident at all) is incidental paging state at that
instant, not something the single-step-only dispatch distinguishes. Confirmed live
(litebox-xfce-1, `dbus-daemon` fork-child investigation, `LITEBOX_DIAG_FATALDUMP=1`/
`LITEBOX_VEH_TRACE=1`): a thread single-stepping cleanly under verification set `rip` to a
source-range value via an ordinary instruction, and the very next event on that thread was a raw
`EXCEPTION_ACCESS_VIOLATION` with the fault address equal to that same `rip` -- an execute fault
reaching this handler entirely outside the `EXCEPTION_SINGLE_STEP` branch, so
`fork_verify::on_single_step`'s case (1) never ran. The fix mirrors case (1) exactly (translate via
the same relocation map already proven correct for every other register at `fork()` time, resume
at the translated address) rather than reimplementing it, and fires only for a genuinely
`is_verifying` thread, only on an EXECUTE-shaped guest-mode AV whose fault address is a real, exact
`is_in_source` membership hit (never a coincidental numeric overlap) -- the narrowest fix this gap
admits, touching no other register or memory, matching the bounded, deterministic shape every safe
fix in `fork_verify.rs` already uses.

## The re-entrant heal lock

`lock_fork_verify_heal_reentrant` is held across both the AV-path healers and the
`on_single_step` call further down (they are sequential alternatives for the same fault, never
nested), so no two threads' healing sequences for two different faults ever interleave. It must be
*re-entrant* blocking acquisition, not the bounded-spin `try_lock` an earlier revision used: that
earlier version's give-up path healed UNSERIALIZED -- exactly the hazard this lock exists to
prevent, in exactly the contended case where it matters.
