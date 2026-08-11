// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Post-`fork()` execution verification for `fork()`-created child threads.
//!
//! # The problem
//!
//! LiteBox's `fork()` duplicates the parent's guest address space into *fresh, different* host
//! addresses (see [`litebox::mm::PageManager::duplicate`]) and translates the child's captured
//! CPU registers into the new address space. Raw pointer values that were already sitting in
//! *memory* at the moment of the `fork()`, however, are copied verbatim and are therefore stale
//! in the child:
//!
//! * return addresses the parent's `call` instructions pushed onto the stack,
//! * pointers spilled into stack slots, globals, or heap structures.
//!
//! On real Linux this is harmless: the child gets its own address space at the *same* virtual
//! addresses, so a copied pointer is still correct. Under LiteBox it is not -- and, critically,
//! it does not fault either, because `fork()` only ever *adds* mappings and never unmaps the
//! parent's, so the stale address is still live, mapped, and (for code) executable memory
//! belonging to the *still-running parent*. A child that `ret`s to a stale return address
//! genuinely resumes executing the parent's copy of the code, and any store that code performs
//! relative to that (unrelocated) `rip` -- or through any other stale pointer -- lands directly in
//! the parent's live state, silently corrupting a process that is still running.
//!
//! This is exactly the case the `Vmem::duplicate` doc comment flags as unsupported, and it is
//! reachable in practice: `ash`'s `fork()` -> `forkchild()` cleanup -> `execve()` sequence (i.e.
//! any `sh -c "a; b"`) hits it every time, corrupting the parent's `g_parsefile` and hanging the
//! shell forever.
//!
//! # What this module does
//!
//! It does **not** try to repair *arbitrary* stale pointers. Enumerating every place a stale
//! pointer could surface (code pointers, data pointers, syscall arguments, ...) is unbounded, and
//! guessing the "right" translation for a value that may not even be a pointer introduces
//! corruption of its own -- an earlier, more ambitious repair attempt that also tried to fix up
//! syscall arguments confirmed this the hard way (see `.gm/prd.yml`): the child ultimately
//! executed into invalid code anyway.
//!
//! What it *does* repair is one narrow, fully-bounded case: `litebox_shim_linux`'s `fork()`
//! handler already translates every CPU register at the instant `fork()` returns
//! (`sys_clone`'s `translate_reg!`) and proactively rewrites the small, deterministic set of
//! stale pointers libc's own post-`fork()` unwind reads back out of the child's stack/TCB before
//! it ever resumes (`fixup_stale_stack_pointers`). This module is what catches the same class of
//! value reached a different way (e.g. copied into a register from a location that proactive pass
//! does not cover, then used or re-spilled by the child's own code) -- from the moment a `fork()`
//! child resumes until it reaches `execve`/`exit`/`exit_group` (after which the parent's addresses
//! are unreachable and the danger is over), the child runs with `EFLAGS.TF` set. On each
//! single-step trap, before the next instruction runs:
//!
//! 1. **Code pointers.** If `rip` itself has landed inside one of the *source* (parent) ranges,
//!    the child is about to execute the parent's code -- almost always a `ret` to a return
//!    address a `call` pushed before `fork()`. Since the source ranges are exactly what
//!    `duplicate()` relocated, `rip` (and `rbp`, equally a live register at this exact trap) is
//!    deterministically translatable the same way any other register is; translate and resume
//!    at the destination address instead of killing.
//! 2. **Data pointers.** Otherwise the instruction at `rip` is decoded (via `iced-x86`), and if
//!    it writes memory, its effective address is computed from the operand plus the live register
//!    values in the trapped context. If that address falls inside a source range, the register(s)
//!    the instruction names as its memory base/index are themselves translated (never a blind
//!    scan of unrelated registers or memory) and the *same* instruction is retried.
//! 3. **Stale pointers reachable through a memory read.** Any instruction with an explicit memory
//!    operand -- not only a `call [mem]`/`jmp [mem]` -- whose effective address is in a
//!    destination range (a slot the child legitimately owns) and whose *stored value* is itself a
//!    stale, untranslated source-range pointer gets that slot healed in place, unconditionally. A
//!    plain load (`mov reg, [slot]`) that stashes the stale value in a register for a *later*
//!    `call reg` is exactly as much of a stale-pointer vector as a direct `call [slot]` -- case (1)
//!    above still eventually catches the resulting stale `rip`, but has no memory operand at that
//!    later instruction to trace back to the slot it came from, so the slot itself never gets
//!    healed and every subsequent read/call through it re-triggers the identical trap. Healing at
//!    the read closes that gap: once the slot holds the correct destination pointer, every later
//!    read through it (a `dtv`/TCB-style field or GOT/PLT entry reread on every call, the common
//!    case, not the exception) is already correct.
//!
//! Every repair only ever substitutes a value already proven translatable via the exact relocation
//! map used for every other register at `fork()` time -- never a guess. If a stale `rip` or
//! effective address is *not* translatable (falls in a source range `duplicate()` never recorded,
//! which should not happen but is not assumed), the child is killed: synthesizing an access
//! violation and letting it flow through the platform's ordinary exception path, so the shim
//! raises a perfectly normal `SIGSEGV` on the child; the child's exit status is recorded and the
//! parent's `wait4()` unblocks as usual.
//!
//! # Why this cannot produce false positives
//!
//! The source ranges are the parent's *pre-`fork()`* mappings, and duplication allocates the
//! child's copies specifically *excluding* them (`Vmem::new_excluding`), so source and destination
//! ranges are disjoint by construction. Consequently:
//!
//! * writes to the child's own relocated stack, `.data`, or heap are in a *destination* range and
//!   are never flagged;
//! * writes to memory the child `mmap`s after the `fork()` are in neither range and are never
//!   flagged;
//! * `rip` inside LiteBox's own syscall trampoline (`call syscall_callback`, which the syscall
//!   rewriter emits in place of every `syscall` instruction) is a *host* address, in neither
//!   range, and is never flagged.
//!
//! The only thing that lands in a source range is an address that was never translated -- which
//! is precisely the bug.
//!
//! # `EFLAGS.TF` lifecycle -- why it must be cleared on every path out of guest mode
//!
//! `TF` is armed only while resuming a verified child (`switch_to_guest`) and is otherwise the
//! exclusive property of this module. Two places matter beyond the obvious single-step handling
//! in [`on_single_step`], because a `#DB` raised while `TF` is live but the current thread is not
//! `is_in_guest` is *unhandled* by [`crate::vectored_exception_handler`] and crashes the whole
//! host process (`STATUS_SINGLE_STEP`, `0x80000004`), not just the child:
//!
//! * When [`vectored_exception_handler`](crate::vectored_exception_handler) redirects a trapped
//!   context into `exception_callback`/`interrupt_callback` (whether for the access violation
//!   this module synthesizes on detection, or for an unrelated genuine exception that happens to
//!   occur mid-verification), `TF` must be cleared from that context first -- control never
//!   returns to [`on_single_step`] to do it once host code starts running.
//! * The syscall rewriter's trampoline reaches `syscall_callback` via an ordinary `call`
//!   instruction executed *as* the guest's single-stepped next instruction, not via the
//!   exception-handler redirect above -- so `TF` is still live in the CPU when `syscall_callback`
//!   starts running, and it clears it itself, first thing, before any other host instruction.

use iced_x86::{Decoder, DecoderOptions, Instruction, OpAccess, OpKind, Register};
use windows_sys::Win32::Foundation as Win32_Foundation;
use windows_sys::Win32::System::Diagnostics::Debug::{CONTEXT, EXCEPTION_RECORD};

use crate::TlsState;

