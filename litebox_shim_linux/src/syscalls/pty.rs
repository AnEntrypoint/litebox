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
//! but there is no kernel-side canonical-mode input buffering (no backspace/erase editing, since
//! that needs a buffer of not-yet-"readable" bytes this module doesn't have) and no
//! signal-generating special characters (^C/^Z/^\ -- these need cross-process signal delivery,
//! which this shim doesn't have at all yet, pty or otherwise). Bytes written to the master appear
//! verbatim on the slave's read side unless `ECHO` is explicitly set. This covers every consumer
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
//! Master and slave are each their own fd-table entry (this subsystem's [`PtyEnd`]), cross-wired
//! via two [`crate::channel::Channel`]s (one per direction) so each fd is independently readable
//! and writable and correctly poll()/epoll()-able. `TCGETS`/`TCSETS*`/`TIOCGWINSZ`/`TIOCSWINSZ`/
//! `TIOCGPGRP`/`TIOCSPGRP` state lives on the shared [`PtyPair`] so it's visible from both sides,
//! matching real Linux where the master and slave observe the same underlying tty state.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

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
    ShimPlatform,
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

/// A pty fd-table entry: either the master or the slave side of a pty pair.
pub(crate) enum PtyEnd<Platform: ShimPlatform> {
    Master(PtyHalf<Platform>),
    Slave(PtyHalf<Platform>),
}

impl<Platform: ShimPlatform> PtyEnd<Platform> {
    fn half(&self) -> &PtyHalf<Platform> {
        match self {
            PtyEnd::Master(h) | PtyEnd::Slave(h) => h,
        }
    }

    pub(crate) fn pair(&self) -> &Arc<PtyPair<Platform>> {
        &self.half().pair
    }

    pub(crate) fn is_master(&self) -> bool {
        matches!(self, PtyEnd::Master(_))
    }

    pub(crate) fn is_slave(&self) -> bool {
        matches!(self, PtyEnd::Slave(_))
    }

    pub(crate) fn get_status(&self) -> OFlags {
        self.half().get_status()
    }

    pub(crate) fn set_status(&self, flag: OFlags, on: bool) {
        self.half().set_status(flag, on);
    }

    pub(crate) fn read(
        &self,
        cx: &WaitContext<'_, Platform>,
        buf: &mut [u8],
    ) -> Result<usize, Errno> {
        self.half().read(cx, buf)
    }

    /// Write `buf` to this side of the pty.
    ///
    /// On the *slave* side only, this applies `ONLCR` output processing (`\n` -> `\r\n`) when the
    /// pty's current termios has `OPOST | ONLCR` set -- matching real Linux, where output
    /// processing happens on what a program writes to its controlling terminal (the slave), not
    /// on what's written to the master (which would instead go through *input* processing, e.g.
    /// `ICRNL`, that this module doesn't implement). Without this, any program that doesn't
    /// manage its own raw mode (i.e. hasn't cleared `OPOST` itself) and just writes plain `\n` --
    /// which is most programs: `ls`, `git log`, a Python script's `print()` -- renders as an
    /// unreadable "staircase" in any terminal UI reading the master (VS Code's pty panel,
    /// ttyd/wetty, xterm.js), since nothing ever adds the `\r`.
    ///
    /// On the *master* side only, if the pty's termios has `ECHO` set, the bytes actually
    /// accepted are also best-effort echoed back to the master's own read side (see
    /// [`PtyHalf::echo`]) -- this is raw-mode echo (`stty -icanon echo`), not canonical-mode line
    /// editing: no input buffering, no backspace/erase handling, and no `ISIG` special characters
    /// (^C/^Z/^\). `ECHO` is never set by default (see [`new_pty_pair`]'s termios default), so
    /// this only ever fires for a consumer that explicitly opts in via `TCSETS`.
    pub(crate) fn write(&self, cx: &WaitContext<'_, Platform>, buf: &[u8]) -> Result<usize, Errno> {
        let termios = self.pair().get_termios();
        let onlcr = !self.is_master() && {
            termios.c_oflag & (litebox_common_linux::OPOST | litebox_common_linux::ONLCR)
                == (litebox_common_linux::OPOST | litebox_common_linux::ONLCR)
        };
        let n = self.half().write(cx, buf, onlcr)?;
        if self.is_master() && termios.c_lflag & litebox_common_linux::ECHO != 0 {
            let echo_onlcr = termios.c_oflag
                & (litebox_common_linux::OPOST | litebox_common_linux::ONLCR)
                == (litebox_common_linux::OPOST | litebox_common_linux::ONLCR);
            self.half().echo(&buf[..n], echo_onlcr);
        }
        Ok(n)
    }

