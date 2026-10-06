// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Unix domain socket implementation for the Linux shim layer.

use core::{
    sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
    time::Duration,
};

use alloc::{
    collections::{btree_map::BTreeMap, vec_deque::VecDeque},
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use litebox::{
    event::{
        Events, IOPollable,
        polling::{Pollee, TryOpError},
        wait::WaitContext,
    },
    fd::{FdEnabledSubsystem, FdEnabledSubsystemEntry},
    fs::{Mode, OFlags, errors::OpenError},
    platform::SharedKernelStateProvider as _,
    sync::{Mutex, RwLock},
    utils::TruncateExt as _,
};
use litebox_common_linux::{
    AddressFamily, IpOption, ReceiveFlags, SendFlags, ShutdownHow, SockFlags, SockType,
    SocketOption, SocketOptionName, Ucred, errno::Errno,
};

use crate::{
    FileFd, GlobalStateHandle, ShimFS, ShimPlatform, Task, UserPtr, UserPtrMut,
    channel::{Channel, ReadEnd, WriteEnd},
    syscalls::net::{SocketOptionValue, SocketOptions},
};

pub(crate) struct UnixSocketSubsystem<Platform: ShimPlatform, FS: ShimFS>(
    core::marker::PhantomData<(Platform, FS)>,
);
impl<Platform: ShimPlatform, FS: ShimFS> FdEnabledSubsystem for UnixSocketSubsystem<Platform, FS> {
    type Entry = UnixSocket<Platform, FS>;
}

impl<Platform: ShimPlatform, FS: ShimFS> FdEnabledSubsystemEntry for UnixSocket<Platform, FS> {}

/// C-compatible structure for Unix socket addresses.
const UNIX_PATH_MAX: usize = 108;
#[repr(C)]
pub(super) struct CSockUnixAddr {
    /// Address family (AF_UNIX)
    pub(super) family: i16,
    /// Socket path or abstract address
    pub(super) path: [u8; UNIX_PATH_MAX],
}

/// Represents a Unix socket address.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UnixSocketAddr {
    /// Unnamed socket (not bound to any address)
    Unnamed,
    /// Filesystem path-based socket
    Path(String),
    /// Abstract namespace socket (not backed by filesystem)
    Abstract(Vec<u8>),
}

/// A bound Unix socket address with associated resources.
///
/// For path-based sockets, this includes a file descriptor to ensure
/// the socket file remains accessible. The file is automatically closed
/// when this structure is dropped.
enum UnixBoundSocketAddr<FS: ShimFS> {
    Path((String, FileFd<FS>, Arc<FS>)),
    /// A path a cross-process fork child's carried listener names but could not open (its
    /// socket file belongs to the parent, which keeps it open).
    UnopenedPath(String),
    Abstract(Vec<u8>),
}

/// Key type for indexing Unix socket addresses in the global address table.
///
/// This is used internally to track which addresses are currently bound
/// by listening sockets.
#[derive(PartialEq, Eq, Hash, Debug, Ord, PartialOrd)]
pub(crate) enum UnixSocketAddrKey {
    // TODO: add inode reference once the file system supports it.
    Path(String),
    Abstract(Vec<u8>),
}

impl UnixSocketAddr {
    /// Returns true if this is an unnamed socket address.
    fn is_unnamed(&self) -> bool {
        matches!(self, UnixSocketAddr::Unnamed)
    }

    /// Binds this address to the filesystem or abstract namespace.
    ///
    /// # Arguments
    ///
    /// * `task` - The current task context
    /// * `is_server` - Whether this is a server socket (creates the file if true)
    ///
    /// # Errors
    ///
    /// Returns an error if the address cannot be bound (e.g., file doesn't exist,
    /// permission denied).
    fn bind<Platform: ShimPlatform, FS: ShimFS>(
        self,
        task: &Task<Platform, FS>,
        is_server: bool,
    ) -> Result<UnixBoundSocketAddr<FS>, Errno> {
        match self {
            UnixSocketAddr::Path(path) => {
                let flags = if is_server {
                    // create the socket file if not exists;
                    // use O_EXCL to ensure exclusive creation
                    OFlags::CREAT | OFlags::EXCL | OFlags::RDWR
                } else {
                    OFlags::RDWR
                };
                // TODO: extend fs to support creating sock file (i.e., with type `InodeType::Socket`)
                let file = task
                    .files
                    .borrow()
                    .fs
                    .open(
                        path.as_str(),
                        flags,
                        Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
                    )
                    .map_err(|err| match err {
                        OpenError::AlreadyExists => Errno::EADDRINUSE,
                        other => Errno::from(other),
                    })?;
                Ok(UnixBoundSocketAddr::Path((
                    path,
                    file,
                    task.files.borrow().fs.clone(),
                )))
            }
            UnixSocketAddr::Abstract(data) => {
                // TODO: check if the abstract address is already in use
                Ok(UnixBoundSocketAddr::Abstract(data))
            }
            UnixSocketAddr::Unnamed => {
                // "Autobind": `bind()` called with no address at all (`addrlen ==
                // sizeof(sa_family_t)`) asks the kernel to assign an abstract-namespace address
                // automatically -- used by some IPC libraries to get a peer-identifiable address
                // before `connect()`ing out, without caring what the address actually is. Real
                // Linux's format (see `unix(7)`) is a leading NUL byte followed by 5 lowercase
                // hex digits; this used to unconditionally panic (`todo!()`) instead.
                let id = task
                    .global
                    .next_unix_autobind_id
                    .fetch_add(1, Ordering::Relaxed)
                    & 0xFFFFF;
                let mut addr = alloc::vec![0u8];
                addr.extend_from_slice(alloc::format!("{id:05x}").as_bytes());
                Ok(UnixBoundSocketAddr::Abstract(addr))
            }
        }
    }

    /// Converts this address to a key for the global address table.
    ///
    /// Returns `None` for unnamed addresses, which cannot be looked up.
    fn to_key(&self) -> Option<UnixSocketAddrKey> {
        match self {
            Self::Unnamed => None,
            Self::Path(path) => Some(UnixSocketAddrKey::Path(path.clone())),
            Self::Abstract(addr) => Some(UnixSocketAddrKey::Abstract(addr.clone())),
        }
    }
}

impl<FS: ShimFS> UnixBoundSocketAddr<FS> {
    /// Converts this bound address to a key for the global address table.
    fn to_key(&self) -> UnixSocketAddrKey {
        match self {
            Self::Path((path, ..)) | Self::UnopenedPath(path) => {
                UnixSocketAddrKey::Path(path.clone())
            }
            Self::Abstract(addr) => UnixSocketAddrKey::Abstract(addr.clone()),
        }
    }
}

impl<FS: ShimFS> Drop for UnixBoundSocketAddr<FS> {
    fn drop(&mut self) {
        match self {
            Self::Path((_, file, fs)) => {
                let _ = fs.close(file);
            }
            Self::UnopenedPath(_) | Self::Abstract(_) => {}
        }
    }
}

impl<FS: ShimFS> From<&UnixBoundSocketAddr<FS>> for UnixSocketAddr {
    fn from(addr: &UnixBoundSocketAddr<FS>) -> Self {
        match addr {
            UnixBoundSocketAddr::Path((path, ..)) | UnixBoundSocketAddr::UnopenedPath(path) => {
                UnixSocketAddr::Path(path.clone())
            }
            UnixBoundSocketAddr::Abstract(data) => UnixSocketAddr::Abstract(data.clone()),
        }
    }
}

/// Represents a Unix stream socket in its initial state.
///
/// This is the state immediately after socket creation, before the socket
/// has been connected, or put into listening mode.
struct UnixInitStream<Platform: ShimPlatform, FS: ShimFS> {
    /// Optional bound address for this socket
    addr: Option<UnixBoundSocketAddr<FS>>,
    pollee: Pollee<Platform>,
    read_shutdown: AtomicBool,
    write_shutdown: AtomicBool,
}

impl<Platform: ShimPlatform, FS: ShimFS> UnixInitStream<Platform, FS> {
    fn new() -> Self {
        Self {
            addr: None,
            pollee: Pollee::new(),
            read_shutdown: AtomicBool::new(false),
            write_shutdown: AtomicBool::new(false),
        }
    }

    fn shutdown(&self, how: ShutdownHow) {
        if how.is_shutdown_read() && !self.read_shutdown.swap(true, Ordering::Release) {
            self.pollee.notify_observers(Events::IN);
        }
        if how.is_shutdown_write() {
            self.write_shutdown.store(true, Ordering::Release);
        }
    }

    /// Binds this socket to the given address.
    fn bind(&mut self, task: &Task<Platform, FS>, addr: UnixSocketAddr) -> Result<(), Errno> {
        if self.addr.is_some() && !addr.is_unnamed() {
            return Err(Errno::EINVAL);
        }
        if self.addr.is_none() {
            let bound_addr = addr.bind(task, true)?;
            self.addr = Some(bound_addr);
        }
        Ok(())
    }

    /// Transitions this socket to listening state.
    ///
    /// # Arguments
    ///
    /// * `backlog` - Maximum number of pending connections to queue
    fn listen(
        self,
        task: &Task<Platform, FS>,
        backlog: u16,
        global: &GlobalStateHandle<Platform, FS>,
    ) -> Result<UnixListenStream<Platform, FS>, (Self, Errno)> {
        let Some(addr) = self.addr else {
            return Err((self, Errno::EINVAL));
        };
        let key = addr.to_key();
        let cred = task.peer_cred();
        let backlog = Arc::new(Backlog::new(addr, backlog, self.pollee, cred));
        let owner_pid = task.pid.get() as u32;
        let (presence_kind, presence_bytes) = presence_kind_and_bytes(&key);
        litebox_util_log::debug!(
            owner_pid:% = owner_pid,
            kind:% = presence_kind,
            key_len:% = presence_bytes.len(),
            key_bytes:? = presence_bytes;
            "DIAG Backlog::listen: registering presence"
        );
        global
            .unix_addr_presence
            .insert(presence_kind, presence_bytes, owner_pid);
        global
            .unix_addr_table
            .write()
            .insert(key, UnixEntry(UnixEntryInner::Stream(backlog.clone())));
        Ok(UnixListenStream {
            backlog,
            global: global.clone(),
            owner_pid,
            carried: false,
        })
    }

    /// Converts this initial socket into a connected stream pair.
    ///
    /// `client_cred` is the real, live credentials of the connecting task; `server_cred`
    /// is the credentials the listening socket's owner captured at `listen(2)` time. Each
    /// returned stream stores the *other* side's credentials as its `SO_PEERCRED` value.
    fn into_connected(
        self,
        peer_addr: Arc<UnixBoundSocketAddr<FS>>,
        client_cred: Ucred,
        server_cred: Ucred,
    ) -> (
        UnixConnectedStream<Platform, FS>,
        UnixConnectedStream<Platform, FS>,
    ) {
        let UnixInitStream {
            addr,
            pollee,
            read_shutdown,
            write_shutdown,
        } = self;
        UnixConnectedStream::new_pair(
            addr.map(Arc::new),
            Some(Arc::new(pollee)),
            Some(peer_addr),
            read_shutdown.load(Ordering::Acquire),
            write_shutdown.load(Ordering::Acquire),
            client_cred,
            server_cred,
        )
    }
}

/// A non-blocking cross-process `connect()` that returned `EINPROGRESS`: the request stays
/// posted in `global.unix_shared_connect_queue`, and this socket's own `poll`/`epoll_wait` path
/// (via `UnixStream::check_io_events`) re-checks `poll_result(request_idx)` on every call --
/// exactly the polling shape `wait_on_events_polling`'s bounded-repoll callers already use
/// elsewhere in this codebase (`Backlog::check_io_events`'s own `has_pending` half of this same
/// rendezvous) -- until the listener's `accept()` claims and completes it.
///
/// FIXES the bug `connect_cross_process`'s own `TryOpError::TryAgain` arm used to describe as a
/// precisely-scoped, not-yet-fixed follow-up (`docs/AGENTS_ARCHIVE_2026-09-22.md`): that arm used
/// to unconditionally `cancel()` the just-posted request on every non-blocking miss, so a caller
/// that correctly polls for writability after `EINPROGRESS` (every real AF_UNIX client library,
/// GLib/GIO's `GSocketClient` among them) could poll forever -- the request it was waiting on had
/// already been withdrawn moments after being posted, so the listener's `accept()` could never
/// claim it. Live-caught as the reason a real `xfce4-session` boot never reaches
/// `_NET_SUPPORTING_WM_CHECK`: its own D-Bus connect (`self_pid` matches its own guest pid in a
/// `LITEBOX_PROCESS_FORK=1` boot's `unix.rs` debug trace) hits exactly this arm and then never
/// forks a single child process again -- consistent with GDBus/GIO's real connect-then-poll-for-
/// writable design blocking forever on a connection this shim had already thrown away.
struct UnixConnectingStream<Platform: ShimPlatform, FS: ShimFS> {
    /// This client's own request index into `global.unix_shared_connect_queue`, from `post()`.
    request_idx: usize,
    /// The address this connect was aimed at -- becomes the completed stream's peer address,
    /// exactly as `connect_cross_process`'s own synchronous-completion path already builds it.
    peer_addr: UnixSocketAddr,
    global: GlobalStateHandle<Platform, FS>,
    /// Registered observers wait on this like any other not-yet-ready fd; nothing here ever
    /// wakes them directly (no genuine cross-process wakeup exists -- see this module's own
    /// "Shared cross-process AF_UNIX connection data plane" doc comment), so completion is only
    /// ever discovered by a fresh `check_io_events` call, matching every other shared-queue
    /// consumer in this file.
    pollee: Pollee<Platform>,
}

impl<Platform: ShimPlatform, FS: ShimFS> UnixConnectingStream<Platform, FS> {
    /// Non-blocking, non-consuming-on-miss: `Some(connected)` once the listener's `accept()` has
    /// claimed and completed this request, `None` while still pending. Mirrors
    /// `connect_cross_process`'s own synchronous-completion construction exactly (`is_client:
    /// true`, `UnixSocketAddr::Unnamed` local address, `server_cred` read back from the slot the
    /// listener allocated).
    fn try_complete(&self) -> Option<UnixConnectedStream<Platform, FS>> {
        let slot = self
            .global
            .unix_shared_connect_queue
            .poll_result(self.request_idx)?;
        Some(UnixConnectedStream::new_shared(
            self.global.clone(),
            slot,
            true,
            UnixSocketAddr::Unnamed,
            self.peer_addr.clone(),
            self.global.unix_shared_conn_table.get(slot).server_cred(),
        ))
    }
}

/// Connection backlog for a listening Unix socket.
///
/// Manages the queue of pending connections and the maximum backlog limit.
struct Backlog<Platform: ShimPlatform, FS: ShimFS> {
    /// The address this socket is listening on
    addr: Arc<UnixBoundSocketAddr<FS>>,
    state: Mutex<Platform, BacklogState<Platform, FS>>,
    pollee: Pollee<Platform>,
    /// Real credentials of the task that called `listen(2)` on this socket, captured at
    /// that time -- reported to connecting clients as their `SO_PEERCRED` peer identity.
    listener_cred: Ucred,
}

struct BacklogState<Platform: ShimPlatform, FS: ShimFS> {
    sockets: VecDeque<UnixConnectedStream<Platform, FS>>,
    /// Maximum number of pending connections
    limit: u16,
    is_shutdown: bool,
}

impl<Platform: ShimPlatform, FS: ShimFS> Backlog<Platform, FS> {
    fn new(
        addr: UnixBoundSocketAddr<FS>,
        backlog: u16,
        pollee: Pollee<Platform>,
        listener_cred: Ucred,
    ) -> Self {
        Self {
            addr: Arc::new(addr),
            state: litebox::sync::Mutex::new(BacklogState {
                sockets: VecDeque::new(),
                limit: backlog,
                is_shutdown: false,
            }),
            pollee,
            listener_cred,
        }
    }

    /// Updates the maximum backlog size.
    fn set_backlog(&self, backlog: u16) {
        self.state.lock().limit = backlog;
    }

    /// Attempts to establish a connection without blocking.
    fn try_connect(
        &self,
        init: UnixInitStream<Platform, FS>,
        client_cred: Ucred,
    ) -> Result<UnixConnectedStream<Platform, FS>, (UnixInitStream<Platform, FS>, Errno)> {
        let mut state = self.state.lock();
        if state.is_shutdown {
            return Err((init, Errno::ECONNREFUSED));
        }

        if state.sockets.len() >= state.limit as usize {
            return Err((init, Errno::EAGAIN));
        }

        let (client, server) =
            init.into_connected(self.addr.clone(), client_cred, self.listener_cred);
        state.sockets.push_back(server);

        self.pollee.notify_observers(Events::IN);
        Ok(client)
    }

    /// Attempts to accept a pending connection without blocking. Checks the private,
    /// same-process backlog first (unchanged fast path); if that's empty and not shut down, also
    /// checks `global.unix_shared_connect_queue` for a genuinely cross-process client's pending
    /// request naming this address -- see the "Shared cross-process AF_UNIX connection data
    /// plane" module doc comment (near [`SharedUnixConnectQueue`]) for the full rendezvous
    /// design this closes.
    fn try_accept(
        &self,
        global: &GlobalStateHandle<Platform, FS>,
    ) -> Result<UnixConnectedStream<Platform, FS>, TryOpError<Errno>> {
        {
            let mut state = self.state.lock();
            match state.sockets.pop_front() {
                Some(stream) => {
                    if !state.is_shutdown {
                        self.pollee.notify_observers(Events::OUT);
                    }
                    return Ok(stream);
                }
                None if state.is_shutdown => return Err(TryOpError::Other(Errno::ESHUTDOWN)),
                None => {}
            }
        }
        match self.try_accept_shared(global) {
            Some(stream) => Ok(stream),
            None => Err(TryOpError::TryAgain),
        }
    }

    /// The shared-queue half of [`Self::try_accept`] -- claims one pending cross-process connect
    /// request naming this listener's own address, if any, and completes it with a fresh
    /// [`SharedUnixConnTable`] slot.
    fn try_accept_shared(
        &self,
        global: &GlobalStateHandle<Platform, FS>,
    ) -> Option<UnixConnectedStream<Platform, FS>> {
        let key = self.addr.to_key();
        let (kind, key_bytes) = presence_kind_and_bytes(&key);
        let (request_idx, client_cred) = global
            .unix_shared_connect_queue
            .try_claim(kind, key_bytes)?;
        let Some(slot) = global.unix_shared_conn_table.alloc(
            global.litebox.platform(),
            &client_cred,
            &self.listener_cred,
        ) else {
            // Pool exhausted -- leave this request CLAIMED-but-never-completed; the client's own
            // bounded poll loop eventually times out and retries with a fresh `post()`. Bounded,
            // self-healing, never a panic.
            return None;
        };
        global.unix_shared_connect_queue.complete(request_idx, slot);
        let local_addr = UnixSocketAddr::from(self.addr.as_ref());
        let stream = UnixConnectedStream::new_shared(
            global.clone(),
            slot,
            false, // this is the accepting ("server") side of the slot
            local_addr,
            // The client's own bound address, if any, isn't carried through the shared queue in
            // this first cut (most AF_UNIX clients connect unbound) -- `getpeername()` on the
            // accepted side reports Unnamed rather than a real path/abstract address in that
            // case, a disclosed, non-panicking simplification.
            UnixSocketAddr::Unnamed,
            client_cred,
        );
        self.pollee.notify_observers(Events::IN);
        Some(stream)
    }

    /// `global` is used to also check `unix_shared_connect_queue` for a genuinely cross-process
    /// pending request naming this listener's own address -- see [`SharedUnixConnectQueue::
    /// has_pending`]'s doc comment for why this half is required at all (not just [`Self::
    /// try_accept`]'s own shared-queue check): a real event-driven listener (Xvfb, dbus-daemon)
    /// calls `poll`/`epoll_wait` to learn a connection is waiting BEFORE ever calling `accept()`.
    fn check_io_events(&self, global: &GlobalStateHandle<Platform, FS>) -> Events {
        let state = self.state.lock();
        let mut events = Events::empty();
        if !state.sockets.is_empty() {
            events |= Events::IN;
        } else if !state.is_shutdown {
            let key = self.addr.to_key();
            let (kind, key_bytes) = presence_kind_and_bytes(&key);
            if global
                .unix_shared_connect_queue
                .has_pending(kind, key_bytes)
            {
                events |= Events::IN;
            }
        }
        if state.is_shutdown {
            events |= Events::IN | Events::HUP;
        } else if state.sockets.len() < state.limit as usize {
            events |= Events::OUT;
        }
        events
    }

    /// Shuts down this backlog, preventing new connections.
    fn shutdown(&self) {
        let mut state = self.state.lock();
        if !state.is_shutdown {
            state.is_shutdown = true;
            self.pollee.notify_observers(Events::HUP);
        }
    }
}

/// Represents a Unix stream socket in listening state.
struct UnixListenStream<Platform: ShimPlatform, FS: ShimFS> {
    backlog: Arc<Backlog<Platform, FS>>,
    global: GlobalStateHandle<Platform, FS>,
    /// The guest pid that registered this address in `global.unix_addr_presence` -- carried so
    /// `Drop` can remove exactly that entry (see `SharedUnixAddrPresenceTable::remove`'s
    /// same-owner-only contract).
    owner_pid: u32,
    /// Rebuilt in a cross-process fork child: its presence entry is its own, but the address
    /// table entry (and the bound address itself) belong to the parent.
    carried: bool,
}

impl<Platform: ShimPlatform, FS: ShimFS> UnixListenStream<Platform, FS> {
    /// Updates the maximum backlog size for pending connections.
    fn listen(&self, backlog: u16) {
        self.backlog.set_backlog(backlog);
    }

    fn register_observer(
        &self,
        observer: Weak<dyn litebox::event::observer::Observer<litebox::event::Events>>,
        mask: litebox::event::Events,
    ) {
        self.backlog.pollee.register_observer(observer, mask);
    }

    /// Returns the local address this socket is bound to.
    fn get_local_addr(&self) -> &UnixBoundSocketAddr<FS> {
        self.backlog.addr.as_ref()
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Drop for UnixListenStream<Platform, FS> {
    fn drop(&mut self) {
        self.backlog.shutdown();

        let key = self.backlog.addr.to_key();
        let mut table = self.global.unix_addr_table.write();
        // Only remove the entry if it still points to our backlog
        let registered_here = matches!(
            table.get(&key),
            Some(UnixEntry(UnixEntryInner::Stream(backlog))) if Arc::ptr_eq(backlog, &self.backlog)
        );
        if registered_here {
            table.remove(&key);
        }
        if registered_here || self.carried {
            let (presence_kind, presence_bytes) = presence_kind_and_bytes(&key);
            self.global
                .unix_addr_presence
                .remove(presence_kind, presence_bytes, self.owner_pid);
        }
    }
}

/// Tracks the local and peer addresses for a connected socket.
struct AddrView<FS: ShimFS> {
    addr: Option<Arc<UnixBoundSocketAddr<FS>>>,
    peer: Option<Arc<UnixBoundSocketAddr<FS>>>,
}

impl<FS: ShimFS> AddrView<FS> {
    /// Creates a pair of address views for two connected sockets.
    ///
    /// The local address of one becomes the peer address of the other.
    fn new_pair(
        addr: Option<Arc<UnixBoundSocketAddr<FS>>>,
        peer: Option<Arc<UnixBoundSocketAddr<FS>>>,
    ) -> (Self, Self) {
        let first = Self {
            addr: addr.clone(),
            peer: peer.clone(),
        };
        let second = Self {
            addr: peer,
            peer: addr,
        };
        (first, second)
    }

    /// Returns the local address, if available.
    fn get_local_addr(&self) -> Option<&UnixBoundSocketAddr<FS>> {
        self.addr.as_deref()
    }

    /// Returns the peer address, if available.
    fn get_peer_addr(&self) -> Option<&UnixBoundSocketAddr<FS>> {
        self.peer.as_deref()
    }
}

/// A file descriptor donated via `SCM_RIGHTS` ancillary data, tagged with which of litebox's
/// eight fd-enabled subsystems it belongs to -- `sendmsg`'s cmsg payload is just raw `int` fd
/// values with no type information of its own, so the sender resolves each one against its own
/// [`crate::FilesState::run_on_raw_fd`] (the same per-subsystem dispatch `dup()`/`fork()` already
/// use) and carries the *result* here, since the receiver has no way to re-discover which
/// subsystem a bare `TypedFd` belongs to once it's already been duplicated out of that dispatch.
pub(super) enum AnyDupFd<Platform: ShimPlatform, FS: ShimFS> {
    Fs(litebox::fd::TypedFd<FS>),
    Net(litebox::fd::TypedFd<litebox::net::Network<Platform>>),
    Pipes(litebox::fd::TypedFd<litebox::pipes::Pipes<Platform>>),
    Eventfd(litebox::fd::TypedFd<crate::syscalls::eventfd::EventfdSubsystem<Platform>>),
    Epoll(litebox::fd::TypedFd<crate::syscalls::epoll::EpollSubsystem<Platform, FS>>),
    Unix(litebox::fd::TypedFd<UnixSocketSubsystem<Platform, FS>>),
    Pty(litebox::fd::TypedFd<crate::syscalls::pty::PtySubsystem<Platform>>),
    Signalfd(litebox::fd::TypedFd<crate::syscalls::signalfd::SignalfdSubsystem<Platform>>),
    Timerfd(litebox::fd::TypedFd<crate::syscalls::timerfd::TimerfdSubsystem<Platform>>),
    Netlink(litebox::fd::TypedFd<crate::syscalls::netlink::NetlinkSocketSubsystem>),
    /// A descriptor that crossed a process boundary as a text spec (see `RingFdMail`); the
    /// receiving task rebuilds it by name instead of `insert_into`.
    Carried(String),
}

/// A batch of `SCM_RIGHTS`-donated fds, as returned alongside a message's byte payload.
pub(super) type AnyDupFds<Platform, FS> = Vec<AnyDupFd<Platform, FS>>;

impl<Platform: ShimPlatform, FS: ShimFS> AnyDupFd<Platform, FS> {
    /// Inserts this fd into `files`' own raw fd table, allocating a fresh raw fd number --
    /// exactly [`crate::FilesState::insert_raw_fd`]'s existing per-subsystem shape, used
    /// elsewhere for `socketpair()`'s own two freshly-inserted descriptors. `cloexec` sets
    /// `FD_CLOEXEC` on the new fd first (`MSG_CMSG_CLOEXEC`'s own contract), using the typed fd
    /// still on hand here -- the same `set_fd_metadata` primitive `dup()`/`fork()` already use,
    /// since once this becomes a bare raw fd number there is no way to recover which subsystem it
    /// belongs to in order to look it back up.
    pub(super) fn insert_into(
        self,
        litebox: &litebox::LiteBox<Platform>,
        files: &crate::syscalls::file::FilesState<Platform, FS>,
        cloexec: bool,
    ) -> Result<usize, Errno> {
        fn go<Platform: ShimPlatform, FS: ShimFS, S: FdEnabledSubsystem>(
            litebox: &litebox::LiteBox<Platform>,
            files: &crate::syscalls::file::FilesState<Platform, FS>,
            fd: litebox::fd::TypedFd<S>,
            cloexec: bool,
        ) -> Result<usize, ()> {
            if cloexec {
                let old = litebox
                    .descriptor_table_mut()
                    .set_fd_metadata(&fd, litebox_common_linux::FileDescriptorFlags::FD_CLOEXEC);
                debug_assert!(old.is_none());
            }
            files.insert_raw_fd(fd).map_err(|_| ())
        }
        let res = match self {
            AnyDupFd::Fs(fd) => go(litebox, files, fd, cloexec),
            AnyDupFd::Net(fd) => go(litebox, files, fd, cloexec),
            AnyDupFd::Pipes(fd) => go(litebox, files, fd, cloexec),
            AnyDupFd::Eventfd(fd) => go(litebox, files, fd, cloexec),
            AnyDupFd::Epoll(fd) => go(litebox, files, fd, cloexec),
            AnyDupFd::Unix(fd) => go(litebox, files, fd, cloexec),
            AnyDupFd::Pty(fd) => go(litebox, files, fd, cloexec),
            AnyDupFd::Signalfd(fd) => go(litebox, files, fd, cloexec),
            AnyDupFd::Timerfd(fd) => go(litebox, files, fd, cloexec),
            AnyDupFd::Netlink(fd) => go(litebox, files, fd, cloexec),
            AnyDupFd::Carried(_) => return Err(Errno::EINVAL),
        };
        // `insert_raw_fd` only fails once the *receiver's* own `RLIMIT_NOFILE` is exceeded --
        // matches real Linux's `recvmsg` behavior of closing an over-limit donated fd and
        // reporting `MSG_CTRUNC` rather than failing the whole read (the byte payload the fd was
        // sent alongside has already been legitimately delivered by this point).
        res.map_err(|()| Errno::EMFILE)
    }

    /// Releases a duplicate [`crate::syscalls::net::Task::resolve_scm_rights_fds`] made for a
    /// donation that is actually crossing a PROCESS boundary: that data plane carries a descriptor
    /// as a text spec (`RingFdMail`, rebuilt by the receiver) and never hands the `TypedFd` to
    /// anyone, so the duplicate would otherwise sit in the SENDER's descriptor table for the rest
    /// of the session.
    ///
    /// That residue is not a harmless fd leak. The duplicate holds an `Arc` reference to the same
    /// descriptor entry, so the sender's own later `close(2)` of the donated fd sees a shared entry
    /// and gets `CloseResult::Duplicated` -- which never runs the subsystem close. Measured
    /// (xproc29): a listening socket donated to a fork child and then closed by the parent, with
    /// the child reaped, still answered its port (`CONNECTED`) where a listener closed without a
    /// donation is `ECONNREFUSED`; the same residue is what makes the shared socket table grow
    /// monotonically across a run that frees every socket it opens (xproc28: `sockets=24` ->
    /// `sockets=46`).
    ///
    /// [`litebox::fd::Descriptors::remove`] drops exactly this process's reference and nothing
    /// more: the sender's own fd keeps the object alive. It hands the entry back only when no other
    /// reference is left (the sender closed its own fd concurrently), and that one is closed
    /// properly instead of dropped.
    pub(super) fn release_undelivered_duplicate(self, global: &GlobalStateHandle<Platform, FS>) {
        fn go<Platform: ShimPlatform, FS: ShimFS, S: FdEnabledSubsystem>(
            global: &GlobalStateHandle<Platform, FS>,
            fd: litebox::fd::TypedFd<S>,
        ) {
            let _ = global.litebox.descriptor_table_mut().remove(&fd);
        }
        match self {
            AnyDupFd::Fs(fd) => go(global, fd),
            AnyDupFd::Pipes(fd) => go(global, fd),
            AnyDupFd::Eventfd(fd) => go(global, fd),
            AnyDupFd::Epoll(fd) => go(global, fd),
            AnyDupFd::Unix(fd) => go(global, fd),
            AnyDupFd::Pty(fd) => go(global, fd),
            AnyDupFd::Signalfd(fd) => go(global, fd),
            AnyDupFd::Timerfd(fd) => go(global, fd),
            AnyDupFd::Netlink(fd) => go(global, fd),
            AnyDupFd::Net(fd) => global.net_lock().release_duplicate_descriptor(&fd),
            // A `Carried` spec is a plain string: nothing was duplicated for it.
            AnyDupFd::Carried(_) => {}
        }
    }
}

/// A message sent over a Unix socket.
struct Message<Platform: ShimPlatform, FS: ShimFS> {
    data: Vec<u8>,
    /// Fds donated via `SCM_RIGHTS`, delivered atomically with this message's own first byte
    /// (matching real Linux: a `recvmsg` that doesn't read up to and past the start of this
    /// message's data never sees these fds at all -- see `do_recvmsg`'s own delivery-on-first-
    /// byte logic in `net.rs`).
    fds: Vec<AnyDupFd<Platform, FS>>,
    /// One text spec per entry of `fds` that a process other than the sender's can rebuild, or
    /// `None` when that descriptor cannot cross a process boundary.
    fd_specs: Vec<Option<String>>,
}

/// The two ways a [`UnixConnectedStream`] moves bytes to/from its peer.
///
/// `Local` is the same-process path: an `Arc`-boxed `crate::channel::Channel` per direction,
/// unusable across a real process boundary. Both ends of a local pair share one [`ConnLink`];
/// when either end is carried into a `LITEBOX_PROCESS_FORK=1` child, the pair is PROMOTED onto a
/// [`SharedUnixConnTable`] slot (see [`UnixConnectedStream::promote_for_fork`]), and from then on
/// both local ends, and the child's copy, send only through that slot. Each promoted local end
/// still drains whatever its own channel had queued before promotion first, so no byte is
/// reordered.
///
/// `Shared` is an endpoint whose only transport is a slot: a cross-process `connect()`/`accept()`,
/// or the child's copy of a carried connection. See the "Shared cross-process AF_UNIX connection
/// data plane" comment near [`SharedUnixConnTable`] for the design and its limits (no
/// `SCM_RIGHTS`, no cross-process wakeup).
enum ConnTransport<Platform: ShimPlatform, FS: ShimFS> {
    Local {
        addr: AddrView<FS>,
        /// The read end of the local socket's channel for receiving messages.
        recv_channel: crate::channel::ReadEnd<Platform, Message<Platform, FS>>,
        /// The write end of the connected peer socket for sending messages.
        connected_send_channel: crate::channel::WriteEnd<Platform, Message<Platform, FS>>,
        /// Shared with the other end of this pair.
        link: Arc<ConnLink<Platform, FS>>,
        /// This end's side of the slot the pair is promoted to (the first end of a pair is the
        /// client side).
        is_client: bool,
    },
    Shared {
        global: GlobalStateHandle<Platform, FS>,
        slot: u32,
        /// `true` if this endpoint is the connect()-ing side (reads `server_to_client`, writes
        /// `client_to_server`); `false` for the accept()-ing side (the reverse).
        is_client: bool,
        local_addr: UnixSocketAddr,
        peer_addr: UnixSocketAddr,
        /// Set true by this endpoint's own `shutdown(SHUT_RD)` -- distinct from the ring's own
        /// `write_shutdown` (which tracks the PEER's write direction).
        self_read_shutdown: AtomicBool,
    },
}

/// What the two ends of a local pair share: whether, and to which slot, the pair was promoted.
struct ConnLink<Platform: ShimPlatform, FS: ShimFS> {
    /// Serializes a promotion against the ends' channel writes, so no channel write lands after
    /// the pair switched to its slot.
    lock: Mutex<Platform, ()>,
    /// The promoted slot, or `u32::MAX`. Set after `global`.
    slot: AtomicU32,
    global: once_cell::race::OnceBox<GlobalStateHandle<Platform, FS>>,
}

impl<Platform: ShimPlatform, FS: ShimFS> ConnLink<Platform, FS> {
    fn new() -> Self {
        Self {
            lock: Mutex::new(()),
            slot: AtomicU32::new(u32::MAX),
            global: once_cell::race::OnceBox::new(),
        }
    }
}

/// One endpoint's view of a [`SharedUnixConnTable`] slot: the only data path of a `Shared`
/// endpoint and of a promoted `Local` one.
struct SharedView<'a, Platform: ShimPlatform, FS: ShimFS> {
    global: &'a GlobalStateHandle<Platform, FS>,
    slot: u32,
    is_client: bool,
}

impl<Platform: ShimPlatform, FS: ShimFS> SharedView<'_, Platform, FS> {
    fn slot_ref(&self) -> &SharedConnSlot<Platform> {
        self.global.unix_shared_conn_table.get(self.slot)
    }