/// The x86 `EFLAGS.TF` (trap flag) bit: when set, the CPU raises `#DB` after every instruction.
///
/// This bit is owned exclusively by this module. It is masked out of every guest-visible eflags
/// value (`litebox_common_linux::arch::SAFE_USER_EFLAGS` omits it, and `save_guest_context` clears
/// it), so a guest can never observe or arm it, and it can never survive into a context resumed
/// after verification has ended.
pub(crate) const EFLAGS_TF: usize = 1 << 8;

/// The maximum length of an x86-64 instruction, and hence how many bytes need to be readable at
/// `rip` to decode one.
const MAX_INSTRUCTION_LEN: usize = 15;

/// Whether the current thread is a `fork()` child whose execution is being verified.
pub(crate) fn is_verifying(tls: &TlsState) -> bool {
    tls.fork_verify.borrow().is_some()
}

/// The `EFLAGS` bits to add when entering guest mode on this thread: `TF` if this thread is a
/// `fork()` child under verification, nothing otherwise.
pub(crate) fn entry_eflags_tf(tls: &TlsState) -> usize {
    if is_verifying(tls) { EFLAGS_TF } else { 0 }
}

/// What [`on_single_step`] decided about the trapped instruction.
pub(crate) enum StepOutcome {
    /// Nothing suspicious; the child has been re-armed and should resume directly.
    Continue,
    /// The child is about to execute at, or write through, a stale pointer into its parent's
    /// address space. It must be killed before the instruction runs.
    StalePointer {
        /// The offending address: `rip` for a code pointer, the write's effective address for a
        /// data pointer.
        address: usize,
        /// Whether the offending access is a write (as opposed to an instruction fetch).
        is_write: bool,
    },
}

