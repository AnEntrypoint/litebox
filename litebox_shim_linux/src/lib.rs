// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! A shim that provides a Linux-compatible ABI via LiteBox.
//!
//! This shim is generic over the choice of [LiteBox platform](../litebox/platform/index.html).
//! The concrete platform is threaded in by the runner via [`LinuxShimBuilder::new`].

#![no_std]
#![expect(
    clippy::unused_self,
    reason = "by convention, syscalls and related methods take &self even if unused"
)]

extern crate alloc;

use alloc::borrow::Cow;
use alloc::vec;
use alloc::vec::Vec;

use alloc::sync::Arc;
use core::cell::{Cell, RefCell};
use litebox::{
    LiteBox,
    fd::TypedFd,
    mm::{PageManager, linux::PAGE_SIZE},
    net::Network,
    pipes::Pipes,
    platform::TimeProvider,
    shim::ContinueOperation,
    sync::futex::FutexManager,
    utils::{ReinterpretSignedExt as _, ReinterpretUnsignedExt as _},
};
use litebox_common_linux::{
    SyscallRequest,
    errno::Errno,
    user_pointers::{UserPtr, UserPtrMut},
};

/// On debug builds, logs that the user attempted to use an unsupported feature.
// DEVNOTE: this is before the `mod` declarations so that it can be used within them.
macro_rules! log_unsupported {
    ($($arg:tt)*) => {
        $crate::log_unsupported_fmt(core::format_args!($($arg)*));
    };
}

pub(crate) mod channel;
pub mod loader;
pub(crate) mod stdio;
pub mod syscalls;
pub mod transport;
mod wait;

use crate::syscalls::file::get_file_descriptor_flags;

pub type DefaultFS<Platform> = LinuxFS<Platform>;

pub(crate) type LinuxFS<Platform> = litebox::fs::layered::FileSystem<
    Platform,
    litebox::fs::in_mem::FileSystem<Platform>,
    litebox::fs::layered::FileSystem<
        Platform,
        litebox::fs::resolver::Resolver<Platform, litebox::fs::composer::Composer>,
        litebox::fs::resolver::Resolver<Platform, litebox::fs::composer::Composer>,
    >,
>;

pub(crate) type FileFd<FS> = litebox::fd::TypedFd<FS>;

/// A trait required for file systems to be used in the shim.
pub trait ShimFS: litebox::fs::FileSystem + Send + Sync + 'static {}
impl<T: litebox::fs::FileSystem + Send + Sync + 'static> ShimFS for T {}

/// Aggregate bound capturing everything the shim requires of a platform.
///
/// This exists so that the (many) `impl` blocks throughout the shim can be written
/// as `impl<Platform: ShimPlatform, ..>` rather than repeating a large `where` clause.
pub trait ShimPlatform:
    litebox::platform::RawPointerProvider
    + litebox::platform::TimeProvider
    + litebox::platform::PageManagementProvider<{ PAGE_SIZE }>
    + litebox::mm::linux::VmemPageFaultHandler
    + litebox::platform::RawMutexProvider
    + litebox::sync::RawSyncPrimitivesProvider
    + litebox::platform::CrngProvider
    + litebox::platform::SystemInfoProvider
    + litebox::platform::ForkChildVerificationProvider
    + litebox::platform::StdioProvider
    + litebox::platform::ArchSpecificProvider
    + litebox::platform::ThreadProvider<ExecutionContext = litebox_common_linux::PtRegs>
    + litebox::platform::TimerProvider<Signal = litebox_common_linux::signal::Signal>
    + litebox::platform::SignalProvider<Signal = litebox_common_linux::signal::Signal>
    + litebox::platform::IPInterfaceProvider
    + 'static
{
}

impl<T> ShimPlatform for T where
    T: litebox::platform::RawPointerProvider
        + litebox::platform::TimeProvider
        + litebox::platform::PageManagementProvider<{ PAGE_SIZE }>
        + litebox::mm::linux::VmemPageFaultHandler
        + litebox::platform::RawMutexProvider
        + litebox::sync::RawSyncPrimitivesProvider
        + litebox::platform::CrngProvider
        + litebox::platform::SystemInfoProvider
        + litebox::platform::ForkChildVerificationProvider
        + litebox::platform::StdioProvider
        + litebox::platform::ArchSpecificProvider
        + litebox::platform::ThreadProvider<ExecutionContext = litebox_common_linux::PtRegs>
        + litebox::platform::TimerProvider<Signal = litebox_common_linux::signal::Signal>
        + litebox::platform::SignalProvider<Signal = litebox_common_linux::signal::Signal>
        + litebox::platform::IPInterfaceProvider
        + 'static
{
}

/// On debug builds, logs that the user attempted to use an unsupported feature.
fn log_unsupported_fmt(args: core::fmt::Arguments<'_>) {
    if cfg!(debug_assertions) {
        litebox_util_log::warn!(feature:% = args; "unsupported");
    }
}

#[cfg(target_pointer_width = "64")]
fn preadv_pwritev_offset(pos_l: usize, _pos_h: usize) -> i64 {
    pos_l.reinterpret_as_signed() as i64
}

#[cfg(target_pointer_width = "32")]
fn preadv_pwritev_offset(pos_l: usize, pos_h: usize) -> i64 {
    ((pos_h as u64) << 32 | pos_l as u64).reinterpret_as_signed()
}

pub struct LinuxShimEntrypoints<Platform: ShimPlatform, FS: ShimFS> {
    task: Task<Platform, FS>,
    // The task should not be moved once it's bound to a platform thread so that
    // we preserve the ability to use TLS in the future.
    _not_send: core::marker::PhantomData<*const ()>,
}