    /// `(ring this side reads from, ring this side writes to)`.
    fn rings(&self) -> (&SharedByteRing<Platform, SHARED_UNIX_CONN_BUF>, &SharedByteRing<Platform, SHARED_UNIX_CONN_BUF>) {
        let slot_ref = self.slot_ref();
        if self.is_client {
            (&slot_ref.server_to_client, &slot_ref.client_to_server)
        } else {
            (&slot_ref.client_to_server, &slot_ref.server_to_client)
        }
    }

    fn platform(&self) -> &Platform {
        self.global.platform
    }

    /// The peer will never write again: it shut its write side down, or no live host process
    /// holds the peer side any more.
    fn peer_gone(&self) -> bool {
        let (read_ring, _) = self.rings();
        if read_ring.is_shutdown() {
            litebox_util_log::__private::tracing::event!(
                target: "litebox_diag::unix_conn_teardown",
                litebox_util_log::__private::tracing::Level::DEBUG,
                slot = %self.slot,
                is_client = %self.is_client,
                host_pid = %self.platform().current_host_pid(),
                "DIAG unix shared conn: peer_gone because the read ring was shut down"
            );
            return true;
        }
        if self.slot_ref().side_gone(!self.is_client, self.platform()) {
            litebox_util_log::__private::tracing::event!(
                target: "litebox_diag::unix_conn_teardown",
                litebox_util_log::__private::tracing::Level::DEBUG,
                slot = %self.slot,
                is_client = %self.is_client,
                host_pid = %self.platform().current_host_pid(),
                "DIAG unix shared conn: peer_gone because the peer side has no live holder"
            );
            read_ring.shutdown();
            return true;
        }
        false
    }

    fn hold(&self) {
        litebox_util_log::__private::tracing::event!(
            target: "litebox_diag::unix_conn_teardown",
            litebox_util_log::__private::tracing::Level::DEBUG,
            slot = %self.slot,
            is_client = %self.is_client,
            host_pid = %self.platform().current_host_pid(),
            "DIAG unix shared conn: hold"
        );
        self.slot_ref()
            .hold(self.is_client, self.platform().current_host_pid(), self.platform());
    }

    /// Wakes every host process holding the peer side so a thread blocked in a read/poll there
    /// (data arrived, EOF) or a writer blocked on a full ring (space freed) re-checks readiness
    /// now, instead of waiting out `SHARED_UNIX_POLL_INTERVAL`. Idempotent and coalescing: the
    /// platform's wake is one auto-reset event per host process.
    fn poke_peer(&self) {
        let platform = self.platform();
        self.slot_ref()
            .for_each_holder_host(!self.is_client, |host| {
                platform.wake_signal_listener(host);
            });
    }

    /// One holder of this side is gone. Once none remains, this side's write direction ends;
    /// once neither side is held, the slot returns to the pool.
    fn release_holder(&self) {
        self.release_holder_for(self.platform().current_host_pid())
    }

    /// [`Self::release_holder`] for a holder counted under `host` rather than this process: the
    /// one [`UnixSocket::fork_carry`] counted for a child that never came up to adopt it.
    fn release_holder_for(&self, host: u32) {
        litebox_util_log::__private::tracing::event!(
            target: "litebox_diag::unix_conn_teardown",
            litebox_util_log::__private::tracing::Level::DEBUG,
            slot = %self.slot,
            is_client = %self.is_client,
            host_pid = %host,
            before = %self.slot_ref().holder_dump(),
            "DIAG unix shared conn: release_holder"
        );
        let slot_ref = self.slot_ref();
        slot_ref.release(self.is_client, host);
        if slot_ref.side_gone(self.is_client, self.platform()) {
            // 118th-pass investigation: root-causing a session-client death cascade (AGENTS.md
            // "Where things stand") to a genuine EOF on each client's own unix-domain connection
            // (confirmed via `litebox_diag::socket_read`'s new `recvmsg` coverage: `size=0` right
            // before each client's Xlib-default-handler `exit(1)`) -- but WHY the last holder of
            // this side goes away at all was still invisible. This is the exact moment a reader on
            // the OTHER side starts seeing EOF (the write ring shuts down right below), so logging
            // it unconditionally (this branch is rare -- real connection teardown, not per-message
            // traffic) finally answers "which host process, holding which role, let go of this
            // slot" for the next capture of this same investigation.
            litebox_util_log::__private::tracing::event!(
                target: "litebox_diag::unix_conn_teardown",
                litebox_util_log::__private::tracing::Level::DEBUG,
                slot = %self.slot,
                is_client = %self.is_client,
                host_pid = %host,
                "DIAG unix shared conn: last holder of this side released, shutting down write ring (peer will see EOF)"
            );
            let (_, write_ring) = self.rings();
            write_ring.shutdown();
            self.poke_peer();
            if slot_ref.side_gone(!self.is_client, self.platform()) {
                self.global.unix_shared_conn_table.free(self.slot);
            }
        }
    }

    /// Prefers an atomic all-or-nothing write (temporary backpressure resolves via the normal
    /// `EAGAIN`-then-retry path); a byte stream degrades to a genuine short write only for a
    /// single message bigger than the whole ring, a record-framed slot refuses one (`EMSGSIZE`).
    fn send(&self, mut msg: Message<Platform, FS>) -> Result<usize, (Message<Platform, FS>, Errno)> {
        // This data plane carries a donated descriptor as its TEXT SPEC alone (`RingFdMail`,
        // rebuilt on the receiving side), so the `TypedFd`s `resolve_scm_rights_fds` duplicated
        // into this process's descriptor table have no receiver here. Release them now, before
        // every path out of this function -- a `Message` handed back to the caller on error is
        // dropped there, which would leave them behind just the same. See
        // `AnyDupFd::release_undelivered_duplicate` for why leaving them behind keeps the donated
        // object open forever.
        let donated = core::mem::take(&mut msg.fds);
        let n_donated = donated.len();
        for fd in donated {
            fd.release_undelivered_duplicate(self.global);
        }
        let fd_specs = if n_donated == 0 {
            None
        } else {
            let specs: Option<Vec<&str>> = msg.fd_specs.iter().map(|s| s.as_deref()).collect();
            let joined = specs
                .filter(|s| s.len() == n_donated)
                .map(|s| s.join("\u{1e}"))
                .filter(|s| s.len() <= RING_FD_MAIL_SPEC_BYTES);
            let Some(joined) = joined else {
                litebox_util_log::warn!(
                    slot:% = self.slot, n_fds:% = n_donated;
                    "unix socket: SCM_RIGHTS over a cross-process connection carries only regular \
                     files, pty slaves, eventfds, shm/memfd snapshots and unix sockets that \
                     `UnixSocket::fork_carry` can describe (a connected endpoint, an unbound or \
                     bound-but-unconnected socket, or a listener); a connect in progress and a \
                     bound or connected DATAGRAM socket are refused -- refusing the send with \
                     EOPNOTSUPP rather than dropping the fds"
                );
                return Err((msg, Errno::EOPNOTSUPP));
            };
            Some(joined)
        };
        let fd_specs = fd_specs.as_deref();
        let (_, write_ring) = self.rings();
        if write_ring.is_shutdown() || self.slot_ref().side_gone(!self.is_client, self.platform())
        {
            return Err((msg, Errno::EPIPE));
        }
        if self.slot_ref().framed.load(Ordering::Acquire) {
            if msg.data.len() + 4 > SHARED_UNIX_CONN_BUF {
                return Err((msg, Errno::EMSGSIZE));
            }
            return if write_ring.try_write_record_with_fds(&msg.data, fd_specs) {
                self.poke_peer();
                Ok(msg.data.len())
            } else {
                Err((msg, Errno::EAGAIN))
            };
        }
        if msg.data.is_empty() {
            return Ok(0);
        }
        if msg.data.len() > SHARED_UNIX_CONN_BUF {
            let n = write_ring.try_write_with_fds(&msg.data[..SHARED_UNIX_CONN_BUF], fd_specs);
            return if n == 0 {
                Err((msg, Errno::EAGAIN))
            } else {
                self.poke_peer();
                Ok(n)
            };
        }
        if !write_ring.try_write_all_with_fds(&msg.data, fd_specs) {
            return Err((msg, Errno::EAGAIN));
        }
        litebox_util_log::debug!(
            slot:% = self.slot, is_client:% = self.is_client, len:% = msg.data.len();
            "DIAG shared unix send: wrote"
        );
        self.poke_peer();
        Ok(msg.data.len())
    }

    /// Reads bytes (or, on a record-framed slot, exactly one record). Never carries `SCM_RIGHTS`
    /// fds -- the sending side refuses them up front.
    fn recv(
        &self,
        buf: &mut [u8],
        self_read_shutdown: bool,
        peek: bool,
    ) -> Result<(usize, AnyDupFds<Platform, FS>), TryOpError<Errno>> {
        if self_read_shutdown {
            return Err(TryOpError::Other(Errno::ESHUTDOWN));
        }
        let (read_ring, _) = self.rings();
        let framed = self.slot_ref().framed.load(Ordering::Acquire);
        if peek {
            let peeked = if framed {
                read_ring.try_peek_record(buf)
            } else {
                Some(read_ring.try_peek(buf)).filter(|&n| n > 0)
            };
            if let Some(n) = peeked {
                return Ok((n, Vec::new()));
            }
            if self.peer_gone() && read_ring.is_empty() {
                return Err(TryOpError::Other(Errno::ESHUTDOWN));
            }
            return Err(TryOpError::TryAgain);
        }
        let got = if framed {
            read_ring.try_read_record_with_fds(buf)
        } else {
            let (n, specs) = read_ring.try_read_with_fds(buf);
            (n > 0).then_some((n, specs))
        };
        if let Some((n, specs)) = got {
            litebox_util_log::debug!(
                slot:% = self.slot, is_client:% = self.is_client, total_read:% = n,
                prefix_hex:? = &buf[..n.min(4096)];
                "diag-unix-shared-read-bytes"
            );
            self.poke_peer();
            return Ok((n, specs.into_iter().map(AnyDupFd::Carried).collect()));
        }
        if self.peer_gone() && read_ring.is_empty() {
            return Err(TryOpError::Other(Errno::ESHUTDOWN));
        }
        Err(TryOpError::TryAgain)
    }

    fn events(&self, self_read_shutdown: bool) -> Events {
        let (read_ring, write_ring) = self.rings();
        let mut events = Events::empty();
        let peer_gone = self.peer_gone();
        if self_read_shutdown || peer_gone {
            events |= Events::RDHUP | Events::IN;
            if write_ring.is_shutdown() || peer_gone {
                events |= Events::HUP;
            }
        }
        if !read_ring.is_empty() {
            events |= Events::IN;
        }
        if write_ring.writable() {
            events |= Events::OUT;
        }
        events
    }

    /// Returns whether this call shut the write direction down.
    fn shutdown_write(&self) -> bool {
        let (_, write_ring) = self.rings();
        if write_ring.is_shutdown() {
            return false;
        }
        // 118th-pass investigation (see `release_holder`'s matching diagnostic, same
        // `litebox_diag::unix_conn_teardown` target): this is the OTHER way a peer's write ring
        // shuts down -- a deliberate `shutdown(fd, SHUT_WR)` call, not a process/holder exiting.
        // Distinguishing the two matters: `release_holder` never fired for the X11 connection in
        // the session-death-cascade investigation's first capture, so this is the next candidate.
        litebox_util_log::__private::tracing::event!(
            target: "litebox_diag::unix_conn_teardown",
            litebox_util_log::__private::tracing::Level::DEBUG,
            slot = %self.slot,
            is_client = %self.is_client,
            host_pid = %self.platform().current_host_pid(),
            "DIAG unix shared conn: explicit shutdown_write, shutting down write ring (peer will see EOF)"
        );
        write_ring.shutdown();
        self.poke_peer();
        true
    }
}

/// Represents a connected Unix stream socket.
struct UnixConnectedStream<Platform: ShimPlatform, FS: ShimFS> {
    transport: ConnTransport<Platform, FS>,
    pollee: Arc<Pollee<Platform>>,
    /// Real credentials (pid/uid/gid) of the *peer* task, as of connection
    /// establishment -- what `getsockopt(SO_PEERCRED)` reports to this side.
    peer_cred: Ucred,
    /// Latest sender in the direction this side writes / reads (pid, uid, gid; pid 0 = unset),
    /// shared with the peer's opposite fields -- see `SharedConnSlot::last_writer_c2s`.
    send_cred: Arc<[AtomicU32; 3]>,
    recv_cred: Arc<[AtomicU32; 3]>,
}

impl<Platform: ShimPlatform, FS: ShimFS> Drop for ConnTransport<Platform, FS> {
    fn drop(&mut self) {
        // A local end's channels shut themselves down via their own `Drop` impls; an endpoint on
        // a slot (shared, or promoted local) releases its holder record instead.
        match self {
            ConnTransport::Shared {
                global,
                slot,
                is_client,
                ..
            } => SharedView {
                global,
                slot: *slot,
                is_client: *is_client,
            }
            .release_holder(),
            ConnTransport::Local {
                link, is_client, ..
            } => {
                let slot = link.slot.load(Ordering::Acquire);
                if slot != u32::MAX
                    && let Some(global) = link.global.get()
                {
                    SharedView {
                        global,
                        slot,
                        is_client: *is_client,
                    }
                    .release_holder();
                }
            }
        }
    }
}

const UNIX_BUF_SIZE: usize = 65536;
impl<Platform: ShimPlatform, FS: ShimFS> UnixConnectedStream<Platform, FS> {
    /// Creates a pair of connected Unix stream sockets.
    ///
    /// `read_shutdown` and `write_shutdown` half-close the corresponding sides of the
    /// *first* returned socket only (used to carry pre-connect shutdown flags from
    /// `UnixInitStream` across `connect(2)` into the connected state).
    ///
    /// `first_cred`/`second_cred` are each side's own real credentials -- stored as the
    /// *other* side's `peer_cred`, matching `SO_PEERCRED`'s peer-identity semantics.
    fn new_pair(
        addr: Option<Arc<UnixBoundSocketAddr<FS>>>,
        pollee: Option<Arc<Pollee<Platform>>>,
        peer: Option<Arc<UnixBoundSocketAddr<FS>>>,
        read_shutdown: bool,
        write_shutdown: bool,
        first_cred: Ucred,
        second_cred: Ucred,
    ) -> (Self, Self) {
        let (addr1, addr2) = AddrView::new_pair(addr, peer);
        let cred_a: Arc<[AtomicU32; 3]> = Arc::new([const { AtomicU32::new(0) }; 3]);
        let cred_b: Arc<[AtomicU32; 3]> = Arc::new([const { AtomicU32::new(0) }; 3]);
        let pollee1 = pollee.unwrap_or(Arc::new(Pollee::new()));
        let pollee2 = Arc::new(Pollee::new());
        let (send_channel, recv_channel) =
            crate::channel::Channel::new(UNIX_BUF_SIZE, pollee2.clone(), pollee1.clone()).split();
        let (send_channel_peer, recv_channel_peer) =
            crate::channel::Channel::new(UNIX_BUF_SIZE, pollee1.clone(), pollee2.clone()).split();
        let link = Arc::new(ConnLink::new());
        let first = UnixConnectedStream {
            transport: ConnTransport::Local {
                addr: addr1,
                recv_channel,
                connected_send_channel: send_channel_peer,
                link: link.clone(),
                is_client: true,
            },
            pollee: pollee1,
            peer_cred: second_cred,
            send_cred: cred_a.clone(),
            recv_cred: cred_b.clone(),
        };
        let second = UnixConnectedStream {
            transport: ConnTransport::Local {
                addr: addr2,
                recv_channel: recv_channel_peer,
                connected_send_channel: send_channel,
                link,
                is_client: false,
            },
            pollee: pollee2,
            peer_cred: first_cred,
            send_cred: cred_b,
            recv_cred: cred_a,
        };
        let ConnTransport::Local {
            recv_channel,
            connected_send_channel,
            ..
        } = &first.transport
        else {
            unreachable!("first's transport was just constructed as ConnTransport::Local above");
        };
        if read_shutdown {
            recv_channel.shutdown();
        }
        if write_shutdown {
            connected_send_channel.shutdown();
        }
        (first, second)
    }

    /// Constructs a `Shared`-transport endpoint on `slot`, counted as one more holder of its
    /// side. `is_client` selects which of the slot's two [`SharedByteRing`]s this side
    /// reads/writes.
    fn new_shared(
        global: GlobalStateHandle<Platform, FS>,
        slot: u32,
        is_client: bool,
        local_addr: UnixSocketAddr,
        peer_addr: UnixSocketAddr,
        peer_cred: Ucred,
    ) -> Self {
        SharedView {
            global: &global,
            slot,
            is_client,
        }
        .hold();
        Self {
            transport: ConnTransport::Shared {
                global,
                slot,
                is_client,
                local_addr,
                peer_addr,
                self_read_shutdown: AtomicBool::new(false),
            },
            pollee: Arc::new(Pollee::new()),
            peer_cred,
            // Unused for `Shared` (the slot carries these); present to keep one struct shape.
            send_cred: Arc::new([const { AtomicU32::new(0) }; 3]),
            recv_cred: Arc::new([const { AtomicU32::new(0) }; 3]),
        }
    }

