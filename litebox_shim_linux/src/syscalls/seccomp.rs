// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! `seccomp(2)`, `prctl(PR_SET_SECCOMP)` and the classic-BPF filter every guest syscall is evaluated
//! against.

use crate::{ShimFS, ShimPlatform, Task, UserPtr};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use litebox::sync::Mutex;
use litebox_common_linux::PtRegs;
use litebox_common_linux::errno::Errno;
use litebox_common_linux::signal::{Siginfo, SiginfoData, Signal};
use zerocopy::FromBytes;

pub(crate) const SECCOMP_MODE_DISABLED: u8 = 0;
pub(crate) const SECCOMP_MODE_STRICT: u8 = 1;
pub(crate) const SECCOMP_MODE_FILTER: u8 = 2;

const SECCOMP_SET_MODE_STRICT: u32 = 0;
const SECCOMP_SET_MODE_FILTER: u32 = 1;
const SECCOMP_GET_ACTION_AVAIL: u32 = 2;

const SECCOMP_FILTER_FLAG_TSYNC: u32 = 1;
const SECCOMP_FILTER_FLAG_LOG: u32 = 2;
const SECCOMP_FILTER_FLAG_SPEC_ALLOW: u32 = 4;

const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_KILL_THREAD: u32 = 0x0000_0000;
const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
const SECCOMP_RET_TRACE: u32 = 0x7ff0_0000;
const SECCOMP_RET_LOG: u32 = 0x7fd0_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ACTION_FULL: u32 = 0xffff_0000;
const SECCOMP_RET_DATA: u32 = 0x0000_ffff;

/// `si_code` of a seccomp-generated `SIGSYS` (`SYS_SECCOMP`, from `linux/audit.h`).
const SYS_SECCOMP: i32 = 1;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;

/// `sizeof(struct seccomp_data)`.
const SECCOMP_DATA_LEN: usize = 64;
/// Linux's `BPF_MAXINSNS` for a classic filter program.
const MAX_INSNS: usize = 4096;
/// Linux caps a `SECCOMP_RET_ERRNO` value at `MAX_ERRNO` and returns it negated.
const MAX_ERRNO: u16 = 4095;

const BPF_CLASS: u16 = 0x07;
const BPF_LD: u16 = 0x00;
const BPF_LDX: u16 = 0x01;
const BPF_ST: u16 = 0x02;
const BPF_STX: u16 = 0x03;
const BPF_ALU: u16 = 0x04;
const BPF_JMP: u16 = 0x05;
const BPF_RET: u16 = 0x06;
const BPF_MISC: u16 = 0x07;

const BPF_SIZE: u16 = 0x18;
const BPF_W: u16 = 0x00;
const BPF_H: u16 = 0x08;
const BPF_B: u16 = 0x10;

const BPF_MODE: u16 = 0xe0;
const BPF_IMM: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_MEM: u16 = 0x60;
const BPF_LEN: u16 = 0x80;

const BPF_X: u16 = 0x08;

const BPF_OP: u16 = 0xf0;
const BPF_ADD: u16 = 0x00;
const BPF_SUB: u16 = 0x10;
const BPF_MUL: u16 = 0x20;
const BPF_DIV: u16 = 0x30;
const BPF_OR: u16 = 0x40;
const BPF_AND: u16 = 0x50;
const BPF_LSH: u16 = 0x60;
const BPF_RSH: u16 = 0x70;
const BPF_NEG: u16 = 0x80;
const BPF_MOD: u16 = 0x90;
const BPF_XOR: u16 = 0xa0;

const BPF_JA: u16 = 0x00;
const BPF_JEQ: u16 = 0x10;
const BPF_JGT: u16 = 0x20;
const BPF_JGE: u16 = 0x30;
const BPF_JSET: u16 = 0x40;

const BPF_TAX: u16 = BPF_MISC;
const BPF_TXA: u16 = BPF_MISC | 0x80;
const BPF_RET_A: u16 = BPF_RET | BPF_B;
const BPF_RET_K: u16 = BPF_RET;

/// Linux's `struct sock_filter`: one classic-BPF instruction.
#[repr(C)]
#[derive(Clone, Copy, Debug, FromBytes)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

