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
//!
//! Either repair only ever substitutes a register value already proven translatable via the exact
//! relocation map used for every other register at `fork()` time -- never a guess. If a stale
//! `rip` or effective address is *not* translatable (falls in a source range `duplicate()` never
//! recorded, which should not happen but is not assumed), the child is killed: synthesizing an
//! access violation and letting it flow through the platform's ordinary exception path, so the
//! shim raises a perfectly normal `SIGSEGV` on the child; the child's exit status is recorded and
//! the parent's `wait4()` unblocks as usual.
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
    // there is a stale, untranslated SOURCE-range pointer, the very next instruction will load that
    // stale value into `rip` and this same trap will re-fire on it via case (1) above -- but only
    // for *that one* execution of this call site. Any later call through the *same* slot (a `call
    // [rip+offset]` PLT/GOT stub is by far the most common shape, and gets executed repeatedly, not
    // just once) hits the identical untranslated value again, since case (1) only ever patches the
    // live `rip`/`rbp` registers, never the memory the value was read from. Observed in practice:
    // the same stale source-range `rip` recurring verbatim across many single-step traps, each time
    // patched only in-register, until the repeated stale round-trips desynchronize some other piece
    // of state and the child eventually executes into corrupted-looking code (eventually hitting
    // `STATUS_PRIVILEGED_INSTRUCTION`). Patching the SLOT itself here -- once -- means every
    // subsequent call through it reads the already-correct destination pointer directly, matching
    // how `fixup_stale_stack_pointers` permanently heals the stack/TCB slots it can reach, for the
    // one narrow additional case (a code pointer loaded through an explicit memory operand) that
    // proactive pass cannot: it never has TCB-adjacent GOT slots reliably identified up front.
    if (instruction.is_call_near_indirect() || instruction.is_jmp_near_indirect())
        && let Some(load_address) = explicit_memory_operand_address(&instruction, context)
        && relocations.is_in_destination(load_address)
        && let Some(stale_value) = read_usize_fault_tolerant(load_address)
        && let Some(translated) = relocations.translate(stale_value)
    {
        litebox_util_log::warn!(
            rip:? = rip, load_address:? = load_address, stale_value:? = stale_value,
            translated:? = translated, mnemonic:? = instruction.mnemonic();
            "fork_verify: stale CODE pointer in indirect call/jmp target slot detected, patching slot in place"
        );
        write_usize_fault_tolerant(load_address, translated);
    }

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
/// not in a committed, writable region.
fn write_usize_fault_tolerant(addr: usize, value: usize) {
    if !is_writable(addr) {
        return;
    }
    // SAFETY: `is_writable` confirmed `addr` is in a committed, writable region; see
    // `read_usize_fault_tolerant`'s comment on why a full `usize` is always in-bounds here.
    unsafe { core::ptr::write_unaligned(addr as *mut usize, value) };
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

/// Per-thread arm/disarm entry points, called through
/// [`litebox::platform::ForkChildVerificationProvider`].
pub(crate) fn begin(relocations: alloc::sync::Arc<litebox::mm::AddressRelocations>) {
    if let Some(tls) = crate::get_tls_ptr() {
        // SAFETY: `get_tls_ptr` returns this thread's live `TlsState`.
        let tls = unsafe { &*tls };
        if std::env::var_os("LITEBOX_FORKVERIFY_OFF").is_none() {
            *tls.fork_verify.borrow_mut() = Some(relocations);
        }
    }
}

pub(crate) fn end() {
    if let Some(tls) = crate::get_tls_ptr() {
        // SAFETY: as above.
        let tls = unsafe { &*tls };
        *tls.fork_verify.borrow_mut() = None;
    }
}