/// Inspect a single-step trap taken in guest mode on a `fork()` child under verification.
///
/// Returns [`StepOutcome::Continue`] after re-arming `TF` if the instruction about to execute is
/// benign; see the module documentation for what "benign" means and why the check cannot produce
/// false positives.
pub(crate) fn on_single_step(tls: &TlsState, context: &mut CONTEXT) -> StepOutcome {
    // `EFLAGS_TF` is a small, fixed bit-mask constant (`1 << 8`); the truncating cast to `u32`
    // (the width of `CONTEXT::EFlags`) never actually loses any bits.
    #[allow(clippy::cast_possible_truncation)]
    let eflags_tf = EFLAGS_TF as u32;

    // The diagnostic code-page watchpoint borrows `TF` to step over a trapped write (see
    // `codewatch`). That step is not a verification step, and on a thread that is not otherwise
    // under verification it is the *only* reason `TF` is set -- so consume it here, re-arming the
    // page, and clear `TF` unless verification wants it kept armed.
    if codewatch::on_single_step_rearm() {
        if !is_verifying(tls) {
            context.EFlags &= !eflags_tf;
        }
        return StepOutcome::Continue;
    }

    let borrow = tls.fork_verify.borrow();
    let Some(relocations) = borrow.as_ref() else {
        // Not a thread under verification: TF must have leaked in from somewhere. Clear it and
        // resume rather than trapping forever.
        context.EFlags &= !eflags_tf;
        return StepOutcome::Continue;
    };

    // This module only builds for `target_arch = "x86_64"` (see the crate's top-level `cfg`), so
    // `usize` and `u64` are the same width here; the cast is exact, never truncating.
    #[allow(clippy::cast_possible_truncation)]
    let rip = context.Rip as usize;

    if crate::veh_trace_enabled() {
        let fsbase = unsafe { litebox_common_linux::rdfsbase() };
        if fsbase == 0 {
            // `vectored_exception_handler` repairs a zeroed FS_BASE for every `EXCEPTION_SINGLE_
            // STEP` unconditionally, immediately before calling this function (see its comment on
            // why this matters far more here than on the non-single-stepped path) -- so this should
            // now be unreachable in the common case where `WindowsUserland::get_thread_fs_base()`
            // holds a real saved value. Seeing this fire means either the repair's `saved != 0`
            // guard rejected a not-yet-initialized FS_BASE (benign, early in thread startup) or the
            // repair genuinely did not stick, which would be a new, distinct bug worth
            // investigating rather than the already-understood reset-under-scheduling-pressure
            // case this used to log routinely.
            eprintln!(
                "[fork_verify] tid={:?} on_single_step: rdfsbase()==0 at rip={rip:#x} (unexpected: FS_BASE repair should have already run)",
                std::thread::current().id(),
            );
        }
    }

    // (1) Code pointer: `rip` itself landed in the parent's pre-`fork()` code. This is almost
    // always a `ret` to a return address a `call` pushed onto the stack *before* `fork()` was
    // invoked -- e.g. musl's own post-`clone()` unwind back through its cancellation-point
    // wrapper into libc/CRT frames that existed above `fork()` on the call stack, something every
    // single `fork()` does, deterministically, before the child ever reaches `execve()`. Most of
    // these are already fixed up proactively, once, at `fork()` setup time (see
    // `fixup_stale_stack_pointers` in `litebox_shim_linux`), which handles the common case where
    // the stale value already sits in scannable stack/TCB memory before the child resumes. This
    // is the fallback for the same class of value reached a different way (e.g. copied into a
    // register from a source location the proactive scan does not cover, then pushed by the
    // child's own code after resuming) -- since the source ranges are exactly the ranges
    // `duplicate()` relocated, `rip` is deterministically translatable the same way register
    // state is (see `sys_clone`'s `translate_reg!`): resume at the translated destination rather
    // than killing. This is narrower than the "repair stale pointers in place" approach ruled out
    // earlier in this investigation (see module docs / `.gm/prd.yml`) -- that attempt tried to
    // additionally repair arbitrary data pointers and syscall arguments, an unbounded problem
    // that left the child executing invalid code. This handles only the one case that is fully
    // bounded and safe: `rip` (and, since it is equally a live register at this exact trap and
    // not a value read back out of arbitrary memory, `rbp`) translated via the exact same
    // relocation map already proven correct for CPU registers, landing on byte-identical
    // relocated code.
    if relocations.is_in_source(rip) {
        if crate::veh_trace_enabled() {
            eprintln!(
                "[fork_verify] tid={:?} on_single_step: rip={rip:#x} is_in_source=true",
                std::thread::current().id(),
            );
        }
        if let Some(translated_rip) = relocations.translate(rip) {
            litebox_util_log::warn!(
                rip:? = rip, translated_rip:? = translated_rip;
                "fork_verify: stale CODE pointer detected, translating and resuming"
            );
            context.Rip = translated_rip as u64;
            #[allow(clippy::cast_possible_truncation)]
            let rbp = context.Rbp as usize;
            if let Some(translated_rbp) = relocations.translate(rbp) {
                context.Rbp = translated_rbp as u64;
            }
            // This trap fires *after* the CPU has already fetched (and, for a `ret`, already
            // popped) the stale value into `rip` -- fixing only the live register here repairs
            // this one execution but leaves the stack slot the value was read from still holding
            // the untranslated original. If the same call site executes again later (any
            // `call`/`ret` pair through the same code path, e.g. a loop or a shared helper called
            // more than once, which is the common case, not the exception), the next `ret` pops
            // that same never-healed slot and this exact trap fires again on the identical stale
            // value -- observed directly via `LITEBOX_VEH_TRACE=1`: the same stale source-range
            // `rip` recurring verbatim across many single-step traps in one run, each time patched
            // only in-register. Left unaddressed, this repeated stale round-trip can desynchronize
            // other state and the child eventually executes into corrupted-looking code (observed
            // downstream as a `STATUS_PRIVILEGED_INSTRUCTION` fault). The overwhelmingly common
            // way this trap fires is a `ret`, which has already incremented `rsp` past the popped
            // slot by the time this handler runs -- so the slot, if it still holds the stale value
            // verbatim, is at `rsp - 8`; patch it in place (a destination-range stack slot the
            // child legitimately owns, so this is exactly as safe as `fixup_stale_stack_pointers`
            // patching the same class of slot proactively). A no-op for any other way this trap
            // could fire (e.g. an indirect `jmp` through a register that never touched the stack):
            // the slot below `rsp` simply won't hold the stale value, so the guarded write below
            // never fires.
            #[allow(clippy::cast_possible_truncation)]
            let rsp = context.Rsp as usize;
            if let Some(ret_addr_slot) = rsp.checked_sub(core::mem::size_of::<usize>())
                && relocations.is_in_destination(ret_addr_slot)
                && read_usize_fault_tolerant(ret_addr_slot) == Some(rip)
            {
                if crate::veh_trace_enabled() {
                    eprintln!(
                        "[fork_verify] HEAL case=1 ret_addr_slot={ret_addr_slot:#x} in_exec_range={} old={rip:#x} new={translated_rip:#x}",
                        relocations.is_in_destination_executable_range(ret_addr_slot),
                    );
                }
                write_usize_fault_tolerant(ret_addr_slot, translated_rip);
            }
            context.EFlags |= eflags_tf;
            return StepOutcome::Continue;
        }
        litebox_util_log::warn!(rip:? = rip; "fork_verify: stale CODE pointer detected");
        return StepOutcome::StalePointer {
            address: rip,
            is_write: false,
        };
    }

    // `rip` outside the child's own duplicated address space entirely is LiteBox's own host code:
    // the syscall rewriter replaces every guest `syscall` instruction with a `call` to
    // `syscall_callback`'s host address, so this happens on every single syscall the child makes.
    // Single-stepping LiteBox's own code would be both pointless and fatal, so disarm here; the
    // next `switch_to_guest` back into the child re-arms `TF` automatically.
    if !relocations.is_in_destination(rip) {
        context.EFlags &= !eflags_tf;
        return StepOutcome::Continue;
    }

    // Keep stepping. Note this must happen even on the paths below that return `Continue` early
    // (e.g. an instruction we cannot decode), or verification silently stops.
    context.EFlags |= eflags_tf;

    // (2) Data pointer: decode the instruction about to execute and check where it writes.
    //
    // `rip` here is guest code (or LiteBox's own trampoline, which is host code that is equally
    // safe to read); reading `MAX_INSTRUCTION_LEN` bytes from it is safe in the same sense the
    // CPU fetching it is, except that the instruction may sit at the very end of a mapping. Read
    // it a byte at a time through a fault-tolerant read so a partial fetch simply yields a
    // shorter (still decodable, or harmlessly undecodable) buffer instead of faulting inside the
    // exception handler.
    let mut code = [0u8; MAX_INSTRUCTION_LEN];
    let len = read_code_bytes(rip, &mut code);
    if len == 0 {
        return StepOutcome::Continue;
    }

    let mut decoder = Decoder::with_ip(64, &code[..len], rip as u64, DecoderOptions::NONE);
    let instruction = decoder.decode();
    if instruction.is_invalid() {
        return StepOutcome::Continue;
    }

    let Some(address) = memory_write_address(&instruction, context) else {
        return StepOutcome::Continue;
    };

    if crate::veh_trace_enabled() && relocations.is_in_destination_executable_range(address) {
        eprintln!(
            "[fork_verify] tid={:?} on_single_step: WRITE INTO DESTINATION-EXECUTABLE range rip={rip:#x} address={address:#x} mnemonic={:?}",
            std::thread::current().id(),
            instruction.mnemonic(),
        );
    }

    if relocations.is_in_source(address) {
        // As with the code-pointer case above: the stale value driving this write is not
        // arbitrary guest data, it is the *base/index register* the instruction itself names
        // (`memory_base`/`memory_index`), most likely reloaded from one of the same pre-`fork()`
        // stack/TCB slots the proactive scan already covers for the common case, reached here a
        // different way (see the code-pointer case's comment). If that exact register is itself a
        // translatable source-range address, fixing the register (there is nowhere to write "the
        // address" back to after the fact) and retrying the *same* instruction is exactly as
        // sound as the register translation `sys_clone` already does for every GPR at fork-resume
        // time. Only do this for a register-relative form we can name and rewrite; anything else
        // still falls through to the kill below.
        if translate_memory_operand_registers(&instruction, context, relocations) {
            litebox_util_log::warn!(
                rip:? = rip, address:? = address, mnemonic:? = instruction.mnemonic();
                "fork_verify: stale DATA pointer detected, translating base/index register(s) and retrying"
            );
            // Do not advance rip: retry the same instruction now that its address-forming
            // register(s) have been corrected.
            return StepOutcome::Continue;
        }
        litebox_util_log::warn!(
            rip:? = rip, address:? = address, mnemonic:? = instruction.mnemonic();
            "fork_verify: stale DATA pointer write detected"
        );
        return StepOutcome::StalePointer {
            address,
            is_write: true,
        };
    }

    // (3) Indirect control transfer through a stale GOT/function-pointer-style memory slot:
    // `call [mem]` / `jmp [mem]` reads (never writes) the target address from memory, so case (2)
    // above -- which only inspects operands `iced-x86` classifies as writes -- never sees it. If
    // that read's effective address is itself in a DESTINATION range (i.e. it is the child's own,
    // legitimately-owned copy of some `.got`/`.data`/vtable-style slot -- never the parent's, since
    // a stale *address of the slot itself* would already have been caught as an ordinary data-read
    // fault or handled by `fixup_stale_stack_pointers`'s proactive pass) and the *value* stored
    // there is a stale, untranslated SOURCE-range pointer, heal the slot in place immediately, so
    // every subsequent call through the same slot (a `call [rip+offset]` PLT/GOT stub is by far the
    // most common shape, and gets executed repeatedly, not just once) reads the already-correct
    // destination pointer directly.
    //
    // Restricted deliberately to instructions `iced-x86` classifies as an indirect call/jmp
    // (rather than firing on every memory read whose loaded value happens to fall in a source
    // range): an earlier version of this fix generalized to any explicit-memory-operand read,
    // reasoning that a plain `mov reg, [slot]` feeding a *later* `call reg` was an equally valid
    // stale-pointer vector case (1) alone could not trace back to its origin slot. That
    // generalization introduced a real false-positive hazard this narrower form avoids: ordinary
    // small-integer program data can coincidentally fall inside a tracked source range (source
    // ranges can include low addresses, e.g. a small `brk`-adjacent value) with no relation to a
    // pointer at all, and "translating" it is not a no-op -- it corrupts a legitimate data slot by
    // overwriting it with an unrelated destination address. Observed directly: a slot healed with
    // `stale_value=286028520` (not a plausible 64-bit pointer) got overwritten with a `translated`
    // value that then desynchronized later execution instead of fixing anything. Restricting back
    // to call/jmp targets keeps the same soundness argument as case (1) and case (2): the read is
    // only ever treated as a pointer when the instruction itself is about to use it as one.
    //
    // `load_address` is ALSO excluded when it falls in the DESTINATION heap
    // (`is_in_destination_heap_range`), for the same reason `fixup_stale_elf_data_pointers`
    // excludes the heap from its own proactive scan (see that function's doc comment in
    // `litebox_shim_linux::syscalls::process`): the heap is dominated by live, allocator-managed
    // payload data, not code-pointer-shaped slots, so even restricting to call/jmp *targets* is
    // not enough there -- a heap slot can transiently hold a small integer that both (a) is the
    // explicit memory operand of some unrelated indirect call/jmp reached via a mis-decoded or
    // coincidental control-flow shape, and (b) happens to numerically fall in a tracked source
    // range. Confirmed live: this exact case was found healing (i.e. corrupting) a live heap slot
    // holding the tail bytes -- including the NUL terminator -- of a freshly-`fork()`ed shell's
    // `argv` string being prepared for `execve()`, observed via a real interactive repro
    // (`apk add nodejs` then `node --version`, and independently reproduced with plain `busybox`
    // after heap-churning fork/exec cycles) that stopped reproducing whenever this module's own
    // `LITEBOX_VEH_TRACE` tracing was enabled -- a timing-sensitivity signature consistent with a
    // narrow single-step healing false-positive, not a deterministic layout bug.
    if (instruction.is_call_near_indirect() || instruction.is_jmp_near_indirect())
        && let Some(load_address) = explicit_memory_operand_address(&instruction, context)
        && relocations.is_in_destination(load_address)
        && !relocations.is_in_destination_heap_range(load_address)
        && let Some(stale_value) = read_usize_fault_tolerant(load_address)
        && let Some(translated) = relocations.translate(stale_value)
    {
        if crate::veh_trace_enabled() {
            eprintln!(
                "[fork_verify] HEAL case=3 load_address={load_address:#x} in_exec_range={} old={stale_value:#x} new={translated:#x} rip={rip:#x}",
                relocations.is_in_destination_executable_range(load_address),
            );
        }
        litebox_util_log::warn!(
            rip:? = rip, load_address:? = load_address, stale_value:? = stale_value,
            translated:? = translated, mnemonic:? = instruction.mnemonic();
            "fork_verify: stale CODE pointer in indirect call/jmp target slot detected, patching slot in place"
        );
        write_usize_fault_tolerant(load_address, translated);
    }

    // (4) Register-indirect control transfer (`call reg` / `jmp reg`) through a value that was
    // itself loaded from a stale GOT/TCB-style memory slot one or more instructions earlier. Case
    // (3) above only sees the read when it is the *explicit memory operand of the call/jmp itself*
    // (`call [mem]`); it cannot see a `mov reg, [slot]` followed later by `call reg`, since that
    // call's only operand is a register, with no memory operand to trace back to a slot at all.
    // Case (1) still catches the resulting stale `rip` once the transfer actually happens (a stale
    // `rip` is a stale `rip` regardless of how it got there) and heals the live register -- but
    // with no slot to patch, the same call site reads the identical stale value out of the same
    // slot again next time, and this trap re-fires on the exact same `rip` forever. This is
    // precisely the shape `.gm/prd.yml`'s `residual-second-fork-verify-corruption-bug` row
    // documents: the same stale source-range `rip` recurring verbatim across many single-step
    // traps even with case (1)'s `[rsp-8]` slot healing active.
    //
    // Closing this safely (without reintroducing case (3)'s false-positive hazard above) requires
    // knowing, with certainty, that the *specific* value about to be used as a call/jmp target was
    // itself just read from a *specific* memory slot -- not merely that some earlier instruction
    // read some source-range-shaped value from memory. `last_memory_load` (updated unconditionally
    // below at the end of every step, independent of whether this step turned out to be
    // suspicious) tracks exactly that: the `(address, value)` of the most recent explicit-memory-
    // operand read, from any instruction, from the previous single-step. If this step is a
    // register-indirect call/jmp whose target register's value matches that recorded value
    // exactly, the slot it came from is safe to heal -- the same soundness argument as case (3),
    // just with one more established fact (the value truly is about to be used as a control-
    // transfer target, not merely data that resembles a pointer).
    //
    // `load_address` must itself be in the DESTINATION (child) address space before we write
    // through it -- the identical requirement case (3) enforces via `is_in_destination` just above.
    // Without this check, `load_address` can be a SOURCE-space (parent) address: per
    // `AddressRelocations::is_in_source`'s doc comment, the parent's original mappings are never
    // unmapped after `fork()`, so a stale, not-yet-relocated GOT/TCB slot address is still live,
    // writable host memory in the child's own process too -- `write_usize_fault_tolerant` cannot
    // tell the difference and will happily patch it. That silently corrupts the PARENT's own
    // live GOT/PLT-style slot with a DESTINATION-space (child) pointer, which the parent later
    // reads back and jumps/calls through, itself faulting (typically at a stale or NULL address,
    // since the child's mappings may have since been torn down by `exec()`/exit) -- observed in
    // practice as the parent shell's own untouched OS thread taking a first-chance
    // `STATUS_ACCESS_VIOLATION` with `CONTEXT.Rip == 0` moments after an unrelated `fork()` child
    // exits, confirmed live via a debugger (`Rip == 0`, faulting address `0`, an "attempt to
    // execute non-executable address 0" record, and a walked stack whose top return-address slot
    // is literally `0x0`) despite the parent thread never itself running under verification.
    //
    // `load_address` is also excluded when it falls in the DESTINATION heap, for the identical
    // reason case (3) excludes it (see that case's comment) -- `last_memory_load` itself never
    // records a heap-resident load in the first place (see the tracking update below), so that
    // half of this check is technically redundant today, but kept here too as defense in depth
    // against a future change to that tracking that reintroduces heap addresses without noticing
    // this heal path.
    if (instruction.is_call_near_indirect() || instruction.is_jmp_near_indirect())
        && explicit_memory_operand_address(&instruction, context).is_none()
        && instruction.op0_kind() == OpKind::Register
        && let Some(target_value) = register_value(instruction.op0_register(), context)
        && let Some((load_address, loaded_value)) = tls.fork_verify_last_load.get()
        && loaded_value == target_value
        && relocations.is_in_source(target_value)
        && relocations.is_in_destination(load_address)
        && !relocations.is_in_destination_heap_range(load_address)
        && let Some(translated) = relocations.translate(target_value)
    {
        if crate::veh_trace_enabled() {
            eprintln!(
                "[fork_verify] HEAL case=4 load_address={load_address:#x} in_exec_range={} old={target_value:#x} new={translated:#x} rip={rip:#x}",
                relocations.is_in_destination_executable_range(load_address),
            );
        }
        litebox_util_log::warn!(
            rip:? = rip, load_address:? = load_address, stale_value:? = target_value,
            translated:? = translated, mnemonic:? = instruction.mnemonic();
            "fork_verify: stale CODE pointer previously loaded from memory slot into register, patching slot in place"
        );
        write_usize_fault_tolerant(load_address, translated);
    }

    // Record this step's memory read (if any) for case (4) on the *next* step, regardless of
    // whether this step was itself flagged -- the read that matters is whichever one happened
    // most recently right before a register-indirect call/jmp, which may be several ordinary
    // (non-suspicious) steps earlier if intervening instructions don't also read memory.
    //
    // A heap-resident load is never recorded at all (rather than recorded and filtered out only
    // at heal time in case (4) above): the heap is live payload data, not a plausible source of a
    // genuine stale code pointer, so tracking it here serves no purpose case (4)'s exclusion check
    // doesn't already cover, and not recording it keeps this the single source of truth for "was
    // this ever a candidate slot" rather than splitting that decision across two places.
    tls.fork_verify_last_load.set(
        explicit_memory_operand_address(&instruction, context)
            .filter(|addr| !relocations.is_in_destination_heap_range(*addr))
            .and_then(|addr| read_usize_fault_tolerant(addr).map(|v| (addr, v))),
    );

    StepOutcome::Continue
}

