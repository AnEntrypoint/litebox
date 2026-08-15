// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Diagnostic-only, standalone `CreateProcess`-based child-spawn primitive: pass 111 of
//! `scratchpad/jqrepro/FINDINGS.txt`'s investigation into an intermittent, guest-visible crash
//! that passes 107-110 root-caused to today's `fork()` running the child as a `std::thread`
//! inside the SAME host process as the parent, which cannot preserve the parent's own virtual
//! addresses for the child (they are already occupied by the parent's own live mappings).
//!
//! Pass 108 designed the real fix: give the child a genuine separate Windows process (a real
//! `CreateProcess`-spawned address space), then force-allocate each of the parent's reservation
//! groups at the SAME address in the child (empty except for its own freshly-loaded image) and
//! `WriteProcessMemory` the parent's actual bytes into it. Pass 109 empirically validated the
//! mechanism in a standalone throwaway program, against synthetic addresses, and found the one
//! sharp platform constraint that shapes it: `VirtualAlloc2`/`MEM_ADDRESS_REQUIREMENTS` can only
//! force a NEW reservation's base onto a 64KB (`dwAllocationGranularity`) boundary -- which is
//! exactly why pass 110 preserved each reservation GROUP's aligned base (not each individual,
//! frequently sub-granularity-offset guest VMA address) in `litebox::mm::AddressRelocations::
//! group_relocations`.
//!
//! This module is pass 111's first LIVE exercise of that mechanism: [`diagnostic_spawn_and_copy`]
//! spawns a real, `CREATE_SUSPENDED` child process re-executing this same litebox runner binary
//! (so the child has the litebox runtime linked in and ready, though it is never actually resumed
//! into it -- see below), forces an allocation at each of the caller's `group_relocations` bases,
//! `WriteProcessMemory`s the corresponding source bytes into it, and reports success/failure per
//! group. It is gated behind `LITEBOX_DIAG_PROCESS_FORK_SPAWN=1` and NEVER runs otherwise; even
//! when it runs, it is purely a side experiment that runs ALONGSIDE the real, unmodified,
//! thread-based `fork()` path in `do_clone` -- the guest's actual execution is unaffected. The
//! child is always `TerminateProcess`'d at the end, never resumed: resuming it into real guest
//! execution needs VEH re-registration, fd inheritance, and signal IPC (pass 108 SECTION 1, Q1/
//! Q3/Q4), none of which this pass builds. This module proves the memory-reservation-and-copy leg
//! of the design against REAL production guest memory layouts (not pass 109's synthetic
//! addresses) -- it does not yet make fork() itself faster, safer, or address-preserving.
//!
//! # The re-exec marker
//!
//! A `CreateProcess`-spawned child must be distinguishable from an ordinary fresh invocation of
//! the litebox runner (a guest binary path, e.g. `alpine-rootfs.tar`'s own `/usr/bin/python3`,
//! must never be misread as this marker). [`REEXEC_CHILD_ENV_VAR`] is an internal-only
//! environment variable litebox's own re-exec sets and checks for -- never guest-visible, never
//! part of any documented CLI surface. This pass's child never actually branches on it (it is
//! `CREATE_SUSPENDED` and torn down before it would run any litebox startup code that might), but
//! setting it now establishes the marker contract a future resuming implementation will rely on.