impl<Platform: ShimPlatform, FS: ShimFS> litebox::shim::EnterShim
    for LinuxShimEntrypoints<Platform, FS>
{
    type ExecutionContext = litebox_common_linux::PtRegs;

    fn init(&self, ctx: &mut Self::ExecutionContext) -> ContinueOperation {
        self.enter_shim(true, ctx, Task::handle_init_request)
    }

    fn syscall(&self, ctx: &mut Self::ExecutionContext) -> ContinueOperation {
        self.enter_shim(false, ctx, Task::handle_syscall_request)
    }

    fn exception(
        &self,
        ctx: &mut Self::ExecutionContext,
        info: &litebox::shim::ExceptionInfo,
    ) -> ContinueOperation {
        // The guest's synthesized sigreturn trampoline (see `ensure_sigreturn_trampoline`)
        // traps via `brk #0xdead` (SIGTRAP/BRK64) rather than the real `rt_sigreturn` syscall
        // number, which stays unconditionally seccomp-allowed on this platform (see the
        // allow-list's doc comment on `SYS_rt_sigreturn`). `ctx.pc` landing exactly on the
        // trampoline's own address is this trap's unambiguous signature -- no other guest code
        // executes this specific brk immediate at this specific address.
        #[cfg(target_arch = "aarch64")]
        if info.exception == litebox::shim::Exception::BREAKPOINT_CURRENT_EL
            && ctx.pc == self.task.sigreturn_trampoline_addr()
            && ctx.pc != 0
        {
            return match self.task.sys_rt_sigreturn(ctx) {
                Ok(_) => ContinueOperation::Resume,
                Err(_) => ContinueOperation::Terminate,
            };
        }
        #[cfg(target_arch = "x86_64")]
        let is_kernel_page_fault =
            info.kernel_mode && info.exception == litebox::shim::Exception::PAGE_FAULT;
        #[cfg(target_arch = "aarch64")]
        let is_kernel_page_fault = info.kernel_mode
            && matches!(
                info.exception,
                litebox::shim::Exception::DATA_ABORT_CURRENT_EL
                    | litebox::shim::Exception::DATA_ABORT_LOWER_EL
                    | litebox::shim::Exception::INSTRUCTION_ABORT_CURRENT_EL
                    | litebox::shim::Exception::INSTRUCTION_ABORT_LOWER_EL
            );
        if is_kernel_page_fault {
            #[cfg(target_arch = "x86_64")]
            let (fault_addr, error_code) = (info.cr2, u64::from(info.error_code));
            #[cfg(target_arch = "aarch64")]
            let (fault_addr, error_code) = (info.fault_address, info.esr);
            if unsafe {
                self.task
                    .process()
                    .pm
                    .handle_page_fault(fault_addr, error_code)
            }
            .is_ok()
            {
                return ContinueOperation::Resume;
            } else {
                return ContinueOperation::Terminate;
            }
        }
        self.enter_shim(false, ctx, |task, _ctx| task.handle_exception_request(info))
    }

    fn interrupt(&self, ctx: &mut Self::ExecutionContext) -> ContinueOperation {
        self.enter_shim(false, ctx, |_, _| {})
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> LinuxShimEntrypoints<Platform, FS> {
    /// Returns a handle to this task's underlying process, usable to wait for its exit after
    /// this `LinuxShimEntrypoints` has been consumed (e.g. by
    /// `litebox_platform_windows_userland::run_thread`, which takes it by value).
    ///
    /// Added for pass 142's production process-based-`fork()` child: a `CreateProcessW`-spawned
    /// child built via [`LinuxShim::adopt_forked_process`] has no other way to recover its real
    /// Linux exit status once `run_thread` has consumed the entrypoints it was called with -- see
    /// [`LinuxShimProcess::wait_for_encoded_cross_process_exit_status`]'s doc comment for why that
    /// status then needs to become this child's real Windows exit code.
    pub fn process(&self) -> LinuxShimProcess<Platform> {
        LinuxShimProcess(self.task.process().clone())
    }

    fn enter_shim(
        &self,
        is_init: bool,
        ctx: &mut litebox_common_linux::PtRegs,
        f: impl FnOnce(&Task<Platform, FS>, &mut litebox_common_linux::PtRegs),
    ) -> ContinueOperation {
        if !is_init {
            self.task.enter_from_guest();
        }
        f(&self.task, ctx);
        if self.task.prepare_to_run_guest(ctx) {
            ContinueOperation::Resume
        } else {
            ContinueOperation::Terminate
        }
    }
}

/// The shim entry point structure.
pub struct LinuxShimBuilder<Platform: ShimPlatform> {
    platform: &'static Platform,
    litebox: LiteBox<Platform>,
}

impl<Platform: ShimPlatform> LinuxShimBuilder<Platform> {
    /// Returns a new shim builder using the given platform.
    pub fn new(platform: &'static Platform) -> Self {
        Self {
            platform,
            litebox: LiteBox::new(platform),
        }
    }

    /// Returns the litebox object for the shim.
    pub fn litebox(&self) -> &LiteBox<Platform> {
        &self.litebox
    }

    /// Create a default layered file system with the given in-memory layer and tar data.
    pub fn default_fs(
        &self,
        in_mem_fs: litebox::fs::in_mem::FileSystem<Platform>,
        tar_data: Cow<'static, [u8]>,
    ) -> DefaultFS<Platform> {
        default_fs(&self.litebox, in_mem_fs, tar_data)
    }

    /// Build the shim.
    pub fn build<FS: ShimFS>(self) -> LinuxShim<Platform, FS> {
        let mut net = Network::new(&self.litebox);
        net.set_platform_interaction(litebox::net::PlatformInteraction::Manual);
        let global = Arc::new(GlobalState {
            platform: self.platform,
            bootstrap_process: once_cell::race::OnceBox::new(),
            futex_manager: FutexManager::new(),
            pipes: Pipes::new(&self.litebox),
            net: litebox::sync::Mutex::new(net),
            boot_time: self.platform.now(),
            next_thread_id: 2.into(), // start from 2, as 1 is used by the main thread
            litebox: self.litebox,
            unix_addr_table: litebox::sync::RwLock::new(syscalls::unix::UnixAddrTable::new()),
            elf_patch_cache: litebox::sync::Mutex::new(alloc::collections::BTreeMap::new()),
            flock_registry: litebox::sync::Mutex::new(alloc::collections::BTreeMap::new()),
            next_flock_holder_id: core::sync::atomic::AtomicU64::new(1),
            pty_registry: litebox::sync::RwLock::new(alloc::collections::BTreeMap::new()),
            daemon_pty_masters: litebox::sync::RwLock::new(alloc::collections::BTreeMap::new()),
            next_pty_id: core::sync::atomic::AtomicU32::new(0),
            next_unix_autobind_id: core::sync::atomic::AtomicU32::new(0),
        });
        LinuxShim(global)
    }
}

pub struct LinuxShim<Platform: ShimPlatform, FS: ShimFS>(Arc<GlobalState<Platform, FS>>);
impl<Platform: ShimPlatform, FS: ShimFS> Clone for LinuxShim<Platform, FS> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> LinuxShim<Platform, FS> {
    /// Loads the program at `path` as the shim's initial task, returning the
    /// initial register state.
    pub fn load_program(
        &self,
        fs: alloc::sync::Arc<FS>,
        task: litebox_common_linux::TaskParams,
        path: &str,
        argv: Vec<alloc::ffi::CString>,
        envp: Vec<alloc::ffi::CString>,
    ) -> Result<LoadedProgram<Platform, FS>, loader::elf::ElfLoaderError> {
        self.load_program_with_pty(fs, task, path, argv, envp, false)
    }

    /// Like [`Self::load_program`], but when `attach_pty` is set, allocates a fresh pty pair and
    /// attaches the loaded process to its slave as controlling terminal (mirroring glibc's
    /// `login_tty()`: `setsid()` + `ioctl(slave, TIOCSCTTY, 0)` + `dup2(slave, 0/1/2)`, the exact
    /// sequence `syscalls::pty`'s own test suite exercises) BEFORE the process's normal
    /// `/dev/stdin`/`/dev/stdout`/`/dev/stderr` stdio wiring would otherwise take effect -- this
    /// is the guest-side half of the session-daemon feature (see
    /// `docs/session-daemon-design.md`'s `--pty-mode`): a HOST-side caller with no `Task` in scope
    /// (a plain background thread) can then drive the pty's master side via
    /// [`Self::pty_master_read`]/[`Self::pty_master_write`], keyed by the returned pty id, while
    /// the loaded process's stdio (attached to the slave) sees ordinary raw-mode-capable terminal
    /// semantics -- exactly what an interactive `vi`/`sh` session needs, and exactly what plain
    /// piped (non-console) stdio cannot provide (`TCSETS` on a non-tty stdio fd routes through
    /// `stdio_ioctl`, gated on `Platform::is_a_tty`, which is false for piped/non-console stdio).
    ///
    /// On success, returns `(LoadedProgram, pty_id)`. Failure to attach the pty (an `Errno` from
    /// the underlying `sys_open`/`sys_ioctl`/`sys_dup` calls) is reported as
    /// [`loader::elf::ElfLoaderError::OpenError`], reusing the ELF loader's existing open-failure
    /// error shape rather than inventing a second error type for what is, from this function
    /// signature's point of view, just another way process setup can fail.
    ///
    /// # Panics
    ///
    /// Never in practice: `load_program_with_pty(.., attach_pty: true)` always sets
    /// `attached_pty_id` before returning `Ok`, so the internal `.get().expect(..)` this function
    /// uses to recover it can only panic if that invariant is broken by a future edit.
    pub fn load_program_attach_pty(
        &self,
        fs: alloc::sync::Arc<FS>,
        task: litebox_common_linux::TaskParams,
        path: &str,
        argv: Vec<alloc::ffi::CString>,
        envp: Vec<alloc::ffi::CString>,
    ) -> Result<(LoadedProgram<Platform, FS>, u32), loader::elf::ElfLoaderError> {
        let loaded = self.load_program_with_pty(fs, task, path, argv, envp, true)?;
        let pty_id = loaded.entrypoints.task.attached_pty_id.get().expect(
            "load_program_with_pty(attach_pty=true) always sets attached_pty_id on success",
        );
        Ok((loaded, pty_id))
    }

    fn load_program_with_pty(
        &self,
        fs: alloc::sync::Arc<FS>,
        task: litebox_common_linux::TaskParams,
        path: &str,
        argv: Vec<alloc::ffi::CString>,
        envp: Vec<alloc::ffi::CString>,
        attach_pty: bool,
    ) -> Result<LoadedProgram<Platform, FS>, loader::elf::ElfLoaderError> {
        let litebox_common_linux::TaskParams {
            pid,
            ppid,
            uid,
            euid,
            gid,
            egid,
        } = task;

        let files = syscalls::file::FilesState::new(fs);
        files.set_max_fd(syscalls::process::RLIMIT_NOFILE_CUR - 1);
        let files = Arc::new(files);
        files.initialize_stdio_in_shared_descriptors_table(&self.0);

        // Created once and threaded into both the new `Process` and the new `Task`'s
        // `SignalState` -- see `do_clone`'s identically-shaped `child_shared_pending` for why
        // they must end up sharing the exact same `Arc`.
        let bootstrap_shared_pending = Arc::new(litebox::sync::Mutex::new(
            syscalls::signal::PendingSignals::new(),
        ));
        let entrypoints = crate::LinuxShimEntrypoints {
            _not_send: core::marker::PhantomData,
            task: Task {
                global: self.0.clone(),
                thread: syscalls::process::ThreadState::new_process(
                    pid,
                    PageManager::new(&self.0.litebox),
                    false,
                    None,
                    bootstrap_shared_pending.clone(),
                ),
                wait_state: wait::WaitState::new(self.0.platform),
                pid,
                ppid,
                tid: pid,
                credentials: syscalls::process::Credentials {
                    uid,
                    euid,
                    gid,
                    egid,
                }
                .into(),
                comm: [0; litebox_common_linux::TASK_COMM_LEN].into(), // set at load time
                fs: Arc::new(syscalls::file::FsState::new()).into(),
                files: files.into(),
                signals: syscalls::signal::SignalState::new_process(bootstrap_shared_pending),
                attached_pty_id: Cell::new(None),
            },
        };

        // Make this process's page manager reachable via `LinuxShim::page_manager` for callers
        // with no `Task` in scope (see `GlobalState::bootstrap_process`'s doc comment) BEFORE
        // ELF loading below, since loading the program can itself trigger page faults.
        let _ = self
            .0
            .bootstrap_process
            .set(alloc::boxed::Box::new(entrypoints.task.process().clone()));

        let (path, argv) = entrypoints
            .task
            .resolve_shebang(alloc::string::String::from(path), argv)
            .map_err(loader::elf::ElfLoaderError::OpenError)?;

        entrypoints.task.load_program(
            loader::elf::ElfLoader::new(&entrypoints.task, &path)?,
            argv,
            envp,
        )?;

        if attach_pty {
            let pty_id = entrypoints
                .task
                .attach_pty_stdio(&self.0)
                .map_err(loader::elf::ElfLoaderError::OpenError)?;
            entrypoints.task.attached_pty_id.set(Some(pty_id));
        }

        let process = LinuxShimProcess(entrypoints.task.process().clone());
        Ok(LoadedProgram {
            entrypoints,
            process,
        })
    }

    /// Read bytes from the master side of a pty allocated via [`Self::load_program_attach_pty`],
    /// keyed by the pty id that call returned. Callable from any thread with no `Task` in scope
    /// (see `GlobalState::daemon_pty_masters`'s doc comment) -- this is what lets a plain
    /// background thread in the runner drain a session's pty output concurrently with
    /// `run_thread` running the guest on its own thread. Blocking: waits for at least one byte
    /// using a throwaway, this-call-only `litebox::event::wait::WaitState` (never the guest's own), matching
    /// [`Self::perform_network_interaction`]'s precedent of driving shim-internal I/O from a
    /// caller with no guest `Task` in scope.
    pub fn pty_master_read(&self, pty_id: u32, buf: &mut [u8]) -> Result<usize, Errno> {
        // Resolve the master's `EntryHandle` (which holds its own `Arc` clone of the entry's
        // lock, independent of the descriptor table itself -- see `EntryHandle`'s doc comment)
        // and drop BOTH the `daemon_pty_masters` and the shim-wide `descriptors` table read
        // guards before blocking below. `end.read(&cx, buf)` can block indefinitely (until the
        // guest writes to the pty), and the shim-wide `descriptor_table()`/`descriptor_table_mut()`
        // lock is a single global `RwLock` shared by every fd in the whole process -- holding its
        // read guard across an indefinite block starves any concurrent guest syscall that needs
        // `descriptor_table_mut()` (e.g. `open`/`close`/`dup`), which the guest thread routinely
        // does during ordinary program startup (dynamic-library loading, `ls`'s `opendir`, etc).
        // That produced a real, intermittent (guest-syscall-timing-dependent) full deadlock: this
        // reader thread parked forever waiting for pty output, while the guest thread sat parked
        // forever waiting for a write lock this thread was still holding. Getting the handle then
        // dropping the table guards before the blocking call fixes it.
        let handle = {
            let masters = self.0.daemon_pty_masters.read();
            let master = masters.get(&pty_id).ok_or(Errno::ENXIO)?;
            self.0
                .litebox
                .descriptor_table()
                .entry_handle(master)
                .ok_or(Errno::ENXIO)?
        };
        let wait_state = litebox::event::wait::WaitState::new(self.0.platform);
        let cx = wait_state.context();
        handle.with_entry(|end: &syscalls::pty::PtyEnd<Platform>| end.read(&cx, buf))
    }

    /// Write bytes to the master side of a pty allocated via [`Self::load_program_attach_pty`].
    /// See [`Self::pty_master_read`]'s doc comment for the threading/host-caller rationale AND
    /// (as of the fix noted there) the lock-ordering rationale for resolving the `EntryHandle`
    /// and dropping the table guards before the blocking `end.write(&cx, buf)` call below.
    pub fn pty_master_write(&self, pty_id: u32, buf: &[u8]) -> Result<usize, Errno> {
        let handle = {
            let masters = self.0.daemon_pty_masters.read();
            let master = masters.get(&pty_id).ok_or(Errno::ENXIO)?;
            self.0
                .litebox
                .descriptor_table()
                .entry_handle(master)
                .ok_or(Errno::ENXIO)?
        };
        let wait_state = litebox::event::wait::WaitState::new(self.0.platform);
        let cx = wait_state.context();
        handle.with_entry(|end: &syscalls::pty::PtyEnd<Platform>| end.write(&cx, buf))
    }

    /// Constructs a `LinuxShimEntrypoints`/`Task` for a process-based-fork child whose guest
    /// memory is ALREADY fully populated at the correct addresses (by an external mechanism --
    /// e.g. a `WriteProcessMemory` copy into a separately-spawned Windows process) and whose
    /// `PageManager` has already been reconstructed to describe that memory (e.g. via
    /// [`litebox::mm::PageManager::new_adopting_existing_memory`]), rather than freshly allocated
    /// and ELF-loaded the way [`Self::load_program`] does.
    ///
    /// This is the process-based-fork analogue of `do_clone`'s real, same-process, thread-based
    /// fork path (`Task::do_clone`'s `CloneFlags::empty()` branch), which likewise never calls
    /// `load_program`/ELF-loads a forked child -- it constructs a `Task` directly from the
    /// parent's already-running state. The difference here is that there is no parent `Task` in
    /// this process to copy from (the parent lives in a different OS process); every field is
    /// built fresh from the caller-supplied `pm`/`pid`/`ppid`/credentials, mirroring a stdio-only,
    /// single-thread, freshly-execve'd-looking process shape.
    ///
    /// Returns bare `LinuxShimEntrypoints`, not a `LoadedProgram` -- there is no ELF-derived
    /// initial register state to report (the caller already has the forked child's own translated
    /// `PtRegs`, captured at the parent's `fork()` call site) and no argv/envp/entry point to
    /// resolve.
    pub fn adopt_forked_process(
        &self,
        fs: alloc::sync::Arc<FS>,
        task: litebox_common_linux::TaskParams,
        pm: PageManager<Platform, PAGE_SIZE>,
    ) -> LinuxShimEntrypoints<Platform, FS> {
        let litebox_common_linux::TaskParams {
            pid,
            ppid,
            uid,
            euid,
            gid,
            egid,
        } = task;
        let files = syscalls::file::FilesState::new(fs);
        files.set_max_fd(syscalls::process::RLIMIT_NOFILE_CUR - 1);
        let files = Arc::new(files);
        files.initialize_stdio_in_shared_descriptors_table(&self.0);

        let shared_pending = Arc::new(litebox::sync::Mutex::new(
            syscalls::signal::PendingSignals::new(),
        ));

        LinuxShimEntrypoints {
            _not_send: core::marker::PhantomData,
            task: Task {
                global: self.0.clone(),
                thread: syscalls::process::ThreadState::new_process(
                    pid,
                    pm,
                    false,
                    None,
                    shared_pending.clone(),
                ),
                wait_state: wait::WaitState::new(self.0.platform),
                pid,
                ppid,
                tid: pid,
                credentials: syscalls::process::Credentials {
                    uid,
                    euid,
                    gid,
                    egid,
                }
                .into(),
                comm: [0; litebox_common_linux::TASK_COMM_LEN].into(),
                fs: Arc::new(syscalls::file::FsState::new()).into(),
                files: files.into(),
                signals: syscalls::signal::SignalState::new_process(shared_pending),
                attached_pty_id: Cell::new(None),
            },
        }
    }

    /// Get the page manager for the shim's bootstrap process (the one created by the first
    /// `load_program` call).
    ///
    /// # Panics
    ///
    /// Panics if `load_program` has not been called yet.
    ///
    /// Only meaningful on single-process targets (e.g. `litebox_runner_snp`'s kernel-context
    /// page-fault handler, which has no `Task` in scope): does not generalize to targets with
    /// multiple processes (real `fork()`), which each have their own independent page manager
    /// reachable only via a `Task`.
    pub fn page_manager(&self) -> &PageManager<Platform, PAGE_SIZE> {
        &self
            .0
            .bootstrap_process
            .get()
            .expect("load_program has not been called yet")
            .pm
    }

    /// Perform queued network interactions with the outside world.
    ///
    /// This function should be invoked in a loop, based on the returned advice.
    pub fn perform_network_interaction(
        &self,
    ) -> litebox::net::PlatformInteractionReinvocationAdvice {
        self.0.net.lock().perform_platform_interaction()
    }

    /// Establish a TCP connection to the given address.
    ///
    /// Returns a [`transport::ShimTransport`] that can be used as a
    /// byte-stream transport (e.g., for a 9P filesystem client).
    pub fn tcp_connection(
        &self,
        addr: core::net::SocketAddr,
    ) -> Result<transport::ShimTransport<Platform>, Errno> {
        transport::ShimTransport::connect(self.0.clone(), addr)
    }

    pub fn litebox(&self) -> &LiteBox<Platform> {
        &self.0.litebox
    }

    /// Returns the platform this shim was built with.
    pub fn platform(&self) -> &'static Platform {
        self.0.platform
    }
}

pub struct LoadedProgram<Platform: ShimPlatform, FS: ShimFS> {
    pub entrypoints: LinuxShimEntrypoints<Platform, FS>,
    pub process: LinuxShimProcess<Platform>,
}

/// A handle to a process loaded via [`LinuxShim::load_program`].
///
/// This can be used to wait for the process to exit.
pub struct LinuxShimProcess<Platform: ShimPlatform>(Arc<syscalls::process::Process<Platform>>);

impl<Platform: ShimPlatform> LinuxShimProcess<Platform> {
    /// Wait for the process to exit, returning its exit code.
    pub fn wait(&self) -> i32 {
        match self.0.wait_for_exit() {
            syscalls::process::ExitStatus::Exit(v) => v.into(),
            // TODO: return the enum instead of just a code?
            syscalls::process::ExitStatus::Signal(signal) => signal.as_i32() + 256,
        }
    }

    /// Waits for the process to exit, then returns its exit status encoded into a raw Windows
    /// exit code via the SAME scheme `syscalls::process::sys_wait4`'s cross-process branch
    /// decodes (see `syscalls::process::decode_cross_process_wait_status`'s doc comment): high 16
    /// bits `0xC0DE`, bit 15 set for `Signal`, low 8 bits the exit code or signal number.
    ///
    /// This is pass 142's production call site for that encoding -- a `LITEBOX_PROCESS_FORK=1`
    /// cross-process fork() child (built via [`LinuxShim::adopt_forked_process`], resumed via
    /// `litebox_platform_windows_userland::run_thread`) has no other way to deliver its real Linux
    /// exit status to the parent's `wait4()`: this process IS a bare re-exec of the litebox
    /// runner binary with no guest tar/CLI args of its own, so its normal Rust `main()` return
    /// would otherwise exit 0 regardless of what the guest actually did. The caller is expected to
    /// pass this value directly to `std::process::exit`.
    pub fn wait_for_encoded_cross_process_exit_status(&self) -> u32 {
        const CROSS_PROCESS_EXIT_MARKER: u32 = 0xC0DE_0000;
        const CROSS_PROCESS_EXIT_SIGNAL_FLAG: u32 = 0x0000_8000;
        match self.0.wait_for_exit() {
            syscalls::process::ExitStatus::Exit(code) => {
                CROSS_PROCESS_EXIT_MARKER | (u32::from(code.cast_unsigned()) & 0xff)
            }
            syscalls::process::ExitStatus::Signal(sig) => {
                CROSS_PROCESS_EXIT_MARKER
                    | CROSS_PROCESS_EXIT_SIGNAL_FLAG
                    | (sig.as_i32().cast_unsigned() & 0xff)
            }
        }
    }
}

/// Create a default layered file system with the given in-memory layer and tar data.
fn default_fs<Platform: ShimPlatform>(
    litebox: &LiteBox<Platform>,
    in_mem_fs: litebox::fs::in_mem::FileSystem<Platform>,
    tar_data: Cow<'static, [u8]>,
) -> LinuxFS<Platform> {
    let dev_stdio = litebox::fs::resolver::Resolver::new(
        litebox,
        litebox::fs::composer::Composer::builder()
            .mount("/dev", |allocator| {
                litebox::fs::devices::Devices::new(litebox, allocator)
            })
            .build()
            .unwrap(),
    );
    let tar_ro = litebox::fs::resolver::Resolver::new(
        litebox,
        litebox::fs::composer::Composer::builder()
            .mount("/", |allocator| {
                litebox::fs::tar_ro::TarRo::new(tar_data, allocator)
            })
            .build()
            .unwrap(),
    );
    litebox::fs::layered::FileSystem::new(
        litebox,
        in_mem_fs,
        litebox::fs::layered::FileSystem::new(
            litebox,
            dev_stdio,
            tar_ro,
            litebox::fs::layered::LayeringSemantics::LowerLayerReadOnly,
        ),
        litebox::fs::layered::LayeringSemantics::LowerLayerWritableFiles,
    )
}

// Special override so that `GETFL` can return stdio-specific flags
#[derive(Clone)]
pub(crate) struct StdioStatusFlags(litebox::fs::OFlags);

/// Per-fd `termios` state, as last set by `ioctl(TCSETS|TCSETSW|TCSETSF)`.
///
/// LiteBox has no real POSIX termios layer underneath on every platform, so a stdio fd's
/// raw/cooked mode is tracked purely as in-memory state: `TCSETS*` stores the guest-requested
/// flags here, and `TCGETS` reads them back, so a `tcgetattr`/`tcsetattr`/`tcgetattr` round-trip
/// (as performed by libuv's `uv__tty_make_raw` to save and later restore terminal state around
/// raw mode) observes self-consistent state.
#[derive(Clone)]
pub(crate) struct TermiosState(pub(crate) litebox_common_linux::Termios);

impl Default for TermiosState {
    fn default() -> Self {
        Self(litebox_common_linux::Termios {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0,
            c_line: 0,
            c_cc: [0; 19],
        })
    }
}

/// A tty fd's foreground process group ID, as last set by `ioctl(TIOCSPGRP)` (`tcsetpgrp`).
///
/// LiteBox has no real POSIX tty-driver layer underneath, so -- mirroring [`TermiosState`] --
/// this tracks the guest-requested foreground pgid purely as in-memory per-fd state:
/// `TIOCSPGRP` stores it here and `TIOCGPGRP` reads it back. There is no entry in the
/// descriptor table until the first `TIOCSPGRP`/explicit initialization, so callers reading
/// before any write fall back to the calling process's own pgid, matching real Linux's
/// default (a freshly opened controlling terminal's foreground group is the opening process's
/// own group).
#[derive(Clone, Copy)]
pub(crate) struct ForegroundPgid(pub(crate) i32);

impl<Platform: ShimPlatform, FS: ShimFS> syscalls::file::FilesState<Platform, FS> {
    fn initialize_stdio_in_shared_descriptors_table(&self, global: &GlobalState<Platform, FS>) {
        use litebox::fs::{Mode, OFlags};
        let stdin = self
            .fs
            .open("/dev/stdin", OFlags::RDONLY, Mode::empty())
            .unwrap();
        let stdout = self
            .fs
            .open("/dev/stdout", OFlags::WRONLY, Mode::empty())
            .unwrap();
        let stderr = self
            .fs
            .open("/dev/stderr", OFlags::WRONLY, Mode::empty())
            .unwrap();
        let mut dt = global.litebox.descriptor_table_mut();
        let mut rds = self.raw_descriptor_store.write();
        for (raw_fd, fd, stream) in [
            (0, stdin, litebox::platform::StdioStream::Stdin),
            (1, stdout, litebox::platform::StdioStream::Stdout),
            (2, stderr, litebox::platform::StdioStream::Stderr),
        ] {
            let status_flags = OFlags::APPEND | OFlags::RDWR;
            debug_assert_eq!(OFlags::STATUS_FLAGS_MASK & status_flags, status_flags);
            let old = dt.set_entry_metadata(&fd, StdioStatusFlags(status_flags));
            assert!(old.is_none());
            let old = dt.set_entry_metadata(&fd, stream);
            assert!(old.is_none());
            let success = rds.fd_into_specific_raw_integer(fd, raw_fd);
            assert!(success);
        }
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    fn close_on_exec(&self) {
        let files = self.files.borrow();
        let alive_fds: Vec<usize> = files.raw_descriptor_store.read().iter_alive().collect();
        for raw_fd in alive_fds {
            if let Ok(flags) = get_file_descriptor_flags(raw_fd, &self.global, &files)
                && flags.contains(litebox_common_linux::FileDescriptorFlags::FD_CLOEXEC)
            {
                let _ = self.do_close(raw_fd);
            }
        }
    }

    /// Closes every fd this process's (now-exiting) fd table still holds open.
    ///
    /// Real Linux implicitly closes every fd a process holds when its last thread exits (the
    /// kernel drops the process's fd table, releasing each open file description's reference).
    /// This shim's fd bookkeeping does not get that for free: `raw_descriptor_store`'s entries
    /// are plain `OwnedFd` tokens indexing into the single process-wide `descriptor_table()`
    /// (see `litebox::fd::Descriptors`), and `OwnedFd`'s own `Drop` impl deliberately does *not*
    /// close its slot -- it only asserts the fd was already closed via a real `close()`/`dup2()`
    /// operation (see its doc comment / `panic_on_unclosed_fd_drop`). Before this fix, nothing
    /// ever called that real close path on process exit: `Task::prepare_for_exit` handled thread
    /// detachment, orphan reparenting, `clear_child_tid`, and `robust_list`, but never iterated
    /// and closed the process's own fds, so every fd a process held leaked forever in the global
    /// descriptor table once the process exited without individually `close()`-ing each one
    /// itself (which real programs routinely never bother to do before `_exit()`/`exit_group()`,
    /// relying on the kernel to do it for them, exactly as this fix now does too).
    ///
    /// This is a real, reproduced hang, not a theoretical gap: a pipe's read end only observes
    /// EOF once every writer-side open file description is gone (`ReadEnd::is_peer_shutdown`,
    /// which checks whether the peer `WriteEnd`'s `Arc` strong count has reached zero via
    /// `Weak::upgrade` -- see `litebox/src/pipes.rs`). `sh -c "timeout 5 tar -tzf
    /// <2-gzip-member.tar.gz>"` deterministically hangs because `tar`'s internal gzip
    /// decompression forks a helper that pipes decompressed bytes back to the parent; once that
    /// helper finishes and calls `exit_group()`, its copy of the pipe's write end was never
    /// closed by this shim, so the `Arc<WriteEnd>` never drops, `is_peer_shutdown()` never
    /// becomes true, and the parent's blocking `read()` on the pipe waits for an EOF that can now
    /// never arrive -- even though the actual Linux kernel semantics this shim is emulating
    /// guarantee that EOF the instant the last writer process exits, without that process ever
    /// calling `close()` itself. The same leak applies to every other fd-backed resource this
    /// shim has (regular files, sockets, eventfds, epoll instances, unix sockets): none of them
    /// were ever released on ordinary process exit.
    ///
    /// Only called once, from `Task::prepare_for_exit`, and only when `process_exited` is `true`
    /// (this was the process's last thread) -- other threads of a still-live multi-threaded
    /// process share this same fd table (`CLONE_FILES`), so closing fds when just one of several
    /// threads exits would incorrectly yank descriptors out from under sibling threads that are
    /// still running.
    fn close_all_fds_on_process_exit(&self) {
        let files = self.files.borrow();
        let alive_fds: Vec<usize> = files.raw_descriptor_store.read().iter_alive().collect();
        for raw_fd in alive_fds {
            // Capture the pty pair (if any) this raw fd refers to BEFORE closing it: real Linux
            // delivers a pty slave hangup (waking a thread blocked reading the master) the instant
            // the last process holding an open slave fd terminates -- unconditionally, whether or
            // not that process bothered to `close()` its fds itself first (see this function's
            // own doc comment for the identical, already-fixed pipe-EOF case). This shim's own
            // `ptmx_open` registry keeps one extra `Arc` reference to the slave alive purely so
            // `pts_open`/`/dev/pts/<id>` can still work (real Linux devpts allows exactly this:
            // reopening a pty whose slave has no current opens, e.g. a detached tmux session), so
            // an ordinary mid-life `close()` of the last real slave fd must NOT itself force a
            // wakeup -- only true process death should. `close_all_fds_on_process_exit`'s own
            // precondition (only ever called once, at real process exit, per its doc comment)
            // makes this the correct and only place to apply that "process actually died" signal,
            // without touching `do_close`'s ordinary semantics at all (a real, previously
            // reproduced hang: a `pty.fork()`-style child crashing before its parent ever closes
            // the master left the registry's own template `Arc` reference as the sole remaining
            // one, since only the master's own close ever drops it -- the master's blocking
            // `read()` then waited forever for an EOF that could now never arrive).
            let slave_pair = files.run_on_raw_fd(
                raw_fd,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |_| None,
                |fd: &litebox::fd::TypedFd<syscalls::pty::PtySubsystem<Platform>>| {
                    self.global
                        .litebox
                        .descriptor_table()
                        .entry_handle(fd)
                        .and_then(|h| {
                            h.with_entry(|end: &syscalls::pty::PtyEnd<Platform>| {
                                end.is_slave().then(|| end.pair().clone())
                            })
                        })
                },
            );
            let _ = self.do_close(raw_fd);
            if let Ok(Some(pair)) = slave_pair {
                self.global.hangup_slave(&pair);
            }
        }
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> syscalls::file::FilesState<Platform, FS> {
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn run_on_raw_fd<R>(
        &self,
        fd: usize,
        fs: impl FnOnce(&TypedFd<FS>) -> R,
        net: impl FnOnce(&TypedFd<Network<Platform>>) -> R,
        pipes: impl FnOnce(&TypedFd<Pipes<Platform>>) -> R,
        eventfd: impl FnOnce(&TypedFd<syscalls::eventfd::EventfdSubsystem<Platform>>) -> R,
        epoll: impl FnOnce(&TypedFd<syscalls::epoll::EpollSubsystem<Platform, FS>>) -> R,
        unix: impl FnOnce(&TypedFd<syscalls::unix::UnixSocketSubsystem<Platform, FS>>) -> R,
        pty: impl FnOnce(&TypedFd<syscalls::pty::PtySubsystem<Platform>>) -> R,
    ) -> Result<R, Errno> {
        let rds = self.raw_descriptor_store.read();
        if let Ok(fd) = rds.fd_from_raw_integer(fd) {
            drop(rds);
            return Ok(fs(&fd));
        }
        if let Ok(fd) = rds.fd_from_raw_integer(fd) {
            drop(rds);
            return Ok(net(&fd));
        }
        if let Ok(fd) = rds.fd_from_raw_integer(fd) {
            drop(rds);
            return Ok(pipes(&fd));
        }
        if let Ok(fd) = rds.fd_from_raw_integer(fd) {
            drop(rds);
            return Ok(eventfd(&fd));
        }
        if let Ok(fd) = rds.fd_from_raw_integer(fd) {
            drop(rds);
            return Ok(epoll(&fd));
        }
        if let Ok(fd) = rds.fd_from_raw_integer(fd) {
            drop(rds);
            return Ok(unix(&fd));
        }
        if let Ok(fd) = rds.fd_from_raw_integer(fd) {
            drop(rds);
            return Ok(pty(&fd));
        }
        Err(Errno::EBADF)
    }
}

// This places size limits on maximum read/write sizes that might occur; it exists primarily to
// prevent OOM due to the user asking for a _massive_ read or such at once. Keeping this too small
// has the downside of requiring too many syscalls, while having it be too large allows for massive
// allocations to be triggered by the userland program. For now, this is set to a
// hopefully-reasonable middle ground.
const MAX_KERNEL_BUF_SIZE: usize = 0x80_000;

trait ToSyscallResult {
    fn to_syscall_result(self) -> Result<usize, Errno>;
}
impl ToSyscallResult for Result<(), Errno> {
    fn to_syscall_result(self) -> Result<usize, Errno> {
        self.map(|()| 0)
    }
}
impl ToSyscallResult for Result<usize, Errno> {
    fn to_syscall_result(self) -> Result<usize, Errno> {
        self
    }
}
impl ToSyscallResult for Result<u32, Errno> {
    fn to_syscall_result(self) -> Result<usize, Errno> {
        self.map(|v| v as usize)
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// A wrapper function around `sys_pread64` that copies data in chunks to avoid OOMing.
    fn pread_with_user_buf(
        &self,
        fd: i32,
        buf: UserPtrMut<u8>,
        count: usize,
        offset: i64,
    ) -> Result<usize, Errno> {
        let mut kernel_buf = vec![0u8; count.min(MAX_KERNEL_BUF_SIZE)];
        let mut read_total = 0;
        while read_total < count {
            let to_read = (count - read_total).min(kernel_buf.len());
            match self.sys_pread64(
                fd,
                &mut kernel_buf[..to_read],
                offset + (read_total.reinterpret_as_signed() as i64),
            ) {
                Ok(0) => break, // EOF
                Ok(size) => {
                    buf.copy_from_slice::<Platform>(read_total, &kernel_buf[..size])
                        .ok_or(Errno::EFAULT)?;
                    read_total += size;
                }
                Err(e) => return Err(e),
            }
        }
        assert!(read_total <= count);
        Ok(read_total)
    }

    /// A single, size-bounded `read(2)` for a non-seekable fd (pipe/socket/eventfd/pty/etc) whose
    /// guest-requested `count` exceeds [`MAX_KERNEL_BUF_SIZE`].
    ///
    /// Unlike [`Self::pread_with_user_buf`] (used for regular files), this doesn't loop to fill
    /// the whole requested `count`: a non-seekable fd has no file offset to preserve across
    /// chunks, and real `read(2)` semantics for a pipe/socket/pty return as soon as *any* data is
    /// available rather than blocking to accumulate a specific amount -- looping here would mean
    /// blocking indefinitely once the peer goes idle, well past what the guest actually asked to
    /// wait for. Capping to a single bounded read is both correct and sufficient to avoid the
    /// unbounded kernel-side allocation a naive `vec![0u8; count]` would otherwise need for an
    /// arbitrarily large guest-requested `count`.
    fn read_with_user_buf_no_offset(
        &self,
        fd: i32,
        buf: UserPtrMut<u8>,
        count: usize,
    ) -> Result<usize, Errno> {
        let mut kernel_buf = vec![0u8; count.min(MAX_KERNEL_BUF_SIZE)];
        let size = self.sys_read(fd, &mut kernel_buf, None)?;
        buf.copy_from_slice::<Platform>(0, &kernel_buf[..size])
            .ok_or(Errno::EFAULT)?;
        Ok(size)
    }

    /// Handle Linux syscalls and dispatch them to LiteBox implementations.
    ///
    /// # Panics
    ///
    /// Unsupported syscalls or arguments would trigger a panic for development purposes.
    fn handle_syscall_request(&self, ctx: &mut litebox_common_linux::PtRegs) {
        let return_value = match self.do_syscall(ctx) {
            Ok(v) => v,
            Err(err) => (err.as_neg() as isize).reinterpret_as_unsigned(),
        };
        #[cfg(target_arch = "x86_64")]
        {
            ctx.rax = return_value;
        }
        #[cfg(target_arch = "aarch64")]
        {
            ctx.regs[0] = return_value;
        }
    }

    fn do_syscall(&self, ctx: &mut litebox_common_linux::PtRegs) -> Result<usize, Errno> {
        // Helper macro to unify the return value from `sys_*`.
        macro_rules! syscall {
            ($func:ident($($args:expr),*)) => {
                self.$func($($args),*).to_syscall_result()
            };
        }

        #[cfg(target_arch = "x86_64")]
        let syscall_number = ctx.orig_rax;
        #[cfg(target_arch = "aarch64")]
        let syscall_number = ctx.syscallno.reinterpret_as_unsigned() as usize;
        let request = SyscallRequest::try_from_raw(syscall_number, ctx, log_unsupported_fmt)?;
        if matches!(
            request,
            SyscallRequest::Clone { .. }
                | SyscallRequest::Clone3 { .. }
                | SyscallRequest::Execve { .. }
                | SyscallRequest::Wait4 { .. }
                | SyscallRequest::Exit { .. }
                | SyscallRequest::ExitGroup { .. }
                | SyscallRequest::Openat { .. }
                | SyscallRequest::Close { .. }
                | SyscallRequest::Mkdirat { .. }
                | SyscallRequest::Renameat { .. }
                | SyscallRequest::Symlinkat { .. }
                | SyscallRequest::Ftruncate { .. }
                | SyscallRequest::Unlinkat { .. }
                | SyscallRequest::Write { .. }
                | SyscallRequest::Writev { .. }
                | SyscallRequest::Read { .. }
                | SyscallRequest::Readv { .. }
                | SyscallRequest::Ioctl { .. }
                | SyscallRequest::Ppoll { .. }
        ) {
            litebox_util_log::trace!(request:? = request; "syscall");
        }

        match request {
            SyscallRequest::Exit { status } => {
                self.sys_exit(status);
                Ok(0)
            }
            SyscallRequest::ExitGroup { status } => {
                self.sys_exit_group(status);
                Ok(0)
            }
            SyscallRequest::Execve {
                pathname,
                argv,
                envp,
            } => self.sys_execve(pathname, argv, envp, ctx),
            SyscallRequest::Read { fd, buf, count } => {
                // Note some applications (e.g., `node`) seem to assume that getting fewer bytes than
                // requested indicates EOF.
                if count <= MAX_KERNEL_BUF_SIZE {
                    let mut kernel_buf = vec![0u8; count.min(MAX_KERNEL_BUF_SIZE)];
                    self.sys_read(fd, &mut kernel_buf, None).and_then(|size| {
                        buf.copy_from_slice::<Platform>(0, &kernel_buf[..size])
                            .map(|()| size)
                            .ok_or(Errno::EFAULT)
                    })
                } else {
                    // If the read size is too large, we need to do some extra work to avoid OOMing.
                    // For a seekable fd (a regular file), read data in chunks and update the file
                    // offset ourselves only if the read succeeds. A non-seekable fd
                    // (pipe/socket/eventfd/pty/etc, `ESPIPE`) has no file offset to preserve, so
                    // it takes a simpler single-bounded-read path instead (see
                    // `read_with_user_buf_no_offset`) -- this used to unconditionally panic,
                    // crashing the whole runner on something as ordinary as a single large
                    // `read()` of a subprocess's stdout pipe or a socket.
                    match self.sys_lseek(fd, 0, litebox::fs::SeekWhence::RelativeToCurrentOffset) {
                        Ok(cur_loc) => self
                            .pread_with_user_buf(fd, buf, count, i64::try_from(cur_loc).unwrap())
                            .inspect(|read_total| {
                                // Update the file offset to reflect the read we just did.
                                self.sys_lseek(
                                    fd,
                                    (cur_loc + read_total).reinterpret_as_signed(),
                                    litebox::fs::SeekWhence::RelativeToBeginning,
                                )
                                // Given that previous lseek and pread succeeded, this lseek should also succeed.
                                .expect("lseek failed");
                            }),
                        Err(Errno::EBADF) => Err(Errno::EBADF),
                        Err(Errno::ESPIPE) => self.read_with_user_buf_no_offset(fd, buf, count),
                        Err(Errno::EINVAL) => {
                            unreachable!(
                                "seekable file should not return EINVAL when getting current offset"
                            );
                        }
                        Err(e) => {
                            unimplemented!("unexpected error from lseek: {}", e);
                        }
                    }
                }
            }
            SyscallRequest::Write { fd, buf, count } => match buf.to_owned_slice::<Platform>(count)
            {
                Some(buf) => self.sys_write(fd, &buf, None),
                None => Err(Errno::EFAULT),
            },
            SyscallRequest::Close { fd } => syscall!(sys_close(fd)),
            SyscallRequest::Lseek { fd, offset, whence } => {
                use litebox::utils::TruncateExt as _;
                syscalls::file::try_into_whence(whence.trunc())
                    .map_err(|_| Errno::EINVAL)
                    .and_then(|seekwhence| self.sys_lseek(fd, offset, seekwhence))
            }
            SyscallRequest::Mkdirat {
                dirfd,
                pathname,
                mode,
            } => pathname
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EFAULT), |path| {
                    syscall!(sys_mkdirat(dirfd, path, mode))
                }),
            SyscallRequest::Fchmodat {
                dirfd,
                pathname,
                mode,
            } => pathname
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EFAULT), |path| {
                    syscall!(sys_fchmodat(dirfd, path, mode))
                }),
            SyscallRequest::Fchmod { fd, mode } => syscall!(sys_fchmod(fd, mode)),
            SyscallRequest::Chdir { pathname } => pathname
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EINVAL), |path| syscall!(sys_chdir(path))),
            SyscallRequest::Fchdir { fd } => syscall!(sys_fchdir(fd)),
            SyscallRequest::RtSigprocmask {
                how,
                set,
                oldset,
                sigsetsize,
            } => self.sys_rt_sigprocmask(how, set, oldset, sigsetsize),
            SyscallRequest::RtSigaction {
                signum,
                act,
                oldact,
                sigsetsize,
            } => self.sys_rt_sigaction(signum, act, oldact, sigsetsize),
            SyscallRequest::RtSigreturn => self.sys_rt_sigreturn(ctx),
            SyscallRequest::Ioctl { fd, arg } => syscall!(sys_ioctl(fd, arg)),
            SyscallRequest::Pread64 {
                fd,
                buf,
                count,
                offset,
            } => self.pread_with_user_buf(fd, buf, count, offset),
            SyscallRequest::Pwrite64 {
                fd,
                buf,
                count,
                offset,
            } => match buf.to_owned_slice::<Platform>(count) {
                Some(buf) => self.sys_pwrite64(fd, &buf, offset),
                None => Err(Errno::EFAULT),
            },
            SyscallRequest::Sendfile {
                out_fd,
                in_fd,
                offset,
                count,
            } => syscall!(sys_sendfile(out_fd, in_fd, offset, count)),
            SyscallRequest::Mmap {
                addr,
                length,
                prot,
                flags,
                fd,
                offset,
            } => self
                .sys_mmap(addr, length, prot, flags, fd, offset)
                .map(|ptr| ptr.as_usize()),
            SyscallRequest::Mprotect { addr, length, prot } => {
                syscall!(sys_mprotect(addr, length, prot))
            }
            SyscallRequest::Mremap {
                old_addr,
                old_size,
                new_size,
                flags,
                new_addr,
            } => self
                .sys_mremap(old_addr, old_size, new_size, flags, new_addr)
                .map(|ptr| ptr.as_usize()),
            SyscallRequest::Munmap { addr, length } => syscall!(sys_munmap(addr, length)),
            SyscallRequest::Brk { addr } => self.sys_brk(addr),
            SyscallRequest::Readv { fd, iovec, iovcnt } => self.sys_readv(fd, iovec, iovcnt),
            SyscallRequest::Writev { fd, iovec, iovcnt } => self.sys_writev(fd, iovec, iovcnt),
            SyscallRequest::Preadv {
                fd,
                iovec,
                iovcnt,
                pos_l,
                pos_h,
            } => self.sys_preadv(fd, iovec, iovcnt, preadv_pwritev_offset(pos_l, pos_h)),
            SyscallRequest::Pwritev {
                fd,
                iovec,
                iovcnt,
                pos_l,
                pos_h,
            } => self.sys_pwritev(fd, iovec, iovcnt, preadv_pwritev_offset(pos_l, pos_h)),
            SyscallRequest::Faccessat {
                dirfd,
                pathname,
                mode,
                flags,
            } => pathname
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EFAULT), |path| {
                    syscall!(sys_faccessat(dirfd, path, mode, flags))
                }),
            SyscallRequest::Madvise {
                addr,
                length,
                behavior,
            } => syscall!(sys_madvise(addr, length, behavior)),
            SyscallRequest::Dup {
                oldfd,
                newfd,
                flags,
            } => syscall!(sys_dup(oldfd, newfd, flags)),
            SyscallRequest::Socket {
                domain,
                type_and_flags,
                protocol,
            } => syscall!(sys_socket(domain, type_and_flags, protocol)),
            SyscallRequest::Socketpair {
                domain,
                type_and_flags,
                protocol,
                sockvec,
            } => syscall!(sys_socketpair(domain, type_and_flags, protocol, sockvec)),
            SyscallRequest::Connect {
                sockfd,
                sockaddr,
                addrlen,
            } => syscall!(sys_connect(sockfd, sockaddr, addrlen)),
            SyscallRequest::Accept {
                sockfd,
                addr,
                addrlen,
                flags,
            } => syscall!(sys_accept(sockfd, addr, addrlen, flags)),
            SyscallRequest::Sendto {
                sockfd,
                buf,
                len,
                flags,
                addr,
                addrlen,
            } => self.sys_sendto(sockfd, buf, len, flags, addr, addrlen),
            SyscallRequest::Sendmsg { sockfd, msg, flags } => self.sys_sendmsg(sockfd, msg, flags),
            SyscallRequest::Sendmmsg {
                sockfd,
                msgvec,
                vlen,
                flags,
            } => self.sys_sendmmsg(sockfd, msgvec, vlen, flags),
            SyscallRequest::Recvfrom {
                sockfd,
                buf,
                len,
                flags,
                addr,
                addrlen,
            } => self.sys_recvfrom(sockfd, buf, len, flags, addr, addrlen),
            SyscallRequest::Recvmsg { sockfd, msg, flags } => self.sys_recvmsg(sockfd, msg, flags),
            SyscallRequest::Recvmmsg {
                sockfd,
                msgvec,
                vlen,
                flags,
                timeout,
            } => self.sys_recvmmsg(sockfd, msgvec, vlen, flags, timeout),
            SyscallRequest::Shutdown { sockfd, how } => syscall!(sys_shutdown(sockfd, how)),
            SyscallRequest::Bind {
                sockfd,
                sockaddr,
                addrlen,
            } => syscall!(sys_bind(sockfd, sockaddr, addrlen)),
            SyscallRequest::Listen { sockfd, backlog } => {
                syscall!(sys_listen(sockfd, backlog))
            }
            SyscallRequest::Setsockopt {
                sockfd,
                level,
                optname,
                optval,
                optlen,
            } => syscall!(sys_setsockopt(sockfd, level, optname, optval, optlen)),
            SyscallRequest::Getsockopt {
                sockfd,
                level,
                optname,
                optval,
                optlen,
            } => syscall!(sys_getsockopt(sockfd, level, optname, optval, optlen)),
            SyscallRequest::Getsockname {
                sockfd,
                addr,
                addrlen,
            } => syscall!(sys_getsockname(sockfd, addr, addrlen)),
            SyscallRequest::Getpeername {
                sockfd,
                addr,
                addrlen,
            } => syscall!(sys_getpeername(sockfd, addr, addrlen)),
            SyscallRequest::Uname { buf } => syscall!(sys_uname(buf)),
            SyscallRequest::Fcntl { fd, arg } => syscall!(sys_fcntl(fd, arg)),
            SyscallRequest::Flock { fd, operation } => syscall!(sys_flock(fd, operation)),
            SyscallRequest::Getcwd { buf, size: count } => {
                let mut kernel_buf = vec![0u8; count.min(MAX_KERNEL_BUF_SIZE)];
                self.sys_getcwd(&mut kernel_buf).and_then(|size| {
                    buf.copy_from_slice::<Platform>(0, &kernel_buf[..size])
                        .map(|()| size)
                        .ok_or(Errno::EFAULT)
                })
            }
            SyscallRequest::EpollCtl {
                epfd,
                op,
                fd,
                event,
            } => syscall!(sys_epoll_ctl(epfd, op, fd, event)),
            SyscallRequest::EpollCreate { size, flags } => {
                // the `size` argument is ignored, but must be greater than zero;
                if size > 0 {
                    syscall!(sys_epoll_create(flags))
                } else {
                    Err(Errno::EINVAL)
                }
            }
            SyscallRequest::EpollPwait {
                epfd,
                events,
                maxevents,
                timeout,
                sigmask,
                sigsetsize,
            } => self.sys_epoll_pwait(epfd, events, maxevents, timeout, sigmask, sigsetsize),
            SyscallRequest::Prctl { args } => self.sys_prctl(args),
            SyscallRequest::ArchPrctl { arg } => syscall!(sys_arch_prctl(arg)),
            SyscallRequest::Readlink {
                pathname,
                buf,
                bufsiz,
            } => pathname
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EFAULT), |path| {
                    let mut kernel_buf = vec![0u8; bufsiz.min(MAX_KERNEL_BUF_SIZE)];
                    self.sys_readlink(path, &mut kernel_buf).and_then(|size| {
                        buf.copy_from_slice::<Platform>(0, &kernel_buf[..size])
                            .map(|()| size)
                            .ok_or(Errno::EFAULT)
                    })
                }),
            SyscallRequest::Ppoll {
                fds,
                nfds,
                timeout,
                sigmask,
                sigsetsize,
            } => self.sys_ppoll(fds, nfds, timeout, sigmask, sigsetsize),
            SyscallRequest::Pselect {
                nfds,
                readfds,
                writefds,
                exceptfds,
                timeout,
                sigsetpack,
            } => self.sys_pselect(nfds, readfds, writefds, exceptfds, timeout, sigsetpack),
            SyscallRequest::Readlinkat {
                dirfd,
                pathname,
                buf,
                bufsiz,
            } => pathname
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EFAULT), |path| {
                    let mut kernel_buf = vec![0u8; bufsiz.min(MAX_KERNEL_BUF_SIZE)];
                    self.sys_readlinkat(dirfd, path, &mut kernel_buf)
                        .and_then(|size| {
                            buf.copy_from_slice::<Platform>(0, &kernel_buf[..size])
                                .map(|()| size)
                                .ok_or(Errno::EFAULT)
                        })
                }),
            SyscallRequest::Gettimeofday { tv, tz } => syscall!(sys_gettimeofday(tv, tz)),
            SyscallRequest::ClockGettime { clockid, tp } => {
                litebox_common_linux::ClockId::try_from(clockid)
                    .map_err(|_| {
                        log_unsupported!("clock_gettime(clockid = {clockid})");
                        Errno::EINVAL
                    })
                    .and_then(|clock_id| syscall!(sys_clock_gettime(clock_id, tp)))
            }
            SyscallRequest::ClockGetres { clockid, res } => {
                litebox_common_linux::ClockId::try_from(clockid)
                    .map_err(|_| {
                        log_unsupported!("clock_getres(clockid = {clockid})");
                        Errno::EINVAL
                    })
                    .and_then(|clock_id| syscall!(sys_clock_getres(clock_id, res)))
            }
            SyscallRequest::ClockNanosleep {
                clockid,
                flags,
                request,
                remain,
            } => litebox_common_linux::ClockId::try_from(clockid)
                .map_err(|_| {
                    log_unsupported!("clock_nanosleep(clockid = {clockid})");
                    Errno::EINVAL
                })
                .and_then(|clock_id| {
                    syscall!(sys_clock_nanosleep(clock_id, flags, request, remain))
                }),
            SyscallRequest::Time { tloc } => self
                .sys_time(tloc)
                .and_then(|second| usize::try_from(second).or(Err(Errno::EOVERFLOW))),
            SyscallRequest::Openat {
                dirfd,
                pathname,
                flags,
                mode,
            } => pathname
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EFAULT), |path| {
                    syscall!(sys_openat(dirfd, path, flags, mode))
                }),
            SyscallRequest::Ftruncate { fd, length } => syscall!(sys_ftruncate(fd, length)),
            SyscallRequest::Mknodat {
                dirfd,
                pathname,
                mode_and_type,
                dev,
            } => pathname
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EFAULT), |path| {
                    syscall!(sys_mknodat(dirfd, path, mode_and_type, dev))
                }),
            SyscallRequest::Unlinkat {
                dirfd,
                pathname,
                flags,
            } => pathname
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EFAULT), |path| {
                    syscall!(sys_unlinkat(dirfd, path, flags))
                }),
            SyscallRequest::Renameat {
                olddirfd,
                oldpath,
                newdirfd,
                newpath,
                flags,
            } => oldpath
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EFAULT), |oldpath| {
                    newpath
                        .to_cstring::<Platform>()
                        .map_or(Err(Errno::EFAULT), |newpath| {
                            syscall!(sys_renameat(olddirfd, oldpath, newdirfd, newpath, flags))
                        })
                }),
            SyscallRequest::Symlinkat {
                target,
                newdirfd,
                linkpath,
            } => target
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EFAULT), |target| {
                    linkpath
                        .to_cstring::<Platform>()
                        .map_or(Err(Errno::EFAULT), |linkpath| {
                            syscall!(sys_symlinkat(target, newdirfd, linkpath))
                        })
                }),
            SyscallRequest::Stat { pathname, buf } => {
                pathname
                    .to_cstring::<Platform>()
                    .map_or(Err(Errno::EFAULT), |path| {
                        self.sys_stat(path).and_then(|stat| {
                            buf.write_at_offset::<Platform>(0, stat)
                                .ok_or(Errno::EFAULT)
                                .map(|()| 0)
                        })
                    })
            }
            SyscallRequest::Lstat { pathname, buf } => {
                pathname
                    .to_cstring::<Platform>()
                    .map_or(Err(Errno::EFAULT), |path| {
                        self.sys_lstat(path).and_then(|stat| {
                            buf.write_at_offset::<Platform>(0, stat)
                                .ok_or(Errno::EFAULT)
                                .map(|()| 0)
                        })
                    })
            }
            SyscallRequest::Fstat { fd, buf } => self.sys_fstat(fd).and_then(|stat| {
                buf.write_at_offset::<Platform>(0, stat)
                    .ok_or(Errno::EFAULT)
                    .map(|()| 0)
            }),
            SyscallRequest::Newfstatat {
                dirfd,
                pathname,
                buf,
                flags,
            } => pathname
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EFAULT), |path| {
                    self.sys_newfstatat(dirfd, path, flags).and_then(|stat| {
                        buf.write_at_offset::<Platform>(0, stat)
                            .ok_or(Errno::EFAULT)
                            .map(|()| 0)
                    })
                }),
            SyscallRequest::Utimensat {
                dirfd,
                pathname,
                times,
                flags,
            } => pathname
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EFAULT), |path| {
                    let times = if times.is_null() {
                        None
                    } else {
                        let Some(atime) = times.read_at_offset::<Platform>(0) else {
                            return Err(Errno::EFAULT);
                        };
                        let Some(mtime) = times.read_at_offset::<Platform>(1) else {
                            return Err(Errno::EFAULT);
                        };
                        Some((atime, mtime))
                    };
                    syscall!(sys_utimensat(dirfd, path, times, flags))
                }),
            SyscallRequest::Statx {
                dirfd,
                pathname,
                flags,
                mask,
                statxbuf,
            } => {
                let (path, flags) = match pathname {
                    // Linux 6.11+ treats a NULL statx path as a request to stat dirfd.
                    None => (
                        Ok(c"".into()),
                        flags | litebox_common_linux::AtFlags::AT_EMPTY_PATH,
                    ),
                    Some(p) => (p.to_cstring::<Platform>().ok_or(Errno::EFAULT), flags),
                };
                path.and_then(|path| {
                    self.sys_statx(dirfd, path, flags, mask).and_then(|sx| {
                        statxbuf
                            .write_at_offset::<Platform>(0, sx)
                            .ok_or(Errno::EFAULT)
                            .map(|()| 0)
                    })
                })
            }
            SyscallRequest::Eventfd2 { initval, flags } => {
                syscall!(sys_eventfd2(initval, flags))
            }
            SyscallRequest::Pipe2 { pipefd, flags } => {
                self.sys_pipe2(flags).and_then(|(read_fd, write_fd)| {
                    pipefd
                        .write_at_offset::<Platform>(0, read_fd)
                        .ok_or(Errno::EFAULT)?;
                    pipefd
                        .write_at_offset::<Platform>(1, write_fd)
                        .ok_or(Errno::EFAULT)?;
                    Ok(0)
                })
            }
            SyscallRequest::Clone { args } => self.sys_clone(ctx, &args),
            SyscallRequest::Clone3 { args } => self.sys_clone3(ctx, args),
            SyscallRequest::SetThreadArea { user_desc } => {
                let _ = user_desc;
                Err(Errno::ENOSYS) // x86_64 does not support set_thread_area
            }
            SyscallRequest::SetTidAddress { tidptr } => {
                Ok(self.sys_set_tid_address(tidptr).reinterpret_as_unsigned() as usize)
            }
            SyscallRequest::Gettid => Ok(self.sys_gettid().reinterpret_as_unsigned() as usize),
            SyscallRequest::Getrlimit { resource, rlim } => {
                syscall!(sys_getrlimit(resource, rlim))
            }
            SyscallRequest::Setrlimit { resource, rlim } => {
                syscall!(sys_setrlimit(resource, rlim))
            }
            SyscallRequest::Prlimit {
                pid,
                resource,
                new_limit,
                old_limit,
            } => syscall!(sys_prlimit(pid, resource, new_limit, old_limit)),
            SyscallRequest::SetRobustList { head } => {
                self.sys_set_robust_list(head);
                Ok(0)
            }
            SyscallRequest::GetRobustList { pid, head, len } => self
                .sys_get_robust_list(pid, head)
                .and_then(|()| {
                    len.write_at_offset::<Platform>(
                        0,
                        size_of::<litebox_common_linux::RobustListHead>(),
                    )
                    .ok_or(Errno::EFAULT)
                })
                .map(|()| 0),
            SyscallRequest::GetRandom { buf, count, flags } => {
                self.sys_getrandom(buf, count, flags)
            }
            SyscallRequest::Getpid => Ok(self.sys_getpid().reinterpret_as_unsigned() as usize),
            SyscallRequest::Getppid => Ok(self.sys_getppid().reinterpret_as_unsigned() as usize),
            SyscallRequest::Getpgid { pid } => {
                Ok(self.sys_getpgid(pid)?.reinterpret_as_unsigned() as usize)
            }
            SyscallRequest::Setpgid { pid, pgid } => {
                self.sys_setpgid(pid, pgid)?;
                Ok(0)
            }
            SyscallRequest::Setsid => Ok(self.sys_setsid()?.reinterpret_as_unsigned() as usize),
            SyscallRequest::Getuid => Ok(self.sys_getuid() as usize),
            SyscallRequest::Getgid => Ok(self.sys_getgid() as usize),
            SyscallRequest::Geteuid => Ok(self.sys_geteuid() as usize),
            SyscallRequest::Getegid => Ok(self.sys_getegid() as usize),
            SyscallRequest::Setuid { uid } => syscall!(sys_setuid(uid)),
            SyscallRequest::Setgid { gid } => syscall!(sys_setgid(gid)),
            SyscallRequest::Sysinfo { buf } => {
                let sysinfo = self.sys_sysinfo();
                buf.write_at_offset::<Platform>(0, sysinfo)
                    .ok_or(Errno::EFAULT)
                    .map(|()| 0)
            }
            SyscallRequest::CapGet { header, data } => syscall!(sys_capget(header, data)),
            SyscallRequest::GetDirent64 { fd, dirp, count } => {
                self.sys_getdirent64(fd, dirp, count)
            }
            SyscallRequest::SchedGetAffinity { pid, len, mask } => {
                const BITS_PER_BYTE: usize = 8;
                let cpuset = self.sys_sched_getaffinity(pid);
                if len * BITS_PER_BYTE < cpuset.len()
                    || len & (core::mem::size_of::<usize>() - 1) != 0
                {
                    Err(Errno::EINVAL)
                } else {
                    let raw_bytes = cpuset.as_bytes();
                    mask.copy_from_slice::<Platform>(0, raw_bytes)
                        .map(|()| raw_bytes.len())
                        .ok_or(Errno::EFAULT)
                }
            }
            SyscallRequest::SchedYield => {
                // Do nothing until we have more scheduler integration with the
                // platform.
                Ok(0)
            }
            SyscallRequest::SchedGetParam { pid, param } => {
                let sched_priority = self.sys_sched_getparam(pid);
                param
                    .write_at_offset::<Platform>(0, sched_priority)
                    .ok_or(Errno::EFAULT)
                    .map(|()| 0)
            }
            SyscallRequest::SchedSetParam { pid, param } => {
                let sched_priority = param.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
                self.sys_sched_setparam(pid, sched_priority);
                Ok(0)
            }
            SyscallRequest::SchedGetScheduler { pid } => {
                Ok(self.sys_sched_getscheduler(pid).reinterpret_as_unsigned() as usize)
            }
            SyscallRequest::SchedSetScheduler { pid, policy, param } => {
                let sched_priority = param.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
                self.sys_sched_setscheduler(pid, policy, sched_priority);
                Ok(0)
            }
            SyscallRequest::Futex { args } => self.sys_futex(args),
            SyscallRequest::Umask { mask } => {
                let old_mask = self.sys_umask(mask);
                Ok(old_mask.bits() as usize)
            }
            SyscallRequest::Wait4 {
                pid,
                wstatus,
                options,
                rusage,
            } => self.sys_wait4(pid, wstatus, options, rusage),
            SyscallRequest::Kill { pid, sig } => self.sys_kill(pid, sig),
            SyscallRequest::Tkill { tid, sig } => self.sys_tkill(tid, sig),
            SyscallRequest::Tgkill { tgid, tid, sig } => self.sys_tgkill(tgid, tid, sig),
            SyscallRequest::Sigaltstack { ss, old_ss } => self.sys_sigaltstack(ss, old_ss, ctx),
            SyscallRequest::Alarm { seconds } => syscall!(sys_alarm(seconds)),
            SyscallRequest::Pause => syscall!(sys_pause()),
            SyscallRequest::GetITimer { which, curr_value } => {
                syscall!(sys_getitimer(which, curr_value))
            }
            SyscallRequest::SetITimer {
                which,
                new_value,
                old_value,
            } => syscall!(sys_setitimer(which, new_value, old_value)),
            _ => {
                log_unsupported!("{request:?}");
                Err(Errno::ENOSYS)
            }
        }
    }
}

