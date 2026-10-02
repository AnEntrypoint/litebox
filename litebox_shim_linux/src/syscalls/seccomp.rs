// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! `seccomp(2)`, `prctl(PR_SET_SECCOMP)` and the classic-BPF filter every guest syscall is evaluated
//! against.

use crate::{ShimFS, ShimPlatform, Task, UserPtr};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use litebox::sync::Mutex;
use litebox_common_linux::errno::Errno;
use litebox_common_linux::signal::{Siginfo, SiginfoData, Signal};
use litebox_common_linux::PtRegs;
use zerocopy::FromBytes;

pub(crate) const SECCOMP_MODE_DISABLED: u8 = 0;
pub(crate) const SECCOMP_MODE_STRICT: u8 = 1;
pub(crate) const SECCOMP_MODE_FILTER: u8 = 2;

const SECCOMP_SET_MODE_STRICT: u32 = 0;
const SECCOMP_SET_MODE_FILTER: u32 = 1;

const SECCOMP_FILTER_FLAG_TSYNC: u32 = 1;
const SECCOMP_FILTER_FLAG_LOG: u32 = 2;
const SECCOMP_FILTER_FLAG_SPEC_ALLOW: u32 = 4;

const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
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
    Trap,
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
            SECCOMP_RET_TRAP => Self::Trap,
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
            Self::Trap => 4,
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
                    if start.checked_add(width).map_or(true, |end| end > SECCOMP_DATA_LEN) {
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

    fn evaluate(&self, nr: i32, ip: usize, args: [u64; 6]) -> Option<Verdict> {
        let mode = self.mode();
        if mode == SECCOMP_MODE_DISABLED {
            return None;
        }
        if mode == SECCOMP_MODE_STRICT {
            return Some(if litebox_common_linux::seccomp_strict_allows(
                usize::try_from(nr).unwrap_or(usize::MAX),
            ) {
                Verdict::Allow
            } else {
                Verdict::KillThread
            });
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
        self.process()
            .seccomp
            .evaluate(nr, ctx.get_ip(), args)
    }

    /// Applies a verdict the syscall must not survive: `Errno` skips the syscall and returns it,
    /// every other non-allow verdict skips it and delivers `SIGSYS` instead.
    pub(crate) fn apply_seccomp_verdict(
        &self,
        verdict: Verdict,
        nr: i32,
        ip: usize,
    ) -> Result<usize, Errno> {
        match verdict {
            Verdict::Errno(data) => Err(errno_from_ret_data(data)),
            Verdict::Trap => {
                self.deliver_sigsys(false, nr, ip);
                Err(Errno::ENOSYS)
            }
            Verdict::Trace | Verdict::KillThread | Verdict::KillProcess => {
                self.deliver_sigsys(true, nr, ip);
                Err(Errno::ENOSYS)
            }
            Verdict::Allow | Verdict::Log => Ok(0),
        }
    }

    fn deliver_sigsys(&self, force_exit: bool, nr: i32, ip: usize) {
        let siginfo = Siginfo {
            signo: Signal::SIGSYS.as_i32(),
            errno: 0,
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
                if flags & !(SECCOMP_FILTER_FLAG_TSYNC
                    | SECCOMP_FILTER_FLAG_LOG
                    | SECCOMP_FILTER_FLAG_SPEC_ALLOW)
                    != 0
                {
                    return Err(Errno::EINVAL);
                }
                self.check_can_install()?;
                let prog = self.read_fprog(args)?;
                self.process().seccomp.add_filter(prog);
                self.publish_proc_seccomp();
                Ok(0)
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

    /// Installing a filter requires `no_new_privs` or `CAP_SYS_ADMIN`. Linux reports `EPERM` for
    /// both failures; the missing-`no_new_privs` case is the one `prctl(2)` documents as `EINVAL`
    /// and the one every real caller (Chromium's sandbox, bwrap) hits, so it is what is
    /// distinguished here.
    fn check_can_install(&self) -> Result<(), Errno> {
        if self.seccomp_no_new_privs() {
            return Ok(());
        }
        if self.creds().cap_eff & litebox_common_linux::CapSet::SYS_ADMIN.bits() != 0 {
            return Ok(());
        }
        Err(Errno::EINVAL)
    }
}