/// If `instruction`'s memory operand's base and/or index register currently holds a value that
/// falls in one of `relocations`'s SOURCE ranges, rewrites that register (in `context`) to the
/// corresponding DESTINATION address and returns `true`. A no-op (returning `false`) if neither
/// register is translatable, so the caller can fall back to killing the child.
///
/// This deliberately only ever touches the specific register(s) `instruction` itself names as a
/// memory base/index -- never a blind scan of every register or of memory -- so it cannot "fix"
/// an unrelated register that happens to coincidentally look like a source-range address.
fn translate_memory_operand_registers(
    instruction: &Instruction,
    context: &mut CONTEXT,
    relocations: &litebox::mm::AddressRelocations,
) -> bool {
    let mut translated_any = false;
    for i in 0..instruction.op_count() {
        if instruction.op_kind(i) != OpKind::Memory {
            continue;
        }
        for reg in [instruction.memory_base(), instruction.memory_index()] {
            if matches!(reg, Register::None | Register::RIP | Register::EIP) {
                continue;
            }
            let Some(value) = register_value(reg, context) else {
                continue;
            };
            let Some(translated) = relocations.translate(value) else {
                continue;
            };
            write_register_value(reg, translated, context);
            translated_any = true;
        }
    }
    translated_any
}

/// Writes `value` into the 64-bit register (or enclosing 64-bit register, for narrower
/// sub-registers) named by `register` in `context`.
fn write_register_value(register: Register, value: usize, context: &mut CONTEXT) {
    let full = register.full_register();
    // This crate's only target is `x86_64`, so `usize` and `u64` are the same width; exact, never
    // truncating.
    #[allow(clippy::cast_possible_truncation)]
    let value = value as u64;
    match full {
        Register::RAX => context.Rax = value,
        Register::RBX => context.Rbx = value,
        Register::RCX => context.Rcx = value,
        Register::RDX => context.Rdx = value,
        Register::RSI => context.Rsi = value,
        Register::RDI => context.Rdi = value,
        Register::RBP => context.Rbp = value,
        Register::RSP => context.Rsp = value,
        Register::R8 => context.R8 = value,
        Register::R9 => context.R9 = value,
        Register::R10 => context.R10 = value,
        Register::R11 => context.R11 = value,
        Register::R12 => context.R12 = value,
        Register::R13 => context.R13 = value,
        Register::R14 => context.R14 = value,
        Register::R15 => context.R15 = value,
        // `register_value` never returns `Some` for anything else, so `translate_memory_operand_
        // registers` never calls this with any other register.
        _ => {}
    }
}