    /// The slot this endpoint sends through, if any: always for `Shared`, once promoted for
    /// `Local`.
    fn shared_view(&self) -> Option<SharedView<'_, Platform, FS>> {
        match &self.transport {
            ConnTransport::Shared {
                global,
                slot,
                is_client,
                ..
            } => Some(SharedView {
                global,
                slot: *slot,
                is_client: *is_client,
            }),
            ConnTransport::Local {
                link, is_client, ..
            } => {
                let slot = link.slot.load(Ordering::Acquire);
                if slot == u32::MAX {
                    return None;
                }
                Some(SharedView {
                    global: link.global.get()?,
                    slot,
                    is_client: *is_client,
                })
            }
        }
    }

    fn self_read_shutdown(&self) -> bool {
        match &self.transport {
            ConnTransport::Shared {
                self_read_shutdown, ..
            } => self_read_shutdown.load(Ordering::Acquire),
            ConnTransport::Local { recv_channel, .. } => recv_channel.is_shutdown(),
        }
    }

    /// Moves this connection onto a [`SharedUnixConnTable`] slot so a cross-process fork child
    /// can hold this end too, and returns `(slot, is_client)`.
    ///
    /// For a local pair: allocates the slot (first promotion of the pair), counts both local ends
    /// as holders in this host process, and moves this end's unread queued messages into the
    /// slot, so the child's copy of this end reads them too. The other local end keeps reading
    /// its own queued messages first and then the slot. Refused -- the caller then keeps the
    /// fork on the thread-based path -- when this end has queued `SCM_RIGHTS` fds or more unread
    /// data than the slot's ring holds, or the table is full.
    fn promote_for_fork(
        &self,
        global: &GlobalStateHandle<Platform, FS>,
        own_cred: Ucred,
        framed: bool,
    ) -> Result<(u32, bool), &'static str> {
        let (recv_channel, link, is_client) = match &self.transport {
            ConnTransport::Shared {
                slot, is_client, ..
            } => return Ok((*slot, *is_client)),
            ConnTransport::Local {
                recv_channel,
                link,
                is_client,
                ..
            } => (recv_channel, link, *is_client),
        };
        let _guard = link.lock.lock();
        let (bytes, messages, has_fds) =
            recv_channel.fold_queued((0usize, 0usize, false), |(b, n, f), m| {
                (b + m.data.len(), n + 1, f || !m.fds.is_empty())
            });
        if has_fds {
            return Err("unread SCM_RIGHTS fds queued on a unix socket");
        }
        let need = bytes + if framed { 4 * messages } else { 0 };
        let table = &global.unix_shared_conn_table;
        let existing = link.slot.load(Ordering::Acquire);
        // Only the first promotion may fold the local peer's state into the shared ring. On a
        // later promotion (this end carried again by another fork) the local peer object being
        // closed no longer means the peer is gone: its side lives on in the slot, held by the
        // process the earlier fork gave it to, and shutting the ring would end a live connection.
        let first_promotion = existing == u32::MAX;
        let slot = if existing == u32::MAX {
            if need > SHARED_UNIX_CONN_BUF {
                return Err("more unread data queued on a unix socket than a shared ring holds");
            }
            let (client_cred, server_cred) = if is_client {
                (own_cred, self.peer_cred)
            } else {
                (self.peer_cred, own_cred)
            };
            let slot = table
                .alloc(global.platform, &client_cred, &server_cred)
                .ok_or("shared unix connection table full")?;
            let slot_ref = table.get(slot);
            slot_ref.framed.store(framed, Ordering::Release);
            let me = global.platform.current_host_pid();
            slot_ref.hold(is_client, me, global.platform);
            if recv_channel.is_peer_shutdown() {
                // The other end already closed: its side counts as gone from the start.
                slot_ref.mark_side_gone(!is_client);
            } else {
                slot_ref.hold(!is_client, me, global.platform);
            }
            let _ = link.global.set(alloc::boxed::Box::new(global.clone()));
            link.slot.store(slot, Ordering::Release);
            slot
        } else {
            let view = SharedView {
                global,
                slot: existing,
                is_client,
            };
            let (read_ring, _) = view.rings();
            if need > 0 && !read_ring.is_empty() {
                return Err("unread data queued on both a unix socket's channel and its slot");
            }
            if need > read_ring.free_space() {
                return Err("more unread data queued on a unix socket than its shared ring holds");
            }
            existing
        };
        let view = SharedView {
            global,
            slot,
            is_client,
        };
        let (read_ring, _) = view.rings();
        while let Ok(data) =
            recv_channel.peek_and_consume_one(|m| Ok((true, core::mem::take(&mut m.data))))
        {
            let written = if framed {
                read_ring.try_write_record(&data)
            } else {
                read_ring.try_write_all(&data)
            };
            debug_assert!(written, "capacity was checked above");
        }
        if first_promotion && recv_channel.is_peer_shutdown() {
            read_ring.shutdown();
        }
        Ok((slot, is_client))
    }

    fn get_local_addr(&self) -> UnixSocketAddr {
        match &self.transport {
            ConnTransport::Local { addr, .. } => match addr.get_local_addr() {
                Some(addr) => UnixSocketAddr::from(addr),
                None => UnixSocketAddr::Unnamed,
            },
            ConnTransport::Shared { local_addr, .. } => local_addr.clone(),
        }
    }

    fn get_peer_addr(&self) -> UnixSocketAddr {
        match &self.transport {
            ConnTransport::Local { addr, .. } => match addr.get_peer_addr() {
                Some(addr) => UnixSocketAddr::from(addr),
                None => UnixSocketAddr::Unnamed,
            },
            ConnTransport::Shared { peer_addr, .. } => peer_addr.clone(),
        }
    }

    /// Returns the number of bytes actually accepted on success -- always `msg.data.len()` for
    /// the local channel (it pushes one whole `Message` atomically), but possibly LESS on a slot
    /// for an over-sized single write (see [`SharedView::send`]).
    fn try_sendto(
        &self,
        msg: Message<Platform, FS>,
    ) -> Result<usize, (Message<Platform, FS>, Errno)> {
        let (connected_send_channel, link) = match &self.transport {
            ConnTransport::Shared { .. } => {
                return self.shared_view().expect("shared endpoint").send(msg);
            }
            ConnTransport::Local {
                connected_send_channel,
                link,
                ..
            } => (connected_send_channel, link),
        };
        let _guard = link.lock.lock();
        if let Some(view) = self.shared_view() {
            drop(_guard);
            return view.send(msg);
        }
        // TODO: write partial data?
        let len = msg.data.len();
        let sock_id = self as *const _ as usize;
        // `LITEBOX_DRM_TRACE=1` (reused; same flag already wired end-to-end for DRM tracing, see
        // `drm::drm_trace_enabled`'s doc comment): dump a bounded hex prefix of the bytes actually
        // written to a unix stream -- the X11-protocol-decode instrumentation for AGENTS.md's
        // "Rendering/scanout blocker" investigation. 4096 bytes, since one `write()` on a busy X11
        // client socket typically batches several requests.
        if crate::syscalls::drm::drm_trace_enabled() {
            let n = core::cmp::min(msg.data.len(), 4096);
            litebox_util_log::debug!(
                sock_id:% = sock_id,
                len:% = len,
                prefix_hex:? = &msg.data[..n];
                "diag-unix-stream-write-bytes"
            );
        }
        let result = connected_send_channel.try_write_one(msg).map(|()| len);
        litebox_util_log::debug!(
            sock_id:% = sock_id,
            len:% = len,
            ok:% = result.is_ok();
            "diag-unix-stream-write: try_sendto pushed into connected_send_channel"
        );
        result
    }

    /// Reads up to `buf.len()` bytes, same message-boundary-spanning behavior as before, plus any
    /// `SCM_RIGHTS` fds attached to a message this call reads the FIRST byte of (a message whose
    /// `data` is already partially drained by an earlier call had its fds delivered on that
    /// earlier call already, matching real Linux: ancillary data rides with the start of the
    /// datagram/record it was sent alongside, never repeated on a later partial read of the same
    /// message's remaining bytes).
    fn try_recvfrom(
        &self,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<(usize, AnyDupFds<Platform, FS>), TryOpError<Errno>> {
        let recv_channel = match &self.transport {
            ConnTransport::Local { recv_channel, .. } => recv_channel,
            ConnTransport::Shared { .. } => {
                return self.shared_view().expect("shared endpoint").recv(
                    buf,
                    self.self_read_shutdown(),
                    peek,
                );
            }
        };
        if recv_channel.is_empty()
            && let Some(view) = self.shared_view()
        {
            return view.recv(buf, self.self_read_shutdown(), peek);
        }
        if peek {
            // Bytes only, across as many queued messages as `buf` holds; ancillary fds stay queued.
            return recv_channel
                .peek_all(|messages| {
                    let mut total = 0;
                    for msg in messages {
                        if total == buf.len() {
                            break;
                        }
                        let n = (buf.len() - total).min(msg.data.len());
                        buf[total..total + n].copy_from_slice(&msg.data[..n]);
                        total += n;
                    }
                    (total, Vec::new())
                })
                .map_err(|e| match e {
                    Errno::EAGAIN => TryOpError::TryAgain,
                    other => TryOpError::Other(other),
                });
        }
        let mut total_read = 0;
        let mut fds = Vec::new();
        // `buf` itself is reassigned (advanced) below as bytes are consumed; keep a raw pointer
        // to the ORIGINAL start so the trace below (added after the loop) can still read from
        // offset 0 regardless of how many partial reads happened. Only used for the bounded,
        // env-gated hex-prefix trace -- never for correctness.
        let buf_start: *const u8 = buf.as_ptr();
        let mut buf: &mut [u8] = buf;
        while !buf.is_empty() {
            let n = match recv_channel.peek_and_consume_one(|msg| {
                // `Vec::append` empties `msg.fds`, so a later partial read of this same
                // (already-drained-of-fds) message correctly appends nothing further.
                fds.append(&mut msg.fds);
                if buf.len() >= msg.data.len() {
                    buf[..msg.data.len()].copy_from_slice(&msg.data);
                    Ok((true, msg.data.len()))
                } else {
                    buf.copy_from_slice(&msg.data[..buf.len()]);
                    msg.data = msg.data.split_off(buf.len());
                    Ok((false, buf.len()))
                }
            }) {
                Ok(n) => n,
                Err(e) => {
                    if total_read > 0 {
                        break;
                    }
                    return match e {
                        Errno::EAGAIN => Err(TryOpError::TryAgain),
                        other => Err(TryOpError::Other(other)),
                    };
                }
            };
            total_read += n;
            buf = &mut buf[n..];
        }
        // See `try_sendto`'s matching comment: same `LITEBOX_DRM_TRACE=1` reuse, same bounded hex
        // prefix, this time on bytes actually delivered back to the reading process.
        if crate::syscalls::drm::drm_trace_enabled() && total_read > 0 {
            let n = core::cmp::min(total_read, 4096);
            // SAFETY: `buf_start` points at the start of the caller-provided buffer, which is
            // still valid for the duration of this function call; `total_read` (and so `n <=
            // total_read`) bytes starting there were just written by the loop above.
            let prefix = unsafe { core::slice::from_raw_parts(buf_start, n) };
            litebox_util_log::debug!(
                sock_id:% = self as *const _ as usize,
                total_read:% = total_read,
                prefix_hex:? = prefix;
                "diag-unix-stream-read-bytes"
            );
        }
        litebox_util_log::debug!(
            sock_id:% = self as *const _ as usize,
            total_read:% = total_read;
            "diag-unix-stream-read: try_recvfrom drained recv_channel"
        );
        Ok((total_read, fds))
    }

    /// `SOCK_SEQPACKET`'s own boundary-preserving read: never spans more than ONE message. A
    /// message larger than `buf` is truncated (matching real Linux `recv(2)`'s "excess bytes in a
    /// datagram are discarded" behavior) rather than left partially queued.
    ///
    /// On a slot, boundaries hold only when the slot is record-framed (a promoted local pair);
    /// a genuinely cross-process `connect()`ed `SOCK_SEQPACKET` connection is a plain byte stream,
    /// a disclosed difference rather than a panic.
    fn try_recvfrom_one_message(
        &self,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<(usize, AnyDupFds<Platform, FS>), TryOpError<Errno>> {
        let recv_channel = match &self.transport {
            ConnTransport::Local { recv_channel, .. } => recv_channel,
            ConnTransport::Shared { .. } => {
                return self.shared_view().expect("shared endpoint").recv(
                    buf,
                    self.self_read_shutdown(),
                    peek,
                );
            }
        };
        if recv_channel.is_empty()
            && let Some(view) = self.shared_view()
        {
            return view.recv(buf, self.self_read_shutdown(), peek);
        }
        if peek {
            return recv_channel
                .peek_all(|messages| {
                    let n = messages.next().map_or(0, |msg| {
                        let n = buf.len().min(msg.data.len());
                        buf[..n].copy_from_slice(&msg.data[..n]);
                        n
                    });
                    (n, Vec::new())
                })
                .map_err(|e| match e {
                    Errno::EAGAIN => TryOpError::TryAgain,
                    other => TryOpError::Other(other),
                });
        }
        let mut fds = Vec::new();
        let n = recv_channel.peek_and_consume_one(|msg| {
            fds.append(&mut msg.fds);
            let n = core::cmp::min(buf.len(), msg.data.len());
            buf[..n].copy_from_slice(&msg.data[..n]);
            // Always fully consume the message from the channel, even if `buf` was too
            // small to hold all of it -- the remainder is discarded, not left for a
            // later read (message-boundary semantics, not stream semantics).
            Ok((true, n))
        });
        match n {
            Ok(n) => Ok((n, fds)),
            Err(Errno::EAGAIN) => Err(TryOpError::TryAgain),
            Err(other) => Err(TryOpError::Other(other)),
        }
    }

    fn check_io_events(&self) -> Events {
        let (recv_channel, connected_send_channel) = match &self.transport {
            ConnTransport::Local {
                recv_channel,
                connected_send_channel,
                ..
            } => (recv_channel, connected_send_channel),
            ConnTransport::Shared { .. } => {
                return self
                    .shared_view()
                    .expect("shared endpoint")
                    .events(self.self_read_shutdown());
            }
        };
        if let Some(view) = self.shared_view() {
            let mut events = view.events(recv_channel.is_shutdown());
            if !recv_channel.is_empty() {
                events |= Events::IN;
            }
            return events;
        }
        let mut events = Events::empty();
        let is_read_shutdown = recv_channel.is_shutdown();
        let is_peer_write_shutdown = recv_channel.is_peer_shutdown();
        let is_write_shutdown = connected_send_channel.is_shutdown();
        // Linux reports `POLLHUP` once the socket is shut down in BOTH directions, which is also
        // what happens to the survivor when its peer is closed (`unix_release_sock` sets the
        // peer's `sk_shutdown` to `SHUTDOWN_MASK`). Code such as D-Bus's babysitter watches its
        // parent socket for exactly that and ignores plain `POLLIN`; without `HUP` it never
        // notices the parent went away and re-polls a permanently-readable fd forever.
        let peer_closed = is_peer_write_shutdown && connected_send_channel.is_peer_shutdown();
        if is_read_shutdown || is_peer_write_shutdown {
            events |= Events::RDHUP | Events::IN;
            if is_write_shutdown || peer_closed {
                events |= Events::HUP;
            }
        }
        if !recv_channel.is_empty() {
            events |= Events::IN;
        }
        if !connected_send_channel.is_full() {
            events |= Events::OUT;
        }
        events
    }

    fn shutdown(&self, how: ShutdownHow) {
        let mut events = Events::empty();
        match &self.transport {
            ConnTransport::Local {
                recv_channel,
                connected_send_channel,
                ..
            } => {
                if how.is_shutdown_read() && recv_channel.shutdown() {
                    events |= Events::IN | Events::RDHUP;
                }
                if how.is_shutdown_write() && connected_send_channel.shutdown() {
                    events |= Events::OUT | Events::HUP;
                }
                if how.is_shutdown_write()
                    && let Some(view) = self.shared_view()
                    && view.shutdown_write()
                {
                    events |= Events::OUT | Events::HUP;
                }
            }
            ConnTransport::Shared {
                self_read_shutdown,
                ..
            } => {
                if how.is_shutdown_read() && !self_read_shutdown.swap(true, Ordering::AcqRel) {
                    events |= Events::IN | Events::RDHUP;
                }
                if how.is_shutdown_write()
                    && self.shared_view().expect("shared endpoint").shutdown_write()
                {
                    events |= Events::OUT | Events::HUP;
                }
            }
        }
        self.pollee.notify_observers(events);
    }
}

enum UnixStreamState<Platform: ShimPlatform, FS: ShimFS> {
    Init(UnixInitStream<Platform, FS>),
    Listen(UnixListenStream<Platform, FS>),
    /// A non-blocking cross-process `connect()` still awaiting the listener's `accept()`. See
    /// [`UnixConnectingStream`]'s own doc comment for why this state exists.
    Connecting(UnixConnectingStream<Platform, FS>),
    Connected(UnixConnectedStream<Platform, FS>),
}

impl<Platform: ShimPlatform, FS: ShimFS> UnixStreamState<Platform, FS> {
    fn connected(&self) -> Option<&UnixConnectedStream<Platform, FS>> {
        match self {
            UnixStreamState::Connected(conn) => Some(conn),
            _ => None,
        }
    }
    fn listen(&self) -> Option<&UnixListenStream<Platform, FS>> {
        match self {
            UnixStreamState::Listen(listen) => Some(listen),
            _ => None,
        }
    }
}

struct UnixStream<Platform: ShimPlatform, FS: ShimFS> {
    state: RwLock<Platform, Option<UnixStreamState<Platform, FS>>>,
    /// `true` for `SOCK_SEQPACKET`, `false` for `SOCK_STREAM`. The two share every bit of
    /// connection-establishment machinery (bind/listen/connect/accept) here -- the only real
    /// behavioral difference Linux draws between them is on the read side: `SOCK_STREAM`
    /// `recv()` freely spans multiple queued messages into one byte stream, while
    /// `SOCK_SEQPACKET` `recv()` never returns more than the front message's own bytes (see
    /// `UnixConnectedStream::try_recvfrom` vs `try_recvfrom_one_message`).
    preserve_boundaries: bool,
}

impl<Platform: ShimPlatform, FS: ShimFS> UnixStream<Platform, FS> {
    /// Records `cred` as the latest sender on a cross-process connection (see
    /// `SharedConnSlot::last_writer_c2s`); a no-op for anything else.
    fn note_sender(&self, cred: &Ucred) {
        self.with_state_ref(|state| {
            if let Some(conn) = state.connected() {
                if let Some(view) = conn.shared_view() {
                    view.slot_ref().set_last_writer(view.is_client, *cred);
                } else {
                    conn.send_cred[0].store(cred.pid as u32, Ordering::Relaxed);
                    conn.send_cred[1].store(cred.uid, Ordering::Relaxed);
                    conn.send_cred[2].store(cred.gid, Ordering::Relaxed);
                }
            }
        });
    }

    fn new(state: UnixStreamState<Platform, FS>, preserve_boundaries: bool) -> Self {
        Self {
            state: litebox::sync::RwLock::new(Some(state)),
            preserve_boundaries,
        }
    }

    fn with_state_ref<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&UnixStreamState<Platform, FS>) -> R,
    {
        let old = self.state.read();
        f(old.as_ref().expect("state should never be None"))
    }

    fn with_state_mut_ref<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut UnixStreamState<Platform, FS>) -> R,
    {
        let mut old = self.state.write();
        f(old.as_mut().expect("state should never be None"))
    }

    fn with_state<F, R>(&self, f: F) -> R
    where
        F: FnOnce(UnixStreamState<Platform, FS>) -> (UnixStreamState<Platform, FS>, R),
    {
        let mut old = self.state.write();
        let (new, result) = f(old.take().expect("state should never be None"));
        *old = Some(new);
        result
    }

    fn bind(&self, task: &Task<Platform, FS>, addr: UnixSocketAddr) -> Result<(), Errno> {
        self.with_state_mut_ref(|state| {
            match state {
                UnixStreamState::Init(init) => init.bind(task, addr),
                UnixStreamState::Listen(_) => {
                    // Note Linux checks the given address and thus may return
                    // a different error code (e.g., EADDRINUSE).
                    Err(Errno::EINVAL)
                }
                UnixStreamState::Connecting(_) => Err(Errno::EISCONN),
                UnixStreamState::Connected(_) => Err(Errno::EISCONN),
            }
        })
    }

    fn listen(
        &self,
        task: &Task<Platform, FS>,
        backlog: u16,
        global: &GlobalStateHandle<Platform, FS>,
    ) -> Result<(), Errno> {
        self.with_state(|state| {
            let ret = match state {
                UnixStreamState::Init(init) => {
                    return match init.listen(task, backlog, global) {
                        Ok(listen) => (UnixStreamState::Listen(listen), Ok(())),
                        Err((init, err)) => (UnixStreamState::Init(init), Err(err)),
                    };
                }
                UnixStreamState::Listen(ref listen) => {
                    listen.listen(backlog);
                    Ok(())
                }
                UnixStreamState::Connecting(_) => Err(Errno::EISCONN),
                UnixStreamState::Connected(_) => Err(Errno::EISCONN),
            };
            (state, ret)
        })
    }

    fn lookup(
        &self,
        task: &Task<Platform, FS>,
        addr: &UnixSocketAddr,
    ) -> Result<Arc<Backlog<Platform, FS>>, Errno> {
        let guard = task.global.unix_addr_table.read();
        let Some(key) = addr.to_key() else {
            return Err(Errno::EINVAL);
        };
        let Some(entry) = guard.get(&key) else {
            // No presence-miss log here: the only caller, `connect`, falls through to
            // `connect_cross_process`, which logs only when the cross-process attempt itself fails.
            return Err(Errno::ECONNREFUSED);
        };
        match &entry.0 {
            UnixEntryInner::Stream(backlog) => Ok(backlog.clone()),
            UnixEntryInner::Datagram(_) => Err(Errno::EPROTOTYPE),
        }
    }
    fn try_connect(
        &self,
        backlog: &Backlog<Platform, FS>,
        client_cred: Ucred,
    ) -> Result<(), TryOpError<Errno>> {
        self.with_state(|state| match state {
            UnixStreamState::Init(init) => match backlog.try_connect(init, client_cred) {
                Ok(connected) => (UnixStreamState::Connected(connected), Ok(())),
                Err((init, err)) => (UnixStreamState::Init(init), Err(err)),
            },
            UnixStreamState::Listen(s) => (UnixStreamState::Listen(s), Err(Errno::EINVAL)),
            UnixStreamState::Connecting(s) => {
                (UnixStreamState::Connecting(s), Err(Errno::EALREADY))
            }
            UnixStreamState::Connected(s) => (UnixStreamState::Connected(s), Err(Errno::EISCONN)),
        })
        .map_err(|err| match err {
            Errno::EAGAIN => TryOpError::TryAgain,
            other => TryOpError::Other(other),
        })
    }
    fn connect(
        &self,
        task: &Task<Platform, FS>,
        addr: UnixSocketAddr,
        is_nonblocking: bool,
    ) -> Result<(), Errno> {
        litebox_util_log::debug!(addr:? = addr; "TRACE unix_connect: entry");
        // A repeated `connect()` on an fd already awaiting a posted cross-process request (real
        // POSIX behaviour after a non-blocking `EINPROGRESS`: the caller either polls for
        // writability -- handled entirely by `check_io_events` below, never here -- or calls
        // `connect()` again, which must report `EALREADY` while still pending and complete the
        // socket in place once the listener's `accept()` has claimed it, rather than starting a
        // brand new request and abandoning this one).
        let already_connecting = self.with_state(|state| match state {
            UnixStreamState::Connecting(connecting) => {
                if let Some(connected) = connecting.try_complete() {
                    (UnixStreamState::Connected(connected), Some(Ok(())))
                } else {
                    (
                        UnixStreamState::Connecting(connecting),
                        Some(Err(Errno::EALREADY)),
                    )
                }
            }
            other => (other, None),
        });
        if let Some(result) = already_connecting {
            litebox_util_log::debug!(addr:? = addr, ok:% = result.is_ok(); "TRACE unix_connect: result (already connecting)");
            return result;
        }
        let backlog = match self.lookup(task, &addr) {
            Ok(b) => b,
            Err(Errno::ECONNREFUSED) => {
                return match self.connect_cross_process(task, &addr, is_nonblocking) {
                    // `ECONNREFUSED` means something is at that path but nobody listens; a path
                    // that does not exist at all is `ENOENT`, which callers such as PulseAudio's
                    // stale-socket cleanup depend on to tell the two apart.
                    Err(Errno::ECONNREFUSED) => Err(Self::refused_or_missing(task, &addr)),
                    other => other,
                };
            }
            Err(e) => {
                litebox_util_log::debug!(addr:? = addr, err:? = e; "TRACE unix_connect: lookup failed");
                return Err(e);
            }
        };
        // check if we can bind to the address
        let _ = addr.clone().bind(task, false)?;
        let client_cred = task.peer_cred();
        let result = wait_on_events_polling(
            &task.wait_cx(),
            is_nonblocking,
            Events::OUT,
            |observer, mask| {
                backlog.pollee.register_observer(observer, mask);
                Ok(())
            },
            || self.try_connect(&backlog, client_cred),
        )
        // AGENTS.md 30th pass: a non-blocking connect() that has not yet completed must surface
        // EINPROGRESS, never EAGAIN -- POSIX contract for connect(2) specifically (EAGAIN there
        // means something else entirely, "no free local port"), and the exact one callers branch
        // on to decide "come back later via poll/select" vs. "this attempt itself failed". The
        // blanket `TryOpError -> Errno` conversion this used to fall through to
        // (`litebox_common_linux::errno`'s `TryOpError::TryAgain => Errno::EAGAIN`) is correct for
        // every OTHER TryOpError use in this file (sendto/recvfrom genuinely want EAGAIN) but wrong
        // here -- `litebox_shim_linux::syscalls::net::connect` already carries the identical
        // override for the TCP path; this mirrors it for AF_UNIX.
        .map_err(|err| match err {
            TryOpError::TryAgain => Errno::EINPROGRESS,
            other => Errno::from(other),
        });
        litebox_util_log::debug!(addr:? = addr, ok:% = result.is_ok(); "TRACE unix_connect: result");
        result
    }

    /// `ENOENT` when `addr` is a filesystem path with nothing at it, else `ECONNREFUSED`.
    fn refused_or_missing(task: &Task<Platform, FS>, addr: &UnixSocketAddr) -> Errno {
        if let UnixSocketAddr::Path(path) = addr {
            if let Err(litebox::fs::errors::FileStatusError::PathError(
                litebox::fs::errors::PathError::NoSuchFileOrDirectory
                | litebox::fs::errors::PathError::MissingComponent,
            )) = task.files.borrow().fs.symlink_metadata(path.as_str())
            {
                return Errno::ENOENT;
            }
        }
        Errno::ECONNREFUSED
    }

    /// The genuinely-cross-process half of [`Self::connect`]: reached only once the ordinary
    /// same-process `lookup()` against the real `unix_addr_table` has already missed. Posts a
    /// request into `global.unix_shared_connect_queue` and polls for the listener's own
    /// `accept()` (running the shared-queue half of [`Backlog::try_accept`]) to complete it --
    /// see the "Shared cross-process AF_UNIX connection data plane" module doc comment for the
    /// full rendezvous design. Falls straight through to the existing, unchanged
    /// `log_cross_process_presence_miss`-then-`ECONNREFUSED` behavior when `unix_addr_presence`
    /// doesn't show a listener anywhere either (a genuinely absent address, not a cross-process
    /// gap).
    fn connect_cross_process(
        &self,
        task: &Task<Platform, FS>,
        addr: &UnixSocketAddr,
        is_nonblocking: bool,
    ) -> Result<(), Errno> {
        let Some(key) = addr.to_key() else {
            return Err(Errno::ECONNREFUSED);
        };
        let (kind, key_bytes) = presence_kind_and_bytes(&key);
        let self_pid = task.pid.get() as u32;
        // Any advertised listener, this process's own included: a listener carried into a
        // cross-process fork child is advertised under the child's pid but is not in its local
        // address table, and takes this connect from the shared queue.
        match task.global.unix_addr_presence.lookup(kind, key_bytes) {
            Some(_) => {}
            None => {
                log_cross_process_presence_miss(task, &key);
                return Err(Errno::ECONNREFUSED);
            }
        }
        let client_cred = task.peer_cred();
        let Some(request_idx) =
            task.global
                .unix_shared_connect_queue
                .post(kind, key_bytes, &client_cred)
        else {
            // Queue full -- ordinary, guest-triggerable degrade, not a bug, but real enough to be
            // worth a visible signal now that `cancel`'s own leak-on-claim-race is fixed (62nd
            // pass): this WARN should never fire in a healthy boot, so its presence in a future
            // log is itself the diagnostic (a real exhaustion, or a NEW leak this fix didn't
            // cover) rather than something a caller needs to react to.
            litebox_util_log::warn!(
                self_pid:% = self_pid, kind:% = kind, key_len:% = key_bytes.len();
                "SharedUnixConnectQueue::post: queue full (all SHARED_UNIX_CONNECT_QUEUE_CAPACITY \
                 slots busy) -- returning EAGAIN"
            );
            return Err(Errno::EAGAIN);
        };
        litebox_util_log::debug!(
            self_pid:% = self_pid,
            kind:% = kind,
            key_len:% = key_bytes.len(),
            key_bytes:? = key_bytes,
            request_idx:% = request_idx;
            "DIAG connect_cross_process: posted request"
        );
        // Bounded even for an ordinary blocking `connect()` with no caller-supplied deadline --
        // live-caught 2026-09-18: an unbounded wait here, for a listener that has bound/listened
        // (so `unix_addr_presence` genuinely shows it) but whose OWN `accept()` loop hasn't run
        // yet (still earlier in its own startup), froze the connecting guest's entire script
        // indefinitely -- strictly worse than this path's OLD behavior (an immediate
        // `ECONNREFUSED` a calling script could itself retry/tolerate). Real POSIX blocking
        // `connect()` CAN legitimately wait a while for a slow-to-accept listener, but never
        // forever without any caller-requested timeout being involved -- bounding it here trades
        // strict fidelity for never being the thing that hangs a boot.
        let cx = task
            .wait_cx()
            .with_timeout(SHARED_UNIX_CROSS_CONNECT_TIMEOUT);
        let result = wait_on_events_polling(
            &cx,
            is_nonblocking,
            Events::empty(),
            |_observer, _mask| Ok::<(), Errno>(()), // no real wake source exists yet -- see doc
            || match task
                .global
                .unix_shared_connect_queue
                .poll_result(request_idx)
            {
                Some(slot) => Ok(slot),
                None => Err(TryOpError::TryAgain),
            },
        );
        let slot = match result {
            Ok(slot) => {
                litebox_util_log::debug!(
                    self_pid:% = self_pid,
                    request_idx:% = request_idx,
                    slot:% = slot;
                    "DIAG connect_cross_process: request completed"
                );
                slot
            }
            Err(TryOpError::WaitError(litebox::event::wait::WaitError::TimedOut)) => {
                litebox_util_log::warn!(
                    self_pid:% = self_pid,
                    request_idx:% = request_idx,
                    key_bytes:? = key_bytes;
                    "connect_cross_process: listener in another process never accepted within \
                     SHARED_UNIX_CROSS_CONNECT_TIMEOUT, cancelling and returning ECONNREFUSED"
                );
                task.global
                    .unix_shared_connect_queue
                    .cancel(&task.global, addr, request_idx);
                return Err(Errno::ECONNREFUSED);
            }
            Err(TryOpError::TryAgain) => {
                // AGENTS.md 30th pass: a non-blocking cross-process connect that has not yet been
                // claimed by the listener's own accept() loop is genuinely still IN PROGRESS, not
                // a failure -- POSIX connect(2) reserves EAGAIN for "no ephemeral port available"
                // and uses EINPROGRESS for exactly this "come back later" case, which is also the
                // one real AF_UNIX/TCP client libraries (libdbus among them) explicitly special-
                // case to mean "poll for writability, don't treat this as an error".
                //
                // FIXED (this pass): this branch used to unconditionally `cancel()` the
                // just-posted queue request right here -- i.e. every non-blocking connect attempt
                // that did not complete synchronously within the same syscall was torn down
                // immediately, so it could never complete later no matter how long the caller
                // polled for writability, which is exactly what a correct non-blocking client is
                // supposed to do after `EINPROGRESS`. Live-caught as the reason a real
                // `xfce4-session` boot never reaches `_NET_SUPPORTING_WM_CHECK`: its own
                // (GDBus/GIO-driven) D-Bus connect hits this exact arm and then never forks a
                // single child process again, consistent with a connect-then-poll-for-writable
                // client blocking forever on a request this shim had already thrown away.
                //
                // Fix: leave the request posted, and transition this socket to
                // `UnixStreamState::Connecting` (see its own doc comment) so `check_io_events`
                // and a later `connect()` on the same fd can both re-check
                // `unix_shared_connect_queue.poll_result(request_idx)` until the listener's
                // `accept()` claims and completes it.
                litebox_util_log::debug!(
                    self_pid:% = self_pid,
                    request_idx:% = request_idx;
                    "DIAG connect_cross_process: request not yet claimed (non-blocking), staying \
                     posted and returning EINPROGRESS"
                );
                let connecting = UnixConnectingStream {
                    request_idx,
                    peer_addr: addr.clone(),
                    global: task.global.clone(),
                    pollee: Pollee::new(),
                };
                return self.with_state(|state| match state {
                    UnixStreamState::Init(_) => (
                        UnixStreamState::Connecting(connecting),
                        Err(Errno::EINPROGRESS),
                    ),
                    // Lost a race with something else mutating this fd's state (e.g. a
                    // concurrent `close()`/`shutdown()` reusing the slot) between the lookup at
                    // the top of this function and here -- withdraw the request rather than leak
                    // it on a state this socket will never revisit.
                    other => {
                        task.global.unix_shared_connect_queue.cancel(
                            &task.global,
                            addr,
                            request_idx,
                        );
                        (other, Err(Errno::EINPROGRESS))
                    }
                });
            }
            Err(e) => {
                litebox_util_log::debug!(
                    self_pid:% = self_pid,
                    request_idx:% = request_idx,
                    err:? = e;
                    "DIAG connect_cross_process: request failed with other error, cancelling"
                );
                task.global
                    .unix_shared_connect_queue
                    .cancel(&task.global, addr, request_idx);
                return Err(Errno::from(e));
            }
        };
        let stream = UnixConnectedStream::new_shared(
            task.global.clone(),
            slot,
            true, // this is the connecting ("client") side of the slot
            UnixSocketAddr::Unnamed,
            addr.clone(),
            task.global.unix_shared_conn_table.get(slot).server_cred(),
        );
        self.with_state(|state| match state {
            UnixStreamState::Init(_) => (UnixStreamState::Connected(stream), Ok(())),
            other => (other, Err(Errno::EISCONN)),
        })
    }

    fn accept(
        &self,
        cx: &WaitContext<'_, Platform>,
        global: &GlobalStateHandle<Platform, FS>,
        mut peer: Option<&mut UnixSocketAddr>,
        is_nonblocking: bool,
    ) -> Result<UnixSocketInner<Platform, FS>, Errno> {
        let backlog =
            self.with_state_ref(|state| -> Result<Arc<Backlog<Platform, FS>>, Errno> {
                let listen = state.listen().ok_or(Errno::EINVAL)?;
                Ok(listen.backlog.clone())
            })?;
        litebox_util_log::debug!(nonblocking:% = is_nonblocking; "TRACE unix_accept: entry");
        let res = wait_on_events_polling(
            cx,
            is_nonblocking,
            Events::IN,
            |observer, mask| {
                backlog.pollee.register_observer(observer, mask);
                Ok(())
            },
            || {
                let accepted = backlog.try_accept(global)?;
                if let Some(peer) = peer.as_deref_mut() {
                    *peer = accepted.get_peer_addr();
                }
                Ok(UnixSocketInner::Stream(UnixStream::new(
                    UnixStreamState::Connected(accepted),
                    self.preserve_boundaries,
                )))
            },
        )
        .map_err(Errno::from);
        litebox_util_log::debug!(ok:% = res.is_ok(); "TRACE unix_accept: result");
        // accept on a shut-down listen: Linux returns EAGAIN for non-blocking, EINVAL
        // for blocking. try_accept signals shutdown via ESHUTDOWN; translate here.
        match res {
            Err(Errno::ESHUTDOWN) if is_nonblocking => Err(Errno::EAGAIN),
            Err(Errno::ESHUTDOWN) => Err(Errno::EINVAL),
            other => other,
        }
    }

    fn sendto(
        &self,
        cx: &WaitContext<'_, Platform>,
        timeout: Option<Duration>,
        buf: &[u8],
        is_nonblocking: bool,
        addr: Option<UnixSocketAddr>,
        fds: Vec<AnyDupFd<Platform, FS>>,
        fd_specs: Vec<Option<String>>,
    ) -> Result<usize, Errno> {
        let mut msg = Some(Message {
            data: buf.to_vec(),
            fds,
            fd_specs,
        });
        wait_on_events_polling(
            &cx.with_timeout(timeout),
            is_nonblocking,
            Events::OUT,
            |observer, mask| {
                self.with_state_ref(|state| {
                    let conn = state.connected().ok_or(Errno::ENOTCONN)?;
                    conn.pollee.register_observer(observer, mask);
                    Ok(())
                })
            },
            || {
                self.with_state_ref(|state| {
                    let conn = state
                        .connected()
                        .ok_or(TryOpError::Other(Errno::ENOTCONN))?;
                    if addr.is_some() {
                        return Err(TryOpError::Other(Errno::EISCONN));
                    }
                    match conn.try_sendto(msg.take().unwrap()) {
                        Ok(n) => Ok(n),
                        Err((m, Errno::EAGAIN)) => {
                            let _ = msg.replace(m);
                            Err(TryOpError::TryAgain)
                        }
                        Err((_, err)) => Err(TryOpError::Other(err)),
                    }
                })
            },
        )
        .map_err(Errno::from)
    }

    fn recvfrom(
        &self,
        cx: &WaitContext<'_, Platform>,
        timeout: Option<Duration>,
        buf: &mut [u8],
        is_nonblocking: bool,
        peek: bool,
        mut source_addr: Option<&mut Option<UnixSocketAddr>>,
    ) -> Result<(usize, AnyDupFds<Platform, FS>), Errno> {
        let res = wait_on_events_polling(
            &cx.with_timeout(timeout),
            is_nonblocking,
            Events::IN,
            |observer, mask| {
                self.with_state_ref(|state| {
                    let conn = state.connected().ok_or(Errno::ENOTCONN)?;
                    conn.pollee.register_observer(observer, mask);
                    Ok(())
                })
            },
            || {
                self.with_state_ref(|state| {
                    let conn = state
                        .connected()
                        .ok_or(TryOpError::Other(Errno::ENOTCONN))?;
                    let n = if self.preserve_boundaries {
                        conn.try_recvfrom_one_message(buf, peek)?
                    } else {
                        conn.try_recvfrom(buf, peek)?
                    };
                    // For connected stream sockets, no need to return the source address
                    if let Some(source_addr) = source_addr.as_deref_mut() {
                        *source_addr = None;
                    }
                    Ok(n)
                })
            },
        )
        .map_err(Errno::from);
        match res {
            // Linux SO_RCVTIMEO expiry surfaces as `EAGAIN`, not `ETIMEDOUT`
            Err(Errno::ETIMEDOUT) => Err(Errno::EAGAIN),
            other => other,
        }
    }

    fn get_local_addr(&self) -> UnixSocketAddr {
        self.with_state_ref(|state| match state {
            UnixStreamState::Init(init) => init
                .addr
                .as_ref()
                .map_or(UnixSocketAddr::Unnamed, UnixSocketAddr::from),
            UnixStreamState::Listen(listen) => UnixSocketAddr::from(listen.get_local_addr()),
            // Matches the `UnixSocketAddr::Unnamed` local address `connect_cross_process`
            // actually constructs the eventual `Connected` stream with.
            UnixStreamState::Connecting(_) => UnixSocketAddr::Unnamed,
            UnixStreamState::Connected(connect) => connect.get_local_addr(),
        })
    }
    fn get_peer_addr(&self) -> Option<UnixSocketAddr> {
        self.with_state_ref(|state| match state {
            // Real Linux's `getpeername()` on a still-connecting socket returns `ENOTCONN`, not a
            // stale/optimistic address -- `None` here maps to that at the syscall layer exactly
            // like `Init`'s own `None` already does.
            UnixStreamState::Init(_)
            | UnixStreamState::Listen(_)
            | UnixStreamState::Connecting(_) => None,
            UnixStreamState::Connected(connect) => Some(connect.get_peer_addr()),
        })
    }

    fn register_observer(
        &self,
        observer: Weak<dyn litebox::event::observer::Observer<Events>>,
        mask: Events,
    ) {
        self.with_state_ref(|state| match state {
            UnixStreamState::Init(init) => init.pollee.register_observer(observer, mask),
            UnixStreamState::Listen(listen) => listen.register_observer(observer, mask),
            UnixStreamState::Connecting(connecting) => {
                connecting.pollee.register_observer(observer, mask);
            }
            UnixStreamState::Connected(connect) => {
                connect.pollee.register_observer(observer, mask);
            }
        });
    }
    fn check_io_events(&self) -> Events {
        // Mutating (`with_state`, not `with_state_ref`): a `Connecting` socket whose request has
        // just been claimed must transition to `Connected` HERE, in the same call that observes
        // it -- `SharedUnixConnectQueue::poll_result` consumes the completion exactly once (see
        // its own doc comment), so a read-only check that discarded a positive result would lose
        // it forever, leaving the socket `Connecting` (and therefore permanently not-writable)
        // even though the connection genuinely completed.
        self.with_state(|state| match state {
            UnixStreamState::Init(init) => {
                // Fresh Init reports OUT|HUP (HUP because not connected). After a
                // shutdown(SHUT_RD) on an Init socket, Linux additionally reports IN
                // (a recv would return EOF immediately). SHUT_WR has no observable
                // effect on Init's poll output.
                let mut events = Events::OUT | Events::HUP;
                if init.read_shutdown.load(Ordering::Acquire) {
                    events |= Events::IN;
                }
                (UnixStreamState::Init(init), events)
            }
            UnixStreamState::Listen(listen) => {
                let events = listen.backlog.check_io_events(&listen.global);
                (UnixStreamState::Listen(listen), events)
            }
            UnixStreamState::Connecting(connecting) => match connecting.try_complete() {
                Some(connected) => {
                    let events = connected.check_io_events();
                    (UnixStreamState::Connected(connected), events)
                }
                // Still pending: not readable, not writable, not hung up -- a poller must keep
                // waiting (or re-`epoll_wait`) rather than being told this fd is ready for
                // anything yet.
                None => (UnixStreamState::Connecting(connecting), Events::empty()),
            },
            UnixStreamState::Connected(conn) => {
                let events = conn.check_io_events();
                (UnixStreamState::Connected(conn), events)
            }
        })
    }

    fn shutdown(&self, how: ShutdownHow) {
        self.with_state(|state| {
            match &state {
                UnixStreamState::Init(init) => init.shutdown(how),
                UnixStreamState::Listen(listen) => {
                    if how.is_shutdown_read() {
                        listen.backlog.shutdown();
                    }
                }
                // A still-pending connect has no data-transfer state to shut down yet; withdraw
                // the posted request (best-effort, matches the timeout path above) so a socket
                // the guest is about to abandon doesn't leave its request occupying a shared
                // queue slot until the listener eventually claims and orphans it.
                UnixStreamState::Connecting(connecting) => {
                    connecting.global.unix_shared_connect_queue.cancel(
                        &connecting.global,
                        &connecting.peer_addr,
                        connecting.request_idx,
                    );
                }
                UnixStreamState::Connected(conn) => conn.shutdown(how),
            }
            (state, ())
        });
    }
}

