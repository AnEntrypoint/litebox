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
pub mod diag;
pub mod loader;
pub(crate) mod stdio;
pub mod syscalls;
pub mod transport;
mod wait;

// `syscalls::drm` is `pub(crate)` (its ioctl surface is not meant to be called directly from
// outside this crate), but `ScanoutSnapshot` -- the one type a runner's control channel needs --
// is re-exported here at the crate root so it is nameable without exposing the rest of that
// module. See `LinuxShim::drm_scanout_snapshot`.
pub use syscalls::drm::ScanoutSnapshot;

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
    + litebox::platform::SharedKernelStateProvider
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
        + litebox::platform::SharedKernelStateProvider
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
    // Unconditional, NOT gated on `debug_assertions`. An unsupported feature is a silent
    // behavioural divergence from Linux -- the syscall returns an error the guest did not
    // deserve -- so it is exactly what a release-build investigation most needs to see.
    // Compiling it out of release builds cost a long hunt for a hang whose actual cause was
    // `sys_tkill` to a remote tid returning ESRCH without sending the signal: that one
    // load-bearing event was invisible at every `LITEBOX_LOG` level, and the search went to
    // futex keying, lost wakeups and thread spawn instead. `warn!` already costs nothing when
    // the level is filtered out, so there is no reason to also strip it at compile time.
    litebox_util_log::warn!(feature:% = args; "unsupported");
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
        litebox_util_log::debug!("drm-diag: init() entry");
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
        litebox_util_log::debug!(
            exception:? = info.exception, kernel_mode:% = info.kernel_mode;
            "drm-diag: exception() entry"
        );
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
        // x86_64's synthesized sigreturn trampoline (see `ensure_sigreturn_trampoline`'s x86_64
        // doc comment) holds the REAL glibc `__restore_rt` bytes verbatim but is mapped
        // `PROT_READ` only, never `PROT_EXEC` -- reaching it via the signal handler's `ret`
        // therefore always raises an instruction-fetch access violation (translated to
        // `Exception::PAGE_FAULT` by the platform layer) at exactly this address, before the real
        // `syscall` byte would ever be decoded. `ctx.rip` landing exactly on the trampoline's own
        // address is this trap's unambiguous signature -- no other guest code legitimately
        // fetches from this specific litebox-owned page.
        #[cfg(target_arch = "x86_64")]
        if info.exception == litebox::shim::Exception::PAGE_FAULT
            && ctx.rip == self.task.sigreturn_trampoline_addr()
            && ctx.rip != 0
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
                    .pm()
                    .handle_page_fault(fault_addr, error_code)
            }
            .is_ok()
            {
                return ContinueOperation::Resume;
            } else {
                return ContinueOperation::Terminate;
            }
        }
        // AGENTS.md pass 257: diagnostic for the still-open weston guest-side SIGSEGV (pass
        // 249, confirmed the single dominant remaining blocker as of pass 256) -- the existing
        // "fatal signal: terminating task" log (syscalls/signal/mod.rs) has no register/fault-
        // address context, only the synthesized Linux signal number. This captures the real
        // guest instruction pointer/stack pointer and the raw exception info BEFORE it's
        // translated into a signal, so the next capture of this exact crash gives an actual
        // faulting instruction address to investigate instead of just "Signal(11)".
        #[cfg(target_arch = "x86_64")]
        {
            litebox_util_log::debug!(
                exception:? = info.exception, kernel_mode:% = info.kernel_mode,
                rip:% = format_args!("{:#x}", ctx.rip), rsp:% = format_args!("{:#x}", ctx.rsp),
                cr2:% = format_args!("{:#x}", info.cr2), error_code:% = format_args!("{:#x}", info.error_code),
                // 2026-09-09 Track-B Xvfb-crash investigation: added to test the "corrupted
                // pointer, not a missing mapping" hypothesis (docs/track-b-fork-fix-progress.md,
                // "closing synthesis" entry) against the decoded faulting instruction
                // (`mov rax, [rdx + rax*8]`) -- rdx is the table base, rax the index; if rdx
                // itself doesn't point anywhere a real relocation/GOT table for one of Xvfb's
                // loaded libraries could plausibly live, that's direct evidence for corruption
                // over a genuinely-missing-mapping explanation.
                rax:% = format_args!("{:#x}", ctx.rax), rdx:% = format_args!("{:#x}", ctx.rdx),
                rcx:% = format_args!("{:#x}", ctx.rcx), rsi:% = format_args!("{:#x}", ctx.rsi),
                rdi:% = format_args!("{:#x}", ctx.rdi);
                "diag-guest-exception: pre-signal snapshot"
            );
            // AGENTS.md pass 257 follow-up: distinguish "genuine weston bug jumping to a real
            // but corrupted pointer value" from "litebox emulation gap leaving cr2 unmapped
            // when it should be mapped" -- dump every guest mapping overlapping a window around
            // `cr2` (or note that NOTHING overlaps at all, i.e. genuinely unmapped address
            // space) so the next capture answers this directly instead of needing a second pass.
            let probe_range = info.cr2.saturating_sub(0x1000)..info.cr2.saturating_add(0x1000);
            // `mappings()` (`litebox/src/mm/mod.rs`) already collects into an owned `Vec` and
            // drops its internal `vmem.read()` lock before returning -- iterating it here holds
            // NO lock, ruling out the "still-held mappings lock" half of the hang hypothesis
            // the prior session recorded. The owned snapshot is kept (not just a bool) so the
            // byte-dump below can reuse the already-confirmed-overlapping range instead of a
            // second, redundant mappings() call.
            let overlapping: Vec<_> = self
                .process()
                .0
                .pm()
                .mappings()
                .into_iter()
                .filter(|(r, _)| r.start < probe_range.end && r.end > probe_range.start)
                .collect();
            for (r, flags) in &overlapping {
                litebox_util_log::debug!(
                    range_start:% = format_args!("{:#x}", r.start),
                    range_end:% = format_args!("{:#x}", r.end),
                    flags:? = flags;
                    "diag-guest-exception: mapping overlapping cr2"
                );
            }
            if overlapping.is_empty() {
                litebox_util_log::debug!(
                    cr2:% = format_args!("{:#x}", info.cr2);
                    "diag-guest-exception: NO mapping overlaps cr2 (genuinely unmapped)"
                );
            }
            // Deliberately no raw byte-dump diagnostic of `*cr2` here: `cr2_mapped` (the mapping
            // walk above) reflects litebox's own VMA tracking, which can diverge from real
            // Windows memory, so a raw dereference "confirmed safe" by it can itself take a
            // second, unrecoverable host-mode fault -- this previously turned an ordinary,
            // gracefully-delivered guest SIGSEGV into a host crash. Do not re-add one without
            // resolving that gap; see docs/track-b-fork-fix-progress.md's 2026-09-09 "symbolized
            // the crash" entries for the full trace. (`rip_mapped`'s dump below is NOT subject to
            // this: `rip`'s fault-free execution up to this instruction is a live guarantee it's
            // genuinely mapped, not an assumption resting on possibly-stale tracking.)
            let rip_mapped = self
                .process()
                .0
                .pm()
                .mappings()
                .into_iter()
                .any(|(r, _)| r.contains(&(ctx.rip as usize)));
            if rip_mapped {
                let dump = unsafe {
                    core::slice::from_raw_parts(ctx.rip as *const u8, 64)
                };
                litebox_util_log::debug!(
                    rip:% = format_args!("{:#x}", ctx.rip),
                    bytes:% = format_args!("{:02x?}", dump);
                    "diag-guest-exception: rip byte dump"
                );
            }
        }
        #[cfg(target_arch = "aarch64")]
        litebox_util_log::debug!(
            exception:? = info.exception, kernel_mode:% = info.kernel_mode,
            pc:% = format_args!("{:#x}", ctx.pc), sp:% = format_args!("{:#x}", ctx.sp),
            fault_address:% = format_args!("{:#x}", info.fault_address);
            "diag-guest-exception: pre-signal snapshot"
        );
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
    /// Create a pipe in this (cross-process fork child) process, place its WRITE end at exactly
    /// `target_fd`, and return a host-side handle on the READ end for a pump thread.
    ///
    /// Runs on this task's own thread by necessity -- `LinuxShimEntrypoints` is deliberately
    /// `!Send`.
    pub fn install_pipe_write_end_at_fd(
        &self,
        target_fd: i32,
    ) -> Option<litebox::pipes::DetachedPipeEnd<Platform>> {
        self.task.install_pipe_write_end_at_fd(target_fd)
    }

    /// Create a pipe in this (cross-process fork child) process, place its READ end at exactly
    /// `target_fd`, and return a host-side handle on the WRITE end for a pump thread. The mirror
    /// of [`Self::install_pipe_write_end_at_fd`]; same thread requirement.
    pub fn install_pipe_read_end_at_fd(
        &self,
        target_fd: i32,
    ) -> Option<litebox::pipes::DetachedPipeEnd<Platform>> {
        self.task.install_pipe_read_end_at_fd(target_fd)
    }

    /// Reopen an inherited regular file at exactly `target_fd`, positioned at `offset`. See
    /// `Task::install_file_at_fd`. Same thread requirement as the pipe installers.
    pub fn install_file_at_fd(
        &self,
        target_fd: i32,
        path: &str,
        flags: u32,
        offset: u64,
    ) -> Option<()> {
        self.task.install_file_at_fd(target_fd, path, flags, offset)
    }

    /// Recreate an inherited eventfd at exactly `target_fd`. See `Task::install_eventfd_at_fd`.
    /// Same thread requirement as the pipe and file installers.
    pub fn install_eventfd_at_fd(&self, target_fd: i32, count: u64, flags: u32) -> Option<()> {
        self.task.install_eventfd_at_fd(target_fd, count, flags)
    }

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
    /// Shared with `GlobalState::proc_self_info` once `build()` runs -- created here (rather than
    /// in `build()`) because `default_fs` (which mounts the `/proc/self` backend sharing this
    /// same cell) always runs before `build()`. See `GlobalState::proc_self_info`'s doc comment.
    proc_self_info: Arc<litebox::sync::RwLock<Platform, litebox::fs::procfs::ProcSelfTable>>,
    /// Shared with `GlobalState::pts_registry` once `build()` runs, for the exact same reason as
    /// `proc_self_info` above: `default_fs` mounts the `/dev/pts` backend sharing this cell, and
    /// that always runs before `build()`. See `litebox::fs::devices::PtsRegistry`'s doc comment.
    pts_registry: Arc<litebox::sync::RwLock<Platform, litebox::fs::devices::PtsRegistry>>,
}