/// Linux's `struct sock_fprog`, including the implicit padding before the pointer.
#[repr(C)]
#[derive(Clone, Copy, Debug, FromBytes)]
struct SockFprog {
    len: u16,
    _pad: u16,
    filter: usize,
}

/// What a filter decided about one syscall, most severe first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Verdict {
    Allow,
    Log,
    Trace,
    Errno(u16),
    /// The 16-bit `SECCOMP_RET_DATA` payload. Linux's `seccomp_send_sigsys` hands it to the
    /// SIGSYS handler as `si_errno`, and it is the ONLY way that handler knows which trap it is:
    /// Chromium's sandbox encodes a trap id there and looks it up in its own handler, so dropping
    /// it (this used to be a bare `Trap`) makes every trap unrecognizable -- Chromium logged
    /// `Unexpected SIGSYS received.` 21 times in one run while our decoding was otherwise
    /// faithful, and each trapped syscall then stayed un-emulated.
    Trap(u16),
    KillThread,
    KillProcess,
}

impl Verdict {
    fn from_ret(ret: u32) -> Self {
        match ret & SECCOMP_RET_ACTION_FULL {
            SECCOMP_RET_ALLOW => Self::Allow,
            SECCOMP_RET_LOG => Self::Log,
            SECCOMP_RET_TRACE => Self::Trace,
            SECCOMP_RET_ERRNO => Self::Errno((ret & SECCOMP_RET_DATA) as u16),
            SECCOMP_RET_TRAP => Self::Trap((ret & SECCOMP_RET_DATA) as u16),
            x if x == SECCOMP_RET_KILL_PROCESS => Self::KillProcess,
            _ => Self::KillThread,
        }
    }

    /// Linux resolves a stack of filters by severity, so the comparison a caller needs.
    fn severity(self) -> u8 {
        match self {
            Self::Allow => 0,
            Self::Log => 1,
            Self::Trace => 2,
            Self::Errno(_) => 3,
            Self::Trap(_) => 4,
            Self::KillThread => 5,
            Self::KillProcess => 6,
        }
    }
}

fn errno_from_ret_data(data: u16) -> Errno {
    Errno::try_from(u32::from(data.clamp(1, MAX_ERRNO))).unwrap_or(Errno::ENOSYS)
}

fn mem_slot(k: u32) -> Option<usize> {
    usize::try_from(k).ok().filter(|i| *i < 16)
}

fn load_abs(data: &[u8; SECCOMP_DATA_LEN], code: u16, k: u32) -> Option<u32> {
    let start = usize::try_from(k).ok()?;
    match code & BPF_SIZE {
        BPF_W => {
            let b: [u8; 4] = data.get(start..start.checked_add(4)?)?.try_into().ok()?;
            Some(u32::from_le_bytes(b))
        }
        BPF_H => {
            let b: [u8; 2] = data.get(start..start.checked_add(2)?)?.try_into().ok()?;
            Some(u32::from(u16::from_le_bytes(b)))
        }
        BPF_B => Some(u32::from(*data.get(start)?)),
        _ => None,
    }
}

fn alu(op: u16, a: u32, y: u32) -> Option<u32> {
    match op {
        BPF_ADD => Some(a.wrapping_add(y)),
        BPF_SUB => Some(a.wrapping_sub(y)),
        BPF_MUL => Some(a.wrapping_mul(y)),
        BPF_DIV => a.checked_div(y),
        BPF_MOD => a.checked_rem(y),
        BPF_AND => Some(a & y),
        BPF_OR => Some(a | y),
        BPF_XOR => Some(a ^ y),
        BPF_LSH => Some(a.wrapping_shl(y)),
        BPF_RSH => Some(a.wrapping_shr(y)),
        BPF_NEG => Some(a.wrapping_neg()),
        _ => None,
    }
}