/// Reads up to `buf.len()` bytes of instruction encoding at `rip`, stopping at the first byte
/// that cannot be read. Returns how many bytes were read.
///
/// Guards against the (rare, but real) case of an instruction sitting so close to the end of a
/// mapping that a full 15-byte fetch would run off the end of it -- which would otherwise fault
/// inside the vectored exception handler.
///
/// Exposed under a `pub(crate)` alias below for diagnostic use from [`crate::vectored_exception_handler`].
pub(crate) fn read_code_bytes_for_diagnostics(rip: usize, buf: &mut [u8]) -> usize {
    read_code_bytes(rip, buf)
}

fn read_code_bytes(rip: usize, buf: &mut [u8]) -> usize {
    let page_size = 0x1000usize;
    // The CPU already fetched the instruction at `rip`, so at minimum the bytes up to the end of
    // `rip`'s own page are readable. Never read past that boundary unless the next page is
    // demonstrably part of the same committed region.
    let to_page_end = page_size - (rip & (page_size - 1));
    let readable = if to_page_end >= buf.len() || is_readable(rip + to_page_end) {
        buf.len()
    } else {
        to_page_end
    };
    // SAFETY: `rip` is the address the CPU just fetched an instruction from, so `readable` bytes
    // starting there are mapped and readable per the check above.
    unsafe {
        core::ptr::copy_nonoverlapping(rip as *const u8, buf.as_mut_ptr(), readable);
    }
    readable
}

/// Whether `addr` is in a committed, readable region of the host address space.
fn is_readable(addr: usize) -> bool {
    use windows_sys::Win32::System::Memory as Win32_Memory;
    const NO_ACCESS: u32 = Win32_Memory::PAGE_NOACCESS | Win32_Memory::PAGE_GUARD;

    let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
    let ok = unsafe {
        Win32_Memory::VirtualQuery(
            addr as *const core::ffi::c_void,
            &raw mut mbi,
            core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
        ) != 0
    };
    if !ok || mbi.State != Win32_Memory::MEM_COMMIT {
        return false;
    }
    mbi.Protect & NO_ACCESS == 0
}

/// Whether `addr` is in a committed, writable region of the host address space.
fn is_writable(addr: usize) -> bool {
    use windows_sys::Win32::System::Memory as Win32_Memory;
    const WRITABLE: u32 = Win32_Memory::PAGE_READWRITE
        | Win32_Memory::PAGE_WRITECOPY
        | Win32_Memory::PAGE_EXECUTE_READWRITE
        | Win32_Memory::PAGE_EXECUTE_WRITECOPY;

    let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
    let ok = unsafe {
        Win32_Memory::VirtualQuery(
            addr as *const core::ffi::c_void,
            &raw mut mbi,
            core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
        ) != 0
    };
    if !ok || mbi.State != Win32_Memory::MEM_COMMIT {
        return false;
    }
    mbi.Protect & WRITABLE != 0
}

/// If `instruction` writes to memory, computes the effective address it writes to from its
/// operands plus the live register values in `context`.
///
/// Returns `None` for instructions that do not write memory at all, and for the handful of forms
/// whose effective address cannot be resolved from the trapped register state alone.
fn memory_write_address(instruction: &Instruction, context: &CONTEXT) -> Option<usize> {
    // Does any operand write memory?
    let mut info_factory = iced_x86::InstructionInfoFactory::new();
    let info = info_factory.info(instruction);
    let writes_memory = info.used_memory().iter().any(|m| {
        matches!(
            m.access(),
            OpAccess::Write | OpAccess::CondWrite | OpAccess::ReadWrite | OpAccess::ReadCondWrite
        )
    });
    if !writes_memory {
        return None;
    }

    // Push/pop-style implicit stack writes target `rsp`, which is always translated for the
    // child, and are covered by the `rsp` value in the context anyway; the general path below
    // handles them via `OpKind::Memory` when they have an explicit memory operand, and via the
    // used-memory info otherwise. Compute the address from the explicit memory operand when there
    // is one -- that is the case that can carry a stale pointer.
    if let Some(address) = explicit_memory_operand_address(instruction, context) {
        return Some(address);
    }

    // No explicit memory operand: an implicit stack access (`push`, `call`, ...). Those target
    // `rsp`, which `fork()` translates, so they are not a stale-pointer vector -- and the
    // `is_in_source` check below would harmlessly reject them anyway. Report the stack address so
    // the check is still applied.
    let used = info.used_memory().first()?;
    if used.base() == Register::RSP || used.base() == Register::RBP {
        // Exact on this crate's only target (`x86_64`): a 64-bit displacement fits `usize` as-is.
        #[allow(clippy::cast_possible_truncation)]
        let displacement = used.displacement() as usize;
        return Some(register_value(used.base(), context)?.wrapping_add(displacement));
    }
    None
}