impl<Platform: ShimPlatform> LinuxShimBuilder<Platform> {
    /// Returns a new shim builder using the given platform.
    pub fn new(platform: &'static Platform) -> Self {
        Self {
            platform,
            litebox: LiteBox::new(platform),
            proc_self_info: Arc::new(litebox::sync::RwLock::new(
                litebox::fs::procfs::ProcSelfTable::default(),
            )),
            pts_registry: Arc::new(litebox::sync::RwLock::new(
                litebox::fs::devices::PtsRegistry::new(),
            )),
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
        default_fs(
            &self.litebox,
            self.platform,
            in_mem_fs,
            vec![tar_data],
            // No caller of this single-pre-merged-tar path has any use for the resulting
            // live-entry list (only the OCI multi-layer path's own redundant-rebuild problem
            // needs it), so don't pay `live_entries_after_merge`'s own extra tree walk to produce
            // one nobody will read.
            MergeInput::BuildFresh {
                capture_for_caller: false,
            },
            self.proc_self_info.clone(),
            self.pts_registry.clone(),
        )
        .0
    }

    /// Create a default layered file system whose read-only lower layer is built from MULTIPLE
    /// OCI-style layer tars (bottom-to-top), rather than a single pre-merged tar -- the runtime
    /// OCI-image-loading path's entrypoint (see
    /// `litebox_runner_linux_on_windows_userland`'s `--oci-image` option and
    /// `litebox::fs::tar_ro::TarRo::from_layers`). Whiteout/opaque-whiteout merging across
    /// `tar_layers` happens entirely inside `TarRo::from_layers`, so this crate never sees or
    /// needs a pre-merged single tar for this path.
    ///
    /// The second return value is `Some(entries)` -- [`litebox::fs::tar_ro::TarRo::
    /// live_entries_after_merge`]'s own output, captured as a side effect of the real
    /// `TarRo::from_layers` build this method just did -- whenever the caller can usefully cache
    /// it for a later, cheaper [`Self::default_fs_multi_layer_with_cached_merge`] call (see that
    /// method's own doc comment for why). This crate has no on-disk cache of its own -- that
    /// policy, and the disk I/O it needs, belongs to the caller (`litebox_runner_linux_on_
    /// windows_userland`, which already owns `.litebox-cache`'s per-layer sibling cache).
    pub fn default_fs_multi_layer(
        &self,
        in_mem_fs: litebox::fs::in_mem::FileSystem<Platform>,
        tar_layers: Vec<Cow<'static, [u8]>>,
    ) -> (
        DefaultFS<Platform>,
        Option<Vec<litebox::fs::tar_ro::MergedLiveEntry>>,
    ) {
        default_fs(
            &self.litebox,
            self.platform,
            in_mem_fs,
            tar_layers,
            MergeInput::BuildFresh {
                capture_for_caller: true,
            },
            self.proc_self_info.clone(),
            self.pts_registry.clone(),
        )
    }

    /// Same as [`Self::default_fs_multi_layer`], but skips `TarRo::from_layers`'s own tar-parse
    /// and whiteout-fold phases by building straight from an already-known
    /// `TarRo::live_entries_after_merge` list instead (`TarRo::from_merged_live_entries`).
    ///
    /// Exists so a cross-process `LITEBOX_PROCESS_FORK=1` child -- which re-derives this SAME
    /// read-only rootfs from scratch on every single fork, since each is a genuinely separate
    /// Windows process with no shared heap for a `HashMap`/`BTreeMap`-shaped structure (see
    /// `GlobalState`'s own "nested collections not actually shared" limitation) -- doesn't have
    /// to pay `TarIndex::from_layers`'s full cost (measured live: 3.2-3.5s per fork, the dominant
    /// share of total fork-child startup time) merely to rediscover a merge result that is, by
    /// construction, bit-for-bit identical to the one the very first process in this fork tree
    /// already computed. See `litebox::fs::tar_ro::TarRo::from_merged_live_entries`'s own doc
    /// comment for why reusing that list instead of recomputing it is sound.
    pub fn default_fs_multi_layer_with_cached_merge(
        &self,
        in_mem_fs: litebox::fs::in_mem::FileSystem<Platform>,
        tar_layers: Vec<Cow<'static, [u8]>>,
        merged_entries: Vec<litebox::fs::tar_ro::MergedLiveEntry>,
    ) -> DefaultFS<Platform> {
        default_fs(
            &self.litebox,
            self.platform,
            in_mem_fs,
            tar_layers,
            MergeInput::UseCached(merged_entries),
            self.proc_self_info.clone(),
            self.pts_registry.clone(),
        )
        .0
    }

    /// Build the shim.
    ///
    /// **Create-vs-attach** (`litebox::platform::SharedKernelStateProvider`,
    /// `SharedKernelStateSlot::ShimGlobalState`): the very first process in a fork family (or any
    /// process on a platform that never returns `true` from
    /// `is_shared_kernel_state_attach_child`) constructs a fresh `GlobalState` exactly as this
    /// always did before this trait existed. A cross-process-fork child that this platform has
    /// confirmed can reach its ancestor's existing allocation instead ATTACHES to that SAME live
    /// instance -- so every field below (`pipes`, `net`, the pid/tid allocator, every registry)
    /// becomes genuinely, live, shared across the whole fork family, not merely placed at a
    /// consistent address. See `Self::proc_self_info`'s own doc comment for the other known
    /// exceptions, and `GlobalStateHandle`'s own doc comment's "Sixth instance" section for
    /// `futex_manager`, which is deliberately NOT part of `GlobalState` at all.
    pub fn build<FS: ShimFS>(self) -> LinuxShim<Platform, FS> {
        let platform = self.platform;
        let slot = litebox::platform::SharedKernelStateSlot::ShimGlobalState;
        // Captured BEFORE the create-vs-attach branch below (which may move `self.litebox` into
        // the shared struct on the create path): this process's OWN `litebox` is what
        // `GlobalStateHandle` uses from here on, regardless of which branch runs -- see that
        // struct's own doc comment for why `GlobalState.litebox` itself must never be read again.
        let my_litebox = self.litebox.clone();
        // Same reasoning as `my_litebox` above -- see `GlobalStateHandle`'s doc comment's
        // "Second instance of the SAME defect" section.
        let my_proc_self_info = self.proc_self_info.clone();
        let my_pts_registry = self.pts_registry.clone();
        // Same reasoning as `my_litebox`/`my_proc_self_info` above -- see `GlobalStateHandle`'s
        // doc comment's "Third instance of the SAME defect" section. Unlike those two, nothing
        // needs to see this before `build()` runs, so it is constructed fresh right here rather
        // than threaded through `LinuxShimBuilder`.
        let my_elf_patch_cache = Arc::new(litebox::sync::Mutex::new(
            alloc::collections::BTreeMap::new(),
        ));
        // Same reasoning as `my_elf_patch_cache` above -- see `GlobalStateHandle`'s doc comment's
        // "Fourth instance of the SAME defect" section.
        let my_exec_ranges_cache = Arc::new(litebox::sync::Mutex::new(
            alloc::collections::BTreeMap::new(),
        ));
        // Same reasoning as `my_exec_ranges_cache` above -- see `GlobalStateHandle`'s doc
        // comment's "Fifth instance of the SAME defect" section.
        let my_segment_scan_cache = Arc::new(litebox::sync::Mutex::new(
            alloc::collections::BTreeMap::new(),
        ));
        // Same reasoning as `my_elf_patch_cache`/etc above -- see `GlobalStateHandle`'s doc
        // comment's "Sixth instance of the SAME defect" section. Unlike `Network`/`Pipes` (which
        // are genuinely, correctly meant to be one shared instance for the whole fork family and
        // so are REBOUND in place, not shadowed), `FutexManager`'s own doc comment already scopes
        // it to "private" (single-process) futexes only -- so a fresh per-process instance is not
        // a workaround, it IS the intended semantics (`FUTEX_PRIVATE_FLAG`-equivalent), and it
        // sidesteps the deeper structural problem entirely: `LoanList`'s entries are pinned on the
        // WAITING THREAD'S OWN STACK by design (its own doc comment), so they can never be safely
        // relocated into cross-process-shared memory or referenced from a different process at all
        // -- unlike a `BTreeMap`, there is no "move the collection into the shared arena" fix
        // available here even in principle.
        let my_futex_manager = Arc::new(FutexManager::new());
        // Same reasoning as `my_elf_patch_cache`/etc above -- see `GlobalStateHandle`'s doc
        // comment's "Seventh and eighth instances of the SAME defect" section.
        let my_memfds = Arc::new(litebox::sync::Mutex::new(alloc::collections::BTreeMap::new()));
        let my_shared_files =
            Arc::new(litebox::sync::Mutex::new(alloc::collections::BTreeMap::new()));
        // Ninth instance of the SAME defect -- see `GlobalStateHandle::unix_addr_table`'s doc
        // comment (2026-09-18 systematic audit).
        let my_unix_addr_table = Arc::new(litebox::sync::RwLock::new(
            syscalls::unix::UnixAddrTable::new(),
        ));
        // Tenth instance of the SAME defect -- see `GlobalStateHandle::fifo_registry`'s doc
        // comment (2026-09-18 systematic audit).
        let my_fifo_registry =
            Arc::new(litebox::sync::RwLock::new(alloc::collections::BTreeMap::new()));
        // Eleventh instance of the SAME defect -- see `GlobalStateHandle::pty_registry`'s doc
        // comment. The genuine cross-process capability these two fields' ORIGINAL doc comments
        // (also preserved there) actually need is restored separately by `GlobalState::
        // shared_pty` below, not by these per-process copies.
        let my_pty_registry =
            Arc::new(litebox::sync::RwLock::new(alloc::collections::BTreeMap::new()));
        let my_daemon_pty_masters =
            Arc::new(litebox::sync::RwLock::new(alloc::collections::BTreeMap::new()));
        let inner = platform
            .is_shared_kernel_state_attach_child(slot)
            .then(|| platform.attach_shared_kernel_state(slot))
            .flatten()
            .unwrap_or_else(|| {
                let mut net = Network::new(&self.litebox);
                net.set_platform_interaction(litebox::net::PlatformInteraction::Manual);
                platform.create_shared_kernel_state(
                    slot,
                    GlobalState {
                        platform: self.platform,
                        _fs: core::marker::PhantomData,
                        bootstrap_process: once_cell::race::OnceBox::new(),
                        pipes: Pipes::new(&self.litebox),
                        net: litebox::sync::Mutex::new(net),
                        boot_time: self.platform.now(),
                        next_thread_id: 2.into(), // start from 2, as 1 is used by the main thread
                        live_cross_process_fork_children: core::sync::atomic::AtomicU32::new(0),
                        unix_addr_presence: syscalls::unix::SharedUnixAddrPresenceTable::new(),
                        unix_shared_conn_table: syscalls::unix::SharedUnixConnTable::new(),
                        unix_shared_connect_queue: syscalls::unix::SharedUnixConnectQueue::new(),
                        shared_file_publish: syscalls::file::SharedFilePublishTable::new(),
                        sysv_shm: litebox::sync::Mutex::new(syscalls::mm::SysvShmTable::new()),
                        next_shmid: core::sync::atomic::AtomicI32::new(1),
                        flock_registry: litebox::sync::Mutex::new(
                            alloc::collections::BTreeMap::new(),
                        ),
                        next_flock_holder_id: core::sync::atomic::AtomicU64::new(1),
                        shared_pty: syscalls::pty::SharedPtyTable::new(),
                        next_pty_id: core::sync::atomic::AtomicU32::new(0),
                        next_unix_autobind_id: core::sync::atomic::AtomicU32::new(0),
                        next_memfd_id: core::sync::atomic::AtomicU64::new(0),
                        drm: syscalls::drm::DrmSubsystem::new(),
                        evdev: syscalls::evdev::EvdevSubsystem::new(),
                    },
                )
            });
        LinuxShim(GlobalStateHandle {
            inner,
            litebox: my_litebox,
            proc_self_info: my_proc_self_info,
            pts_registry: my_pts_registry,
            elf_patch_cache: my_elf_patch_cache,
            exec_ranges_cache: my_exec_ranges_cache,
            segment_scan_cache: my_segment_scan_cache,
            futex_manager: my_futex_manager,
            memfds: my_memfds,
            shared_files: my_shared_files,
            unix_addr_table: my_unix_addr_table,
            fifo_registry: my_fifo_registry,
            pty_registry: my_pty_registry,
            daemon_pty_masters: my_daemon_pty_masters,
        })
    }
}

pub struct LinuxShim<Platform: ShimPlatform, FS: ShimFS>(GlobalStateHandle<Platform, FS>);

impl<Platform: ShimPlatform, FS: ShimFS> LinuxShim<Platform, FS> {
    /// Diagnostic-only read of the shim-wide `next_thread_id` allocator (`LITEBOX_DIAG_
    /// GLOBALSTATE_SHARE_PROBE=1`'s decisive live cross-process `GlobalState` create-vs-attach
    /// proof -- see `syscalls::process::Task::try_cross_process_fork`'s matching parent-side
    /// bump). Not gated itself (a plain atomic load is cheap and side-effect-free); the
    /// PARENT-side bump this is meant to observe is what's gated.
    pub fn diag_next_thread_id(&self) -> i32 {
        self.0
            .next_thread_id
            .load(core::sync::atomic::Ordering::SeqCst)
    }

    /// Diagnostic-only insert into `unix_addr_presence` (`LITEBOX_DIAG_UNIX_ADDR_PRESENCE_PROBE=1`'s
    /// decisive live cross-process presence-table proof -- see
    /// `syscalls::process::Task::try_cross_process_fork`'s matching parent-side inserts, mirroring
    /// `diag_next_thread_id`'s own `GLOBALSTATE_SHARE_PROBE` pattern exactly). `kind` is
    /// [`syscalls::unix::UNIX_ADDR_KIND_PATH`]/[`syscalls::unix::UNIX_ADDR_KIND_ABSTRACT`].
    pub fn diag_unix_addr_presence_insert(&self, kind: u32, key: &[u8], owner_pid: u32) -> bool {
        self.0.unix_addr_presence.insert(kind, key, owner_pid)
    }

    /// Diagnostic-only lookup into `unix_addr_presence`, the child-side half of the same proof.
    pub fn diag_unix_addr_presence_lookup(&self, kind: u32, key: &[u8]) -> Option<u32> {
        self.0.unix_addr_presence.lookup(kind, key)
    }

    /// Build a `WaitContext` for host-side pipe I/O.
    ///
    /// A pump thread has no `Task` to borrow one from, and `WaitState` is deliberately per-thread
    /// (`Send` but not `Sync`), so each call makes its own. `WaitState::new` takes the `&'static`
    /// platform, which is exactly what is available here.
    fn host_wait_state(&self) -> litebox::event::wait::WaitState<Platform> {
        litebox::event::wait::WaitState::new(self.0.platform)
    }

    /// Read from a detached pipe end on the HOST side, for a cross-process fork pump thread.
    /// Mirrors the existing `pty_master_read`, which is how the runner already bridges a guest
    /// stream to a real OS handle for stdio.
    pub fn detached_pipe_read(
        &self,
        end: &litebox::pipes::DetachedPipeEnd<Platform>,
        buf: &mut [u8],
    ) -> Option<usize> {
        let wait_state = self.host_wait_state();
        end.read(&wait_state.context(), buf).ok()
    }

    /// Write into a detached pipe end on the HOST side. See [`Self::detached_pipe_read`].
    pub fn detached_pipe_write(
        &self,
        end: &litebox::pipes::DetachedPipeEnd<Platform>,
        buf: &[u8],
    ) -> Option<usize> {
        let wait_state = self.host_wait_state();
        end.write(&wait_state.context(), buf).ok()
    }
}
impl<Platform: ShimPlatform, FS: ShimFS> Clone for LinuxShim<Platform, FS> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> LinuxShim<Platform, FS> {
    /// Install (or replace) the host-side callback invoked on every real DRM page-flip (see
    /// [`syscalls::drm::DrmSubsystem::add_flip_callback`]'s doc comment for the exact contract and
    /// why it deliberately takes plain pixel bytes rather than a platform-specific handle) -- the
    /// sole public entry point into the shim's own `/dev/dri/card0` emulation, since
    /// `DrmSubsystem` itself stays a private implementation detail. A runner binary that depends
    /// on a concrete presentation layer (e.g. `litebox_platform_windows_userland`'s wgpu-backed
    /// `Presenter`) calls this once, right after [`LinuxShimBuilder::build`], to make flipped
    /// frames actually visible; a runner target with no GUI story simply never calls it.
    ///
    /// ADDITIVE: each call installs an ADDITIONAL observer rather than replacing the previous
    /// one, so a `--gui` window and a `LITEBOX_DUMP_FRAMES` capture can both watch the same
    /// frames. See [`syscalls::drm::DrmSubsystem::flip_callbacks`] for why that matters.
    pub fn add_drm_flip_callback(
        &self,
        callback: impl Fn(&[u8], u32, u32, u32, u32) + Send + Sync + 'static,
    ) {
        self.0.drm.add_flip_callback(callback);
    }

    /// The current front buffer's identity (platform shared-memory handle plus geometry and
    /// frame sequence number), for a host-side `ControlServer` to hand off to an external
    /// presenter process -- see [`syscalls::drm::ScanoutSnapshot`] and
    /// `docs/presenter-process-design.md` section 2.2/2.3. Unlike
    /// [`Self::add_drm_flip_callback`], this is a plain on-demand query, not a per-flip observer:
    /// a control channel calls it once when handling a `scanout` command, and again whenever it
    /// wants to check whether the buffer identity changed (a mode change/reallocation) after
    /// observing `seq` advance. `None` until the guest has attached a framebuffer to the CRTC at
    /// least once.
    pub fn drm_scanout_snapshot(&self) -> Option<syscalls::drm::ScanoutSnapshot<Platform>> {
        self.0.drm.scanout_snapshot()
    }

    /// Push a real keyboard/mouse-button transition into the shim's `/dev/input/event0` queue --
    /// see [`syscalls::evdev::EvdevSubsystem::push_key`]'s doc comment for the `code`/`value`
    /// contract. The sole public entry point into the shim's own evdev emulation, mirroring
    /// [`Self::add_drm_flip_callback`]'s role for DRM; a runner binary with a concrete
    /// presentation/input layer (e.g. `litebox_platform_windows_userland`'s winit-backed window)
    /// calls this from its own keyboard/mouse-button event handler.
    pub fn push_input_key(&self, code: u16, value: i32) {
        self.0.evdev.push_key(code, value);
    }

    /// Push a real relative mouse-motion or wheel event into the shim's `/dev/input/event0`
    /// queue -- see [`syscalls::evdev::EvdevSubsystem::push_rel`]'s doc comment for the
    /// `code`/`value` contract.
    pub fn push_input_rel(&self, code: u16, value: i32) {
        self.0.evdev.push_rel(code, value);
    }