use core::ffi::c_void;
use std::ops::Range;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::Memory::{
    MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_EXTENDED_PARAMETER_0,
    MEM_EXTENDED_PARAMETER_1, MEM_RELEASE, MEM_RESERVE, MemExtendedParameterAddressRequirements,
    PAGE_READWRITE, VirtualFreeEx,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessW, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

/// Internal-only marker env var: set (never read as guest-visible) on a `CreateProcess`-spawned
/// litebox child so a future resuming implementation can distinguish "I am being set up as a
/// forked child" from "I am a normal fresh litebox invocation". Pass 114 is the first consumer:
/// the runner's own `main()` checks [`is_diagnostic_resume_child`] before clap-parsing argv (a
/// resume-diagnostic child is spawned with NO argv at all, so it must never reach the normal
/// `CliArgs::parse()` path, which requires a program path and `--initial-files`).
const REEXEC_CHILD_ENV_VAR: &str = "LITEBOX_INTERNAL_FORK_CHILD";

/// Marker line the resume-diagnostic child prints to its (pipe-redirected) stdout once it has
/// reached its own normal process initialization -- i.e. once `WindowsUserland::new()` has run
/// and registered its own VEH via `AddVectoredExceptionHandler`. The parent reads this line back
/// out of the pipe as the live proof the child's own startup completed without crashing despite
/// its address space having been pre-populated with the parent's guest bytes before the OS loader
/// and CRT/Rust runtime init ran. Never emitted by a normal litebox invocation (guarded by
/// [`is_diagnostic_resume_child`]), so it cannot be confused with ordinary guest output.
pub const RESUME_CHILD_READY_MARKER: &str = "LITEBOX_DIAG_RESUME_CHILD_READY";

/// Marker line the resume-diagnostic child prints, THROUGH a `DuplicateHandle`'d copy of the
/// parent's own `STD_OUTPUT_HANDLE` rather than its own inherited stdout pipe, when pass 115's
/// fd-inheritance probe ([`diag_process_fork_fds_enabled`]) is also enabled. Distinct from
/// [`RESUME_CHILD_READY_MARKER`] specifically so the parent can tell the two proofs apart: this
/// marker traveling through a duplicated HANDLE and successfully reaching whatever the parent's
/// own stdout is wired to (a real console, a pipe, `NUL`) is the live, end-to-end evidence that
/// `DuplicateHandle`-based fd inheritance works, not merely that the marker text exists somewhere.
pub const RESUME_CHILD_FD_MARKER: &str = "LITEBOX_DIAG_RESUME_CHILD_FD_OK";

/// Whether the CURRENT process is a `CreateProcess`-spawned diagnostic resume child (pass 114),
/// checked by the runner's `main()` before clap-parsing argv. `std::env::var` (not `var_os`) is
/// deliberate: the marker's value is meaningless, only presence matters, and this mirrors
/// [`diag_process_fork_spawn_enabled`]'s own presence-check style.
#[must_use]
pub fn is_diagnostic_resume_child() -> bool {
    std::env::var_os(REEXEC_CHILD_ENV_VAR).is_some()
}

/// Entry point the runner's `main()` calls instead of the normal `CliArgs::parse()` + `run()`
/// path when [`is_diagnostic_resume_child`] is true. Deliberately does NOT construct `CliArgs`,
/// load a guest tar, or touch `litebox_shim_linux` at all -- this pass's whole point is proving
/// the child survives its OWN process startup (loader, CRT, `WindowsUserland::new()`'s VEH
/// registration) with pre-populated foreign memory already sitting in its address space, not
/// resuming the ORIGINAL parent's guest execution (which needs fd inheritance and signal IPC that
/// do not exist yet -- pass 108 Q3/Q4, explicitly out of scope here).
pub fn run_diagnostic_resume_child() -> Result<(), std::convert::Infallible> {
    use std::io::Write as _;

    let _platform = litebox_platform_windows_userland_new_for_diagnostic_resume();
    println!("{RESUME_CHILD_READY_MARKER}");
    let _ = std::io::stdout().flush();

    // Pass 115's fd-inheritance probe: if the parent handed us (via our own inherited stdin pipe --
    // see `SuspendedChildGuard::stdin_write`'s doc comment for why a pipe carries this rather than
    // the environment/argv) a `DuplicateHandle`'d copy of ITS own `STD_OUTPUT_HANDLE`, write a
    // SEPARATE marker directly through that duplicated handle via a raw `WriteFile` -- proving,
    // from the CHILD's own side, that a HANDLE explicitly duplicated into this process at spawn
    // time is valid and immediately usable for real I/O, not merely present in the handle table.
    // This is deliberately independent of the `RESUME_CHILD_READY_MARKER` print above (which goes
    // through this child's own INHERITED stdout pipe, not a duplicated handle) so the two proofs
    // cannot be confused with one another. Silently does nothing if no line arrives quickly (the
    // pass-114-only resume path, fds probe not requested, never writes to this child's stdin at
    // all, so a blocking read would hang forever -- a short bounded read avoids that).
    if let Some(handle_value) = try_read_duplicated_handle_value_from_stdin() {
        write_marker_through_duplicated_handle(handle_value as HANDLE);
    }

    Ok(())
}

/// Reads one newline-terminated decimal line from this child's own inherited `STD_INPUT_HANDLE`,
/// bounded to a short timeout via a byte-at-a-time non-blocking-ish poll loop (this child has no
/// async I/O available at this point in startup) -- returns `None` if no line arrives in time
/// (the ordinary case when pass 115's fds probe was not requested, so the parent never writes
/// anything to this pipe) or if the line does not parse as a handle value.
fn try_read_duplicated_handle_value_from_stdin() -> Option<isize> {
    let stdin = unsafe {
        windows_sys::Win32::System::Console::GetStdHandle(
            windows_sys::Win32::System::Console::STD_INPUT_HANDLE,
        )
    };
    if stdin.is_null() || stdin == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return None;
    }
    // Bounded wait: peek for available bytes rather than a blocking ReadFile, since a normal
    // pass-114-only resume child has NO writer on this pipe at all and would hang forever
    // otherwise.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    let mut collected = Vec::new();
    loop {
        let mut avail = 0u32;
        let peek_ok = unsafe {
            windows_sys::Win32::System::Pipes::PeekNamedPipe(
                stdin,
                core::ptr::null_mut(),
                0,
                core::ptr::null_mut(),
                &raw mut avail,
                core::ptr::null_mut(),
            )
        };
        if peek_ok == 0 {
            // Not a pipe (e.g. a real console STD_INPUT_HANDLE, or the write end already closed
            // with nothing buffered) -- nothing to read.
            return None;
        }
        if avail > 0 {
            let mut buf = [0u8; 64];
            let mut read = 0u32;
            let read_ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::ReadFile(
                    stdin,
                    buf.as_mut_ptr(),
                    u32::try_from(buf.len().min(avail as usize))
                        .expect("bounded by fixed-size buf.len()"),
                    &raw mut read,
                    core::ptr::null_mut(),
                )
            };
            if read_ok == 0 {
                return None;
            }
            collected.extend_from_slice(&buf[..read as usize]);
            if collected.contains(&b'\n') {
                break;
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
    }
    let line = String::from_utf8_lossy(&collected);
    line.trim().parse::<isize>().ok()
}

/// Writes [`RESUME_CHILD_FD_MARKER`] (newline-terminated) directly via `WriteFile` on the given
/// HANDLE value -- interpreted as a raw Win32 `HANDLE` already valid IN THIS (child) process's own
/// handle table, exactly the shape `DuplicateHandle(..., dest_process = child, ...)` produces.
/// Never panics: a stale/invalid handle value simply fails the `WriteFile` call, which is reported
/// via `GetLastError` on stderr (the resume-diagnostic child's stderr is also pipe-captured by the
/// parent alongside stdout) rather than crashing the child.
fn write_marker_through_duplicated_handle(handle: HANDLE) {
    let msg = alloc_marker_bytes();
    let mut written = 0u32;
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::WriteFile(
            handle,
            msg.as_ptr(),
            u32::try_from(msg.len()).expect("small fixed marker fits in u32"),
            &raw mut written,
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        eprintln!(
            "[process_fork_diag] fd-probe (child): WriteFile through duplicated handle FAILED, GetLastError={}",
            unsafe { GetLastError() }
        );
    } else if written as usize != msg.len() {
        eprintln!(
            "[process_fork_diag] fd-probe (child): WriteFile through duplicated handle short write ({written}/{})",
            msg.len()
        );
    }
}

fn alloc_marker_bytes() -> Vec<u8> {
    let mut v = RESUME_CHILD_FD_MARKER.as_bytes().to_vec();
    v.push(b'\n');
    v
}

/// Thin indirection so this module (which the child's own `main()` calls into) does not need a
/// circular dependency on the `WindowsUserland` type it lives alongside in the same crate --
/// calls `WindowsUserland::new()` directly, which is the exact same per-process initialization
/// (VEH registration included) every normal litebox invocation already performs once.
fn litebox_platform_windows_userland_new_for_diagnostic_resume() -> &'static crate::WindowsUserland
{
    crate::WindowsUserland::new()
}

/// Whether the pass-111 `CreateProcess`-based fork diagnostic is enabled
/// (`LITEBOX_DIAG_PROCESS_FORK_SPAWN=1`). Never runs otherwise -- see this module's doc comment.
#[must_use]
pub fn diag_process_fork_spawn_enabled() -> bool {
    std::env::var_os("LITEBOX_DIAG_PROCESS_FORK_SPAWN").is_some()
}

/// Whether pass 114's further, riskier step -- actually `ResumeThread`-ing the diagnostic child
/// after its memory has been pre-populated, to see whether it survives its OWN process
/// initialization -- is enabled (`LITEBOX_DIAG_PROCESS_FORK_RESUME=1`). Deliberately a SEPARATE
/// gate from [`diag_process_fork_spawn_enabled`]: the memory-copy probe (pass 111/112) is fully
/// proven and safe (the child is never resumed), while actually resuming a real child process
/// into execution is this pass's own, narrower, higher-risk addition -- an operator/CI run can
/// opt into the proven memory-copy probe without also opting into the resume step, or vice versa
/// (though in practice this flag is only meaningful when the spawn flag is also set, since resume
/// happens after the same spawn+copy sequence). Only checked when
/// [`diag_process_fork_spawn_enabled`] is already true.
#[must_use]
pub fn diag_process_fork_resume_enabled() -> bool {
    std::env::var_os("LITEBOX_DIAG_PROCESS_FORK_RESUME").is_some()
}

/// Whether pass 115's fd/HANDLE-inheritance probe (Q3 of pass 108's design) is enabled
/// (`LITEBOX_DIAG_PROCESS_FORK_FDS=1`). A THIRD, separate gate from
/// [`diag_process_fork_spawn_enabled`]/[`diag_process_fork_resume_enabled`]: this probe needs the
/// resume step to already be live (there is no child to hand a duplicated HANDLE to otherwise), so
/// it is only meaningful -- and only checked -- when both of those are also set, but keeping it a
/// distinct flag lets an operator opt into the proven memory-copy-and-resume probes without also
/// opting into this pass's own, narrower fd-duplication addition.
///
/// # Scope (see this module's and `duplicate_stdio_into_child`'s doc comments for the full
/// investigation)
///
/// Reading `litebox::fd::RawDescriptorStorage`/`fork_duplicate` and every one of the 7 fd
/// subsystems it dispatches to (FS, Network, Pipes, Eventfd, Epoll, UnixSocket, Pty) found that
/// **none** of litebox's guest file descriptors are directly backed by a real Windows `HANDLE`:
/// every subsystem's `Entry` type is pure in-process Rust state (mutexes, atomics, ring buffers,
/// `smoltcp` virtual sockets, `BTreeMap`s) with no `OwnedHandle`/raw `HANDLE` field anywhere,
/// transitively. `OwnedFd` itself (`litebox/src/fd/mod.rs`) is just `{ raw: u32, closed:
/// AtomicBool }` -- an index into `Descriptors`' own in-process `Vec<Option<IndividualEntry<_>>>`
/// table, not a HANDLE wrapper. This means pass 108's Q3 proposal ("explicit per-fd
/// `DuplicateHandle` calls, building on `fork_duplicate`'s already-fork-aware per-subsystem
/// duplication design") does not apply to guest fds as originally framed -- there is no HANDLE
/// backing a guest fd for `DuplicateHandle` to duplicate; a real cross-process fork would instead
/// need to re-establish each subsystem's in-process object graph in the child some other way
/// (shared memory, a real IPC channel, or literally proxying reads/writes back to the parent),
/// which is a materially different, and NOT yet designed, mechanism -- named explicitly as the
/// genuine limitation this pass found, not glossed over.
///
/// The ONE class of real Windows `HANDLE` this investigation found anywhere near the guest process
/// boundary is the host's own `STD_OUTPUT_HANDLE`/`STD_ERROR_HANDLE`/`STD_INPUT_HANDLE` -- used
/// directly via `GetStdHandle`/`WriteFile`/`ReadFile` in `lib.rs`'s `write_stdio`/console-reader
/// paths, entirely OUTSIDE the guest fd-table abstraction (it is how litebox's OWN platform layer
/// talks to the real console/pipe the litebox process itself was launched with, not a per-guest-fd
/// mechanism). This probe scopes itself to exactly that straightforward, genuinely HANDLE-backed
/// case: it duplicates the CURRENT process's real `STD_OUTPUT_HANDLE` into the diagnostic resume
/// child and verifies, from the child's own side, that the duplicated handle is valid and usable
/// for real I/O -- proving the `DuplicateHandle` mechanism itself works end-to-end for the one
/// concrete HANDLE-backed case that exists, while documenting precisely why it does NOT generalize
/// to guest fds without further, materially different design work.
#[must_use]
pub fn diag_process_fork_fds_enabled() -> bool {
    std::env::var_os("LITEBOX_DIAG_PROCESS_FORK_FDS").is_some()
}

/// Per-group outcome reported by [`diagnostic_spawn_and_copy`].
pub struct GroupCopyResult {
    /// The group's span in the (real, live) parent address space, exactly as `Vmem::duplicate`
    /// recorded it.
    pub source_group: Range<usize>,
    /// Whether reserving this exact span at this exact address in the fresh child process, then
    /// `WriteProcessMemory`-ing the parent's real bytes into it, succeeded.
    pub succeeded: bool,
    /// `GetLastError()` from whichever Win32 call failed, if `succeeded` is `false`.
    pub last_error: u32,
}

/// RAII guard for the diagnostic child process's handles: guarantees `TerminateProcess` +
/// `CloseHandle` run on every exit path (including an early `?`-propagated failure partway
/// through the per-group loop), matching this pass's "never resume the child" invariant even
/// when something goes wrong before the loop finishes.
struct SuspendedChildGuard {
    process: HANDLE,
    thread: HANDLE,
    /// Read end of the child's stdout pipe, present whenever pass 114's resume step might read
    /// the child's readiness marker back. `None` keeps the pre-pass-114 no-pipe shape when the
    /// resume gate is off, so the memory-copy-only probe stays byte-identical to pass 111/112.
    stdout_read: Option<HANDLE>,
    /// Write end of a SEPARATE inheritable pipe wired to the child's stdin, present only when
    /// pass 115's fd-inheritance probe is active. Used to hand the child the (child-process-
    /// relative) `HANDLE` VALUE that `duplicate_stdio_into_child` produced -- there is no argv (a
    /// resume-diagnostic child is spawned with none) and the environment block is fixed at
    /// `CreateProcessW` time, before the child process (and hence a valid `DuplicateHandle` target)
    /// exists, so this pipe is the simplest correct way to deliver a value computed AFTER spawn.
    stdin_write: Option<HANDLE>,
}

impl Drop for SuspendedChildGuard {
    fn drop(&mut self) {
        // Best-effort, unconditional cleanup regardless of whether the child was ever resumed:
        // TerminateProcess is a no-op-equivalent for a process that already exited on its own
        // (pass 114's resumed child exits cleanly after printing its marker), and is the correct,
        // safe teardown for a child still suspended or still running (pass 111/112's untouched
        // memory-copy-only path, and pass 114's resume path if the child hangs).
        unsafe {
            TerminateProcess(self.process, 0);
            if let Some(read) = self.stdout_read {
                CloseHandle(read);
            }
            if let Some(write) = self.stdin_write {
                CloseHandle(write);
            }
            CloseHandle(self.thread);
            CloseHandle(self.process);
        }
    }
}

/// Spawns a real, `CREATE_SUSPENDED` Windows child process re-executing the current litebox
/// runner binary, then for each `(source_group, _)` pair in `group_relocations` (as produced by
/// `litebox::mm::AddressRelocations::group_relocations`, i.e. pass 110's per-reservation-group
/// aligned-base bookkeeping) attempts to:
///
///  1. Force-reserve+commit `source_group`'s exact span at the SAME address in the child, via
///     `VirtualAllocEx` + `MEM_ADDRESS_REQUIREMENTS` (a hard requirement, not a hint -- see pass
///     109's finding that a plain hint can silently round to the wrong address with no error).
///  2. `WriteProcessMemory` `read_source_bytes(source_group)`'s bytes into that reservation.
///
/// The child is `TerminateProcess`'d (never resumed) when this function returns, regardless of
/// outcome -- see this module's doc comment for why resuming it is out of scope for this pass.
///
/// `read_source_bytes` reads the CALLER's (parent's) own live memory at the given range -- the
/// caller supplies it rather than this function reaching into `litebox::platform::RawConstPointer`
/// directly, keeping this platform-crate module free of a dependency on `litebox`'s guest-memory
/// abstraction it does not otherwise need.
///
/// Returns one [`GroupCopyResult`] per input group, in the same order, even if some fail -- a
/// failure on one group does not abort the rest, so the diagnostic reports the FULL picture for
/// this fork() call's actual guest memory layout, not just the first failure.
pub fn diagnostic_spawn_and_copy(
    group_relocations: &[(Range<usize>, usize)],
    mut read_source_bytes: impl FnMut(Range<usize>) -> Option<Vec<u8>>,
) -> Result<Vec<GroupCopyResult>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe() failed: {e}"))?;
    let mut exe_wide: Vec<u16> = exe
        .as_os_str()
        .encode_wide_for_windows()
        .chain(std::iter::once(0))
        .collect();

    // Mark the child, for a future resuming implementation's benefit -- see
    // `REEXEC_CHILD_ENV_VAR`'s doc comment. `CreateProcessW`'s `lpEnvironment: null` means
    // "inherit the parent's environment block", so setting this on the CURRENT process's
    // environment before spawning (and clearing it right after, since this diagnostic's own
    // process must not appear to be a fork child to anything else checking this var) is the
    // simplest correct way to propagate it without hand-building a full environment block.
    // Safety: `std::env::set_var`/`remove_var` are documented as not thread-safe against
    // concurrent reads of the environment on this platform; `do_clone`'s caller already holds
    // whatever process-wide serialization real fork() requires (see `Vmem::duplicate`'s own
    // safety contract: "no other code is concurrently mutating memory ... other threads must be
    // stopped"), so this diagnostic, invoked from exactly that same call site, inherits the same
    // guarantee.
    let want_resume = diag_process_fork_resume_enabled();
    let want_fds = want_resume && diag_process_fork_fds_enabled();
    unsafe {
        std::env::set_var(REEXEC_CHILD_ENV_VAR, "1");
    }
    let spawn_result = spawn_suspended(&mut exe_wide, want_resume, want_fds);
    unsafe {
        std::env::remove_var(REEXEC_CHILD_ENV_VAR);
    }
    let (process, thread, stdout_read, stdin_write) = spawn_result?;
    let guard = SuspendedChildGuard {
        process,
        thread,
        stdout_read,
        stdin_write,
    };

    let mut results = Vec::with_capacity(group_relocations.len());
    for (source_group, dest_base) in group_relocations {
        results.push(copy_one_group(
            guard.process,
            source_group,
            *dest_base,
            &mut read_source_bytes,
        ));
    }

    if want_fds {
        duplicate_stdio_into_child(&guard);
    }

    if want_resume {
        resume_and_observe(&guard, want_fds);
    }

    // `guard` drops here: TerminateProcess + CloseHandle, unconditionally, whether or not it was
    // ever resumed -- see the guard's own doc comment.
    Ok(results)
}