/// Computes the effective address of `instruction`'s explicit memory operand (if it has one) from
/// the live register values in `context`, regardless of whether that operand is read or written.
///
/// Returns `None` if `instruction` has no explicit `OpKind::Memory` operand, or its address cannot
/// be resolved from the trapped register state alone.
fn explicit_memory_operand_address(instruction: &Instruction, context: &CONTEXT) -> Option<usize> {
    for i in 0..instruction.op_count() {
        if instruction.op_kind(i) != OpKind::Memory {
            continue;
        }
        // Exact on this crate's only target (`x86_64`): a 64-bit displacement fits `usize` as-is.
        #[allow(clippy::cast_possible_truncation)]
        let mut address = instruction.memory_displacement64() as usize;
        match instruction.memory_base() {
            // `None`: no base register to add. `RIP`/`EIP`: RIP-relative addressing, and
            // `memory_displacement64` already folded in the instruction pointer -- both leave
            // `address` unchanged, but are kept as distinct arms since they mean different things.
            Register::None | Register::RIP | Register::EIP => {}
            base => address = address.wrapping_add(register_value(base, context)?),
        }
        if instruction.memory_index() != Register::None {
            let index = register_value(instruction.memory_index(), context)?;
            // `memory_index_scale()` is one of 1/2/4/8; always fits `usize`.
            #[allow(clippy::cast_possible_truncation)]
            let scale = instruction.memory_index_scale() as usize;
            address = address.wrapping_add(index.wrapping_mul(scale));
        }
        return Some(address);
    }
    None
}

/// Reads a `usize` from `addr` via a fault-tolerant access, returning `None` if `addr` is not in a
/// committed, readable region.
fn read_usize_fault_tolerant(addr: usize) -> Option<usize> {
    if !is_readable(addr) {
        return None;
    }
    // SAFETY: `is_readable` confirmed `addr` is in a committed, readable region of at least one
    // page; callers of this function only ever pass addresses inside a tracked destination range,
    // which -- by construction of `Vmem::duplicate` -- are always mapped with room for a full
    // `usize` (never split mid-word across mapping boundaries with different protection).
    Some(unsafe { core::ptr::read_unaligned(addr as *const usize) })
}

/// Writes `value` as a `usize` to `addr` via a fault-tolerant access, doing nothing if `addr` is
/// not in a committed region at all.
///
/// If `addr` is committed but currently read-only (e.g. a `.got`/`.data.rel.ro`-style slot RELRO
/// or the dynamic linker has already marked read-only post-relocation -- exactly the shape of a
/// GOT/PLT entry, one of the most common slots this module ever needs to heal), the region's
/// protection is temporarily switched to writable, the write performed, and the *original*
/// protection restored immediately after -- never left more permissive than it started. Without
/// this, healing a read-only slot silently no-ops (as a plain `is_writable` gate would), which is
/// exactly the residual this function's callers exist to close: a GOT/PLT-style slot healed once
/// by [`on_single_step`]'s memory-read case appeared to patch successfully (the call site logs
/// unconditionally) but the value silently never changed, so the identical stale pointer kept
/// being read back out on every subsequent call through the same slot.
fn write_usize_fault_tolerant(addr: usize, value: usize) {
    use windows_sys::Win32::System::Memory as Win32_Memory;

    if is_writable(addr) {
        // SAFETY: `is_writable` confirmed `addr` is in a committed, writable region; see
        // `read_usize_fault_tolerant`'s comment on why a full `usize` is always in-bounds here.
        unsafe { core::ptr::write_unaligned(addr as *mut usize, value) };
        return;
    }
    if !is_readable(addr) {
        // Not committed/accessible at all: nothing to patch.
        return;
    }

    // Committed and readable but not currently writable: temporarily flip to
    // `PAGE_EXECUTE_READWRITE` (a superset of every other protection this slot could legitimately
    // have -- read-only data, read-only+exec code, or already read-write, so widening to it and
    // back is always a strict round trip), write, then restore exactly what `VirtualProtect`
    // reports as the prior protection.
    let mut old_protect = 0u32;
    let ok = unsafe {
        Win32_Memory::VirtualProtect(
            addr as *mut core::ffi::c_void,
            core::mem::size_of::<usize>(),
            Win32_Memory::PAGE_EXECUTE_READWRITE,
            &raw mut old_protect,
        ) != 0
    };
    if !ok {
        return;
    }
    // SAFETY: the `VirtualProtect` call above just made this region writable, and
    // `read_usize_fault_tolerant`'s comment covers why a full `usize` is always in-bounds here.
    unsafe { core::ptr::write_unaligned(addr as *mut usize, value) };
    let mut restored = 0u32;
    unsafe {
        Win32_Memory::VirtualProtect(
            addr as *mut core::ffi::c_void,
            core::mem::size_of::<usize>(),
            old_protect,
            &raw mut restored,
        );
    }
}

/// Reads the 64-bit value of `register` (or the enclosing 64-bit register, for narrower
/// sub-registers) from the trapped context.
fn register_value(register: Register, context: &CONTEXT) -> Option<usize> {
    let full = register.full_register();
    let value = match full {
        Register::RAX => context.Rax,
        Register::RBX => context.Rbx,
        Register::RCX => context.Rcx,
        Register::RDX => context.Rdx,
        Register::RSI => context.Rsi,
        Register::RDI => context.Rdi,
        Register::RBP => context.Rbp,
        Register::RSP => context.Rsp,
        Register::R8 => context.R8,
        Register::R9 => context.R9,
        Register::R10 => context.R10,
        Register::R11 => context.R11,
        Register::R12 => context.R12,
        Register::R13 => context.R13,
        Register::R14 => context.R14,
        Register::R15 => context.R15,
        // Vector-index (`vsib`) registers, segment registers, and anything else cannot be
        // resolved from the integer context; skip the instruction rather than guess.
        _ => return None,
    };
    // A 32-bit base/index register is zero-extended to 64 bits, exactly as the CPU does. This
    // crate's only target is `x86_64`, so `usize` is 64 bits and neither conversion below actually
    // truncates: the first intentionally narrows to 32 bits before zero-extending back up, and the
    // second is a same-width reinterpretation.
    #[allow(clippy::cast_possible_truncation)]
    if register.size() == 4 {
        Some((value as u32) as usize)
    } else {
        Some(value as usize)
    }
}

/// Builds a synthetic `EXCEPTION_ACCESS_VIOLATION` record describing an access to `address`, so a
/// detected stale-pointer access flows through the platform's ordinary exception path and reaches
/// the shim as an ordinary `SIGSEGV` -- exactly what real hardware would have delivered.
pub(crate) fn access_violation_record(
    template: &EXCEPTION_RECORD,
    address: usize,
    is_write: bool,
) -> EXCEPTION_RECORD {
    let mut record = *template;
    record.ExceptionCode = Win32_Foundation::EXCEPTION_ACCESS_VIOLATION;
    record.NumberParameters = 2;
    record.ExceptionInformation[0] = usize::from(is_write);
    record.ExceptionInformation[1] = address;
    record
}