    pub(crate) fn with_iopollable<R>(&self, f: impl FnOnce(&dyn IOPollable) -> R) -> R {
        f(self.half())
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
        // Every other flag (input processing, canonical-mode input buffering/echo, ISIG special
        // characters) stays at zero: this module doesn't implement any of those, so claiming
        // otherwise via TCGETS would be actively misleading to a guest program deciding its own
        // behavior based on what it reads back.
        termios: Mutex::new(Termios {
            c_oflag: litebox_common_linux::OPOST | litebox_common_linux::ONLCR,
            ..Termios::default()
        }),
        winsize: Mutex::new(Winsize::default()),
        fg_pgid: AtomicI32::new(0),
        locked: AtomicBool::new(true),
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
        write: m2s_write,
        pollee: master_pollee,
        status: AtomicU32::new((OFlags::RDWR).bits()),
        pair: pair.clone(),
        echo_write: Some(s2m_write.clone()),
    });
    let slave = PtyEnd::Slave(PtyHalf {
        read: m2s_read,
        write: s2m_write,
        pollee: slave_pollee,
        status: AtomicU32::new((OFlags::RDWR).bits()),
        pair,
        echo_write: None,
    });

    let mut dt = litebox.descriptor_table_mut();
    let master = dt.insert(master);
    let slave = dt.insert(slave);
    (master, slave)
}

impl<Platform: ShimPlatform, FS: crate::ShimFS> crate::GlobalState<Platform, FS> {
    /// Handle `open("/dev/ptmx")`: allocate a new pty pair, register the slave side (never
    /// installed into any process's own fd table directly -- see [`Self::pts_open`]), and
    /// return `(master_fd, id)`, where `id` is what `TIOCGPTN`/`/dev/pts/<id>` should use.
    pub(crate) fn ptmx_open(&self) -> (PtyFd<Platform>, u32) {
        let id = self.next_pty_id.fetch_add(1, Ordering::Relaxed);
        let (master, slave) = new_pty_pair(&self.litebox, id);
        self.pty_registry.write().insert(id, slave);
        (master, id)
    }

    /// Handle `open("/dev/pts/<id>")`: produce a fresh, independent fd that duplicates the
    /// registered slave entry (the same mechanism `dup()`/`fork()` use), so every open of the
    /// same pty id shares one underlying entry. Fails with `ENXIO` if no such pty exists, or
    /// `EIO` if the master hasn't unlocked it yet (`TIOCSPTLCK`/`unlockpt`), matching real Linux
    /// devpts.
    pub(crate) fn pts_open(&self, id: u32) -> Result<PtyFd<Platform>, Errno> {
        let registry = self.pty_registry.read();
        let slave = registry.get(&id).ok_or(Errno::ENXIO)?;
        let locked = self
            .litebox
            .descriptor_table()
            .entry_handle(slave)
            .ok_or(Errno::ENXIO)?
            .with_entry(|end: &PtyEnd<Platform>| end.pair().is_locked());
        if locked {
            return Err(Errno::EIO);
        }
        self.litebox
            .descriptor_table_mut()
            .duplicate(slave)
            .ok_or(Errno::ENXIO)
    }

    /// Drop this shim's held template copy of `id`'s slave fd (called when the pty's master fd
    /// is closed). Any fds already produced by [`Self::pts_open`] are unaffected -- each holds
    /// its own independent duplicate of the same underlying entry, exactly like any other
    /// `dup()`'d fd surviving the original being closed.
    pub(crate) fn ptmx_closed(&self, id: u32) {
        if let Some(slave) = self.pty_registry.write().remove(&id) {
            drop(self.litebox.descriptor_table_mut().remove(&slave));
        }
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
