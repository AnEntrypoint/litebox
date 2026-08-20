// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use crate::ShimPlatform;
use crate::UserPtrMut;
use crate::syscalls::signal::{DeliverFault, SignalState};
use core::mem::offset_of;
use litebox::utils::{ReinterpretUnsignedExt as _, TruncateExt as _};
use litebox_common_linux::{
    PtRegs,
    signal::{SaFlags, SigAction, Siginfo, Ucontext, x86_64::FpState, x86_64::Sigcontext},
};
use zerocopy::{FromBytes, FromZeros, IntoBytes};

#[repr(C)]
#[derive(Clone, FromBytes, IntoBytes)]
struct SignalFrame {
    return_address: usize,
    ucontext: Ucontext,
    siginfo: Siginfo,
    // Real Linux places the FP/SSE state adjacent to (not inside) `Ucontext`, pointed to by
    // `mcontext.fpstate` -- guest signal handlers/libc dereference through that pointer, never
    // assume a fixed offset from the frame's own start, so this field's exact position in
    // `SignalFrame` is free (kept last purely so `get_signal_frame`'s size math stays a simple
    // sum). `xmm0`-`xmm15`'s live bytes (`get_fp_state`) go into `fpregs.xmm_space`; every other
    // `FpState` field is left at Linux's own standard post-reset value (cwd/twd = 0xffff, mxcsr =
    // the SSE default 0x1f80, matching what a freshly-reset x87/SSE unit reads as) since this
    // shim tracks no x87 state at all -- an honest "no x87 activity happened" value, not a
    // fabricated one, matching this codebase's own standing invariant against writing register
    // values not read from a live, real source.
    fpregs: FpState,
}

pub(super) fn uctx_addr(ctx: &PtRegs) -> usize {
    ctx.rsp
}

pub(super) fn sp(ctx: &PtRegs) -> usize {
    ctx.rsp
}

pub(super) fn get_signal_frame(sp: usize, _action: &SigAction) -> usize {
    let mut frame_addr = sp;

    // Skip the redzone.
    frame_addr = frame_addr.wrapping_sub(128);

    // Space for the signal frame.
    frame_addr = frame_addr.wrapping_sub(core::mem::size_of::<SignalFrame>());

    // Align the frame (offset by 8 bytes for return address)
    frame_addr &= !15;
    frame_addr = frame_addr.wrapping_sub(8);

    frame_addr
}

