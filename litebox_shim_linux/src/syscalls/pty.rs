// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Unix98 pseudoterminal (PTY) support.
//!
//! Implements `/dev/ptmx` allocation, `TIOCGPTN`/`TIOCSPTLCK`, and `/dev/pts/<id>` opens, with
//! duplex master<->slave byte forwarding -- the subset of Linux's pty machinery that
//! `node-pty`/`pexpect`/`tmux`/`script`-style tools need to allocate and drive a pty.
//!
//! **Input-side line discipline is only partially implemented**: raw-mode echo (`ECHO` set
//! without `ICANON` -- e.g. `stty -icanon echo`) works (see [`PtyEnd::write`]'s echo handling),
//! and so does `ISIG` (the `VINTR`/`VQUIT`/`VSUSP` characters written to the master signal the
//! foreground process group through the cross-process registry `syscalls::signal::xproc`, see
//! [`PtyEnd::write`]), but there is no kernel-side canonical-mode input buffering (no
//! backspace/erase editing, since that needs a buffer of not-yet-"readable" bytes this module
//! doesn't have). Other bytes written to the master appear verbatim on the slave's read side
//! unless `ECHO` is explicitly set. This covers every consumer
//! that puts the pty into raw mode itself (which is what `node-pty`, `ptyprocess`/`pexpect`, and
//! most modern pty libraries do immediately after opening) but not a guest shell relying on the
//! kernel for full cooked-mode line editing.
//!
//! **Output-side processing is partially implemented**: a fresh pty defaults to `OPOST|ONLCR`
//! (matching real Linux), and slave-side writes get `\n` translated to `\r\n` accordingly (see
//! [`PtyEnd::write`]) -- this is what keeps ordinary programs that don't manage their own raw
//! mode (`ls`, `git log`, a plain `print()`) from rendering as an unreadable "staircase" in a
//! real terminal UI reading the master.
//!
//! Master and slave are each their own fd-table entry (this subsystem's [`PtyEnd`]). A pty lives
//! in [`SharedPtyTable`] whenever a slot is free, and then every end of it -- the master and each
//! `/dev/pts/<id>` open, in whichever process of the fork family -- is a `Shared*` end whose one
//! data path is that slot's two rings, with control state (termios/winsize/foreground group/lock)
//! in the slot too. Only when the table is full does a pty fall back to an in-process pair
//! ([`PtyHalf`]s cross-wired by two [`crate::channel::Channel`]s, control state on a [`PtyPair`]),
//! reachable from the allocating process alone.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use core::time::Duration;

use litebox::{
    event::{
        Events, IOPollable,
        observer::Observer,
        polling::{Pollee, TryOpError},
        wait::WaitContext,
    },
    fd::{FdEnabledSubsystem, FdEnabledSubsystemEntry, TypedFd},
    fs::OFlags,
    sync::Mutex,
};
use litebox_common_linux::{Termios, Winsize, errno::Errno};

use crate::{
    GlobalStateHandle, ShimPlatform, Task,
    channel::{Channel, ReadEnd, WriteEnd},
};

/// Ring buffer capacity for each direction of a pty pair.
const PTY_BUF_SIZE: usize = 8192;

pub(crate) struct PtySubsystem<Platform: ShimPlatform>(core::marker::PhantomData<Platform>);
impl<Platform: ShimPlatform> FdEnabledSubsystem for PtySubsystem<Platform> {
    type Entry = PtyEnd<Platform>;
}
impl<Platform: ShimPlatform> FdEnabledSubsystemEntry for PtyEnd<Platform> {}

pub(crate) type PtyFd<Platform> = TypedFd<PtySubsystem<Platform>>;

/// State shared between a pty pair's master and slave sides, mirroring what real Linux tracks
/// per-pty (as opposed to per-open-file-description).
pub(crate) struct PtyPair<Platform: ShimPlatform> {
    /// The pty's index, exposed via `TIOCGPTN` and used to build `/dev/pts/<id>`.
    pub(crate) id: u32,
    termios: Mutex<Platform, Termios>,
    winsize: Mutex<Platform, Winsize>,
    fg_pgid: AtomicI32,
    /// Starts locked, matching real Linux devpts: opening the slave before the master issues
    /// `TIOCSPTLCK(0)` (`unlockpt`) fails with `EIO`.
    locked: AtomicBool,
    /// `TIOCPKT` state: accepted and stored (`TIOCGPTPEER`'s doc comment on `IoctlArg::TIOCPKT`
    /// explains why accepting it at all matters), but not acted on -- no consumer in this
    /// codebase's terminal-emulation path reads via packet mode's control-byte-prefixed
    /// protocol, so there is nothing to change about `read()`'s behavior here.
    packet_mode: AtomicBool,
}

impl<Platform: ShimPlatform> PtyPair<Platform> {
    pub(crate) fn get_termios(&self) -> Termios {
        self.termios.lock().clone()
    }

    pub(crate) fn set_termios(&self, t: Termios) {
        *self.termios.lock() = t;
    }

    pub(crate) fn get_winsize(&self) -> Winsize {
        self.winsize.lock().clone()
    }

    pub(crate) fn set_winsize(&self, ws: Winsize) {
        *self.winsize.lock() = ws;
    }

    pub(crate) fn get_fg_pgid(&self) -> i32 {
        self.fg_pgid.load(Ordering::Relaxed)
    }

    pub(crate) fn set_fg_pgid(&self, pgid: i32) {
        self.fg_pgid.store(pgid, Ordering::Relaxed);
    }

    pub(crate) fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Acquire)
    }

    pub(crate) fn set_locked(&self, locked: bool) {
        self.locked.store(locked, Ordering::Release);
    }

    pub(crate) fn set_packet_mode(&self, enabled: bool) {
        self.packet_mode.store(enabled, Ordering::Relaxed);
    }
}

pub(crate) struct PtyHalf<Platform: ShimPlatform> {
    read: ReadEnd<Platform, u8>,
    write: WriteEnd<Platform, u8>,
    pollee: Arc<Pollee<Platform>>,
    /// File status flags (see [`OFlags::STATUS_FLAGS_MASK`]).
    status: AtomicU32,
    pair: Arc<PtyPair<Platform>>,
    /// Master side only: a clone of the slave's write end (the same direction the master itself
    /// *reads* from), used to echo bytes written to the master back to whatever's reading it --
    /// see [`PtyEnd::write`]'s echo handling. `None` on the slave side, which never echoes.
    echo_write: Option<WriteEnd<Platform, u8>>,
    /// Slave side only: a clone of the master's write end (the same direction the slave itself
    /// *reads* from -- i.e. what a real keyboard would feed in), used to synthesize a Device
    /// Status Report response when the program attached to this slave queries the cursor
    /// position (`\x1b[6n`) -- see [`PtyEnd::write`]'s DSR-responder handling. `None` on the
    /// master side, which never receives a DSR query to answer (real terminal emulators, not
    /// this shim, are the ones expected to answer a master-side reader's own `\x1b[6n`).
    dsr_reply_write: Option<WriteEnd<Platform, u8>>,
}

impl<Platform: ShimPlatform> PtyHalf<Platform> {
    super::common_functions_for_file_status!();

    fn try_read_into(&self, buf: &mut [u8]) -> Result<usize, TryOpError<Errno>> {
        let mut n = 0;
        while n < buf.len() {
            match self.read.peek_and_consume_one(|byte| {
                buf[n] = *byte;
                Ok((true, ()))
            }) {
                Ok(()) => n += 1,
                Err(Errno::ESHUTDOWN) => return Ok(n),
                Err(_) => break,
            }
        }
        if n == 0 {
            Err(TryOpError::TryAgain)
        } else {
            Ok(n)
        }
    }

    fn read(&self, cx: &WaitContext<'_, Platform>, buf: &mut [u8]) -> Result<usize, Errno> {
        self.pollee
            .wait(
                cx,
                self.get_status().contains(OFlags::NONBLOCK),
                Events::IN,
                || self.try_read_into(buf),
            )
            .map_err(Errno::from)
    }

    /// Write `buf`, optionally applying `ONLCR` output processing (`\n` -> `\r\n`) as each byte
    /// is queued.
    ///
    /// `n`, the returned/counted progress, is always in units of *original* `buf` bytes (matching
    /// `write(2)`'s contract that the return value describes how much of the caller's buffer was
    /// consumed) even though a translated `\n` enqueues two channel bytes.
    ///
    /// Edge case: if the channel has exactly one free slot when a `\n` is being translated, the
    /// `\r` can be enqueued but the paired `\n` then fails with the channel full -- since the
    /// channel has no "undo the last enqueue" operation, that `\r` is left queued without its
    /// `\n`. This is a narrow, cosmetic-only edge case (a stray `\r` rendered, not a crash, hang,
    /// or data loss) that only a stalled/slow reader against a nearly-full 8192-byte channel can
    /// trigger; not worth the added complexity of a fully atomic two-byte enqueue for that.
    fn try_write_from(&self, buf: &[u8], onlcr: bool) -> Result<usize, TryOpError<Errno>> {
        let mut n = 0;
        let mut first_err = None;
        'outer: while n < buf.len() {
            let byte = buf[n];
            let translated: &[u8] = if onlcr && byte == b'\n' {
                b"\r\n"
            } else {
                core::slice::from_ref(&byte)
            };
            for &out_byte in translated {
                match self.write.try_write_one(out_byte) {
                    Ok(()) => {}
                    Err((_, e)) => {
                        first_err = Some(e);
                        break 'outer;
                    }
                }
            }
            n += 1;
        }
        if n > 0 {
            return Ok(n);
        }
        match first_err {
            Some(Errno::EAGAIN) | None => Err(TryOpError::TryAgain),
            Some(e) => Err(TryOpError::Other(e)),
        }
    }

    fn write(
        &self,
        cx: &WaitContext<'_, Platform>,
        buf: &[u8],
        onlcr: bool,
    ) -> Result<usize, Errno> {
        self.pollee
            .wait(
                cx,
                self.get_status().contains(OFlags::NONBLOCK),
                Events::OUT,
                || self.try_write_from(buf, onlcr),
            )
            .map_err(Errno::from)
    }

    /// Best-effort echo of `buf` (the bytes just accepted by [`Self::write`]) back through
    /// `echo_write`, applying the same `\n` -> `\r\n` translation as an ordinary write when
    /// `onlcr`. Master-side only -- see the `echo_write` field doc comment.
    ///
    /// Always non-blocking and never surfaces an error to the caller: a full destination channel
    /// (`EAGAIN`) is exactly what a real terminal driver does under output backpressure (drop or
    /// stop echoing, never block the write that triggered it), and a torn-down slave (`EPIPE`,
    /// via the cloned `WriteEnd` sharing the real slave-side end's shutdown state) must not turn
    /// an otherwise-successful `write()` to the master into an error.
    fn echo(&self, buf: &[u8], onlcr: bool) {
        let Some(echo_write) = &self.echo_write else {
            return;
        };
        for &byte in buf {
            let translated: &[u8] = if onlcr && byte == b'\n' {
                b"\r\n"
            } else {
                core::slice::from_ref(&byte)
            };
            for &out_byte in translated {
                if echo_write.try_write_one(out_byte).is_err() {
                    return;
                }
            }
        }
    }

    /// Slave-side only: if `buf` contains a Device Status Report / cursor-position query
    /// (`\x1b[6n`), synthesize the reply a real terminal emulator would send back over the
    /// "keyboard" input path (`\x1b[<row>;<col>R`) -- see [`PtyEnd::write`]'s DSR-responder
    /// handling and the `dsr_reply_write` field doc comment.
    ///
    /// Without this, a program that queries cursor position and blocks on the answer (observed
    /// live: busybox `ash`'s own interactive-prompt startup issues `\x1b[6n` immediately after
    /// printing its prompt) stalls forever, since nothing previously answered this query --
    /// `docs/session-daemon-design.md`'s "Known limitations" section documented this as the
    /// as-yet-unroot-caused reason bare interactive `ash` hangs under `--pty-mode`.
    ///
    /// This shim has no real screen model (no cursor-position tracking of its own), so it always
    /// reports the cursor at row 1, column 1 -- a placeholder answer, not a tracked one. This is
    /// still correct enough to unblock a program that merely wants an initial "yes, something is
    /// listening and terminal-shaped" answer (exactly ash's use here), even though it wouldn't
    /// suffice for a program relying on an accurate mid-session cursor position (out of scope for
    /// this fix; `litebox_termemu`'s `vt100::Parser`, already used by the session-daemon feature,
    /// tracks real cursor position and could supply an accurate answer if wired up here later).
    fn maybe_reply_to_dsr(&self, buf: &[u8]) {
        let Some(reply_write) = &self.dsr_reply_write else {
            return;
        };
        if !buf
            .windows(DSR_QUERY.len())
            .any(|window| window == DSR_QUERY)
        {
            return;
        }
        for &out_byte in DSR_REPLY {
            if reply_write.try_write_one(out_byte).is_err() {
                return;
            }
        }
    }
}