/// A datagram message with source address information
#[derive(Clone)]
struct DatagramMessage {
    data: Vec<u8>,
    // TODO: add control messages
    // cmsgs: Option<Vec<Cmsg>>,
    source: UnixSocketAddr,
    /// The sending task's credentials, stamped at send time so a receiver with `SO_PASSCRED` can
    /// report them as `SCM_CREDENTIALS` -- Linux records them on the skb while it is still in the
    /// sender's context, so a later `setuid` by the sender cannot change a queued message's cred.
    cred: Ucred,
}

impl<Platform: ShimPlatform> WriteEnd<Platform, DatagramMessage> {
    fn try_write(&self, msg: DatagramMessage) -> Result<(), (DatagramMessage, Errno)> {
        self.try_write_one(msg)
    }
    fn write(
        &self,
        cx: &WaitContext<'_, Platform>,
        timeout: Option<Duration>,
        msg: DatagramMessage,
        is_nonblocking: bool,
    ) -> Result<(), Errno> {
        let mut msg = Some(msg);
        cx.with_timeout(timeout)
            .wait_on_events(
                is_nonblocking,
                Events::OUT,
                |observer, mask| {
                    self.register_observer(observer, mask);
                    Ok(())
                },
                || match self.try_write(msg.take().unwrap()) {
                    Ok(()) => Ok(()),
                    Err((m, Errno::EAGAIN)) => {
                        let _ = msg.replace(m);
                        Err(TryOpError::TryAgain)
                    }
                    Err((_, err)) => Err(TryOpError::Other(err)),
                },
            )
            .map_err(Errno::from)
    }
}
impl<Platform: ShimPlatform> ReadEnd<Platform, DatagramMessage> {
    /// Attempts to read a single datagram message without blocking.
    ///
    /// Reads exactly one message, preserving message boundaries. If the buffer
    /// is smaller than the message, the excess data is discarded (truncated).
    /// Returns the original message size (which may exceed `buf.len()`).
    fn try_read(
        &self,
        buf: &mut [u8],
        mut source_addr: Option<&mut Option<UnixSocketAddr>>,
        peek: bool,
        mut sender_cred: Option<&mut Option<Ucred>>,
    ) -> Result<usize, TryOpError<Errno>> {
        let is_self_shutdown = self.is_shutdown();
        self.peek_and_consume_one(|msg| {
            let copy_len = buf.len().min(msg.data.len());
            buf[..copy_len].copy_from_slice(&msg.data[..copy_len]);
            if let Some(source_addr) = source_addr.as_deref_mut() {
                *source_addr = Some(msg.source.clone());
            }
            if let Some(sender_cred) = sender_cred.as_deref_mut() {
                *sender_cred = Some(msg.cred);
            }
            // Always consume the entire message to preserve boundaries (unless only peeking).
            Ok((!peek, msg.data.len()))
        })
        .map_err(|e| match e {
            Errno::EAGAIN => TryOpError::TryAgain,
            // ESHUTDOWN from the channel layer collapses two distinct conditions: our own
            // SHUT_RD (caller wants EOF) and peer SHUT_WR (Linux keeps the socket
            // receivable in principle, since other senders could still target it). For
            // datagram, only the self case synthesizes EOF; peer-shutdown looks like
            // "empty queue, try again".
            Errno::ESHUTDOWN if !is_self_shutdown => TryOpError::TryAgain,
            other => TryOpError::Other(other),
        })
    }
}

/// The local address of a bound datagram socket together with the global state it was registered
/// in (used to deregister the address on drop), and the guest pid that registered it in
/// `global.unix_addr_presence` (see `SharedUnixAddrPresenceTable::remove`'s same-owner-only
/// contract).
type BoundDatagramAddr<Platform, FS> = (
    UnixBoundSocketAddr<FS>,
    GlobalStateHandle<Platform, FS>,
    u32,
);

struct UnixDatagramInner<Platform: ShimPlatform, FS: ShimFS> {
    /// The local address this socket is bound to, if any.
    addr: Option<BoundDatagramAddr<Platform, FS>>,
    /// The read end of the local socket's channel for receiving messages.
    /// Set when the socket is bound via `bind` or `new_pair`.
    recv_channel: Option<ReadEnd<Platform, DatagramMessage>>,
    /// The write end of the connected peer socket for sending messages.
    /// Set when the socket is connected via `connect` or `new_pair`.
    connected_send_channel: Option<(WriteEnd<Platform, DatagramMessage>, UnixSocketAddr)>,
    read_shutdown: bool,
    write_shutdown: bool,
    pollee: Arc<Pollee<Platform>>,
    /// Credentials of the task that sent the last message this socket read, for a receiver with
    /// `SO_PASSCRED` (see [`UnixSocket::passcred_ucred`]). `None` until a message arrives: unlike
    /// a stream, a datagram socket has no single peer to fall back on, so nothing is reported
    /// until something is actually received -- which is also all Linux can do.
    last_sender: Option<Ucred>,
}
/// Represents a Unix datagram socket.
struct UnixDatagram<Platform: ShimPlatform, FS: ShimFS> {
    inner: RwLock<Platform, UnixDatagramInner<Platform, FS>>,
}

