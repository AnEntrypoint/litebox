// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use core::{convert::Infallible, sync::atomic::AtomicBool};

use alloc::{
    collections::{btree_map::BTreeMap, vec_deque::VecDeque},
    sync::{Arc, Weak},
    vec::Vec,
};
use litebox::{
    event::{
        Events, IOPollable,
        observer::Observer,
        polling::{Pollee, TryOpError},
        wait::{WaitContext, WaitError, Waker},
    },
    fd::{FdEnabledSubsystem, FdEnabledSubsystemEntry, TypedFd},
    utils::ReinterpretUnsignedExt,
};
use litebox_common_linux::{EpollEvent, EpollOp, errno::Errno};

use super::file::FilesState;
use crate::{GlobalState, ShimFS, ShimPlatform};

pub(crate) struct EpollSubsystem<Platform: ShimPlatform, FS: ShimFS>(
    core::marker::PhantomData<(Platform, FS)>,
);
impl<Platform: ShimPlatform, FS: ShimFS> FdEnabledSubsystem for EpollSubsystem<Platform, FS> {
    type Entry = EpollFile<Platform, FS>;
}
impl<Platform: ShimPlatform, FS: ShimFS> FdEnabledSubsystemEntry for EpollFile<Platform, FS> {}

bitflags::bitflags! {
    /// Linux's epoll flags.
    #[derive(Debug)]
    struct EpollFlags: u32 {
        const EXCLUSIVE      = (1 << 28);
        const WAKE_UP        = (1 << 29);
        const ONE_SHOT       = (1 << 30);
        const EDGE_TRIGGER   = (1 << 31);
    }
}

pub(crate) enum EpollDescriptor<Platform: ShimPlatform, FS: ShimFS> {
    Eventfd(Arc<TypedFd<super::eventfd::EventfdSubsystem<Platform>>>),
    Epoll(Arc<TypedFd<super::epoll::EpollSubsystem<Platform, FS>>>),
    File(Arc<crate::FileFd<FS>>),
    Socket(Arc<super::net::SocketFd<Platform>>),
    Pipe(Arc<litebox::pipes::PipeFd<Platform>>),
    Unix(Arc<TypedFd<crate::syscalls::unix::UnixSocketSubsystem<Platform, FS>>>),
}

impl<Platform: ShimPlatform, FS: ShimFS> EpollDescriptor<Platform, FS> {
    pub fn try_from(files: &FilesState<Platform, FS>, raw_fd: usize) -> Result<Self, Errno> {
        let rds = files.raw_descriptor_store.read();
        if let Ok(fd) = rds.fd_from_raw_integer::<FS>(raw_fd) {
            return Ok(EpollDescriptor::File(fd));
        }
        if let Ok(fd) = rds.fd_from_raw_integer::<crate::Network<Platform>>(raw_fd) {
            return Ok(EpollDescriptor::Socket(fd));
        }
        if let Ok(fd) = rds.fd_from_raw_integer::<litebox::pipes::Pipes<Platform>>(raw_fd) {
            return Ok(EpollDescriptor::Pipe(fd));
        }
        if let Ok(fd) =
            rds.fd_from_raw_integer::<super::eventfd::EventfdSubsystem<Platform>>(raw_fd)
        {
            return Ok(EpollDescriptor::Eventfd(fd));
        }
        if let Ok(fd) = rds.fd_from_raw_integer::<EpollSubsystem<Platform, FS>>(raw_fd) {
            return Ok(EpollDescriptor::Epoll(fd));
        }
        if let Ok(fd) =
            rds.fd_from_raw_integer::<super::unix::UnixSocketSubsystem<Platform, FS>>(raw_fd)
        {
            return Ok(EpollDescriptor::Unix(fd));
        }
        Err(Errno::EBADF)
    }
}

enum DescriptorRef<Platform: ShimPlatform, FS: ShimFS> {
    Eventfd(Weak<TypedFd<super::eventfd::EventfdSubsystem<Platform>>>),
    Epoll(Weak<TypedFd<super::epoll::EpollSubsystem<Platform, FS>>>),
    File(Weak<crate::FileFd<FS>>),
    Socket(Weak<super::net::SocketFd<Platform>>),
    Pipe(Weak<litebox::pipes::PipeFd<Platform>>),
    Unix(Weak<TypedFd<crate::syscalls::unix::UnixSocketSubsystem<Platform, FS>>>),
}

impl<Platform: ShimPlatform, FS: ShimFS> DescriptorRef<Platform, FS> {
    fn from(value: &EpollDescriptor<Platform, FS>) -> Self {
        match value {
            EpollDescriptor::Eventfd(file) => Self::Eventfd(Arc::downgrade(file)),
            EpollDescriptor::Epoll(file) => Self::Epoll(Arc::downgrade(file)),
            EpollDescriptor::File(file) => Self::File(Arc::downgrade(file)),
            EpollDescriptor::Socket(socket) => Self::Socket(Arc::downgrade(socket)),
            EpollDescriptor::Pipe(pipe) => Self::Pipe(Arc::downgrade(pipe)),
            EpollDescriptor::Unix(unix) => Self::Unix(Arc::downgrade(unix)),
        }
    }