/// Pass 115's fd-inheritance probe: `DuplicateHandle`s the CURRENT (parent) process's real
/// `STD_OUTPUT_HANDLE` into `guard.process` (still suspended at this point -- called before
/// [`resume_and_observe`]'s `ResumeThread`), then writes the resulting child-process-relative
/// `HANDLE` value as a decimal line through `guard.stdin_write` so the child can read it back and
/// use it once resumed (see [`SuspendedChildGuard::stdin_write`]'s doc comment for why a pipe,
/// not argv/env, carries this post-spawn-computed value). A `DuplicateHandle` failure is reported
/// but not fatal to the rest of the diagnostic -- the memory-copy-and-resume probes above already
/// completed and their own results stand regardless of whether this additional fd probe succeeds.
fn duplicate_stdio_into_child(guard: &SuspendedChildGuard) {
    use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let Some(stdin_write) = guard.stdin_write else {
        eprintln!(
            "[process_fork_diag] fd-probe: no stdin pipe available, cannot hand child a duplicated handle"
        );
        return;
    };

    let source_stdout = unsafe {
        windows_sys::Win32::System::Console::GetStdHandle(
            windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE,
        )
    };
    if source_stdout.is_null()
        || source_stdout == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
    {
        eprintln!(
            "[process_fork_diag] fd-probe: GetStdHandle(STD_OUTPUT_HANDLE) returned no real handle \
             (GetLastError={}) -- skipping (e.g. no console/redirected-to-nothing environment)",
            unsafe { GetLastError() }
        );
        return;
    }

    let mut dest_handle: HANDLE = core::ptr::null_mut();
    let ok = unsafe {
        windows_sys::Win32::Foundation::DuplicateHandle(
            GetCurrentProcess(),
            source_stdout,
            guard.process,
            &raw mut dest_handle,
            0,
            0, // bInheritHandle: irrelevant here, we hand the value over explicitly via the pipe
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        eprintln!(
            "[process_fork_diag] fd-probe: DuplicateHandle(STD_OUTPUT_HANDLE -> child) FAILED, GetLastError={}",
            unsafe { GetLastError() }
        );
        return;
    }
    eprintln!(
        "[process_fork_diag] fd-probe: DuplicateHandle(STD_OUTPUT_HANDLE -> child) succeeded, \
         child-relative handle value={dest_handle:#x?}"
    );

    let line = format!("{}\n", dest_handle as isize);
    let mut written = 0u32;
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::WriteFile(
            stdin_write,
            line.as_ptr(),
            u32::try_from(line.len()).expect("small line fits in u32"),
            &raw mut written,
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        eprintln!(
            "[process_fork_diag] fd-probe: WriteFile(child stdin, handle value) FAILED, GetLastError={}",
            unsafe { GetLastError() }
        );
    }
}

/// Pass 114's own step, gated entirely behind [`diag_process_fork_resume_enabled`]: resumes the
/// child's main thread (its memory already pre-populated with the parent's guest bytes at the
/// SAME addresses by the per-group copy loop above) and checks whether it reaches its own
/// [`RESUME_CHILD_READY_MARKER`] -- i.e. whether it survives its own process initialization
/// (loader, CRT, Rust runtime init, `WindowsUserland::new()`'s VEH registration) despite that
/// foreign memory already sitting in its address space before any of that init ran. Bounded wait
/// (2 seconds): if the marker never arrives, this is reported as a hang/crash, not left to block
/// indefinitely -- the caller's `guard` unconditionally terminates the child immediately
/// afterward regardless of the outcome either way.
fn resume_and_observe(guard: &SuspendedChildGuard, want_fds: bool) {
    const TIMEOUT_MS: u32 = 2000;

    let resumed = unsafe { ResumeThread(guard.thread) };
    if resumed == u32::MAX {
        eprintln!(
            "[process_fork_diag] resume: ResumeThread failed, GetLastError={}",
            unsafe { GetLastError() }
        );
        return;
    }

    let Some(stdout_read) = guard.stdout_read else {
        eprintln!("[process_fork_diag] resume: no stdout pipe available, cannot observe marker");
        return;
    };

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(u64::from(TIMEOUT_MS));
    let mut collected = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "[process_fork_diag] resume: TIMED OUT waiting for child readiness marker after {TIMEOUT_MS}ms \
                 (collected so far: {:?})",
                String::from_utf8_lossy(&collected)
            );
            return;
        }
        let mut read = 0u32;
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::ReadFile(
                stdout_read,
                buf.as_mut_ptr(),
                u32::try_from(buf.len()).expect("small fixed-size buffer fits in u32"),
                &raw mut read,
                core::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            // ERROR_BROKEN_PIPE (109): child exited (closed its stdout) -- expected exit path for
            // a well-behaved resume child that printed its marker and returned from main().
            if err == 109 {
                break;
            }
            eprintln!(
                "[process_fork_diag] resume: ReadFile on child stdout failed, GetLastError={err}"
            );
            return;
        }
        if read == 0 {
            break;
        }
        collected.extend_from_slice(&buf[..read as usize]);
        if collected
            .windows(RESUME_CHILD_READY_MARKER.len())
            .any(|w| w == RESUME_CHILD_READY_MARKER.as_bytes())
        {
            break;
        }
    }

    let saw_marker = collected
        .windows(RESUME_CHILD_READY_MARKER.len())
        .any(|w| w == RESUME_CHILD_READY_MARKER.as_bytes());

    if saw_marker {
        eprintln!(
            "[process_fork_diag] resume: child reached its own startup successfully (marker observed) \
             -- pre-populated foreign memory did NOT interfere with child process init"
        );
        if want_fds {
            // The fd-probe marker (`RESUME_CHILD_FD_MARKER`) was written by the child through a
            // `DuplicateHandle`'d copy of the PARENT's own `STD_OUTPUT_HANDLE` -- i.e. it travels
            // to wherever the PARENT's real stdout is wired to (this diagnostic process's own
            // console/pipe/NUL), NOT through the pass-114 stdout-capture pipe this function is
            // reading from. There is nothing further to read here; the live proof is that the
            // marker text appears on THIS process's own real stdout stream (verified externally by
            // the operator/CI capturing this process's stdout, exactly as pass 115's FINDINGS.txt
            // entry documents doing for its live verification runs).
            eprintln!(
                "[process_fork_diag] fd-probe: duplicated-HANDLE marker was requested -- check this \
                 process's own real stdout (not this diagnostic's stderr) for {RESUME_CHILD_FD_MARKER:?}"
            );
        }
    } else {
        // Give the process a brief moment to actually finish exiting/crashing so the exit code is
        // meaningful, then report it -- a crash inside the child's own init (loader collision,
        // CRT init failure, VEH registration failure) is exactly what this probe exists to catch.
        let wait_result = unsafe { WaitForSingleObject(guard.process, 500) };
        eprintln!(
            "[process_fork_diag] resume: child did NOT print its readiness marker \
             (collected stdout: {:?}, WaitForSingleObject={wait_result})",
            String::from_utf8_lossy(&collected)
        );
    }
}