/// Runs one classic-BPF program over `data`, returning the raw `SECCOMP_RET_*` value.
///
/// Anything Linux would reject at install time but that reaches here anyway (an unsupported mode,
/// a division by zero, running off the end) yields `SECCOMP_RET_KILL_THREAD`, the same verdict
/// Linux's own interpreter produces for a non-`RET` exit.
fn run_filter(prog: &[SockFilter], data: &[u8; SECCOMP_DATA_LEN]) -> u32 {
    let mut a: u32 = 0;
    let mut x: u32 = 0;
    let mut m = [0u32; 16];
    let mut pc = 0usize;
    // Only forward jumps are legal (see `validate`), so `pc` strictly increases and this cannot
    // loop; the bound is what makes that guarantee local rather than spread over the jump code.
    while let Some(insn) = prog.get(pc) {
        let code = insn.code;
        let k = insn.k;
        let class = code & BPF_CLASS;
        if class == BPF_RET {
            if code == BPF_RET_A {
                return a;
            }
            if code == BPF_RET_K {
                return k;
            }
            return 0;
        }
        if class == BPF_MISC {
            match code {
                BPF_TAX => x = a,
                BPF_TXA => a = x,
                _ => return 0,
            }
            pc += 1;
            continue;
        }
        if class == BPF_ST || class == BPF_STX {
            let Some(i) = mem_slot(k) else { return 0 };
            m[i] = if class == BPF_ST { a } else { x };
            pc += 1;
            continue;
        }
        if class == BPF_LD || class == BPF_LDX {
            let value = match code & BPF_MODE {
                BPF_IMM => k,
                BPF_MEM => {
                    let Some(i) = mem_slot(k) else { return 0 };
                    m[i]
                }
                BPF_ABS => {
                    let Some(v) = load_abs(data, code, k) else {
                        return 0;
                    };
                    v
                }
                BPF_LEN => u32::try_from(SECCOMP_DATA_LEN).unwrap_or(0),
                _ => return 0,
            };
            if class == BPF_LD {
                a = value;
            } else {
                x = value;
            }
            pc += 1;
            continue;
        }
        if class == BPF_ALU {
            let y = if code & BPF_X == BPF_X { x } else { k };
            let Some(v) = alu(code & BPF_OP, a, y) else {
                return 0;
            };
            a = v;
            pc += 1;
            continue;
        }
        if class == BPF_JMP {
            let y = if code & BPF_X == BPF_X { x } else { k };
            let op = code & BPF_OP;
            let offset = match op {
                BPF_JA => usize::try_from(k).ok(),
                BPF_JEQ => Some(usize::from(if a == y { insn.jt } else { insn.jf })),
                BPF_JGT => Some(usize::from(if a > y { insn.jt } else { insn.jf })),
                BPF_JGE => Some(usize::from(if a >= y { insn.jt } else { insn.jf })),
                BPF_JSET => Some(usize::from(if a & y != 0 { insn.jt } else { insn.jf })),
                _ => None,
            };
            let Some(offset) = offset else { return 0 };
            let Some(next) = pc.checked_add(1).and_then(|n| n.checked_add(offset)) else {
                return 0;
            };
            // Backward jumps would let a program loop; `validate` rejects them at install time,
            // and falling off the end (next == len) is a kill, exactly as Linux treats it.
            if next <= pc || next > prog.len() {
                return 0;
            }
            pc = next;
            continue;
        }
        return 0;
    }
    0
}

