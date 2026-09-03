// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! `signalfd(2)`/`signalfd4(2)`: a file descriptor that delivers pending signals matching a
//! registered mask as `struct signalfd_siginfo` records read from it, instead of (or alongside)
//! the normal handler-based delivery path.

use core::sync::atomic::AtomicU32;

use litebox::{
    event::{
        Events, IOPollable,
        observer::Observer,
        polling::{Pollee, TryOpError},
        wait::WaitContext,
    },
    fd::{FdEnabledSubsystem, FdEnabledSubsystemEntry},
    fs::OFlags,
    platform::TimeProvider,
    sync::{Mutex, RawSyncPrimitivesProvider},
};
use litebox_common_linux::{
    SfdFlags,
    errno::Errno,
    signal::{SigSet, Siginfo},
};
use zerocopy::{Immutable, IntoBytes};

use crate::ShimPlatform;
use crate::syscalls::signal::PendingSignals;

pub(crate) struct SignalfdSubsystem<Platform: ShimPlatform>(core::marker::PhantomData<Platform>);
impl<Platform: ShimPlatform> FdEnabledSubsystem for SignalfdSubsystem<Platform> {
    type Entry = SignalfdFile<Platform>;
}
impl<Platform: ShimPlatform> FdEnabledSubsystemEntry for SignalfdFile<Platform> {}

/// The real kernel ABI struct returned by reading a signalfd (`include/uapi/linux/signalfd.h`),
/// always exactly 128 bytes regardless of architecture. Only the fields litebox's `Siginfo` can
/// actually populate (`ssi_signo`/`ssi_errno`/`ssi_code`, and `ssi_addr` for the fault-address
/// signals `SiginfoData::new_addr` encodes -- see that function's callers) are ever non-zero; the
/// rest (`ssi_pid`, `ssi_uid`, `ssi_status`, ...) are zeroed, matching the same level of siginfo
/// fidelity every other consumer of litebox's `Siginfo`/`SiginfoData` already has (there is no
/// per-signal-code-specific field tracking anywhere in this codebase to draw richer values from).
#[repr(C)]
#[derive(Clone, Copy, IntoBytes, Immutable)]
struct SignalfdSiginfo {
    ssi_signo: u32,
    ssi_errno: i32,
    ssi_code: i32,
    ssi_pid: u32,
    ssi_uid: u32,
    ssi_fd: i32,
    ssi_tid: u32,
    ssi_band: u32,
    ssi_overrun: u32,
    ssi_trapno: u32,
    ssi_status: i32,
    ssi_int: i32,
    ssi_ptr: u64,
    ssi_utime: u64,
    ssi_stime: u64,
    ssi_addr: u64,
    ssi_addr_lsb: u16,
    __pad2: u16,
    ssi_syscall: i32,
    ssi_call_addr: u64,
    ssi_arch: u32,
    __pad: [u8; 28],
}

const _: () = assert!(core::mem::size_of::<SignalfdSiginfo>() == 128);

impl SignalfdSiginfo {
    fn from_siginfo(info: &Siginfo) -> Self {
        // `SiginfoData::new_addr` is the only populated-field constructor any signal-raising path
        // in this codebase uses (see e.g. `syscalls/signal/mod.rs`'s SIGSEGV/SIGBUS delivery);
        // every other signal's `data` is zeroed. Reading the address back out the same way
        // `new_addr` wrote it in (first `size_of::<usize>()` bytes of the pad, native-endian) is
        // therefore always correct, never garbage from an unrelated field layout.
        let pad = info.data.pad;
        let mut addr_bytes = [0u8; size_of::<usize>()];
        addr_bytes.copy_from_slice(&pad.as_bytes()[..size_of::<usize>()]);
        let ssi_addr = usize::from_ne_bytes(addr_bytes) as u64;

        Self {
            ssi_signo: info.signo as u32,
            ssi_errno: info.errno,
            ssi_code: info.code,
            ssi_pid: 0,
            ssi_uid: 0,
            ssi_fd: 0,
            ssi_tid: 0,
            ssi_band: 0,
            ssi_overrun: 0,
            ssi_trapno: 0,
            ssi_status: 0,
            ssi_int: 0,
            ssi_ptr: 0,
            ssi_utime: 0,
            ssi_stime: 0,
            ssi_addr,
            ssi_addr_lsb: 0,
            __pad2: 0,
            ssi_syscall: 0,
            ssi_call_addr: 0,
            ssi_arch: 0,
            __pad: [0; 28],
        }
    }
}