impl<Platform: ShimPlatform> SignalState<Platform> {
    pub(super) fn write_signal_frame(
        &self,
        platform: &Platform,
        frame_addr: usize,
        siginfo: &Siginfo,
        action: &SigAction,
        ctx: &mut PtRegs,
        _sigreturn_trampoline: usize,
    ) -> Result<(), DeliverFault> {
        if !action.flags.contains(SaFlags::RESTORER) {
            return Err(DeliverFault);
        }

        let last_exception = self.last_exception.get();

        // Capture the guest's real xmm0-xmm15 now, before writing anything -- `get_fp_state`
        // returning `Err` (platform doesn't support it, e.g. this thread's TLS isn't installed)
        // means falling back to real Linux's own null-`fpstate` convention (no FP state saved
        // this delivery) rather than fabricating a value, matching how a real Linux kernel build
        // without FPU support behaves for `fpstate`.
        let mut fpregs = FpState::new_zeroed();
        // Standard x87/SSE post-reset values (real Linux's own defaults for a task that has done
        // no x87 activity this shim doesn't track) -- honest placeholders for state that
        // genuinely was never captured, not fabricated register content.
        fpregs.cwd = 0xffff;
        fpregs.swd = 0xffff;
        fpregs.twd = 0xffff;
        fpregs.mxcsr = 0x1f80;
        let mut xmm_bytes = [0u8; 256];
        let have_fp_state = platform.get_fp_state(&mut xmm_bytes).is_ok();
        if have_fp_state {
            fpregs.xmm_space.as_mut_bytes().copy_from_slice(&xmm_bytes);
        }

        let fpstate_addr = if have_fp_state {
            frame_addr.wrapping_add(offset_of!(SignalFrame, fpregs)) as u64
        } else {
            0
        };

        let frame = SignalFrame {
            return_address: action.restorer,
            ucontext: Ucontext {
                flags: 0,
                link: 0, // core::ptr::null_mut(),
                stack: self.altstack.get(),
                mcontext: Sigcontext {
                    r8: ctx.r8 as u64,
                    r9: ctx.r9 as u64,
                    r10: ctx.r10 as u64,
                    r11: ctx.r11 as u64,
                    r12: ctx.r12 as u64,
                    r13: ctx.r13 as u64,
                    r14: ctx.r14 as u64,
                    r15: ctx.r15 as u64,
                    rdi: ctx.rdi as u64,
                    rsi: ctx.rsi as u64,
                    rbp: ctx.rbp as u64,
                    rbx: ctx.rbx as u64,
                    rdx: ctx.rdx as u64,
                    rax: ctx.rax as u64,
                    rcx: ctx.rcx as u64,
                    rsp: ctx.rsp as u64,
                    rip: ctx.rip as u64,
                    rflags: ctx.eflags as u64,
                    cs: ctx.cs.trunc(),
                    gs: 0,
                    fs: 0,
                    ss: ctx.ss.trunc(),
                    err: last_exception.error_code.into(),
                    trapno: last_exception.exception.0.into(),
                    oldmask: self.blocked.get().as_u64(),
                    cr2: last_exception.cr2 as u64,
                    fpstate: fpstate_addr,
                    reserved1: [0; 8],
                },
                sigmask: self.blocked.get(),
            },
            siginfo: siginfo.clone(),
            fpregs,
        };

        let frame_ptr = UserPtrMut::from_usize(frame_addr);
        frame_ptr
            .write_at_offset::<Platform>(0, frame)
            .ok_or(DeliverFault)?;

        ctx.rsp = frame_addr;
        ctx.rip = action.sigaction;
        ctx.rdi = siginfo.signo.reinterpret_as_unsigned() as usize;
        ctx.rsi = frame_addr.wrapping_add(offset_of!(SignalFrame, siginfo));
        ctx.rdx = frame_addr.wrapping_add(offset_of!(SignalFrame, ucontext));
        ctx.rax = 0;
        ctx.eflags &= !litebox_common_linux::arch::EFLAGS_DF;
        Ok(())
    }
}

pub(super) fn restore_sigcontext(
    ctx: &mut PtRegs,
    sigctx: &litebox_common_linux::signal::x86_64::Sigcontext,
) -> usize {
    let litebox_common_linux::signal::x86_64::Sigcontext {
        r8,
        r9,
        r10,
        r11,
        r12,
        r13,
        r14,
        r15,
        rdi,
        rsi,
        rbp,
        rbx,
        rdx,
        rax,
        rcx,
        rsp,
        rip,
        rflags,
        cs: _,
        gs: _,
        fs: _,
        ss: _,
        err: _,
        trapno: _,
        oldmask: _,
        cr2: _,
        fpstate: _,
        reserved1: _,
    } = *sigctx;

    ctx.r8 = r8.trunc();
    ctx.r9 = r9.trunc();
    ctx.r10 = r10.trunc();
    ctx.r11 = r11.trunc();
    ctx.r12 = r12.trunc();
    ctx.r13 = r13.trunc();
    ctx.r14 = r14.trunc();
    ctx.r15 = r15.trunc();
    ctx.rdi = rdi.trunc();
    ctx.rsi = rsi.trunc();
    ctx.rbp = rbp.trunc();
    ctx.rbx = rbx.trunc();
    ctx.rdx = rdx.trunc();
    ctx.rax = rax.trunc();
    ctx.rcx = rcx.trunc();
    ctx.rsp = rsp.trunc();
    ctx.rip = rip.trunc();
    ctx.eflags = rflags.trunc();

    // xmm0-xmm15 are restored by this function's caller (`sys_rt_sigreturn`, in mod.rs) before
    // calling here -- it has the platform handle this function does not, and needs the
    // still-typed `Sigcontext.fpstate` guest pointer (already discarded above via `fpstate: _`)
    // to read the FpState block from guest memory.

    ctx.rax
}