impl<Platform: ShimPlatform> PtyHalf<Platform> {
    /// Shuts this half's channel ends down, waking any peer blocked on them. Idempotent (see
    /// `common_functions_for_channel!`'s `shutdown`), so safe to call redundantly from both
    /// [`crate::GlobalState::hangup_slave`] (an explicit early trigger fired at real process
    /// death, bypassing the registry's own extra `Arc` reference -- see that function's doc
    /// comment) and `Drop` (the eventual true last-`Arc`-reference release, which is what
    /// ordinarily performs this and remains the only trigger for the master side and for a slave
    /// that never had a registry template to begin with).
    fn shutdown_channel(&self) {
        self.read.shutdown();
        self.write.shutdown();
    }
}

impl<Platform: ShimPlatform> Drop for PtyHalf<Platform> {
    fn drop(&mut self) {
        // `channel::{ReadEnd,WriteEnd}` (unlike `litebox::pipes`' own end types) don't notify
        // the peer's pollee on `Drop` by themselves -- only an explicit `shutdown()` call does
        // (see `common_functions_for_channel!`). Without this, a thread blocked reading from the
        // master would never wake up when the slave's last fd closes (or vice versa): the peer
        // would still correctly observe EOF/EPIPE on its *next* poll via `is_peer_shutdown`'s
        // `Weak::upgrade` check, but nothing would prompt that next poll to happen. This runs
        // exactly once per pty side, when its last surviving fd (the last `dup()`/`fork()`-shared
        // reference to this entry) is actually closed -- matching real Linux's "HUP fires on the
        // last close of an open file description," not on every individual fd's close.
        self.shutdown_channel();
    }
}

impl<Platform: ShimPlatform> IOPollable for PtyHalf<Platform> {
    fn register_observer(&self, observer: alloc::sync::Weak<dyn Observer<Events>>, filter: Events) {
        self.pollee.register_observer(observer, filter);
    }

    fn check_io_events(&self) -> Events {
        let mut events = Events::empty();
        if self.read.is_shutdown() || self.read.is_peer_shutdown() {
            events |= Events::HUP;
        }
        if !self.read.is_empty() {
            events |= Events::IN;
        }
        if !self.write.is_full() {
            events |= Events::OUT;
        }
        events
    }
}

// ---------------------------------------------------------------------------------------------
// Shared cross-process pty data plane.
//
// A pty's `Arc`-boxed in-process state cannot be reached from another process of the fork family
// (the `GlobalState` pointer-sharing defect class, `docs/AGENTS_ARCHIVE_2026-09-18.md`), yet
// `LITEBOX_PROCESS_FORK=1` puts a terminal emulator's shell -- the child that holds the slave --
// in a different host process from the master. [`SharedPtyTable`] is a fixed-capacity,
// pointer-free field of `GlobalState` holding, per pty, the control state and two
// [`crate::syscalls::unix::SharedByteRing`]s (master->slave, slave->master). It is the only data
// path of a published pty: every read, write and readiness check of a `Shared*` end goes through
// it, whichever process the end lives in.
//
// - **Line discipline**: `ISIG`, `ICRNL`, `ECHO` and `OPOST|ONLCR` are applied on the way into
//   the rings (see [`PtyEnd::write`]); there is no canonical-mode line buffering.
// - **Wakeups**: no ring write can wake a waiter in another process, so a blocking read or write
//   re-checks on [`SHARED_PTY_POLL_INTERVAL`] ([`poll_shared`]), and `poll`/`epoll` treat a
//   `Shared*` end as needing the same bounded re-poll (see [`PtyEnd::needs_repoll`]).
// - **Hangup**: open slave file descriptions are counted per host process; once a slave has been
//   opened and none remains (or the host processes holding them died), master reads return `EIO`
//   and poll reports `IN|HUP` -- how VTE notices the session ended. Once the master is closed
//   (its slot released by [`GlobalStateHandle::ptmx_closed`]) or its host process is gone, slave
//   reads return EOF and slave writes `EIO`.
const SHARED_PTY_SLOT_EMPTY: u32 = 0;
const SHARED_PTY_SLOT_OCCUPIED: u32 = 1;

/// Realistic upper bound on simultaneously live ptys in one guest session (interactive terminal
/// emulators, `tmux`/`screen` panes, `forkpty()`-based tools) -- same bounded-capacity-over-
/// dynamic-growth sizing philosophy as `syscalls::unix::SHARED_UNIX_CONN_CAPACITY`, and subject to
/// the SAME shared-arena by-value-construction stack-overflow constraint documented on that
/// constant. Kept deliberately small (8, not 32): each slot embeds TWO
/// `crate::syscalls::unix::SharedByteRing`s (~2.6 KiB each, dominated by `SHARED_UNIX_CONN_BUF`)
/// plus two `litebox::sync::Mutex`-wrapped fields, and `Mutex`'s own `RawMutex` backing embeds a
/// fixed 32-slot `WaiterQueue` (`litebox_platform_windows_userland::WaiterQueue`) -- live-caught,
/// this pass: 32 slots (~6.7 KiB/slot, ~214 KiB total) reproduced a real `cargo test`
/// `STATUS_STACK_OVERFLOW` constructing `GlobalState` on the test thread's stack (the exact
/// by-value-construction hazard `SHARED_UNIX_CONN_CAPACITY`'s own doc comment already documents,
/// on top of that table's own already-substantial ~340 KiB), even before ever reaching the shared
/// arena. 8 slots (~54 KiB) confirmed live to NOT reproduce it (`cargo test -p litebox_shim_linux
/// --lib syscalls::pty::` -- see this module's own `#[cfg(test)]` suite, all 11 pre-existing cases
/// plus this table's own construction).
pub(crate) const SHARED_PTY_CAPACITY: usize = 8;

/// Bounded re-poll cadence for [`poll_shared`] -- same value and rationale as
/// `syscalls::unix::SHARED_UNIX_POLL_INTERVAL`.
pub(crate) const SHARED_PTY_POLL_INTERVAL: Duration = Duration::from_millis(15);

struct SharedPtySlot<Platform: ShimPlatform> {
    state: AtomicU32,
    id: AtomicU32,
    termios: Mutex<Platform, Termios>,
    winsize: Mutex<Platform, Winsize>,
    fg_pgid: AtomicI32,
    locked: AtomicBool,
    packet_mode: AtomicBool,
    /// What the master writes (synthetic keyboard input); the slave side reads this.
    master_to_slave: crate::syscalls::unix::SharedByteRing<Platform>,
    /// What the slave writes (guest program output); the master side reads this.
    slave_to_master: crate::syscalls::unix::SharedByteRing<Platform>,
    /// Open master file descriptions, counted per host process exactly like the slaves below: the
    /// master is held by the process that opened `/dev/ptmx`, and also by a cross-process fork
    /// child that inherited it (VTE/GLib's child setup issues `TIOCGPTPEER` on the inherited
    /// master before `exec`). The pty lives until the last live holder closes; slave reads see EOF
    /// once none remains.
    master_hosts: [AtomicU32; PTY_MASTER_HOLDER_SLOTS],
    master_counts: [AtomicU32; PTY_MASTER_HOLDER_SLOTS],
    /// Open slave file descriptions, counted per host process (`slave_hosts[i]` holds
    /// `slave_counts[i]` of them), so a host process that died without closing its fds stops
    /// counting. Master reads return `EIO` once a slave was opened and none remains.
    slave_hosts: [AtomicU32; PTY_SLAVE_HOLDER_SLOTS],
    slave_counts: [AtomicU32; PTY_SLAVE_HOLDER_SLOTS],
    slave_ever_opened: AtomicBool,
}

/// Distinct host processes that can hold a pty's slave open at once.
const PTY_SLAVE_HOLDER_SLOTS: usize = 16;

/// Distinct host processes that can hold a pty's master open at once.
const PTY_MASTER_HOLDER_SLOTS: usize = 4;

impl<Platform: ShimPlatform> SharedPtySlot<Platform> {
    fn slave_opened(&self, host: u32) {
        self.slave_ever_opened.store(true, Ordering::Release);
        for (h, c) in self.slave_hosts.iter().zip(&self.slave_counts) {
            if h.load(Ordering::Acquire) == host && c.load(Ordering::Acquire) > 0 {
                c.fetch_add(1, Ordering::AcqRel);
                return;
            }
        }
        for (h, c) in self.slave_hosts.iter().zip(&self.slave_counts) {
            if c.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                h.store(host, Ordering::Release);
                return;
            }
        }
    }

    fn slave_closed(&self, host: u32) {
        for (h, c) in self.slave_hosts.iter().zip(&self.slave_counts) {
            if h.load(Ordering::Acquire) == host
                && c.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                    .is_ok()
            {
                return;
            }
        }
    }

    /// A slave was opened and no live host process still holds one.
    fn slaves_gone(&self, io: &PtyIo<'_, Platform>) -> bool {
        if !self.slave_ever_opened.load(Ordering::Acquire) {
            return false;
        }
        let mut any = false;
        for (h, c) in self.slave_hosts.iter().zip(&self.slave_counts) {
            if c.load(Ordering::Acquire) == 0 {
                continue;
            }
            if io.host_alive(h.load(Ordering::Acquire)) {
                any = true;
            } else {
                c.store(0, Ordering::Release);
            }
        }
        !any
    }

    fn master_held(&self, host: u32) {
        for (h, c) in self.master_hosts.iter().zip(&self.master_counts) {
            if h.load(Ordering::Acquire) == host && c.load(Ordering::Acquire) > 0 {
                c.fetch_add(1, Ordering::AcqRel);
                return;
            }
        }
        for (h, c) in self.master_hosts.iter().zip(&self.master_counts) {
            if c.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                h.store(host, Ordering::Release);
                return;
            }
        }
    }

    fn master_released(&self, host: u32) {
        for (h, c) in self.master_hosts.iter().zip(&self.master_counts) {
            if h.load(Ordering::Acquire) == host
                && c.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                    .is_ok()
            {
                return;
            }
        }
    }

    /// No live host process holds the master any more (a holder whose process died without
    /// closing it stops counting).
    fn master_gone(&self, io: &PtyIo<'_, Platform>) -> bool {
        let mut any = false;
        for (h, c) in self.master_hosts.iter().zip(&self.master_counts) {
            if c.load(Ordering::Acquire) == 0 {
                continue;
            }
            if io.host_alive(h.load(Ordering::Acquire)) {
                any = true;
            } else {
                c.store(0, Ordering::Release);
            }
        }
        !any
    }

    fn new_empty() -> Self {
        Self {
            state: AtomicU32::new(SHARED_PTY_SLOT_EMPTY),
            id: AtomicU32::new(0),
            termios: Mutex::new(Termios::default()),
            winsize: Mutex::new(Winsize::default()),
            fg_pgid: AtomicI32::new(0),
            locked: AtomicBool::new(true),
            packet_mode: AtomicBool::new(false),
            master_to_slave: crate::syscalls::unix::SharedByteRing::new_empty(),
            slave_to_master: crate::syscalls::unix::SharedByteRing::new_empty(),
            master_hosts: core::array::from_fn(|_| AtomicU32::new(0)),
            master_counts: core::array::from_fn(|_| AtomicU32::new(0)),
            slave_hosts: core::array::from_fn(|_| AtomicU32::new(0)),
            slave_counts: core::array::from_fn(|_| AtomicU32::new(0)),
            slave_ever_opened: AtomicBool::new(false),
        }
    }
}