    /// Push one 2D mouse movement as a SINGLE evdev report (`REL_X`, `REL_Y`, one `SYN_REPORT`),
    /// which is what real hardware emits for one physical motion -- see
    /// [`syscalls::evdev::EvdevSubsystem::push_rel_motion`]. Prefer this over two `push_input_rel`
    /// calls for cursor movement: those produce two separately-synced reports, making a client
    /// process the motion twice.
    pub fn push_input_rel_motion(&self, dx: i32, dy: i32) {
        self.0.evdev.push_rel_motion(dx, dy);
    }

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
                thread: RefCell::new(syscalls::process::ThreadState::new_process(
                    pid,
                    Arc::new(PageManager::new(&self.0.litebox)),
                    false,
                    None,
                    bootstrap_shared_pending.clone(),
                    None,
                )),
                wait_state: wait::WaitState::new(self.0.platform),
                pid: Cell::new(pid),
                ppid: Cell::new(ppid),
                tid: Cell::new(pid),
                credentials: syscalls::process::Credentials {
                    uid,
                    euid,
                    gid,
                    egid,
                }
                .into(),
                comm: [0; litebox_common_linux::TASK_COMM_LEN].into(),
                dumpable: Cell::new(1),
                fs: Arc::new(syscalls::file::FsState::new()).into(),
                files: files.into(),
                signals: RefCell::new(syscalls::signal::SignalState::new_process(
                    bootstrap_shared_pending,
                )),
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
    /// (see `GlobalStateHandle::daemon_pty_masters`'s doc comment) -- this is what lets a plain
    /// background thread in the runner drain a session's pty output concurrently with
    /// `run_thread` running the guest on its own thread. Blocking: waits for at least one byte
    /// using a throwaway, this-call-only `litebox::event::wait::WaitState` (never the guest's own), matching
    /// [`Self::perform_network_interaction`]'s precedent of driving shim-internal I/O from a
    /// caller with no guest `Task` in scope.
    ///
    /// Cross-process fallback: if `pty_id` is absent from THIS process's own local
    /// `daemon_pty_masters` (it was allocated by a DIFFERENT process in the fork family, or this
    /// process attached to the shared kernel state after `attach_pty_stdio` already ran
    /// elsewhere), reads directly from `syscalls::pty::SharedPtyTable` instead -- see that type's
    /// own doc comment for the full design and explicit scope limits.
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
            masters
                .get(&pty_id)
                .and_then(|master| self.0.litebox.descriptor_table().entry_handle(master))
        };
        let wait_state = litebox::event::wait::WaitState::new(self.0.platform);
        let cx = wait_state.context();
        match handle {
            Some(handle) => {
                handle.with_entry(|end: &syscalls::pty::PtyEnd<Platform>| end.read(&cx, buf, &self.0.shared_pty))
            }
            None => syscalls::pty::poll_shared(&cx, false, || {
                self.0.shared_pty.try_read_side(pty_id, true, buf)
            }),
        }
    }

    /// Write bytes to the master side of a pty allocated via [`Self::load_program_attach_pty`].
    /// See [`Self::pty_master_read`]'s doc comment for the threading/host-caller rationale, the
    /// lock-ordering rationale for resolving the `EntryHandle` and dropping the table guards
    /// before the blocking `end.write(&cx, buf, ...)` call below, AND the cross-process fallback.
    pub fn pty_master_write(&self, pty_id: u32, buf: &[u8]) -> Result<usize, Errno> {
        let handle = {
            let masters = self.0.daemon_pty_masters.read();
            masters
                .get(&pty_id)
                .and_then(|master| self.0.litebox.descriptor_table().entry_handle(master))
        };
        let wait_state = litebox::event::wait::WaitState::new(self.0.platform);
        let cx = wait_state.context();
        match handle {
            Some(handle) => handle
                .with_entry(|end: &syscalls::pty::PtyEnd<Platform>| end.write(&cx, buf, &self.0.shared_pty)),
            None => syscalls::pty::poll_shared(&cx, false, || {
                self.0.shared_pty.try_write_side(pty_id, true, buf)
            }),
        }
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
        let thread_state = syscalls::process::ThreadState::new_process(
            pid,
            Arc::new(pm),
            false,
            None,
            shared_pending.clone(),
            None,
        );

        LinuxShimEntrypoints {
            _not_send: core::marker::PhantomData,
            task: Task {
                global: self.0.clone(),
                thread: RefCell::new(thread_state),
                wait_state: wait::WaitState::new(self.0.platform),
                pid: Cell::new(pid),
                ppid: Cell::new(ppid),
                tid: Cell::new(pid),
                credentials: syscalls::process::Credentials {
                    uid,
                    euid,
                    gid,
                    egid,
                }
                .into(),
                comm: [0; litebox_common_linux::TASK_COMM_LEN].into(),
                dumpable: Cell::new(1),
                fs: Arc::new(syscalls::file::FsState::new()).into(),
                files: files.into(),
                signals: RefCell::new(syscalls::signal::SignalState::new_process(shared_pending)),
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
    pub fn page_manager(&self) -> Arc<PageManager<Platform, PAGE_SIZE>> {
        self.0
            .bootstrap_process
            .get()
            .expect("load_program has not been called yet")
            .pm()
    }

    /// Perform queued network interactions with the outside world.
    ///
    /// This function should be invoked in a loop, based on the returned advice.
    pub fn perform_network_interaction(
        &self,
    ) -> litebox::net::PlatformInteractionReinvocationAdvice {
        self.0.net_lock().perform_platform_interaction()
    }

    /// Force the exact same recovery [`GlobalStateHandle::net_lock`]'s own dead-holder path
    /// already performs (see `litebox::net::Network::reset_after_poisoning`'s doc comment) --
    /// for a caller that caught a panic escaping [`Self::perform_network_interaction`] (e.g.
    /// smoltcp's own `"handle does not refer to a valid socket"`, `socket_set.rs:116`, seen live
    /// 2026-09-18 during a `LITEBOX_PROCESS_FORK=1` full webtop boot).
    ///
    /// # Why this is needed
    ///
    /// A panic that unwinds through `perform_network_interaction`'s `MutexGuard` releases the
    /// lock normally (the holding thread is still alive, just unwinding) -- `RawMutex`'s own
    /// dead-holder poisoning is an OS-level "the recorded holder PROCESS died" signal and is
    /// never set by an ordinary same-process panic, so the very next `net_lock()` call would NOT
    /// see `recovered_from_dead_holder` and would NOT reset anything, leaving whatever stale
    /// `SocketHandle` caused the panic still in place. Every future tick (this process's own next
    /// `wait_on_tun` cycle, and every OTHER process's, since `Network` is shared across the whole
    /// fork family) would panic on the exact same handle again. Left unaddressed, each process's
    /// own `net_worker` thread panics and dies in turn the first time it touches the same stale
    /// handle, until every process's worker has died and networking is silently gone
    /// platform-wide with no further log output at all -- live-confirmed as the mechanism behind
    /// a genuine full-boot stall: this exact panic was the LAST thing ever logged before total,
    /// permanent log silence (`docs/AGENTS_ARCHIVE_2026-09-18.md`, fifteenth pass).
    ///
    /// Calling this after catching such a panic is the same trade-off `reset_after_poisoning`
    /// itself already discloses (a real loss of in-flight connections, in exchange for
    /// self-consistent state for every future caller) -- just triggered by a live in-process
    /// panic instead of an OS-level dead-holder signal. The caller (necessarily `std`-enabled,
    /// since this `no_std` crate cannot itself call `catch_unwind`) is expected to wrap its own
    /// call to [`Self::perform_network_interaction`] in `std::panic::catch_unwind` and call this
    /// method from the `Err` arm before resuming its polling loop.
    pub fn force_reset_network_after_panic(&self) {
        self.0.net_lock().reset_after_poisoning();
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

/// How [`default_fs`] should obtain its `/`-mount `TarRo` backend.
enum MergeInput {
    /// Call `TarRo::from_layers` for real. `capture_for_caller` says whether to ALSO pay
    /// `TarRo::live_entries_after_merge`'s own extra `O(final entry count)` tree walk afterward
    /// and hand the result back through [`default_fs`]'s second return value -- worth it for
    /// [`LinuxShimBuilder::default_fs_multi_layer`] (whose caller can cache the result for a
    /// later, far cheaper rebuild), wasted work for [`LinuxShimBuilder::default_fs`]'s single-
    /// pre-merged-tar callers, none of which have any use for it.
    BuildFresh { capture_for_caller: bool },
    /// Skip `TarRo::from_layers` entirely and build straight from an already-known
    /// `live_entries_after_merge` list via `TarRo::from_merged_live_entries` -- see that
    /// function's own doc comment for the soundness argument.
    UseCached(Vec<litebox::fs::tar_ro::MergedLiveEntry>),
}

/// Create a default layered file system with the given in-memory layer and one or more
/// (bottom-to-top) tar layers backing the read-only lower layer.
fn default_fs<Platform: ShimPlatform>(
    litebox: &LiteBox<Platform>,
    platform: &'static Platform,
    in_mem_fs: litebox::fs::in_mem::FileSystem<Platform>,
    tar_layers: Vec<Cow<'static, [u8]>>,
    merge_input: MergeInput,
    proc_self_info: Arc<litebox::sync::RwLock<Platform, litebox::fs::procfs::ProcSelfTable>>,
    pts_registry: Arc<litebox::sync::RwLock<Platform, litebox::fs::devices::PtsRegistry>>,
) -> (
    LinuxFS<Platform>,
    Option<Vec<litebox::fs::tar_ro::MergedLiveEntry>>,
) {
    // Populated as a side effect of the `/`-mount closure below, ONLY on the
    // `BuildFresh { capture_for_caller: true }` branch -- `Composer::builder().mount`'s closure
    // must return just the backend itself (`TarRo`), so there is no direct return path for this;
    // a `RefCell` captured by reference is the plainest way to smuggle a second output out of a
    // closure whose signature the caller (`Composer`) fixes. Read back once, immediately after
    // `.build()` below returns (single-threaded, synchronous -- the closure has already run by
    // then), never touched concurrently.
    let freshly_built_entries: core::cell::RefCell<
        Option<Vec<litebox::fs::tar_ro::MergedLiveEntry>>,
    > = core::cell::RefCell::new(None);
    // Real host logical-CPU count -- see `litebox::platform::SystemInfoProvider::cpu_count`'s doc
    // comment for why GLib's thread-pool sizing needs this to be accurate, not just present.
    let cpu_count = platform.cpu_count();
    // No live guest-visible memory-pressure tracking exists in this shim; a large fixed value
    // (4 GiB) is a safe, always-parseable `/proc/meminfo` stand-in -- see `format_meminfo`'s doc
    // comment for why the exact value is not load-bearing for any known consumer.
    // Real host memory, queried from the platform (Windows: `GlobalMemoryStatusEx`), NOT a fixed
    // constant. The previous hardcoded 4 GiB -- combined with `format_meminfo`'s old 3/4-of-total
    // formula -- advertised exactly 3 GiB free to every guest regardless of what the host actually
    // had. Xorg sized an allocation to precisely that figure and repeatedly got the whole guest
    // killed by the host's low-memory watchdog. See `SystemInfoProvider::memory_info_kb`.
    let (mem_total_kb, mem_avail_kb) = platform.memory_info_kb();
    // No live wall-clock uptime source is reachable from this `no_std` shim at `default_fs` time
    // (before `GlobalState::boot_time` exists) -- a fixed placeholder is fine, see
    // `format_uptime`'s doc comment.
    const BOOT_UPTIME_SECS: u64 = 0;
    let dev_stdio = litebox::fs::resolver::Resolver::new(
        litebox,
        litebox::fs::composer::Composer::builder()
            .mount("/dev", |allocator| {
                litebox::fs::devices::Devices::new(litebox, allocator)
            })
            .mount("/dev/dri", |allocator| {
                litebox::fs::devices::DriDevices::new(litebox, allocator)
            })
            .mount("/dev/input", |allocator| {
                litebox::fs::devices::InputDevices::new(litebox, allocator)
            })
            // See `litebox::fs::devices::PtsDevices`'s doc comment: without this, `/dev/pts/<id>`
            // and `stat("/dev/pts")` both work (the shim intercepts those directly), but
            // `open("/dev/pts", O_DIRECTORY)` does not -- which is what glibc's real `openpty()`
            // needs (via `ttyname_r`'s directory-scan cross-check) to succeed at all.
            .mount("/dev/pts", |allocator| {
                litebox::fs::devices::PtsDevices::new(litebox, allocator, pts_registry.clone())
            })
            .mount("/sys/class/drm", |allocator| {
                litebox::fs::devices::SysClassDrm::new(litebox, allocator)
            })
            .mount("/sys/class/input", |allocator| {
                litebox::fs::devices::SysClassInput::new(litebox, allocator)
            })
            .mount("/run/udev/data", |allocator| {
                litebox::fs::devices::UdevDb::new(litebox, allocator)
            })
            .mount("/sys/dev/char", |allocator| {
                litebox::fs::devices::SysDevChar::new(litebox, allocator)
            })
            // Every `/proc/sys` file whose value is fixed for the life of a guest, in one
            // table-driven backend (see `litebox::fs::static_files`). This replaces a dedicated
            // ~270-line `Backend` implementation that served `overflowuid`/`overflowgid` and
            // nothing else -- adding the three files below to it would have meant either
            // extending that one-off or writing more of them, and a live XFCE session was
            // observed asking for eleven distinct constant `/proc`+`/sys` paths and getting
            // `ENOENT` for every one.
            .mount("/proc/sys", |allocator| {
                litebox::fs::static_files::StaticFiles::new(
                    litebox,
                    allocator,
                    alloc::vec![
                        // The traditional fixed "nobody" ids, `65534` on every Linux kernel --
                        // what a user namespace maps anything outside its own uid/gid range to.
                        // `bwrap` (bubblewrap, behind `glycin`'s per-format sandboxed image
                        // decoders, which is what `gdk-pixbuf` uses in place of its old
                        // in-process loader modules) reads both while setting up its sandbox,
                        // right after `prctl(PR_SET_NO_NEW_PRIVS, 1)`. Without them it fails
                        // outright ("bwrap: Can't read /proc/sys/kernel/overflowuid"), which
                        // breaks sandboxed decode entirely and surfaces as a `Gtk:ERROR`
                        // assertion abort in any GTK app that has to decode a PNG -- confirmed
                        // live as what was crashing `xfce4-panel` on its own bundled
                        // `image-missing.png` fallback icon.
                        litebox::fs::static_files::file("kernel/overflowuid", b"65534
"),
                        litebox::fs::static_files::file("kernel/overflowgid", b"65534
"),
                        // The highest capability number this "kernel" knows. `40` is
                        // `CAP_CHECKPOINT_RESTORE`, the last one defined as of Linux 5.9 and
                        // still the last in 6.x. libcap reads this to size its own capability
                        // bitmaps and to bound `cap_get_bound` loops.
                        litebox::fs::static_files::file("kernel/cap_last_cap", b"40
"),
                        // Not a FIPS build. OpenSSL and GnuTLS both read this at init; absent, at
                        // least one of them logs a startup complaint on every process.
                        litebox::fs::static_files::file("crypto/fips_enabled", b"0
"),
                        // Heuristic overcommit (the Linux default). Read by allocators deciding
                        // whether a large speculative reservation will be honoured -- which, on
                        // this platform, it is, since `allocate_pages` reserves without
                        // committing until touched.
                        litebox::fs::static_files::file("vm/overcommit_memory", b"0
"),
                    ],
                )
            })
            // Sandboxing-capability probes. `N` is what a kernel built without AppArmor reports,
            // and it is the truth here: litebox has no LSM. Absent, these read as `ENOENT`, which
            // some probes treat as "unknown" and retry rather than as a settled "no".
            .mount("/sys/module/apparmor/parameters", |allocator| {
                litebox::fs::static_files::StaticFiles::new(
                    litebox,
                    allocator,
                    alloc::vec![
                        litebox::fs::static_files::file("enabled", b"N
"),
                        litebox::fs::static_files::file("available", b"N
"),
                    ],
                )
            })
            // CPU topology. Fixed for the life of the guest but not at compile time, which is why
            // `StaticFiles` owns its table rather than borrowing a `&'static` one. GLib and
            // libstdc++ both prefer these over `sysconf` when present, and a wrong or missing
            // answer here sizes every thread pool in the session.
            .mount("/sys/devices/system/cpu", |allocator| {
                let range = if cpu_count > 1 {
                    alloc::format!("0-{}
", cpu_count - 1)
                } else {
                    alloc::string::String::from("0
")
                };
                let mut entries = alloc::vec![
                    litebox::fs::static_files::file("online", range.as_bytes()),
                    litebox::fs::static_files::file("possible", range.as_bytes()),
                ];
                // One `cpuN/cpu_capacity` per CPU, not just `cpu0`. Declaring only `cpu0` was a
                // real gap, not a simplification: a live session was observed reading
                // `cpu1/cpu_capacity` and getting `ENOENT`, because a reader that finds the file
                // for one CPU reasonably expects it for all of them. `1024` is the scheduler's
                // "full capacity" reference value, what every core on a uniform
                // (non-big.LITTLE) machine reports.
                for cpu in 0..cpu_count {
                    entries.push(litebox::fs::static_files::file(
                        &alloc::format!("cpu{cpu}/cpu_capacity"),
                        b"1024
",
                    ));
                }
                litebox::fs::static_files::StaticFiles::new(litebox, allocator, entries)
            })
            .mount("/proc", |allocator| {
                litebox::fs::procfs::Procfs::new(
                    litebox,
                    allocator,
                    cpu_count,
                    mem_total_kb,
                    mem_avail_kb,
                    BOOT_UPTIME_SECS,
                    proc_self_info.clone(),
                )
            })
            .mount("/proc/self", |allocator| {
                litebox::fs::procfs::ProcSelf::new(litebox, allocator, proc_self_info)
            })
            .build()
            .unwrap(),
    );
    let tar_ro = litebox::fs::resolver::Resolver::new(
        litebox,
        litebox::fs::composer::Composer::builder()
            .mount("/", |allocator| match merge_input {
                MergeInput::UseCached(entries) => {
                    litebox::fs::tar_ro::TarRo::from_merged_live_entries(
                        tar_layers, entries, allocator,
                    )
                }
                MergeInput::BuildFresh { capture_for_caller } => {
                    let built = litebox::fs::tar_ro::TarRo::from_layers(tar_layers, allocator);
                    if capture_for_caller {
                        *freshly_built_entries.borrow_mut() =
                            Some(built.live_entries_after_merge());
                    }
                    built
                }
            })
            .build()
            .unwrap(),
    );
    let fs = litebox::fs::layered::FileSystem::new(
        litebox,
        in_mem_fs,
        litebox::fs::layered::FileSystem::new(
            litebox,
            dev_stdio,
            tar_ro,
            litebox::fs::layered::LayeringSemantics::LowerLayerReadOnly,
        ),
        litebox::fs::layered::LayeringSemantics::LowerLayerWritableFiles,
    );
    (fs, freshly_built_entries.into_inner())
}

/// The `(min, max)` priority values Linux reports for a scheduling `policy`.
///
/// These are the real kernel's numbers, not placeholders: the real-time policies span 1..=99 and
/// every non-real-time policy is fixed at 0..=0. glibc reads both at startup and stores them, then
/// ASSERTS against them whenever a thread's priority changes -- so getting them wrong is not a
/// cosmetic inaccuracy.
///
/// Neither syscall was implemented, which left glibc with whatever the unsupported-syscall path
/// returned and produced, on a plain `PTHREAD_PRIO_INHERIT` mutex:
///
/// ```text
/// Fatal glibc error: tpp.c:83 (__pthread_tpp_change_priority): assertion failed:
///   new_prio == -1 || (new_prio >= fifo_min_prio && new_prio <= fifo_max_prio)
/// ```
///
/// Reporting the range faithfully costs nothing and does not claim litebox honours priorities: a
/// guest may ask what the valid range IS and still find that scheduling behaves uniformly, exactly
/// as it does on a machine where every thread happens to run at the same priority.
fn sched_priority_range(policy: i32) -> Result<(i32, i32), Errno> {
    match policy {
        // SCHED_FIFO, SCHED_RR
        1 | 2 => Ok((1, 99)),
        // SCHED_OTHER, SCHED_BATCH, SCHED_IDLE, SCHED_DEADLINE
        0 | 3 | 5 | 6 => Ok((0, 0)),
        _ => Err(Errno::EINVAL),
    }
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
    /// Takes `&GlobalStateHandle`, deliberately NOT `&GlobalState` -- `global.litebox` below must
    /// resolve to `GlobalStateHandle`'s own, always-locally-valid `litebox` field (see that
    /// struct's doc comment), not `GlobalState`'s shared/cross-process-unsafe one (which no
    /// longer exists as a field at all, precisely to make that mistake impossible here).
    fn initialize_stdio_in_shared_descriptors_table(&self, global: &GlobalStateHandle<Platform, FS>) {
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
                                end.is_slave().then(|| end.local_pair().cloned()).flatten()
                            })
                        })
                },
                |_| None,
                |_| None,
                |_| None,
            );
            // A still-connected TCP socket must not be closed via the ordinary `do_close` path
            // here: that path (`GlobalState::close_socket`) performs a *graceful* close by
            // default (waiting, with no timeout at all when the socket has no `SO_LINGER` set,
            // for the peer to send its own FIN/`Events::HUP`) -- correct for a guest's own
            // explicit `close(2)` call, where blocking the calling thread is exactly what real
            // Linux's default (non-`SO_LINGER`) close semantics already do, but wrong here: real
            // Linux's kernel does NOT block an *exiting* process waiting for a graceful TCP
            // close -- it tears the socket down and lets the kernel finish the FIN/RST exchange
            // asynchronously in the background, so the exiting process's `exit()`/`exit_group()`
            // never blocks on it. Confirmed live: `node -e "...connect then process.exit(0)..."`
            // hung forever here (this exact wait, no timeout, for a HUP the remote server has no
            // reason to send first) even though the guest had already unconditionally committed
            // to exiting. Close immediately instead, mirroring the same escape hatch
            // `close_socket`'s own `WaitError::TimedOut` fallback already uses.
            let is_network_fd = files
                .run_on_raw_fd(
                    raw_fd,
                    |_| false,
                    |_| true,
                    |_| false,
                    |_| false,
                    |_| false,
                    |_| false,
                    |_| false,
                    |_| false,
                    |_| false,
                    |_| false,
                )
                .unwrap_or(false);
            if is_network_fd {
                let mut rds = files.raw_descriptor_store.write();
                if let Ok(fd) =
                    rds.fd_consume_raw_integer::<litebox::net::Network<Platform>>(raw_fd)
                {
                    drop(rds);
                    let _ = self
                        .global
                        .net_lock()
                        .close(&fd, litebox::net::CloseBehavior::Immediate);
                }
            } else {
                let _ = self.do_close(raw_fd);
            }
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
        signalfd: impl FnOnce(&TypedFd<syscalls::signalfd::SignalfdSubsystem<Platform>>) -> R,
        timerfd: impl FnOnce(&TypedFd<syscalls::timerfd::TimerfdSubsystem<Platform>>) -> R,
        netlink: impl FnOnce(&TypedFd<syscalls::netlink::NetlinkSocketSubsystem>) -> R,
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
        if let Ok(fd) = rds.fd_from_raw_integer(fd) {
            drop(rds);
            return Ok(signalfd(&fd));
        }
        if let Ok(fd) = rds.fd_from_raw_integer(fd) {
            drop(rds);
            return Ok(timerfd(&fd));
        }
        if let Ok(fd) = rds.fd_from_raw_integer(fd) {
            drop(rds);
            return Ok(netlink(&fd));
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

/// Builds the canned tmpfs-shaped `statfs`/`fstatfs` reply and writes it to `buf` -- shared by
/// `SyscallRequest::Statfs`/`SyscallRequest::Fstatfs`, each of which validates its own target
/// (path via `sys_stat`, fd via `sys_fstat`) exists/is open BEFORE calling this, so a nonexistent
/// path or a stale/never-opened fd gets a real `ENOENT`/`EBADF` instead of this canned success
/// (previously `pathname`/`fd` were ignored entirely -- `Statfs { pathname: _, .. }` -- so ANY
/// path or fd number, including ones that don't exist, reported a successful tmpfs statfs).
fn write_tmpfs_statfs<Platform: ShimPlatform>(
    buf: UserPtrMut<litebox_common_linux::Statfs>,
) -> Result<usize, Errno> {
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    const BSIZE: i64 = 4096;
    // 16 GiB of 4 KiB blocks, all reported free. Any caller doing a real capacity check gets a
    // plausible answer; nothing here is a durable store to fill up.
    const BLOCKS: u64 = (16 * 1024 * 1024 * 1024) / 4096;
    let statfs = litebox_common_linux::Statfs {
        f_type: TMPFS_MAGIC,
        f_bsize: BSIZE,
        f_blocks: BLOCKS,
        f_bfree: BLOCKS,
        f_bavail: BLOCKS,
        f_files: 1 << 20,
        f_ffree: 1 << 20,
        f_fsid: [0, 0],
        f_namelen: 255,
        f_frsize: BSIZE,
        f_flags: 0,
        f_spare: [0; 4],
    };
    buf.write_at_offset::<Platform>(0, statfs)
        .ok_or(Errno::EFAULT)
        .map(|()| 0)
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
        // Advisor-db diagnostics item 1 (`LITEBOX_STRACE_SUMMARY=1`): lazily latch the env flag
        // on first dispatch (this `#![no_std]` crate has no other way to observe the host
        // process environment -- see `diag`'s module doc comment) and, when enabled, time this
        // dispatch and record the outcome. Zero overhead beyond one relaxed atomic load when
        // unset.
        crate::diag::init_strace_summary(|| self.global.platform.env_flag("LITEBOX_STRACE_SUMMARY"));
        let timed = crate::diag::strace_summary_enabled();
        #[cfg(target_arch = "x86_64")]
        let syscall_number = ctx.orig_rax;
        #[cfg(target_arch = "aarch64")]
        let syscall_number = ctx.syscallno.reinterpret_as_unsigned() as usize;
        let start = timed.then(|| self.global.platform.now());

        // `LITEBOX_DIAG_SYSCALL_TIMELINE=1`: log syscall ENTRY (before dispatch, so a syscall
        // that blocks forever still shows up -- `LITEBOX_STRACE_SUMMARY`'s aggregate-only
        // exit-time recording above cannot show this) with pid/comm/syscall name/timestamp, for
        // processes whose `comm` matches [`crate::diag::is_syscall_timeline_target_comm`] only.
        // Built for the "what is this specific client process blocked on" question -- see
        // AGENTS.md's "Rendering/scanout blocker" section: a client's last X11 write is known
        // precisely (from the unix-stream byte trace), but not what it does afterward. An
        // unfiltered every-process version was tried first and OOM'd the host runner process (a
        // 1.25GB single allocation failure, ~21s into a busy full-desktop run, ~26000 log lines
        // already emitted by then) -- logging every syscall of every guest process on a busy
        // multi-process desktop session is not viable. Filtering by comm keeps this proportional
        // to the specific client processes under investigation.
        //
        // WHICH comms is now the env var's VALUE, not a `const` in `diag.rs`. That constant was
        // four hard-coded XFCE names, and this comment used to explain that a configurable filter
        // was "avoided here specifically because that trait is in `litebox/src/platform/mod.rs`,
        // mid-edit by a peer session this pass" -- a scheduling accident, recorded honestly, that
        // then outlived its cause and made the shim's most useful instrument answer questions
        // about exactly one desktop. `SystemInfoProvider::env_value` now exists and this reads it;
        // see `diag::init_syscall_timeline` for why the bound is unchanged by that.
        crate::diag::init_syscall_timeline(|| self.global.platform.env_value("LITEBOX_DIAG_SYSCALL_TIMELINE"));
        // 74th pass: same lazy-latch shape, for the `litebox_diag::socket_read` payload-preview
        // diagnostic's own optional comm filter -- see `diag::init_socket_read_filter`'s doc
        // comment for what problem this solves (blanket-on-every-process cost once that
        // tracing target is enabled) and why unset is a no-op, not a behavior change.
        crate::diag::init_socket_read_filter(|| {
            self.global.platform.env_value("LITEBOX_DIAG_SOCKET_READ_TARGET")
        });
        let comm_bytes = self.comm.get();
        let is_target = crate::diag::syscall_timeline_enabled()
            && crate::diag::is_syscall_timeline_target_comm(&comm_bytes);
        if is_target {
            // Straight to stderr, not through `litebox_util_log` -- see
            // `diag::emit_timeline_line` for why (the log macros are gated on `LITEBOX_LOG`,
            // so this instrument used to accept its own env var and then print nothing).
            crate::diag::emit_timeline_line(
                self.global.platform,
                &alloc::format!(
                    "[diag-syscall-enter] pid={} tid={} comm={} syscall={} syscall_num={}",
                    self.pid.get(),
                    self.tid.get(),
                    alloc::string::String::from_utf8_lossy(&comm_bytes),
                    crate::diag::syscall_name_pub(syscall_number),
                    syscall_number,
                ),
            );
        }

        let result = self.do_syscall(ctx);

        if is_target {
            crate::diag::emit_timeline_line(
                self.global.platform,
                &alloc::format!(
                    "[diag-syscall-exit] pid={} tid={} comm={} syscall={} ok={}",
                    self.pid.get(),
                    self.tid.get(),
                    alloc::string::String::from_utf8_lossy(&comm_bytes),
                    crate::diag::syscall_name_pub(syscall_number),
                    result.is_ok(),
                ),
            );
        }

        if let Some(start) = start {
            let elapsed = litebox::platform::Instant::duration_since(&self.global.platform.now(), &start);
            let err_debug = result.as_ref().err().map(|e| alloc::format!("{e:?}"));
            crate::diag::record_syscall(
                syscall_number,
                elapsed.as_nanos().try_into().unwrap_or(u64::MAX),
                err_debug,
            );
        }

        let return_value = match result {
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
        let request = match SyscallRequest::try_from_raw(syscall_number, ctx, log_unsupported_fmt)
        {
            Ok(r) => r,
            Err(e) => {
                if crate::diag::strace_summary_enabled() {
                    crate::diag::record_unresolved_syscall(
                        syscall_number,
                        self.pid.get(),
                        &alloc::string::String::from_utf8_lossy(&self.comm.get()),
                    );
                }
                return Err(e);
            }
        };
        // `LITEBOX_DIAG_SYSCALL_TIMELINE=1`, matching comm only (see `handle_syscall_request`'s
        // own copy of this gate/comment): log the full, typed request args here (not just the
        // syscall name, as the earlier version in `handle_syscall_request` did) -- this is
        // AFTER `SyscallRequest::try_from_raw` has decoded e.g. an `Open`'s path string, so it
        // answers "which file/library" a hanging `open`->`dlopen` sequence was for, not just
        // that some `open` happened. Truncated: some variants (e.g. a `write` with a large
        // buffer) could otherwise produce a huge line.
        if crate::diag::is_syscall_timeline_target_comm(&self.comm.get()) {
            let debug_str = alloc::format!("{request:?}");
            let truncated = if debug_str.len() > 200 {
                alloc::format!("{}...", &debug_str[..200])
            } else {
                debug_str
            };
            crate::diag::emit_timeline_line(
                self.global.platform,
                &alloc::format!(
                    "[diag-syscall-request-detail] pid={} tid={} comm={} request={}",
                    self.pid.get(),
                    self.tid.get(),
                    alloc::string::String::from_utf8_lossy(&self.comm.get()),
                    truncated,
                ),
            );
        }
        if matches!(
            request,
            SyscallRequest::Clone { .. }
                | SyscallRequest::Clone3 { .. }
                | SyscallRequest::Execve { .. }
                | SyscallRequest::Wait4 { .. }
            | SyscallRequest::Waitid { .. }
                | SyscallRequest::Exit { .. }
                | SyscallRequest::ExitGroup { .. }
                | SyscallRequest::Openat { .. }
                | SyscallRequest::Close { .. }
                | SyscallRequest::CloseRange { .. }
                | SyscallRequest::Mkdirat { .. }
                | SyscallRequest::Renameat { .. }
                | SyscallRequest::Symlinkat { .. }
                | SyscallRequest::Ftruncate { .. }
                | SyscallRequest::Unlinkat { .. }
                | SyscallRequest::Linkat { .. }
                | SyscallRequest::Write { .. }
                | SyscallRequest::Writev { .. }
                | SyscallRequest::Read { .. }
                | SyscallRequest::Readv { .. }
                | SyscallRequest::Ioctl { .. }
                | SyscallRequest::Ppoll { .. }
                | SyscallRequest::Socket { .. }
                | SyscallRequest::Socketpair { .. }
                | SyscallRequest::Dup { .. }
                | SyscallRequest::Fcntl { .. }
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
            SyscallRequest::CloseRange { first, last, flags } => {
                syscall!(sys_close_range(first, last, flags))
            }
            SyscallRequest::Fsync { fd } => syscall!(sys_fsync(fd)),
            SyscallRequest::Fdatasync { fd } => syscall!(sys_fdatasync(fd)),
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
            SyscallRequest::Fchownat {
                dirfd,
                pathname,
                owner,
                group,
            } => pathname
                .to_cstring::<Platform>()
                .map_or(Err(Errno::EFAULT), |path| {
                    syscall!(sys_fchownat(dirfd, path, owner, group))
                }),
            SyscallRequest::Fchown { fd, owner, group } => syscall!(sys_fchown(fd, owner, group)),
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
            SyscallRequest::RtSigsuspend { mask, sigsetsize } => {
                self.sys_rt_sigsuspend(ctx, mask, sigsetsize)
            }
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
            SyscallRequest::Fallocate {
                fd,
                mode,
                offset,
                len,
            } => syscall!(sys_fallocate(fd, mode, offset, len)),
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
            SyscallRequest::Linkat {
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
                            syscall!(sys_linkat(olddirfd, oldpath, newdirfd, newpath, flags))
                        })
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
            // `statfs`/`fstatfs` describe the FILESYSTEM rather than a file. litebox's guest fs
            // is a layered in-memory/tar-backed overlay with no fixed device behind it, so there
            // are no true block counts to report -- but ENOSYS is the wrong answer: it is
            // user-visible (`stat -f /` printed "Function not implemented") and library code
            // that probes the filesystem can take a pessimistic path on failure rather than
            // merely losing an optimisation. Report an honest description instead.
            //
            // `f_type` is TMPFS_MAGIC: the guest fs really does behave like a memory-backed
            // filesystem (contents live in host memory, nothing is durable across runs), so
            // callers that special-case tmpfs -- skipping fsync-heavy durability paths, or
            // declining to place lock files -- get the behaviour that is actually correct here.
            // Block counts are reported as a large, non-zero capacity rather than 0: callers
            // routinely treat 0 free blocks as "disk full" and refuse to write.
            //
            // Both arms validate FIRST (`sys_stat`/`sys_fstat`) that the target actually exists /
            // the fd is actually open before reporting this canned success -- this used to ignore
            // `pathname`/`fd` entirely (`pathname: _`/`fd: _`) and report a successful tmpfs-shaped
            // statfs for ANY path or fd number, including ones that don't exist/aren't open. That
            // is genuinely wrong on its own (a `statfs()` existence probe on a nonexistent path,
            // or an `fstatfs()` on a stale/closed/never-opened fd, must fail, not silently
            // succeed) and was specifically investigated as a candidate for the Xvfb
            // `ProcSELinuxGetClientContext` SIGSEGV (39th pass): libselinux's `is_selinux_enabled()`
            // (`libselinux/src/init.c` `verify_selinuxmnt`) calls `statfs("/sys/fs/selinux", ...)`
            // and only treats SELinux as present if `f_type == SELINUX_MAGIC` (0xf97cff8c) --
            // since this handler always returns `TMPFS_MAGIC` (0x01021994), the magic never
            // matches even with the existence bug, so that specific theory does NOT hold (the
            // XSELinux protocol extension's own `AddExtension` call is gated on
            // `is_selinux_enabled()` in `Xext/xselinux_ext.c`'s `SELinuxExtensionInit` and should
            // never even run in this guest) -- but the existence-blind behavior is a real,
            // independent correctness bug on its own and is fixed here regardless.
            SyscallRequest::Statfs { pathname, buf } => {
                pathname
                    .to_cstring::<Platform>()
                    .map_or(Err(Errno::EFAULT), |path| {
                        self.sys_stat(path)?;
                        write_tmpfs_statfs::<Platform>(buf)
                    })
            }
            SyscallRequest::Fstatfs { fd, buf } => {
                self.sys_fstat(fd)?;
                write_tmpfs_statfs::<Platform>(buf)
            }
            // Advisory only: the hint is accepted and deliberately ignored (see the dispatch
            // site's comment on why refusing an advisory hint is pure downside).
            SyscallRequest::Fadvise64 => Ok(0),
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
            } => {
                // `utimensat(dirfd, NULL, times, flags)` is legal and means "operate on `dirfd`
                // itself" -- it is exactly what `futimens(fd, times)` compiles down to on musl
                // (see `sys_utimensat`'s own doc comment, which already documents this), and
                // coreutils' `touch` reaches it too. A NULL `pathname` is therefore NOT a bad
                // address: it must be forwarded as an EMPTY path, which `FsPath::new` already
                // maps to `FsPath::Fd(dirfd)`/`FsPath::Cwd`, rather than rejected.
                //
                // Previously this arm called `to_cstring()` unconditionally and returned
                // `EFAULT` on the resulting `None`, so every `futimens`/NULL-path `utimensat`
                // failed with `Bad address` without ever reaching `sys_utimensat`, whose
                // `AT_EMPTY_PATH` handling for this case was consequently dead code. Confirmed
                // live via `touch` under `--oci-image webtop:debian-i3`: `pathname=0x0`,
                // `path_ok=0x0`, guest reports "setting times of '/tmp/direct_touch': Bad
                // address".
                let path = if pathname.is_null() {
                    Some(alloc::ffi::CString::default())
                } else {
                    pathname.to_cstring::<Platform>()
                };
                path
            }
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
            SyscallRequest::Signalfd4 {
                fd,
                mask,
                sizemask,
                flags,
            } => {
                syscall!(sys_signalfd4(fd, mask, sizemask, flags))
            }
            SyscallRequest::TimerfdCreate { clockid, flags } => {
                syscall!(sys_timerfd_create(clockid, flags))
            }
            SyscallRequest::TimerfdSettime {
                fd,
                flags,
                new_value,
                old_value,
            } => syscall!(sys_timerfd_settime(fd, flags, new_value, old_value)),
            SyscallRequest::TimerfdGettime { fd, curr_value } => {
                syscall!(sys_timerfd_gettime(fd, curr_value))
            }
            SyscallRequest::MemfdCreate { name, flags } => {
                // The name is cosmetic only (see `sys_memfd_create`'s own doc comment) but a bad
                // pointer must still surface as a real `EFAULT`, matching real Linux, rather than
                // being silently ignored.
                name.to_cstring::<Platform>()
                    .map_or(Err(Errno::EFAULT), |_name| syscall!(sys_memfd_create(flags)))
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
            SyscallRequest::Getresuid { ruid, euid, suid } => {
                syscall!(sys_getresuid(ruid, euid, suid))
            }
            SyscallRequest::Getresgid { rgid, egid, sgid } => {
                syscall!(sys_getresgid(rgid, egid, sgid))
            }
            SyscallRequest::Getuid => Ok(self.sys_getuid() as usize),
            SyscallRequest::Getgid => Ok(self.sys_getgid() as usize),
            SyscallRequest::Geteuid => Ok(self.sys_geteuid() as usize),
            SyscallRequest::Getegid => Ok(self.sys_getegid() as usize),
            SyscallRequest::Setuid { uid } => syscall!(sys_setuid(uid)),
            SyscallRequest::Setgid { gid } => syscall!(sys_setgid(gid)),
            SyscallRequest::Setresuid { ruid, euid, suid } => {
                syscall!(sys_setresuid(ruid, euid, suid))
            }
            SyscallRequest::Setresgid { rgid, egid, sgid } => {
                syscall!(sys_setresgid(rgid, egid, sgid))
            }
            SyscallRequest::Getgroups { size, list } => syscall!(sys_getgroups(size, list)),
            SyscallRequest::Setgroups { size, list } => syscall!(sys_setgroups(size, list)),
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
            SyscallRequest::SchedSetAffinity { pid, len, mask } => {
                syscall!(sys_sched_setaffinity(pid, len, mask))
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
            SyscallRequest::SchedGetPriorityMax { policy } => {
                Ok(sched_priority_range(policy)?.1 as usize)
            }
            SyscallRequest::SchedGetPriorityMin { policy } => {
                Ok(sched_priority_range(policy)?.0 as usize)
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
            SyscallRequest::Shmget { key, size, shmflg } => self.sys_shmget(key, size, shmflg),
            SyscallRequest::Shmat {
                shmid,
                shmaddr,
                shmflg,
            } => self.sys_shmat(shmid, shmaddr, shmflg),
            SyscallRequest::Shmdt { shmaddr } => self.sys_shmdt(shmaddr),
            SyscallRequest::Shmctl { shmid, cmd, buf } => self.sys_shmctl(shmid, cmd, buf),
            SyscallRequest::Waitid {
                idtype,
                id,
                infop,
                options,
                rusage,
            } => self.sys_waitid(idtype, id, infop, options, rusage),
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
                if crate::diag::strace_summary_enabled() {
                    crate::diag::record_unsupported_subcommand(
                        &alloc::format!("{request:?}"),
                        "ENOSYS",
                        self.pid.get(),
                        &alloc::string::String::from_utf8_lossy(&self.comm.get()),
                    );
                }
                Err(Errno::ENOSYS)
            }
        }
    }
}

/// Global shim state, shared across all tasks.
/// The pipe standing behind one open FIFO: both ends, held for the FIFO's lifetime.
///
/// See `GlobalState::fifo_registry` for why both are kept rather than just the one an opener asked
/// for.
struct FifoPipe<Platform: ShimPlatform> {
    reader: litebox::pipes::PipeFd<Platform>,
    writer: litebox::pipes::PipeFd<Platform>,
}

/// Cross-process-shareable handle to the shim-wide [`GlobalState`] singleton (see
/// `litebox::platform::SharedKernelStateProvider`'s own doc comment) -- mirrors
/// `litebox::LiteBox`'s own `Platform::Handle<LiteBoxX<Platform>>`-wrapping shape exactly, as a
/// named type since `GlobalState` is referenced across many files/fields in this crate the same
/// way `Arc<GlobalState<Platform, FS>>` was referenced before this trait existed -- every such
/// call site needs no further change: `Clone`/`Deref` below give it identical ergonomics.
///
/// # ROOT-CAUSE FIX (2026-09-17): `litebox` is a SEPARATE, always-locally-valid field here, not
/// read from the shared `GlobalState.litebox` field it shadows
///
/// `GlobalState`'s own `litebox: LiteBox<Platform>` field is placed INLINE in the cross-process
/// shared kernel arena by `create_shared_kernel_state` (see `LinuxShimBuilder::build`) -- but
/// `LiteBox<Platform>` is `Platform::Handle<LiteBoxX<Platform>>` (effectively an `Arc` pointer):
/// `SharedArc::new`/`create_shared_kernel_state` places only the pointer's literal inline bytes,
/// never what it points to (see `AGENTS.md`'s "Does NOT close XVFB_FAILED/DBUS_FAILED" section --
/// the SAME defect class already documented for `unix_addr_table`/`pty_registry`/etc: "an
/// attaching process's copy of the root pointer is meaningless in its own address space"). A
/// cross-process-fork child that ATTACHES (rather than creates) the shared `GlobalState` was
/// therefore reading the FIRST creator's private-heap `LiteBox` pointer value -- meaningless, and
/// dereferencing effectively garbage memory in the attaching child's own address space.
///
/// Live-confirmed root cause of the pre-existing, load-scaling stack-overflow class this session
/// was tasked with root-causing: bisection (temporary `[bisect1..5]` diagnostic logging, since
/// removed) proved the very FIRST cross-process fork child in a boot always completes cleanly (it
/// takes the CREATE branch, so its own `litebox` value is genuinely its own), while every
/// SUBSEQUENT one -- deterministically, regardless of guest program complexity, reproduced by
/// `mkdir`/`rm -rf` as readily as `xset q` -- died with a real host `STATUS_STACK_OVERFLOW` inside
/// `initialize_stdio_in_shared_descriptors_table`'s very first `descriptor_table_mut()`/
/// `set_entry_metadata` call, i.e. the first real use of the ATTACHED, cross-process-garbage
/// `litebox` pointer's `RwLock`. Chasing that garbage pointer's lock/wait-queue bookkeeping is
/// what actually consumed the stack, not guest instruction count or fork-verify single-stepping
/// (both independently ruled out live before this was found).
///
/// Fix: `GlobalStateHandle` keeps its OWN `litebox` field, populated from THIS process's own
/// `LinuxShimBuilder::litebox` (always freshly, validly constructed in `LinuxShimBuilder::new`,
/// every process, attach or create alike) rather than ever reading `GlobalState`'s shared copy.
/// Rust's field-resolution rules try the receiver's own concrete type before auto-`Deref`ing, so
/// this SHADOWS `GlobalState.litebox` transparently for every one of this crate's existing
/// `xxx.litebox` call sites -- none of them needed to change.
/// # Second instance of the SAME defect, fixed the SAME way: `proc_self_info`/`pts_registry`
///
/// `GlobalState`'s own doc comments already named this "known cross-process-attach gap" (2026-
/// 09-17 create-vs-attach pass) before this session started: `LinuxShimBuilder::default_fs`/
/// `default_fs_multi_layer` mounts the `/proc/self` and `/dev/pts` backends with a clone of
/// `LinuxShimBuilder::proc_self_info`/`pts_registry` BEFORE `build()` (and hence before the
/// create-vs-attach decision) ever runs -- so an attaching child's own mounted FS backend keeps
/// pointing at ITS OWN fresh, per-process table while a `proc_self_info`/`pts_registry` field on
/// the shared `GlobalState` struct would be whichever one the ORIGINAL creator made, exactly
/// like `litebox` above. Live-confirmed THIS session: with the `litebox` fix above alone, the
/// SECOND cross-process-forked guest process to ever call `execve` (i.e. the second real command
/// in a boot) hit a clean, host-diagnosed `STATUS_ACCESS_VIOLATION` (`addr=0xffffffffffffffff`)
/// inside `<litebox::fs::procfs::ProcSelfTable>::set`, called from `Task::load_program` through
/// `self.global.proc_self_info` -- the exact mechanism this pre-existing doc comment predicted.
/// Fixed the same way as `litebox`: `GlobalStateHandle` keeps its own copies, populated from
/// `LinuxShimBuilder`'s per-process fields (the SAME instances `default_fs`/`default_fs_multi_
/// layer` already mounted), never from `GlobalState`'s shared/cross-process-stale ones.
///
/// # Third instance of the SAME defect: `elf_patch_cache`
///
/// Live-diagnosed 2026-09-17, same session as `unix_addr_table` presence sharing: with the
/// `litebox`/`proc_self_info`/`pts_registry` fixes above landed, a cross-process-fork boot
/// progressed to the first `execve` of a plain external command (`mkdir`, reproduced equally by
/// any exec) and hit a real `alloc::collections::btree::node.rs` panic ("range end index ... out
/// of range for slice of length ...") inside `BTreeMap::entry(...).or_insert(...)` on
/// `GlobalState::elf_patch_cache` -- a corrupted-node read exactly like the `litebox` stack
/// overflow, just one field deeper. Unlike `unix_addr_table` et al., this field does not need a
/// `SharedUnixAddrPresenceTable`-style flat rebuild: every key is `(pid, fd)` and no call site
/// ever looks up another process's entry (see `GlobalState`'s own removed-field note above), so
/// it is fixed the SAME way as `litebox`/`proc_self_info`/`pts_registry`: `GlobalStateHandle`
/// carries its own `elf_patch_cache`, freshly constructed once per process in
/// `LinuxShimBuilder::build`, attach or create alike, shadowing `GlobalState`'s (now removed)
/// field for every existing `self.global.elf_patch_cache` call site with no further change.
///
/// # Fourth instance of the SAME defect: `exec_ranges_cache`
///
/// Live-diagnosed 2026-09-17, immediately after the `elf_patch_cache` fix above unblocked the
/// next `execve`: fixed the identical way, for a performance-only (not per-process-identity)
/// reason -- see `GlobalState`'s own removed-field note for `exec_ranges_cache`.
///
/// # Fifth instance of the SAME defect: `segment_scan_cache`
///
/// Live-diagnosed 2026-09-17, immediately after the `exec_ranges_cache` fix above: the same
/// mkdir repro stopped panicking but started HANGING instead (host CPU climbing, zero new log
/// output) -- see `GlobalState`'s own removed-field note for `segment_scan_cache` for why a
/// corrupted `BTreeMap` can hang instead of panicking. Fixed the identical way.
///
/// # Sixth instance of the SAME defect, different underlying shape: `futex_manager`
///
/// Live-diagnosed 2026-09-17 via two `cdb -p` snapshots 20s apart with identical stacks (confirmed
/// genuine hang, not slow progress): with the fifth instance above and the separate `RawMutex`/
/// `Pipes` fixes all landed, the same repro hung one step later inside `FutexManager::wake` ->
/// `LoanList::extract_if` -> `RawMutex::block`. `FutexManager.table: Box<[LoanList<...>; 256]>` is
/// process-private-heap exactly like `elf_patch_cache` et al. above, but `LoanList` itself is
/// structurally deeper, not just "a `BTreeMap` whose nodes happen to be on the wrong heap": its own
/// doc comment says entries are "allocated once by the caller, potentially on the stack", and
/// `FutexManager::wait` does exactly that (`pin!(LoanListEntry::new(...))` on the waiting guest
/// thread's own stack). A stack address is fork-family-address-identical only for the ONE thread
/// that actually called `fork()`; any OTHER thread's stack-resident entry, or lock state a
/// non-forking thread held at fork time, becomes permanently unrecoverable garbage to every other
/// process in the family -- classic post-fork "the lock's owner doesn't exist here" deadlock, not a
/// relocatable-pointer bug. There is no "move it into the shared arena" fix available even in
/// principle. Resolved the same way as `elf_patch_cache`: `FutexManager`'s own pre-existing doc
/// comment already scopes it to "private" (single-process) futexes only ("this only supports
/// 'private' futexes, since it assumes only a single process"), so giving each process in the fork
/// family its own fresh `FutexManager` is not a workaround, it is the already-documented intended
/// semantics -- `GlobalStateHandle` carries its own, always-freshly-constructed `futex_manager`
/// field, shadowing `GlobalState`'s (now removed) field for every existing
/// `self.global.futex_manager` call site with no further change.
pub(crate) struct GlobalStateHandle<Platform: ShimPlatform, FS: ShimFS> {
    inner: Platform::Handle<GlobalState<Platform, FS>>,
    litebox: litebox::LiteBox<Platform>,
    proc_self_info: Arc<litebox::sync::RwLock<Platform, litebox::fs::procfs::ProcSelfTable>>,
    pts_registry: Arc<litebox::sync::RwLock<Platform, litebox::fs::devices::PtsRegistry>>,
    elf_patch_cache: Arc<litebox::sync::Mutex<Platform, syscalls::mm::ElfPatchCache>>,
    exec_ranges_cache: Arc<litebox::sync::Mutex<Platform, syscalls::mm::ExecRangesCache>>,
    segment_scan_cache: Arc<litebox::sync::Mutex<Platform, syscalls::mm::SegmentScanCache>>,
    futex_manager: Arc<FutexManager<Platform>>,
    /// Seventh/eighth instances of the SAME defect this struct's own doc comment already
    /// documents six times over: see `LinuxShimBuilder::build`'s `my_memfds`/`my_shared_files`
    /// for the live-caught 2026-09-18 evidence (`sed`/`xset` both died on this, a corrupted-
    /// `BTreeMap`-node panic in `syscalls::mm::MemfdRegistry`, fixed the identical way).
    memfds: Arc<litebox::sync::Mutex<Platform, syscalls::mm::MemfdRegistry<Platform>>>,
    shared_files: Arc<litebox::sync::Mutex<Platform, syscalls::mm::MemfdRegistry<Platform>>>,
    /// Ninth instance of the SAME defect class this struct's own doc comment documents eight
    /// times over, found during the 2026-09-18 systematic `GlobalState`-field audit: this table's
    /// own doc comment (`syscalls::unix::SharedUnixAddrPresenceTable`'s, on the removed
    /// `GlobalState` field below) already says the real per-address bind/listen entries are kept
    /// "alongside (never instead of) each process's OWN real `UnixAddrTable`" -- i.e. this was
    /// always intended to be per-process private state, with `unix_addr_presence`/
    /// `unix_shared_conn_table` below (both flat, pointer-free, and correctly left as plain
    /// `GlobalState` fields) doing all of the genuine cross-process work. Leaving the real
    /// `BTreeMap` itself as a byte-shared `GlobalState` field was still the same live hazard as
    /// `elf_patch_cache` et al.: an attaching cross-process-fork child's copy of its root pointer
    /// is the first creator's, meaningless in its own address space, on the very first `bind()`/
    /// `connect()`/`listen()` that child performs. Fixed the identical way: `GlobalStateHandle`
    /// carries its own, always-freshly-constructed-per-process `unix_addr_table`, shadowing
    /// `GlobalState`'s (now removed) field for every existing `self.global.unix_addr_table` call
    /// site with no further change.
    unix_addr_table: Arc<litebox::sync::RwLock<Platform, syscalls::unix::UnixAddrTable<Platform, FS>>>,
    /// Tenth instance of the SAME defect class, found in the same audit: `GlobalState::
    /// fifo_registry`'s own doc comment already says outright "shared by every thread of this
    /// process -- but NOT across processes" -- i.e. this too was always meant to be per-process
    /// private state, mistakenly placed as a byte-shared `GlobalState` field. Fixed the identical
    /// way, shadowing `GlobalState`'s (now removed) field.
    fifo_registry: Arc<
        litebox::sync::RwLock<Platform, alloc::collections::BTreeMap<(usize, usize), FifoPipe<Platform>>>,
    >,
    /// Eleventh instance of the SAME defect class this struct's own doc comment documents ten
    /// times over: a real cross-process-forked child's copy of this `BTreeMap<u32,
    /// syscalls::pty::PtyFd<Platform>>`'s root pointer is the first creator's, meaningless in its
    /// own address space -- the exact same live hazard as `elf_patch_cache`/`unix_addr_table`/etc,
    /// just never yet caught live for pty because no earlier pass's boot reached a cross-process
    /// pty touch. Unlike `fifo_registry` (whose own doc comment explicitly disclaimed any
    /// cross-process need), `syscalls::pty::SharedPtyTable`'s own doc comment on `GlobalState`
    /// below establishes real cross-process pty registration/data specifically because
    /// `pty_registry`'s doc comment (right below) DOES claim genuine cross-process need ("any
    /// process that knows the id ... can open it", matching real devpts) -- so this field alone is
    /// shadowed per-process exactly like the ten before it (fixing the crash), while
    /// `SharedPtyTable` (a plain, pointer-free `GlobalState` field, same free-riding-on-
    /// `GlobalState`'s-own-sharing rationale as `unix_addr_presence`) separately restores the
    /// genuine cross-process capability this one alone can no longer provide.
    ///
    /// Registry of allocated ptys' slave-side fd, keyed by pty id (`TIOCGPTN`'s value). The slave
    /// fd held here is never installed into any process's own fd table directly; each
    /// `open("/dev/pts/<id>")` duplicates it (via [`litebox::fd::Descriptors::duplicate`], the
    /// same mechanism `dup()`/`fork()` use) to produce an independent fd sharing the same
    /// underlying entry, for every open THIS process itself performs. A DIFFERENT process's
    /// `open("/dev/pts/<id>")` for an id THIS process allocated instead takes
    /// `GlobalStateHandle::pts_open`'s `SharedPtyTable`-backed cross-process fallback.
    pty_registry: Arc<
        litebox::sync::RwLock<Platform, alloc::collections::BTreeMap<u32, syscalls::pty::PtyFd<Platform>>>,
    >,
    /// Same reasoning as [`Self::pty_registry`] immediately above. Registry of allocated ptys'
    /// master-side fd, keyed by pty id, populated only for a pty created via
    /// `Task::attach_pty_stdio` (the session-daemon `--pty-mode` path) -- see that field's
    /// original doc comment (preserved verbatim on `LinuxShim::pty_master_read`'s own doc comment)
    /// for the full rationale. `LinuxShim::pty_master_read`/`pty_master_write` fall back to
    /// `SharedPtyTable` directly when THIS process's own copy of this map doesn't have the
    /// requested id (i.e. it was allocated by a different process in the fork family).
    daemon_pty_masters: Arc<
        litebox::sync::RwLock<Platform, alloc::collections::BTreeMap<u32, syscalls::pty::PtyFd<Platform>>>,
    >,
}

impl<Platform: ShimPlatform, FS: ShimFS> Clone for GlobalStateHandle<Platform, FS> {
    fn clone(&self) -> Self {
        GlobalStateHandle {
            inner: self.inner.clone(),
            litebox: self.litebox.clone(),
            proc_self_info: self.proc_self_info.clone(),
            pts_registry: self.pts_registry.clone(),
            elf_patch_cache: self.elf_patch_cache.clone(),
            exec_ranges_cache: self.exec_ranges_cache.clone(),
            segment_scan_cache: self.segment_scan_cache.clone(),
            futex_manager: self.futex_manager.clone(),
            memfds: self.memfds.clone(),
            shared_files: self.shared_files.clone(),
            unix_addr_table: self.unix_addr_table.clone(),
            fifo_registry: self.fifo_registry.clone(),
            pty_registry: self.pty_registry.clone(),
            daemon_pty_masters: self.daemon_pty_masters.clone(),
        }
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> core::ops::Deref for GlobalStateHandle<Platform, FS> {
    type Target = GlobalState<Platform, FS>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> GlobalStateHandle<Platform, FS> {
    /// Lock the shared `Network` and rebind its process-relative fields (`litebox`, `device`) to
    /// THIS process's own, always-locally-valid state before handing out the guard -- sixth and
    /// seventh instances of the SAME cross-process-stale-pointer defect class documented on this
    /// struct's own doc comment, found one (or two) levels deeper than the `GlobalState` fields
    /// above: `Network` itself is genuinely, correctly meant to be shared across the whole fork
    /// family (one virtual NIC for the whole guest), but two of its OWN fields are raw pointers
    /// captured once by whichever process constructed `GlobalState` first, meaningless (or, after
    /// that process exits, genuinely dangling) in every other process's address space. Every call
    /// site that used to reach `GlobalState.net.lock()` directly must go through this instead --
    /// see `litebox::net::Network::rebind_per_process_fields`'s own doc comment for the live crash
    /// evidence (first `STATUS_ACCESS_VIOLATION` inside `Descriptors::iter_mut`, reached via
    /// `close_pending_sockets`; second, immediately after that fix landed, inside
    /// `litebox_platform_windows_userland::net::receive_ip_packet` via `phy::Device::receive` --
    /// both on a plain `mkdir` fork child with zero sockets of its own).
    ///
    /// Also the sole call site that consults [`litebox::sync::Mutex::lock_recovering_poison`] for
    /// this particular lock: if acquiring it required forcing open a lock whose recorded holder
    /// was confirmed dead mid-hold (`platform::RawMutex::take_poison`, set by
    /// `litebox_platform_windows_userland`'s `try_recover_from_dead_holder_unregistered`), the
    /// `Network` this guard protects may be torn -- some handle removed from `socket_set` but
    /// still named in `closing_in_background`, or vice versa -- so it is wholesale reset to a safe
    /// empty state (`Network::reset_after_poisoning`, see its own doc comment for the full defect
    /// and the accepted, disclosed loss of in-flight connections this trades for) before this
    /// guard is ever handed to a caller. Live evidence this closes: every occurrence of
    /// `smoltcp::iface::socket_set.rs:103`'s `"handle does not refer to a valid socket"` panic seen
    /// so far was immediately preceded in the log by exactly this dead-holder-recovery warning.
    pub(crate) fn net_lock(
        &self,
    ) -> litebox::sync::MutexGuard<'_, Platform, litebox::net::Network<Platform>> {
        let (mut guard, recovered_from_dead_holder) = self.net.lock_recovering_poison();
        if recovered_from_dead_holder {
            litebox_util_log::warn!(
                "GlobalStateHandle::net_lock: acquired a Network lock recovered from a dead holder -- resetting Network to a safe empty state to avoid reading torn socket_set/closing_in_background/local_port_allocator state"
            );
            guard.reset_after_poisoning();
        }
        guard.rebind_per_process_fields(&self.litebox);
        guard
    }

    /// Rebind the shared `Pipes`'s `litebox` handle to THIS process's own, always-locally-valid
    /// state before handing out access -- same "rebind a stale-in-shared-memory pointer field
    /// before every use" shape as [`Self::net_lock`], for `litebox::pipes::Pipes` instead of
    /// `litebox::net::Network`. See [`litebox::pipes::Pipes`]'s own doc comment for the live crash
    /// evidence (`STATUS_ACCESS_VIOLATION` inside a per-process descriptor table's own `Drop`,
    /// reached while tearing down an inherited stdio pipe). Every call site that used to reach
    /// `GlobalState.pipes` directly must go through this instead.
    pub(crate) fn pipes(&self) -> &litebox::pipes::Pipes<Platform> {
        self.pipes.rebind_per_process_fields(&self.litebox);
        &self.pipes
    }
}

struct GlobalState<Platform: ShimPlatform, FS: ShimFS> {
    /// The platform instance used throughout the shim.
    platform: &'static Platform,
    // `FS` is no longer used by any field of this struct as of the 2026-09-18 systematic audit:
    // its last use (`unix_addr_table: RwLock<Platform, UnixAddrTable<Platform, FS>>`) moved to
    // `GlobalStateHandle` (see that struct's `unix_addr_table` field doc comment) since it was
    // always meant to be per-process-private state, not a genuinely shared `GlobalState` field.
    // Kept as a zero-sized marker rather than dropped from this struct's generics entirely,
    // since every caller already threads `FS` through `GlobalState<Platform, FS>` and removing
    // the parameter would be a wider, purely-cosmetic churn for no behavior change.
    _fs: core::marker::PhantomData<FS>,
    // NOTE: this struct deliberately has NO `litebox` field. `LiteBox<Platform>` is
    // `Platform::Handle<LiteBoxX<Platform>>` (effectively an `Arc` pointer); a value placed here
    // would be copied byte-for-byte into the cross-process shared kernel arena on the CREATE
    // path, and a later ATTACHing cross-process-fork child would read back the FIRST creator's
    // pointer -- meaningless in its own address space (the same defect class already documented
    // for `unix_addr_table`/`pty_registry`/etc, see `docs/AGENTS_ARCHIVE_2026-09-17.md`), root-
    // caused THIS session as the actual mechanism behind a genuine, load-scaling, live host
    // `STATUS_STACK_OVERFLOW`. Every reader of "the shim-wide `LiteBox`" instead goes through
    // `GlobalStateHandle`'s own separate, always-locally-valid `litebox` field (see its doc
    // comment) -- do not re-add a field with this name here.
    // NOTE: this struct deliberately has NO `futex_manager` field -- SIXTH instance of the SAME
    // cross-process-garbage-pointer defect class documented on `GlobalStateHandle`'s own doc
    // comment (`litebox`/`proc_self_info`/`pts_registry`/`elf_patch_cache`/etc), live-diagnosed
    // 2026-09-17 via `cdb -p`: a hang inside `FutexManager::wake` -> `LoanList::extract_if` ->
    // `RawMutex::block`. `FutexManager` itself already only supports `FUTEX_PRIVATE_FLAG`-style
    // per-process futexes (see its own doc comment), and `LoanList` entries are pinned on the
    // WAITING THREAD'S OWN STACK by design, so there is no way to relocate them into shared memory
    // even in principle -- unlike `Network`/`Pipes`, this one genuinely cannot be rebound, only
    // kept per-process. `GlobalStateHandle` carries its own, always-freshly-constructed
    // `futex_manager` field instead (populated once per process in `LinuxShimBuilder::build`,
    // attach or create alike) -- do not re-add a field with this name here.
    /// The anonymous pipe implementation.
    pipes: Pipes<Platform>,
    /// The network subsystem.
    net: litebox::sync::Mutex<Platform, Network<Platform>>,
    /// The time when the shim was started.
    boot_time: <Platform as TimeProvider>::Instant,
    /// Next thread ID to assign.
    // TODO: better management of thread IDs
    next_thread_id: core::sync::atomic::AtomicI32,
    /// Count of cross-process-fork children (`LITEBOX_PROCESS_FORK=1`) that have been spawned
    /// (`CreateProcessW` succeeded) but not yet reaped by their parent's `wait4()` -- a plain
    /// `AtomicU32` field of this same struct, free-riding on `GlobalState`'s own cross-process
    /// sharing exactly like `unix_addr_presence` below, so every process in the fork FAMILY
    /// (not just direct parent/child pairs) observes the same live count.
    ///
    /// Root-caused 76th pass: each cross-process-fork child independently rebuilds its entire
    /// merged/rewritten OCI rootfs into its own private heap on startup (~350MB-1.1GB working
    /// set observed live, `.wfgy/pass76_crater_procsnapshot.txt`), and nothing previously bounded
    /// how many such children could be simultaneously alive -- a real boot's desktop-startup
    /// scripts fork in a TREE (parent -> child -> grandchild -> great-grandchild via nested
    /// command substitutions/subshells), not a flat sequence, so this cost multiplies by tree
    /// depth x branching factor. A live capture at the crater moment found 33 simultaneous
    /// `litebox_runner_linux_on_windows_userland.exe` processes, 4 generations deep, ~10.5GB
    /// combined working set on a 15GB host (0.4GB free), one call stack after another rebuilding
    /// the identical read-only rootfs. `try_reserve_fork_slot`/`release_fork_slot` (see
    /// `syscalls::process`) gate `spawn_cross_process_fork_child` on this counter staying under
    /// [`CROSS_PROCESS_FORK_CONCURRENCY_CAP`], bounding peak transient RAM without changing fork
    /// correctness (still genuinely cross-process, just admission-controlled) -- a real, deeper
    /// fix (share the parsed rootfs itself instead of re-deriving it per child) remains open, see
    /// AGENTS.md.
    live_cross_process_fork_children: core::sync::atomic::AtomicU32,
    // NOTE: this struct deliberately has NO `unix_addr_table` field -- NINTH instance of the SAME
    // cross-process-garbage-pointer defect class documented on `GlobalStateHandle`'s own doc
    // comment, found in the 2026-09-18 systematic audit. See `GlobalStateHandle::unix_addr_table`'s
    // own doc comment for the full reasoning (this was always meant to be per-process private
    // state, per `SharedUnixAddrPresenceTable`'s own doc comment) -- do not re-add a field with
    // this name here.
    /// Cross-process-visible companion to the real per-process `unix_addr_table` (see
    /// `syscalls::unix::SharedUnixAddrPresenceTable`'s own doc comment for exactly what it does
    /// and does not close): a plain, no-pointer-indirection fixed-size field of this SAME struct,
    /// so it inherits whatever cross-process sharing `GlobalState` itself already gets (real on
    /// `WindowsUserland`'s cross-process-fork path, an ordinary unshared value everywhere else)
    /// for free -- no second `SharedKernelStateProvider` slot needed, unlike `unix_addr_table`
    /// itself, whose `BTreeMap` nodes remain private-heap-allocated regardless.
    unix_addr_presence: syscalls::unix::SharedUnixAddrPresenceTable,
    /// Shared-arena-native pool of cross-process AF_UNIX connection byte-transport slots -- see
    /// `syscalls::unix::SharedUnixConnTable`'s own doc comment (the "Shared cross-process AF_UNIX
    /// connection data plane" section of `syscalls/unix.rs`) for the full rendezvous design this
    /// and `unix_shared_connect_queue` below implement together. A plain field of this same
    /// struct, same free-riding-on-`GlobalState`'s-own-sharing rationale as `unix_addr_presence`.
    unix_shared_conn_table: syscalls::unix::SharedUnixConnTable<Platform>,
    /// Cross-process `connect()`/`accept()` rendezvous queue -- see `unix_shared_conn_table`'s doc
    /// comment above.
    unix_shared_connect_queue: syscalls::unix::SharedUnixConnectQueue,
    /// Cross-process-visible side channel for the small handful of named byte-string files a
    /// long-lived, never-exiting daemon publishes that a short-lived sibling needs to read (e.g.
    /// D-Bus's `/tmp/addr`) -- see `syscalls::file::SharedFilePublishTable`'s own doc comment for
    /// the full mechanism and scope boundary. Same free-riding-on-`GlobalState`'s-own-sharing
    /// rationale as `unix_addr_presence` above: a plain, pointer-free field of this same struct.
    shared_file_publish: syscalls::file::SharedFilePublishTable,
    // NOTE: this struct deliberately has NO `elf_patch_cache` field -- THIRD instance of the SAME
    // cross-process-garbage-pointer defect class documented on `GlobalStateHandle`'s own doc
    // comment (`litebox`/`proc_self_info`/`pts_registry`), live-diagnosed 2026-09-17: an attaching
    // cross-process-fork child's copy of this `BTreeMap<(pid, fd), ElfPatchState>`'s root pointer
    // is the FIRST creator's, meaningless in its own address space (real panic:
    // `alloc::collections::btree::node.rs` `insert_recursing`, "range end index ... out of range
    // for slice of length ...", from `Task::do_mmap_file`'s `elf_patch_cache.entry(...)
    // .or_insert(...)` on the very first `execve` any cross-process-fork child performs). Unlike
    // `unix_addr_table` et al., this one genuinely does NOT need true cross-process visibility to
    // begin with: every key is `(self.pid.get(), fd)` (see `Task::elf_patch_key`) and no call site
    // ever looks up another pid's entry -- the doc comment on `syscalls::mm::ElfPatchKey` explains
    // the `pid` component exists only to stop two DIFFERENT processes' entries from colliding on
    // one fd number, not to let one process observe another's state. `ElfPatchState` itself also
    // holds absolute per-process addresses (`trampoline_addr`, `file_mappings`, `patched_ranges`)
    // that are meaningless outside the process that produced them, and re-running the patcher on
    // already-patched code is documented as idempotent/safe (`ElfPatchState`'s own doc comment on
    // `patched_ranges`) -- so a fork child starting with an empty local cache is safe by
    // construction, exactly like `litebox`/`proc_self_info`/`pts_registry` above.
    // `GlobalStateHandle` carries its own, always-freshly-constructed `elf_patch_cache` field
    // instead (populated once per process in `LinuxShimBuilder::build`, attach or create alike) --
    // do not re-add a field with this name here.
    // NOTE: this struct deliberately has NO `segment_scan_cache` field either -- FIFTH instance of
    // the SAME defect class, live-diagnosed 2026-09-17 immediately after the `exec_ranges_cache`
    // fix above: with that fix landed, the identical minimal `mkdir` repro no longer panicked but
    // instead HUNG (host CPU climbing with zero new log output for 90+s, no crash) -- a corrupted-
    // BTreeMap symptom one step worse than a clean `navigate.rs`/`node.rs` panic (a garbage root
    // pointer can just as easily walk into a long or cyclic chain as into an out-of-bounds
    // `unwrap`/slice index), inside `do_mmap_file`'s `segment_scan_cache.get(&key)`/`.insert(...)`
    // -- the only other shared `BTreeMap` still on this file's hot path for every fresh `execve`.
    // Originally kept shared for a real performance reason (was: "shared by every mapping of it in
    // every guest process... `xfwm4` mapped `libLLVM` 130 MB 74 times and spent 225.9s patching"),
    // but that reuse is dominated by REPEATED mappings of the same big library WITHIN one process
    // (a single process's own `dlopen` probing loop), which a per-process cache still fully
    // captures -- only cross-PROCESS reuse of an already-scanned file is lost, a real but strictly
    // secondary regression next to a live hang. `GlobalStateHandle` carries its own, always-
    // freshly-constructed `segment_scan_cache` field instead, same as `elf_patch_cache`/
    // `exec_ranges_cache` above -- do not re-add a field with this name here. Restoring genuine
    // cross-process reuse (a flat, pointer-free `SharedUnixAddrPresenceTable`-style redesign, or a
    // real shared-`Arc`-capable allocator) is real, separate, follow-on performance work.
    // NOTE: this struct deliberately has NO `exec_ranges_cache` field -- FOURTH instance of the
    // SAME defect class, live-diagnosed 2026-09-17 immediately after the `elf_patch_cache` fix
    // above unblocked the next `execve`: real panic `alloc::collections::btree::node.rs`
    // `insert_recursing`, "range end index ... out of range for slice of length ...", inside
    // `BTreeMap<(u64, u64), Arc<Vec<Range<u64>>>>::insert` -- the exact type of
    // `syscalls::mm::ExecRangesCache` -- from the ELF-load path's exec-ranges lookup/insert.
    // Unlike `segment_scan_cache` just above (kept shared: performance-critical across processes,
    // per its own doc comment, not yet fixed), this cache's correctness does not depend on
    // cross-process sharing -- every value is a pure, deterministic function of the keyed file's
    // own on-disk section headers (`litebox_syscall_rewriter::executable_section_file_ranges`), so
    // a fork child starting with an empty local cache simply re-derives it correctly on first use,
    // exactly as safe as `elf_patch_cache`'s fix above, just for a performance-only reason instead
    // of a per-process-identity one. `GlobalStateHandle` carries its own, always-freshly-
    // constructed `exec_ranges_cache` field instead -- do not re-add a field with this name here.
    /// System V shared-memory segments, keyed by `shmid`.
    ///
    /// Shim-wide because SysV shm is a global namespace by definition -- any process that knows
    /// the key or id can attach. See [`syscalls::mm::SysvShmSegment`].
    sysv_shm: litebox::sync::Mutex<Platform, syscalls::mm::SysvShmTable>,
    /// Next `shmid` to hand out.
    next_shmid: core::sync::atomic::AtomicI32,
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
    // NOTE: this struct deliberately has NO `pty_registry`/`daemon_pty_masters` fields --
    // ELEVENTH instance of the SAME cross-process-garbage-pointer defect class documented on
    // `GlobalStateHandle`'s own doc comment. Unlike `fifo_registry` (whose own doc comment
    // explicitly disclaimed any cross-process need), `pty_registry`'s ORIGINAL doc comment here
    // (preserved on `GlobalStateHandle::pty_registry`) explicitly claimed one: "any process that
    // knows the id ... can open it", matching real devpts. `GlobalStateHandle` carries its own,
    // always-freshly-constructed-per-process `pty_registry`/`daemon_pty_masters` fields instead
    // (fixing the crash, exactly like the ten instances before it), and the genuine cross-process
    // capability those two fields' doc comments actually need is restored separately by
    // `shared_pty` below -- do not re-add fields with these names here.
    /// Shared-arena-native, fixed-capacity, pointer-free companion to the per-process
    /// `pty_registry`/`daemon_pty_masters` (see [`syscalls::pty::SharedPtyTable`]'s own doc
    /// comment for the full design and its explicit scope limits): existence, control state
    /// (termios/winsize/locked/fg_pgid/packet-mode), and the master<->slave byte data plane for
    /// every currently-allocated pty, visible to every process in the fork family regardless of
    /// which one allocated it. A plain field of this same struct, same free-riding-on-
    /// `GlobalState`'s-own-sharing rationale as `unix_addr_presence`.
    shared_pty: syscalls::pty::SharedPtyTable<Platform>,
    /// Next id to hand out to a freshly `open("/dev/ptmx")`-allocated pty pair.
    next_pty_id: core::sync::atomic::AtomicU32,
    /// The live pipe behind each open FIFO, keyed by the FIFO's `(dev, ino)`.
    ///
    /// A FIFO is a filesystem entry (see `litebox::fs::FileType::Fifo`) with no data of its own;
    /// what `open()` on one has to produce is a PIPE. This registry is what makes every open of
    /// the same path land on the same pipe: the first one creates it, and each open thereafter
    /// duplicates the end its access mode calls for -- exactly the mechanism `pty_registry` uses
    /// for `/dev/pts/<id>`, and for the same reason (a shared namespace any process can open by
    /// name).
    ///
    /// Both ends are held here for the FIFO's lifetime, deliberately. A real FIFO's reader blocks
    /// until a writer appears rather than seeing EOF, and the registry's own writer reference is
    /// what keeps that true when no guest process currently holds one open.
    ///
    /// Shim-wide, so it is shared by every thread of this process -- but NOT across processes. A
    /// cross-process `fork()` child recognises the FIFO (its type travels with the writable layer)
    /// and gets a pipe of its own, so data written by one process does not reach a reader in
    /// another. See `Task::open_fifo`.
    // NOTE: this struct deliberately has NO `fifo_registry` field -- TENTH instance of the SAME
    // cross-process-garbage-pointer defect class documented on `GlobalStateHandle`'s own doc
    // comment, found in the 2026-09-18 systematic audit: this field's own doc comment (above)
    // already says outright it is shared "by every thread of this process -- but NOT across
    // processes", i.e. it was always meant to be per-process private state, mistakenly placed as
    // a byte-shared `GlobalState` field. `GlobalStateHandle` carries its own, always-freshly-
    // constructed-per-process `fifo_registry` field instead -- do not re-add a field with this
    // name here.
    /// Next id to hand out for AF_UNIX socket "autobind" (`bind()` called with no address),
    /// formatted the same way real Linux formats its autobind abstract-namespace names: a
    /// leading NUL byte followed by 5 lowercase hex digits (see `unix(7)`). Real Linux starts
    /// from an unpredictable point and retries on collision; this shim-wide counter instead
    /// increments monotonically, which is simpler and still unique for any realistic number of
    /// autobind calls within one shim instance's lifetime (wraps at 2^20, matching the same
    /// 5-hex-digit range Linux itself uses).
    next_unix_autobind_id: core::sync::atomic::AtomicU32,
    /// Next id to mint a unique, private path for a `memfd_create` fd's backing in-mem file
    /// (see `Task::sys_memfd_create`) -- guarantees two concurrent calls never collide on the
    /// same path even with an identical (or empty) guest-supplied name.
    next_memfd_id: core::sync::atomic::AtomicU64,
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
    /// The one virtual DRM/KMS device's state (`/dev/dri/card0`). Shim-wide, not per-process,
    /// since real DRM device state (allocated buffers, current mode/framebuffer) is genuinely
    /// global -- any process holding a fd to the card sees the same connector/CRTC/buffers, just
    /// like real Linux's `struct drm_device` is one kernel-wide object regardless of how many
    /// processes have it open.
    drm: syscalls::drm::DrmSubsystem<Platform>,
    /// The one virtual evdev keyboard+mouse device's state (`/dev/input/event0`). Shim-wide for
    /// the same reason `drm` is: real input-device state (queued events) is genuinely global,
    /// matching how a real kernel input device is one object regardless of how many processes
    /// have it open.
    evdev: syscalls::evdev::EvdevSubsystem<Platform>,
    // NOTE: this struct deliberately has NO `memfds`/`shared_files` fields -- SEVENTH and EIGHTH
    // instances of the SAME cross-process-garbage-pointer defect class documented on
    // `GlobalStateHandle`'s own doc comment (`litebox`/`proc_self_info`/`pts_registry`/
    // `elf_patch_cache`/etc), live-caught 2026-09-18 under `cdb -o` child-process debugging: a
    // real `alloc::collections::btree::node.rs:1232` "range end index 25710 out of range for
    // slice of length 11" panic inside `MemfdRegistry`'s `BTreeMap::insert`/`get_mut`, hit by a
    // cross-process-fork child's `try_memfd_mmap`/`try_shared_file_mmap` (`syscalls::mm`) on the
    // very first file-backed `mmap()` it performed post-fork (i.e. essentially every exec'd guest
    // binary's own dynamic linker mapping its shared libraries -- confirmed live on both `sed`
    // and, matching the same-day investigation this closes, `xset`). The panic unwound to the
    // guest-execution thread's `.join().expect(...)` in
    // `litebox_runner_linux_on_windows_userland::diag_process_fork_globalstate_probe`, which
    // re-panicked on the process's own `main` thread uncaught -- a clean Rust
    // `std::process::exit(101)` after printing the panic (see that panic message's own stack
    // trace for confirmation), NOT a hardware fault, which is exactly why this crash never showed
    // up in litebox's VEH-based `RECENT_FAULTS`/`RECOVERY_LOG` machinery (nothing there to catch:
    // there was no CPU exception, just a controlled panicking exit) -- the parent's `wait4()`
    // emulation then reports that unrecognized host exit code to the guest shell as a bare
    // `Killed`, with zero further diagnostic. `GlobalStateHandle` carries its own, always-
    // freshly-constructed-per-process `memfds`/`shared_files` fields instead (below), shadowing
    // these (now removed) fields for every existing `self.global.memfds`/`self.global.
    // shared_files` call site with no further change -- same fix shape as `elf_patch_cache`
    // above, and the same accepted tradeoff: a memfd/shared-file mapping created by one process
    // in a cross-process-fork family is no longer visible to another member of that family (it
    // never safely was -- it panicked instead), rather than losing the WITHIN-one-process
    // dup()/thread-fork sharing `memfds`'s own doc comment used to describe, which is unaffected
    // since it never crossed a `GlobalStateHandle` instance to begin with.
    // NOTE: this struct deliberately has NO `proc_self_info`/`pts_registry` fields either, for
    // the SAME reason it has no `litebox` field above -- see `GlobalStateHandle`'s doc comment's
    // "Second instance of the SAME defect" section. `GlobalStateHandle` carries its OWN, always
    // per-process-correct copies (the same instances `LinuxShimBuilder::default_fs`/
    // `default_fs_multi_layer` already mounted the `/proc/self`/`/dev/pts` backends with).
}

impl<Platform: ShimPlatform, FS: ShimFS> GlobalStateHandle<Platform, FS> {
    /// Runs `f` with every shim-WIDE lock held, for the one caller that genuinely needs it:
    /// [`syscalls::process::Task::try_cross_process_fork`]'s native-`fork()` path (see
    /// [`litebox::platform::ForkChildVerificationProvider::native_fork`]'s doc comment).
    ///
    /// This is the shim-level half of the same discipline glibc's own `__libc_fork` uses
    /// internally before calling the kernel's `fork()`: a REAL `fork()` duplicates the calling
    /// thread only, so a lock some OTHER host thread happened to be holding at that instant is
    /// held forever in the child -- the thread that would have released it does not exist there.
    /// Acquiring every such lock first (which, for a `spin`-backed `RawMutex`, simply waits for
    /// whichever thread currently holds it to finish its critical section and release) guarantees
    /// none of them can be caught mid-hold at the instant `fork()` actually runs; `f` (the
    /// `fork()` call itself) then executes with the whole set quiesced, and every guard is
    /// dropped -- an ordinary, syscall-free unlock -- when this function returns, in BOTH the
    /// parent and, since `fork()` duplicates this stack frame verbatim, the child.
    ///
    /// **Scope, stated rather than left implicit.** This covers every lock that is genuinely
    /// SHIM-WIDE -- reachable from more than one guest process under this architecture's single
    /// shared address space, which is the one hazard a real `fork()` on genuine, independent-
    /// address-space Linux would never have (unrelated processes there share no locks at all).
    /// It deliberately does NOT reach into `litebox` (per-process `PageManager`/descriptor-table
    /// locks live under each `Arc<Process>`, not here), `futex_manager`, or `pipes`. A lock held
    /// by a SIBLING thread of the SAME forking guest process at fork time is the ordinary,
    /// general "`fork()` in a multithreaded program" hazard POSIX itself documents -- no worse
    /// than a real multithreaded guest program forking on real Linux already has to be written to
    /// tolerate, and not specific to litebox. Extending coverage into those structures is real,
    /// separate follow-on work, not silently claimed here.
    ///
    /// **Why this, and not the standard `pthread_atfork(prepare, parent, child)` registration**
    /// the glibc comparison above might suggest reaching for instead. `pthread_atfork` exists to
    /// decouple "who calls `fork()`" from "who needs to prepare" -- essential when `fork()` may
    /// be invoked by code you do not control (a library calling it on your behalf, or several
    /// independent call sites). Neither applies here: `native_fork` has exactly one caller in the
    /// entire codebase (`try_native_cross_process_fork`, always reaching it through this very
    /// function), so there is nothing to decouple. Registering real handlers instead would add a
    /// DOCUMENTED hazard for no offsetting benefit: POSIX's own `pthread_atfork` guidance warns
    /// that handlers "should not call library functions... This includes avoiding the use of any
    /// interfaces which may directly or indirectly attempt to allocate memory" specifically
    /// because other already-registered handlers (glibc's own malloc-arena ones, or a linked
    /// library's) can deadlock against a handler that allocates -- and acquiring any of this
    /// crate's own locks is not provably allocation-free. A plain closure sidesteps that whole
    /// hazard class: it participates in no global registration, runs only around this one call,
    /// and never interleaves with any other library's own atfork handlers' acquisition order.
    /// Real `fork()`'s OWN internal glibc/libc atfork handlers (malloc's included) still run
    /// normally, inside the `libc::fork()` call this wraps -- nothing here replaces those, only
    /// adds to them, narrowly, for the one thing this crate owns that they don't: its own
    /// shim-wide locks.
    fn with_shimwide_locks_held<R>(&self, f: impl FnOnce() -> R) -> R {
        let _net = self.net_lock();
        let _unix_addr_table = self.unix_addr_table.write();
        let _elf_patch_cache = self.elf_patch_cache.lock();
        let _segment_scan_cache = self.segment_scan_cache.lock();
        let _exec_ranges_cache = self.exec_ranges_cache.lock();
        let _sysv_shm = self.sysv_shm.lock();
        let _flock_registry = self.flock_registry.lock();
        let _pty_registry = self.pty_registry.write();
        let _daemon_pty_masters = self.daemon_pty_masters.write();
        let _memfds = self.memfds.lock();
        let _shared_files = self.shared_files.lock();
        let _proc_self_info = self.proc_self_info.write();
        f()
    }
}

struct Task<Platform: ShimPlatform, FS: ShimFS> {
    global: GlobalStateHandle<Platform, FS>,
    /// Unlike [`Self::pid`]/[`Self::thread`]/[`Self::signals`], this does NOT need to become
    /// replaceable for a native fork() child: it describes THIS HOST THREAD's own park/wake
    /// primitives, which a real `fork()` leaves completely unaffected -- only which GUEST
    /// PROCESS the thread belongs to changes, never its own interruptibility. The existing
    /// value stays exactly as correct for the child as it was for the parent.
    wait_state: wait::WaitState<Platform>,
    /// `RefCell` for the same reason as [`Self::pid`]: a native fork() child needs its own
    /// [`syscalls::process::Process`] (fresh children list, parent pointing at the process that
    /// forked it, its own adopted [`litebox::mm::PageManager`]) in place of the one it continues
    /// to hold immediately after `fork()` returns, which is still this SAME process's own.
    thread: RefCell<syscalls::process::ThreadState<Platform>>,
    /// Process ID. `Cell`, not a plain `i32`: a native `fork()` continues this exact host
    /// thread, at this exact `Task`'s fixed address, as the CHILD -- there is no other storage
    /// to construct a fresh identity into. See
    /// [`Task::reinit_as_native_fork_child`].
    pid: Cell<i32>,
    /// Parent Process ID. `Cell` for the same reason as [`Self::pid`].
    ppid: Cell<i32>,
    /// Thread ID. `Cell` for the same reason as [`Self::pid`].
    tid: Cell<i32>,
    /// Task credentials. These are set per task but are Arc'd to save space
    /// since most tasks never change their credentials.
    credentials: Arc<syscalls::process::Credentials>,
    /// Command name (usually the executable name, excluding the path)
    comm: Cell<[u8; litebox_common_linux::TASK_COMM_LEN]>,
    /// `PR_SET_DUMPABLE`/`PR_GET_DUMPABLE` state, per process.
    ///
    /// Tracked rather than refused because the pair is read-write and callers check what they set:
    /// glibc clears it after a privileged exec, gnupg and several session helpers set it
    /// deliberately, and a `prctl` that fails where real Linux always succeeds is a refusal they
    /// have no reason to expect. It changes nothing observable here -- litebox has no core dumps
    /// and no `ptrace` -- so honouring it means storing it faithfully and reading it back.
    ///
    /// `1` is Linux's own default (`SUID_DUMP_USER`).
    dumpable: Cell<u32>,
    /// Filesystem state. `RefCell` to support `unshare` in the future.
    fs: RefCell<Arc<syscalls::file::FsState<Platform>>>,
    /// File descriptors. `RefCell` to support `unshare` in the future.
    files: RefCell<Arc<syscalls::file::FilesState<Platform, FS>>>,
    /// Signal state. `RefCell` for the same reason as [`Self::pid`]: a native fork() child's
    /// pending signals must be cleared and its `shared_pending` repointed at the fresh child
    /// `Process`'s own queue (see [`syscalls::signal::SignalState::clone_for_new_task`], already
    /// used -- at ordinary Task-construction time, never in place -- by the thread-based fork
    /// path this one can't use).
    signals: RefCell<syscalls::signal::SignalState<Platform>>,
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

    impl<Platform: ShimPlatform, FS: ShimFS> GlobalStateHandle<Platform, FS> {
        /// Make a new task with default values for testing.
        pub(crate) fn new_test_task(self, fs: alloc::sync::Arc<FS>) -> Task<Platform, FS> {
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
                thread: RefCell::new(syscalls::process::ThreadState::new_process(
                    pid,
                    Arc::new(PageManager::new(&self.litebox)),
                    false,
                    None,
                    shared_pending.clone(),
                    None,
                )),
                pid: Cell::new(pid),
                ppid: Cell::new(0),
                tid: Cell::new(pid),
                credentials: Arc::new(syscalls::process::Credentials {
                    uid: 0,
                    euid: 0,
                    gid: 0,
                    egid: 0,
                }),
                comm: Cell::new(*b"test\0\0\0\0\0\0\0\0\0\0\0\0"),
                dumpable: Cell::new(1),
                fs: Arc::new(syscalls::file::FsState::new()).into(),
                files: files.into(),
                signals: RefCell::new(syscalls::signal::SignalState::new_process(shared_pending)),
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
                thread: RefCell::new(self.thread.borrow().new_thread(tid)?),
                pid: Cell::new(self.pid.get()),
                ppid: Cell::new(self.ppid.get()),
                tid: Cell::new(tid),
                credentials: self.credentials.clone(),
                comm: self.comm.clone(),
                dumpable: self.dumpable.clone(),
                fs: self.fs.clone(),
                files: self.files.clone(),
                // Always a same-process thread clone -- see `self.thread.borrow().new_thread(tid)` above.
                signals: RefCell::new(self.signals.borrow().clone_for_new_task(None)),
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
                Arc::new(PageManager::new(&self.global.litebox)),
                false,
                Some(Arc::downgrade(&self.process())),
                shared_pending.clone(),
                Some(litebox_common_linux::signal::Signal::SIGCHLD.as_i32()),
            );
            let child = Task {
                wait_state: wait::WaitState::new(self.global.platform),
                global: self.global.clone(),
                thread: RefCell::new(thread),
                pid: Cell::new(pid),
                ppid: Cell::new(self.pid.get()),
                tid: Cell::new(pid),
                credentials: self.credentials.clone(),
                comm: self.comm.clone(),
                dumpable: self.dumpable.clone(),
                fs: self.fs.clone(),
                files: self.files.clone(),
                signals: RefCell::new(
                    self.signals.borrow().clone_for_new_task(Some(shared_pending)),
                ),
                attached_pty_id: Cell::new(self.attached_pty_id.get()),
            };
            self.process().add_child_for_test(pid, child.process());
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
            self.thread.borrow()
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

/// Re-exported so the runner can forward `LITEBOX_COW_MMAP` into this `no_std` crate, mirroring
/// how `litebox::mm::linux::set_mapping_guard_gap_disabled` is forwarded for
/// `LITEBOX_NO_MAPPING_GUARD_GAP`. See `syscalls::mm::COW_MMAP_ENABLED` for why it defaults off.
pub use syscalls::mm::set_cow_mmap_enabled;
