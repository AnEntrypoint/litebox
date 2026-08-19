// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use crate::ShimPlatform;
use crate::UserPtrMut;
use crate::syscalls::signal::{DeliverFault, SignalState};
use core::mem::offset_of;
use litebox::utils::{ReinterpretUnsignedExt as _, TruncateExt as _};
use litebox_common_linux::{
    PtRegs,
    signal::{SaFlags, SigAction, Siginfo, Ucontext, aarch64::Sigcontext},
};
use zerocopy::{FromBytes, IntoBytes};

#[repr(C)]
#[derive(Clone, FromBytes, IntoBytes)]
struct SignalFrame {
    ucontext: Ucontext,
    siginfo: Siginfo,
}

pub(super) fn uctx_addr(ctx: &PtRegs) -> usize {
    ctx.sp
}

pub(super) fn sp(ctx: &PtRegs) -> usize {
    ctx.sp
}

pub(super) fn get_signal_frame(sp: usize, _action: &SigAction) -> usize {
    // aarch64 has no x86-style "redzone" below the stack pointer that must be skipped.
    let mut frame_addr = sp;

    // Space for the signal frame.
    frame_addr = frame_addr.wrapping_sub(core::mem::size_of::<SignalFrame>());

    // The AArch64 procedure call standard requires the stack pointer to be 16-byte aligned
    // at every public interface, including a signal handler's entry.
    frame_addr &= !15;

    frame_addr
}

impl<Platform: ShimPlatform> SignalState<Platform> {
    pub(super) fn write_signal_frame(
        &self,
        frame_addr: usize,
        siginfo: &Siginfo,
        action: &SigAction,
        ctx: &mut PtRegs,
        sigreturn_trampoline: usize,
    ) -> Result<(), DeliverFault> {
        // Unlike x86_64 (where glibc always transparently supplies SA_RESTORER + a real
        // restorer address, regardless of whether the caller explicitly asked for it), aarch64
        // glibc's sigaction() wrapper does NOT set SA_RESTORER or sa_restorer at all -- live
        // confirmed: a guest calling sigaction(SIGSEGV, {.sa_flags = SA_SIGINFO}) (no
        // SA_RESTORER) reaches here with flags=SA_SIGINFO only and restorer=0. This matches
        // real aarch64 Linux's ABI: the kernel always provides sigreturn via a fixed VDSO page
        // regardless of sa_restorer, so userspace on real hardware never needs to supply one.
        // litebox has no real VDSO (see get_vdso_address's doc comment), so it must supply its
        // OWN synthesized restorer -- `sigreturn_trampoline`, a tiny guest-visible
        // `mov x8, #139 (rt_sigreturn) ; svc #0` stub lazily allocated by
        // `Task::ensure_sigreturn_trampoline` -- whenever the guest didn't provide a real one.
        // The original SA_RESTORER-required behavior is kept as a fallback for a guest that DID
        // supply one (a hand-rolled restorer, matching real Linux's actual contract).
        let restorer = if action.flags.contains(SaFlags::RESTORER) {
            action.restorer
        } else if sigreturn_trampoline != 0 {
            sigreturn_trampoline
        } else {
            return Err(DeliverFault);
        };

        let last_exception = self.last_exception.get();
        let mut regs = [0u64; litebox_common_linux::AARCH64_GENERAL_REGISTER_COUNT];
        for (dst, src) in regs.iter_mut().zip(ctx.regs.iter()) {
            *dst = *src as u64;
        }
        let frame = SignalFrame {
            ucontext: Ucontext {
                flags: 0,
                link: 0,
                stack: self.altstack.get(),
                sigmask: self.blocked.get(),
                __unused: [0; 1024 / 8
                    - core::mem::size_of::<litebox_common_linux::signal::SigSet>()],
                __align_pad: [0; 8],
                mcontext: Sigcontext {
                    fault_address: last_exception.fault_address as u64,
                    regs,
                    sp: ctx.sp as u64,
                    pc: ctx.pc as u64,
                    pstate: ctx.pstate,
                    __reserved_pad: [0; 8],
                    __reserved: [0; 4096],
                },
            },
            siginfo: siginfo.clone(),
        };

        let frame_ptr = UserPtrMut::from_usize(frame_addr);
        frame_ptr
            .write_at_offset::<Platform>(0, frame)
            .ok_or(DeliverFault)?;

        // aarch64's rt_sigreturn calling convention: the kernel places the signal handler's
        // return address (the restorer) directly in the link register (x30/lr) before
        // transferring control, rather than pushing a return address onto the stack the way
        // x86_64's `call`-based ABI does -- there is no return-address slot in `SignalFrame`
        // itself (unlike x86_64's `SignalFrame::return_address`).
        ctx.regs[30] = restorer; // lr
        ctx.sp = frame_addr;
        ctx.pc = action.sigaction;
        ctx.regs[0] = siginfo.signo.reinterpret_as_unsigned() as usize; // x0: signum
        ctx.regs[1] = frame_addr.wrapping_add(offset_of!(SignalFrame, siginfo)); // x1: siginfo*
        ctx.regs[2] = frame_addr.wrapping_add(offset_of!(SignalFrame, ucontext)); // x2: ucontext*
        Ok(())
    }
}

pub(super) fn restore_sigcontext(
    ctx: &mut PtRegs,
    sigctx: &litebox_common_linux::signal::aarch64::Sigcontext,
) -> usize {
    let litebox_common_linux::signal::aarch64::Sigcontext {
        fault_address: _,
        regs,
        sp,
        pc,
        pstate,
        __reserved_pad: _,
        __reserved: _,
    } = sigctx;

    for (dst, src) in ctx.regs.iter_mut().zip(regs.iter()) {
        *dst = src.trunc();
    }
    ctx.sp = sp.trunc();
    ctx.pc = pc.trunc();
    ctx.pstate = *pstate;

    // TODO: restore FP/SIMD state (__reserved holds the FPSIMD context record).

    ctx.regs[0]
}