/// What a pty data operation on the shared transport needs besides the table: which host process
/// is asking, and whether another host process is still alive.
pub(crate) struct PtyIo<'a, Platform: ShimPlatform> {
    pub(crate) table: &'a SharedPtyTable<Platform>,
    pub(crate) platform: &'a Platform,
}

impl<Platform: ShimPlatform> PtyIo<'_, Platform> {
    fn me(&self) -> u32 {
        self.platform.current_host_pid()
    }

    fn host_alive(&self, host: u32) -> bool {
        host == self.me() || self.platform.is_process_alive(host)
    }
}

/// Shared-arena-native, fixed-capacity, pointer-free registry of every currently-allocated pty's
/// cross-process-visible existence, control state, and byte data plane -- see this module's own
/// "Shared cross-process pty data plane" doc comment above for the full design and its explicit
/// scope limits. A plain field of `GlobalState` (never behind an `Arc`/`Box`), so it inherits
/// whatever cross-process sharing `GlobalState` itself already gets for free, exactly like
/// `syscalls::unix::SharedUnixAddrPresenceTable`.
pub(crate) struct SharedPtyTable<Platform: ShimPlatform> {
    slots: [SharedPtySlot<Platform>; SHARED_PTY_CAPACITY],
}

impl<Platform: ShimPlatform> SharedPtyTable<Platform> {
    pub(crate) fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| SharedPtySlot::new_empty()),
        }
    }

    fn find(&self, id: u32) -> Option<&SharedPtySlot<Platform>> {
        self.slots.iter().find(|s| {
            s.state.load(Ordering::Acquire) == SHARED_PTY_SLOT_OCCUPIED
                && s.id.load(Ordering::Relaxed) == id
        })
    }

    /// Publishes a freshly allocated pty's existence and initial control state. Best-effort: if
    /// every slot is occupied, this one pty simply stays invisible to any OTHER process (degrades
    /// to exactly the pre-existing per-process-only behavior for this single pty), never panics --
    /// same degrade-gracefully contract as `SharedUnixAddrPresenceTable::insert`.
    pub(crate) fn publish(&self, id: u32, locked: bool, io: &PtyIo<'_, Platform>) -> bool {
        if self.try_publish(id, locked, io.me()) {
            return true;
        }
        // Full: a slot whose master's host process died without closing it is reclaimable.
        for slot in &self.slots {
            if slot.state.load(Ordering::Acquire) == SHARED_PTY_SLOT_OCCUPIED
                && slot.master_gone(io)
            {
                slot.state.store(SHARED_PTY_SLOT_EMPTY, Ordering::Release);
            }
        }
        self.try_publish(id, locked, io.me())
    }

    fn try_publish(&self, id: u32, locked: bool, master_host: u32) -> bool {
        for slot in &self.slots {
            if slot
                .state
                .compare_exchange(
                    SHARED_PTY_SLOT_EMPTY,
                    SHARED_PTY_SLOT_OCCUPIED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                slot.id.store(id, Ordering::Relaxed);
                *slot.termios.lock() = default_pty_termios();
                *slot.winsize.lock() = Winsize::default();
                slot.fg_pgid.store(0, Ordering::Relaxed);
                slot.locked.store(locked, Ordering::Relaxed);
                slot.packet_mode.store(false, Ordering::Relaxed);
                slot.master_to_slave.reset();
                slot.slave_to_master.reset();
                for c in &slot.master_counts {
                    c.store(0, Ordering::Release);
                }
                slot.master_hosts[0].store(master_host, Ordering::Release);
                slot.master_counts[0].store(1, Ordering::Release);
                for c in &slot.slave_counts {
                    c.store(0, Ordering::Release);
                }
                slot.slave_ever_opened.store(false, Ordering::Release);
                return true;
            }
        }
        false
    }

    /// Records a newly opened slave file description held by this host process.
    pub(crate) fn slave_opened(&self, id: u32, io: &PtyIo<'_, Platform>) {
        if let Some(slot) = self.find(id) {
            slot.slave_opened(io.me());
        }
    }

    /// Records the last close of a slave file description held by this host process.
    pub(crate) fn slave_closed(&self, id: u32, io: &PtyIo<'_, Platform>) {
        if let Some(slot) = self.find(id) {
            slot.slave_closed(io.me());
        }
    }

    pub(crate) fn exists(&self, id: u32) -> bool {
        self.find(id).is_some()
    }

    pub(crate) fn live_ids(&self) -> alloc::vec::Vec<u32> {
        self.slots
            .iter()
            .filter(|s| s.state.load(Ordering::Acquire) == SHARED_PTY_SLOT_OCCUPIED)
            .map(|s| s.id.load(Ordering::Relaxed))
            .collect()
    }

    /// Releases `id`'s slot. Called only by the same process that published it, at the moment its
    /// own local `pty_registry` entry is also torn down ([`GlobalStateHandle::ptmx_closed`]) --
    /// see this module's own doc comment's "No per-consumer close tracking" scope-limit paragraph
    /// for why this single-owner-releases discipline (rather than `SharedUnixConnTable`'s
    /// both-sides-must-drop discipline) is the right one here: real devpts already keeps a pty
    /// alive past every slave close, so "the master's owning process is done with this id" is the
    /// one unambiguous release trigger, and it already exists as `ptmx_closed`.
    /// Records another master file description of published pty `id` held by this host process
    /// (a cross-process fork child re-attaching the master it inherited). `false` if the pty is
    /// gone.
    pub(crate) fn master_acquire(&self, id: u32, io: &PtyIo<'_, Platform>) -> bool {
        match self.find(id) {
            Some(slot) => {
                slot.master_held(io.me());
                true
            }
            None => false,
        }
    }

    /// The last close of one master file description held by this host process. The pty itself
    /// is only torn down once no live host process holds a master any more.
    pub(crate) fn master_closed(&self, id: u32, io: &PtyIo<'_, Platform>) {
        if let Some(slot) = self.find(id) {
            slot.master_released(io.me());
            if slot.master_gone(io) {
                slot.master_to_slave.shutdown();
                slot.slave_to_master.shutdown();
                slot.state.store(SHARED_PTY_SLOT_EMPTY, Ordering::Release);
            }
        }
    }

    pub(crate) fn get_termios(&self, id: u32) -> Termios {
        self.find(id).map(|s| s.termios.lock().clone()).unwrap_or_default()
    }

    pub(crate) fn set_termios(&self, id: u32, v: Termios) {
        if let Some(s) = self.find(id) {
            *s.termios.lock() = v;
        }
    }

    pub(crate) fn get_winsize(&self, id: u32) -> Winsize {
        self.find(id).map(|s| s.winsize.lock().clone()).unwrap_or_default()
    }

    pub(crate) fn set_winsize(&self, id: u32, v: Winsize) {
        if let Some(s) = self.find(id) {
            *s.winsize.lock() = v;
        }
    }

    pub(crate) fn get_fg_pgid(&self, id: u32) -> i32 {
        self.find(id).map(|s| s.fg_pgid.load(Ordering::Relaxed)).unwrap_or(0)
    }

    pub(crate) fn set_fg_pgid(&self, id: u32, v: i32) {
        if let Some(s) = self.find(id) {
            s.fg_pgid.store(v, Ordering::Relaxed);
        }
    }

    pub(crate) fn is_locked(&self, id: u32) -> bool {
        self.find(id)
            .map(|s| s.locked.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    pub(crate) fn set_locked(&self, id: u32, v: bool) {
        if let Some(s) = self.find(id) {
            s.locked.store(v, Ordering::Release);
        }
    }

    pub(crate) fn set_packet_mode(&self, id: u32, v: bool) {
        if let Some(s) = self.find(id) {
            s.packet_mode.store(v, Ordering::Relaxed);
        }
    }

    /// Non-blocking read. The master reads what the slave wrote and gets `EIO` once every slave
    /// is closed and the data is drained (what terminal emulators such as VTE use to notice the
    /// session ended). The slave reads what the master wrote and gets EOF once the master is
    /// closed or its host process is gone.
    pub(crate) fn try_read_side(
        &self,
        id: u32,
        for_master: bool,
        buf: &mut [u8],
        io: &PtyIo<'_, Platform>,
    ) -> Result<usize, Errno> {
        let Some(slot) = self.find(id) else {
            return if for_master { Err(Errno::EIO) } else { Ok(0) };
        };
        let ring = if for_master {
            &slot.slave_to_master
        } else {
            &slot.master_to_slave
        };
        let n = ring.try_read(buf);
        if n > 0 {
            Ok(n)
        } else if for_master && slot.slaves_gone(io) {
            Err(Errno::EIO)
        } else if !for_master && slot.master_gone(io) {
            Ok(0)
        } else {
            Err(Errno::EAGAIN)
        }
    }

    /// Readiness of one end: readable when its incoming ring holds data or the other side is
    /// gone (`HUP` too), writable when its outgoing ring has room.
    pub(crate) fn events(&self, id: u32, for_master: bool, io: &PtyIo<'_, Platform>) -> Events {
        let Some(slot) = self.find(id) else {
            return Events::IN | Events::HUP;
        };
        let (incoming, outgoing) = if for_master {
            (&slot.slave_to_master, &slot.master_to_slave)
        } else {
            (&slot.master_to_slave, &slot.slave_to_master)
        };
        let mut events = Events::empty();
        if !incoming.is_empty() {
            events |= Events::IN;
        }
        let peer_gone = if for_master {
            slot.slaves_gone(io)
        } else {
            slot.master_gone(io)
        };
        if peer_gone {
            events |= Events::IN | Events::HUP;
        }
        if !outgoing.is_full() {
            events |= Events::OUT;
        }
        events
    }

    /// Non-blocking write of `buf` from one end into the other end's incoming ring, returning how
    /// many bytes of `buf` were consumed. `onlcr` translates each `\n` into `\r\n` (written as
    /// one unit, so a translation is never split). A slave write fails with `EIO` once the master
    /// is gone, as on Linux.
    pub(crate) fn try_write_side(
        &self,
        id: u32,
        for_master: bool,
        buf: &[u8],
        onlcr: bool,
        io: &PtyIo<'_, Platform>,
    ) -> Result<usize, Errno> {
        let slot = self.find(id).ok_or(Errno::EIO)?;
        if !for_master && slot.master_gone(io) {
            return Err(Errno::EIO);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let ring = if for_master {
            &slot.master_to_slave
        } else {
            &slot.slave_to_master
        };
        let n = write_translated(ring, buf, onlcr);
        if n > 0 { Ok(n) } else { Err(Errno::EAGAIN) }
    }

    /// Best-effort, non-blocking injection into the ring the master reads (`to_master`) or the
    /// one the slave reads: echo of master input, and the synthesized cursor-position reply.
    /// Dropped when the ring is full, like a terminal driver under output backpressure.
    fn inject(&self, id: u32, to_master: bool, bytes: &[u8], onlcr: bool) {
        if let Some(slot) = self.find(id) {
            let ring = if to_master {
                &slot.slave_to_master
            } else {
                &slot.master_to_slave
            };
            let _ = write_translated(ring, bytes, onlcr);
        }
    }
}

/// Writes as much of `buf` as fits into `ring`, translating `\n` to `\r\n` when `onlcr`; returns
/// the number of `buf` bytes consumed.
fn write_translated<Platform: ShimPlatform>(
    ring: &crate::syscalls::unix::SharedByteRing<Platform>,
    buf: &[u8],
    onlcr: bool,
) -> usize {
    if !onlcr {
        return ring.try_write(buf);
    }
    let mut done = 0;
    while done < buf.len() {
        let rest = &buf[done..];
        let run = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        if run > 0 {
            let n = ring.try_write(&rest[..run]);
            done += n;
            if n < run {
                break;
            }
            continue;
        }
        if !ring.try_write_all(b"\r\n") {
            break;
        }
        done += 1;
    }
    done
}

/// Abstracts over a pty's control state living either on a LOCAL `Arc<PtyPair>` (same-process fast
/// path, unchanged) or in a [`SharedPtyTable`] slot (cross-process path) -- lets `pty_ioctl`
/// (`syscalls::file`) and this module's own [`PtyEnd::write`] read/write termios/winsize/fg_pgid/
/// locked state through one uniform interface regardless of which transport a given `PtyEnd` uses.
pub(crate) enum PtyStateRef<'a, Platform: ShimPlatform> {
    /// An unpublished in-process pty (see [`GlobalStateHandle::ptmx_open`]).
    Local(&'a Arc<PtyPair<Platform>>),
    Shared(u32, &'a SharedPtyTable<Platform>),
}

impl<'a, Platform: ShimPlatform> PtyStateRef<'a, Platform> {
    pub(crate) fn id(&self) -> u32 {
        match self {
            Self::Local(p) => p.id,
            Self::Shared(id, _) => *id,
        }
    }

    pub(crate) fn get_termios(&self) -> Termios {
        match self {
            Self::Local(p) => p.get_termios(),
            Self::Shared(id, t) => t.get_termios(*id),
        }
    }

    pub(crate) fn set_termios(&self, v: Termios) {
        match self {
            Self::Local(p) => p.set_termios(v),
            Self::Shared(id, t) => t.set_termios(*id, v),
        }
    }

    pub(crate) fn get_winsize(&self) -> Winsize {
        match self {
            Self::Local(p) => p.get_winsize(),
            Self::Shared(id, t) => t.get_winsize(*id),
        }
    }

    pub(crate) fn set_winsize(&self, v: Winsize) {
        match self {
            Self::Local(p) => p.set_winsize(v),
            Self::Shared(id, t) => t.set_winsize(*id, v),
        }
    }

    pub(crate) fn get_fg_pgid(&self) -> i32 {
        match self {
            Self::Local(p) => p.get_fg_pgid(),
            Self::Shared(id, t) => t.get_fg_pgid(*id),
        }
    }

    pub(crate) fn set_fg_pgid(&self, v: i32) {
        match self {
            Self::Local(p) => p.set_fg_pgid(v),
            Self::Shared(id, t) => t.set_fg_pgid(*id, v),
        }
    }

    pub(crate) fn is_locked(&self) -> bool {
        match self {
            Self::Local(p) => p.is_locked(),
            Self::Shared(id, t) => t.is_locked(*id),
        }
    }

    pub(crate) fn set_locked(&self, v: bool) {
        match self {
            Self::Local(p) => p.set_locked(v),
            Self::Shared(id, t) => t.set_locked(*id, v),
        }
    }

    pub(crate) fn set_packet_mode(&self, v: bool) {
        match self {
            Self::Local(p) => p.set_packet_mode(v),
            Self::Shared(id, t) => t.set_packet_mode(*id, v),
        }
    }
}

/// Blocks `try_op` until it stops returning `Errno::EAGAIN`, re-checking on a short bounded
/// timeout instead of a real wake -- see this module's own "Shared cross-process pty data plane"
/// doc comment for why (nothing in this codebase can deliver a wake from one process's ring write
/// into a different process's blocked wait). Mirrors `syscalls::unix::wait_on_events_polling`'s
/// own loop structure, adapted to a plain `Errno`-returning `try_op` (no `Pollee`/observer
/// registration exists for a cross-process pty end to register against at all -- see
/// [`PtySharedHalf`]'s `IOPollable` impl).
pub(crate) fn poll_shared<Platform: ShimPlatform, R>(
    cx: &WaitContext<'_, Platform>,
    nonblock: bool,
    mut try_op: impl FnMut() -> Result<R, Errno>,
) -> Result<R, Errno> {
    let has_real_deadline = cx.deadline().is_some();
    loop {
        match try_op() {
            Ok(v) => return Ok(v),
            Err(Errno::EAGAIN) if nonblock => return Err(Errno::EAGAIN),
            Err(Errno::EAGAIN) => {}
            Err(e) => return Err(e),
        }
        let remaining = cx.remaining_timeout();
        if has_real_deadline && remaining.is_none() {
            return Err(Errno::EAGAIN);
        }
        let this_iter = remaining.map_or(SHARED_PTY_POLL_INTERVAL, |d| d.min(SHARED_PTY_POLL_INTERVAL));
        match cx.with_timeout(this_iter).sleep() {
            litebox::event::wait::WaitError::Interrupted => return Err(Errno::EINTR),
            litebox::event::wait::WaitError::TimedOut => {}
        }
    }
}

/// One end of a published pty, whose control state and data live in its [`SharedPtyTable`] slot
/// -- see this module's "Shared cross-process pty data plane" comment.
pub(crate) struct PtySharedHalf<Platform: ShimPlatform> {
    id: u32,
    is_master: bool,
    status: AtomicU32,
    // `fn() -> Platform`, not a bare `Platform`: this marker must stay `Send`/`Sync`
    // unconditionally (matching every OTHER cross-process-shared type in this file, none of which
    // require `Platform: Send`/`Sync` themselves -- `Platform` only ever appears as a type
    // parameter to already-`Send`/`Sync` primitives like `Mutex<Platform, T>`), so this fd-table
    // entry stays usable from `FdEnabledSubsystemEntry`'s own `Send` bound.
    _platform: core::marker::PhantomData<fn() -> Platform>,
}

impl<Platform: ShimPlatform> PtySharedHalf<Platform> {
    fn new(id: u32, is_master: bool) -> Self {
        Self {
            id,
            is_master,
            status: AtomicU32::new((OFlags::RDWR).bits()),
            _platform: core::marker::PhantomData,
        }
    }

    super::common_functions_for_file_status!();

    fn read(
        &self,
        cx: &WaitContext<'_, Platform>,
        buf: &mut [u8],
        io: &PtyIo<'_, Platform>,
    ) -> Result<usize, Errno> {
        let nonblock = self.get_status().contains(OFlags::NONBLOCK);
        poll_shared(cx, nonblock, || {
            io.table.try_read_side(self.id, self.is_master, buf, io)
        })
    }

    /// Output processing mirrors [`PtyHalf`]'s: the slave's `\n` becomes `\r\n` under
    /// `OPOST|ONLCR`, master input is echoed back to the master under `ECHO`, and a slave's
    /// cursor-position query gets a synthesized reply (see [`PtyHalf::maybe_reply_to_dsr`]).
    fn write(
        &self,
        cx: &WaitContext<'_, Platform>,
        buf: &[u8],
        io: &PtyIo<'_, Platform>,
        termios: &Termios,
    ) -> Result<usize, Errno> {
        let onlcr = litebox_common_linux::OFlagBits::from_bits_retain(termios.c_oflag).contains(
            litebox_common_linux::OFlagBits::OPOST | litebox_common_linux::OFlagBits::ONLCR,
        );
        let echo = litebox_common_linux::LFlagBits::from_bits_retain(termios.c_lflag)
            .contains(litebox_common_linux::LFlagBits::ECHO);
        let nonblock = self.get_status().contains(OFlags::NONBLOCK);
        let n = poll_shared(cx, nonblock, || {
            io.table
                .try_write_side(self.id, self.is_master, buf, !self.is_master && onlcr, io)
        })?;
        if self.is_master {
            if echo {
                io.table.inject(self.id, true, &buf[..n], onlcr);
            }
        } else if buf[..n].windows(DSR_QUERY.len()).any(|w| w == DSR_QUERY) {
            io.table.inject(self.id, false, DSR_REPLY, false);
        }
        Ok(n)
    }

    fn events(&self, io: &PtyIo<'_, Platform>) -> Events {
        io.table.events(self.id, self.is_master, io)
    }
}

/// Device Status Report cursor-position query, and the placeholder reply this shim gives it.
const DSR_QUERY: &[u8] = b"\x1b[6n";
const DSR_REPLY: &[u8] = b"\x1b[1;1R";

/// A pty fd-table entry: either the master or the slave side of a pty pair, via either transport
/// -- see this module's own "Shared cross-process pty data plane" doc comment.
pub(crate) enum PtyEnd<Platform: ShimPlatform> {
    Master(PtyHalf<Platform>),
    Slave(PtyHalf<Platform>),
    /// The master of a published pty (`/dev/ptmx`, or `Task::attach_pty_stdio`'s host-driven one).
    SharedMaster(PtySharedHalf<Platform>),
    /// One `/dev/pts/<id>` open of a published pty, in any process of the fork family.
    SharedSlave(PtySharedHalf<Platform>),
}

impl<Platform: ShimPlatform> PtyEnd<Platform> {
    /// Only ever called on a `Local` variant -- see the two call sites
    /// ([`Self::pair`], now removed in favor of [`Self::pty_state`], and
    /// [`crate::GlobalStateHandle::hangup_slave`]'s `shutdown_channel` call). `hangup_slave`
    /// resolves its entry exclusively through the LOCAL `pty_registry`, which by construction
    /// (see [`crate::GlobalStateHandle::pts_open`]'s own doc comment) never contains a `Shared*`
    /// variant, so this is structurally, not merely typically, unreachable for those -- the same
    /// "proven unreachable by construction" discipline `syscalls::unix::ConnTransport`'s own
    /// `unreachable!()` call sites already use.
    fn half(&self) -> &PtyHalf<Platform> {
        match self {
            PtyEnd::Master(h) | PtyEnd::Slave(h) => h,
            PtyEnd::SharedMaster(_) | PtyEnd::SharedSlave(_) => unreachable!(
                "half() is only reached via hangup_slave, which resolves entries through the \
                 local pty_registry -- that registry never contains a Shared* variant"
            ),
        }
    }

    /// Abstracts over this end's control-state storage (LOCAL `Arc<PtyPair>` vs. a
    /// [`SharedPtyTable`] slot) -- see [`PtyStateRef`].
    pub(crate) fn pty_state<'a>(&'a self, shared: &'a SharedPtyTable<Platform>) -> PtyStateRef<'a, Platform> {
        match self {
            PtyEnd::Master(h) | PtyEnd::Slave(h) => PtyStateRef::Local(&h.pair),
            PtyEnd::SharedMaster(h) | PtyEnd::SharedSlave(h) => PtyStateRef::Shared(h.id, shared),
        }
    }

    /// The LOCAL `Arc<PtyPair>` this end shares its control state through, if any -- `None` for a
    /// `Shared*` end (whose control state lives in a [`SharedPtyTable`] slot instead, reached via
    /// [`Self::pty_state`]). Exists for the few external call sites that specifically need the
    /// real `Arc<PtyPair>` object itself, e.g. to look it up by id in the LOCAL `pty_registry`
    /// (`GlobalStateHandle::hangup_slave`) -- a `Shared*` end's underlying pty was never
    /// published into THIS process's own local `pty_registry` to begin with (see
    /// `GlobalStateHandle::pts_open`'s own doc comment), so there is nothing for those call sites
    /// to reach for one; they degrade to a no-op for a `Shared*` end rather than panicking.
    pub(crate) fn local_pair(&self) -> Option<&Arc<PtyPair<Platform>>> {
        match self {
            PtyEnd::Master(h) | PtyEnd::Slave(h) => Some(&h.pair),
            PtyEnd::SharedMaster(_) | PtyEnd::SharedSlave(_) => None,
        }
    }

    /// Convenience wrapper around [`Self::local_pair`] for call sites that only need the id.
    pub(crate) fn local_id(&self) -> Option<u32> {
        self.local_pair().map(|p| p.id)
    }

    pub(crate) fn is_master(&self) -> bool {
        matches!(self, PtyEnd::Master(_) | PtyEnd::SharedMaster(_))
    }

    pub(crate) fn is_slave(&self) -> bool {
        matches!(self, PtyEnd::Slave(_) | PtyEnd::SharedSlave(_))
    }

    pub(crate) fn get_status(&self) -> OFlags {
        match self {
            PtyEnd::Master(h) | PtyEnd::Slave(h) => h.get_status(),
            PtyEnd::SharedMaster(h) | PtyEnd::SharedSlave(h) => h.get_status(),
        }
    }

    pub(crate) fn set_status(&self, flag: OFlags, on: bool) {
        match self {
            PtyEnd::Master(h) | PtyEnd::Slave(h) => h.set_status(flag, on),
            PtyEnd::SharedMaster(h) | PtyEnd::SharedSlave(h) => h.set_status(flag, on),
        }
    }

    pub(crate) fn read(
        &self,
        cx: &WaitContext<'_, Platform>,
        buf: &mut [u8],
        io: &PtyIo<'_, Platform>,
    ) -> Result<usize, Errno> {
        match self {
            PtyEnd::Master(h) | PtyEnd::Slave(h) => h.read(cx, buf),
            PtyEnd::SharedMaster(h) | PtyEnd::SharedSlave(h) => h.read(cx, buf, io),
        }
    }

    /// The pty id of a shared-transport end.
    pub(crate) fn shared_id(&self) -> Option<u32> {
        match self {
            PtyEnd::SharedMaster(h) | PtyEnd::SharedSlave(h) => Some(h.id),
            PtyEnd::Master(_) | PtyEnd::Slave(_) => None,
        }
    }

    /// Current readiness, registering `observer` where a real wakeup exists (the in-process
    /// transport). A shared-transport end has no cross-process wakeup; pollers re-check it on a
    /// bounded interval instead (see [`Self::needs_repoll`]).
    pub(crate) fn poll_events(
        &self,
        io: &PtyIo<'_, Platform>,
        observer: Option<alloc::sync::Weak<dyn Observer<Events>>>,
        mask: Events,
    ) -> Events {
        match self {
            PtyEnd::Master(h) | PtyEnd::Slave(h) => {
                if let Some(observer) = observer {
                    h.register_observer(observer, mask);
                }
                h.check_io_events()
            }
            PtyEnd::SharedMaster(h) | PtyEnd::SharedSlave(h) => h.events(io),
        }
    }

    /// Whether a poller must re-check this end on a bounded interval rather than wait for an
    /// observer notification: true for the shared transport, whose peer may be another process.
    pub(crate) fn needs_repoll(&self) -> bool {
        matches!(self, PtyEnd::SharedMaster(_) | PtyEnd::SharedSlave(_))
    }

    /// Write `buf` to this side of the pty, mirroring the result into `shared` (see this module's
    /// own doc comment) for cross-process visibility regardless of which transport this end uses.
    ///
    /// On the *slave* side only, this applies `ONLCR` output processing (`\n` -> `\r\n`) when the
    /// pty's current termios has `OPOST | ONLCR` set -- matching real Linux, where output
    /// processing happens on what a program writes to its controlling terminal (the slave), not
    /// on what's written to the master (which would instead go through *input* processing, e.g.
    /// `ICRNL`, that this module doesn't implement). Without this, any program that doesn't
    /// manage its own raw mode (i.e. hasn't cleared `OPOST` itself) and just writes plain `\n` --
    /// which is most programs: `ls`, `git log`, a Python script's `print()` -- renders as an
    /// unreadable "staircase" in any terminal UI reading the master (VS Code's pty panel,
    /// ttyd/wetty, xterm.js), since nothing ever adds the `\r`. This translation only applies to
    /// the LOCAL transport's own bytes -- the mirror copy, and a `Shared*` end's own primary
    /// write, carry raw bytes only (see this module's own doc comment's scope-limit paragraph).
    ///
    /// On the *master* side only, if the pty's termios has `ECHO` set, the bytes actually
    /// accepted are also best-effort echoed back to the master's own read side (see
    /// [`PtyHalf::echo`]) -- this is raw-mode echo (`stty -icanon echo`), not canonical-mode line
    /// editing: no input buffering and no backspace/erase handling. `ECHO` is never set by
    /// default (see [`default_pty_termios`]), so
    /// this only ever fires for a consumer that explicitly opts in via `TCSETS`. Local-transport
    /// only, same scope limit as `ONLCR` above.
    ///
    /// On the *master* side, with `ISIG` set and a foreground process group recorded
    /// (`TIOCSPGRP`), each `VINTR`/`VQUIT`/`VSUSP` byte is consumed instead of forwarded and
    /// `SIGINT`/`SIGQUIT`/`SIGTSTP` is sent to that group through `signal_group` -- Ctrl-C,
    /// Ctrl-Backslash and Ctrl-Z in a terminal emulator. The input queue is not flushed (as if
    /// `NOFLSH` were set) and nothing is echoed for the character.
    pub(crate) fn write(
        &self,
        cx: &WaitContext<'_, Platform>,
        buf: &[u8],
        io: &PtyIo<'_, Platform>,
        signal_group: &dyn Fn(i32, litebox_common_linux::signal::Signal) -> bool,
    ) -> Result<usize, Errno> {
        use litebox_common_linux::signal::Signal;
        if !self.is_master() {
            return self.write_through(cx, buf, io);
        }
        let state = self.pty_state(io.table);
        let termios = state.get_termios();
        let fg_pgid = state.get_fg_pgid();
        litebox_util_log::debug!(
            pty:% = state.id(), c_lflag:% = termios.c_lflag, fg_pgid:% = fg_pgid, len:% = buf.len();
            "pty master write: ISIG check"
        );
        if !litebox_common_linux::LFlagBits::from_bits_retain(termios.c_lflag)
            .contains(litebox_common_linux::LFlagBits::ISIG)
            || fg_pgid <= 0
        {
            return self.write_through(cx, buf, io);
        }
        let special = |b: u8| {
            [
                (litebox_common_linux::VINTR, Signal::SIGINT),
                (litebox_common_linux::VQUIT, Signal::SIGQUIT),
                (litebox_common_linux::VSUSP, Signal::SIGTSTP),
            ]
            .into_iter()
            .find(|&(i, _)| termios.c_cc[i] != 0 && termios.c_cc[i] == b)
            .map(|(_, sig)| sig)
        };
        let mut consumed = 0;
        while consumed < buf.len() {
            let rest = &buf[consumed..];
            let plain = rest
                .iter()
                .position(|&b| special(b).is_some())
                .unwrap_or(rest.len());
            if plain > 0 {
                match self.write_through(cx, &rest[..plain], io) {
                    Ok(n) => {
                        consumed += n;
                        if n < plain {
                            return Ok(consumed);
                        }
                    }
                    Err(e) if consumed == 0 => return Err(e),
                    Err(_) => return Ok(consumed),
                }
                continue;
            }
            if let Some(signal) = special(rest[0]) {
                let reached = signal_group(fg_pgid, signal);
                litebox_util_log::debug!(
                    fg_pgid:% = fg_pgid, signal:? = signal, reached:% = reached;
                    "pty master write: ISIG character signalled the foreground process group"
                );
            }
            consumed += 1;
        }
        Ok(consumed)
    }

    fn write_through(
        &self,
        cx: &WaitContext<'_, Platform>,
        buf: &[u8],
        io: &PtyIo<'_, Platform>,
    ) -> Result<usize, Errno> {
        let termios = self.pty_state(io.table).get_termios();
        let icrnl_input;
        let buf = if self.is_master()
            && litebox_common_linux::IFlagBits::from_bits_retain(termios.c_iflag)
                .contains(litebox_common_linux::IFlagBits::ICRNL)
            && buf.contains(&b'\r')
        {
            icrnl_input = buf
                .iter()
                .map(|&b| if b == b'\r' { b'\n' } else { b })
                .collect::<alloc::vec::Vec<u8>>();
            &icrnl_input[..]
        } else {
            buf
        };
        let h = match self {
            PtyEnd::SharedMaster(h) | PtyEnd::SharedSlave(h) => {
                return h.write(cx, buf, io, &termios);
            }
            PtyEnd::Master(h) | PtyEnd::Slave(h) => h,
        };
        let oflags = litebox_common_linux::OFlagBits::from_bits_retain(termios.c_oflag);
        let lflags = litebox_common_linux::LFlagBits::from_bits_retain(termios.c_lflag);
        let onlcr_wanted = oflags.contains(
            litebox_common_linux::OFlagBits::OPOST | litebox_common_linux::OFlagBits::ONLCR,
        );
        let onlcr = !self.is_master() && onlcr_wanted;
        let n = h.write(cx, buf, onlcr)?;
        if let PtyEnd::Master(h) = self
            && lflags.contains(litebox_common_linux::LFlagBits::ECHO)
        {
            h.echo(&buf[..n], onlcr_wanted);
        }
        if let PtyEnd::Slave(h) = self {
            h.maybe_reply_to_dsr(&buf[..n]);
        }
        Ok(n)
    }

}

/// A fresh pty's termios: Linux's defaults for every part of the line discipline this module
/// implements -- `ICRNL` input translation, `OPOST|ONLCR` output processing, `ECHO`, and `ISIG`
/// with the default special characters (see [`PtyEnd::write`]). `ECHO` matters beyond echoing:
/// readline records whether the terminal echoed when it started and displays typed input only if
/// it did, so a shell in a terminal emulator shows nothing typed without it. `ICANON` stays clear:
/// there is no canonical-mode line buffering, and claiming it via `TCGETS` would mislead a guest
/// deciding its own behavior from what it reads back.
fn default_pty_termios() -> Termios {
    Termios {
        c_iflag: litebox_common_linux::IFlagBits::ICRNL.bits(),
        c_oflag: litebox_common_linux::OPOST | litebox_common_linux::ONLCR,
        c_cflag: litebox_common_linux::DEFAULT_C_CFLAG,
        c_lflag: (litebox_common_linux::LFlagBits::ISIG | litebox_common_linux::LFlagBits::ECHO)
            .bits(),
        c_cc: litebox_common_linux::DEFAULT_C_CC,
        ..Termios::default()
    }
}

/// The session-daemon `--pty-mode` pty's termios: no `ECHO` and no `ICRNL`, since the host-side
/// driver forwards a real terminal that already echoes and sends its own line endings.
fn session_daemon_termios() -> Termios {
    Termios {
        c_iflag: 0,
        c_lflag: litebox_common_linux::LFlagBits::ISIG.bits(),
        ..default_pty_termios()
    }
}

/// Allocate a new pty pair: `(master, slave)`, both already inserted into the descriptor table
/// (the caller decides which raw fd, if any, each side ends up installed at).
pub(crate) fn new_pty_pair<Platform: ShimPlatform>(
    litebox: &litebox::LiteBox<Platform>,
    id: u32,
) -> (PtyFd<Platform>, PtyFd<Platform>) {
    let pair = Arc::new(PtyPair {
        id,
        // `c_oflag` defaults to `OPOST | ONLCR` -- matching a real, freshly allocated Linux
        // pty's cooked-mode default -- because that's the one piece of output-side line
        // discipline this module actually implements (see `PtyEnd::write`'s doc comment).
        termios: Mutex::new(default_pty_termios()),
        winsize: Mutex::new(Winsize::default()),
        fg_pgid: AtomicI32::new(0),
        locked: AtomicBool::new(true),
        packet_mode: AtomicBool::new(false),
    });
    let master_pollee = Arc::new(Pollee::new());
    let slave_pollee = Arc::new(Pollee::new());
    // master -> slave direction: master writes, slave reads.
    let (m2s_write, m2s_read) =
        Channel::new(PTY_BUF_SIZE, master_pollee.clone(), slave_pollee.clone()).split();
    // slave -> master direction: slave writes, master reads.
    let (s2m_write, s2m_read) =
        Channel::new(PTY_BUF_SIZE, slave_pollee.clone(), master_pollee.clone()).split();

    let master = PtyEnd::Master(PtyHalf {
        read: s2m_read,
        write: m2s_write.clone(),
        pollee: master_pollee,
        status: AtomicU32::new((OFlags::RDWR).bits()),
        pair: pair.clone(),
        echo_write: Some(s2m_write.clone()),
        dsr_reply_write: None,
    });
    let slave = PtyEnd::Slave(PtyHalf {
        read: m2s_read,
        write: s2m_write,
        pollee: slave_pollee,
        status: AtomicU32::new((OFlags::RDWR).bits()),
        pair,
        echo_write: None,
        dsr_reply_write: Some(m2s_write),
    });

    let mut dt = litebox.descriptor_table_mut();
    let master = dt.insert(master);
    let slave = dt.insert(slave);
    (master, slave)
}

impl<Platform: ShimPlatform, FS: crate::ShimFS> crate::GlobalStateHandle<Platform, FS> {
    /// Handle `open("/dev/ptmx")`: allocate a new pty pair, register the slave side (never
    /// installed into any process's own fd table directly -- see [`Self::pts_open`]), and
    /// return `(master_fd, id)`, where `id` is what `TIOCGPTN`/`/dev/pts/<id>` should use.
    ///
    /// The pty lives in [`SharedPtyTable`] whenever a slot is free, and then BOTH ends -- this
    /// master and every `/dev/pts/<id>` open, in this process or any other -- use its rings as
    /// the one data path. Only when the table is full does the pty fall back to an in-process
    /// pair that no other process can reach.
    pub(crate) fn ptmx_open(&self) -> (PtyFd<Platform>, u32) {
        let id = self.next_pty_id.fetch_add(1, Ordering::Relaxed);
        // Mirror into `pts_registry` too -- see that field's doc comment -- so `/dev/pts` lists
        // this id the moment it exists, matching real devpts.
        self.pts_registry.write().insert(id);
        // Starts locked, as on Linux: opening the slave fails with `EIO` until `TIOCSPTLCK(0)`.
        if self.shared_pty.publish(id, true, &self.pty_io()) {
            let master = PtyEnd::SharedMaster(PtySharedHalf::new(id, true));
            return (self.litebox.descriptor_table_mut().insert(master), id);
        }
        let (master, slave) = new_pty_pair(&self.litebox, id);
        self.pty_registry.write().insert(id, slave);
        (master, id)
    }

    /// The shared-transport context for pty data operations from this process.
    pub(crate) fn pty_io(&self) -> PtyIo<'_, Platform> {
        PtyIo {
            table: &self.shared_pty,
            platform: self.platform,
        }
    }

    /// Opens a new shared-transport slave for published pty `id`, counted as one more open
    /// slave file description held by this host process.
    fn open_shared_slave(&self, id: u32) -> Result<PtyFd<Platform>, Errno> {
        if self.shared_pty.is_locked(id) {
            return Err(Errno::EIO);
        }
        self.shared_pty.slave_opened(id, &self.pty_io());
        let end = PtyEnd::SharedSlave(PtySharedHalf::new(id, false));
        Ok(self.litebox.descriptor_table_mut().insert(end))
    }

    /// Called on the last close of a pty file description (`end` is the removed entry).
    pub(crate) fn pty_description_closed(&self, end: &PtyEnd<Platform>) {
        match (end.is_master(), end.shared_id(), end.local_id()) {
            (true, Some(id), _) | (true, None, Some(id)) => self.ptmx_closed(id),
            (false, Some(id), _) => self.shared_pty.slave_closed(id, &self.pty_io()),
            _ => {}
        }
    }

/// Is `id` a currently-allocated pty, i.e. does `/dev/pts/<id>` exist right now?
    ///
    /// Exists so `stat`/`access` on a pty slave path can be answered from the SAME registry that
    /// `pts_open` consults, rather than approximated. glibc's `ptsname_r` issues `TIOCGPTN`, builds
    /// `/dev/pts/<n>` and stats it before opening -- so a wrong answer here is the difference
    /// between a working `openpty()` and `xfce4-terminal`'s "error creating pty".
    ///
    /// The filesystem layer cannot answer this: `/dev/pts` is per-open shim state, not a static
    /// device table (see `litebox::fs::devices::Device::Ptmx`'s doc comment for the same split on
    /// the multiplexer side).
    pub(crate) fn pty_exists(&self, id: u32) -> bool {
        self.pty_registry.read().contains_key(&id) || self.shared_pty.exists(id)
    }

    /// Every currently-allocated pty id, for listing `/dev/pts` -- the union of this process's own
    /// local registry and [`SharedPtyTable`]'s cross-process-visible ids (a pty allocated by a
    /// DIFFERENT process in this fork family is never in the former, but is always in the latter).
    pub(crate) fn live_pty_ids(&self) -> alloc::vec::Vec<u32> {
        let mut ids: alloc::vec::Vec<u32> = self.pty_registry.read().keys().copied().collect();
        for shared_id in self.shared_pty.live_ids() {
            if !ids.contains(&shared_id) {
                ids.push(shared_id);
            }
        }
        ids
    }

    /// Handle `open("/dev/pts/<id>")`. For a published pty (any process of the fork family may
    /// have allocated it) this is a new [`PtyEnd::SharedSlave`] file description, counted as an
    /// open slave; for an unpublished in-process pty it duplicates the registered slave entry.
    /// Fails with `ENXIO` if no such pty exists, or `EIO` if the master hasn't unlocked it yet
    /// (`TIOCSPTLCK`/`unlockpt`), matching real Linux devpts.
    pub(crate) fn pts_open(&self, id: u32) -> Result<PtyFd<Platform>, Errno> {
        {
            let registry = self.pty_registry.read();
            if let Some(slave) = registry.get(&id) {
                let locked = self
                    .litebox
                    .descriptor_table()
                    .entry_handle(slave)
                    .ok_or(Errno::ENXIO)?
                    .with_entry(|end: &PtyEnd<Platform>| end.pty_state(&self.shared_pty).is_locked());
                if locked {
                    return Err(Errno::EIO);
                }
                return self
                    .litebox
                    .descriptor_table_mut()
                    .duplicate(slave)
                    .ok_or(Errno::ENXIO);
            }
        }
        if self.shared_pty.exists(id) {
            return self.open_shared_slave(id);
        }
        Err(Errno::ENXIO)
    }

    /// Drop this shim's held template copy of `id`'s slave fd (called when the pty's master fd
    /// is closed). Any fds already produced by [`Self::pts_open`] are unaffected -- each holds
    /// its own independent duplicate of the same underlying entry, exactly like any other
    /// `dup()`'d fd surviving the original being closed. Also releases `id`'s [`SharedPtyTable`]
    /// slot, if any -- see that type's own `release` doc comment for why this (the master's
    /// owning process tearing down its local registry entry) is the correct single release
    /// trigger.
    pub(crate) fn ptmx_closed(&self, id: u32) {
        if let Some(slave) = self.pty_registry.write().remove(&id) {
            drop(self.litebox.descriptor_table_mut().remove(&slave));
        }
        self.pts_registry.write().remove(&id);
        self.shared_pty.master_closed(id, &self.pty_io());
    }

    /// Re-attaches, in a cross-process fork child, the master of published pty `id` that the
    /// parent held at fork time: a new master file description counted as one more holder.
    pub(crate) fn pty_master_attach(&self, id: u32) -> Option<PtyFd<Platform>> {
        if !self.shared_pty.master_acquire(id, &self.pty_io()) {
            return None;
        }
        Some(
            self.litebox
                .descriptor_table_mut()
                .insert(PtyEnd::SharedMaster(PtySharedHalf::new(id, true))),
        )
    }

    /// Wakes a thread blocked reading `pair`'s master, matching real Linux's behavior of
    /// delivering a pty hangup the instant the process holding the slave's last real open
    /// terminates -- unconditionally, whether or not that process bothered to `close()` its own
    /// fds first.
    ///
    /// Called ONLY from [`crate::Task::close_all_fds_on_process_exit`] (see the call site's own
    /// doc comment), i.e. only at genuine process death, never from an ordinary mid-life
    /// `close()`/`sys_close`. That distinction matters: `ptmx_open`'s registry keeps one extra
    /// `Arc` reference to the slave alive purely so `/dev/pts/<id>` can still be reopened later --
    /// real Linux devpts allows exactly this (a detached tmux/screen session's slave has zero
    /// current opens yet the pty and its master both stay fully alive and reopenable). An ordinary
    /// `close()` of what happens to be the last real slave fd must NOT itself force this wakeup,
    /// or reattachment after a deliberate detach would break -- only actual process termination
    /// should. This directly calls the shared [`PtyHalf`]'s channel shutdown (looked up via the
    /// registry, which is guaranteed to still hold a live reference to it), which is the same
    /// shared instance every dup/duplicate of this pty id's slave points at.
    pub(crate) fn hangup_slave(&self, pair: &Arc<PtyPair<Platform>>) {
        let registry = self.pty_registry.read();
        if let Some(slave) = registry.get(&pair.id)
            && let Some(h) = self.litebox.descriptor_table().entry_handle(slave)
        {
            h.with_entry(|end: &PtyEnd<Platform>| end.half().shutdown_channel());
        }
    }
}

impl<Platform: ShimPlatform, FS: crate::ShimFS> Task<Platform, FS> {
    /// Session-daemon `--pty-mode` support (see `docs/session-daemon-design.md`): allocate a
    /// fresh pty pair, attach this task to its slave as controlling terminal, and replace fds
    /// 0/1/2 with the slave -- mirroring glibc's `login_tty()` (`setsid()` +
    /// `ioctl(slave, TIOCSCTTY, 0)`), the exact sequence this module's own test suite exercises
    /// (see `tests::open_unlocked_pty_pair`/`tests::tiocsctty_on_slave_makes_own_pgrp_the_foreground_group`).
    ///
    /// Unlike an ordinary guest-driven `open("/dev/ptmx")` (`GlobalState::ptmx_open`, which only
    /// registers the slave, on the assumption the *guest* itself will hold and use the master fd),
    /// this ALSO registers the master side in `global.daemon_pty_masters`, keyed by the returned
    /// pty id, so a HOST-side caller with no `Task` in scope can drive it via
    /// `LinuxShim::pty_master_read`/`pty_master_write`. The master is deliberately never installed
    /// into this task's own fd table -- nothing inside the guest should be able to `read()`/
    /// `write()` its own controlling terminal's master side directly, matching how a real
    /// `forkpty()`-spawned child never sees its own master fd either (the parent that called
    /// `forkpty()` keeps it).
    ///
    /// Returns the new pty's id (`TIOCGPTN`'s value) on success.
    pub(crate) fn attach_pty_stdio(
        &self,
        global: &GlobalStateHandle<Platform, FS>,
    ) -> Result<u32, Errno> {
        let id = global.next_pty_id.fetch_add(1, Ordering::Relaxed);
        let winsize = litebox_common_linux::Winsize {
            row: 24,
            col: 80,
            xpixel: 0,
            ypixel: 0,
        };
        global.pts_registry.write().insert(id);
        // Shared transport whenever a slot is free (see `GlobalStateHandle::ptmx_open`), so a
        // host-side reader sees output from this process and from any process it forks.
        if global.shared_pty.publish(id, false, &global.pty_io()) {
            global.shared_pty.set_winsize(id, winsize);
            global.shared_pty.set_termios(id, session_daemon_termios());
            let master = global
                .litebox
                .descriptor_table_mut()
                .insert(PtyEnd::SharedMaster(PtySharedHalf::new(id, true)));
            global.daemon_pty_masters.write().insert(id, master);
            let slave = global.pts_open(id)?;
            let scratch_fd = {
                let files = self.files.borrow();
                files.raw_descriptor_store.write().fd_into_raw_integer(slave)
            };
            return self.install_pty_stdio(scratch_fd, id);
        }
        let (master, slave) = new_pty_pair(&global.litebox, id);

        // Unlock the slave (mirrors `TIOCSPTLCK(0)`/`unlockpt()`) -- `new_pty_pair` starts every
        // fresh pty locked, matching real Linux devpts, but there is no separate guest-visible
        // `open("/dev/pts/<id>")` step here to perform the unlock through; do it directly. Also
        // give the pty a real (non-zero) default winsize here: `new_pty_pair` otherwise leaves it
        // at `Winsize::default()` (all zeros), which makes a shell that checks its terminal size
        // at startup (e.g. busybox `ash`) treat the pty as size-unknown and fall back to probing
        // via a `\x1b[6n` (Device Status Report / cursor-position query) escape sequence -- a
        // query this session-daemon feature has no consumer wired up to answer yet, stalling the
        // shell's own prompt. 24x80 matches `litebox_termemu::TerminalEmulator`'s own default
        // (see `litebox_termemu/src/lib.rs`) and `verify_live.rs`'s usage of it.
        global
            .litebox
            .descriptor_table()
            .entry_handle(&master)
            .expect("just-inserted master fd must still be present")
            .with_entry(|end: &PtyEnd<Platform>| {
                let state = end.pty_state(&global.shared_pty);
                state.set_locked(false);
                state.set_winsize(winsize);
                state.set_termios(session_daemon_termios());
            });

        global.daemon_pty_masters.write().insert(id, master);
        // Register the slave in `pty_registry` too -- exactly like `GlobalState::ptmx_open` does
        // for an ordinary guest-driven `/dev/ptmx` open -- so `GlobalState::hangup_slave` (called
        // from `Task::close_all_fds_on_process_exit` at real process death) can find this pty's
        // slave the same way it already finds every other pty's, and correctly wake up a thread
        // blocked in `LinuxShim::pty_master_read` once this process exits. Without this, a
        // session-daemon pty's master-side read would block forever past guest exit: real Linux
        // (and this shim's own `ptmx_open` path) delivers that hangup unconditionally at process
        // death, not only when the process explicitly closed its slave fds first.
        global.pty_registry.write().insert(id, slave);

        // The registry above holds the canonical slave entry now (mirroring `ptmx_open`'s own
        // comment: "never installed into any process's own fd table directly"). Get an
        // independent duplicate -- the same mechanism `pts_open`/`dup()`/`fork()` use -- to serve
        // as the scratch source for the `sys_dup` (dup2 semantics: closes whatever currently
        // occupies the target, matching real Linux's `dup2`) calls below that install it at fds
        // 0/1/2, then close the scratch fd once each of 0/1/2 holds its own independent
        // duplicate -- reusing the exact same fd-table machinery three real
        // `dup2(slave_fd, n)` calls would, rather than reaching into descriptor-table internals
        // directly.
        let scratch_fd = {
            let dup = global
                .litebox
                .descriptor_table_mut()
                .duplicate(
                    global
                        .pty_registry
                        .read()
                        .get(&id)
                        .expect("just-inserted slave must still be present"),
                )
                .expect("just-inserted slave must still be duplicable");
            let files = self.files.borrow();
            files.raw_descriptor_store.write().fd_into_raw_integer(dup)
        };
        self.install_pty_stdio(scratch_fd, id)
    }

    /// Installs the slave at raw fd `scratch_fd` as fds 0/1/2, closes the scratch fd, and makes
    /// the pty this task's controlling terminal (`login_tty()`'s sequence).
    fn install_pty_stdio(&self, scratch_fd: usize, id: u32) -> Result<u32, Errno> {
        let scratch_fd = i32::try_from(scratch_fd).map_err(|_| Errno::EMFILE)?;
        self.sys_dup(scratch_fd, Some(0), None)?;
        self.sys_dup(scratch_fd, Some(1), None)?;
        self.sys_dup(scratch_fd, Some(2), None)?;
        self.do_close(usize::try_from(scratch_fd).unwrap())?;

        self.sys_setsid()?;
        self.sys_ioctl(0, litebox_common_linux::IoctlArg::TIOCSCTTY(0))?;

        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use litebox::fs::{Mode, OFlags};
    use litebox_common_linux::{IoctlArg, Winsize, errno::Errno};

    use crate::{UserPtr, UserPtrMut};

    /// Opens `/dev/ptmx`, unlocks it (`TIOCSPTLCK(0)`), and opens the corresponding
    /// `/dev/pts/<id>`. Returns `(master_raw_fd, slave_raw_fd)`.
    fn open_unlocked_pty_pair(
        task: &crate::Task<
            crate::syscalls::tests::TestPlatform,
            crate::DefaultFS<crate::syscalls::tests::TestPlatform>,
        >,
    ) -> (i32, i32) {
        let master = task
            .sys_open("/dev/ptmx", OFlags::RDWR, Mode::empty())
            .expect("open /dev/ptmx failed")
            .cast_signed();

        let mut unlock: i32 = 0;
        let unlock_ptr = UserPtr::from_usize((&raw mut unlock).expose_provenance());
        task.sys_ioctl(master, IoctlArg::TIOCSPTLCK(unlock_ptr))
            .expect("TIOCSPTLCK failed");

        let mut id: u32 = u32::MAX;
        let id_ptr = UserPtrMut::from_usize((&raw mut id).expose_provenance());
        task.sys_ioctl(master, IoctlArg::TIOCGPTN(id_ptr))
            .expect("TIOCGPTN failed");
        assert_ne!(id, u32::MAX, "TIOCGPTN must write a real pty id");

        let pts_path = alloc::format!("/dev/pts/{id}");
        let slave = task
            .sys_open(&pts_path, OFlags::RDWR, Mode::empty())
            .expect("open /dev/pts/<id> failed after unlocking")
            .cast_signed();

        (master, slave)
    }

    #[test]
    fn pts_open_fails_eio_until_master_unlocks_it() {
        let task = crate::syscalls::tests::init_platform(None);

        let master = task
            .sys_open("/dev/ptmx", OFlags::RDWR, Mode::empty())
            .unwrap()
            .cast_signed();

        let mut id: u32 = u32::MAX;
        let id_ptr = UserPtrMut::from_usize((&raw mut id).expose_provenance());
        task.sys_ioctl(master, IoctlArg::TIOCGPTN(id_ptr)).unwrap();

        let pts_path = alloc::format!("/dev/pts/{id}");
        assert_eq!(
            task.sys_open(&pts_path, OFlags::RDWR, Mode::empty())
                .unwrap_err(),
            Errno::EIO,
            "a freshly allocated pty's slave must stay locked until TIOCSPTLCK(0)"
        );

        // Opening a pty id that was never allocated at all is ENXIO, not EIO.
        assert_eq!(
            task.sys_open("/dev/pts/999999", OFlags::RDWR, Mode::empty())
                .unwrap_err(),
            Errno::ENXIO
        );

        let mut unlock: i32 = 0;
        let unlock_ptr = UserPtr::from_usize((&raw mut unlock).expose_provenance());
        task.sys_ioctl(master, IoctlArg::TIOCSPTLCK(unlock_ptr))
            .unwrap();
        assert!(
            task.sys_open(&pts_path, OFlags::RDWR, Mode::empty())
                .is_ok(),
            "TIOCSPTLCK(0) must unlock the slave for opening"
        );
    }

    #[test]
    fn slave_writes_get_onlcr_translated_by_default() {
        // A fresh pty defaults to OPOST|ONLCR (matching real Linux's cooked-mode default), and
        // this is the one piece of output-side line discipline actually implemented: a plain
        // `\n` written by whatever's attached to the slave (an ordinary program that doesn't
        // manage its own raw mode -- most programs) must come out the master side as `\r\n`.
        let task = crate::syscalls::tests::init_platform(None);
        let (master, slave) = open_unlocked_pty_pair(&task);

        let n = task
            .sys_write(slave, b"line1\nline2\n", None)
            .expect("write to slave failed");
        assert_eq!(
            n,
            b"line1\nline2\n".len(),
            "return value counts original bytes, not translated ones"
        );

        let mut buf = [0u8; 64];
        let n = task
            .sys_read(master, &mut buf, None)
            .expect("read from master failed");
        assert_eq!(&buf[..n], b"line1\r\nline2\r\n");
    }

    #[test]
    fn master_writes_are_not_onlcr_translated() {
        // ONLCR is output processing for what a program writes to its controlling terminal (the
        // slave); writing to the master simulates something typed at a keyboard and must not be
        // touched by it, regardless of the pty's OPOST|ONLCR default.
        let task = crate::syscalls::tests::init_platform(None);
        let (master, slave) = open_unlocked_pty_pair(&task);

        task.sys_write(master, b"typed\n", None)
            .expect("write to master failed");
        let mut buf = [0u8; 64];
        let n = task
            .sys_read(slave, &mut buf, None)
            .expect("read from slave failed");
        assert_eq!(&buf[..n], b"typed\n");
    }

    #[test]
    fn onlcr_is_not_applied_once_opost_is_cleared() {
        // A consumer that puts the pty in raw mode (cfmakeraw()-style, which clears OPOST among
        // other flags -- exactly what node-pty/pexpect/ptyprocess do) must see raw, untranslated
        // bytes even on the slave side.
        let task = crate::syscalls::tests::init_platform(None);
        let (master, slave) = open_unlocked_pty_pair(&task);

        let mut raw_termios = litebox_common_linux::Termios::default();
        let set_ptr = UserPtr::from_usize((&raw mut raw_termios).expose_provenance());
        task.sys_ioctl(slave, IoctlArg::TCSETS(set_ptr))
            .expect("TCSETS failed");

        task.sys_write(slave, b"raw\n", None)
            .expect("write to slave failed");
        let mut buf = [0u8; 64];
        let n = task
            .sys_read(master, &mut buf, None)
            .expect("read from master failed");
        assert_eq!(&buf[..n], b"raw\n");
    }

    #[test]
    fn tiocgptn_is_master_only() {
        let task = crate::syscalls::tests::init_platform(None);
        let (_master, slave) = open_unlocked_pty_pair(&task);

        let mut id: u32 = 0;
        let id_ptr = UserPtrMut::from_usize((&raw mut id).expose_provenance());
        assert_eq!(
            task.sys_ioctl(slave, IoctlArg::TIOCGPTN(id_ptr)),
            Err(Errno::ENOTTY),
            "TIOCGPTN on the slave side must fail, matching real Linux"
        );
    }

    #[test]
    fn master_and_slave_are_independently_readable_and_writable() {
        let task = crate::syscalls::tests::init_platform(None);
        let (master, slave) = open_unlocked_pty_pair(&task);

        let n = task
            .sys_write(master, b"hello from master", None)
            .expect("write to master failed");
        assert_eq!(n, b"hello from master".len());
        let mut buf = [0u8; 64];
        let n = task
            .sys_read(slave, &mut buf, None)
            .expect("read from slave failed");
        assert_eq!(&buf[..n], b"hello from master");

        let n = task
            .sys_write(slave, b"hi master", None)
            .expect("write to slave failed");
        assert_eq!(n, b"hi master".len());
        let mut buf2 = [0u8; 64];
        let n = task
            .sys_read(master, &mut buf2, None)
            .expect("read from master failed");
        assert_eq!(&buf2[..n], b"hi master");
    }

    #[test]
    fn tiocsctty_on_slave_makes_own_pgrp_the_foreground_group() {
        // Mirrors glibc's login_tty(): setsid() (own process-group leader) followed by
        // ioctl(slave_fd, TIOCSCTTY, 0) -- the exact sequence forkpty()-based tools (node-pty,
        // Python's os.forkpty(), tmux, script) rely on to attach a freshly forked child to its
        // pty as a controlling terminal.
        let task = crate::syscalls::tests::init_platform(None);
        let (_master, slave) = open_unlocked_pty_pair(&task);

        let pid = task.sys_setsid().expect("setsid must succeed");

        assert_eq!(task.sys_ioctl(slave, IoctlArg::TIOCSCTTY(0)), Ok(0));

        let mut got_pgrp: i32 = -1;
        let got_ptr = UserPtrMut::from_usize((&raw mut got_pgrp).expose_provenance());
        assert_eq!(task.sys_ioctl(slave, IoctlArg::TIOCGPGRP(got_ptr)), Ok(0));
        assert_eq!(got_pgrp, pid);
    }

    #[test]
    fn winsize_is_shared_between_master_and_slave() {
        let task = crate::syscalls::tests::init_platform(None);
        let (master, slave) = open_unlocked_pty_pair(&task);

        let mut ws = Winsize {
            row: 40,
            col: 120,
            xpixel: 0,
            ypixel: 0,
        };
        let ws_ptr = UserPtr::from_usize((&raw mut ws).expose_provenance());
        assert_eq!(task.sys_ioctl(master, IoctlArg::TIOCSWINSZ(ws_ptr)), Ok(0));

        let mut got = Winsize::default();
        let got_ptr = UserPtrMut::from_usize((&raw mut got).expose_provenance());
        assert_eq!(task.sys_ioctl(slave, IoctlArg::TIOCGWINSZ(got_ptr)), Ok(0));
        assert_eq!((got.row, got.col), (40, 120));
    }

    #[test]
    fn pts_can_be_reopened_after_all_slave_fds_close() {
        let task = crate::syscalls::tests::init_platform(None);
        let (master, slave1) = open_unlocked_pty_pair(&task);
        task.sys_close(slave1)
            .expect("closing first slave open failed");

        // Even though every slave *open* was just closed, the pty itself (and its master) stays
        // alive -- matching real Linux, where a detaching terminal multiplexer (tmux/screen)
        // relies on exactly this: closing the slave doesn't tear down the pty, and `/dev/pts/<id>`
        // can be reopened later to reattach.
        let mut id: u32 = 0;
        let id_ptr = UserPtrMut::from_usize((&raw mut id).expose_provenance());
        task.sys_ioctl(master, IoctlArg::TIOCGPTN(id_ptr)).unwrap();
        let pts_path = alloc::format!("/dev/pts/{id}");
        let slave2 = task
            .sys_open(&pts_path, OFlags::RDWR, Mode::empty())
            .expect("re-opening /dev/pts/<id> after the only slave fd closed must still work")
            .cast_signed();

        let n = task.sys_write(master, b"still alive", None).unwrap();
        assert_eq!(n, b"still alive".len());
        let mut buf = [0u8; 32];
        let n = task.sys_read(slave2, &mut buf, None).unwrap();
        assert_eq!(&buf[..n], b"still alive");
    }

    #[test]
    fn echo_is_off_by_default() {
        // ECHO is never set by default (see `new_pty_pair`'s termios default), so writing to the
        // master must not produce anything on the master's own read side.
        let task = crate::syscalls::tests::init_platform(None);
        let (master, _slave) = open_unlocked_pty_pair(&task);

        task.sys_fcntl(
            master,
            litebox_common_linux::FcntlArg::SETFL(OFlags::NONBLOCK),
        )
        .expect("fcntl(F_SETFL, O_NONBLOCK) failed");

        task.sys_write(master, b"typed", None)
            .expect("write to master failed");

        let mut buf = [0u8; 64];
        assert_eq!(
            task.sys_read(master, &mut buf, None),
            Err(Errno::EAGAIN),
            "no echo must appear on the master's read side when ECHO is unset"
        );
    }

    #[test]
    fn echo_reflects_master_writes_back_while_still_delivering_them_to_the_slave() {
        // Regression test for the ECHO ("raw-mode echo", stty -icanon echo) slice of input-side
        // line discipline: with ECHO set, bytes written to the master (simulating what's typed at
        // a keyboard) must both (a) still reach the slave's read side unmodified (the real input
        // path a shell reads as stdin) and (b) be echoed back to the master's own read side (what
        // a terminal display shows as the user types), matching real Linux's `n_tty` echo.
        let task = crate::syscalls::tests::init_platform(None);
        let (master, slave) = open_unlocked_pty_pair(&task);

        let mut termios = litebox_common_linux::Termios {
            c_lflag: litebox_common_linux::ECHO,
            ..litebox_common_linux::Termios::default()
        };
        let set_ptr = UserPtr::from_usize((&raw mut termios).expose_provenance());
        task.sys_ioctl(master, IoctlArg::TCSETS(set_ptr))
            .expect("TCSETS failed");

        let n = task
            .sys_write(master, b"hi", None)
            .expect("write to master failed");
        assert_eq!(n, 2);

        let mut slave_buf = [0u8; 64];
        let n = task
            .sys_read(slave, &mut slave_buf, None)
            .expect("read from slave failed");
        assert_eq!(
            &slave_buf[..n],
            b"hi",
            "ECHO must not change what the slave (the real input path) receives"
        );

        let mut master_buf = [0u8; 64];
        let n = task
            .sys_read(master, &mut master_buf, None)
            .expect("read from master failed");
        assert_eq!(
            &master_buf[..n],
            b"hi",
            "ECHO must reflect the typed bytes back to the master's own read side"
        );
    }

    #[test]
    fn echo_applies_onlcr_but_does_not_affect_what_the_slave_receives() {
        let task = crate::syscalls::tests::init_platform(None);
        let (master, slave) = open_unlocked_pty_pair(&task);

        let mut termios = litebox_common_linux::Termios {
            c_lflag: litebox_common_linux::ECHO,
            c_oflag: litebox_common_linux::OPOST | litebox_common_linux::ONLCR,
            ..litebox_common_linux::Termios::default()
        };
        let set_ptr = UserPtr::from_usize((&raw mut termios).expose_provenance());
        task.sys_ioctl(master, IoctlArg::TCSETS(set_ptr))
            .expect("TCSETS failed");

        task.sys_write(master, b"hi\n", None)
            .expect("write to master failed");

        let mut slave_buf = [0u8; 64];
        let n = task
            .sys_read(slave, &mut slave_buf, None)
            .expect("read from slave failed");
        assert_eq!(
            &slave_buf[..n],
            b"hi\n",
            "the input path itself is never ONLCR-translated"
        );

        let mut master_buf = [0u8; 64];
        let n = task
            .sys_read(master, &mut master_buf, None)
            .expect("read from master failed");
        assert_eq!(
            &master_buf[..n],
            b"hi\r\n",
            "the echoed copy goes through the same ONLCR output processing as an ordinary write"
        );
    }

    #[test]
    fn echo_does_not_error_when_the_slave_is_already_closed() {
        // The echo path shares the slave's real write end's shutdown state (it's a clone of the
        // same underlying WriteEnd), so once the slave is gone, echoing must be silently skipped
        // rather than turning an otherwise-successful write() to the master into an error.
        let task = crate::syscalls::tests::init_platform(None);
        let (master, slave) = open_unlocked_pty_pair(&task);

        let mut termios = litebox_common_linux::Termios {
            c_lflag: litebox_common_linux::ECHO,
            ..litebox_common_linux::Termios::default()
        };
        let set_ptr = UserPtr::from_usize((&raw mut termios).expose_provenance());
        task.sys_ioctl(master, IoctlArg::TCSETS(set_ptr))
            .expect("TCSETS failed");

        task.sys_close(slave).expect("closing slave failed");

        let n = task
            .sys_write(master, b"typed", None)
            .expect("write to master must still succeed once the slave is closed");
        assert_eq!(n, b"typed".len());
    }

    #[test]
    fn master_close_surfaces_epipe_on_slave_write() {
        let task = crate::syscalls::tests::init_platform(None);
        let (master, slave) = open_unlocked_pty_pair(&task);

        task.sys_close(master).expect("closing master failed");

        // The slave fd itself is a genuinely separate fd-table entry and stays open, but with no
        // master left to ever read them, writes to it must fail immediately rather than block
        // forever waiting for buffer space a reader will never free up.
        assert_eq!(
            task.sys_write(slave, b"anyone listening?", None),
            Err(Errno::EPIPE)
        );
    }
}