/// Trait-free helper: `OsStr::encode_wide` is already available via
/// `std::os::windows::ffi::OsStrExt`, imported locally to keep this module's top-level `use`
/// block Windows-specific-import-free at a glance (this whole crate is Windows-only per its
/// crate-level `#![cfg(...)]`, but keeping the platform-specific extension trait import scoped to
/// its one call site matches this module's own narrow, diagnostic-only footprint).
trait EncodeWideExt {
    fn encode_wide_for_windows(&self) -> std::os::windows::ffi::EncodeWide<'_>;
}
impl EncodeWideExt for std::ffi::OsStr {
    fn encode_wide_for_windows(&self) -> std::os::windows::ffi::EncodeWide<'_> {
        use std::os::windows::ffi::OsStrExt as _;
        self.encode_wide()
    }
}

/// Spawns the `CREATE_SUSPENDED` diagnostic child. When `want_stdout_pipe` is true (pass 114's
/// resume step only -- the pass 111/112 memory-copy-only probe never sets this), also creates an
/// inheritable stdout pipe and wires it into the child via `STARTF_USESTDHANDLES`, returning the
/// PARENT's own read end so [`resume_and_observe`] can read the child's readiness marker back.
/// When `want_stdin_pipe` is ALSO true (pass 115's fd-inheritance probe only), additionally creates
/// a second inheritable pipe wired to the child's stdin, returning the PARENT's own write end so
/// [`duplicate_stdio_into_child`] can hand the child the duplicated-handle value it needs (see
/// [`SuspendedChildGuard::stdin_write`]'s doc comment for why a pipe, not the environment/argv, is
/// used for this).
fn spawn_suspended(
    exe_wide: &mut [u16],
    want_stdout_pipe: bool,
    want_stdin_pipe: bool,
) -> Result<(HANDLE, HANDLE, Option<HANDLE>, Option<HANDLE>), String> {
    let mut startup_info: STARTUPINFOW = unsafe { core::mem::zeroed() };
    startup_info.cb =
        u32::try_from(core::mem::size_of::<STARTUPINFOW>()).expect("STARTUPINFOW fits in u32");
    let mut process_info: PROCESS_INFORMATION = unsafe { core::mem::zeroed() };

    let mut stdout_read: HANDLE = core::ptr::null_mut();
    let mut stdin_write: HANDLE = core::ptr::null_mut();
    let mut inherit_handles = 0i32;
    if want_stdout_pipe {
        let mut sec_attrs: SECURITY_ATTRIBUTES = unsafe { core::mem::zeroed() };
        sec_attrs.nLength =
            u32::try_from(core::mem::size_of::<SECURITY_ATTRIBUTES>()).expect("fits in u32");
        sec_attrs.bInheritHandle = 1;
        let mut stdout_write: HANDLE = core::ptr::null_mut();
        let pipe_ok = unsafe {
            CreatePipe(
                &raw mut stdout_read,
                &raw mut stdout_write,
                &raw const sec_attrs,
                0,
            )
        };
        if pipe_ok == 0 {
            return Err(format!("CreatePipe failed: GetLastError={}", unsafe {
                GetLastError()
            }));
        }
        // Ensure the PARENT's own read end is never inherited by the child (it must only ever
        // hold the write end) -- a leaked read-end handle in the child would keep the pipe open
        // even after the child's own write end closes, defeating the ERROR_BROKEN_PIPE-on-exit
        // signal `resume_and_observe` relies on to detect the child exiting.
        unsafe {
            windows_sys::Win32::Foundation::SetHandleInformation(stdout_read, 1, 0);
        }
        startup_info.dwFlags |= STARTF_USESTDHANDLES;
        startup_info.hStdOutput = stdout_write;
        startup_info.hStdError = stdout_write;
        inherit_handles = 1;

        if want_stdin_pipe {
            let mut stdin_read: HANDLE = core::ptr::null_mut();
            let pipe_ok = unsafe {
                CreatePipe(
                    &raw mut stdin_read,
                    &raw mut stdin_write,
                    &raw const sec_attrs,
                    0,
                )
            };
            if pipe_ok == 0 {
                let err = unsafe { GetLastError() };
                unsafe {
                    CloseHandle(stdout_read);
                    CloseHandle(stdout_write);
                }
                return Err(format!("CreatePipe (stdin) failed: GetLastError={err}"));
            }
            // Symmetric to the stdout read end above: the PARENT's own write end must never be
            // inherited by the child.
            unsafe {
                windows_sys::Win32::Foundation::SetHandleInformation(stdin_write, 1, 0);
            }
            startup_info.hStdInput = stdin_read;
        }
    }

    let ok = unsafe {
        CreateProcessW(
            core::ptr::null(),
            exe_wide.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            inherit_handles,
            CREATE_SUSPENDED,
            core::ptr::null(),
            core::ptr::null(),
            &raw const startup_info,
            &raw mut process_info,
        )
    };
    // The child's own inherited copy of the write/read handles keeps them open in the child; the
    // parent must close ITS copies regardless of CreateProcessW's outcome so each pipe only stays
    // open via the child's handle (needed for ERROR_BROKEN_PIPE to fire correctly once the child
    // exits, and to avoid leaking the parent's copy of the child's stdin read end either way).
    if want_stdout_pipe && !startup_info.hStdOutput.is_null() {
        unsafe {
            CloseHandle(startup_info.hStdOutput);
        }
    }
    if want_stdin_pipe && !startup_info.hStdInput.is_null() {
        unsafe {
            CloseHandle(startup_info.hStdInput);
        }
    }
    if ok == 0 {
        if !stdout_read.is_null() {
            unsafe {
                CloseHandle(stdout_read);
            }
        }
        if !stdin_write.is_null() {
            unsafe {
                CloseHandle(stdin_write);
            }
        }
        return Err(format!(
            "CreateProcessW(CREATE_SUSPENDED) failed: GetLastError={}",
            unsafe { GetLastError() }
        ));
    }
    Ok((
        process_info.hProcess,
        process_info.hThread,
        if want_stdout_pipe {
            Some(stdout_read)
        } else {
            None
        },
        if want_stdin_pipe {
            Some(stdin_write)
        } else {
            None
        },
    ))
}