/// Global shim state, shared across all tasks.
struct GlobalState<Platform: ShimPlatform, FS: ShimFS> {
    /// The platform instance used throughout the shim.
    platform: &'static Platform,
    /// The LiteBox instance used throughout the shim.
    litebox: litebox::LiteBox<Platform>,
    /// The futex manager for handling futex operations.
    futex_manager: FutexManager<Platform>,
    /// The anonymous pipe implementation.
    pipes: Pipes<Platform>,
    /// The network subsystem.
    net: litebox::sync::Mutex<Platform, Network<Platform>>,
    /// The time when the shim was started.
    boot_time: <Platform as TimeProvider>::Instant,
    /// Next thread ID to assign.
    // TODO: better management of thread IDs
    next_thread_id: core::sync::atomic::AtomicI32,
    /// UNIX domain socket address table
    unix_addr_table: litebox::sync::RwLock<Platform, syscalls::unix::UnixAddrTable<Platform, FS>>,
    /// Per-process collection of ELF patching state for runtime syscall rewriting.
    elf_patch_cache: litebox::sync::Mutex<Platform, syscalls::mm::ElfPatchCache>,
    /// Registry of `flock(2)` advisory-lock state, keyed by the underlying file's `(dev, ino)`.
    ///
    /// This is deliberately shim-wide (not per-`FilesState`/per-process): real `flock()` locks
    /// must contend across *any* two open file descriptions of the same underlying file, even ones
    /// reached from independent `open()` calls in different (e.g. `fork()`-created) processes, not
    /// just fds `dup()`-derived from a single `open()`. See [`syscalls::file::FlockFile`].
    flock_registry: litebox::sync::Mutex<Platform, syscalls::file::FlockRegistry<Platform>>,
    /// Next id to hand out to a `flock()` holder, identifying an open file description to the
    /// `flock()` implementation (see `syscalls::file`). Shim-wide (rather than a function-local
    /// `static`) so it composes with the crate's existing "no bare `static`s outside of the
    /// ratcheted set" discipline.
    next_flock_holder_id: core::sync::atomic::AtomicU64,
    /// Registry of allocated ptys' slave-side fd, keyed by pty id (`TIOCGPTN`'s value).
    ///
    /// The slave fd held here is never installed into any process's own fd table directly; each
    /// `open("/dev/pts/<id>")` duplicates it (via [`litebox::fd::Descriptors::duplicate`], the
    /// same mechanism `dup()`/`fork()` use) to produce an independent fd sharing the same
    /// underlying entry. Shim-wide (not per-process) because `/dev/pts/<id>` is a global
    /// namespace: any process that knows the id (e.g. via a fd inherited across `fork()`, or by
    /// reading `/proc/self/fd` in a real Linux guest) can open it.
    pty_registry: litebox::sync::RwLock<
        Platform,
        alloc::collections::BTreeMap<u32, syscalls::pty::PtyFd<Platform>>,
    >,
    /// Registry of allocated ptys' master-side fd, keyed by pty id, populated only for a pty
    /// created via `Task::attach_pty_stdio` (the session-daemon `--pty-mode` path).
    /// Ordinary guest-driven `/dev/ptmx` opens (`ptmx_open`) never populate this -- the master fd
    /// there lives purely in the opening process's own fd table, reachable only via the guest's
    /// own syscalls, matching real Linux. This registry exists so a HOST-side caller (the runner,
    /// via [`LinuxShim::pty_master_read`]/[`LinuxShim::pty_master_write`]) can drive the master
    /// side of a session-daemon pty from a plain background thread with no `Task` in scope --
    /// `PtyFd` read/write only need the shared `syscalls::pty::PtyEnd` entry itself, not a
    /// process's fd table, so no per-thread `Task` is needed to use it.
    daemon_pty_masters: litebox::sync::RwLock<
        Platform,
        alloc::collections::BTreeMap<u32, syscalls::pty::PtyFd<Platform>>,
    >,
    /// Next id to hand out to a freshly `open("/dev/ptmx")`-allocated pty pair.
    next_pty_id: core::sync::atomic::AtomicU32,
    /// Next id to hand out for AF_UNIX socket "autobind" (`bind()` called with no address),
    /// formatted the same way real Linux formats its autobind abstract-namespace names: a
    /// leading NUL byte followed by 5 lowercase hex digits (see `unix(7)`). Real Linux starts
    /// from an unpredictable point and retries on collision; this shim-wide counter instead
    /// increments monotonically, which is simpler and still unique for any realistic number of
    /// autobind calls within one shim instance's lifetime (wraps at 2^20, matching the same
    /// 5-hex-digit range Linux itself uses).
    next_unix_autobind_id: core::sync::atomic::AtomicU32,
    /// The first process created by [`LinuxShim::load_program`], set once and kept for the
    /// lifetime of the shim.
    ///
    /// Since real `fork()` (see [`syscalls::process::Process`]) gives each process its own
    /// [`litebox::mm::PageManager`], there is no longer a single shim-wide page manager -- code
    /// with a [`Task`]/[`syscalls::process::Process`] in scope reaches its own via
    /// `task.process().pm`. This field exists solely for the narrow single-process callers (e.g.
    /// `litebox_runner_snp`'s kernel-context page-fault handler) that have no `Task` in scope and
    /// only ever run a single bootstrap process, exposed via [`LinuxShim::page_manager`].
    bootstrap_process: once_cell::race::OnceBox<Arc<syscalls::process::Process<Platform>>>,
}