/// Static validation of a filter program, mirroring Linux's own `bpf_check_classic`.
///
/// One notch more permissive than Linux where being stricter would only reject programs Linux
/// accepts in practice: a jump whose target is exactly the end of the program is allowed here and
/// kills at runtime, where Linux's forward-reachability walk would have refused it at install.
fn validate(prog: &[SockFilter]) -> Result<(), Errno> {
    if prog.is_empty() || prog.len() > MAX_INSNS {
        return Err(Errno::EINVAL);
    }
    if prog[prog.len() - 1].code & BPF_CLASS != BPF_RET {
        return Err(Errno::EINVAL);
    }
    for (pc, insn) in prog.iter().enumerate() {
        let code = insn.code;
        let class = code & BPF_CLASS;
        match class {
            BPF_LD | BPF_LDX => match code & BPF_MODE {
                BPF_IMM | BPF_LEN => {}
                BPF_MEM if insn.k < 16 => {}
                BPF_ABS => {
                    let width = match code & BPF_SIZE {
                        BPF_W => 4usize,
                        BPF_H => 2,
                        BPF_B => 1,
                        _ => return Err(Errno::EINVAL),
                    };
                    let start = usize::try_from(insn.k).unwrap_or(usize::MAX);
                    if start
                        .checked_add(width)
                        .map_or(true, |end| end > SECCOMP_DATA_LEN)
                    {
                        return Err(Errno::EINVAL);
                    }
                }
                _ => return Err(Errno::EINVAL),
            },
            BPF_ST | BPF_STX => {
                if insn.k >= 16 {
                    return Err(Errno::EINVAL);
                }
            }
            BPF_ALU => {
                if !matches!(
                    code & BPF_OP,
                    BPF_ADD
                        | BPF_SUB
                        | BPF_MUL
                        | BPF_DIV
                        | BPF_OR
                        | BPF_AND
                        | BPF_LSH
                        | BPF_RSH
                        | BPF_NEG
                        | BPF_MOD
                        | BPF_XOR
                ) {
                    return Err(Errno::EINVAL);
                }
            }
            BPF_JMP => {
                // A target of `prog.len()` is the fall-off-the-end kill; anything beyond is not.
                let limit = prog.len() - 1 - pc;
                let op = code & BPF_OP;
                let in_range = if op == BPF_JA {
                    usize::try_from(insn.k).map_or(false, |o| o <= limit)
                } else {
                    matches!(op, BPF_JEQ | BPF_JGT | BPF_JGE | BPF_JSET)
                        && usize::from(insn.jt) <= limit
                        && usize::from(insn.jf) <= limit
                };
                if !in_range {
                    return Err(Errno::EINVAL);
                }
            }
            BPF_RET => {
                if code != BPF_RET_K && code != BPF_RET_A {
                    return Err(Errno::EINVAL);
                }
            }
            BPF_MISC => {
                if code != BPF_TAX && code != BPF_TXA {
                    return Err(Errno::EINVAL);
                }
            }
            _ => return Err(Errno::EINVAL),
        }
    }
    Ok(())
}

/// A thread group's seccomp state: the mode, `no_new_privs` and the installed filter stack.
///
/// Held on [`crate::syscalls::process::Process`], which every thread of a group shares, so
/// `SECCOMP_FILTER_FLAG_TSYNC` (install for the whole thread group) needs no extra work and a
/// filter survives both `clone` and `execve`. `mode` is an atomic so the per-syscall check that
/// gates all of this costs one relaxed load for the processes that never install a filter.
pub(crate) struct SeccompState<Platform: ShimPlatform> {
    mode: AtomicU8,
    no_new_privs: AtomicBool,
    /// Newest first: `seccomp` prepends, and severity ordering makes the order irrelevant to the
    /// verdict.
    filters: Mutex<Platform, Vec<Box<[SockFilter]>>>,
}