/// Attempts steps 1-2 of [`diagnostic_spawn_and_copy`]'s doc comment for a single reservation
/// group. Never panics on failure -- a failed group is reported in the returned
/// [`GroupCopyResult`], not propagated as an error, so the caller sees every group's outcome.
fn copy_one_group(
    child: HANDLE,
    source_group: &Range<usize>,
    _dest_base: usize,
    read_source_bytes: &mut impl FnMut(Range<usize>) -> Option<Vec<u8>>,
) -> GroupCopyResult {
    const PAGE_SIZE: usize = 4096;
    let len = source_group.len();
    let fail = |err: u32| GroupCopyResult {
        source_group: source_group.clone(),
        succeeded: false,
        last_error: err,
    };

    // Step 1: force-reserve+commit the group's exact span, at its exact SOURCE address, in the
    // child. `MEM_ADDRESS_REQUIREMENTS` makes this a hard requirement -- per pass 109, either it
    // lands EXACTLY here or the call fails outright (ERROR_INVALID_PARAMETER if the address isn't
    // 64KB-aligned, which a reservation-GROUP base per pass 110's own bookkeeping always is;
    // otherwise typically ERROR_INVALID_ADDRESS if something in the child's own fresh image/DLL
    // layout already occupies it -- exactly the collision class this diagnostic exists to check
    // for against REAL, not synthetic, guest addresses).
    let mut addr_req = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: source_group.start as *mut c_void,
        HighestEndingAddress: (source_group.end - 1) as *mut c_void,
        Alignment: 0,
    };
    let mut ext_param = MEM_EXTENDED_PARAMETER {
        Anonymous1: MEM_EXTENDED_PARAMETER_0 {
            _bitfield: MemExtendedParameterAddressRequirements as u64,
        },
        Anonymous2: MEM_EXTENDED_PARAMETER_1 {
            Pointer: (&raw mut addr_req).cast::<c_void>(),
        },
    };
    // `VirtualAllocEx` is the cross-process sibling of `VirtualAlloc2` used elsewhere in this
    // crate (see `lib.rs`'s `reserve_and_commit`); `windows-sys` 0.60.2's `VirtualAllocEx`
    // signature does not itself take extended parameters, so route through the raw
    // `VirtualAlloc2` entry point with an explicit process handle instead of the no-handle
    // `VirtualAlloc2`-via-current-process helper `lib.rs` uses -- both are the same underlying
    // Win32 API, `VirtualAlloc2`, just called with a non-current-process handle here.
    let reserved = unsafe {
        windows_sys::Win32::System::Memory::VirtualAlloc2(
            child,
            core::ptr::null_mut(),
            len,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_READWRITE,
            &raw mut ext_param,
            1,
        )
    };
    if reserved.is_null() {
        return fail(unsafe { GetLastError() });
    }
    if reserved as usize != source_group.start {
        // Should be unreachable given `MEM_ADDRESS_REQUIREMENTS` is a hard requirement (pass 109
        // observed silent rounding ONLY under the plain-hint API, never under this one) -- but
        // treat a mismatch as a failure rather than trusting the assumption blindly, since this
        // is exactly the class of bug ("silently landed at the wrong address") the whole design
        // exists to eliminate.
        unsafe {
            VirtualFreeEx(child, reserved, 0, MEM_RELEASE);
        }
        return fail(0);
    }

    // Step 2: copy the parent's real, live bytes for this group into the child's newly reserved
    // span at the identical address.
    //
    // `source_group` here is `GroupRelocation::source_group` (see its doc comment in
    // `litebox::mm::linux`), rounded OUT to Windows allocation granularity so `VirtualAllocEx`
    // above can force-reserve it -- it may therefore extend beyond the actual mapped guest content
    // (padding pages the real, page-granularity `Vmem::duplicate` copy loop never touches and that
    // are genuinely unmapped/unreadable in the parent). A single whole-span
    // `read_source_bytes(source_group)` would fail outright the moment ANY page in that padding is
    // unreadable, even though every REAL content page copied fine -- read page-by-page instead so
    // an unreadable padding page is simply left as the zero-fill `VirtualAlloc2`/`MEM_COMMIT`
    // already guarantees for it, matching what a real fork() would produce there (nothing, since
    // no guest mapping exists in that padding either).
    let mut cursor = source_group.start;
    while cursor < source_group.end {
        let page_end = (cursor + PAGE_SIZE).min(source_group.end);
        let page_range = cursor..page_end;
        let Some(bytes) = read_source_bytes(page_range.clone()) else {
            // Unreadable page: leave it as the child's already-zero-filled MEM_COMMIT content
            // (real guest padding pages are unmapped too, so this matches production behavior).
            cursor = page_end;
            continue;
        };
        debug_assert_eq!(
            bytes.len(),
            page_range.len(),
            "read_source_bytes returned wrong length"
        );
        let mut written = 0usize;
        let ok = unsafe {
            WriteProcessMemory(
                child,
                (reserved as usize + (cursor - source_group.start)) as *mut c_void,
                bytes.as_ptr().cast::<c_void>(),
                bytes.len(),
                &raw mut written,
            )
        };
        if ok == 0 || written != bytes.len() {
            return fail(unsafe { GetLastError() });
        }
        cursor = page_end;
    }

    GroupCopyResult {
        source_group: source_group.clone(),
        succeeded: true,
        last_error: 0,
    }
}