struct Task<Platform: ShimPlatform, FS: ShimFS> {
    global: Arc<GlobalState<Platform, FS>>,
    wait_state: wait::WaitState<Platform>,
    thread: syscalls::process::ThreadState<Platform>,
    /// Process ID
    pid: i32,
    /// Parent Process ID
    ppid: i32,
    /// Thread ID
    tid: i32,
    /// Task credentials. These are set per task but are Arc'd to save space
    /// since most tasks never change their credentials.
    credentials: Arc<syscalls::process::Credentials>,
    /// Command name (usually the executable name, excluding the path)
    comm: Cell<[u8; litebox_common_linux::TASK_COMM_LEN]>,
    /// Filesystem state. `RefCell` to support `unshare` in the future.
    fs: RefCell<Arc<syscalls::file::FsState<Platform>>>,
    /// File descriptors. `RefCell` to support `unshare` in the future.
    files: RefCell<Arc<syscalls::file::FilesState<Platform, FS>>>,
    /// Signal state
    signals: syscalls::signal::SignalState<Platform>,
    /// Set by [`LinuxShim::load_program_attach_pty`]'s internal call to `Self::attach_pty_stdio`
    /// once this task's stdio has been attached to a fresh pty's slave -- the pty id a host-side
    /// caller (with no `Task` in scope) should pass to
    /// [`LinuxShim::pty_master_read`]/[`LinuxShim::pty_master_write`]. `None` for every ordinary
    /// (non-`--pty-mode`) process.
    attached_pty_id: Cell<Option<u32>>,
}