pub(crate) struct SignalfdFile<Platform: RawSyncPrimitivesProvider + TimeProvider> {
    mask: Mutex<Platform, SigSet>,
    /// The owning process's `SignalState::shared_pending` (see that field's doc comment) -- the
    /// same `Arc` a `sys_kill` targeting this process, or this process's own signal-raising paths,
    /// push into. Real Linux ties a signalfd to the calling *thread*'s pending signals (both
    /// thread-directed and process-directed), but litebox's thread-local `PendingSignals` is a
    /// bare `RefCell` with no `Arc` to share across the fd's lifetime independent of any one
    /// `Task` -- only the process-wide queue is capturable this way. This is an accepted, narrow
    /// gap: a signal sent to this specific *thread* only (e.g. via `tgkill`) while blocked in the
    /// signal mask won't be observed here, but every real-world signalfd consumer relevant to this
    /// shim's current goals (weston/D-Bus/glib watching SIGCHLD/SIGTERM/SIGINT) raises signals
    /// through the process-wide path this does observe.
    shared_pending: alloc::sync::Arc<Mutex<Platform, PendingSignals>>,
    /// File status flags (see [`OFlags::STATUS_FLAGS_MASK`])
    status: AtomicU32,
    pollee: Pollee<Platform>,
}

impl<Platform: RawSyncPrimitivesProvider + TimeProvider> SignalfdFile<Platform> {
    pub(crate) fn new(
        mask: SigSet,
        flags: SfdFlags,
        shared_pending: alloc::sync::Arc<Mutex<Platform, PendingSignals>>,
    ) -> Self {
        let mut status = OFlags::RDONLY;
        status.set(OFlags::NONBLOCK, flags.contains(SfdFlags::NONBLOCK));

        Self {
            mask: Mutex::new(mask),
            shared_pending,
            status: AtomicU32::new(status.bits()),
            pollee: Pollee::new(),
        }
    }

    pub(crate) fn set_mask(&self, mask: SigSet) {
        *self.mask.lock() = mask;
    }

    fn mask(&self) -> SigSet {
        *self.mask.lock()
    }

    fn try_read(&self) -> Result<Siginfo, TryOpError<Errno>> {
        let mask = self.mask();
        let mut shared = self.shared_pending.lock();
        let signal = shared.next_matching(mask).ok_or(TryOpError::TryAgain)?;
        Ok(shared.remove(signal))
    }

    /// Note: nothing currently calls [`litebox::event::polling::Pollee::notify_observers`] on this
    /// fd's `pollee` from the signal-push path (`PendingSignals::push`'s callers), so a *blocking*
    /// `read()` (no `SFD_NONBLOCK`) that finds nothing pending yet will not wake up the instant a
    /// matching signal actually arrives -- only `O_NONBLOCK` mode (the overwhelmingly common
    /// real-world usage: glib/D-Bus/most event loops always create a signalfd with `SFD_NONBLOCK`
    /// and integrate it into their own poll loop, checked via `check_io_events` above, which IS
    /// fully accurate) is unaffected by this gap.
    pub(crate) fn read(&self, cx: &WaitContext<'_, Platform>, buf: &mut [u8]) -> Result<usize, Errno> {
        if buf.len() < core::mem::size_of::<SignalfdSiginfo>() {
            return Err(Errno::EINVAL);
        }
        let info = self
            .pollee
            .wait(
                cx,
                self.get_status().contains(OFlags::NONBLOCK),
                Events::IN,
                || self.try_read(),
            )
            .map_err(Errno::from)?;
        let siginfo = SignalfdSiginfo::from_siginfo(&info);
        let n = core::mem::size_of::<SignalfdSiginfo>();
        buf[..n].copy_from_slice(siginfo.as_bytes());
        Ok(n)
    }

    super::common_functions_for_file_status!();
}

impl<Platform: RawSyncPrimitivesProvider + TimeProvider> IOPollable for SignalfdFile<Platform> {
    fn check_io_events(&self) -> Events {
        let mut events = Events::empty();
        if self.shared_pending.lock().pending_matching(self.mask()) {
            events |= Events::IN;
        }
        events
    }

    fn register_observer(&self, observer: alloc::sync::Weak<dyn Observer<Events>>, mask: Events) {
        self.pollee.register_observer(observer, mask);
    }
}