/// Diagnostic-only `PAGE_GUARD` watchpoint over a `fork()` child's own destination *executable*
/// pages.
///
/// # Why this exists
///
/// [`on_single_step`] is structurally blind to any write performed while `rip` is outside the
/// child's destination ranges: it disarms `TF` unconditionally there (see the
/// `!relocations.is_in_destination(rip)` early return), which is exactly the window in which
/// LiteBox's own host-side syscall servicing runs -- every guest `syscall` is rewritten into a
/// `call` to `syscall_callback`. A corruption of the child's *own copied code* that happens
/// during that window therefore produces no trace line at all, which is precisely what the
/// `base+0xa98a` `0xf4` (HLT) crash under investigation looks like.
///
/// A page-protection watchpoint has no such blind spot: it is a property of the *page*, not of
/// the thread's trap flag, so it fires for any write from any thread, host or guest code alike.
///
/// The protection used is `PAGE_EXECUTE_READ` (write permission simply dropped), **not**
/// `PAGE_GUard`-style one-shot guarding. `PAGE_GUARD` was tried first and is unusable here: it
/// traps on *every* access including instruction fetches, so the guest executing its own code
/// storms the handler thousands of times per millisecond and never makes forward progress
/// (observed directly -- the repro stalled indefinitely). Dropping only write access instead
/// lets execution and reads run at full speed and traps exclusively on the writes, which are the
/// only accesses that can corrupt.
///
/// On a trapped write the handler logs the faulting `rip`, restores write permission, and
/// single-steps the one faulting instruction via `EFLAGS.TF` so it can complete; the subsequent
/// `#DB` re-drops write permission. That is why [`on_single_step`] consults
/// [`codewatch::on_single_step_rearm`] before anything else.
///
/// Gated behind `LITEBOX_CODEWATCH=1` and inert otherwise.
///
/// # What it established
///
/// Run against the `sh -c "ls /; ls /usr; ls /tmp; ls /bin | head -3"` repro, this watchpoint
/// **disproved** the long-running "something corrupts the child's copied code" theory that the
/// deterministic `0xC0000096` (`STATUS_PRIVILEGED_INSTRUCTION`) crash at destination offset
/// `+0xa98a` had been attributed to. With every executable destination range armed non-writable
/// for the whole run, *zero* writes ever trapped, the page was still `PAGE_EXECUTE_READ` at crash
/// time, and the bytes there matched the parent's source byte-for-byte. The `0xf4` at the crash
/// `rip` is original, unmodified musl code: `testb $0xf, %dil; je +1; hlt` -- mallocng's inline
/// "pointer is not 16-byte aligned" assertion inside `free()`, whose `hlt` is *designed* to be
/// jumped over and only executes when the check fails. Nothing self-modifies; the same bytes
/// simply execute with a bad `rdi` on a later pass.
///
/// The real defect is upstream: `rdi` holds an untranslated *source*-space pointer (observed as
/// `0x100c55f8`, inside a tracked source range, while every other live register held a correctly
/// relocated destination address), i.e. a stale post-`fork()` pointer that reached `free()`
/// without being translated. See `.gm/prd.yml`'s
/// `residual-second-fork-verify-corruption-bug` row.
pub(crate) use codewatch::State as CodewatchState;

mod codewatch {
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use windows_sys::Win32::System::Memory as Win32_Memory;

    /// A `fork()` child has a handful of executable destination ranges (three in practice for a
    /// busybox/musl guest) and all of them must be watched, since the crash site is not always in
    /// the first.
    const MAX_WATCHED: usize = 8;

    /// All of the watchpoint's mutable state. Lives as a field on [`crate::TlsState`] rather than
    /// in a bare `static`, matching how the rest of this crate keeps long-lived state off the
    /// `ratchet_globals` bare-static budget (see `WindowsUserland::console_stdin_reader`'s
    /// comment). Per-thread is also its natural scope: the `fork()` child arms the ranges on its
    /// own thread and is the thread that traps on them. Fixed-capacity and lock-free so the
    /// exception handler never allocates or blocks.
    pub(crate) struct State {
        /// Number of ranges currently armed. Non-zero only under `LITEBOX_CODEWATCH=1`.
        armed: AtomicUsize,
        /// The watched regions, as parallel `[start, end)` pairs.
        start: [AtomicUsize; MAX_WATCHED],
        end: [AtomicUsize; MAX_WATCHED],
        /// The page awaiting re-arm after its faulting write was stepped over, or 0 if none.
        pending_rearm: AtomicUsize,
    }

    impl State {
        pub(crate) const fn new() -> Self {
            Self {
                armed: AtomicUsize::new(0),
                start: [const { AtomicUsize::new(0) }; MAX_WATCHED],
                end: [const { AtomicUsize::new(0) }; MAX_WATCHED],
                pending_rearm: AtomicUsize::new(0),
            }
        }
    }

    /// Runs `f` against the calling thread's watchpoint state, or returns `default` if this thread
    /// has no `TlsState` yet (the watchpoint is only ever armed from a thread that does).
    fn with<R>(default: R, f: impl FnOnce(&State) -> R) -> R {
        match crate::get_tls_ptr() {
            // SAFETY: `get_tls_ptr` returns this thread's live `TlsState`.
            Some(tls) => f(&unsafe { &*tls }.codewatch),
            None => default,
        }
    }

    pub(super) fn enabled() -> bool {
        std::env::var_os("LITEBOX_CODEWATCH").is_some()
    }

    /// Whether `addr` falls inside any currently watched region.
    pub(super) fn contains(addr: usize) -> bool {
        with(false, |s| {
            let armed = s.armed.load(Ordering::Relaxed).min(MAX_WATCHED);
            (0..armed).any(|i| {
                (s.start[i].load(Ordering::Relaxed)..s.end[i].load(Ordering::Relaxed))
                    .contains(&addr)
            })
        })
    }

    /// Sets `[start, end)` to `protect`. Returns whether it succeeded.
    fn protect(start: usize, len: usize, protect: u32) -> bool {
        let mut old = 0u32;
        // SAFETY: `VirtualProtect` validates the range itself and reports failure via a zero
        // return; it never dereferences the range's contents.
        unsafe {
            Win32_Memory::VirtualProtect(start as *mut c_void, len, protect, &raw mut old) != 0
        }
    }

    /// Reports the region type and protection backing `addr`, to tell a private commit apart from
    /// a mapped section view (the latter can be aliased by a second view that this watchpoint's
    /// `VirtualProtect` does not cover).
    pub(super) fn describe(addr: usize) -> (u32, u32, usize) {
        let mut mbi = Win32_Memory::MEMORY_BASIC_INFORMATION::default();
        // SAFETY: `VirtualQuery` only reads process metadata and tolerates any address value.
        let ok = unsafe {
            Win32_Memory::VirtualQuery(
                addr as *const c_void,
                &raw mut mbi,
                core::mem::size_of::<Win32_Memory::MEMORY_BASIC_INFORMATION>(),
            ) != 0
        };
        if ok {
            (mbi.Type, mbi.Protect, mbi.AllocationBase.addr())
        } else {
            (0, 0, 0)
        }
    }

    /// Drops write permission from `[start, end)` so that writes -- and only writes -- trap.
    /// Returns whether the arm succeeded.
    pub(super) fn arm(start: usize, end: usize) -> bool {
        with(false, |s| {
            let slot = s.armed.load(Ordering::Relaxed);
            if slot >= MAX_WATCHED {
                return false;
            }
            let ok = protect(start, end - start, Win32_Memory::PAGE_EXECUTE_READ);
            if ok {
                s.start[slot].store(start, Ordering::Relaxed);
                s.end[slot].store(end, Ordering::Relaxed);
                s.armed.store(slot + 1, Ordering::Relaxed);
            }
            ok
        })
    }

    /// Temporarily restores write permission to the page containing `addr` so the faulting
    /// instruction can be replayed and complete.
    pub(super) fn unprotect_page(addr: usize) {
        protect(addr & !0xfff, 0x1000, Win32_Memory::PAGE_EXECUTE_READWRITE);
    }

    /// Re-drops write permission on the page containing `addr` once the faulting instruction has
    /// been stepped over.
    fn rearm_page(addr: usize) {
        protect(addr & !0xfff, 0x1000, Win32_Memory::PAGE_EXECUTE_READ);
    }

    /// Forgets all watched regions, so a subsequent `fork()` re-arms from a clean slate rather
    /// than exhausting the fixed slot table across many forks.
    pub(super) fn reset() {
        with((), |s| s.armed.store(0, Ordering::Relaxed));
    }