impl<Platform: ShimPlatform, FS: ShimFS> Drop for Task<Platform, FS> {
    fn drop(&mut self) {
        self.prepare_for_exit();
    }
}

#[cfg(test)]
mod test_utils {
    extern crate std;
    use super::*;

    impl<Platform: ShimPlatform, FS: ShimFS> GlobalState<Platform, FS> {
        /// Make a new task with default values for testing.
        pub(crate) fn new_test_task(
            self: Arc<Self>,
            fs: alloc::sync::Arc<FS>,
        ) -> Task<Platform, FS> {
            let pid = self
                .next_thread_id
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let files = Arc::new(syscalls::file::FilesState::new(fs));
            files.initialize_stdio_in_shared_descriptors_table(&self);
            let shared_pending = Arc::new(litebox::sync::Mutex::new(
                syscalls::signal::PendingSignals::new(),
            ));
            Task {
                wait_state: wait::WaitState::new(self.platform),
                thread: syscalls::process::ThreadState::new_process(
                    pid,
                    PageManager::new(&self.litebox),
                    false,
                    None,
                    shared_pending.clone(),
                ),
                pid,
                ppid: 0,
                tid: pid,
                credentials: Arc::new(syscalls::process::Credentials {
                    uid: 0,
                    euid: 0,
                    gid: 0,
                    egid: 0,
                }),
                comm: Cell::new(*b"test\0\0\0\0\0\0\0\0\0\0\0\0"),
                fs: Arc::new(syscalls::file::FsState::new()).into(),
                files: files.into(),
                signals: syscalls::signal::SignalState::new_process(shared_pending),
                attached_pty_id: Cell::new(None),
                global: self,
            }
        }
    }

    impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
        /// Returns a clone of this task with a new TID for testing.
        pub(crate) fn clone_for_test(&self) -> Option<Self> {
            let tid = self
                .global
                .next_thread_id
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let task = Task {
                wait_state: wait::WaitState::new(self.global.platform),
                global: self.global.clone(),
                thread: self.thread.new_thread(tid)?,
                pid: self.pid,
                ppid: self.ppid,
                tid,
                credentials: self.credentials.clone(),
                comm: self.comm.clone(),
                fs: self.fs.clone(),
                files: self.files.clone(),
                // Always a same-process thread clone -- see `self.thread.new_thread(tid)` above.
                signals: self.signals.clone_for_new_task(None),
                attached_pty_id: Cell::new(self.attached_pty_id.get()),
            };
            Some(task)
        }

        /// Returns a clone of this task as a genuine new **process** (a real fork()-shaped
        /// child: new `Process`, registered in `self`'s `children` so `do_kill`'s remote-child
        /// case can find it, given its own independent `shared_pending`), rather than
        /// [`Self::clone_for_test`]'s same-process thread-clone.
        ///
        /// Deliberately skips everything `do_clone`'s real process-clone branch does that isn't
        /// relevant to testing cross-process signal delivery: address-space duplication,
        /// register/TLS translation, `ThreadInitState::ForkedChild` setup. This produces a
        /// process family shaped correctly for `do_kill`/`interrupt_all_threads` to exercise,
        /// not a functioning forked guest process.
        pub(crate) fn clone_as_forked_child_for_test(&self) -> Self {
            let pid = self
                .global
                .next_thread_id
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let shared_pending = Arc::new(litebox::sync::Mutex::new(
                syscalls::signal::PendingSignals::new(),
            ));
            let thread = syscalls::process::ThreadState::new_process(
                pid,
                PageManager::new(&self.global.litebox),
                false,
                Some(Arc::downgrade(self.process())),
                shared_pending.clone(),
            );
            let child = Task {
                wait_state: wait::WaitState::new(self.global.platform),
                global: self.global.clone(),
                thread,
                pid,
                ppid: self.pid,
                tid: pid,
                credentials: self.credentials.clone(),
                comm: self.comm.clone(),
                fs: self.fs.clone(),
                files: self.files.clone(),
                signals: self.signals.clone_for_new_task(Some(shared_pending)),
                attached_pty_id: Cell::new(self.attached_pty_id.get()),
            };
            self.process()
                .add_child_for_test(pid, child.process().clone());
            child
        }