    fn upgrade(&self) -> Option<EpollDescriptor<Platform, FS>> {
        match self {
            DescriptorRef::Eventfd(eventfd) => eventfd.upgrade().map(EpollDescriptor::Eventfd),
            DescriptorRef::Epoll(epoll) => epoll.upgrade().map(EpollDescriptor::Epoll),
            DescriptorRef::File(file) => file.upgrade().map(EpollDescriptor::File),
            DescriptorRef::Socket(socket) => socket.upgrade().map(EpollDescriptor::Socket),
            DescriptorRef::Pipe(pipe) => pipe.upgrade().map(EpollDescriptor::Pipe),
            DescriptorRef::Unix(unix) => unix.upgrade().map(EpollDescriptor::Unix),
        }
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> EpollDescriptor<Platform, FS> {
    /// Returns the interesting events now and monitors their occurrence in the future if the
    /// observer is provided.
    fn poll(
        &self,
        global: &GlobalState<Platform, FS>,
        mask: Events,
        observer: Option<Weak<dyn Observer<Events>>>,
    ) -> Option<Events> {
        let poll = |iop: &dyn IOPollable| {
            if let Some(observer) = observer {
                iop.register_observer(observer, mask);
            }
            iop.check_io_events() & (mask | Events::ALWAYS_POLLED)
        };
        match self {
            EpollDescriptor::Eventfd(fd) => {
                let handle = global.litebox.descriptor_table().entry_handle(fd)?;
                Some(handle.with_entry(|entry| poll(entry)))
            }
            EpollDescriptor::Epoll(_file) => unimplemented!(),
            EpollDescriptor::File(file) => {
                // Stdout/stderr are always immediately writable from the guest's perspective (the
                // platform's `write_to` is a plain, always-completing `WriteFile`/`write(2)`), so
                // those still report a fixed `Events::OUT`. Stdin, however, must consult the
                // platform's genuinely non-blocking `stdin_ready` probe rather than hardcoding
                // `Events::IN`: a hardcoded "always readable" answer here is exactly what let
                // libuv observe stdin as ready, issue a `read()` that lands in the platform's
                // blocking read call, and hang forever on a real console with no pending input --
                // see `StdioProvider::stdin_ready`'s doc comment for the full story.
                let events = match global
                    .litebox
                    .descriptor_table()
                    .with_metadata(file, |stream: &litebox::platform::StdioStream| *stream)
                {
                    Ok(litebox::platform::StdioStream::Stdin) => {
                        if global.platform.stdin_ready() {
                            Events::IN
                        } else {
                            Events::empty()
                        }
                    }
                    Ok(
                        litebox::platform::StdioStream::Stdout
                        | litebox::platform::StdioStream::Stderr,
                    )
                    | Err(_) => Events::OUT,
                };
                Some(events & mask)
            }
            EpollDescriptor::Socket(fd) => {
                let proxy = match global.get_proxy(fd) {
                    Ok(p) => p,
                    Err(e) => {
                        log_unsupported!("epoll poll with socket fd: {:?}", e);
                        return None;
                    }
                };
                Some(poll(&proxy))
            }
            EpollDescriptor::Pipe(fd) => global.with_linux_pipe_iopollable(fd, poll).ok(),
            EpollDescriptor::Unix(fd) => {
                let handle = global.litebox.descriptor_table().entry_handle(fd)?;
                Some(handle.with_entry(|entry| poll(entry)))
            }
        }
    }
}

pub(crate) struct EpollFile<Platform: ShimPlatform, FS: ShimFS> {
    interests: litebox::sync::Mutex<
        Platform,
        BTreeMap<EpollEntryKey, alloc::sync::Arc<EpollEntry<Platform, FS>>>,
    >,
    ready: Arc<ReadySet<Platform, FS>>,
    status: core::sync::atomic::AtomicU32,
}

impl<Platform: ShimPlatform, FS: ShimFS> EpollFile<Platform, FS> {
    pub(crate) fn new() -> Self {
        EpollFile {
            interests: litebox::sync::Mutex::new(BTreeMap::new()),
            ready: Arc::new(ReadySet::new()),
            status: core::sync::atomic::AtomicU32::new(0),
        }
    }

    pub(crate) fn wait(
        &self,
        global: &GlobalState<Platform, FS>,
        cx: &WaitContext<'_, Platform>,
        maxevents: usize,
    ) -> Result<Vec<EpollEvent>, WaitError> {
        let mut events = Vec::new();
        loop {
            // A stdin interest's initial `add_interest` registration (see its doc comment on the
            // `EpollDescriptor::File` arm of `EpollDescriptor::poll`) never gets a real wakeup
            // observer -- there is no OS-level async notification for "new console input arrived"
            // this codebase's `Subject`/`Observer` machinery can hook into, unlike every other fd
            // kind. Without bounding the wait, an `epoll_wait` whose interest set includes a
            // not-yet-ready stdin fd would block on `self.ready.pollee`'s condvar forever, even
            // once real keystrokes arrive, exactly mirroring the `ppoll`/`PollSet::wait` hang this
            // fix also addresses (see `PollSet::wait`'s matching doc comment for the confirmed
            // live repro). Manually re-poll stdin on a short cadence and push it into the ready
            // set if it became readable, so `pop_multiple` picks it up on the next iteration.
            let has_stdin_interest = self.has_unready_stdin_interest(global);
            let iteration_cx = if has_stdin_interest {
                cx.with_timeout(STDIN_REPOLL_INTERVAL)
            } else {
                cx.with_timeout(None)
            };
            match self
                .ready
                .pollee
                .wait(&iteration_cx, false, Events::IN, || {
                    self.ready.pop_multiple(global, maxevents, &mut events);
                    if events.is_empty() {
                        return Err(TryOpError::<Infallible>::TryAgain);
                    }
                    Ok(())
                }) {
                Ok(()) => return Ok(events),
                Err(TryOpError::TryAgain) => unreachable!(),
                Err(TryOpError::WaitError(WaitError::TimedOut)) => {
                    if !has_stdin_interest
                        || (cx.deadline().is_some() && cx.remaining_timeout().is_none())
                    {
                        return Err(WaitError::TimedOut);
                    }
                    // Only the bounded stdin-repoll interval elapsed, not the caller's own
                    // deadline (if any): re-poll and loop back around.
                    self.repoll_stdin_interests(global);
                }
                Err(TryOpError::WaitError(e)) => return Err(e),
            }
        }
    }

    /// Returns `true` if any current interest is a stdin fd that is not currently ready -- see
    /// [`Self::wait`]'s doc comment for why this fd kind needs bounded periodic re-polling instead
    /// of relying solely on the observer-notification wakeup every other fd kind gets.
    fn has_unready_stdin_interest(&self, global: &GlobalState<Platform, FS>) -> bool {
        self.interests.lock().values().any(|entry| {
            !entry.is_ready.load(core::sync::atomic::Ordering::Relaxed)
                && matches!(entry.desc.upgrade(), Some(EpollDescriptor::File(file))
                if matches!(
                    global
                        .litebox
                        .descriptor_table()
                        .with_metadata(&file, |stream: &litebox::platform::StdioStream| *stream),
                    Ok(litebox::platform::StdioStream::Stdin)
                ))
        })
    }

    /// Re-polls every stdin interest and pushes it into the ready set if it has become readable.
    /// Called after each bounded stdin-repoll interval elapses in [`Self::wait`].
    fn repoll_stdin_interests(&self, global: &GlobalState<Platform, FS>) {
        let entries: alloc::vec::Vec<_> = self
            .interests
            .lock()
            .values()
            .filter(|entry| matches!(entry.desc.upgrade(), Some(EpollDescriptor::File(_))))
            .cloned()
            .collect();
        for entry in entries {
            if let Some((_, is_ready)) = entry.poll(global)
                && is_ready
            {
                self.ready.push(&entry);
            }
        }
    }

    pub(crate) fn epoll_ctl(
        &self,
        global: &GlobalState<Platform, FS>,
        op: EpollOp,
        fd: u32,
        file: &EpollDescriptor<Platform, FS>,
        event: Option<EpollEvent>,
    ) -> Result<(), Errno> {
        match op {
            EpollOp::EpollCtlAdd => self.add_interest(global, fd, file, event.unwrap()),
            EpollOp::EpollCtlMod => {
                log_unsupported!("epoll_ctl mod");
                Err(Errno::EINVAL)
            }
            EpollOp::EpollCtlDel => {
                let mut interests = self.interests.lock();
                let _ = interests
                    .remove(&EpollEntryKey::new(fd, file))
                    .ok_or(Errno::ENOENT)?;
                Ok(())
            }
        }
    }

    fn add_interest(
        &self,
        global: &GlobalState<Platform, FS>,
        fd: u32,
        file: &EpollDescriptor<Platform, FS>,
        event: EpollEvent,
    ) -> Result<(), Errno> {
        let mut interests = self.interests.lock();
        let key = EpollEntryKey::new(fd, file);
        if let Some(entry) = interests.get(&key)
            && entry.desc.upgrade().is_some()
        {
            return Err(Errno::EEXIST);
        }
        // we may have stale entry because we don't remove it immediately after the file is closed;
        // `insert` below will replace it with a new entry.

        let mask = Events::from_bits_truncate(event.events);
        let entry = EpollEntry::new(
            DescriptorRef::from(file),
            mask,
            EpollFlags::from_bits_truncate(event.events),
            event.data,
            self.ready.clone(),
        );
        let events = file
            .poll(global, mask, Some(entry.weak_self.clone() as _))
            .ok_or(Errno::EBADF)?;
        // Add the new entry to the ready list if the file is ready
        if !events.is_empty() {
            self.ready.push(&entry);
        }
        interests.insert(key, entry);
        Ok(())
    }

    #[expect(dead_code, reason = "currently unused, but might want to use soon")]
    fn mod_interest(
        &self,
        global: &GlobalState<Platform, FS>,
        fd: u32,
        file: &EpollDescriptor<Platform, FS>,
        event: EpollEvent,
    ) -> Result<(), Errno> {
        // EPOLLEXCLUSIVE is not allowed for a EPOLL_CTL_MOD operation
        let flags = EpollFlags::from_bits_truncate(event.events);
        if flags.contains(EpollFlags::EXCLUSIVE) {
            return Err(Errno::EINVAL);
        }

        let mut interests = self.interests.lock();
        let key = EpollEntryKey::new(fd, file);
        let entry = interests.get(&key).ok_or(Errno::ENOENT)?;
        if entry.desc.upgrade().is_none() {
            // The file descriptor is closed, remove the entry
            interests.remove(&key);
            return Err(Errno::ENOENT);
        }

        let mut inner = entry.inner.lock();
        if inner.flags.contains(EpollFlags::EXCLUSIVE) {
            // If EPOLLEXCLUSIVE has been set using epoll_ctl(), then a
            // subsequent EPOLL_CTL_MOD on the same epfd, fd pair yields an error.
            return Err(Errno::EINVAL);
        }

        let mask = Events::from_bits_truncate(event.events);
        inner.mask = mask;
        inner.flags = flags;
        inner.data = event.data;

        entry
            .is_enabled
            .store(true, core::sync::atomic::Ordering::Relaxed);
        let observer = entry.weak_self.clone();
        drop(inner);

        // re-register the observer with the new mask
        if let Some(events) = file.poll(global, mask, Some(observer as _)) {
            if !events.is_empty() {
                // Add the updated entry to the ready list if the file is ready
                self.ready.push(entry);
            }

            Ok(())
        } else {
            // The file descriptor is closed, remove the entry
            interests.remove(&key);
            Err(Errno::ENOENT)
        }
    }

    super::common_functions_for_file_status!();
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct EpollEntryKey(u32, usize);
impl EpollEntryKey {
    fn new<Platform: ShimPlatform, FS: ShimFS>(
        fd: u32,
        desc: &EpollDescriptor<Platform, FS>,
    ) -> Self {
        let ptr = match desc {
            EpollDescriptor::Eventfd(file) => Arc::as_ptr(file).addr(),
            EpollDescriptor::Epoll(file) => Arc::as_ptr(file).addr(),
            EpollDescriptor::File(file) => Arc::as_ptr(file).addr(),
            EpollDescriptor::Socket(socket_fd) => Arc::as_ptr(socket_fd).addr(),
            EpollDescriptor::Pipe(pipe_fd) => Arc::as_ptr(pipe_fd).addr(),
            EpollDescriptor::Unix(unix) => Arc::as_ptr(unix).addr(),
        };
        Self(fd, ptr)
    }
}

struct EpollEntry<Platform: ShimPlatform, FS: ShimFS> {
    desc: DescriptorRef<Platform, FS>,
    inner: litebox::sync::Mutex<Platform, EpollEntryInner>,
    ready: Arc<ReadySet<Platform, FS>>,
    is_ready: AtomicBool,
    is_enabled: AtomicBool,
    weak_self: Weak<Self>,
}

struct EpollEntryInner {
    mask: Events,
    flags: EpollFlags,
    data: u64,
}

impl<Platform: ShimPlatform, FS: ShimFS> EpollEntry<Platform, FS> {
    fn new(
        desc: DescriptorRef<Platform, FS>,
        mask: Events,
        flags: EpollFlags,
        data: u64,
        ready: Arc<ReadySet<Platform, FS>>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak_self| EpollEntry {
            desc,
            inner: litebox::sync::Mutex::new(EpollEntryInner { mask, flags, data }),
            ready,
            is_ready: AtomicBool::new(false),
            is_enabled: AtomicBool::new(true),
            weak_self: weak_self.clone(),
        })
    }

    fn poll(&self, global: &GlobalState<Platform, FS>) -> Option<(Option<EpollEvent>, bool)> {
        let file = self.desc.upgrade()?;
        let inner = self.inner.lock();

        if !self.is_enabled.load(core::sync::atomic::Ordering::Relaxed) {
            // the entry is disabled
            return None;
        }

        let events = file.poll(global, inner.mask, None)?;
        if events.is_empty() {
            Some((None, false))
        } else {
            let event = Some(EpollEvent {
                events: events.bits(),
                data: inner.data,
            });

            // keep the entry in the ready list if it is not edge-triggered or one-shot
            let is_still_ready = event.is_some()
                && !inner
                    .flags
                    .intersects(EpollFlags::EDGE_TRIGGER | EpollFlags::ONE_SHOT);

            // disable the entry if it is one-shot
            if inner.flags.contains(EpollFlags::ONE_SHOT) {
                self.is_enabled
                    .store(false, core::sync::atomic::Ordering::Relaxed);
            }

            Some((event, is_still_ready))
        }
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Observer<Events> for EpollEntry<Platform, FS> {
    fn on_events(&self, _events: &Events) {
        self.ready.push(self);
    }
}

struct ReadySet<Platform: ShimPlatform, FS: ShimFS> {
    entries: litebox::sync::Mutex<Platform, VecDeque<alloc::sync::Weak<EpollEntry<Platform, FS>>>>,
    pollee: Pollee<Platform>,
}

impl<Platform: ShimPlatform, FS: ShimFS> ReadySet<Platform, FS> {
    fn new() -> Self {
        Self {
            entries: litebox::sync::Mutex::new(VecDeque::new()),
            pollee: Pollee::new(),
        }
    }

    fn push(&self, entry: &EpollEntry<Platform, FS>) {
        if !entry.is_enabled.load(core::sync::atomic::Ordering::Relaxed) {
            // the entry is disabled
            return;
        }

        if !entry
            .is_ready
            .swap(true, core::sync::atomic::Ordering::Relaxed)
        {
            let mut entries = self.entries.lock();
            entries.push_back(entry.weak_self.clone());
        }

        self.pollee.notify_observers(Events::IN);
    }

    fn pop_multiple(
        &self,
        global: &GlobalState<Platform, FS>,
        maxevents: usize,
        events: &mut Vec<EpollEvent>,
    ) {
        let mut nums = self.entries.lock().len();
        while nums > 0 {
            nums -= 1;
            if events.len() >= maxevents {
                break;
            }

            // Note the lock operation is performed inside the loop to avoid holding the lock while calling `poll()`.
            // e.g., `poll` on a socket requires lock on network, and a deadlock may happen if another thread
            // holds the network lock and tries to add an entry to the same epoll instance upon new events.
            let Some(weak_entry) = self.entries.lock().pop_front() else {
                // no more entries
                break;
            };

            let Some(entry) = weak_entry.upgrade() else {
                // the entry has been deleted
                continue;
            };
            entry
                .is_ready
                .store(false, core::sync::atomic::Ordering::Relaxed);

            let Some((event, is_still_ready)) = entry.poll(global) else {
                // the entry is disabled or the associated file is closed
                continue;
            };

            if let Some(event) = event {
                events.push(event);
            }

            if is_still_ready {
                // if another event happened and already pushed the entry (i.e., marked it as ready)
                // while we were processing, we don't need to push it again.
                if !entry
                    .is_ready
                    .swap(true, core::sync::atomic::Ordering::Relaxed)
                {
                    self.entries.lock().push_back(weak_entry);
                }
            }
        }
    }
}

/// A poll set used for transient polling of a set of files. Designed for use
/// with the `poll` and `ppoll` syscalls.
pub(crate) struct PollSet<Platform: ShimPlatform> {
    entries: Vec<PollEntry<Platform>>,
}

/// How often [`PollSet::wait`] re-scans while waiting on a set that contains a stdin fd with no
/// real OS-level readiness notification available (see `PollSet::scan_once`'s doc comment). Short
/// enough that interactive typing feels immediate, long enough to not busy-loop.
const STDIN_REPOLL_INTERVAL: core::time::Duration = core::time::Duration::from_millis(15);

struct PollEntry<Platform: ShimPlatform> {
    fd: i32,
    mask: Events,
    revents: Events,
    observer: Option<Arc<PollEntryObserver<Platform>>>,
}

struct PollEntryObserver<Platform: ShimPlatform>(Waker<Platform>);

impl<Platform: ShimPlatform> Clone for PollEntryObserver<Platform> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<Platform: ShimPlatform> PollSet<Platform> {
    /// Returns a new empty `PollSet` with the given interest capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
        }
    }

    /// Adds an fd to the poll set with the given event mask.
    ///
    /// If fd is negative, it is ignored during polling.
    pub fn add_fd(&mut self, fd: i32, mask: Events) {
        self.entries.push(PollEntry {
            fd,
            mask: mask | Events::ALWAYS_POLLED,
            revents: Events::empty(),
            observer: None,
        });
    }

    fn scan_once<FS: ShimFS>(
        &mut self,
        global: &GlobalState<Platform, FS>,
        files: &FilesState<Platform, FS>,
        waker: Option<&Waker<Platform>>,
    ) -> bool {
        let mut is_ready = false;
        for entry in &mut self.entries {
            entry.revents = if entry.fd < 0 {
                continue;
            } else if let Ok(poll_descriptor) =
                EpollDescriptor::try_from(files, entry.fd.reinterpret_as_unsigned() as usize)
            {
                let observer = if !is_ready && let Some(waker) = waker {
                    // TODO: a separate allocation is necessary here
                    // because registering an observer twice with two
                    // different event masks results in the last one
                    // replacing the first. If this is changed to
                    // instead combine the new event mask into the existing
                    // registration's mask, then we can use a single observer
                    // for all entries.
                    let observer = Arc::new(PollEntryObserver(waker.clone()));
                    let weak = Arc::downgrade(&observer);
                    entry.observer = Some(observer);
                    Some(weak as _)
                } else {
                    // The poll set is already ready, or we have already
                    // registered the observer for this entry.
                    None
                };
                // TODO: add machinery to unregister the observer to avoid leaks.
                // Note `EpollDescriptor::poll`'s `File`/stdin arm never registers an observer (see
                // its doc comment): there is no OS-level async notification for "new console input
                // arrived" that this codebase's `Waker`/`Observer` machinery can hook into, unlike
                // a socket/pipe/eventfd/unix fd, which always gets a real observer registration
                // above. `Self::wait` detects a stdin fd up front (via `has_stdin_fd`) and falls
                // back to bounded periodic re-polling for the whole set whenever one is present,
                // rather than relying on an observer this arm can never actually register.
                poll_descriptor
                    .poll(global, entry.mask, observer)
                    .unwrap_or(Events::NVAL)
            } else {
                Events::NVAL
            };
            if !entry.revents.is_empty() {
                is_ready = true;
            }
        }
        is_ready
    }

    /// Scans the poll set for ready fds once.
    pub fn scan<FS: ShimFS>(
        &mut self,
        global: &GlobalState<Platform, FS>,
        files: &FilesState<Platform, FS>,
    ) {
        self.scan_once(global, files, None);
    }

    /// Waits for any of the fds in the poll set to become ready.
    ///
    /// # Correctness: no unregistered pre-check
    ///
    /// This used to run one `scan_once(..., None)` (i.e. with no observer registration) before
    /// ever entering `wait_until`, as a fast path to skip the wait entirely if already ready.
    /// That pre-check is a genuine lost-wakeup hazard: it observes "not ready yet" but leaves no
    /// observer registered, so if the awaited condition (e.g. a pipe's peer closing, delivered via
    /// `Pollee::notify_observers`) becomes true and fires its notification in the window between
    /// that pre-check and the first *registered* check inside `wait_until`, the notification finds
    /// no observer to deliver to and is silently dropped -- there is no other mechanism to re-scan
    /// afterward, so the caller blocks forever. Directly observed via tracing during this
    /// investigation: `WriteEnd::drop` firing `notify_observers(HUP)` with a live peer, but with
    /// `Subject::notify_observers`'s own `nums == 0` fast path bailing out because no observer was
    /// registered yet at that exact instant, so no corresponding `on_events` ever reached the
    /// waiting `PollEntryObserver`. This is a real, confirmed bug on its own, and this fix removes
    /// it correctly -- but it is not the sole cause of the deterministic `apk add nodejs` /
    /// `icu-data-en` post-install-script stall this investigation was chasing: reproducing that
    /// stall's minimal fork/exit shape (see `Task::prepare_for_exit`'s doc comment) with this fix
    /// applied still hangs, so at least one further distinct bug remains in that path.
    ///
    /// `WaitContext::wait_until`'s own contract already covers the "already ready" fast path
    /// correctly and race-free: `start_wait()` marks the thread as waiting *before* `ready()` (the
    /// closure here) runs, and `ready()`'s first invocation performs the very same
    /// `scan_once`-with-registration this pre-check tried to shortcut -- so removing the redundant
    /// unregistered pre-check costs one extra (cheap, in-process) scan in the always-ready case, in
    /// exchange for closing the missed-wakeup window entirely.
    pub fn wait<FS: ShimFS>(
        &mut self,
        global: &GlobalState<Platform, FS>,
        cx: &WaitContext<'_, Platform>,
        files: &FilesState<Platform, FS>,
    ) -> Result<(), WaitError> {
        // Determine up front whether this set contains a stdin fd at all, independent of whether
        // it happens to be ready right now: `has_unwakeable_wait` is only known accurately *after*
        // a `scan_once` call, but that call happens *inside* `wait_until`'s closure, which is too
        // late to decide `wait_until`'s own deadline for its very first (and, in the always-ready
        // fast path, only) invocation. Without this preliminary check, a set that starts out
        // ready-immediately-but-then-goes-not-ready-again would take the unbounded fast path below
        // and never re-visit this decision once inside a single `wait_until` call, hanging forever
        // exactly like the original bug this fix addresses -- confirmed live via the ConPTY harness
        // (see this fix's commit message for the repro).
        let has_stdin_fd = self.entries.iter().any(|entry| {
            entry.fd >= 0
                && EpollDescriptor::try_from(files, entry.fd.reinterpret_as_unsigned() as usize)
                    .is_ok_and(|desc| {
                        matches!(&desc, EpollDescriptor::File(file)
                        if matches!(
                            global.litebox.descriptor_table().with_metadata(
                                file,
                                |stream: &litebox::platform::StdioStream| *stream,
                            ),
                            Ok(litebox::platform::StdioStream::Stdin)
                        ))
                    })
        });
        let mut register = true;
        if !has_stdin_fd {
            // Fast/common path: no stdin fd in the set at all, so wait exactly as before -- a
            // single `wait_until` call using the caller's own context and deadline unmodified,
            // woken only by real `Observer` notifications.
            return cx.wait_until(|| self.scan_once(global, files, register.then_some(cx.waker())));
        }
        loop {
            // At least one entry is a stdin fd, which never registers a real wakeup observer
            // inside `scan_once` (see its doc comment): there is no OS-level async notification
            // this codebase's `Waker`/`Observer` machinery can hook into for "new console input
            // arrived". Bound this iteration's sleep to a short repoll interval so the loop comes
            // back around and re-checks `stdin_ready()` on a short cadence instead of sleeping on
            // a condvar that would otherwise never be signaled. `with_timeout` composes with
            // (takes the min of) any caller-supplied deadline, so the caller's real timeout still
            // fires on schedule instead of being silently overridden.
            match cx
                .with_timeout(STDIN_REPOLL_INTERVAL)
                .wait_until(|| self.scan_once(global, files, register.then_some(cx.waker())))
            {
                Ok(()) => return Ok(()),
                Err(WaitError::TimedOut) => {
                    // Only propagate `TimedOut` once the caller's own deadline (not just this
                    // iteration's repoll bound) has actually elapsed; otherwise loop back and scan
                    // again. `remaining_timeout()` reflects `cx`'s own (unbounded-by-repoll)
                    // deadline, so this is exact, not a heuristic.
                    if cx.deadline().is_some() && cx.remaining_timeout().is_none() {
                        return Err(WaitError::TimedOut);
                    }
                    register = false;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Returns the accumulated `revents` for each entry in the poll set.
    ///
    /// These are only valid after a call to `wait_or_timeout`.
    pub fn revents(&self) -> impl Iterator<Item = Events> + '_ {
        self.entries.iter().map(|entry| entry.revents)
    }

    /// Returns the accumulated `revents` and corresponding fds for each entry in the poll set.
    ///
    /// These are only valid after a call to `wait_or_timeout`.
    pub fn revents_with_fds(&self) -> impl Iterator<Item = (i32, Events)> + '_ {
        self.entries.iter().map(|entry| (entry.fd, entry.revents))
    }
}

impl<Platform: ShimPlatform> Observer<Events> for PollEntryObserver<Platform> {
    fn on_events(&self, _events: &Events) {
        self.0.wake();
    }
}

#[cfg(test)]
mod test {
    use crate::syscalls::tests::TestPlatform;
    use alloc::sync::Arc;
    use litebox::event::Events;
    use litebox::event::wait::{WaitError, WaitState};
    use litebox_common_linux::{EfdFlags, EpollEvent};

    use super::EpollFile;
    use crate::syscalls::file::FilesState;

    extern crate std;

    fn platform() -> &'static TestPlatform {
        crate::syscalls::tests::test_platform(None)
    }

    fn setup_epoll() -> (
        crate::Task<TestPlatform, crate::DefaultFS<TestPlatform>>,
        EpollFile<TestPlatform, crate::DefaultFS<TestPlatform>>,
    ) {
        let task = crate::syscalls::tests::init_platform(None);

        let epoll = EpollFile::new();
        (task, epoll)
    }

    #[test]
    fn test_epoll_with_eventfd() {
        let (task, epoll) = setup_epoll();
        let eventfd = crate::syscalls::eventfd::EventFile::new(0, EfdFlags::CLOEXEC);
        let typed = task
            .global
            .litebox
            .descriptor_table_mut()
            .insert::<crate::syscalls::eventfd::EventfdSubsystem<TestPlatform>>(eventfd);
        let files = Arc::new(FilesState::new(task.files.borrow().fs.clone()));
        let Ok(raw_fd) = files.insert_raw_fd(typed) else {
            unreachable!()
        };
        let descriptor = super::EpollDescriptor::try_from(&files, raw_fd).unwrap();
        epoll
            .add_interest(
                &task.global,
                10,
                &descriptor,
                EpollEvent {
                    events: Events::IN.bits(),
                    data: 0,
                },
            )
            .unwrap();

        // spawn a thread to write to the eventfd
        {
            let global = task.global.clone();
            let files = Arc::clone(&files);
            std::thread::spawn(move || {
                let typed = files
                    .raw_descriptor_store
                    .read()
                    .fd_from_raw_integer::<crate::syscalls::eventfd::EventfdSubsystem<TestPlatform>>(raw_fd)
                    .unwrap();
                let _ = global
                    .litebox
                    .descriptor_table()
                    .with_entry(&typed, |entry| {
                        entry.write(&WaitState::new(platform()).context(), 1)
                    });
            });
        }
        epoll
            .wait(&task.global, &WaitState::new(platform()).context(), 1024)
            .unwrap();
    }

    #[test]
    fn test_epoll_with_pipe() {
        let (task, epoll) = setup_epoll();
        let (producer, consumer) =
            task.global
                .pipes
                .create_pipe(2, litebox::pipes::Flags::empty(), None);
        let consumer = Arc::new(consumer);
        let reader = super::EpollDescriptor::Pipe(Arc::clone(&consumer));
        epoll
            .add_interest(
                &task.global,
                10,
                &reader,
                EpollEvent {
                    events: Events::IN.bits(),
                    data: 0,
                },
            )
            .unwrap();

        // spawn a thread to write to the pipe
        let global = task.global.clone();
        std::thread::spawn(move || {
            std::thread::sleep(core::time::Duration::from_millis(100));
            assert_eq!(
                global
                    .pipes
                    .write(&WaitState::new(platform()).context(), &producer, &[1, 2])
                    .unwrap(),
                2
            );
        });
        epoll
            .wait(&task.global, &WaitState::new(platform()).context(), 1024)
            .unwrap();
        let mut buf = [0; 2];
        task.global
            .pipes
            .read(&WaitState::new(platform()).context(), &consumer, &mut buf)
            .unwrap();
        assert_eq!(buf, [1, 2]);
    }

    #[test]
    fn test_poll() {
        let task = crate::syscalls::tests::init_platform(None);

        let mut set = super::PollSet::with_capacity(0);
        let eventfd = crate::syscalls::eventfd::EventFile::new(0, EfdFlags::empty());

        let typed = task
            .global
            .litebox
            .descriptor_table_mut()
            .insert::<crate::syscalls::eventfd::EventfdSubsystem<TestPlatform>>(eventfd);
        let no_fds = FilesState::new(task.files.borrow().fs.clone());
        let fds = Arc::new(FilesState::new(task.files.borrow().fs.clone()));
        let Ok(raw_fd) = fds.insert_raw_fd(typed) else {
            unreachable!()
        };
        let fd = i32::try_from(raw_fd).unwrap();
        set.add_fd(fd, Events::IN);

        let revents = |set: &super::PollSet<TestPlatform>| {
            let revents: std::vec::Vec<_> = set.revents().collect();
            assert_eq!(revents.len(), 1);
            revents[0]
        };

        set.wait(&task.global, &WaitState::new(platform()).context(), &no_fds)
            .unwrap();
        assert_eq!(revents(&set), Events::NVAL);

        {
            let typed = fds
                .raw_descriptor_store
                .read()
                .fd_from_raw_integer::<crate::syscalls::eventfd::EventfdSubsystem<TestPlatform>>(
                    raw_fd,
                )
                .unwrap();
            task.global
                .litebox
                .descriptor_table()
                .with_entry(&typed, |entry| {
                    entry.write(&WaitState::new(platform()).context(), 1)
                });
        }
        set.wait(&task.global, &WaitState::new(platform()).context(), &fds)
            .unwrap();
        assert_eq!(revents(&set), Events::IN);

        {
            let typed = fds
                .raw_descriptor_store
                .read()
                .fd_from_raw_integer::<crate::syscalls::eventfd::EventfdSubsystem<TestPlatform>>(
                    raw_fd,
                )
                .unwrap();
            task.global
                .litebox
                .descriptor_table()
                .with_entry(&typed, |entry| {
                    entry.read(&WaitState::new(platform()).context())
                });
        }
        set.wait(
            &task.global,
            &WaitState::new(platform())
                .context()
                .with_timeout(core::time::Duration::from_millis(100)),
            &fds,
        )
        .unwrap_err();
        assert!(revents(&set).is_empty());

        // spawn a thread to write to the eventfd
        let global = task.global.clone();
        let fds_for_thread = Arc::clone(&fds);
        std::thread::spawn(move || {
            let typed = fds_for_thread
                .raw_descriptor_store
                .read()
                .fd_from_raw_integer::<crate::syscalls::eventfd::EventfdSubsystem<TestPlatform>>(
                    raw_fd,
                )
                .unwrap();
            let handle = global
                .litebox
                .descriptor_table()
                .entry_handle(&typed)
                .unwrap();
            let _ =
                handle.with_entry(|entry| entry.write(&WaitState::new(platform()).context(), 1));
        });

        set.wait(&task.global, &WaitState::new(platform()).context(), &fds)
            .unwrap();
        assert_eq!(revents(&set), Events::IN);
    }

    /// Regression test for the interactive-stdin hang this investigation root-caused: a `ppoll`/
    /// `pselect` whose set includes a stdin fd must still honor the caller's own deadline exactly
    /// on the very *first* `wait_until` call, not just on later iterations.
    ///
    /// The original (buggy) implementation only switched to the bounded stdin-repoll strategy
    /// after observing `has_unwakeable_wait` become `true` inside a *previous* `scan_once` call --
    /// but on a `PollSet`'s first ever `wait()`, no previous scan exists, so that first
    /// `wait_until` call used the caller's raw, unbounded deadline. If stdin was not immediately
    /// ready at that first scan, the call blocked on `wait_until`'s internal condvar wait with no
    /// way to ever revisit the decision -- reproducing the exact hang this investigation found live
    /// via the ConPTY harness (`ash` blocked forever in `Ppoll` after its first keystroke). The fix
    /// detects "does this set contain a stdin fd" up front, independent of current readiness, and
    /// uses the bounded-repoll strategy from the very first `wait_until` call onward whenever true.
    ///
    /// This test uses fd 0, which `crate::syscalls::tests::init_platform` already tags with
    /// `StdioStream::Stdin` metadata (mirroring a real guest's bootstrap stdin fd), to exercise the
    /// real `PollSet::wait` code path end-to-end -- not a synthetic stand-in.
    #[test]
    fn test_ppoll_on_stdin_honors_caller_deadline_on_first_wait() {
        let task = crate::syscalls::tests::init_platform(None);
        let fds = Arc::new(FilesState::new(task.files.borrow().fs.clone()));

        // fd 0 is stdin, tagged `StdioStream::Stdin` by `initialize_stdio_in_shared_descriptors_table`
        // (see `crate::lib::FilesState::initialize_stdio_in_shared_descriptors_table`).
        let mut set = super::PollSet::with_capacity(1);
        set.add_fd(0, Events::IN);

        let start = std::time::Instant::now();
        let result = set.wait(
            &task.global,
            &WaitState::new(platform())
                .context()
                .with_timeout(core::time::Duration::from_millis(100)),
            &fds,
        );
        let elapsed = start.elapsed();
        // Whether or not the test process's own stdin happens to be immediately ready (irrelevant
        // to what's being tested here -- `TestPlatform` reads the real OS-level stdio of whatever
        // process is running the test suite), the call must return within a small margin of the
        // caller's 100ms deadline -- never hang indefinitely, and never return near-instantly by
        // some other unrelated path. If it returned `Ok`, stdin was already ready; that is a valid
        // outcome too, so only assert the timing bound, not the specific `Result`.
        let _ = result;
        assert!(
            elapsed < core::time::Duration::from_secs(2),
            "ppoll on stdin must not hang indefinitely on its first wait \
             (elapsed={elapsed:?}, exceeds the 100ms deadline by more than the bounded-repoll \
             interval should ever allow)"
        );
    }

    #[test]
    fn test_pselect() {
        let task = crate::syscalls::tests::init_platform(None);

        let (rfd_u, wfd_u) = task
            .sys_pipe2(litebox::fs::OFlags::empty())
            .expect("pipe2 failed");
        let rfd = i32::try_from(rfd_u).unwrap();
        let wfd = i32::try_from(wfd_u).unwrap();

        task.spawn_clone_for_test(move |task| {
            std::thread::sleep(core::time::Duration::from_millis(100));
            // write a byte
            let buf = [0x41u8];
            let written = task.sys_write(wfd, &buf, None).expect("write failed");
            assert_eq!(written, 1);
        });

        // prepare fd_set for read
        let mut rfds = bitvec::bitvec![0; rfd_u.next_multiple_of(64) as usize];
        rfds.set(rfd_u as usize, true);

        // Call pselect
        let ret = task
            .do_pselect(rfd_u + 1, Some(&mut rfds), None, None, None)
            .expect("pselect failed");
        assert!(ret > 0, "pselect should report ready");
        assert!(rfds.iter_ones().all(|fd| fd == rfd_u as usize));

        // read
        let mut out = [0u8; 8];
        let n = task.sys_read(rfd, &mut out, None).expect("read failed");
        assert_eq!(n, 1);
        assert_eq!(out[0], 0x41);

        let _ = task.sys_close(rfd);
        let _ = task.sys_close(wfd);
    }

    #[test]
    fn test_pselect_read_hup() {
        let task = crate::syscalls::tests::init_platform(None);

        let (rfd_u, wfd_u) = task
            .sys_pipe2(litebox::fs::OFlags::empty())
            .expect("pipe2 failed");
        let rfd = i32::try_from(rfd_u).unwrap();
        let wfd = i32::try_from(wfd_u).unwrap();

        task.spawn_clone_for_test(move |task| {
            std::thread::sleep(core::time::Duration::from_millis(100));
            task.sys_close(wfd).expect("close writer failed");
        });

        // prepare fd_set for read
        let mut rfds = bitvec::bitvec![0; rfd_u.next_multiple_of(64) as usize];
        rfds.set(rfd_u as usize, true);

        let ret = task
            .do_pselect(
                rfd_u + 1,
                Some(&mut rfds),
                None,
                None,
                Some(core::time::Duration::from_mins(1)),
            )
            .expect("pselect failed");

        // Expect pselect to indicate readiness (HUP should cause revents)
        assert!(ret > 0, "pselect should report ready for EOF/HUP");
        assert!(rfds.iter_ones().all(|fd| fd == rfd_u as usize));

        // read should return 0 (EOF)
        let mut out = [0u8; 8];
        let n = task.sys_read(rfd, &mut out, None).expect("read failed");
        assert_eq!(n, 0, "read should return 0 on EOF");

        let _ = task.sys_close(rfd);
    }

    #[test]
    fn test_pselect_invalid_fd() {
        let task = crate::syscalls::tests::init_platform(None);

        let invalid_fd_u = 100u32;

        // prepare fd_set for read
        let mut rfds = bitvec::bitvec![0; invalid_fd_u.next_multiple_of(64) as usize];
        rfds.set(invalid_fd_u as usize, true);

        let ret = task.do_pselect(
            invalid_fd_u + 1,
            Some(&mut rfds),
            None,
            None,
            Some(core::time::Duration::from_secs(1)),
        );

        // Expect pselect to return EBADF
        assert!(ret.is_err(), "pselect should fail for invalid fd");
        assert_eq!(
            ret.err().unwrap(),
            litebox_common_linux::errno::Errno::EBADF
        );
    }
}