    pub(super) fn set_pending_rearm(addr: usize) {
        with((), |s| s.pending_rearm.store(addr, Ordering::Relaxed));
    }

    /// If a watched page had its protection temporarily lifted to let a faulting write complete,
    /// re-drops write permission now that the write has been single-stepped. Returns whether this
    /// single-step trap belonged to the watchpoint (and so should be consumed rather than treated
    /// as a `fork_verify` verification step).
    pub(super) fn on_single_step_rearm() -> bool {
        let addr = with(0, |s| s.pending_rearm.swap(0, Ordering::Relaxed));
        if addr == 0 {
            return false;
        }
        rearm_page(addr);
        true
    }
}

/// Handles an `EXCEPTION_ACCESS_VIOLATION` that is really a watched-code-page write trap.
///
/// Returns whether the exception was ours (and has been handled, so the caller should resume by
/// replaying the faulting instruction). See [`codewatch`] for why this exists and why it is
/// diagnostic-only.
pub(crate) fn on_codewatch_write(record: &EXCEPTION_RECORD, context: &mut CONTEXT) -> bool {
    unsafe extern "C" {
        safe static __ImageBase: core::ffi::c_void;
    }
    if !codewatch::enabled() {
        return false;
    }
    // `ExceptionInformation[0]` is the access type (0 read / 1 write / 8 execute) and `[1]` the
    // faulting address, per `EXCEPTION_ACCESS_VIOLATION`'s documented parameters. Only writes can
    // corrupt, and only writes are trapped by the `PAGE_EXECUTE_READ` arming, so anything else
    // reaching here is a genuine fault that must be left to the normal handling below.
    if record.ExceptionInformation[0] != 1 {
        return false;
    }
    let address = record.ExceptionInformation[1];
    if !codewatch::contains(address) {
        return false;
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "diagnostic-only; this platform is x86_64-only, rip fits in usize"
    )]
    let rip = context.Rip as usize;

    let image_base = (&raw const __ImageBase).addr();
    let mut code = [0u8; MAX_INSTRUCTION_LEN];
    let len = read_code_bytes(rip, &mut code);
    let mnemonic = if len == 0 {
        None
    } else {
        let mut decoder = Decoder::with_ip(64, &code[..len], rip as u64, DecoderOptions::NONE);
        let instruction = decoder.decode();
        (!instruction.is_invalid()).then(|| instruction.mnemonic())
    };
    eprintln!(
        "[codewatch] tid={:?} WRITE into watched guest code page address={address:#x} rip={rip:#x} host_rva={:#x} mnemonic={mnemonic:?} bytes={:02x?}",
        std::thread::current().id(),
        rip.wrapping_sub(image_base),
        &code[..len],
    );

    // Let the faulting instruction complete: restore write permission and single-step it, so the
    // page can be re-armed immediately afterwards in `on_single_step`.
    codewatch::unprotect_page(address);
    codewatch::set_pending_rearm(address);
    #[allow(clippy::cast_possible_truncation)]
    let eflags_tf = EFLAGS_TF as u32;
    context.EFlags |= eflags_tf;
    true
}

/// Reports the region backing a crash `rip`, so a genuinely corrupted byte can be told apart from
/// execution having landed in a *different* mapping than the one the watchpoint armed.
pub(crate) fn describe_crash_page_for_diagnostics(rip: usize) {
    let (mtype, protect, alloc_base) = codewatch::describe(rip);
    eprintln!(
        "[codewatch] crash page rip={rip:#x} type={mtype:#x} protect={protect:#x} alloc_base={alloc_base:#x} watched={}",
        codewatch::contains(rip),
    );
    // Re-read the surrounding bytes relative to the *page*, so a `0xf4` at `rip` can be checked
    // against what the copy left there: if the page still holds the original instruction stream
    // and only `rip` disagrees, execution arrived at a misaligned address rather than the byte
    // having been overwritten.
    let page = rip & !0xfff;
    let offset = rip & 0xfff;
    let start = page + offset.saturating_sub(16);
    let mut window = [0u8; 32];
    let n = read_code_bytes(start, &mut window);
    eprintln!(
        "[codewatch] crash page window at {start:#x} (rip at +{}): {:02x?}",
        rip - start,
        &window[..n],
    );
}

/// Consumes the `TF` single-step the code-page watchpoint armed to let a trapped write complete,
/// when that step lands in *host* code (`is_in_guest == false`), where [`on_single_step`] is never
/// reached. Returns whether the step was the watchpoint's.
pub(crate) fn on_codewatch_step(context: &mut CONTEXT) -> bool {
    if !codewatch::enabled() || !codewatch::on_single_step_rearm() {
        return false;
    }
    #[allow(clippy::cast_possible_truncation)]
    let eflags_tf = EFLAGS_TF as u32;
    context.EFlags &= !eflags_tf;
    true
}

/// Arms the diagnostic code-page watchpoint over `relocations`' destination executable ranges.
fn arm_codewatch(relocations: &litebox::mm::AddressRelocations) {
    if !codewatch::enabled() {
        return;
    }
    codewatch::reset();
    for (index, (source_range, dest_base)) in relocations.ranges().iter().enumerate() {
        if !relocations.is_executable_range(index) {
            continue;
        }
        let start = *dest_base;
        let end = dest_base + source_range.len();
        let armed = codewatch::arm(start, end);
        let (mtype, protect, alloc_base) = codewatch::describe(start);
        eprintln!(
            "[codewatch] tid={:?} arm dest=[{start:#x},{end:#x}) len={:#x} ok={armed} type={mtype:#x} protect={protect:#x} alloc_base={alloc_base:#x}",
            std::thread::current().id(),
            source_range.len(),
        );
        // Self-test: a null result from this watchpoint ("no write ever trapped") is only
        // meaningful if a write provably *does* trap. `LITEBOX_CODEWATCH=selftest` proves it by
        // writing one byte back to itself per armed range and checking the trap fires; it is a
        // separate mode because it deliberately perturbs the pages under investigation.
        if armed && std::env::var_os("LITEBOX_CODEWATCH").is_some_and(|v| v == "selftest") {
            let probe = start + 0x100;
            // SAFETY: `probe` is inside a committed, just-armed executable destination range, and
            // the value written is the one just read back, so guest state is left unchanged.
            unsafe {
                let byte = core::ptr::read_volatile(probe as *const u8);
                core::ptr::write_volatile(probe as *mut u8, byte);
            }
            eprintln!("[codewatch] selftest wrote to {probe:#x}");
        }
    }
}

/// Per-thread arm/disarm entry points, called through
/// [`litebox::platform::ForkChildVerificationProvider`].
pub(crate) fn begin(relocations: alloc::sync::Arc<litebox::mm::AddressRelocations>) {
    if crate::veh_trace_enabled() {
        eprintln!(
            "[fork_verify] tid={:?} begin: ranges={:?}",
            std::thread::current().id(),
            relocations.ranges(),
        );
    }
    arm_codewatch(&relocations);
    if let Some(tls) = crate::get_tls_ptr() {
        // SAFETY: `get_tls_ptr` returns this thread's live `TlsState`.
        let tls = unsafe { &*tls };
        if std::env::var_os("LITEBOX_FORKVERIFY_OFF").is_none() {
            *tls.fork_verify.borrow_mut() = Some(relocations);
        }
    }
}

pub(crate) fn end() {
    if crate::veh_trace_enabled() {
        eprintln!("[fork_verify] tid={:?} end", std::thread::current().id());
    }
    if let Some(tls) = crate::get_tls_ptr() {
        // SAFETY: as above.
        let tls = unsafe { &*tls };
        *tls.fork_verify.borrow_mut() = None;
    }
}