        /// Spawns a thread that runs with a clone of this task and a new TID.
        ///
        /// # Panics
        /// Panics if the test process is already terminating.
        pub(crate) fn spawn_clone_for_test<R>(
            &self,
            f: impl 'static + Send + FnOnce(Task<Platform, FS>) -> R,
        ) -> std::thread::JoinHandle<R>
        where
            R: 'static + Send,
        {
            let task = self.clone_for_test().unwrap();
            std::thread::spawn(move || f(task))
        }

        /// Publishes this task's [`ThreadHandle`](litebox::event::wait::ThreadHandle) into
        /// `syscalls::process::ThreadRemote::handle`, exactly as `Task::handle_init_request` does
        /// for a real guest thread before it first runs guest code.
        ///
        /// Must be called once, on the OS thread that will run this task, after that thread has
        /// registered its own platform-level `ThreadHandle`
        /// (e.g. via [`ThreadProvider::run_test_thread`](litebox::platform::ThreadProvider::run_test_thread)),
        /// and before performing any interruptible wait on this task -- otherwise
        /// `ThreadRemote::interrupt`/`exit_group` cannot reach this thread at all, since nothing
        /// else ever populates `ThreadRemote::handle` outside the real
        /// [`litebox::shim::EnterShim::init`] entrypoint that production guest-thread startup
        /// always goes through, which `spawn_clone_for_test` deliberately bypasses (it does not
        /// run any guest code).
        pub(crate) fn set_thread_handle_for_test(&self) {
            self.thread
                .remote_handle_cell()
                .set(alloc::boxed::Box::new(self.wait_state.thread_handle()))
                .ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_read_on_pipe_does_not_panic() {
        // Regression test: a single `read()` requesting more than `MAX_KERNEL_BUF_SIZE` from a
        // non-seekable fd (pipe/socket/eventfd/pty/etc) used to unconditionally panic via
        // `unimplemented!()` in the `SyscallRequest::Read` dispatch, because the large-read path
        // always probed the fd's offset with `lseek` first and treated the resulting `ESPIPE`
        // (correctly returned for any non-seekable fd) as an unhandled case. This exercises the
        // `read_with_user_buf_no_offset` fallback that replaced that panic.
        let task = crate::syscalls::tests::init_platform(None);
        let (reader, writer) = task.sys_pipe2(litebox::fs::OFlags::empty()).unwrap();
        task.sys_write(writer.try_into().unwrap(), b"hello", None)
            .unwrap();

        let mut buf = [0u8; 16];
        let buf_ptr = UserPtrMut::from_usize(buf.as_mut_ptr().expose_provenance());
        let n = task
            .read_with_user_buf_no_offset(reader.try_into().unwrap(), buf_ptr, 600_000)
            .expect("large read on a non-seekable fd must not panic or error");
        assert_eq!(&buf[..n], b"hello");
    }
}
