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
    sync::{Mutex, RwLock},
    utils::TruncateExt as _,
};
use litebox_common_linux::{
    IpOption, ReceiveFlags, SendFlags, ShutdownHow, SockFlags, SockType, SocketOption,
    SocketOptionName, Ucred, errno::Errno,
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
            Self::Path((path, ..)) => UnixSocketAddrKey::Path(path.clone()),
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
            Self::Abstract(_) => {}
        }
    }
}

impl<FS: ShimFS> From<&UnixBoundSocketAddr<FS>> for UnixSocketAddr {
    fn from(addr: &UnixBoundSocketAddr<FS>) -> Self {
        match addr {
            UnixBoundSocketAddr::Path((path, ..)) => UnixSocketAddr::Path(path.clone()),
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
        let (request_idx, client_cred) = global.unix_shared_connect_queue.try_claim(kind, key_bytes)?;
        let Some(slot) = global
            .unix_shared_conn_table
            .alloc(global.litebox.platform(), &client_cred, &self.listener_cred)
        else {
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
            if global.unix_shared_connect_queue.has_pending(kind, key_bytes) {
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
        if let Some(UnixEntry(UnixEntryInner::Stream(backlog))) = table.get(&key)
            && Arc::ptr_eq(backlog, &self.backlog)
        {
            table.remove(&key);
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
        };
        // `insert_raw_fd` only fails once the *receiver's* own `RLIMIT_NOFILE` is exceeded --
        // matches real Linux's `recvmsg` behavior of closing an over-limit donated fd and
        // reporting `MSG_CTRUNC` rather than failing the whole read (the byte payload the fd was
        // sent alongside has already been legitimately delivered by this point).
        res.map_err(|()| Errno::EMFILE)
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
}

/// The two ways a [`UnixConnectedStream`] moves bytes to/from its peer. `Local` is the original,
/// unchanged same-process path (an `Arc`-boxed `crate::channel::Channel`, unusable across a real
/// process boundary). `Shared` is the new cross-process-fork path: a slot in
/// [`SharedUnixConnTable`], living in the shared kernel arena, with no `Arc`/`Box`/pointer
/// crossing between the two processes anywhere -- see that type's own module doc comment for the
/// full rendezvous design and its explicit scope limits (byte-stream only, no genuine cross-
/// process wakeup).
enum ConnTransport<Platform: ShimPlatform, FS: ShimFS> {
    Local {
        addr: AddrView<FS>,
        /// The read end of the local socket's channel for receiving messages.
        recv_channel: crate::channel::ReadEnd<Platform, Message<Platform, FS>>,
        /// The write end of the connected peer socket for sending messages.
        connected_send_channel: crate::channel::WriteEnd<Platform, Message<Platform, FS>>,
    },
    Shared {
        global: GlobalStateHandle<Platform, FS>,
        slot: u32,
        /// `true` if this endpoint is the connect()-ing side (reads `server_to_client`, writes
        /// `client_to_server`); `false` for the accept()-ing side (the reverse).
        is_client: bool,
        local_addr: UnixSocketAddr,
        peer_addr: UnixSocketAddr,
        /// Set true by this endpoint's own `Drop`/`shutdown(SHUT_RD)` -- distinct from the ring's
        /// own `write_shutdown` (which tracks the PEER's write direction), needed because a
        /// `Shared` connection has no `channel::EndPointer`-style `Arc` this side can inspect to
        /// ask "did I already shut myself down".
        self_read_shutdown: AtomicBool,
    },
}

/// Represents a connected Unix stream socket.
struct UnixConnectedStream<Platform: ShimPlatform, FS: ShimFS> {
    transport: ConnTransport<Platform, FS>,
    pollee: Arc<Pollee<Platform>>,
    /// Real credentials (pid/uid/gid) of the *peer* task, as of connection
    /// establishment -- what `getsockopt(SO_PEERCRED)` reports to this side.
    peer_cred: Ucred,
}

impl<Platform: ShimPlatform, FS: ShimFS> Drop for ConnTransport<Platform, FS> {
    fn drop(&mut self) {
        // `Local`'s two channel ends already shut themselves down via their own `Drop` impls
        // (`channel.rs`) -- nothing extra needed here. `Shared` has no such per-field `Drop`
        // (its fields are a plain index/bools, not `Arc`-boxed endpoints), so this is the only
        // place that can mark this endpoint's own direction shut down and, once BOTH sides are
        // gone, release the slot back to the shared pool.
        if let ConnTransport::Shared {
            global, slot, is_client, ..
        } = self
        {
            let table = &global.unix_shared_conn_table;
            let slot_ref = table.get(*slot);
            if *is_client {
                slot_ref.client_to_server.shutdown();
            } else {
                slot_ref.server_to_client.shutdown();
            }
            // Both directions shut down (i.e. both endpoints have dropped, or one dropped after
            // already shutting down its write side and the other direction was already shut by
            // the peer's own earlier drop) -- safe to reclaim the slot. This is a conservative,
            // best-effort check: worst case a slot is freed slightly late (bounded by
            // SHARED_UNIX_CONN_CAPACITY, self-healing on the next connection churn), never
            // double-freed (each endpoint's `Drop` runs at most once).
            if slot_ref.client_to_server.is_shutdown() && slot_ref.server_to_client.is_shutdown() {
                table.free(*slot);
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
        let pollee1 = pollee.unwrap_or(Arc::new(Pollee::new()));
        let pollee2 = Arc::new(Pollee::new());
        let (send_channel, recv_channel) =
            crate::channel::Channel::new(UNIX_BUF_SIZE, pollee2.clone(), pollee1.clone()).split();
        let (send_channel_peer, recv_channel_peer) =
            crate::channel::Channel::new(UNIX_BUF_SIZE, pollee1.clone(), pollee2.clone()).split();
        let first = UnixConnectedStream {
            transport: ConnTransport::Local {
                addr: addr1,
                recv_channel,
                connected_send_channel: send_channel_peer,
            },
            pollee: pollee1,
            peer_cred: second_cred,
        };
        let second = UnixConnectedStream {
            transport: ConnTransport::Local {
                addr: addr2,
                recv_channel: recv_channel_peer,
                connected_send_channel: send_channel,
            },
            pollee: pollee2,
            peer_cred: first_cred,
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

    /// Constructs the `Shared`-transport half of a genuinely cross-process connection --
    /// see [`ConnTransport::Shared`]'s doc comment. `is_client` selects which of the slot's two
    /// [`SharedByteRing`]s this side reads/writes.
    fn new_shared(
        global: GlobalStateHandle<Platform, FS>,
        slot: u32,
        is_client: bool,
        local_addr: UnixSocketAddr,
        peer_addr: UnixSocketAddr,
        peer_cred: Ucred,
    ) -> Self {
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
        }
    }

    /// For `Shared` transport, returns `(ring this side reads from, ring this side writes to)`.
    fn shared_rings(
        global: &GlobalStateHandle<Platform, FS>,
        slot: u32,
        is_client: bool,
    ) -> (&SharedByteRing<Platform>, &SharedByteRing<Platform>) {
        let slot_ref = global.unix_shared_conn_table.get(slot);
        if is_client {
            (&slot_ref.server_to_client, &slot_ref.client_to_server)
        } else {
            (&slot_ref.client_to_server, &slot_ref.server_to_client)
        }
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
    /// `Local` transport (its underlying `Channel` pushes one whole `Message` atomically, no
    /// partial concept), but possibly LESS than `msg.data.len()` for `Shared` transport on an
    /// over-sized single write (see [`Self::try_sendto_shared`]/[`SHARED_UNIX_CONN_BUF`]'s doc
    /// comments).
    fn try_sendto(
        &self,
        msg: Message<Platform, FS>,
    ) -> Result<usize, (Message<Platform, FS>, Errno)> {
        let ConnTransport::Local {
            connected_send_channel,
            ..
        } = &self.transport
        else {
            return self.try_sendto_shared(msg);
        };
        // TODO: write partial data?
        let len = msg.data.len();
        let sock_id = self as *const _ as usize;
        // `LITEBOX_DRM_TRACE=1` (reused; same flag already wired end-to-end for DRM tracing, see
        // `drm::drm_trace_enabled`'s doc comment -- not adding a new env var to avoid touching
        // the runner's own std-only `env::var_os` call site, which is mid-edit by a peer session
        // this pass): dump a bounded hex prefix of the bytes actually written to a unix stream.
        // This is the X11-protocol-decode instrumentation for AGENTS.md's "Rendering/scanout
        // blocker" investigation -- an X11 request over a unix-domain socket starts with a 1-byte
        // opcode (CreateWindow=1, MapWindow=8, ConfigureWindow=12, ...). A 32-byte prefix was
        // tried first and was NOT enough: a `write()` syscall on a busy X11 client socket
        // typically batches several small requests together (observed live: a single write
        // covering the whole QueryExtension/CreateGC/GetProperty/... startup sequence), so a
        // short prefix only ever shows the first one or two requests before running off the end
        // of the capture window -- exactly the failure mode that made the CreateWindow/MapWindow
        // question undecidable with 32 bytes. Raised to 4096 (most individual X11 requests,
        // including a `CreateWindow` with a full property/attribute list, fit well inside this;
        // only genuinely large payloads like `PutImage`/`ChangeProperty` with big property data
        // exceed it, which is fine -- those aren't the requests this trace needs to see in full).
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

    /// `Shared`-transport half of [`Self::try_sendto`] -- see [`ConnTransport::Shared`]'s doc
    /// comment for the byte-stream-only, no-`SCM_RIGHTS` scope limit this enforces. Prefers an
    /// atomic all-or-nothing write (temporary backpressure resolves via the normal `EAGAIN`-then-
    /// retry path); only degrades to a genuine short write for the one case that could never
    /// resolve any other way -- a single message bigger than the whole ring.
    fn try_sendto_shared(
        &self,
        msg: Message<Platform, FS>,
    ) -> Result<usize, (Message<Platform, FS>, Errno)> {
        let ConnTransport::Shared {
            global,
            slot,
            is_client,
            ..
        } = &self.transport
        else {
            unreachable!("try_sendto_shared only called for ConnTransport::Shared");
        };
        if !msg.fds.is_empty() {
            return Err((msg, Errno::EOPNOTSUPP));
        }
        let (_, write_ring) = Self::shared_rings(global, *slot, *is_client);
        if write_ring.is_shutdown() {
            return Err((msg, Errno::EPIPE));
        }
        if msg.data.is_empty() {
            return Ok(0);
        }
        if msg.data.len() > SHARED_UNIX_CONN_BUF {
            let n = write_ring.try_write(&msg.data[..SHARED_UNIX_CONN_BUF]);
            litebox_util_log::debug!(
                slot:% = *slot, is_client:% = *is_client, len:% = n;
                "DIAG try_sendto_shared: partial write (oversized message)"
            );
            return if n == 0 {
                Err((msg, Errno::EAGAIN))
            } else {
                Ok(n)
            };
        }
        if !write_ring.try_write_all(&msg.data) {
            return Err((msg, Errno::EAGAIN));
        }
        // `diag_cursor()` read AFTER the write (2026-09-20) -- pairs with the same call in
        // `check_io_events_shared` to prove/disprove cross-process visibility of this exact
        // cursor update from the writer's OWN process's point of view.
        let (wp, rp) = write_ring.diag_cursor();
        litebox_util_log::debug!(
            slot:% = *slot, is_client:% = *is_client, len:% = msg.data.len(),
            write_pos_after:% = wp, read_pos_after:% = rp;
            "DIAG try_sendto_shared: wrote"
        );
        Ok(msg.data.len())
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
    ) -> Result<(usize, AnyDupFds<Platform, FS>), TryOpError<Errno>> {
        let ConnTransport::Local { recv_channel, .. } = &self.transport else {
            return self.try_recvfrom_shared(buf);
        };
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
        // prefix, this time on bytes actually delivered back to the reading process (an X11
        // reply/error/event starts with a 1-byte type byte: 0=Error, 1=Reply, 2+=Event).
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

    /// `Shared`-transport half of [`Self::try_recvfrom`]. Pure byte-stream (no message
    /// boundaries to preserve -- [`SharedByteRing`] never had any), and always reports an empty
    /// `fds` list (see [`ConnTransport::Shared`]'s scope-limit doc comment: `SCM_RIGHTS` is
    /// rejected up front on the sending side, so there is never anything to deliver here).
    fn try_recvfrom_shared(
        &self,
        buf: &mut [u8],
    ) -> Result<(usize, AnyDupFds<Platform, FS>), TryOpError<Errno>> {
        let ConnTransport::Shared {
            global,
            slot,
            is_client,
            self_read_shutdown,
            ..
        } = &self.transport
        else {
            unreachable!("try_recvfrom_shared only called for ConnTransport::Shared");
        };
        if self_read_shutdown.load(Ordering::Acquire) {
            return Err(TryOpError::Other(Errno::ESHUTDOWN));
        }
        let (read_ring, _) = Self::shared_rings(global, *slot, *is_client);
        let n = read_ring.try_read(buf);
        if n > 0 {
            litebox_util_log::debug!(
                slot:% = *slot, is_client:% = *is_client, len:% = n;
                "DIAG try_recvfrom_shared: read"
            );
            // Same hex-prefix trace `try_recvfrom` (the `Local`-transport sibling) already has for
            // its own `LITEBOX_DRM_TRACE`-gated variant, extended to the `Shared`-transport path --
            // Xvfb's own X11 socket runs over `Shared`, and this path never had this diagnostic, so
            // the 30th-pass investigation into Xvfb's deterministic mid-boot SIGSEGV could see WHICH
            // bytes it read but not their actual content, only length. Gated on this module's own
            // `LITEBOX_LOG` debug level (same as the plain length-only event two lines up) rather
            // than `drm_trace_enabled()`'s `AtomicBool`: that flag is set exactly once, by the
            // top-level runner's own startup routine (`litebox_runner_linux_on_windows_userland::
            // run`), which a `LITEBOX_PROCESS_FORK=1` cross-process fork CHILD's own resume path
            // never re-invokes (confirmed live, 2026-09-21 -- zero `diag-unix-shared-read-bytes`
            // lines from a boot where `LITEBOX_DRM_TRACE=1` was exported to the whole process tree
            // and `litebox_shim_linux::syscalls::unix=debug`'s own sibling lines fired thousands of
            // times in the SAME child processes), so it silently stays `false` in every fork child,
            // Xvfb included -- the one process this trace exists to observe. `LITEBOX_LOG`'s
            // `EnvFilter`, by contrast, is already proven live to re-initialize correctly per fork
            // child (every other diagnostic in this investigation relied on exactly that). Bounded
            // to the same 4096-byte cap as the `Local`-path sibling.
            let preview_len = n.min(4096);
            litebox_util_log::debug!(
                slot:% = *slot, is_client:% = *is_client, total_read:% = n,
                prefix_hex:? = &buf[..preview_len];
                "diag-unix-shared-read-bytes"
            );
            self.pollee.notify_observers(Events::OUT);
            return Ok((n, Vec::new()));
        }
        if read_ring.is_shutdown() {
            return Err(TryOpError::Other(Errno::ESHUTDOWN));
        }
        Err(TryOpError::TryAgain)
    }

    /// `SOCK_SEQPACKET`'s own boundary-preserving read: unlike [`Self::try_recvfrom`], never
    /// spans more than the ONE message at the front of `recv_channel`. A message larger than
    /// `buf` is truncated (matching real Linux `recv(2)`'s "excess bytes in a datagram are
    /// discarded" behavior for message-boundary-preserving socket types) rather than left
    /// partially in the queue for a follow-up read to continue.
    ///
    /// `Shared` transport has no message-boundary concept at all (see [`ConnTransport::Shared`]'s
    /// doc comment: byte-stream only) -- `SOCK_SEQPACKET` over a genuinely cross-process
    /// connection degrades to the same behavior as [`Self::try_recvfrom_shared`], which is a
    /// disclosed, guest-observable difference from real boundary-preserving `SEQPACKET` semantics
    /// rather than a panic; no real boot-critical guest (X11, D-Bus) uses `SOCK_SEQPACKET`.
    fn try_recvfrom_one_message(
        &self,
        buf: &mut [u8],
    ) -> Result<(usize, AnyDupFds<Platform, FS>), TryOpError<Errno>> {
        let ConnTransport::Local { recv_channel, .. } = &self.transport else {
            return self.try_recvfrom_shared(buf);
        };
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
        let ConnTransport::Local {
            recv_channel,
            connected_send_channel,
            ..
        } = &self.transport
        else {
            return self.check_io_events_shared();
        };
        let mut events = Events::empty();
        let is_read_shutdown = recv_channel.is_shutdown();
        let is_peer_write_shutdown = recv_channel.is_peer_shutdown();
        let is_write_shutdown = connected_send_channel.is_shutdown();
        if is_read_shutdown || is_peer_write_shutdown {
            events |= Events::RDHUP | Events::IN;
            if is_write_shutdown {
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

    fn check_io_events_shared(&self) -> Events {
        let ConnTransport::Shared {
            global,
            slot,
            is_client,
            self_read_shutdown,
            ..
        } = &self.transport
        else {
            unreachable!("check_io_events_shared only called for ConnTransport::Shared");
        };
        let (read_ring, write_ring) = Self::shared_rings(global, *slot, *is_client);
        let mut events = Events::empty();
        let is_read_shutdown = self_read_shutdown.load(Ordering::Acquire);
        let is_peer_write_shutdown = read_ring.is_shutdown();
        if is_read_shutdown || is_peer_write_shutdown {
            events |= Events::RDHUP | Events::IN;
            if write_ring.is_shutdown() {
                events |= Events::HUP;
            }
        }
        if !read_ring.is_empty() {
            events |= Events::IN;
        }
        if !write_ring.is_full() {
            events |= Events::OUT;
        }
        // 1-in-100 throttled + always-log-on-nonzero-cursor (2026-09-20): definitive answer to
        // whether THIS process's view of the peer-written ring's cursor ever advances off
        // (0, 0) at all -- if `write_pos` is stuck at 0 here while the writer's own process
        // logged a successful `try_write_all`/`try_sendto_shared: wrote` for this exact slot,
        // that's live proof of a cross-process shared-memory visibility gap (same root-cause
        // shape as the earlier writable-layer-visibility bug), not a logic bug in this
        // readiness check itself.
        static DIAG_CIOE_COUNTER: core::sync::atomic::AtomicU64 =
            core::sync::atomic::AtomicU64::new(0);
        let (wp, rp) = read_ring.diag_cursor();
        let call_idx = DIAG_CIOE_COUNTER.fetch_add(1, Ordering::Relaxed);
        if wp != 0 || call_idx % 100 == 0 {
            litebox_util_log::debug!(
                slot:% = *slot,
                is_client:% = *is_client,
                read_write_pos:% = wp,
                read_read_pos:% = rp,
                computed_events:? = events;
                "DIAG check_io_events_shared: read_ring cursor snapshot"
            );
        }
        events
    }

    fn shutdown(&self, how: ShutdownHow) {
        match &self.transport {
            ConnTransport::Local {
                recv_channel,
                connected_send_channel,
                ..
            } => {
                let mut events = Events::empty();
                if how.is_shutdown_read() && recv_channel.shutdown() {
                    events |= Events::IN | Events::RDHUP;
                }
                if how.is_shutdown_write() && connected_send_channel.shutdown() {
                    events |= Events::OUT | Events::HUP;
                }
                self.pollee.notify_observers(events);
            }
            ConnTransport::Shared {
                global,
                slot,
                is_client,
                self_read_shutdown,
                ..
            } => {
                let mut events = Events::empty();
                if how.is_shutdown_read() && !self_read_shutdown.swap(true, Ordering::AcqRel) {
                    events |= Events::IN | Events::RDHUP;
                }
                if how.is_shutdown_write() {
                    let (_, write_ring) = Self::shared_rings(global, *slot, *is_client);
                    if !write_ring.is_shutdown() {
                        write_ring.shutdown();
                        events |= Events::OUT | Events::HUP;
                    }
                }
                self.pollee.notify_observers(events);
            }
        }
    }
}

enum UnixStreamState<Platform: ShimPlatform, FS: ShimFS> {
    Init(UnixInitStream<Platform, FS>),
    Listen(UnixListenStream<Platform, FS>),
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
            log_cross_process_presence_miss(task, &key);
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
        let backlog = match self.lookup(task, &addr) {
            Ok(b) => b,
            Err(Errno::ECONNREFUSED) => {
                return self.connect_cross_process(task, &addr, is_nonblocking);
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
        match task.global.unix_addr_presence.lookup(kind, key_bytes) {
            Some(owner_pid) if owner_pid != self_pid => {}
            _ => {
                log_cross_process_presence_miss(task, &key);
                return Err(Errno::ECONNREFUSED);
            }
        }
        let client_cred = task.peer_cred();
        let Some(request_idx) = task
            .global
            .unix_shared_connect_queue
            .post(kind, key_bytes, &client_cred)
        else {
            // Queue full -- ordinary, guest-triggerable degrade, not a bug.
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
        let cx = task.wait_cx().with_timeout(SHARED_UNIX_CROSS_CONNECT_TIMEOUT);
        let result = wait_on_events_polling(
            &cx,
            is_nonblocking,
            Events::empty(),
            |_observer, _mask| Ok::<(), Errno>(()), // no real wake source exists yet -- see doc
            || match task.global.unix_shared_connect_queue.poll_result(request_idx) {
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
                litebox_util_log::debug!(
                    self_pid:% = self_pid,
                    request_idx:% = request_idx;
                    "DIAG connect_cross_process: request TIMED OUT, cancelling"
                );
                task.global.unix_shared_connect_queue.cancel(request_idx);
                return Err(Errno::ECONNREFUSED);
            }
            Err(TryOpError::TryAgain) => {
                // AGENTS.md 30th pass: a non-blocking cross-process connect that has not yet been
                // claimed by the listener's own accept() loop is genuinely still IN PROGRESS, not
                // a failure -- POSIX connect(2) reserves EAGAIN for "no ephemeral port available"
                // and uses EINPROGRESS for exactly this "come back later" case, which is also the
                // one real AF_UNIX/TCP client libraries (libdbus among them) explicitly special-
                // case to mean "poll for writability, don't treat this as an error". The blanket
                // `TryOpError::TryAgain -> Errno::EAGAIN` conversion this used to fall through to
                // (`litebox_common_linux::errno`) surfaced the wrong one here, and this whole
                // branch used to ALSO cancel the just-posted queue request on every such call --
                // i.e. every non-blocking connect attempt that did not complete synchronously
                // within the same syscall was torn down immediately, so it could never complete
                // later no matter how long the caller polled. Only the errno is fixed this pass
                // (mirrors `litebox_shim_linux::syscalls::net::connect`'s existing, proven-correct
                // override for the analogous TCP path); NOT cancelling and instead giving the
                // caller a way to reattach to the same still-pending `request_idx` on a later
                // `connect()`/poll needs its own state-machine change to `UnixStreamState::Init`
                // and is left as a precisely-scoped follow-up (see AGENTS.md pickup list) rather
                // than risked half-done in this pass.
                litebox_util_log::debug!(
                    self_pid:% = self_pid,
                    request_idx:% = request_idx;
                    "DIAG connect_cross_process: request not yet claimed (non-blocking), cancelling \
                     and returning EINPROGRESS"
                );
                task.global.unix_shared_connect_queue.cancel(request_idx);
                return Err(Errno::EINPROGRESS);
            }
            Err(e) => {
                litebox_util_log::debug!(
                    self_pid:% = self_pid,
                    request_idx:% = request_idx,
                    err:? = e;
                    "DIAG connect_cross_process: request failed with other error, cancelling"
                );
                task.global.unix_shared_connect_queue.cancel(request_idx);
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
    ) -> Result<usize, Errno> {
        let mut msg = Some(Message {
            data: buf.to_vec(),
            fds,
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
                        conn.try_recvfrom_one_message(buf)?
                    } else {
                        conn.try_recvfrom(buf)?
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
            UnixStreamState::Connected(connect) => connect.get_local_addr(),
        })
    }
    fn get_peer_addr(&self) -> Option<UnixSocketAddr> {
        self.with_state_ref(|state| match state {
            UnixStreamState::Init(_) | UnixStreamState::Listen(_) => None,
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
            UnixStreamState::Connected(connect) => {
                connect.pollee.register_observer(observer, mask);
            }
        });
    }
    fn check_io_events(&self) -> Events {
        self.with_state_ref(|state| match state {
            UnixStreamState::Init(init) => {
                // Fresh Init reports OUT|HUP (HUP because not connected). After a
                // shutdown(SHUT_RD) on an Init socket, Linux additionally reports IN
                // (a recv would return EOF immediately). SHUT_WR has no observable
                // effect on Init's poll output.
                let mut events = Events::OUT | Events::HUP;
                if init.read_shutdown.load(Ordering::Acquire) {
                    events |= Events::IN;
                }
                events
            }
            UnixStreamState::Listen(listen) => listen.backlog.check_io_events(&listen.global),
            UnixStreamState::Connected(conn) => conn.check_io_events(),
        })
    }

    fn shutdown(&self, how: ShutdownHow) {
        self.with_state_ref(|state| match state {
            UnixStreamState::Init(init) => init.shutdown(how),
            UnixStreamState::Listen(listen) => {
                if how.is_shutdown_read() {
                    listen.backlog.shutdown();
                }
            }
            UnixStreamState::Connected(conn) => conn.shutdown(how),
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
    ) -> Result<usize, TryOpError<Errno>> {
        let is_self_shutdown = self.is_shutdown();
        self.peek_and_consume_one(|msg| {
            let copy_len = buf.len().min(msg.data.len());
            buf[..copy_len].copy_from_slice(&msg.data[..copy_len]);
            if let Some(source_addr) = source_addr.as_deref_mut() {
                *source_addr = Some(msg.source.clone());
            }
            // Always consume the entire message to preserve boundaries.
            Ok((true, msg.data.len()))
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
type BoundDatagramAddr<Platform, FS> = (UnixBoundSocketAddr<FS>, GlobalStateHandle<Platform, FS>, u32);

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
                }),
            },
        )
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
        mut source_addr: Option<&mut Option<UnixSocketAddr>>,
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
                    recv_channel.try_read(buf, source_addr.as_deref_mut())
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
            SockType::Stream | SockType::SeqPacket => {
                UnixSocketInner::Stream(UnixStream::new(
                    UnixStreamState::Init(UnixInitStream::new()),
                    matches!(sock_type, SockType::SeqPacket),
                ))
            }
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
        self.sendmsg(task, buf, flags, addr, Vec::new())
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
                stream.sendto(&task.wait_cx(), timeout, buf, is_nonblocking, addr, fds)
            }
            UnixSocketInner::Datagram(datagram) => {
                datagram.sendto(task, timeout, buf, is_nonblocking, addr)
            }
        }
    }

    pub(super) fn recvfrom(
        &self,
        cx: &WaitContext<'_, Platform>,
        buf: &mut [u8],
        flags: ReceiveFlags,
        source_addr: Option<&mut Option<UnixSocketAddr>>,
    ) -> Result<usize, Errno> {
        self.recvmsg(cx, buf, flags, source_addr).map(|(n, _)| n)
    }

    /// `recvfrom`'s own superset: also returns any `SCM_RIGHTS` fds delivered alongside the data
    /// read (always empty for a datagram socket or a message with no attached fds).
    pub(super) fn recvmsg(
        &self,
        cx: &WaitContext<'_, Platform>,
        buf: &mut [u8],
        flags: ReceiveFlags,
        source_addr: Option<&mut Option<UnixSocketAddr>>,
    ) -> Result<(usize, AnyDupFds<Platform, FS>), Errno> {
        // CMSG_CLOEXEC is meaningless for plain recvfrom (no ancillary data ever flows there) but
        // harmless to accept -- net.rs's do_recvmsg is what actually honors it.
        let supported_flags =
            ReceiveFlags::DONTWAIT | ReceiveFlags::TRUNC | ReceiveFlags::CMSG_CLOEXEC;
        if flags.intersects(supported_flags.complement()) {
            log_unsupported!("Unsupported recvfrom flags: {:?}", flags);
            return Err(Errno::EINVAL);
        }
        let is_nonblocking =
            flags.contains(ReceiveFlags::DONTWAIT) || self.get_status().contains(OFlags::NONBLOCK);
        let timeout = self.options.lock().recv_timeout;
        let ret = match &self.inner {
            UnixSocketInner::Stream(stream) => {
                stream.recvfrom(cx, timeout, buf, is_nonblocking, source_addr)
            }
            UnixSocketInner::Datagram(datagram) => datagram
                .recvfrom(cx, timeout, buf, is_nonblocking, source_addr)
                .map(|n| (n, Vec::new())),
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
                IpOption::TOS => Err(Errno::EOPNOTSUPP),
            },
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
                // SO_RCVBUF / SO_SNDBUF are advisory hints. Accept them and keep
                // the fixed internal buffer size, instead of returning EOPNOTSUPP.
                // Log at debug so the accepted-but-ignored option stays visible.
                SocketOption::RCVBUF | SocketOption::SNDBUF => {
                    litebox_util_log::debug!(
                        "accepting and ignoring setsockopt(SO_RCVBUF/SO_SNDBUF) on unix socket; using fixed buffer size"
                    );
                    Ok(())
                }
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
                IpOption::TOS => return Err(Errno::EOPNOTSUPP),
            },
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
                SocketOption::TYPE => match &self.inner {
                    UnixSocketInner::Stream(stream) if stream.preserve_boundaries => {
                        SockType::SeqPacket as u32
                    }
                    UnixSocketInner::Stream(_) => SockType::Stream as u32,
                    UnixSocketInner::Datagram(_) => SockType::Datagram as u32,
                },
                SocketOption::RCVBUF | SocketOption::SNDBUF => UNIX_BUF_SIZE.trunc(),
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
    match task.global.unix_addr_presence.lookup(presence_kind, presence_bytes) {
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
// - **Byte-stream only.** `Message::fds` (`SCM_RIGHTS`) has no representation here -- a
//   `TypedFd`/`AnyDupFd` is fundamentally a handle into ONE process's own descriptor table, not
//   plain bytes, and cannot be made shared-memory-native by this pattern at all. `try_sendto`
//   refuses a non-empty `fds` list on a `Shared`-transport connection with `EOPNOTSUPP` (never
//   silently drops them -- a caller that actually depended on a donated fd must see a clean
//   error, not a mysteriously missing descriptor on the other end).
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
fn wait_on_events_polling<Platform, R, E>(
    cx: &WaitContext<'_, Platform>,
    nonblock: bool,
    events: Events,
    mut register_observer: impl FnMut(Weak<dyn litebox::event::observer::Observer<Events>>, Events) -> Result<(), E>,
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
    loop {
        let remaining = cx.remaining_timeout();
        if has_real_deadline && remaining.is_none() {
            return Err(TryOpError::WaitError(litebox::event::wait::WaitError::TimedOut));
        }
        let this_iter_timeout = match remaining {
            None => SHARED_UNIX_POLL_INTERVAL,
            Some(d) => d.min(SHARED_UNIX_POLL_INTERVAL),
        };
        let bounded = cx.with_timeout(this_iter_timeout);
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

/// Realistic upper bound on simultaneously OPEN cross-process AF_UNIX stream connections in one
/// guest session (X11 rarely has more than a handful of concurrent clients; D-Bus system+session
/// bus each accept a modest number) -- same sizing philosophy as [`UNIX_ADDR_PRESENCE_CAPACITY`]/
/// `MAX_SOCKETS`.
///
/// Sized small on purpose, together with [`SHARED_UNIX_CONN_BUF`] below -- learned live,
/// 2026-09-18: `GlobalState` (which embeds this table) is constructed as an ordinary Rust value
/// and passed BY VALUE through `create_shared_kernel_state`/`SharedArc::new` before being placed
/// in the shared arena, so an oversized field here blows the constructing thread's stack before
/// ever reaching the arena at all (`thread 'main' has overflowed its stack`, live-reproduced with
/// this table at 8 MiB total). [`UnixAddrPresenceSlot`]'s own proven-safe table is ~31 KiB total
/// (256 slots x ~124 bytes) -- this table's total footprint is kept in that same order of
/// magnitude rather than sized generously the way a heap-backed collection could be.
///
/// Raised from 8 to 64 (matching [`SHARED_UNIX_CONNECT_QUEUE_CAPACITY`]'s own scale; still well
/// under an order of magnitude below the 8 MiB stack-overflow threshold above at ~256 KiB total)
/// as a mitigation for a real, live-caught leak this same pass also adds proper (bounded)
/// reclaim for -- see [`SharedUnixConnTable::alloc`]'s doc comment: a slot's only release path is
/// a cooperative `Drop` that never runs when its owning process is killed externally rather than
/// exiting normally, which every `sys_ppoll`-stuck client caught and killed during this
/// investigation did. A bigger pool buys more time before that leak (now partially, safely
/// recovered by `alloc`'s own dead-holder reclaim) can exhaust it entirely.
pub(crate) const SHARED_UNIX_CONN_CAPACITY: usize = 64;

/// Per-direction shared ring buffer capacity. Bounded like a real kernel AF_UNIX socket's own
/// finite send/receive buffer: this is control-plane protocol traffic (X11/D-Bus requests and
/// replies), not bulk pixel data (MIT-SHM already carries that over System V shared memory, never
/// through this socket). Total shared-arena footprint:
/// `SHARED_UNIX_CONN_CAPACITY * 2 * SHARED_UNIX_CONN_BUF` = 32 KiB -- see
/// [`SHARED_UNIX_CONN_CAPACITY`]'s doc comment for why this is deliberately small, not generous.
/// A single write larger than this never hangs regardless (see [`SharedByteRing::try_write`] and
/// its call site in `UnixConnectedStream::try_sendto_shared`): it degrades to a real short write
/// of the first `SHARED_UNIX_CONN_BUF` bytes, matching a real kernel socket's own short-write
/// behavior on an over-sized single `write(2)`, rather than looping on `EAGAIN` forever.
const SHARED_UNIX_CONN_BUF: usize = 2048;

const CONN_SLOT_EMPTY: u32 = 0;
const CONN_SLOT_OCCUPIED: u32 = 1;

/// One direction's worth of shared byte transport: a fixed ring buffer plus its read/write
/// cursors, all guarded by one cross-process `RawMutex`-backed lock (same primitive
/// `GlobalStateHandle::net_lock` already uses for `Network`) so concurrent same-direction access
/// from either side's process is always serialized through real, dereferenceable local memory --
/// no raw pointer is ever handed from one process to another; each side only ever touches its OWN
/// copy of the `Arc`-free, inline-in-the-shared-arena bytes.
struct SharedByteRing<Platform: ShimPlatform> {
    cursor: Mutex<Platform, RingCursor>,
    buf: [AtomicU8; SHARED_UNIX_CONN_BUF],
}

#[derive(Clone, Copy, Default)]
struct RingCursor {
    write_pos: usize,
    read_pos: usize,
    /// Set once the writing side calls `shutdown`/is dropped. Mirrors `channel::EndPointer`'s own
    /// `is_shutdown` -- queued bytes remain readable after this is set; a reader only observes
    /// EOF once `write_pos == read_pos` as well.
    write_shutdown: bool,
}

impl<Platform: ShimPlatform> SharedByteRing<Platform> {
    fn new_empty() -> Self {
        Self {
            cursor: Mutex::new(RingCursor::default()),
            buf: core::array::from_fn(|_| AtomicU8::new(0)),
        }
    }

    fn reset(&self) {
        *self.cursor.lock() = RingCursor::default();
    }

    /// All-or-nothing write, mirroring `channel::WriteEnd::try_write_one`'s own contract (that
    /// pushes one whole `Message` or fails with the ring untouched) so callers don't need to
    /// track a partial-write remainder across calls: either every byte of `data` fits right now
    /// and is written, or NONE of it is and the buffer is left exactly as it was. Never blocks,
    /// never panics.
    fn try_write_all(&self, data: &[u8]) -> bool {
        let mut cursor = self.cursor.lock();
        if cursor.write_shutdown {
            return false;
        }
        let used = cursor.write_pos.wrapping_sub(cursor.read_pos);
        let free = SHARED_UNIX_CONN_BUF - used;
        if data.len() > free {
            return false;
        }
        for (i, b) in data.iter().enumerate() {
            self.buf[(cursor.write_pos.wrapping_add(i)) % SHARED_UNIX_CONN_BUF]
                .store(*b, Ordering::Relaxed);
        }
        cursor.write_pos = cursor.write_pos.wrapping_add(data.len());
        true
    }

    /// Partial write: writes as many LEADING bytes of `data` as currently fit, returning the
    /// count (which may be `0` if the ring is full or shut down -- never blocks, never panics).
    /// Only used for the one case [`Self::try_write_all`] can never resolve no matter how empty
    /// the ring is -- a single message bigger than the whole ring -- see
    /// [`SHARED_UNIX_CONN_BUF`]'s doc comment.
    fn try_write(&self, data: &[u8]) -> usize {
        if data.is_empty() {
            return 0;
        }
        let mut cursor = self.cursor.lock();
        if cursor.write_shutdown {
            return 0;
        }
        let used = cursor.write_pos.wrapping_sub(cursor.read_pos);
        let free = SHARED_UNIX_CONN_BUF - used;
        let n = data.len().min(free);
        for (i, b) in data.iter().take(n).enumerate() {
            self.buf[(cursor.write_pos.wrapping_add(i)) % SHARED_UNIX_CONN_BUF]
                .store(*b, Ordering::Relaxed);
        }
        cursor.write_pos = cursor.write_pos.wrapping_add(n);
        n
    }

    /// Reads up to `out.len()` bytes; returns the count read. `0` is ambiguous between "empty and
    /// still open" and "empty and shut down" by design -- callers distinguish via
    /// [`Self::is_shutdown`]/[`Self::is_empty`], mirroring `channel::ReadEnd::peek_and_consume_one`'s
    /// own EAGAIN-vs-ESHUTDOWN split.
    fn try_read(&self, out: &mut [u8]) -> usize {
        let mut cursor = self.cursor.lock();
        let avail = cursor.write_pos.wrapping_sub(cursor.read_pos);
        let n = out.len().min(avail);
        for (i, slot) in out.iter_mut().take(n).enumerate() {
            *slot = self.buf[(cursor.read_pos.wrapping_add(i)) % SHARED_UNIX_CONN_BUF]
                .load(Ordering::Relaxed);
        }
        cursor.read_pos = cursor.read_pos.wrapping_add(n);
        n
    }

    fn is_empty(&self) -> bool {
        let cursor = self.cursor.lock();
        cursor.write_pos == cursor.read_pos
    }

    fn is_full(&self) -> bool {
        let cursor = self.cursor.lock();
        cursor.write_pos.wrapping_sub(cursor.read_pos) >= SHARED_UNIX_CONN_BUF
    }

    fn shutdown(&self) {
        self.cursor.lock().write_shutdown = true;
    }

    fn is_shutdown(&self) -> bool {
        self.cursor.lock().write_shutdown
    }

    /// Diagnostic-only (2026-09-20): exposes the raw cursor for the targeted, throttled
    /// `check_io_events_shared` trace below -- answers definitively whether a reader in a
    /// DIFFERENT process ever observes the writer's cursor update at all (root-causing the
    /// AF_UNIX Xvfb-never-reads-`xset q` gap), as opposed to inferring it indirectly from
    /// `is_empty()` alone.
    fn diag_cursor(&self) -> (usize, usize) {
        let cursor = self.cursor.lock();
        (cursor.write_pos, cursor.read_pos)
    }
}

/// One allocated cross-process connection: two independent [`SharedByteRing`]s (client-to-server,
/// server-to-client) plus enough bookkeeping for `SO_PEERCRED` on both sides. Sized/guarded
/// exactly like [`UnixAddrPresenceSlot`] -- deliberately no `Vec`/`Box`/pointer field anywhere,
/// so the whole slot's live state is its own inline bytes.
struct SharedConnSlot<Platform: ShimPlatform> {
    state: AtomicU32,
    client_to_server: SharedByteRing<Platform>,
    server_to_client: SharedByteRing<Platform>,
    client_pid: AtomicU32,
    client_uid: AtomicU32,
    client_gid: AtomicU32,
    server_pid: AtomicU32,
    server_uid: AtomicU32,
    server_gid: AtomicU32,
}

impl<Platform: ShimPlatform> SharedConnSlot<Platform> {
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
        }
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
pub(crate) struct SharedUnixConnTable<Platform: ShimPlatform> {
    slots: [SharedConnSlot<Platform>; SHARED_UNIX_CONN_CAPACITY],
}

impl<Platform: ShimPlatform> SharedUnixConnTable<Platform> {
    pub(crate) fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| SharedConnSlot::new_empty()),
        }
    }

    /// Claims a free slot and fills it in; returns its index, or `None` if every slot is in use
    /// and none can be reclaimed (degrades to `ECONNREFUSED`/`EAGAIN` at the call site, never
    /// panics).
    ///
    /// `platform` is used only for the dead-holder reclaim pass below (see [`Self::
    /// reclaim_dead_slot`]) -- never for the ordinary fast path, which stays exactly as cheap as
    /// before.
    fn alloc(&self, platform: &Platform, client_cred: &Ucred, server_cred: &Ucred) -> Option<u32> {
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
        for slot in &self.slots {
            if self.reclaim_dead_slot(slot, platform)
                && let Some(idx) = self.try_claim_empty(client_cred, server_cred)
            {
                return Some(idx);
            }
        }
        None
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
                slot.client_pid.store(client_cred.pid as u32, Ordering::Relaxed);
                slot.client_uid.store(client_cred.uid, Ordering::Relaxed);
                slot.client_gid.store(client_cred.gid, Ordering::Relaxed);
                slot.server_pid.store(server_cred.pid as u32, Ordering::Relaxed);
                slot.server_uid.store(server_cred.uid, Ordering::Relaxed);
                slot.server_gid.store(server_cred.gid, Ordering::Relaxed);
                return Some(i as u32);
            }
        }
        None
    }

    /// If `slot` is `OCCUPIED` but both its client and server owning processes are confirmed
    /// dead, frees it back to `EMPTY` and returns `true`. A CAS guards the actual free so two
    /// racing callers that both observe the same orphaned slot never double-free it -- the loser
    /// simply returns `false` and moves on (to the next slot, or a later call).
    fn reclaim_dead_slot(&self, slot: &SharedConnSlot<Platform>, platform: &Platform) -> bool {
        if slot.state.load(Ordering::Acquire) != CONN_SLOT_OCCUPIED {
            return false;
        }
        let client_pid = slot.client_pid.load(Ordering::Relaxed);
        let server_pid = slot.server_pid.load(Ordering::Relaxed);
        if platform.is_process_alive(client_pid) || platform.is_process_alive(server_pid) {
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

/// Realistic upper bound on simultaneously in-flight cross-process `connect()` attempts (bounded
/// like every other fixed-capacity table in this file; a full queue degrades a connect attempt to
/// a retry, never a panic).
pub(crate) const SHARED_UNIX_CONNECT_QUEUE_CAPACITY: usize = 64;

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
                req.client_pid.store(client_cred.pid as u32, Ordering::Relaxed);
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
                    let bytes: alloc::vec::Vec<u8> =
                        (0..len).map(|j| req.bytes[j].load(Ordering::Relaxed)).collect();
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
                    .compare_exchange(REQ_PENDING, REQ_CLAIMED, Ordering::AcqRel, Ordering::Acquire)
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
    /// attempt's own deadline expired). If a listener concurrently claimed it in the meantime,
    /// this is a no-op -- the listener's `complete()` still runs and the resulting slot is simply
    /// never attached to by anyone, a bounded leak matching this table's existing degrade-rather-
    /// than-panic philosophy (reclaimed when the whole fork family exits).
    fn cancel(&self, idx: usize) {
        let req = &self.requests[idx];
        let _ = req
            .state
            .compare_exchange(REQ_PENDING, REQ_EMPTY, Ordering::AcqRel, Ordering::Acquire);
    }
}