impl<Platform: ShimPlatform> SeccompState<Platform> {
    pub(crate) fn new() -> Self {
        Self {
            mode: AtomicU8::new(SECCOMP_MODE_DISABLED),
            no_new_privs: AtomicBool::new(false),
            filters: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn mode(&self) -> u8 {
        self.mode.load(Ordering::Relaxed)
    }

    pub(crate) fn no_new_privs(&self) -> bool {
        self.no_new_privs.load(Ordering::Relaxed)
    }

    fn set_no_new_privs(&self) {
        self.no_new_privs.store(true, Ordering::Relaxed);
    }

    fn install_strict(&self) {
        self.mode.store(SECCOMP_MODE_STRICT, Ordering::Relaxed);
    }

    fn add_filter(&self, prog: Box<[SockFilter]>) {
        self.mode.store(SECCOMP_MODE_FILTER, Ordering::Relaxed);
        self.filters.lock().insert(0, prog);
    }

    /// Copies a fork/clone parent's whole seccomp state onto a fresh child's.
    pub(crate) fn inherit_from(&self, parent: &Self) {
        self.mode.store(parent.mode(), Ordering::Relaxed);
        self.no_new_privs
            .store(parent.no_new_privs(), Ordering::Relaxed);
        let mut filters = self.filters.lock();
        filters.clear();
        filters.extend(parent.filters.lock().iter().cloned());
    }

    /// Serializes this thread group's seccomp state for a cross-process fork child.
    ///
    /// A fork child MUST inherit all three of these on real Linux: the mode, `no_new_privs` and
    /// the whole filter stack survive `fork()`. Losing `no_new_privs` is not a cosmetic drift --
    /// Chromium's renderer is a fork of the zygote, and its
    /// `SandboxBPF::KernelSupportsSeccompBPF()` probe is
    /// `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, nullptr)`, which Linux answers `EFAULT` and
    /// Chromium reads as "this kernel has seccomp-bpf". Without the inherited bit the same call
    /// answers `EACCES`, the probe reports "not supported", and Chromium's sandbox
    /// initialization `IMMEDIATE_CRASH`es -- a bare `int3`, SIGTRAP, no message -- about 0.1s
    /// into every renderer's life. `Arc` cannot cross the OS process boundary, but a filter
    /// program is plain bytes, so it can.
    pub(crate) fn to_spec(&self) -> alloc::string::String {
        let mut out = alloc::format!("{:02x}{:02x}", self.mode(), u8::from(self.no_new_privs()));
        let filters = self.filters.lock();
        out.push_str(&alloc::format!("{:04x}", filters.len()));
        for prog in filters.iter() {
            out.push_str(&alloc::format!("{:08x}", prog.len()));
            for insn in prog.iter() {
                out.push_str(&alloc::format!(
                    "{:04x}{:02x}{:02x}{:08x}",
                    insn.code,
                    insn.jt,
                    insn.jf,
                    insn.k
                ));
            }
        }
        out
    }

    /// Restores a [`Self::to_spec`] payload onto this (fresh, unfiltered) seccomp state. `None`
    /// on a malformed spec, which leaves this state exactly as it was -- a child that cannot be
    /// given its parent's filters is better off unfiltered than half-filtered.
    pub(crate) fn restore_from_spec(&self, spec: &str) -> Option<()> {
        let mode = u8::from_str_radix(spec.get(0..2)?, 16).ok()?;
        let no_new_privs = u8::from_str_radix(spec.get(2..4)?, 16).ok()? != 0;
        let count = usize::from_str_radix(spec.get(4..8)?, 16).ok()?;
        let mut rest = spec.get(8..)?;
        let mut filters = Vec::new();
        for _ in 0..count {
            let len = usize::from_str_radix(rest.get(0..8)?, 16).ok()?;
            rest = rest.get(8..)?;
            let mut prog = Vec::new();
            for _ in 0..len {
                let code = u16::from_str_radix(rest.get(0..4)?, 16).ok()?;
                let jt = u8::from_str_radix(rest.get(4..6)?, 16).ok()?;
                let jf = u8::from_str_radix(rest.get(6..8)?, 16).ok()?;
                let k = u32::from_str_radix(rest.get(8..16)?, 16).ok()?;
                rest = rest.get(16..)?;
                prog.push(SockFilter { code, jt, jf, k });
            }
            filters.push(prog.into_boxed_slice());
        }
        self.mode.store(mode, Ordering::Relaxed);
        self.no_new_privs.store(no_new_privs, Ordering::Relaxed);
        let mut slot = self.filters.lock();
        slot.clear();
        slot.extend(filters);
        Some(())
    }

    fn evaluate(&self, nr: i32, ip: usize, args: [u64; 6]) -> Option<Verdict> {
        let mode = self.mode();
        if mode == SECCOMP_MODE_DISABLED {
            return None;
        }
        if mode == SECCOMP_MODE_STRICT {
            return Some(
                if litebox_common_linux::seccomp_strict_allows(
                    usize::try_from(nr).unwrap_or(usize::MAX),
                ) {
                    Verdict::Allow
                } else {
                    Verdict::KillThread
                },
            );
        }
        let mut data = [0u8; SECCOMP_DATA_LEN];
        data[0..4].copy_from_slice(&nr.to_ne_bytes());
        data[4..8].copy_from_slice(&AUDIT_ARCH.to_ne_bytes());
        data[8..8 + core::mem::size_of::<usize>()].copy_from_slice(&ip.to_ne_bytes());
        for (i, arg) in args.iter().enumerate() {
            let off = 16 + i * 8;
            data[off..off + 8].copy_from_slice(&arg.to_ne_bytes());
        }
        let mut worst: Option<Verdict> = None;
        for prog in self.filters.lock().iter() {
            let verdict = Verdict::from_ret(run_filter(prog, &data));
            if worst.map_or(true, |w| verdict.severity() > w.severity()) {
                worst = Some(verdict);
            }
        }
        worst
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Evaluates this thread group's seccomp filters against the syscall about to run.
    ///
    /// `None` when nothing is installed, which is the only case a syscall pays for more than the
    /// single relaxed load in [`SeccompState::evaluate`].
    pub(crate) fn seccomp_verdict(&self, ctx: &PtRegs, nr: i32) -> Option<Verdict> {
        let mut args = [0u64; 6];
        for (i, arg) in args.iter_mut().enumerate() {
            *arg = ctx.syscall_arg(i) as u64;
        }
        self.process().seccomp.evaluate(nr, ctx.get_ip(), args)
    }

    /// Applies a verdict the syscall must not survive: `Errno` skips the syscall and returns it,
    /// every other non-allow verdict skips it and delivers `SIGSYS` instead. A `Trap` returns the
    /// SYSCALL NUMBER, which is real Linux's `syscall_rollback` (`regs->ax = regs->orig_ax`), not a
    /// success value -- see the `Trap` arm's comment.
    pub(crate) fn apply_seccomp_verdict(
        &self,
        verdict: Verdict,
        nr: i32,
        ip: usize,
    ) -> Result<usize, Errno> {
        // Trap and kill verdicts are rare and always fatal-ish for the guest thread, so they are
        // worth one line each: which syscall a filter refused is the only way to tell an
        // intentional Chromium trap (it installs SIGSYS handlers for those) from a filter our
        // BPF interpreter evaluated wrongly. `Errno` verdicts are deliberately NOT logged --
        // Chromium's baseline policy answers hundreds of them per second with EPERM/ENOSYS and
        // expects every one, so logging them drowns the log (a 1GB log in 2.5 minutes earlier
        // this pass).
        let name = crate::diag::syscall_name_pub(usize::try_from(nr).unwrap_or(usize::MAX));
        match verdict {
            Verdict::Trap(data) => {
                litebox_util_log::warn!(pid:% = self.pid.get(), nr:? = nr, syscall:? = name, ip:? = ip, trap_id:? = data; "seccomp: SECCOMP_RET_TRAP, delivering SIGSYS")
            }
            Verdict::Trace => {
                litebox_util_log::warn!(pid:% = self.pid.get(), nr:? = nr, syscall:? = name, ip:? = ip; "seccomp: SECCOMP_RET_TRACE with no tracer, delivering SIGSYS")
            }
            Verdict::KillThread => {
                litebox_util_log::warn!(pid:% = self.pid.get(), nr:? = nr, syscall:? = name, ip:? = ip; "seccomp: SECCOMP_RET_KILL_THREAD, delivering SIGSYS")
            }
            Verdict::KillProcess => {
                litebox_util_log::warn!(pid:% = self.pid.get(), nr:? = nr, syscall:? = name, ip:? = ip; "seccomp: SECCOMP_RET_KILL_PROCESS, delivering SIGSYS")
            }
            Verdict::Allow | Verdict::Log | Verdict::Errno(_) => {}
        }
        match verdict {
            Verdict::Errno(data) => Err(errno_from_ret_data(data)),
            Verdict::Trap(data) => {
                // `data` is the trap id the filter chose; the guest's SIGSYS handler reads it from
                // `si_errno` (see `deliver_sigsys`), exactly as Linux's `seccomp_send_sigsys`
                // does. Without it a handler that emulates trapped syscalls cannot tell which one
                // it was asked to emulate.
                self.deliver_sigsys(false, nr, ip, data);
                // `syscall_rollback`: the SIGSYS handler's ucontext must show the syscall NUMBER in
                // `rax` and the trapping `rip`, because that is the register pair the handler
                // inspects and overwrites to emulate the syscall (`Syscall::PutValueInUcontext`).
                // Chromium's `Trap::SigSys` asserts exactly this --
                // `si_call_addr != SECCOMP_IP(ctx) || si_syscall != SECCOMP_SYSCALL(ctx) ||
                // si_arch != SECCOMP_ARCH` -- and abandons the trap with "Sanity checks are
                // failing after receiving SIGSYS." when it does not hold.
                //
                // The rollback has to be THIS function's return value rather than a write to
                // `ctx.rax`: the signal is only queued here and delivered later by
                // `process_signals`, which snapshots `ctx` into the frame, and by then
                // `handle_syscall_request` has stored whatever this returned into `ctx.rax`.
                // Returning `-ENOSYS` (as this used to) made RAX and `si_syscall` disagree on every
                // trap. A handler that emulates nothing gets the syscall number back as the return
                // value, which is also what real Linux leaves there.
                Ok(usize::try_from(nr).unwrap_or(0))
            }
            Verdict::Trace | Verdict::KillThread | Verdict::KillProcess => {
                self.deliver_sigsys(true, nr, ip, 0);
                Err(Errno::ENOSYS)
            }
            Verdict::Allow | Verdict::Log => Ok(0),
        }
    }

    /// `trap_id` becomes the delivered `siginfo`'s `si_errno`, matching Linux's
    /// `seccomp_send_sigsys` (`info->si_errno = SECCOMP_RET_DATA`): a SIGSYS handler that emulates
    /// trapped syscalls (which is what Chromium's sandbox installs one for) has no other way to
    /// learn which trap fired -- the syscall number alone is not enough when one filter traps
    /// several syscalls for different reasons.
    fn deliver_sigsys(&self, force_exit: bool, nr: i32, ip: usize, trap_id: u16) {
        let siginfo = Siginfo {
            signo: Signal::SIGSYS.as_i32(),
            errno: i32::from(trap_id),
            code: SYS_SECCOMP,
            __pad: 0,
            data: SiginfoData::new_sigsys(ip, nr, AUDIT_ARCH),
        };
        self.force_signal_with_info(Signal::SIGSYS, force_exit, siginfo);
    }

    pub(crate) fn publish_proc_seccomp(&self) {
        let process = self.process();
        let mode = process.seccomp.mode();
        let no_new_privs = process.seccomp.no_new_privs();
        self.global
            .proc_self_info
            .write()
            .with_mut(self.pid.get(), |info| {
                info.seccomp_mode = mode;
                info.no_new_privs = no_new_privs;
            });
    }

    /// `prctl(PR_SET_NO_NEW_PRIVS, 1)`: one-way, inherited by children and never cleared by
    /// `execve`. Litebox's `execve` grants no privilege escalation to begin with, so the bit only
    /// has to gate `seccomp` installs -- which it does, in [`Self::sys_seccomp`].
    pub(crate) fn set_no_new_privs(&self) {
        self.process().seccomp.set_no_new_privs();
        self.publish_proc_seccomp();
    }

    pub(crate) fn seccomp_no_new_privs(&self) -> bool {
        self.process().seccomp.no_new_privs()
    }

    pub(crate) fn seccomp_mode(&self) -> u8 {
        self.process().seccomp.mode()
    }

    /// Reads a `struct sock_fprog` from guest memory and validates its program.
    fn read_fprog(&self, args: usize) -> Result<Box<[SockFilter]>, Errno> {
        let fprog: SockFprog = UserPtr::<SockFprog>::from_usize(args)
            .read_at_offset::<Platform>(0)
            .ok_or(Errno::EFAULT)?;
        let len = usize::from(fprog.len);
        if len == 0 || len > MAX_INSNS {
            return Err(Errno::EINVAL);
        }
        let prog: UserPtr<SockFilter> = UserPtr::from_usize(fprog.filter);
        let prog = prog.to_owned_slice::<Platform>(len).ok_or(Errno::EFAULT)?;
        validate(&prog)?;
        Ok(prog)
    }

    /// Handle syscall `seccomp`.
    pub(crate) fn sys_seccomp(
        &self,
        operation: u32,
        flags: u32,
        args: usize,
    ) -> Result<usize, Errno> {
        match operation {
            SECCOMP_SET_MODE_STRICT => {
                if flags != 0 || args != 0 {
                    return Err(Errno::EINVAL);
                }
                self.check_can_install()?;
                self.process().seccomp.install_strict();
                self.publish_proc_seccomp();
                Ok(0)
            }
            SECCOMP_SET_MODE_FILTER => {
                // `NEW_LISTENER` (8) is the only defined flag not modelled here: it hands out an
                // fd for userspace notification, which has no counterpart in this shim.
                if flags
                    & !(SECCOMP_FILTER_FLAG_TSYNC
                        | SECCOMP_FILTER_FLAG_LOG
                        | SECCOMP_FILTER_FLAG_SPEC_ALLOW)
                    != 0
                {
                    return Err(Errno::EINVAL);
                }
                // Linux's `seccomp_set_mode_filter` copies `args` out of userspace BEFORE it checks
                // `no_new_privs`, so a caller that passes a bogus pointer gets EFAULT whether or not
                // it could ever have installed a filter. Chromium's `KernelSupportsSeccompBPF()`
                // probes exactly that: it calls `seccomp(SECCOMP_SET_MODE_FILTER, 0, nullptr)` and
                // reads EFAULT as "the kernel has seccomp-bpf" and anything else as "it does not".
                // Checking the privilege first turned that probe into EINVAL and made Chromium
                // conclude the sandbox was unsupported and silently run unsandboxed.
                let prog = self.read_fprog(args)?;
                self.check_can_install()?;
                self.process().seccomp.add_filter(prog);
                self.publish_proc_seccomp();
                Ok(0)
            }
            // `SECCOMP_GET_NOTIF_SIZES` is deliberately absent: it exists only for the user-
            // notification feature, which this shim does not model (`NEW_LISTENER` is rejected
            // above for the same reason), so answering it with sizes would be a lie.
            SECCOMP_GET_ACTION_AVAIL => {
                if flags != 0 {
                    return Err(Errno::EINVAL);
                }
                let action = UserPtr::<u32>::from_usize(args)
                    .read_at_offset::<Platform>(0)
                    .ok_or(Errno::EFAULT)?;
                match action {
                    SECCOMP_RET_KILL_PROCESS
                    | SECCOMP_RET_KILL_THREAD
                    | SECCOMP_RET_TRAP
                    | SECCOMP_RET_ERRNO
                    | SECCOMP_RET_USER_NOTIF
                    | SECCOMP_RET_TRACE
                    | SECCOMP_RET_LOG
                    | SECCOMP_RET_ALLOW => Ok(action as usize),
                    _ => Err(Errno::EINVAL),
                }
            }
            _ => Err(Errno::EINVAL),
        }
    }

    /// `prctl(PR_SET_SECCOMP, mode, prog)`, the pre-`seccomp(2)` form: no flags, no `TSYNC`.
    pub(crate) fn sys_prctl_set_seccomp(&self, mode: u32, prog: usize) -> Result<usize, Errno> {
        match mode {
            m if m == u32::from(SECCOMP_MODE_STRICT) => {
                self.check_can_install()?;
                self.process().seccomp.install_strict();
                self.publish_proc_seccomp();
                Ok(0)
            }
            m if m == u32::from(SECCOMP_MODE_FILTER) => {
                self.check_can_install()?;
                let prog = self.read_fprog(prog)?;
                self.process().seccomp.add_filter(prog);
                self.publish_proc_seccomp();
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    /// Installing a filter requires `no_new_privs` or `CAP_SYS_ADMIN`. Linux reports `EACCES`
    /// (not `EINVAL`, which is reserved for an unknown mode, bad flags or an invalid program) both
    /// from `seccomp(2)` and from `prctl(PR_SET_SECCOMP)`, so that is what is returned here --
    /// callers such as Chromium's sandbox treat any other failure as "the kernel has no seccomp
    /// at all" and silently drop to running unsandboxed.
    fn check_can_install(&self) -> Result<(), Errno> {
        if self.seccomp_no_new_privs() {
            return Ok(());
        }
        if self.creds().cap_eff & litebox_common_linux::CapSet::SYS_ADMIN.bits() != 0 {
            return Ok(());
        }
        Err(Errno::EACCES)
    }
}