impl<Platform: ShimPlatform, FS: ShimFS> Drop for UnixDatagramInner<Platform, FS> {
    fn drop(&mut self) {
        if let Some((addr, global, owner_pid)) = self.addr.take() {
            let key = addr.to_key();
            let mut table = global.unix_addr_table.write();
            // Only remove the entry if it matches the current socket
            if let Some(UnixEntry(UnixEntryInner::Datagram(send_channel))) = table.get(&key)
                && let Some(recv_channel) = &self.recv_channel
                && send_channel.is_pair(recv_channel)
            {
                table.remove(&key);
                let (presence_kind, presence_bytes) = presence_kind_and_bytes(&key);
                global
                    .unix_addr_presence
                    .remove(presence_kind, presence_bytes, owner_pid);
            }
        }
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> UnixDatagramInner<Platform, FS> {
    /// Binds this socket to the given address.
    fn bind(&mut self, task: &Task<Platform, FS>, addr: UnixSocketAddr) -> Result<(), Errno> {
        if self.addr.is_some() {
            if addr.is_unnamed() {
                return Ok(());
            }
            return Err(Errno::EINVAL);
        }

        let bound_addr = addr.bind(task, true)?;
        let key = bound_addr.to_key();
        let owner_pid = task.pid.get() as u32;
        let (presence_kind, presence_bytes) = presence_kind_and_bytes(&key);
        task.global
            .unix_addr_presence
            .insert(presence_kind, presence_bytes, owner_pid);
        // Registers the write end of the socket in the global address table so it
        // can receive messages sent to this address.
        let (send_channel, recv_channel) =
            Channel::new(UNIX_BUF_SIZE, Arc::new(Pollee::new()), self.pollee.clone()).split();
        let _ = task
            .global
            .unix_addr_table
            .write()
            .insert(key, UnixEntry(UnixEntryInner::Datagram(send_channel)));
        self.addr = Some((bound_addr, task.global.clone(), owner_pid));
        if self.read_shutdown {
            recv_channel.shutdown();
        }
        self.recv_channel = Some(recv_channel);
        Ok(())
    }

    fn shutdown(&mut self, how: ShutdownHow) {
        let mut events = Events::empty();
        if how.is_shutdown_read() {
            self.read_shutdown = true;
            if let Some(recv_channel) = &self.recv_channel {
                recv_channel.shutdown();
            }
            events |= Events::IN | Events::RDHUP;
        }
        if how.is_shutdown_write() {
            self.write_shutdown = true;
            if let Some((connected_send_channel, _)) = &self.connected_send_channel {
                connected_send_channel.shutdown();
            }
            events |= Events::OUT | Events::HUP;
        }
        self.pollee.notify_observers(events);
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> UnixDatagram<Platform, FS> {
    fn new() -> Self {
        Self {
            inner: RwLock::new(UnixDatagramInner {
                addr: None,
                recv_channel: None,
                connected_send_channel: None,
                read_shutdown: false,
                write_shutdown: false,
                pollee: Arc::new(Pollee::new()),
                last_sender: None,
            }),
        }
    }

    fn new_pair() -> (UnixDatagram<Platform, FS>, UnixDatagram<Platform, FS>) {
        let pollee1 = Arc::new(Pollee::new());
        let pollee2 = Arc::new(Pollee::new());
        let (send_channel, recv_channel) =
            crate::channel::Channel::new(UNIX_BUF_SIZE, pollee2.clone(), pollee1.clone()).split();
        let (send_channel_peer, recv_channel_peer) =
            crate::channel::Channel::new(UNIX_BUF_SIZE, pollee1.clone(), pollee2.clone()).split();
        (
            // Cross-wire: each socket keeps the other side's send channel.
            UnixDatagram {
                inner: RwLock::new(UnixDatagramInner {
                    addr: None,
                    recv_channel: Some(recv_channel),
                    connected_send_channel: Some((send_channel_peer, UnixSocketAddr::Unnamed)),
                    read_shutdown: false,
                    write_shutdown: false,
                    pollee: pollee1,
                    last_sender: None,
                }),
            },
            UnixDatagram {
                inner: RwLock::new(UnixDatagramInner {
                    addr: None,
                    recv_channel: Some(recv_channel_peer),
                    connected_send_channel: Some((send_channel, UnixSocketAddr::Unnamed)),
                    read_shutdown: false,
                    write_shutdown: false,
                    pollee: pollee2,
                    last_sender: None,
                }),
            },
        )
    }

    /// Remembers the credentials of the task that sent the last message this socket read, for
    /// [`UnixSocket::passcred_ucred`] to hand out as `SCM_CREDENTIALS`.
    fn note_last_sender(&self, cred: Ucred) {
        self.inner.write().last_sender = Some(cred);
    }

    /// [`Self::note_last_sender`]'s read side: `None` before this socket has received anything.
    fn last_sender(&self) -> Option<Ucred> {
        self.inner.read().last_sender
    }

    /// Binds this socket to the given address.
    fn bind(&self, task: &Task<Platform, FS>, addr: UnixSocketAddr) -> Result<(), Errno> {
        self.inner.write().bind(task, addr)
    }

    /// Looks up a socket address and returns its write endpoint.
    fn lookup(
        &self,
        task: &Task<Platform, FS>,
        addr: UnixSocketAddr,
    ) -> Result<WriteEnd<Platform, DatagramMessage>, Errno> {
        let guard = task.global.unix_addr_table.read();
        let Some(key) = addr.to_key() else {
            return Err(Errno::EINVAL);
        };
        let Some(entry) = guard.get(&key) else {
            log_cross_process_presence_miss(task, &key);
            return Err(Errno::ECONNREFUSED);
        };
        // check if we can bind to the address
        let _ = addr.bind(task, false)?;
        match &entry.0 {
            UnixEntryInner::Stream(_) => Err(Errno::EPROTOTYPE),
            UnixEntryInner::Datagram(send_channel) => Ok(send_channel.clone()),
        }
    }

    /// Connects this socket to a default peer address.
    ///
    /// Subsequent sends without an address will use this peer.
    fn connect(&self, task: &Task<Platform, FS>, addr: UnixSocketAddr) -> Result<(), Errno> {
        let send_channel = self.lookup(task, addr.clone())?;
        let mut inner = self.inner.write();
        if inner.write_shutdown {
            send_channel.shutdown();
        }
        inner.connected_send_channel = Some((send_channel, addr));
        Ok(())
    }

    fn recvfrom(
        &self,
        cx: &WaitContext<'_, Platform>,
        timeout: Option<Duration>,
        buf: &mut [u8],
        is_nonblocking: bool,
        peek: bool,
        mut source_addr: Option<&mut Option<UnixSocketAddr>>,
        mut sender_cred: Option<&mut Option<Ucred>>,
    ) -> Result<usize, Errno> {
        let res = cx
            .with_timeout(timeout)
            .wait_on_events(
                is_nonblocking,
                Events::IN,
                |observer, mask| {
                    self.inner.read().pollee.register_observer(observer, mask);
                    Ok(())
                },
                || {
                    let guard = self.inner.read();
                    let Some(recv_channel) = &guard.recv_channel else {
                        return Err(TryOpError::Other(Errno::ENOTCONN));
                    };
                    recv_channel.try_read(buf, source_addr.as_deref_mut(), peek, sender_cred.as_deref_mut())
                },
            )
            .map_err(Errno::from);
        // - Non-blocking + self-shutdown(SHUT_RD) with empty queue: Linux returns EAGAIN
        //   instead of EOF (datagram boundaries; no message synthesized for the absent peer).
        // - SO_RCVTIMEO expiry on a blocking recv: Linux returns EAGAIN, not ETIMEDOUT
        //   (the latter is reserved for connect-style timeouts).
        match res {
            Err(Errno::ESHUTDOWN) if is_nonblocking => Err(Errno::EAGAIN),
            Err(Errno::ETIMEDOUT) => Err(Errno::EAGAIN),
            other => other,
        }
    }

    // Sends data to the specified or connected peer.
    ///
    /// If `addr` is provided, sends to that address. Otherwise, uses the
    /// connected peer (set via `connect()`).
    fn sendto(
        &self,
        task: &Task<Platform, FS>,
        timeout: Option<Duration>,
        buf: &[u8],
        is_nonblocking: bool,
        addr: Option<UnixSocketAddr>,
    ) -> Result<usize, Errno> {
        let source = self.get_local_addr();
        let connected_send_channel = {
            let inner = self.inner.read();
            if inner.write_shutdown {
                return Err(Errno::EPIPE);
            }
            inner
                .connected_send_channel
                .as_ref()
                .map(|(send_channel, _)| send_channel.clone())
        };

        let send_channel = if let Some(addr) = addr {
            self.lookup(task, addr)?
        } else if let Some(connected_send_channel) = connected_send_channel {
            connected_send_channel
        } else {
            return Err(Errno::ENOTCONN);
        };
        send_channel.write(
            &task.wait_cx(),
            timeout,
            DatagramMessage {
                data: buf.to_vec(),
                source,
                // Same real-ids snapshot a stream send records for `SCM_CREDENTIALS`.
                cred: task.scm_cred(),
            },
            is_nonblocking,
        )?;
        Ok(buf.len())
    }

    fn get_local_addr(&self) -> UnixSocketAddr {
        self.inner
            .read()
            .addr
            .as_ref()
            .map_or(UnixSocketAddr::Unnamed, |(addr, _, _)| {
                UnixSocketAddr::from(addr)
            })
    }
    fn get_peer_addr(&self) -> Option<UnixSocketAddr> {
        self.inner
            .read()
            .connected_send_channel
            .as_ref()
            .map(|(_, addr)| addr.clone())
    }

    fn check_io_events(&self) -> Events {
        let mut events = Events::empty();
        let inner = self.inner.read();
        let recv_shutdown = inner.read_shutdown;
        let send_shutdown = inner.write_shutdown;

        if recv_shutdown {
            events |= Events::IN | Events::RDHUP;
        } else if let Some(recv_channel) = &inner.recv_channel
            && !recv_channel.is_empty()
        {
            events |= Events::IN;
        }

        if let Some((connected_send_channel, _)) = &inner.connected_send_channel {
            if !connected_send_channel.is_full() {
                events |= Events::OUT;
            }
        } else if !send_shutdown {
            // If not connected, allow to sendto any address?
            events |= Events::OUT;
        }
        // Linux reports POLLHUP on a dgram fd only when *both* local directions are
        // shut down (peer-side shutdown is invisible since dgrams are connectionless).
        if recv_shutdown && send_shutdown {
            events |= Events::HUP;
        }
        events
    }

    fn shutdown(&self, how: ShutdownHow) {
        let mut inner = self.inner.write();
        inner.shutdown(how);
    }
}

enum UnixSocketInner<Platform: ShimPlatform, FS: ShimFS> {
    Stream(UnixStream<Platform, FS>),
    Datagram(UnixDatagram<Platform, FS>),
}
pub(crate) struct UnixSocket<Platform: ShimPlatform, FS: ShimFS> {
    inner: UnixSocketInner<Platform, FS>,
    status: AtomicU32,
    options: Mutex<Platform, SocketOptions>,
}

impl<Platform: ShimPlatform, FS: ShimFS> UnixSocket<Platform, FS> {
    fn new_with_inner(inner: UnixSocketInner<Platform, FS>, flags: SockFlags) -> Self {
        let mut status = OFlags::RDWR;
        status.set(OFlags::NONBLOCK, flags.contains(SockFlags::NONBLOCK));
        Self {
            inner,
            status: AtomicU32::new(status.bits()),
            options: litebox::sync::Mutex::new(SocketOptions::default()),
        }
    }

    pub(super) fn new(sock_type: SockType, flags: SockFlags) -> Option<Self> {
        let inner = match sock_type {
            SockType::Stream | SockType::SeqPacket => UnixSocketInner::Stream(UnixStream::new(
                UnixStreamState::Init(UnixInitStream::new()),
                matches!(sock_type, SockType::SeqPacket),
            )),
            SockType::Datagram => UnixSocketInner::Datagram(UnixDatagram::new()),
            e => {
                log_unsupported!("Unsupported unix socket type: {:?}", e);
                return None;
            }
        };
        Some(Self::new_with_inner(inner, flags))
    }

    pub(super) fn bind(
        &self,
        task: &Task<Platform, FS>,
        addr: UnixSocketAddr,
    ) -> Result<(), Errno> {
        match &self.inner {
            UnixSocketInner::Stream(stream) => stream.bind(task, addr),
            UnixSocketInner::Datagram(datagram) => datagram.bind(task, addr),
        }
    }

    pub(super) fn listen(
        &self,
        task: &Task<Platform, FS>,
        backlog: u16,
        global: &GlobalStateHandle<Platform, FS>,
    ) -> Result<(), Errno> {
        match &self.inner {
            UnixSocketInner::Stream(stream) => stream.listen(task, backlog, global),
            UnixSocketInner::Datagram(_) => Err(Errno::EOPNOTSUPP),
        }
    }

    pub(super) fn connect(
        &self,
        task: &Task<Platform, FS>,
        addr: UnixSocketAddr,
    ) -> Result<(), Errno> {
        match &self.inner {
            UnixSocketInner::Stream(stream) => {
                stream.connect(task, addr, self.get_status().contains(OFlags::NONBLOCK))
            }
            UnixSocketInner::Datagram(datagram) => datagram.connect(task, addr),
        }
    }

    pub(super) fn accept(
        &self,
        cx: &WaitContext<'_, Platform>,
        global: &GlobalStateHandle<Platform, FS>,
        flags: SockFlags,
        peer: Option<&mut UnixSocketAddr>,
    ) -> Result<UnixSocket<Platform, FS>, Errno> {
        match &self.inner {
            UnixSocketInner::Stream(stream) => {
                let accepted = stream.accept(
                    cx,
                    global,
                    peer,
                    self.get_status().contains(OFlags::NONBLOCK)
                        | flags.contains(SockFlags::NONBLOCK),
                )?;
                Ok(UnixSocket::new_with_inner(accepted, flags))
            }
            UnixSocketInner::Datagram(_) => Err(Errno::EOPNOTSUPP),
        }
    }

    pub(super) fn sendto(
        &self,
        task: &Task<Platform, FS>,
        buf: &[u8],
        flags: SendFlags,
        addr: Option<UnixSocketAddr>,
    ) -> Result<usize, Errno> {
        self.sendmsg(task, buf, flags, addr, Vec::new(), Vec::new())
    }

    /// `sendto`'s own superset: also carries `SCM_RIGHTS` fds (empty for the plain `sendto`/
    /// `sendmsg`-with-no-cmsg case). Datagram sockets don't support ancillary data at all yet
    /// (matches `Message`'s own stream-only `fds` field -- `DatagramMessage` is untouched); a
    /// non-empty `fds` there is silently dropped rather than erroring, since litebox has no
    /// datagram-socket-based real client using SCM_RIGHTS to notice the difference (Wayland, the
    /// motivating use case, uses `SOCK_STREAM`).
    pub(super) fn sendmsg(
        &self,
        task: &Task<Platform, FS>,
        buf: &[u8],
        flags: SendFlags,
        addr: Option<UnixSocketAddr>,
        fds: Vec<AnyDupFd<Platform, FS>>,
        fd_specs: Vec<Option<String>>,
    ) -> Result<usize, Errno> {
        let supported_flags = SendFlags::DONTWAIT | SendFlags::NOSIGNAL;
        if flags.intersects(supported_flags.complement()) {
            log_unsupported!("Unsupported sendto flags: {:?}", flags);
            return Err(Errno::EINVAL);
        }
        let is_nonblocking =
            flags.contains(SendFlags::DONTWAIT) || self.get_status().contains(OFlags::NONBLOCK);
        let timeout = self.options.lock().send_timeout;
        match &self.inner {
            UnixSocketInner::Stream(stream) => {
                // `SCM_CREDENTIALS` carries the sender's REAL ids, which is what `scm_cred`
                // reports; `peer_cred` is the effective-ids snapshot `SO_PEERCRED` uses.
                stream.note_sender(&task.scm_cred());
                stream.sendto(
                    &task.wait_cx(),
                    timeout,
                    buf,
                    is_nonblocking,
                    addr,
                    fds,
                    fd_specs,
                )
            }
            UnixSocketInner::Datagram(datagram) => {
                datagram.sendto(task, timeout, buf, is_nonblocking, addr)
            }
        }
    }

    /// A `recvfrom` that cannot hand out ancillary data still consumes any `SCM_RIGHTS` fds the
    /// message carried, so the sender's placeholder hold on the shared connection slot has to be
    /// dropped here or it outlives the message (`read`/`recv`/`recvfrom` all reach this).
    pub(super) fn recvfrom(
        &self,
        global: &GlobalStateHandle<Platform, FS>,
        cx: &WaitContext<'_, Platform>,
        buf: &mut [u8],
        flags: ReceiveFlags,
        source_addr: Option<&mut Option<UnixSocketAddr>>,
    ) -> Result<usize, Errno> {
        let (n, fds) = self.recvmsg(cx, buf, flags, source_addr)?;
        for fd in fds {
            if let AnyDupFd::Carried(spec) = fd {
                Self::release_unadopted_carry(global, &spec);
            }
        }
        Ok(n)
    }

    /// `recvfrom`'s own superset: also returns any `SCM_RIGHTS` fds delivered alongside the data
    /// read (always empty for a datagram socket or a message with no attached fds).
    /// The credentials to attach as `SCM_CREDENTIALS` to data this socket receives: those of the
    /// task that last sent, when `SO_PASSCRED` is on. A connected stream falls back to the peer
    /// it was established with until that peer sends; a datagram socket reports nothing until it
    /// has actually received a message.
    pub(super) fn passcred_ucred(&self) -> Option<Ucred> {
        if !self.options.lock().passcred {
            return None;
        }
        match &self.inner {
            UnixSocketInner::Stream(stream) => stream.with_state_ref(|state| match state {
                UnixStreamState::Connected(conn) => Some(match conn.shared_view() {
                    // The peer that last SENT, not the one that connected.
                    Some(view) => view
                        .slot_ref()
                        .last_writer(!view.is_client)
                        .unwrap_or(conn.peer_cred),
                    None => {
                        let pid = conn.recv_cred[0].load(Ordering::Relaxed);
                        if pid == 0 {
                            conn.peer_cred
                        } else {
                            Ucred {
                                pid: pid as _,
                                uid: conn.recv_cred[1].load(Ordering::Relaxed),
                                gid: conn.recv_cred[2].load(Ordering::Relaxed),
                            }
                        }
                    }
                }),
                _ => None,
            }),
            UnixSocketInner::Datagram(datagram) => datagram.last_sender(),
        }
    }

    pub(super) fn recvmsg(
        &self,
        cx: &WaitContext<'_, Platform>,
        buf: &mut [u8],
        flags: ReceiveFlags,
        source_addr: Option<&mut Option<UnixSocketAddr>>,
    ) -> Result<(usize, AnyDupFds<Platform, FS>), Errno> {
        // CMSG_CLOEXEC is meaningless for plain recvfrom (no ancillary data ever flows there) but
        // harmless to accept -- net.rs's do_recvmsg is what actually honors it.
        let supported_flags = ReceiveFlags::DONTWAIT
            | ReceiveFlags::TRUNC
            | ReceiveFlags::CMSG_CLOEXEC
            | ReceiveFlags::PEEK;
        if flags.intersects(supported_flags.complement()) {
            log_unsupported!("Unsupported recvfrom flags: {:?}", flags);
            return Err(Errno::EINVAL);
        }
        let is_nonblocking =
            flags.contains(ReceiveFlags::DONTWAIT) || self.get_status().contains(OFlags::NONBLOCK);
        let peek = flags.contains(ReceiveFlags::PEEK);
        let timeout = self.options.lock().recv_timeout;
        let ret = match &self.inner {
            UnixSocketInner::Stream(stream) => {
                stream.recvfrom(cx, timeout, buf, is_nonblocking, peek, source_addr)
            }
            UnixSocketInner::Datagram(datagram) => {
                let mut sender = None;
                datagram
                    .recvfrom(
                        cx,
                        timeout,
                        buf,
                        is_nonblocking,
                        peek,
                        source_addr,
                        Some(&mut sender),
                    )
                    .map(|n| {
                        if let Some(cred) = sender {
                            datagram.note_last_sender(cred);
                        }
                        (n, Vec::new())
                    })
            }
        };
        match ret {
            Err(Errno::ESHUTDOWN) => Ok((0, Vec::new())),
            other => other,
        }
    }

    pub(super) fn get_local_addr(&self) -> UnixSocketAddr {
        match &self.inner {
            UnixSocketInner::Stream(stream) => stream.get_local_addr(),
            UnixSocketInner::Datagram(datagram) => datagram.get_local_addr(),
        }
    }
    pub(super) fn get_peer_addr(&self) -> Option<UnixSocketAddr> {
        match &self.inner {
            UnixSocketInner::Stream(stream) => stream.get_peer_addr(),
            UnixSocketInner::Datagram(datagram) => datagram.get_peer_addr(),
        }
    }

    pub(super) fn new_connected_pair(
        task: &Task<Platform, FS>,
        ty: SockType,
        flags: SockFlags,
    ) -> Option<(UnixSocket<Platform, FS>, UnixSocket<Platform, FS>)> {
        match ty {
            SockType::Stream | SockType::SeqPacket => {
                // Both ends of a socketpair(2) are created by the same task, so each
                // reports the creating task's own real credentials as its peer's identity
                // -- matching real Linux's symmetric behavior for socketpair-created socks.
                let cred = task.peer_cred();
                let (conn1, conn2) =
                    UnixConnectedStream::new_pair(None, None, None, false, false, cred, cred);
                let preserve_boundaries = matches!(ty, SockType::SeqPacket);
                Some((
                    UnixSocket::new_with_inner(
                        UnixSocketInner::Stream(UnixStream::new(
                            UnixStreamState::Connected(conn1),
                            preserve_boundaries,
                        )),
                        flags,
                    ),
                    UnixSocket::new_with_inner(
                        UnixSocketInner::Stream(UnixStream::new(
                            UnixStreamState::Connected(conn2),
                            preserve_boundaries,
                        )),
                        flags,
                    ),
                ))
            }
            SockType::Datagram => {
                let (datagram1, datagram2) = UnixDatagram::new_pair();
                Some((
                    UnixSocket::new_with_inner(UnixSocketInner::Datagram(datagram1), flags),
                    UnixSocket::new_with_inner(UnixSocketInner::Datagram(datagram2), flags),
                ))
            }
            _ => None,
        }
    }

    pub(super) fn setsockopt(
        &self,
        global: &GlobalStateHandle<Platform, FS>,
        optname: SocketOptionName,
        optval: UserPtr<u8>,
        optlen: usize,
    ) -> Result<(), Errno> {
        match global.setsockopt_common(optname, optval, optlen, |so, value| {
            match (so, value) {
                (SocketOption::RCVTIMEO, SocketOptionValue::Timeout(timeout)) => {
                    self.options.lock().recv_timeout = timeout;
                }
                (SocketOption::SNDTIMEO, SocketOptionValue::Timeout(timeout)) => {
                    self.options.lock().send_timeout = timeout;
                }
                (SocketOption::LINGER, SocketOptionValue::Timeout(timeout)) => {
                    self.options.lock().linger_timeout = timeout;
                }
                (SocketOption::REUSEADDR, SocketOptionValue::U32(val)) => {
                    self.options.lock().reuse_address = val != 0;
                }
                (SocketOption::KEEPALIVE, SocketOptionValue::U32(val)) => {
                    self.options.lock().keep_alive = val != 0;
                }
                (SocketOption::BROADCAST, SocketOptionValue::U32(val)) => {
                    self.options.lock().broadcast = val != 0;
                }
                _ => unreachable!(),
            }
            Ok(())
        }) {
            Err(Errno::ENOPROTOOPT) => {} // continue to handle unix
            other => return other,
        }

        match optname {
            SocketOptionName::IP(ip) => match ip {
                IpOption::TOS | IpOption::RECVERR => Err(Errno::EOPNOTSUPP),
                _ => Err(Errno::ENOPROTOOPT),
            },
            SocketOptionName::IPV6(_) => Err(Errno::ENOPROTOOPT),
            SocketOptionName::Socket(so) => match so {
                // handled by `setsockopt_common`
                SocketOption::RCVTIMEO
                | SocketOption::SNDTIMEO
                | SocketOption::LINGER
                | SocketOption::REUSEADDR
                | SocketOption::KEEPALIVE
                | SocketOption::BROADCAST => {
                    unreachable!()
                }
                // Don't allow changing socket type and credentials
                SocketOption::TYPE | SocketOption::PEERCRED | SocketOption::ERROR => {
                    Err(Errno::ENOPROTOOPT)
                }
                // `SO_DOMAIN`/`SO_PROTOCOL` report what the socket was created as; Linux has no
                // setter for them (`sock_setsockopt` falls through to `ENOPROTOOPT`). Answering
                // `Ok(())` here would let a caller believe it had re-classified the socket.
                SocketOption::DOMAIN | SocketOption::PROTOCOL => Err(Errno::ENOPROTOOPT),
                // SO_RCVBUF / SO_SNDBUF are advisory hints. Accept them and keep
                // the fixed internal buffer size, instead of returning EOPNOTSUPP.
                // Log at debug so the accepted-but-ignored option stays visible.
                SocketOption::RCVBUF | SocketOption::SNDBUF => {
                    litebox_util_log::debug!(
                        "accepting and ignoring setsockopt(SO_RCVBUF/SO_SNDBUF) on unix socket; using fixed buffer size"
                    );
                    Ok(())
                }
                SocketOption::PASSCRED => {
                    let val: u32 = super::read_from_user::<_, Platform>(optval, optlen)?;
                    self.options.lock().passcred = val != 0;
                    Ok(())
                }
                SocketOption::PRIORITY | SocketOption::REUSEPORT => Ok(()),
            },
            SocketOptionName::TCP(_) => Err(Errno::EOPNOTSUPP),
        }
    }
    pub(super) fn getsockopt(
        &self,
        global: &GlobalStateHandle<Platform, FS>,
        optname: SocketOptionName,
        optval: UserPtrMut<u8>,
        len: u32,
    ) -> Result<usize, Errno> {
        match global.getsockopt_common(optname, optval, len, |sopt| match sopt {
            SocketOption::RCVTIMEO => SocketOptionValue::Timeout(self.options.lock().recv_timeout),
            SocketOption::SNDTIMEO => SocketOptionValue::Timeout(self.options.lock().send_timeout),
            SocketOption::LINGER => SocketOptionValue::Timeout(self.options.lock().linger_timeout),
            SocketOption::REUSEADDR => {
                SocketOptionValue::U32(u32::from(self.options.lock().reuse_address))
            }
            SocketOption::KEEPALIVE => {
                SocketOptionValue::U32(u32::from(self.options.lock().keep_alive))
            }
            SocketOption::BROADCAST => {
                SocketOptionValue::U32(u32::from(self.options.lock().broadcast))
            }
            _ => unreachable!(),
        }) {
            Err(Errno::ENOPROTOOPT) => {} // continue to handle unix
            other => return other,
        }

        let val: u32 = match optname {
            SocketOptionName::IP(ip) => match ip {
                IpOption::TOS | IpOption::RECVERR => return Err(Errno::EOPNOTSUPP),
                _ => return Err(Errno::ENOPROTOOPT),
            },
            SocketOptionName::IPV6(_) => return Err(Errno::ENOPROTOOPT),
            SocketOptionName::Socket(so) => match so {
                // handled by `getsockopt_common`
                SocketOption::RCVTIMEO
                | SocketOption::SNDTIMEO
                | SocketOption::LINGER
                | SocketOption::REUSEADDR
                | SocketOption::KEEPALIVE
                | SocketOption::BROADCAST => {
                    unreachable!()
                }
                // Unix sockets don't track async errors
                SocketOption::ERROR => 0,
                // `SO_DOMAIN` is the family `socket(2)` was called with, which is `AF_UNIX` for
                // every socket here. `SO_PROTOCOL` is 0 because a unix socket has no protocol --
                // that is Linux's own value (`sk->sk_protocol` stays `IPPROTO_IP`), not a
                // placeholder. Found by a real failure: Python's `socket.socket(fileno=fd)` --
                // the only way to adopt an fd received over SCM_RIGHTS -- probes both, and
                // raising `OSError(92, 'Protocol not available')` made every carried unix
                // socket unusable.
                SocketOption::DOMAIN => AddressFamily::UNIX as u32,
                SocketOption::PROTOCOL => 0,
                SocketOption::TYPE => match &self.inner {
                    UnixSocketInner::Stream(stream) if stream.preserve_boundaries => {
                        SockType::SeqPacket as u32
                    }
                    UnixSocketInner::Stream(_) => SockType::Stream as u32,
                    UnixSocketInner::Datagram(_) => SockType::Datagram as u32,
                },
                SocketOption::RCVBUF | SocketOption::SNDBUF => UNIX_BUF_SIZE.trunc(),
                SocketOption::PASSCRED => u32::from(self.options.lock().passcred),
                SocketOption::PRIORITY | SocketOption::REUSEPORT => 0,
                SocketOption::PEERCRED => match &self.inner {
                    UnixSocketInner::Stream(stream) => {
                        let ucred = stream.with_state_ref(|state| -> Result<Ucred, Errno> {
                            match state {
                                UnixStreamState::Connected(conn) => Ok(conn.peer_cred),
                                _ => Ok(litebox_common_linux::Ucred {
                                    pid: 0,
                                    uid: u32::MAX,
                                    gid: u32::MAX,
                                }),
                            }
                        })?;
                        return super::write_to_user::<_, Platform>(ucred, optval, len);
                    }
                    UnixSocketInner::Datagram(_) => {
                        log_unsupported!("get PEERCRED for unix datagram socket");
                        return Err(Errno::EOPNOTSUPP);
                    }
                },
            },
            SocketOptionName::TCP(_) => return Err(Errno::EOPNOTSUPP),
        };
        super::write_to_user::<_, Platform>(val, optval, len)
    }

    pub(super) fn shutdown(&self, how: ShutdownHow) {
        match &self.inner {
            UnixSocketInner::Stream(stream) => stream.shutdown(how),
            UnixSocketInner::Datagram(datagram) => datagram.shutdown(how),
        }
    }

    super::common_functions_for_file_status!();
}

impl<Platform: ShimPlatform, FS: ShimFS> IOPollable for UnixSocket<Platform, FS> {
    fn register_observer(
        &self,
        observer: Weak<dyn litebox::event::observer::Observer<Events>>,
        mask: Events,
    ) {
        match &self.inner {
            UnixSocketInner::Stream(stream) => {
                stream.register_observer(observer, mask);
            }
            UnixSocketInner::Datagram(datagram) => {
                datagram
                    .inner
                    .read()
                    .pollee
                    .register_observer(observer, mask);
            }
        }
    }

    fn check_io_events(&self) -> Events {
        match &self.inner {
            UnixSocketInner::Stream(stream) => stream.check_io_events(),
            UnixSocketInner::Datagram(datagram) => datagram.check_io_events(),
        }
    }
}

pub(crate) struct UnixEntry<Platform: ShimPlatform, FS: ShimFS>(UnixEntryInner<Platform, FS>);
enum UnixEntryInner<Platform: ShimPlatform, FS: ShimFS> {
    Stream(Arc<Backlog<Platform, FS>>),
    Datagram(WriteEnd<Platform, DatagramMessage>),
}

/// Type alias for the global Unix socket address table.
pub(crate) type UnixAddrTable<Platform, FS> = BTreeMap<UnixSocketAddrKey, UnixEntry<Platform, FS>>;

/// Matches Linux's `sockaddr_un.sun_path` byte capacity -- the longest key
/// [`SharedUnixAddrPresenceTable`] ever needs to store (a `Path` key's UTF-8 bytes, or an
/// `Abstract` key's raw bytes).
pub(crate) const UNIX_ADDR_KEY_MAX: usize = 108;

/// Realistic upper bound on simultaneously bound AF_UNIX addresses in one guest session (the X11
/// display socket, D-Bus system + session bus, at most a handful of application IPC sockets) --
/// sized generously rather than exactly, since an unused slot costs only a few dozen bytes and
/// this table is a single fixed-size allocation, never grown.
pub(crate) const UNIX_ADDR_PRESENCE_CAPACITY: usize = 256;

const PRESENCE_SLOT_EMPTY: u32 = 0;
const PRESENCE_SLOT_WRITING: u32 = 1;
const PRESENCE_SLOT_OCCUPIED: u32 = 2;

/// `UnixSocketAddrKey`'s two variants, recorded numerically so a presence-table slot can compare
/// against a key without depending on that enum's own (non-`Copy`, heap-owning) representation.
pub(crate) const UNIX_ADDR_KIND_PATH: u32 = 0;
pub(crate) const UNIX_ADDR_KIND_ABSTRACT: u32 = 1;

/// One slot of [`SharedUnixAddrPresenceTable`]. Every field is a plain fixed-width atomic --
/// deliberately no `Vec`/`Box`/pointer anywhere in this type, unlike [`UnixAddrTable`]'s
/// `BTreeMap` -- so the WHOLE slot's live state is its own inline bytes, with no separately
/// heap-allocated node for a cross-process attacher to fail to resolve. This is what lets placing
/// [`SharedUnixAddrPresenceTable`] as an ordinary field of `GlobalState` (itself placed in the
/// shared kernel arena on `WindowsUserland`'s cross-process-fork path, see
/// `docs/AGENTS_ARCHIVE_2026-09-17.md`'s "Shared kernel heap"/`SharedArc` sections) give it real
/// cross-process content sharing for free, the same way `next_thread_id`'s plain `AtomicI32`
/// already does -- without needing a second `SharedKernelStateProvider` slot, a second
/// create-or-attach protocol, or any `unsafe` at all.
struct UnixAddrPresenceSlot {
    /// [`PRESENCE_SLOT_EMPTY`] / [`PRESENCE_SLOT_WRITING`] / [`PRESENCE_SLOT_OCCUPIED`]. Every
    /// other field is only meaningful once this is [`PRESENCE_SLOT_OCCUPIED`] -- `Acquire`-loaded
    /// before reading them, `Release`-stored after writing them, so a reader that observes
    /// `PRESENCE_SLOT_OCCUPIED` also observes every byte a concurrent inserter wrote before its
    /// own `Release` store (the same publish pattern `SharedArc::new`'s doc comment already
    /// establishes for this codebase's other lock-free cross-process structures).
    state: AtomicU32,
    /// [`UNIX_ADDR_KIND_PATH`] / [`UNIX_ADDR_KIND_ABSTRACT`].
    kind: AtomicU32,
    /// Number of valid leading bytes in `bytes` (`<= UNIX_ADDR_KEY_MAX`).
    len: AtomicU32,
    /// The guest pid ([`Task::pid`], globally unique across the whole fork family via the
    /// already-cross-process-shared `next_thread_id` allocator -- not the host OS pid, which no
    /// platform-agnostic code in this `no_std` crate can read) that inserted this entry.
    owner_pid: AtomicU32,
    bytes: [AtomicU8; UNIX_ADDR_KEY_MAX],
}

impl UnixAddrPresenceSlot {
    fn new_empty() -> Self {
        Self {
            state: AtomicU32::new(PRESENCE_SLOT_EMPTY),
            kind: AtomicU32::new(0),
            len: AtomicU32::new(0),
            owner_pid: AtomicU32::new(0),
            bytes: core::array::from_fn(|_| AtomicU8::new(0)),
        }
    }

    fn matches(&self, kind: u32, key: &[u8]) -> bool {
        self.kind.load(Ordering::Relaxed) == kind
            && self.len.load(Ordering::Relaxed) as usize == key.len()
            && key
                .iter()
                .enumerate()
                .all(|(i, b)| self.bytes[i].load(Ordering::Relaxed) == *b)
    }
}

/// Cross-process-visible AF_UNIX address presence table: a fixed-capacity, lock-free (pure
/// `core::sync::atomic`, no `RawMutex`/OS wait primitive -- see [`UnixAddrPresenceSlot`]'s doc
/// comment for why none is needed) side-index recording WHICH addresses are currently
/// bound/listening and by which guest pid, kept alongside (never instead of) each process's own
/// real [`UnixAddrTable`].
///
/// # Scope -- what this table does NOT do
///
/// This closes only the "is address K bound anywhere in this fork family" visibility gap
/// (`docs/AGENTS_ARCHIVE_2026-09-17.md`'s `globalstate-nested-collections-not-actually-shared`
/// PRD). It deliberately does NOT attempt to make a cross-process `connect()` actually complete: a
/// `Backlog`'s pending-connection queue and a connected stream's `crate::channel::Channel`
/// byte-transport buffers are themselves further heap-allocated (`VecDeque`/ring-buffer internals
/// on the ordinary private per-process heap), so even a slot that names a REAL, currently-occupied
/// address cannot safely hand back a dereferenceable `Arc<Backlog>` to a DIFFERENT process's
/// `connect()` call -- that needs a genuinely new shared-memory-native channel/pollee
/// implementation, out of scope here (see call sites of [`SharedUnixAddrPresenceTable::lookup`]
/// for the precise diagnostic this enables instead: distinguishing "nothing is listening" from
/// "something is listening, in a different process, not yet reachable").
pub(crate) struct SharedUnixAddrPresenceTable {
    slots: [UnixAddrPresenceSlot; UNIX_ADDR_PRESENCE_CAPACITY],
}

impl SharedUnixAddrPresenceTable {
    pub(crate) fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| UnixAddrPresenceSlot::new_empty()),
        }
    }

    /// Registers `key` (of the given `kind`) as owned by `owner_pid`. Returns `false` -- never
    /// panics, this is a guest-reachable path -- if `key` exceeds [`UNIX_ADDR_KEY_MAX`] or every
    /// slot is occupied; both degrade only THIS side table's diagnostic value, never the real
    /// per-process [`UnixAddrTable`] insert a caller already performed first.
    pub(crate) fn insert(&self, kind: u32, key: &[u8], owner_pid: u32) -> bool {
        if key.len() > UNIX_ADDR_KEY_MAX {
            return false;
        }
        for slot in &self.slots {
            if slot
                .state
                .compare_exchange(
                    PRESENCE_SLOT_EMPTY,
                    PRESENCE_SLOT_WRITING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                for (i, b) in key.iter().enumerate() {
                    slot.bytes[i].store(*b, Ordering::Relaxed);
                }
                slot.len.store(key.len() as u32, Ordering::Relaxed);
                slot.kind.store(kind, Ordering::Relaxed);
                slot.owner_pid.store(owner_pid, Ordering::Relaxed);
                slot.state.store(PRESENCE_SLOT_OCCUPIED, Ordering::Release);
                return true;
            }
        }
        false
    }

    /// Removes the occupied slot matching `(kind, key, owner_pid)` exactly, if any. Only ever
    /// called by the same process that inserted it (bind/listen and its own `Drop` always run in
    /// the same process), so this plain `Acquire` scan + `Release` store back to
    /// [`PRESENCE_SLOT_EMPTY`] cannot race with a concurrent remover of the SAME logical entry.
    pub(crate) fn remove(&self, kind: u32, key: &[u8], owner_pid: u32) {
        if key.len() > UNIX_ADDR_KEY_MAX {
            return;
        }
        for slot in &self.slots {
            if slot.state.load(Ordering::Acquire) == PRESENCE_SLOT_OCCUPIED
                && slot.owner_pid.load(Ordering::Relaxed) == owner_pid
                && slot.matches(kind, key)
            {
                slot.state.store(PRESENCE_SLOT_EMPTY, Ordering::Release);
                return;
            }
        }
    }

    /// Looks up `key`; returns the owning guest pid if occupied by ANY process in the fork
    /// family, including the caller's own.
    pub(crate) fn lookup(&self, kind: u32, key: &[u8]) -> Option<u32> {
        if key.len() > UNIX_ADDR_KEY_MAX {
            return None;
        }
        for slot in &self.slots {
            if slot.state.load(Ordering::Acquire) == PRESENCE_SLOT_OCCUPIED
                && slot.matches(kind, key)
            {
                return Some(slot.owner_pid.load(Ordering::Relaxed));
            }
        }
        None
    }
}

/// Numeric kind plus raw key bytes for [`SharedUnixAddrPresenceTable`], derived from a real
/// [`UnixSocketAddrKey`] so every call site shares one conversion instead of matching the enum
/// itself repeatedly.
pub(crate) fn presence_kind_and_bytes(key: &UnixSocketAddrKey) -> (u32, &[u8]) {
    match key {
        UnixSocketAddrKey::Path(path) => (UNIX_ADDR_KIND_PATH, path.as_bytes()),
        UnixSocketAddrKey::Abstract(bytes) => (UNIX_ADDR_KIND_ABSTRACT, bytes.as_slice()),
    }
}

/// Called on every real (non-diagnostic) `unix_addr_table` lookup miss -- i.e. every
/// `ECONNREFUSED` this module was already about to return -- to distinguish, with real evidence
/// instead of a hypothesis, the two cases `docs/AGENTS_ARCHIVE_2026-09-17.md`'s
/// `globalstate-nested-collections-not-actually-shared` finding could not previously tell apart
/// from a log alone: "nothing is listening at this address anywhere" (real `ECONNREFUSED`,
/// `unix_addr_presence` also misses) vs. "something IS listening, in a DIFFERENT process, just
/// not yet reachable from this one" (`unix_addr_presence` hits with a foreign `owner_pid` --
/// [`SharedUnixAddrPresenceTable`]'s own doc comment explains why this table cannot yet also fix
/// the connection itself). Always-on, not gated behind an env var: this fires only on an already-
/// failing path, at most once per failed `connect`/`sendto`, so its cost is negligible.
fn log_cross_process_presence_miss<Platform: ShimPlatform, FS: ShimFS>(
    task: &Task<Platform, FS>,
    key: &UnixSocketAddrKey,
) {
    let (presence_kind, presence_bytes) = presence_kind_and_bytes(key);
    match task
        .global
        .unix_addr_presence
        .lookup(presence_kind, presence_bytes)
    {
        Some(owner_pid) if owner_pid != task.pid.get() as u32 => {
            litebox_util_log::warn!(
                self_pid:% = task.pid.get(), owner_pid:% = owner_pid;
                "[unix_addr_presence] ECONNREFUSED but address IS bound, by a DIFFERENT guest pid -- \
                 cross-process AF_UNIX data-plane sharing gap (unix_addr_table's Backlog/Channel \
                 values are not yet shared-memory-native), not a genuinely absent listener"
            );
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Shared cross-process AF_UNIX connection data plane.
//
// `unix_addr_presence` (above) only proves a listener EXISTS somewhere in the fork family; it
// deliberately can't hand back a usable connection because `Backlog`/`UnixConnectedStream`'s
// `crate::channel::Channel` byte-transport buffers are `Arc`-boxed on the ordinary per-process
// heap -- the same stale-cross-process-pointer class fixed a dozen times today for
// `Network::socket_set` et al., just one layer deeper. Extending that same fixed-slot,
// pointer-free pattern to the CONNECTION itself needs more than a storage swap, though: today's
// `connect()`/`accept()` are direct same-process method calls on `Arc<Backlog>`/
// `Arc<Pollee>` -- objects a genuinely different process can never dereference at all, no matter
// what backs them. The types below are therefore a real rendezvous PROTOCOL, not just a shared
// buffer: a client that cannot resolve `unix_addr_table` locally but sees a foreign owner in
// `unix_addr_presence` posts a request into `SharedUnixConnectQueue`; the listener's own
// `accept()` loop, which already polls its private backlog, also polls this queue for requests
// naming its own address and completes them by handing out a slot from `SharedUnixConnTable` --
// a fixed pool of fixed-capacity byte ring buffers, each guarded by the same cross-process
// `RawMutex`-backed `litebox::sync::Mutex` every other shared registry in this codebase already
// uses (`Network::net_lock` et al.), so both processes can safely read/write from their own
// address space with no raw pointer crossing the process boundary anywhere.
//
// # Explicit scope limits (guest-reachable, never a panic on the excluded paths)
//
// - **No `SCM_RIGHTS`.** `Message::fds` has no representation here -- a `TypedFd`/`AnyDupFd` is
//   a handle into ONE process's own descriptor table, not plain bytes. `SharedView::send` refuses
//   a non-empty `fds` list with `EOPNOTSUPP` and a warning (never silently drops them).
// - **Byte stream, except a promoted `SOCK_SEQPACKET` pair**, whose slot is record-framed
//   (`SharedConnSlot::framed`); a `connect()`ed cross-process `SOCK_SEQPACKET` connection is a
//   plain byte stream.
// - **Holders are counted per host process** (`SharedConnSlot::hold`): after a cross-process fork
//   the parent and child hold the same side, and the peer sees EOF/`EPIPE` only once every holder
//   of that side is gone or its host process died.
// - **No genuine cross-process wakeup.** Nothing in this codebase can deliver a Windows-level
//   wake from one process's write into a blocked wait in a different process's `Pollee` (that
//   would need `litebox_platform_windows_userland/src/xproc_sync.rs`'s named-event primitive,
//   `docs/AGENTS_ARCHIVE_2026-09-17.md` calls it "still unwired") -- so every blocking operation
//   on a `Shared`-transport connection (`accept`/`connect`/`sendto`/`recvfrom` all reaching
//   `EAGAIN`) is driven by the call sites below re-polling on a short bounded timeout
//   ([`SHARED_UNIX_POLL_INTERVAL`]) instead of a real wait, matching this codebase's own already-
//   established fallback philosophy for exactly this situation (`RawMutex::
//   poll_until_value_changes`'s doc comment: "Correct by construction ... at the cost of
//   latency/CPU while polling, never loses a wakeup").
pub(crate) const SHARED_UNIX_POLL_INTERVAL: Duration = Duration::from_millis(15);

/// Upper bound on how long [`UnixStream::connect_cross_process`] waits for a listener in a
/// DIFFERENT process to reach its own `accept()` call, even for an ordinary blocking `connect()`
/// with no caller-supplied deadline -- see that function's own doc comment for the live-caught
/// indefinite-hang this bounds.
///
/// Widened from the original `3s` (twenty-fifth pass): live-caught, twice in a row, a listener
/// that WAS genuinely bound+listening (`unix_addr_presence` confirmed it) and had already served
/// a request successfully moments earlier in a calm run (twenty-fourth pass, ~23ms) still let a
/// real request sit unclaimed for the full old 3s bound and time out, once the `$XSOCK`
/// writable-layer-visibility fix (same pass) let the boot legitimately reach this code path for
/// the first time and put 8 real concurrent cross-process-forked Windows processes in flight at
/// once. `SHARED_UNIX_POLL_INTERVAL`'s 15ms re-poll cadence only fires while the listener's OWN
/// thread is actually scheduled -- under that much concurrent contention (host free RAM measured
/// as low as ~300-450MB mid-boot this pass) a thread can legitimately sit unscheduled for several
/// real seconds at a time, which is a host-scheduling latency problem, not a defect in the
/// rendezvous protocol itself (confirmed sound, twenty-fourth pass). `15s` keeps this bounded
/// (never the literal-forever hang the original comment guards against) while giving a genuinely
/// slow-but-alive listener real room to get scheduled.
pub(crate) const SHARED_UNIX_CROSS_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Drop-in replacement for [`WaitContext::wait_on_events`] that additionally re-polls on a short
/// bounded timeout instead of relying purely on a real wake -- see this module's own "Shared
/// cross-process AF_UNIX connection data plane" doc comment above for why: nothing in this
/// codebase can deliver a wake from one process's write into a different process's blocked wait.
///
/// Safe to use unconditionally, including for ordinary same-process connections: a real wake
/// still unblocks a waiter immediately (this only ever ADDS an upper bound on how long a call can
/// be stuck with no real wake pending, it never removes or delays the real wake path), and a
/// non-blocking call is unaffected (`wait_on_events` itself returns before ever reaching this
/// function's loop in that case). A genuine caller-supplied deadline (`cx`'s own, e.g. from
/// `SO_RCVTIMEO`) is always honored exactly: this function's own polling interval never widens
/// `cx`'s deadline (`WaitContext::with_timeout` only ever narrows it), and the final iteration
/// before a real deadline uses the real remaining duration verbatim, so that iteration's own
/// `TimedOut` is genuine rather than one of this function's own synthetic sub-waits.
pub(crate) fn wait_on_events_polling<Platform, R, E>(
    cx: &WaitContext<'_, Platform>,
    nonblock: bool,
    events: Events,
    mut register_observer: impl FnMut(
        Weak<dyn litebox::event::observer::Observer<Events>>,
        Events,
    ) -> Result<(), E>,
    mut try_op: impl FnMut() -> Result<R, TryOpError<E>>,
) -> Result<R, TryOpError<E>>
where
    Platform: ShimPlatform,
{
    // Captured ONCE, outside the loop: `WaitContext::remaining_timeout` returns `None` in TWO
    // different situations -- "no deadline was ever set" AND "the deadline has already passed" --
    // so re-deriving "did a real deadline just expire" from a bare `None` inside the loop is
    // ambiguous and was live-caught 2026-09-18 turning a supposedly-3-second-bounded cross-process
    // `connect()` into an unbounded poll loop (once the real deadline passed, `remaining_timeout()`
    // started returning `None` on every subsequent iteration, which this function's own earlier
    // version misread as "no real deadline exists" and kept polling forever). Recording whether a
    // real deadline exists up front, before it can have expired, resolves the ambiguity: `None`
    // afterward can only mean "expired", never "never had one".
    let has_real_deadline = cx.deadline().is_some();
    // The operation is always attempted at least once, even when the deadline has already passed
    // (a zero timeout is a poll, not a refusal to look): only a LATER iteration may report
    // `TimedOut` from an expired deadline.
    let mut first_attempt = true;
    loop {
        let remaining = cx.remaining_timeout();
        if has_real_deadline && remaining.is_none() && !first_attempt {
            return Err(TryOpError::WaitError(
                litebox::event::wait::WaitError::TimedOut,
            ));
        }
        let this_iter_timeout = match remaining {
            None if has_real_deadline => core::time::Duration::ZERO,
            None => SHARED_UNIX_POLL_INTERVAL,
            Some(d) => d.min(SHARED_UNIX_POLL_INTERVAL),
        };
        let bounded = cx.with_timeout(this_iter_timeout);
        first_attempt = false;
        match bounded.wait_on_events(nonblock, events, &mut register_observer, &mut try_op) {
            Err(TryOpError::WaitError(litebox::event::wait::WaitError::TimedOut))
                if !has_real_deadline || remaining.is_some_and(|d| d > this_iter_timeout) =>
            {
                // This was one of our own synthetic sub-wait ticks, not a real caller deadline
                // (either there IS no real deadline, or it's still further away than this tick) --
                // loop around and re-check for a cross-process change.
                continue;
            }
            other => return other,
        }
    }
}

/// Simultaneously-OPEN cross-process AF_UNIX stream connections one guest session can hold.
/// Same bounded-capacity-over-dynamic-growth sizing philosophy as [`UNIX_ADDR_PRESENCE_CAPACITY`]/
/// `MAX_SOCKETS`.
///
/// **Raised 64 -> 1024, 2026-10-03, for one measured reason** (`.wfgy/chr16.err`, run `chr16`):
/// all 1265 of that run's `SCM_RIGHTS: this fd cannot cross a process boundary` refusals carried
/// `reason=shared unix connection table full` -- not one other reason. Chromium establishes each
/// child's Mojo IPC channel by sending it an unnamed `socketpair` endpoint over a unix socket, and
/// carrying one needs a slot here, so with 64 slots no channel is ever established and
/// `--dump-dom` never emits a page. A `LITEBOX_PROCESS_FORK=1` Chromium runs ~20 host processes
/// and opens far more than 64 concurrent channels; 1024 leaves an order of magnitude of headroom
/// over that.
///
/// **Measured demand, not a guess (chr20, 2026-10-03).** Chromium fills 1024 slots in ~7.4s and
/// then refuses every further socket carry (`305` `SCM_RIGHTS` refusals, all
/// `reason=shared unix connection table full`); the census's per-slot sample
/// (`slot 0 client_pid=8 server_pid=8 holders="client[103368x1 132720x1 53696x1 96676x2 43816x1 ]
/// server[42820x1 ]"`) is the shape that demand takes -- ONE connection whose client side is held
/// by SIX host processes, because every cross-process fork hands each process another holder of the
/// same endpoint. So this is saturation by inheritance fan-out, not the never-adopted-carry leak
/// the census was built to detect (that one shows up as the SAME pid counted twice on one side).
/// 4096 is 4x the measured floor and still fits the arena because the slot shrank with it (see
/// [`SHARED_UNIX_CONN_BUF`] and [`RING_FD_MAIL_ENTRIES`]): ~29 MiB of one 128 MiB reservation.
///
/// **Sized for the shared arena now, not for the stack.** The original 64 was a stack-overflow
/// workaround,
/// not a judgement about connection counts: `GlobalState` (which embeds this table) is constructed
/// as an ordinary Rust value and passed BY VALUE through `create_shared_kernel_state`/
/// `SharedArc::new` before being placed in the shared arena, so an oversized inline field here
/// blows the constructing thread's stack before ever reaching the arena at all
/// (`thread 'main' has overflowed its stack`, live-reproduced with this table at 8 MiB total;
/// `syscalls::pty::SHARED_PTY_CAPACITY`'s doc comment records the same hazard reproducing a real
/// `STATUS_STACK_OVERFLOW` at just 32 pty slots / ~214 KiB). 1024 inline slots would be ~15 MiB.
/// [`SharedUnixConnTable`] no longer holds an inline array at all -- it holds one
/// `shared_kernel_arena_alloc_bytes` region of `1024 * size_of::<SharedConnSlot<Platform>>()`
/// (~15 MiB, rounded up to 16 MiB by that allocator's power-of-two sizing, out of a 64 MiB arena),
/// initialized IN PLACE one slot at a time, so no stack value of that size is ever constructed and
/// `GlobalState`'s own by-value footprint DROPS by ~0.9 MiB. The region is shared by the whole
/// fork family, so this is ~15 MiB once per session, not per host process.
///
/// The leak that originally motivated the small-but-bounded pool is still handled by
/// [`SharedUnixConnTable::alloc`]'s dead-holder reclaim pass, and the capacity raise is the
/// complementary mitigation for the case that pass still cannot recover -- see that doc comment.
pub(crate) const SHARED_UNIX_CONN_CAPACITY: usize = 4096;

/// Per-direction shared ring buffer capacity for a cross-process AF_UNIX connection. Bounded like a real kernel AF_UNIX socket's own
/// finite send/receive buffer: this is control-plane protocol traffic (X11/D-Bus requests and
/// replies), not bulk pixel data (MIT-SHM already carries that over System V shared memory, never
/// through this socket). Shared-arena footprint: `SHARED_UNIX_CONN_CAPACITY` slots x
/// `2 * SHARED_UNIX_CONN_BUF` of ring bytes each -- ~15 MiB at 1024 slots, which is
/// [`SHARED_UNIX_CONN_CAPACITY`]'s 16 MiB arena allocation, not `GlobalState`'s own size (the
/// slots are no longer inline there).
/// A single write larger than this never hangs regardless (see [`SharedByteRing::try_write`] and
/// its call site in `SharedView::send`): it degrades to a real short write
/// of the first `SHARED_UNIX_CONN_BUF` bytes, matching a real kernel socket's own short-write
/// behaviour on an over-sized single `write(2)`, rather than looping on `EAGAIN` forever.
///
/// Halved from 4096 so that 4096 slots (see [`SHARED_UNIX_CONN_CAPACITY`]) still fit one 32 MiB
/// power-of-two arena region: the ring bytes are the slot's whole footprint, and this is a
/// control-plane buffer whose only cost when too small is more event-driven short-write round
/// trips (the writer registers `blocked_need` and the reader wakes it -- never a fixed poll).
const SHARED_UNIX_CONN_BUF: usize = 2048;

/// The pty data plane's own ring size: `syscalls::pty` reuses [`SharedByteRing`] verbatim, and a
/// terminal carries bulk output (a `cat` of a large file, a full-screen redraw), so it keeps the
/// bigger buffer the AF_UNIX control plane gave up to buy connection slots.
pub(crate) const PTY_RING_BYTES: usize = 4096;

const CONN_SLOT_EMPTY: u32 = 0;
const CONN_SLOT_OCCUPIED: u32 = 1;

/// One direction's worth of shared byte transport: a fixed ring buffer plus its read/write
/// cursors, all guarded by one cross-process `RawMutex`-backed lock (same primitive
/// `GlobalStateHandle::net_lock` already uses for `Network`) so concurrent same-direction access
/// from either side's process is always serialized through real, dereferenceable local memory --
/// no raw pointer is ever handed from one process to another; each side only ever touches its OWN
/// copy of the `Arc`-free, inline-in-the-shared-arena bytes.
/// `pub(crate)`, not module-private: reused directly (per this codebase's own "reuse
/// `SharedByteRing`/`SharedArc`/the shared kernel arena infrastructure directly, don't reinvent"
/// convention) by `syscalls::pty::SharedPtyTable` for the cross-process pty master<->slave byte
/// data plane -- the exact same shape (a fixed ring plus cross-process `RawMutex`-backed cursor)
/// AF_UNIX already proved sound here, just keyed by pty id instead of a connection slot.
pub(crate) struct SharedByteRing<Platform: ShimPlatform, const RING_BYTES: usize> {
    cursor: Mutex<Platform, RingCursor>,
    buf: [AtomicU8; RING_BYTES],
}

#[derive(Clone, Copy)]
struct RingCursor {
    write_pos: usize,
    read_pos: usize,
    /// Set once the writing side calls `shutdown`/is dropped. Mirrors `channel::EndPointer`'s own
    /// `is_shutdown` -- queued bytes remain readable after this is set; a reader only observes
    /// EOF once `write_pos == read_pos` as well.
    write_shutdown: bool,
    fd_mail: [RingFdMail; RING_FD_MAIL_ENTRIES],
    blocked_need: usize,
    blocked_fd: bool,
}

impl Default for RingCursor {
    fn default() -> Self {
        Self {
            write_pos: 0,
            read_pos: 0,
            write_shutdown: false,
            fd_mail: [RingFdMail::EMPTY; RING_FD_MAIL_ENTRIES],
            blocked_need: 0,
            blocked_fd: false,
        }
    }
}

const RING_FD_MAIL_ENTRIES: usize = 2;
const RING_FD_MAIL_SPEC_BYTES: usize = 640;

/// Separates the per-descriptor specs inside one [`RingFdMail`]; never occurs in a path.
pub(crate) const RING_FD_SPEC_SEPARATOR: char = '\u{1e}';

/// Descriptors sent with the message whose first byte sits at stream position `offset`, described
/// as text specs the receiving process rebuilds by name (file path, eventfd state) because a
/// descriptor-table handle cannot cross a process boundary.
#[derive(Clone, Copy)]
struct RingFdMail {
    used: bool,
    offset: usize,
    len: usize,
    spec: [u8; RING_FD_MAIL_SPEC_BYTES],
}

impl RingFdMail {
    const EMPTY: Self = Self {
        used: false,
        offset: 0,
        len: 0,
        spec: [0; RING_FD_MAIL_SPEC_BYTES],
    };
}

impl RingCursor {
    fn post_fds(&mut self, offset: usize, specs: &str) -> bool {
        if specs.len() > RING_FD_MAIL_SPEC_BYTES {
            return false;
        }
        let Some(slot) = self.fd_mail.iter_mut().find(|m| !m.used) else {
            return false;
        };
        slot.used = true;
        slot.offset = offset;
        slot.len = specs.len();
        slot.spec[..specs.len()].copy_from_slice(specs.as_bytes());
        true
    }

    /// Bytes readable before the next descriptor-bearing message starts, so one read never
    /// swallows the start of a second message that carries descriptors.
    fn readable_before_next_fd_mail(&self, avail: usize) -> usize {
        self.fd_mail
            .iter()
            .filter(|m| m.used)
            .map(|m| m.offset.wrapping_sub(self.read_pos))
            .filter(|distance| *distance > 0 && *distance < avail)
            .min()
            .unwrap_or(avail)
    }

    fn take_fds_in(&mut self, start: usize, consumed: usize) -> Vec<String> {
        let mut out = Vec::new();
        for mail in self.fd_mail.iter_mut().filter(|m| m.used) {
            if mail.offset.wrapping_sub(start) < consumed {
                mail.used = false;
                if let Ok(text) = core::str::from_utf8(&mail.spec[..mail.len]) {
                    out.extend(text.split(RING_FD_SPEC_SEPARATOR).map(String::from));
                }
            }
        }
        out
    }
}

impl<Platform: ShimPlatform, const RING_BYTES: usize> SharedByteRing<Platform, RING_BYTES> {
    pub(crate) fn new_empty() -> Self {
        Self {
            cursor: Mutex::new(RingCursor::default()),
            buf: core::array::from_fn(|_| AtomicU8::new(0)),
        }
    }

    pub(crate) fn reset(&self) {
        *self.cursor.lock() = RingCursor::default();
    }

    /// All-or-nothing write, mirroring `channel::WriteEnd::try_write_one`'s own contract (that
    /// pushes one whole `Message` or fails with the ring untouched) so callers don't need to
    /// track a partial-write remainder across calls: either every byte of `data` fits right now
    /// and is written, or NONE of it is and the buffer is left exactly as it was. Never blocks,
    /// never panics.
    pub(crate) fn try_write_all(&self, data: &[u8]) -> bool {
        self.try_write_all_with_fds(data, None)
    }

    /// [`Self::try_write_all`], posting `fd_specs` (see [`RingFdMail`]) against the first byte.
    pub(crate) fn try_write_all_with_fds(&self, data: &[u8], fd_specs: Option<&str>) -> bool {
        let mut cursor = self.cursor.lock();
        if cursor.write_shutdown {
            return false;
        }
        let used = cursor.write_pos.wrapping_sub(cursor.read_pos);
        let free = RING_BYTES - used;
        if data.len() > free {
            cursor.blocked_need = data.len();
            cursor.blocked_fd = fd_specs.is_some();
            return false;
        }
        if let Some(specs) = fd_specs {
            let at = cursor.write_pos;
            if !cursor.post_fds(at, specs) {
                cursor.blocked_need = data.len();
                cursor.blocked_fd = true;
                return false;
            }
        }
        for (i, b) in data.iter().enumerate() {
            self.buf[(cursor.write_pos.wrapping_add(i)) % RING_BYTES]
                .store(*b, Ordering::Relaxed);
        }
        cursor.write_pos = cursor.write_pos.wrapping_add(data.len());
        cursor.blocked_need = 0;
        cursor.blocked_fd = false;
        true
    }

    pub(crate) fn writable(&self) -> bool {
        let cursor = self.cursor.lock();
        let free = RING_BYTES - cursor.write_pos.wrapping_sub(cursor.read_pos);
        free > 0
            && free >= cursor.blocked_need
            && (!cursor.blocked_fd || cursor.fd_mail.iter().any(|m| !m.used))
    }

    /// Partial write: writes as many LEADING bytes of `data` as currently fit, returning the
    /// count (which may be `0` if the ring is full or shut down -- never blocks, never panics).
    /// Only used for the one case [`Self::try_write_all`] can never resolve no matter how empty
    /// the ring is -- a single message bigger than the whole ring -- see
    /// [`RING_BYTES`]'s doc comment.
    pub(crate) fn try_write(&self, data: &[u8]) -> usize {
        self.try_write_with_fds(data, None)
    }

    /// [`Self::try_write`], posting `fd_specs` against the first byte written.
    pub(crate) fn try_write_with_fds(&self, data: &[u8], fd_specs: Option<&str>) -> usize {
        if data.is_empty() {
            return 0;
        }
        let mut cursor = self.cursor.lock();
        if cursor.write_shutdown {
            return 0;
        }
        let used = cursor.write_pos.wrapping_sub(cursor.read_pos);
        let free = RING_BYTES - used;
        let n = data.len().min(free);
        if n == 0 {
            return 0;
        }
        if let Some(specs) = fd_specs {
            let at = cursor.write_pos;
            if !cursor.post_fds(at, specs) {
                return 0;
            }
        }
        for (i, b) in data.iter().take(n).enumerate() {
            self.buf[(cursor.write_pos.wrapping_add(i)) % RING_BYTES]
                .store(*b, Ordering::Relaxed);
        }
        cursor.write_pos = cursor.write_pos.wrapping_add(n);
        n
    }

    /// Like [`Self::try_read`] but leaves the bytes queued (`MSG_PEEK`).
    pub(crate) fn try_peek(&self, out: &mut [u8]) -> usize {
        let cursor = self.cursor.lock();
        let avail = cursor.write_pos.wrapping_sub(cursor.read_pos);
        let n = out.len().min(cursor.readable_before_next_fd_mail(avail));
        for (i, slot) in out.iter_mut().take(n).enumerate() {
            *slot = self.buf[(cursor.read_pos.wrapping_add(i)) % RING_BYTES]
                .load(Ordering::Relaxed);
        }
        n
    }

    /// Reads up to `out.len()` bytes; returns the count read. `0` is ambiguous between "empty and
    /// still open" and "empty and shut down" by design -- callers distinguish via
    /// [`Self::is_shutdown`]/[`Self::is_empty`], mirroring `channel::ReadEnd::peek_and_consume_one`'s
    /// own EAGAIN-vs-ESHUTDOWN split.
    pub(crate) fn try_read(&self, out: &mut [u8]) -> usize {
        self.try_read_with_fds(out).0
    }

    /// [`Self::try_read`], also returning the descriptor specs posted against the bytes read. A
    /// read stops short of the start of a later descriptor-bearing message.
    pub(crate) fn try_read_with_fds(&self, out: &mut [u8]) -> (usize, Vec<String>) {
        let mut cursor = self.cursor.lock();
        let avail = cursor.write_pos.wrapping_sub(cursor.read_pos);
        let n = out.len().min(cursor.readable_before_next_fd_mail(avail));
        for (i, slot) in out.iter_mut().take(n).enumerate() {
            *slot = self.buf[(cursor.read_pos.wrapping_add(i)) % RING_BYTES]
                .load(Ordering::Relaxed);
        }
        let start = cursor.read_pos;
        cursor.read_pos = cursor.read_pos.wrapping_add(n);
        let specs = if n > 0 {
            cursor.take_fds_in(start, n)
        } else {
            Vec::new()
        };
        (n, specs)
    }

    pub(crate) fn is_empty(&self) -> bool {
        let cursor = self.cursor.lock();
        cursor.write_pos == cursor.read_pos
    }

    pub(crate) fn free_space(&self) -> usize {
        let cursor = self.cursor.lock();
        RING_BYTES - cursor.write_pos.wrapping_sub(cursor.read_pos)
    }

    /// Writes `data` as one record -- a 4-byte little-endian length, then the bytes -- all or
    /// nothing. The message-boundary-preserving form a promoted `SOCK_SEQPACKET` connection uses.
    pub(crate) fn try_write_record(&self, data: &[u8]) -> bool {
        self.try_write_record_with_fds(data, None)
    }

    pub(crate) fn try_write_record_with_fds(&self, data: &[u8], fd_specs: Option<&str>) -> bool {
        let Ok(len) = u32::try_from(data.len()) else {
            return false;
        };
        let mut framed = Vec::with_capacity(4 + data.len());
        framed.extend_from_slice(&len.to_le_bytes());
        framed.extend_from_slice(data);
        self.try_write_all_with_fds(&framed, fd_specs)
    }

    /// Consumes one whole record written by [`Self::try_write_record`], copying at most
    /// `out.len()` bytes of it (the rest is discarded, as a datagram read truncates). `None` when
    /// no complete record is queued.
    pub(crate) fn try_read_record(&self, out: &mut [u8]) -> Option<usize> {
        self.try_read_record_with_fds(out).map(|(n, _)| n)
    }

    pub(crate) fn try_read_record_with_fds(&self, out: &mut [u8]) -> Option<(usize, Vec<String>)> {
        let mut cursor = self.cursor.lock();
        let avail = cursor.write_pos.wrapping_sub(cursor.read_pos);
        if avail < 4 {
            return None;
        }
        let at = |cursor: &RingCursor, i: usize| {
            self.buf[(cursor.read_pos.wrapping_add(i)) % RING_BYTES].load(Ordering::Relaxed)
        };
        let len = u32::from_le_bytes([at(&cursor, 0), at(&cursor, 1), at(&cursor, 2), at(&cursor, 3)])
            as usize;
        if avail < 4 + len {
            return None;
        }
        let n = out.len().min(len);
        for (i, b) in out.iter_mut().take(n).enumerate() {
            *b = at(&cursor, 4 + i);
        }
        let start = cursor.read_pos;
        cursor.read_pos = cursor.read_pos.wrapping_add(4 + len);
        let specs = cursor.take_fds_in(start, 4 + len);
        Some((n, specs))
    }

    /// Like [`Self::try_read_record`] but leaves the record queued (`MSG_PEEK`).
    pub(crate) fn try_peek_record(&self, out: &mut [u8]) -> Option<usize> {
        let cursor = self.cursor.lock();
        let avail = cursor.write_pos.wrapping_sub(cursor.read_pos);
        if avail < 4 {
            return None;
        }
        let at = |i: usize| {
            self.buf[(cursor.read_pos.wrapping_add(i)) % RING_BYTES].load(Ordering::Relaxed)
        };
        let len = u32::from_le_bytes([at(0), at(1), at(2), at(3)]) as usize;
        if avail < 4 + len {
            return None;
        }
        let n = out.len().min(len);
        for (i, b) in out.iter_mut().take(n).enumerate() {
            *b = at(4 + i);
        }
        Some(n)
    }

    pub(crate) fn is_full(&self) -> bool {
        let cursor = self.cursor.lock();
        cursor.write_pos.wrapping_sub(cursor.read_pos) >= RING_BYTES
    }

    pub(crate) fn shutdown(&self) {
        self.cursor.lock().write_shutdown = true;
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.cursor.lock().write_shutdown
    }
}

/// One allocated cross-process connection: two independent [`SharedByteRing`]s (client-to-server,
/// server-to-client) plus enough bookkeeping for `SO_PEERCRED` on both sides. Sized/guarded
/// exactly like [`UnixAddrPresenceSlot`] -- deliberately no `Vec`/`Box`/pointer field anywhere,
/// so the whole slot's live state is its own inline bytes.
struct SharedConnSlot<Platform: ShimPlatform> {
    state: AtomicU32,
    client_to_server: SharedByteRing<Platform, SHARED_UNIX_CONN_BUF>,
    server_to_client: SharedByteRing<Platform, SHARED_UNIX_CONN_BUF>,
    client_pid: AtomicU32,
    client_uid: AtomicU32,
    client_gid: AtomicU32,
    server_pid: AtomicU32,
    server_uid: AtomicU32,
    server_gid: AtomicU32,
    /// Endpoints holding each side (index 0 = client, 1 = server), counted per host process
    /// (`holder_hosts[side][i]` holds `holder_counts[side][i]` of them): after a cross-process
    /// fork the parent and child both hold the same side, and a side ends only when every holder
    /// is gone -- or its host process died without dropping it.
    holder_hosts: [[AtomicU32; CONN_HOLDER_HOSTS]; 2],
    holder_counts: [[AtomicU32; CONN_HOLDER_HOSTS]; 2],
    side_ever_held: [AtomicBool; 2],
    /// The rings carry length-prefixed records (a promoted `SOCK_SEQPACKET` connection).
    framed: AtomicBool,
    /// Credentials of whichever process most recently sent in each direction -- what
    /// `SCM_CREDENTIALS` reports (a forked or `SCM_RIGHTS`-passed endpoint sends from a process
    /// other than the one that connected, and the receiver must see the sender).
    last_writer_c2s: [AtomicU32; 3],
    last_writer_s2c: [AtomicU32; 3],
}

/// Distinct host processes that can hold one side of a shared connection at once. A cross-process
/// fork gives the SAME side to the parent and to every child that inherits the endpoint, and a
/// process that dies without releasing keeps its slot until something reads this connection, so
/// this is sized for a real session's fork fan-out rather than for the two ends of a socketpair.
const CONN_HOLDER_HOSTS: usize = 32;

fn conn_side(is_client: bool) -> usize {
    if is_client { 0 } else { 1 }
}

impl<Platform: ShimPlatform> SharedConnSlot<Platform> {
    /// Counts `host` as holding `is_client`'s side of this connection.
    ///
    /// `h` is claimed BEFORE `c` is raised, never after. The other order leaves a window in which
    /// the slot shows a live count against the PREVIOUS host's pid, and `side_gone` -- a lazy,
    /// read-time liveness check any host thread can run at any moment -- then compares that stale
    /// pid against reality, finds it dead, and zeroes the count. That drops a holder which is in
    /// fact alive, the worst outcome this bookkeeping can produce: the side is later declared gone
    /// while a live process still holds it, and the peer gets an EOF Linux would never give it.
    /// Live-caught in `chrF2.err` (18 `this holder is not counted` warnings, `comm=chromium`
    /// taking crashpad's `IMMEDIATE_CRASH()` int3 when its `sendmsg` to the handler's end fails).
    fn hold(&self, is_client: bool, host: u32, platform: &Platform) {
        let side = conn_side(is_client);
        self.side_ever_held[side].store(true, Ordering::Release);
        let hosts = &self.holder_hosts[side];
        let counts = &self.holder_counts[side];
        for (h, c) in hosts.iter().zip(counts) {
            if h.load(Ordering::Acquire) == host && c.load(Ordering::Acquire) > 0 {
                c.fetch_add(1, Ordering::AcqRel);
                return;
            }
        }
        for (h, c) in hosts.iter().zip(counts) {
            if c.load(Ordering::Acquire) != 0 {
                continue;
            }
            let prev = h.load(Ordering::Acquire);
            if h.compare_exchange(prev, host, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                c.fetch_add(1, Ordering::AcqRel);
                return;
            }
        }
        // Every slot is taken by a host that is still counted. A host process that died without
        // releasing keeps its slot until somebody happens to read this connection and `side_gone`
        // notices, so those are reclaimable right here: evict one rather than drop the live holder
        // that would otherwise turn into a spurious EOF for the peer.
        for (h, c) in hosts.iter().zip(counts) {
            let prev = h.load(Ordering::Acquire);
            if c.load(Ordering::Acquire) == 0 || prev == host {
                continue;
            }
            if !platform.is_process_alive(prev)
                && h.compare_exchange(prev, host, Ordering::AcqRel, Ordering::Acquire).is_ok()
            {
                c.store(1, Ordering::Release);
                return;
            }
        }
        litebox_util_log::warn!(
            host:% = host, is_client:% = is_client;
            "shared unix connection: more host processes hold one side than tracked; this \
             holder is not counted"
        );
    }

    fn release(&self, is_client: bool, host: u32) {
        let side = conn_side(is_client);
        for (h, c) in self.holder_hosts[side].iter().zip(&self.holder_counts[side]) {
            if h.load(Ordering::Acquire) == host
                && c.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                    .is_ok()
            {
                return;
            }
        }
    }

    fn holder_dump(&self) -> alloc::string::String {
        let mut out = alloc::string::String::new();
        for side in 0..2 {
            out.push_str(if side == 0 { "client[" } else { " server[" });
            for (h, c) in self.holder_hosts[side].iter().zip(&self.holder_counts[side]) {
                let n = c.load(Ordering::Acquire);
                if n > 0 {
                    out.push_str(&alloc::format!("{}x{} ", h.load(Ordering::Acquire), n));
                }
            }
            out.push(']');
        }
        out
    }

    /// The side was held and no live host process holds it any more.
    fn side_gone(&self, is_client: bool, platform: &Platform) -> bool {
        let side = conn_side(is_client);
        if !self.side_ever_held[side].load(Ordering::Acquire) {
            return false;
        }
        let me = platform.current_host_pid();
        let mut any = false;
        for (h, c) in self.holder_hosts[side].iter().zip(&self.holder_counts[side]) {
            if c.load(Ordering::Acquire) == 0 {
                continue;
            }
            let host = h.load(Ordering::Acquire);
            if host == me || platform.is_process_alive(host) {
                any = true;
            } else {
                // 118th-pass investigation (AGENTS.md "Where things stand"): this is the ONLY
                // place a side can go "gone" without either `release_holder` or `shutdown_write`
                // ever running -- a lazy, read-time liveness check, not a reaction to an event.
                // Two earlier capture rounds with those two call sites logged found neither fired
                // near a session client's death, so THIS is the real mechanism to observe: which
                // host pid gets declared dead, and (checked independently against `Get-Process`
                // on the host at capture time) whether it actually was.
                litebox_util_log::__private::tracing::event!(
                    target: "litebox_diag::unix_conn_teardown",
                    litebox_util_log::__private::tracing::Level::DEBUG,
                    is_client = %is_client,
                    dead_host_pid = %host,
                    checking_host_pid = %me,
                    "DIAG unix shared conn: side_gone() declared a holder host dead on a lazy read-time check"
                );
                c.store(0, Ordering::Release);
            }
        }
        !any
    }

    /// Calls `f` with every distinct host process currently holding `is_client`'s side.
    fn for_each_holder_host(&self, is_client: bool, mut f: impl FnMut(u32)) {
        let side = conn_side(is_client);
        for (h, c) in self.holder_hosts[side].iter().zip(&self.holder_counts[side]) {
            if c.load(Ordering::Acquire) > 0 {
                f(h.load(Ordering::Acquire));
            }
        }
    }

    /// Marks a side as held once and already gone: its endpoint closed before the connection was
    /// promoted to this slot.
    fn mark_side_gone(&self, is_client: bool) {
        self.side_ever_held[conn_side(is_client)].store(true, Ordering::Release);
    }

    fn reset_holders(&self) {
        for side in 0..2 {
            for c in &self.holder_counts[side] {
                c.store(0, Ordering::Release);
            }
            self.side_ever_held[side].store(false, Ordering::Release);
        }
        self.framed.store(false, Ordering::Release);
    }

    fn new_empty() -> Self {
        Self {
            state: AtomicU32::new(CONN_SLOT_EMPTY),
            client_to_server: SharedByteRing::new_empty(),
            server_to_client: SharedByteRing::new_empty(),
            client_pid: AtomicU32::new(0),
            client_uid: AtomicU32::new(0),
            client_gid: AtomicU32::new(0),
            server_pid: AtomicU32::new(0),
            server_uid: AtomicU32::new(0),
            server_gid: AtomicU32::new(0),
            holder_hosts: core::array::from_fn(|_| core::array::from_fn(|_| AtomicU32::new(0))),
            holder_counts: core::array::from_fn(|_| core::array::from_fn(|_| AtomicU32::new(0))),
            side_ever_held: [AtomicBool::new(false), AtomicBool::new(false)],
            framed: AtomicBool::new(false),
            last_writer_c2s: [const { AtomicU32::new(0) }; 3],
            last_writer_s2c: [const { AtomicU32::new(0) }; 3],
        }
    }

    fn set_last_writer(&self, from_client: bool, cred: Ucred) {
        let w = if from_client {
            &self.last_writer_c2s
        } else {
            &self.last_writer_s2c
        };
        w[0].store(cred.pid as u32, Ordering::Relaxed);
        w[1].store(cred.uid, Ordering::Relaxed);
        w[2].store(cred.gid, Ordering::Relaxed);
    }

    fn last_writer(&self, from_client: bool) -> Option<Ucred> {
        let w = if from_client {
            &self.last_writer_c2s
        } else {
            &self.last_writer_s2c
        };
        let pid = w[0].load(Ordering::Relaxed);
        (pid != 0).then(|| Ucred {
            pid: pid as _,
            uid: w[1].load(Ordering::Relaxed),
            gid: w[2].load(Ordering::Relaxed),
        })
    }

    fn server_cred(&self) -> Ucred {
        Ucred {
            pid: self.server_pid.load(Ordering::Relaxed) as u32,
            uid: self.server_uid.load(Ordering::Relaxed),
            gid: self.server_gid.load(Ordering::Relaxed),
        }
    }
}

/// Fixed pool of [`SharedConnSlot`]s -- the shared-arena-native backing for every currently-open
/// cross-process AF_UNIX stream connection, allocated by a listener's `accept()` and released
/// when the last side referencing it drops.
///
/// # The slots live in the shared kernel arena, not inline in `GlobalState`
///
/// A `[SharedConnSlot; SHARED_UNIX_CONN_CAPACITY]` field would be ~15 MiB at the current capacity,
/// and `GlobalState` (which embeds this table) is built BY VALUE on the constructing thread's stack
/// before `create_shared_kernel_state` moves it into the arena -- see
/// [`SHARED_UNIX_CONN_CAPACITY`]'s doc comment for the live-reproduced `STATUS_STACK_OVERFLOW`
/// that constrains it. So the pool is one `shared_kernel_arena_alloc_bytes` region instead, held
/// here as a `&'static mut` slice over it -- the same shape `litebox::net` already uses for
/// `Network::socket_set`'s own `&'static mut [SocketStorage]`, and sound for the same reasons:
///
/// - **Same address in every host process.** That allocator hands back memory from the fixed-base
///   shared arena, so the pointer value is directly valid in every process of the fork family (a
///   cross-process-fork child ATTACHES to this already-built `GlobalState` rather than calling
///   [`Self::new`] at all, so the pointer the family shares is the one this process stored).
/// - **Never reclaimed**, per that allocator's own contract, so `'static` is honest.
/// - **Flat and pointer-free**, like every other shared struct here: `SharedConnSlot` is atomics
///   plus two [`SharedByteRing`]s, no heap, so a second process dereferencing these bytes sees the
///   same objects the first one does (the `GlobalState`-pointer-sharing defect class).
///
/// The one cost of moving it out of `GlobalState`'s inline bytes is that the region must be
/// initialized through the pointer -- which [`Self::new`] does one slot at a time, so the largest
/// value ever built on the stack is a single ~14.5 KiB slot.
pub(crate) struct SharedUnixConnTable<Platform: ShimPlatform> {
    slots: &'static mut [SharedConnSlot<Platform>],
    /// `false` only when the arena allocation itself failed and [`Self::new`] fell back to a
    /// process-private one: such a table can never serve a genuinely cross-process connection
    /// (another process has no way to reach those bytes), so [`Self::alloc`] refuses everything
    /// instead of handing out indices into memory no peer can see. Degrades every cross-process
    /// AF_UNIX attempt to its ordinary errno path; never a panic.
    arena_backed: bool,
}

impl<Platform: ShimPlatform> SharedUnixConnTable<Platform> {
    /// Allocates the pool in the cross-process SHARED kernel arena and initializes every slot.
    ///
    /// Runs exactly once per fork family, from `LinuxShimBuilder::build`'s create branch -- every
    /// other process in the family attaches to the already-built `GlobalState` and never calls
    /// this (the same create-vs-attach split `litebox::net::alloc_shared_socket_storage`'s own doc
    /// comment describes for `Network::socket_set`).
    pub(crate) fn new(platform: &Platform) -> Self {
        let layout =
            core::alloc::Layout::array::<SharedConnSlot<Platform>>(SHARED_UNIX_CONN_CAPACITY)
                .expect("SHARED_UNIX_CONN_CAPACITY slot-array layout computation cannot overflow");
        let arena = platform.shared_kernel_arena_alloc_bytes(layout);
        let arena_backed = arena.is_some();
        let ptr = arena
            .unwrap_or_else(|| {
                // Arena exhausted: still never a panic (AGENTS.md's standing rule -- the host
                // process IS the whole guest session). Fall back to a leaked process-private
                // allocation so every later `get`/`free` stays memory-safe, and let `alloc`
                // refuse everything via `arena_backed`.
                litebox_util_log::error!(
                    capacity:% = SHARED_UNIX_CONN_CAPACITY,
                    bytes:% = layout.size();
                    "shared unix connection table: shared kernel arena exhausted; cross-process \
                     AF_UNIX is disabled in this process"
                );
                // SAFETY: `layout` has a non-zero size (`SHARED_UNIX_CONN_CAPACITY` slots).
                core::ptr::NonNull::new(unsafe { alloc::alloc::alloc_zeroed(layout) })
                    .expect("shared unix connection table: fallback allocation failed")
            })
            .cast::<SharedConnSlot<Platform>>();
        for i in 0..SHARED_UNIX_CONN_CAPACITY {
            // SAFETY: `ptr` names `SHARED_UNIX_CONN_CAPACITY` contiguous, uninitialized
            // `SharedConnSlot`s per `layout`, so `add(i)` stays inside that region for every
            // `i < SHARED_UNIX_CONN_CAPACITY`, and `write`-ing a freshly built value into
            // uninitialized memory (rather than dropping a prior one) is exactly what `write` is
            // for. Writing them ONE AT A TIME through the pointer is the entire point of this
            // function: `SharedConnSlot::new_empty()` is ~14.5 KiB of stack at a time, where a
            // `[SharedConnSlot; SHARED_UNIX_CONN_CAPACITY]` value would be ~15 MiB of it.
            unsafe {
                ptr.as_ptr().add(i).write(SharedConnSlot::new_empty());
            }
        }
        Self {
            // SAFETY: `ptr` is non-null, aligned per `layout`, and all `SHARED_UNIX_CONN_CAPACITY`
            // slots at it were just initialized by the loop above. `'static` is sound because this
            // allocation is arena-backed and never reclaimed (or, on the fallback path, a leaked
            // global-allocator allocation), and nothing else holds a reference to it, so handing
            // out an exclusive `&'static mut` is sound.
            slots: unsafe {
                core::slice::from_raw_parts_mut(ptr.as_ptr(), SHARED_UNIX_CONN_CAPACITY)
            },
            arena_backed,
        }
    }

    /// Claims a free slot and fills it in; returns its index, or `None` if every slot is in use
    /// and none can be reclaimed (degrades to `ECONNREFUSED`/`EAGAIN` at the call site, never
    /// panics).
    ///
    /// `platform` is used only for the dead-holder reclaim pass below (see [`Self::
    /// reclaim_dead_slot`]) and its failure-path occupancy census ([`Self::log_full`]) -- never for
    /// the ordinary fast path, which stays exactly as cheap as before.
    fn alloc(&self, platform: &Platform, client_cred: &Ucred, server_cred: &Ucred) -> Option<u32> {
        if !self.arena_backed {
            // [`Self::new`] could not place the pool in the shared arena, so no index this table
            // could hand out would name memory another host process can see.
            return None;
        }
        if let Some(idx) = self.try_claim_empty(client_cred, server_cred) {
            return Some(idx);
        }
        // No EMPTY slot on the fast path -- before degrading the caller to `ECONNREFUSED`/
        // `EAGAIN`, check whether any OCCUPIED slot is actually orphaned: live-caught 2026-09-18
        // (Track B, twentieth pass), every `LITEBOX_PROCESS_FORK=1` client caught stuck in
        // `sys_ppoll` on a cross-process AF_UNIX connection (via `cdb -pv`, confirming the AF_UNIX
        // bounded-15ms repoll this codebase already added to `PollSet::wait` IS engaged --
        // `has_unwakeable_fd=true`/`register=false` live in the debugger -- so the repoll itself
        // is not the defect) was eventually killed by the boot script's own timeout rather than
        // exiting normally. A killed process never runs `Drop`, so its
        // `ConnTransport::Shared`/`UnixConnectedStream`'s `free()` call never happens either --
        // this table's fixed pool silently, permanently loses one slot per such kill, with no
        // prior recovery path at all. Reclaim requires BOTH the client's and the server's owning
        // process to be confirmed dead ([`litebox::platform::SystemInfoProvider::
        // is_process_alive`]) -- deliberately conservative: reclaiming a slot a still-live process
        // (e.g. a long-lived listener like Xvfb, which normally outlives any one client) still
        // holds a reference to would hand a live user's connection state out from under it, a
        // strictly worse bug than the leak this fixes. This therefore does not recover every leak
        // (a slot whose long-lived server side never dies is never reclaimed this way -- see
        // [`SHARED_UNIX_CONN_CAPACITY`]'s own doc comment for why the capacity was also raised, as
        // the complementary mitigation for exactly that remaining case), but is unconditionally
        // safe: it never disrupts a slot either endpoint might still be using.
        for slot in self.slots.iter() {
            if self.reclaim_dead_slot(slot, platform)
                && let Some(idx) = self.try_claim_empty(client_cred, server_cred)
            {
                return Some(idx);
            }
        }
        // Still nothing: log a strided occupancy census so the next session can tell genuine
        // saturation from a leak (see [`Self::log_full`]).
        self.log_full(platform);
        None
    }

    /// How many failed [`Self::alloc`] calls go by between two occupancy censuses
    /// ([`Self::log_full`]). A Chromium-scale flood emits thousands of these per run (1265 in
    /// `chr16`), so logging each one would produce a gigabyte of log carrying one line's worth of
    /// information -- the same sampling discipline this codebase already applies to repeated heals
    /// (`MAX_AV_PATH_HEALS`/`AV_HEAL_LOG_SAMPLE_STRIDE`).
    const FULL_LOG_STRIDE: u64 = 64;

    /// Occupancy census on [`Self::alloc`]'s failure path, in three buckets:
    ///
    /// - `held_live`: occupied, and at least one side is still held by a live host process.
    /// - `orphaned`: occupied, every holder is dead -- i.e. reclaimable. Normally 0 here, since the
    ///   reclaim pass above already freed those.
    /// - `unheld`: occupied but NO side was ever held. These are the permanently lost ones:
    ///   [`SharedConnSlot::side_gone`] deliberately reports "not gone" for a side that was never
    ///   held, so [`Self::reclaim_dead_slot`] can never free them. A process that died between
    ///   `try_claim_empty` setting `OCCUPIED` and its own `hold()` leaves exactly one of these.
    ///
    /// Without this, the only evidence a full table leaves is
    /// `reason=shared unix connection table full` repeated thousands of times, which says nothing
    /// about WHY: `chr16` (2026-10-02) logged 1265 identical refusals and left the next session
    /// unable to distinguish "1024 genuinely open connections" from "1024 leaked slots" -- two
    /// cases with completely different fixes.
    fn log_full(&self, platform: &Platform) {
        static STRIDE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        if STRIDE.fetch_add(1, Ordering::Relaxed) % Self::FULL_LOG_STRIDE != 0 {
            return;
        }
        let mut occupied = 0usize;
        let mut held_live = 0usize;
        let mut orphaned = 0usize;
        let mut unheld = 0usize;
        // `one_host` vs `multi_host` is the saturation-vs-leak discriminator the census was
        // missing: a slot BOTH of whose sides are held by the SAME host process is a connection
        // no second process ever took the other end of -- the signature of a carried endpoint
        // whose receiver never adopted it (its placeholder hold stays counted under the sender's
        // host pid forever). `multi_host` slots are what a real cross-process connection looks
        // like. `chr18` (2026-10-03) hit `occupied=1024 held_live=1024 orphaned=0 unheld=0` and
        // this split is what tells whether that is 1024 live channels (raise the capacity) or
        // 1024 forgotten ones (fix the release path).
        let mut one_host = 0usize;
        let mut multi_host = 0usize;
        let first_census = STRIDE.load(Ordering::Relaxed) == 1;
        let mut samples = 0usize;
        for (idx, slot) in self.slots.iter().enumerate() {
            if slot.state.load(Ordering::Acquire) != CONN_SLOT_OCCUPIED {
                continue;
            }
            occupied += 1;
            let mut hosts = [0u32; 2 * CONN_HOLDER_HOSTS];
            let mut hosts_n = 0usize;
            for side in 0..2 {
                for (h, c) in slot.holder_hosts[side].iter().zip(&slot.holder_counts[side]) {
                    if c.load(Ordering::Acquire) == 0 {
                        continue;
                    }
                    let host = h.load(Ordering::Acquire);
                    if hosts[..hosts_n].contains(&host) {
                        continue;
                    }
                    if let Some(dst) = hosts.get_mut(hosts_n) {
                        *dst = host;
                        hosts_n += 1;
                    }
                }
            }
            if hosts_n == 1 {
                one_host += 1;
            } else if hosts_n > 1 {
                multi_host += 1;
            }
            if first_census && samples < 6 {
                samples += 1;
                litebox_util_log::warn!(
                    slot:% = idx,
                    client_pid:% = slot.client_pid.load(Ordering::Relaxed),
                    server_pid:% = slot.server_pid.load(Ordering::Relaxed),
                    hosts_n:% = hosts_n,
                    holders = slot.holder_dump();
                    "shared unix connection table full: occupied slot sample"
                );
            }
            if !slot.side_ever_held[0].load(Ordering::Acquire)
                && !slot.side_ever_held[1].load(Ordering::Acquire)
            {
                unheld += 1;
            } else if slot.side_gone(true, platform) && slot.side_gone(false, platform) {
                orphaned += 1;
            } else {
                held_live += 1;
            }
        }
        litebox_util_log::warn!(
            capacity:% = SHARED_UNIX_CONN_CAPACITY,
            slot_bytes:% = core::mem::size_of::<SharedConnSlot<Platform>>(),
            arena_backed:% = self.arena_backed,
            occupied:% = occupied,
            held_live:% = held_live,
            one_host:% = one_host,
            multi_host:% = multi_host,
            orphaned:% = orphaned,
            unheld:% = unheld;
            "shared unix connection table full"
        );
    }

    /// Fast-path claim: atomically takes the first `EMPTY` slot found and fills it in for a new
    /// connection. Split out of [`Self::alloc`] so its dead-holder reclaim pass can retry this
    /// exact logic after freeing an orphaned slot, without duplicating the fill-in fields.
    fn try_claim_empty(&self, client_cred: &Ucred, server_cred: &Ucred) -> Option<u32> {
        for (i, slot) in self.slots.iter().enumerate() {
            if slot
                .state
                .compare_exchange(
                    CONN_SLOT_EMPTY,
                    CONN_SLOT_OCCUPIED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                slot.client_to_server.reset();
                slot.server_to_client.reset();
                slot.reset_holders();
                slot.client_pid
                    .store(client_cred.pid as u32, Ordering::Relaxed);
                slot.client_uid.store(client_cred.uid, Ordering::Relaxed);
                slot.client_gid.store(client_cred.gid, Ordering::Relaxed);
                slot.server_pid
                    .store(server_cred.pid as u32, Ordering::Relaxed);
                slot.server_uid.store(server_cred.uid, Ordering::Relaxed);
                slot.server_gid.store(server_cred.gid, Ordering::Relaxed);
                return Some(i as u32);
            }
        }
        None
    }

    /// If `slot` is `OCCUPIED` but no live host process holds either side, frees it back to
    /// `EMPTY` and returns `true`. (The credential pids are guest pids, not host pids, so
    /// liveness is judged from the per-host holder records.) A CAS guards the actual free so two
    /// racing callers that both observe the same orphaned slot never double-free it -- the loser
    /// simply returns `false` and moves on (to the next slot, or a later call).
    fn reclaim_dead_slot(&self, slot: &SharedConnSlot<Platform>, platform: &Platform) -> bool {
        if slot.state.load(Ordering::Acquire) != CONN_SLOT_OCCUPIED {
            return false;
        }
        if !(slot.side_gone(true, platform) && slot.side_gone(false, platform)) {
            return false;
        }
        let reclaimed = slot
            .state
            .compare_exchange(
                CONN_SLOT_OCCUPIED,
                CONN_SLOT_EMPTY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if reclaimed {
            slot.client_to_server.reset();
            slot.server_to_client.reset();
        }
        reclaimed
    }

    fn get(&self, idx: u32) -> &SharedConnSlot<Platform> {
        &self.slots[idx as usize]
    }

    /// Releases a slot back to the pool. Best-effort, idempotent: only the LAST of the two
    /// endpoints to drop actually frees it (guarded by each `ConnTransport::Shared` handle's own
    /// `Drop`, which only calls this after marking itself gone -- see that `Drop` impl).
    fn free(&self, idx: u32) {
        let slot = &self.slots[idx as usize];
        slot.client_to_server.reset();
        slot.server_to_client.reset();
        slot.state.store(CONN_SLOT_EMPTY, Ordering::Release);
    }
}

const REQ_EMPTY: u32 = 0;
const REQ_WRITING: u32 = 1;
const REQ_PENDING: u32 = 2;
const REQ_CLAIMED: u32 = 3;
const REQ_ACCEPTED: u32 = 4;

/// Upper bound on simultaneously in-flight cross-process `connect()` attempts (bounded like every
/// other fixed-capacity table in this file; a full queue degrades a connect attempt to a retry,
/// never a panic).
///
/// **Raised 64 -> 256, 2026-10-03**, together with [`SHARED_UNIX_CONN_CAPACITY`]'s own raise: this
/// queue is the rendezvous every one of those connections is created through, so with hundreds of
/// concurrent channels a 64-deep queue would become the next wall behind the (measured) conn-table
/// one. Not itself a measured blocker in `chr16` -- that run's 1265 refusals were all
/// `shared unix connection table full`, none from here -- so this is headroom, not a fix. Still
/// inline in `GlobalState` by value on purpose: 256 slots is ~35 KiB total, two orders of magnitude
/// below the by-value size [`SHARED_UNIX_CONN_CAPACITY`]'s move into the shared arena just removed.
pub(crate) const SHARED_UNIX_CONNECT_QUEUE_CAPACITY: usize = 256;

struct PendingConnectRequest {
    state: AtomicU32,
    kind: AtomicU32,
    len: AtomicU32,
    bytes: [AtomicU8; UNIX_ADDR_KEY_MAX],
    client_pid: AtomicU32,
    client_uid: AtomicU32,
    client_gid: AtomicU32,
    /// Valid once `state == REQ_ACCEPTED`; `u32::MAX` means "not yet set".
    conn_slot: AtomicU32,
}

impl PendingConnectRequest {
    fn new_empty() -> Self {
        Self {
            state: AtomicU32::new(REQ_EMPTY),
            kind: AtomicU32::new(0),
            len: AtomicU32::new(0),
            bytes: core::array::from_fn(|_| AtomicU8::new(0)),
            client_pid: AtomicU32::new(0),
            client_uid: AtomicU32::new(0),
            client_gid: AtomicU32::new(0),
            conn_slot: AtomicU32::new(u32::MAX),
        }
    }

    fn matches(&self, kind: u32, key: &[u8]) -> bool {
        self.kind.load(Ordering::Relaxed) == kind
            && self.len.load(Ordering::Relaxed) as usize == key.len()
            && key
                .iter()
                .enumerate()
                .all(|(i, b)| self.bytes[i].load(Ordering::Relaxed) == *b)
    }
}

/// Cross-process AF_UNIX connect/accept rendezvous: the piece [`SharedUnixAddrPresenceTable`]'s
/// own doc comment explicitly named as out of scope ("cannot safely hand back a dereferenceable
/// `Arc<Backlog>` to a DIFFERENT process's `connect()` call"). A client posts a request naming the
/// address it wants; the listener's own `accept()` loop -- already running in the ONE process that
/// legitimately owns the real `Arc<Backlog>` -- claims matching requests and completes them with a
/// [`SharedUnixConnTable`] slot index, entirely without either side ever dereferencing a pointer
/// that came from the other process.
pub(crate) struct SharedUnixConnectQueue {
    requests: [PendingConnectRequest; SHARED_UNIX_CONNECT_QUEUE_CAPACITY],
}

impl SharedUnixConnectQueue {
    pub(crate) fn new() -> Self {
        Self {
            requests: core::array::from_fn(|_| PendingConnectRequest::new_empty()),
        }
    }

    /// Client side: posts a connect request for `(kind, key)`. Returns the request index to poll,
    /// or `None` if `key` is oversized or every slot is busy (caller returns `EAGAIN`/retries,
    /// same degrade-gracefully contract as [`SharedUnixAddrPresenceTable::insert`]).
    fn post(&self, kind: u32, key: &[u8], client_cred: &Ucred) -> Option<usize> {
        if key.len() > UNIX_ADDR_KEY_MAX {
            return None;
        }
        for (i, req) in self.requests.iter().enumerate() {
            if req
                .state
                .compare_exchange(REQ_EMPTY, REQ_WRITING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                for (j, b) in key.iter().enumerate() {
                    req.bytes[j].store(*b, Ordering::Relaxed);
                }
                req.len.store(key.len() as u32, Ordering::Relaxed);
                req.kind.store(kind, Ordering::Relaxed);
                req.client_pid
                    .store(client_cred.pid as u32, Ordering::Relaxed);
                req.client_uid.store(client_cred.uid, Ordering::Relaxed);
                req.client_gid.store(client_cred.gid, Ordering::Relaxed);
                req.conn_slot.store(u32::MAX, Ordering::Relaxed);
                req.state.store(REQ_PENDING, Ordering::Release);
                return Some(i);
            }
        }
        None
    }

    /// Listener side, read-only: does a request naming `(kind, key)` currently sit `PENDING`?
    /// Unlike [`Self::try_claim`], never mutates state -- this is `Backlog::check_io_events`'s
    /// own half of the rendezvous, called from `poll`/`select`/`epoll_wait` to decide whether the
    /// listening fd is readable, which must happen BEFORE a real event-driven server (Xvfb,
    /// dbus-daemon: both wait on readiness before ever calling `accept()`) will call `accept()`
    /// at all. Missing this half was a real, live-caught bug (2026-09-18): `try_accept`'s shared-
    /// queue check was correct but functionally dead code, because nothing ever told the
    /// listener's `poll()` loop a cross-process connection was waiting, so it never got called.
    fn has_pending(&self, kind: u32, key: &[u8]) -> bool {
        if key.len() > UNIX_ADDR_KEY_MAX {
            return false;
        }
        // DIAGNOSTIC (2026-09-18, twenty-fourth pass): throttled dump of the whole queue's real
        // state every ~6s (400 calls * the ~15ms bounded-repoll interval this is exclusively
        // called from) alongside the specific (kind, key) THIS check is looking for -- settles
        // live whether the livelock is "queue genuinely empty, nobody is posting" vs "queue HAS
        // pending entries but none match this listener's own key" (timing vs address-mismatch).
        static DIAG_COUNTER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        if DIAG_COUNTER.fetch_add(1, Ordering::Relaxed) % 400 == 0 {
            let snapshot: alloc::vec::Vec<_> = self
                .requests
                .iter()
                .enumerate()
                .filter(|(_, req)| req.state.load(Ordering::Acquire) == REQ_PENDING)
                .map(|(i, req)| {
                    let len = req.len.load(Ordering::Relaxed) as usize;
                    let len = len.min(UNIX_ADDR_KEY_MAX);
                    let bytes: alloc::vec::Vec<u8> = (0..len)
                        .map(|j| req.bytes[j].load(Ordering::Relaxed))
                        .collect();
                    (i, req.kind.load(Ordering::Relaxed), bytes)
                })
                .collect();
            litebox_util_log::debug!(
                checking_kind:% = kind,
                checking_key_len:% = key.len(),
                checking_key_bytes:? = key,
                pending_count:% = snapshot.len(),
                pending_snapshot:? = snapshot;
                "DIAG SharedUnixConnectQueue::has_pending: queue snapshot"
            );
        }
        self.requests
            .iter()
            .any(|req| req.state.load(Ordering::Acquire) == REQ_PENDING && req.matches(kind, key))
    }

    /// Listener side: finds and claims (so a racing second `accept()` on the same address can't
    /// double-claim it) one pending request naming `(kind, key)`. Returns the claimed request's
    /// index and the client's real credentials.
    fn try_claim(&self, kind: u32, key: &[u8]) -> Option<(usize, Ucred)> {
        if key.len() > UNIX_ADDR_KEY_MAX {
            return None;
        }
        for (i, req) in self.requests.iter().enumerate() {
            if req.state.load(Ordering::Acquire) == REQ_PENDING
                && req.matches(kind, key)
                && req
                    .state
                    .compare_exchange(
                        REQ_PENDING,
                        REQ_CLAIMED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            {
                let cred = Ucred {
                    pid: req.client_pid.load(Ordering::Relaxed) as u32,
                    uid: req.client_uid.load(Ordering::Relaxed),
                    gid: req.client_gid.load(Ordering::Relaxed),
                };
                litebox_util_log::debug!(
                    idx:% = i,
                    kind:% = kind,
                    key_len:% = key.len(),
                    key_bytes:? = key,
                    client_pid:% = cred.pid;
                    "DIAG SharedUnixConnectQueue::try_claim: claimed request"
                );
                return Some((i, cred));
            }
        }
        None
    }

    /// Listener side: finishes a claimed request with the slot it allocated for the connection.
    fn complete(&self, idx: usize, conn_slot: u32) {
        let req = &self.requests[idx];
        req.conn_slot.store(conn_slot, Ordering::Relaxed);
        req.state.store(REQ_ACCEPTED, Ordering::Release);
    }

    /// Client side: non-blocking poll for the outcome of `post`'s returned index. Consumes the
    /// request (resets it to `REQ_EMPTY`) exactly once, the same call that first observes
    /// `REQ_ACCEPTED` -- safe because only the one client that posted this index ever polls it.
    fn poll_result(&self, idx: usize) -> Option<u32> {
        let req = &self.requests[idx];
        if req.state.load(Ordering::Acquire) == REQ_ACCEPTED {
            let slot = req.conn_slot.load(Ordering::Relaxed);
            req.state.store(REQ_EMPTY, Ordering::Release);
            Some(slot)
        } else {
            None
        }
    }

    /// Client side: best-effort withdrawal of a still-unclaimed request (e.g. the connect
    /// attempt's own deadline expired). If a listener concurrently claimed it in the meantime (a
    /// real, live race between this connect attempt's own timeout/error path and the listener's
    /// `accept()`), drain and free the [`SharedUnixConnTable`] slot the listener allocated too,
    /// rather than leaking it for the rest of the whole fork family's lifetime.
    ///
    /// FIXED (62nd pass, 2026-09-23): this used to just give up on losing the
    /// `REQ_PENDING`->`REQ_EMPTY` race and walk away, leaving the request (and, once the
    /// listener's own `complete()` ran a moment later, a real [`SharedUnixConnTable`] slot) stuck
    /// non-`REQ_EMPTY`/non-`CONN_SLOT_EMPTY` until the whole fork family exited -- a real,
    /// previously-only-theoretical leak this pass's own `LITEBOX_PROCESS_FORK=1` desktop-boot
    /// investigation raised to a live concern: `SHARED_UNIX_CONNECT_QUEUE_CAPACITY` is 64, and a
    /// real `xfce4-session` boot's ~28+ processes each racing several D-Bus/X11 connect attempts
    /// under real host-RAM pressure (this same investigation's own measured ~8.5GB->under-1GB-in-
    /// <90s cost) is exactly the shape that could exhaust it well before the fork family ever
    /// exits, permanently `EAGAIN`-ing every later connect to an otherwise perfectly healthy
    /// listener. Safe to drain here rather than call [`SharedUnixConnTable::free`] directly:
    /// constructing this side's own [`UnixConnectedStream`] and immediately dropping it defers to
    /// [`ConnTransport`]'s existing Drop-based both-sides-shut-down bookkeeping (the SAME path a
    /// synchronously-completed connect that the caller immediately closes already goes through),
    /// so the slot is only actually freed once the listener's own side ALSO later closes --
    /// calling `free()` straight from here would race a listener still genuinely using the slot.
    fn cancel<Platform: ShimPlatform, FS: ShimFS>(
        &self,
        global: &GlobalStateHandle<Platform, FS>,
        peer_addr: &UnixSocketAddr,
        idx: usize,
    ) {
        let req = &self.requests[idx];
        if req
            .state
            .compare_exchange(REQ_PENDING, REQ_EMPTY, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return;
        }
        // Lost the race: the listener's `try_claim` already moved this request to `REQ_CLAIMED`.
        // Its own `complete()` call is a handful of instructions later, never a real wait, so a
        // bounded spin (not a blocking wait -- no genuine cross-process wakeup exists for this,
        // matching every other consumer of this queue) is the correct tool here.
        for _ in 0..100_000 {
            if req.state.load(Ordering::Acquire) == REQ_ACCEPTED {
                let conn_slot = req.conn_slot.load(Ordering::Relaxed);
                req.state.store(REQ_EMPTY, Ordering::Release);
                if conn_slot != u32::MAX {
                    litebox_util_log::debug!(
                        idx:% = idx, conn_slot:% = conn_slot;
                        "SharedUnixConnectQueue::cancel: lost the claim race, draining the \
                         abandoned conn_slot instead of leaking it"
                    );
                    let server_cred = global.unix_shared_conn_table.get(conn_slot).server_cred();
                    // Constructed only to be dropped immediately -- see this fn's own doc comment
                    // for why this, not a direct `free()`, is the safe way to release it.
                    drop(UnixConnectedStream::<Platform, FS>::new_shared(
                        global.clone(),
                        conn_slot,
                        true,
                        UnixSocketAddr::Unnamed,
                        peer_addr.clone(),
                        server_cred,
                    ));
                }
                return;
            }
            core::hint::spin_loop();
        }
        litebox_util_log::warn!(
            idx:% = idx;
            "SharedUnixConnectQueue::cancel: listener claimed this request but never completed \
             it within a bounded spin -- leaking this slot (existing degrade-not-panic \
             philosophy, self-healing once the whole fork family exits)"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Carrying a unix socket into a `LITEBOX_PROCESS_FORK=1` cross-process fork child.
//
// The child is a separate host process, so an fd can only follow it as a description it rebuilds
// on its side: `UnixSocket::fork_carry` writes one (a "spec" string), the platform ships it in the
// child's environment, and `UnixSocket::from_fork_spec` rebuilds the socket there.
//
// - A connected stream/seqpacket socket is promoted onto a `SharedUnixConnTable` slot (see
//   `UnixConnectedStream::promote_for_fork`); parent and child then hold the same side of the
//   same slot. The parent counts one extra holder of that side on the child's behalf, so the
//   peer cannot see EOF while the child is still starting; the child moves that count to its own
//   host process when it rebuilds the endpoint.
// - A listener is rebuilt in the child without registering the address again; the child's
//   `accept()` takes cross-process connects from `SharedUnixConnectQueue`, while connects from
//   the parent's own host process keep going to the parent's backlog.
// - An unbound, unconnected stream or datagram socket is rebuilt fresh.
// - Everything else (a bound but unconnected socket, a connect in progress, a bound or connected
//   datagram socket including a datagram socketpair) is refused, and the fork stays on the
//   thread-based path.
//
// The very same specs carry a unix socket over `SCM_RIGHTS` between two host processes:
// `Task::scm_carry_spec` calls `Self::fork_carry` with `child_pid == 0` and ships the result as a
// `U|` spec next to the message's bytes in the shared ring; `Task::rebuild_carried_unix` rebuilds
// it with `Self::from_fork_spec`. So the two carries share ONE code path, and the differences are
// only the two the sender's ignorance of the receiver forces: an `SCM_RIGHTS` connection keeps the
// SENDER's host pid as the holder the receiver releases on adoption (`fork_carry`'s `carried_me`),
// and an `SCM_RIGHTS` listener's presence entry is registered by the RECEIVER (a fork child's is
// registered by the parent, which knows the child's pid).

fn encode_unix_addr(addr: &UnixSocketAddr) -> String {
    let hex = |bytes: &[u8]| bytes.iter().map(|b| alloc::format!("{b:02x}")).collect::<String>();
    match addr {
        UnixSocketAddr::Unnamed => String::from("u"),
        UnixSocketAddr::Path(p) => alloc::format!("p{}", hex(p.as_bytes())),
        UnixSocketAddr::Abstract(a) => alloc::format!("a{}", hex(a)),
    }
}

fn decode_unix_addr(s: &str) -> Option<UnixSocketAddr> {
    let unhex = |h: &str| -> Option<Vec<u8>> {
        (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
            .collect()
    };
    match s.split_at_checked(1)? {
        ("u", "") => Some(UnixSocketAddr::Unnamed),
        ("p", h) => Some(UnixSocketAddr::Path(String::from_utf8(unhex(h)?).ok()?)),
        ("a", h) => Some(UnixSocketAddr::Abstract(unhex(h)?)),
        _ => None,
    }
}

/// What the parent must undo if the fork that a [`UnixSocket::fork_carry`] was made for does not
/// happen: the connection holder it counted, or the listener presence it advertised, on the
/// child's behalf.
pub(crate) enum UnixCarryHold {
    /// The connection holder counted for the child on `slot`'s `is_client` side. `host` is the
    /// host pid it is registered under right now: the carrying process's own until
    /// [`UnixSocket::fork_carry_move_to_child`] moves it onto the child's, once the child exists.
    Conn {
        slot: u32,
        is_client: bool,
        host: u32,
    },
    Presence {
        key: UnixSocketAddrKey,
        owner_pid: u32,
    },
}

impl<Platform: ShimPlatform, FS: ShimFS> UnixSocket<Platform, FS> {
    /// Whether [`Self::fork_carry`] can carry this socket; the reason when it cannot. Changes
    /// nothing.
    pub(super) fn fork_carry_check(&self) -> Result<(), &'static str> {
        match &self.inner {
            UnixSocketInner::Stream(stream) => stream.with_state_ref(|state| match state {
                // A bound-but-unconnected socket carries as `B`: the address is all the state it
                // has beyond the two shutdown bits, and both survive the encode (Linux would hand
                // over a dup sharing the same bound socket; litebox rebuilds one bound to the same
                // name, which is the same approximation a carried listener `L` already makes).
                UnixStreamState::Init(_) => Ok(()),
                UnixStreamState::Listen(_) | UnixStreamState::Connected(_) => Ok(()),
                UnixStreamState::Connecting(_) => Err("unix-socket(connect-in-progress)"),
            }),
            UnixSocketInner::Datagram(datagram) => {
                let inner = datagram.inner.read();
                if inner.addr.is_none()
                    && inner.recv_channel.is_none()
                    && inner.connected_send_channel.is_none()
                {
                    Ok(())
                } else {
                    Err("unix-datagram(bound-or-connected)")
                }
            }
        }
    }

    /// Describes this socket for a cross-process fork child (see this section's comment),
    /// promoting a connection onto a shared slot. `own_cred` is the forking task's.
    ///
    /// A listener is advertised under `child_pid` right away, so a connect made after the parent
    /// closes its own copy but before the child is running still finds it (and waits in the shared
    /// queue for the child's `accept()`), as it would on Linux, where the socket never went away.
    ///
    /// `child_pid == 0` is the SCM_RIGHTS flavour of the same carry (`Task::scm_carry_spec` is its
    /// only caller that passes 0): the receiver is a DIFFERENT task whose pid the sender cannot
    /// know, so anything the spec would have to record under the receiver's pid is left for the
    /// receiver to do itself when it rebuilds (see `Self::from_fork_spec`'s `L` branch). A real
    /// fork always names a real tid, so 0 is unambiguous.
    ///
    /// An SCM_RIGHTS listener is NOT advertised here: `sendmsg` hands over a duplicate, so the
    /// sender keeps its own copy and its own presence entry stays valid; the receiver registers
    /// its own entry instead (so the address survives the sender closing its copy later), which
    /// is also the only option available -- the sender does not know the receiver's pid.
    pub(super) fn fork_carry(
        &self,
        global: &GlobalStateHandle<Platform, FS>,
        own_cred: Ucred,
        child_pid: i32,
        for_spawn: bool,
    ) -> Result<(String, Option<UnixCarryHold>), &'static str> {
        self.fork_carry_check()?;
        let status = self.get_status().bits();
        let body = match &self.inner {
            UnixSocketInner::Datagram(_) => return Ok((alloc::format!("{status:x};D"), None)),
            UnixSocketInner::Stream(stream) => {
                let seq = u8::from(stream.preserve_boundaries);
                stream.with_state_ref(|state| match state {
                    UnixStreamState::Init(init) => {
                        let Some(addr) = init.addr.as_ref() else {
                            return Ok((alloc::format!("I,{seq}"), None));
                        };
                        Ok((
                            alloc::format!(
                                "B,{seq},{},{},{}",
                                encode_unix_addr(&UnixSocketAddr::from(addr)),
                                u8::from(init.read_shutdown.load(Ordering::Acquire)),
                                u8::from(init.write_shutdown.load(Ordering::Acquire))
                            ),
                            None,
                        ))
                    }
                    UnixStreamState::Listen(listen) => {
                        let backlog = &listen.backlog;
                        let cred = backlog.listener_cred;
                        let key = backlog.addr.to_key();
                        let owner_pid = child_pid.cast_unsigned();
                        // `child_pid == 0`: SCM_RIGHTS, not a fork -- see this fn's doc comment.
                        // Nothing is registered here; the receiver registers the address under
                        // its own pid when it rebuilds (`from_fork_spec`), and the spec's last
                        // field tells it to.
                        let receiver_registers_presence = child_pid == 0;
                        if !receiver_registers_presence {
                            let (kind, bytes) = presence_kind_and_bytes(&key);
                            if !global.unix_addr_presence.insert(kind, bytes, owner_pid) {
                                return Err("unix-socket(listener; shared presence table full)");
                            }
                        }
                        Ok((
                            alloc::format!(
                                "L,{seq},{},{},{},{},{},{}",
                                backlog.state.lock().limit,
                                cred.pid,
                                cred.uid,
                                cred.gid,
                                encode_unix_addr(&UnixSocketAddr::from(backlog.addr.as_ref())),
                                u8::from(receiver_registers_presence)
                            ),
                            if receiver_registers_presence {
                                None
                            } else {
                                Some(UnixCarryHold::Presence { key, owner_pid })
                            },
                        ))
                    }
                    UnixStreamState::Connected(conn) => {
                        let (slot, is_client) =
                            conn.promote_for_fork(global, own_cred, stream.preserve_boundaries)?;
                        let me = global.platform.current_host_pid();
                        // A spawned child adopts its inherited fds only after its whole host
                        // process has booted (about a second here: rootfs, shared arena, VMA
                        // layout), while its parent -- typically a `fork()`+`exec()` launcher --
                        // may exit at any instant in between. Counting this hold under the
                        // parent's pid would leave the slot's side with no LIVE holder for that
                        // whole window, and a reader on the other side then sees EOF even though
                        // the child is alive: the `Broken pipe` Chromium's zygote reports when
                        // its launcher exits first (`zygote_linux.cc:138`). The hold is therefore
                        // moved onto the child's own pid as soon as the child exists
                        // (`Self::fork_carry_move_to_child`), and the spec's `me` field is left
                        // 0 to tell the adopter to release its own pid rather than the parent's.
                        // An fd handed over by `SCM_RIGHTS` has no such window -- the sender
                        // holds it until the receiver adopts -- so it keeps the sender's pid.
                        let carried_me = if for_spawn { 0 } else { me };
                        litebox_util_log::__private::tracing::event!(
                            target: "litebox_diag::unix_conn_teardown",
                            litebox_util_log::__private::tracing::Level::DEBUG,
                            slot = %slot,
                            is_client = %is_client,
                            host_pid = %me,
                            "DIAG unix shared conn: fork_carry extra hold for the child"
                        );
                        global
                            .unix_shared_conn_table
                            .get(slot)
                            .hold(is_client, me, global.platform);
                        let peer = conn.peer_cred;
                        Ok((
                            alloc::format!(
                                "C,{seq},{slot},{},{carried_me},{},{},{},{},{}",
                                u8::from(is_client),
                                peer.pid,
                                peer.uid,
                                peer.gid,
                                encode_unix_addr(&conn.get_local_addr()),
                                encode_unix_addr(&conn.get_peer_addr())
                            ),
                            Some(UnixCarryHold::Conn {
                                slot,
                                is_client,
                                host: me,
                            }),
                        ))
                    }
                    UnixStreamState::Connecting(_) => Err("unix-socket(connect-in-progress)"),
                })?
            }
        };
        Ok((alloc::format!("{status:x};{}", body.0), body.1))
    }

    /// Moves the connection holder [`Self::fork_carry`] counted for a child off the carrying
    /// process's host pid and onto the child's real one, as soon as the child's host process
    /// exists. See the comment at that call site for the launcher-exits-first window this closes.
    pub(super) fn fork_carry_move_to_child(
        global: &GlobalStateHandle<Platform, FS>,
        hold: &mut UnixCarryHold,
        child_host: u32,
    ) {
        let UnixCarryHold::Conn {
            slot,
            is_client,
            host,
        } = hold
        else {
            return;
        };
        if *host == child_host {
            return;
        }
        let slot_ref = global.unix_shared_conn_table.get(*slot);
        slot_ref.hold(*is_client, child_host, global.platform);
        slot_ref.release(*is_client, *host);
        litebox_util_log::__private::tracing::event!(
            target: "litebox_diag::unix_conn_teardown",
            litebox_util_log::__private::tracing::Level::DEBUG,
            slot = %*slot,
            is_client = %*is_client,
            from_host_pid = %*host,
            child_host_pid = %child_host,
            after = %slot_ref.holder_dump(),
            "DIAG unix shared conn: carried hold moved onto the child's own host pid"
        );
        *host = child_host;
    }

    /// Undoes a [`Self::fork_carry`] whose fork did not happen.
    pub(super) fn fork_carry_abandon(global: &GlobalStateHandle<Platform, FS>, hold: UnixCarryHold) {
        match hold {
            UnixCarryHold::Conn {
                slot,
                is_client,
                host,
            } => SharedView {
                global,
                slot,
                is_client,
            }
            .release_holder_for(host),
            UnixCarryHold::Presence { key, owner_pid } => {
                let (kind, bytes) = presence_kind_and_bytes(&key);
                global.unix_addr_presence.remove(kind, bytes, owner_pid);
            }
        }
    }

    /// Releases the connection hold [`Self::fork_carry`] counted for a receiver that will never
    /// adopt this spec -- the read consumed the bytes but the fd is never rebuilt (no ancillary
    /// buffer, `EMFILE`, a rebuild that fails, a message dropped outright). Without this that hold
    /// is counted under the sender's host pid until the sender's whole process exits, because
    /// "gone" is decided by host-process liveness, not by whether the fd is still open
    /// (`SharedConnSlot::side_gone`), so every such message leaks one slot for the session.
    ///
    /// Parses only the fields [`Self::from_fork_spec`]'s `C` branch needs, and stays silent on
    /// anything else: a spec that describes no shared connection has no hold to release.
    pub(super) fn release_unadopted_carry(
        global: &GlobalStateHandle<Platform, FS>,
        spec: &str,
    ) {
        let Some((_status, body)) = spec.split_once(';') else {
            return;
        };
        let mut fields = body.split(',');
        let Some(kind) = fields.next() else {
            return;
        };
        // `D` (datagram) has no seq field and no connection; `I`/`L` (init/listen) hold nothing
        // but presence, which `fork_carry_abandon` undoes.
        if kind != "C" {
            return;
        }
        let _seq = fields.next();
        let (Some(slot), Some(is_client), Some(host)) = (
            fields.next().and_then(|s| s.parse::<u32>().ok()),
            fields.next().map(|s| s == "1"),
            fields.next().and_then(|s| s.parse::<u32>().ok()),
        ) else {
            return;
        };
        if slot as usize >= SHARED_UNIX_CONN_CAPACITY {
            return;
        }
        // `0` means the hold was already moved onto the receiving process's own pid (a spawned
        // fork child), so that pid is what has to release it -- see `from_fork_spec`.
        let release_host = if host == 0 {
            global.platform.current_host_pid()
        } else {
            host
        };
        SharedView {
            global,
            slot,
            is_client,
        }
        .release_holder_for(release_host);
    }

    /// Rebuilds, in a cross-process fork child, the socket a parent's [`Self::fork_carry`]
    /// described.
    pub(super) fn from_fork_spec(task: &Task<Platform, FS>, spec: &str) -> Option<Self> {
        let (status, body) = spec.split_once(';')?;
        let status = OFlags::from_bits_truncate(u32::from_str_radix(status, 16).ok()?);
        let mut fields = body.split(',');
        let kind = fields.next()?;
        let inner = if kind == "D" {
            UnixSocketInner::Datagram(UnixDatagram::new())
        } else {
            let seq = fields.next()? == "1";
            let state = match kind {
                "I" => UnixStreamState::Init(UnixInitStream::new()),
                "B" => {
                    let addr = decode_unix_addr(fields.next()?)?;
                    let read_shutdown = fields.next() == Some("1");
                    let write_shutdown = fields.next() == Some("1");
                    // Opened, not created: the carrier already bound it, so the socket file (if
                    // any) exists and this process only re-opens the same name -- exactly what a
                    // carried listener `L` does. A name that cannot be re-opened degrades to
                    // `UnopenedPath`, so `getsockname` still reports the address.
                    let addr = match (addr.clone().bind(task, false), addr) {
                        (Ok(bound), _) => bound,
                        (Err(err), UnixSocketAddr::Path(path)) => {
                            litebox_util_log::debug!(
                                path:% = path, err:? = err;
                                "carried bound unix socket: could not open its socket file; keeping \
                                 the name only"
                            );
                            UnixBoundSocketAddr::UnopenedPath(path)
                        }
                        (Err(_), _) => return None,
                    };
                    UnixStreamState::Init(UnixInitStream {
                        addr: Some(addr),
                        pollee: Pollee::new(),
                        read_shutdown: AtomicBool::new(read_shutdown),
                        write_shutdown: AtomicBool::new(write_shutdown),
                    })
                }
                "L" => {
                    let limit = fields.next()?.parse().ok()?;
                    let cred = Ucred {
                        pid: fields.next()?.parse().ok()?,
                        uid: fields.next()?.parse().ok()?,
                        gid: fields.next()?.parse().ok()?,
                    };
                    // Opened, not created: the parent already bound it. Not put in this process's
                    // address table (the parent's backlog serves connects from the parent's own
                    // host process); the parent already advertised it in the shared presence
                    // table under this process's pid (see `fork_carry`), so cross-process connects
                    // keep reaching this listener after the parent closes its copy.
                    let addr = decode_unix_addr(fields.next()?)?;
                    // Trailing field, absent in a spec written before it existed (and always
                    // absent for a fork carry): `1` when the CARRIER could not register this
                    // address (SCM_RIGHTS -- it does not know this process's pid), so this
                    // process registers it itself, under its own pid, right here. Both entries
                    // can coexist: `SharedUnixAddrPresenceTable::insert` takes any EMPTY slot, so
                    // the same address under two pids gets two slots, and
                    // `Backlog::try_accept_shared` claims a pending cross-process connect by
                    // ADDRESS alone (`SharedUnixConnectQueue::try_claim`), never by owner pid --
                    // so either copy of the listener can accept it, the same sharing a
                    // fork-carried listener already has with its parent.
                    let register_own_presence = fields.next() == Some("1");
                    let addr = match (addr.clone().bind(task, false), addr) {
                        (Ok(bound), _) => bound,
                        (Err(err), UnixSocketAddr::Path(path)) => {
                            litebox_util_log::debug!(
                                path:% = path, err:? = err;
                                "carried unix listener: could not open its socket file; keeping                                  the name only"
                            );
                            UnixBoundSocketAddr::UnopenedPath(path)
                        }
                        (Err(_), _) => return None,
                    };
                    let owner_pid = task.pid.get().cast_unsigned();
                    if register_own_presence {
                        let key = addr.to_key();
                        let (presence_kind, presence_bytes) = presence_kind_and_bytes(&key);
                        let inserted = task
                            .global
                            .unix_addr_presence
                            .insert(presence_kind, presence_bytes, owner_pid);
                        if !inserted {
                            // Degrade, never panic: the address simply is not visible to a
                            // cross-process `connect()` that misses this process's own address
                            // table either, exactly as before this carry existed.
                            litebox_util_log::warn!(
                                owner_pid:% = owner_pid;
                                "carried unix listener: shared presence table full, address not \
                                 advertised cross-process"
                            );
                        }
                    }
                    UnixStreamState::Listen(UnixListenStream {
                        backlog: Arc::new(Backlog::new(addr, limit, Pollee::new(), cred)),
                        global: task.global.clone(),
                        owner_pid,
                        carried: true,
                    })
                }
                "C" => {
                    let slot: u32 = fields.next()?.parse().ok()?;
                    let is_client = fields.next()? == "1";
                    let parent_host: u32 = fields.next()?.parse().ok()?;
                    let peer_cred = Ucred {
                        pid: fields.next()?.parse().ok()?,
                        uid: fields.next()?.parse().ok()?,
                        gid: fields.next()?.parse().ok()?,
                    };
                    let local_addr = decode_unix_addr(fields.next()?)?;
                    let peer_addr = decode_unix_addr(fields.next()?)?;
                    if slot as usize >= SHARED_UNIX_CONN_CAPACITY {
                        return None;
                    }
                    let conn = UnixConnectedStream::new_shared(
                        task.global.clone(),
                        slot,
                        is_client,
                        local_addr,
                        peer_addr,
                        peer_cred,
                    );
                    // The holder the parent counted for this child now lives here. A spawned
                    // child's spec names no parent pid (`fork_carry` writes 0 there) because the
                    // hold was moved onto the child's own pid the moment the child existed --
                    // before this adoption, which is far too late to keep the connection alive.
                    let release_host = if parent_host == 0 {
                        task.global.platform.current_host_pid()
                    } else {
                        parent_host
                    };
                    litebox_util_log::__private::tracing::event!(
                        target: "litebox_diag::unix_conn_teardown",
                        litebox_util_log::__private::tracing::Level::DEBUG,
                        slot = %slot,
                        is_client = %is_client,
                        parent_host = %parent_host,
                        release_host = %release_host,
                        child_host = %task.global.platform.current_host_pid(),
                        "DIAG unix shared conn: child adopted, releasing the parent's extra hold"
                    );
                    // The holder the parent counted for this child now lives here.
                    task.global
                        .unix_shared_conn_table
                        .get(slot)
                        .release(is_client, release_host);
                    litebox_util_log::__private::tracing::event!(
                        target: "litebox_diag::unix_conn_teardown",
                        litebox_util_log::__private::tracing::Level::DEBUG,
                        slot = %slot,
                        after = %task.global.unix_shared_conn_table.get(slot).holder_dump(),
                        "DIAG unix shared conn: holders after the child adopted"
                    );
                    UnixStreamState::Connected(conn)
                }
                _ => return None,
            };
            UnixSocketInner::Stream(UnixStream::new(state, seq))
        };
        let socket = Self::new_with_inner(inner, SockFlags::empty());
        socket.set_status(status, true);
        Some(socket)
    }
}
