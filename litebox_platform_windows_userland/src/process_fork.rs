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

/// Internal-only marker env var (pass 120), set alongside [`REEXEC_CHILD_ENV_VAR`] only when the
/// real-guest-resume probe (`LITEBOX_DIAG_PROCESS_FORK_REAL_RESUME=1`) is active for this `fork()`
/// call. Distinguishes "park after my own init, waiting to be injected with a real guest context"
/// from every other resume-diagnostic child shape (pass 114's plain survive-startup child, and pass
/// 118/119's never-run-thread minimal-stub-injection child, neither of which ever reaches
/// [`run_diagnostic_resume_child`]'s park point).
const REAL_RESUME_CHILD_ENV_VAR: &str = "LITEBOX_INTERNAL_FORK_CHILD_REAL_RESUME";

/// Whether the CURRENT process is a pass-120 real-guest-resume diagnostic child -- see
/// [`REAL_RESUME_CHILD_ENV_VAR`]'s doc comment.
#[must_use]
fn is_real_resume_child() -> bool {
    std::env::var_os(REAL_RESUME_CHILD_ENV_VAR).is_some()
}

/// Parks the current (child) thread in a bounded kernel wait so the parent can safely
/// `SuspendThread` + `SetThreadContext` it -- see [`run_diagnostic_resume_child`]'s call site doc
/// comment for why this is safe (the thread has already run, unlike pass 118/119's never-run-thread
/// target) and necessary (the parent cannot inject into a thread that is not suspended). Bounded to
/// 10 seconds so a child that is somehow never injected into (e.g. the parent crashed, or the probe
/// is disabled after all) still exits instead of hanging forever; the parent unconditionally
/// `TerminateProcess`'s this child on drop regardless, so this bound is a courtesy, not a
/// correctness requirement.
fn park_for_real_resume_injection() {
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    let event = unsafe { CreateEventW(core::ptr::null(), 1, 0, core::ptr::null()) };
    if event.is_null() {
        return;
    }
    unsafe {
        WaitForSingleObject(event, 10_000);
    }
}

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

    // Pass 120: if the parent requested the real-guest-resume probe, park THIS thread (via a
    // blocking wait on an event the parent never signals) immediately after `WindowsUserland::new()`
    // completes -- i.e. after VEH registration, matching pass 114's proven-safe "child survives its
    // own startup" shape -- so the parent can `SuspendThread` it (safe: this thread is blocked in a
    // kernel wait, not mid-instruction) and inject the REAL translated guest context via
    // `SetThreadContext`. Unlike pass 118/119's probe (which targets a NEVER-RUN thread, unconditionally
    // hitting ntdll's loader-init thunk first), this thread HAS already run -- it reached this wait via
    // ordinary execution -- so the loader-thunk hazard pass 119 found does not apply here; this is
    // the same "resume an already-run thread via SetThreadContext" shape `ThreadHandle::interrupt`'s
    // own already-proven-working cross-thread injection uses. Never blocks in the pass-114-only or
    // pass-118/119-only resume paths (no writer ever signals this env var there).
    if is_real_resume_child() {
        park_for_real_resume_injection();
    }

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

/// Whether pass 118's cross-process register-injection probe is enabled
/// (`LITEBOX_DIAG_PROCESS_FORK_REGISTERS=1`). A FOURTH, separate gate layered on top of
/// [`diag_process_fork_spawn_enabled`]/[`diag_process_fork_resume_enabled`]: this probe replaces
/// pass 114/115's "resume into the child's own normal entry point" step with "inject a translated
/// register context via `SetThreadContext`, pointing `Rip` at a tiny diagnostic stub THIS module
/// writes into the child's memory, and resume into THAT instead" -- so it only makes sense, and is
/// only checked, when the resume gate is also set (there is no suspended thread to inject into
/// otherwise). Kept independent so an operator can opt into the proven spawn+copy+resume-into-own-
/// startup probes without also opting into this pass's own, narrower, purpose-built register-
/// injection addition.
///
/// # Scope (pass 118 of `scratchpad/jqrepro/FINDINGS.txt`)
///
/// This probe deliberately does NOT attempt to resume the real guest's actual next instruction via
/// the full `switch_to_guest` machinery -- pass 117 measured that gap and found it needs cross-
/// process `Task` reconstruction (fs/files/signals/process-tree state) and process-local runtime
/// state (VEH, TLS, `fork_verify`) neither of which exist for a `CreateProcess`-spawned child yet.
/// Instead this probe answers ONE narrow, previously-completely-untested question: does
/// `SetThreadContext`, targeting a cross-process `CREATE_SUSPENDED` thread that has NEVER run any
/// instruction yet, with a translated register context whose `Rip` points into memory this module
/// itself wrote via `WriteProcessMemory` (the same shape the real `switch_to_guest`-based injection
/// would eventually need), actually take effect -- i.e. does the CPU start executing AT the injected
/// context, not at the loader's normal entry point? The injected stub is a handful of bytes THIS
/// module controls (not real guest code), ending in a marker write to its own memory followed by
/// `NtTerminateProcess` with a distinctive exit code (see `inject_and_observe`'s doc comment for
/// why NOT a stdout-pipe marker, unlike this module's other probes -- kernel32.dll, which
/// `WriteFile` needs, is not even loaded yet in a `CREATE_SUSPENDED` child that has never run its
/// own first instruction) -- it never touches `switch_to_guest`, `Task`, `WindowsUserland::new()`,
/// or any of the missing subsystems pass 117 identified.
#[must_use]
pub fn diag_process_fork_registers_enabled() -> bool {
    std::env::var_os("LITEBOX_DIAG_PROCESS_FORK_REGISTERS").is_some()
}

/// Whether pass 120's real-guest-resume probe is enabled (`LITEBOX_DIAG_PROCESS_FORK_REAL_RESUME=1`).
/// A FIFTH gate, layered on top of SPAWN/RESUME/REGISTERS: unlike pass 118/119's probe (which
/// injects a tiny, purpose-built diagnostic stub -- never real guest code -- into a fresh
/// `CREATE_SUSPENDED` child and never lets the child reach its own `main()`), this probe lets the
/// child run its OWN normal `run_diagnostic_resume_child()` init first (so `WindowsUserland::new()`
/// registers this child process's own VEH, matching pass 114's proven-safe shape), THEN -- after
/// that init completes but before the child's `main()` would otherwise return -- injects the REAL
/// translated guest register context (every GPR, `eflags`, `cs`/`ss`, the exact values `do_clone`'s
/// own `child_ctx` carries) via the same `SetThreadContext` mechanism pass 118/119 proved reliable,
/// with `Rip` pointing at a stub that immediately parks (not `switch_to_guest` itself -- see this
/// module's `real_resume` doc comment for why). Mutually exclusive with the pass 118/119 minimal-stub
/// probe for a given `fork()` call (see `diagnostic_spawn_and_copy`'s dispatch): the two probe two
/// different questions and must not race the same suspended thread.
#[must_use]
pub fn diag_process_fork_real_resume_enabled() -> bool {
    std::env::var_os("LITEBOX_DIAG_PROCESS_FORK_REAL_RESUME").is_some()
}

/// Whether pass 116's fd-complexity classification log (a pure, read-only report of whether the
/// fork()ing process's fd table is "simple" -- only stdio slots 0/1/2 occupied -- or "complex" --
/// any other fd open, which per pass 115's finding cannot yet be inherited by a process-based
/// fork at all, since none of litebox's fd subsystems are backed by a real Windows HANDLE) is
/// enabled (`LITEBOX_DIAG_PROCESS_FORK_FD_COMPLEXITY=1`). Deliberately independent of every other
/// gate in this module: unlike the spawn/resume/fds probes, this one performs NO process
/// creation, memory copy, or HANDLE duplication at all -- it only logs a count `do_clone` already
/// computed -- so it is safe to enable on its own, without also opting into any of the heavier,
/// child-process-spawning probes.
#[must_use]
pub fn diag_process_fork_fd_complexity_enabled() -> bool {
    std::env::var_os("LITEBOX_DIAG_PROCESS_FORK_FD_COMPLEXITY").is_some()
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
    inject_gprs: Option<litebox::platform::ForkGprSnapshot>,
    inject_full_gprs: Option<litebox::platform::ForkFullGprSnapshot>,
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
    let want_real_resume =
        want_resume && diag_process_fork_real_resume_enabled() && inject_full_gprs.is_some();
    unsafe {
        std::env::set_var(REEXEC_CHILD_ENV_VAR, "1");
        if want_real_resume {
            std::env::set_var(REAL_RESUME_CHILD_ENV_VAR, "1");
        }
    }
    let spawn_result = spawn_suspended(&mut exe_wide, want_resume, want_fds);
    unsafe {
        std::env::remove_var(REEXEC_CHILD_ENV_VAR);
        std::env::remove_var(REAL_RESUME_CHILD_ENV_VAR);
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
        // Pass 120's real-guest-resume probe takes priority when active (it already required
        // `want_resume` and a full snapshot above): the child was spawned with
        // `REAL_RESUME_CHILD_ENV_VAR` set, so it will run its OWN normal init then park, rather
        // than either pass 114's "print marker and return" shape or pass 118/119's "never resumed
        // into its own entry point at all" shape -- none of the other three probes are meaningful
        // for this same child, so they are mutually exclusive for a given `fork()` call.
        if want_real_resume {
            if let Some(full_gprs) = inject_full_gprs {
                real_resume_and_observe(&guard, full_gprs);
            }
        } else {
            // Pass 118: when an operator opts into BOTH the resume gate and the register-injection
            // gate AND `do_clone` supplied a translated snapshot (only present on `target_arch =
            // "x86_64"`, this whole crate's only supported architecture, so always `Some` in
            // practice when the gate is on), take the injection path instead of pass 114's "resume
            // into the child's own normal entry point" path -- the two are mutually exclusive for a
            // given fork() call: injecting a context and then also letting the loader's entry point
            // run would race two completely different execution paths in the same suspended thread,
            // which is not a sensible thing to attempt.
            match inject_gprs {
                Some(gprs) => inject_and_observe(&guard, gprs),
                None => resume_and_observe(&guard, want_fds),
            }
        }
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

/// Pass 120: lets the child run its OWN normal `run_diagnostic_resume_child()` init (VEH
/// registration via `WindowsUserland::new()`, matching pass 114's proven-safe shape) by
/// `ResumeThread`-ing it exactly as [`resume_and_observe`] does, waits for
/// [`RESUME_CHILD_READY_MARKER`] to confirm that init completed, then -- BEFORE the child's own
/// `main()` would otherwise return -- `SuspendThread`s it (safe: the child is parked in a bounded
/// kernel wait via [`park_for_real_resume_injection`], not mid-instruction) and injects the REAL
/// translated guest register context via `SetThreadContext`, mirroring `switch_to_guest_ntcontinue`'s
/// own `CONTEXT` field mapping exactly (`lib.rs`), with `Rip` set directly to the real, translated
/// guest instruction address `do_clone`'s own `child_ctx.rip` computed for this `fork()` call --
/// not a diagnostic stub, unlike pass 118/119's probe.
///
/// # Why this does NOT call the real `switch_to_guest` function
///
/// `switch_to_guest` is a private `unsafe extern "C" fn` in `lib.rs`, callable only from within this
/// SAME process on the CURRENT thread (its own doc comment: "This can only be called if
/// `run_thread_arch` is on the stack"). There is no cross-process calling convention for invoking
/// another process's private function -- the only cross-process mechanism this whole investigation
/// has ever had available is `SetThreadContext`, which is exactly `switch_to_guest_ntcontinue`'s OWN
/// underlying mechanism (it builds a `CONTEXT` and calls `NtContinue`, itself just a same-process,
/// same-thread `SetThreadContext`-equivalent). This function therefore reuses `switch_to_guest_ntcontinue`'s
/// `CONTEXT`-construction logic AS DATA (the exact same field mapping, mirrored here since it cannot
/// be called as code across the process boundary) rather than its code, and injects that `CONTEXT`
/// via the literal cross-process `SetThreadContext` pass 118/119 already proved reliable for an
/// ALREADY-RUN thread (this child's thread has run its own init and reached the park point, unlike
/// pass 118/119's never-run thread) -- see [`set_child_context`]'s doc comment for why `Rsp` must
/// never be substituted on a never-run thread; that hazard does not apply here (this thread's `Rsp`
/// IS already the child's own loader-established, already-in-use stack, and this function does not
/// touch it, deliberately, to remain on the safe side of that finding either way).
///
/// This deliberately does NOT attempt cross-process `Task` reconstruction (fs/files/signals/
/// process-tree state) -- the child's own `WindowsUserland::new()` set up a BLANK slate, not a copy
/// of the parent's real `Task`. Injecting the real guest `Rip` into that blank-slate child and
/// observing exactly where/how it fails is this function's whole purpose -- see this module's
/// `FINDINGS.txt` PASS 120 section for what was actually observed.
fn real_resume_and_observe(
    guard: &SuspendedChildGuard,
    gprs: litebox::platform::ForkFullGprSnapshot,
) {
    use windows_sys::Win32::System::Threading::SuspendThread;

    const READY_TIMEOUT_MS: u32 = 2000;

    let resumed = unsafe { ResumeThread(guard.thread) };
    if resumed == u32::MAX {
        eprintln!(
            "[process_fork_diag] real-resume: initial ResumeThread failed, GetLastError={}",
            unsafe { GetLastError() }
        );
        return;
    }

    let Some(stdout_read) = guard.stdout_read else {
        eprintln!(
            "[process_fork_diag] real-resume: no stdout pipe available, cannot observe marker"
        );
        return;
    };
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(u64::from(READY_TIMEOUT_MS));
    let mut collected = Vec::new();
    let mut buf = [0u8; 256];
    let mut saw_ready = false;
    loop {
        if std::time::Instant::now() >= deadline {
            break;
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
        if ok == 0 || read == 0 {
            break;
        }
        collected.extend_from_slice(&buf[..read as usize]);
        if collected
            .windows(RESUME_CHILD_READY_MARKER.len())
            .any(|w| w == RESUME_CHILD_READY_MARKER.as_bytes())
        {
            saw_ready = true;
            break;
        }
    }
    if !saw_ready {
        eprintln!(
            "[process_fork_diag] real-resume: child never reached its own readiness marker \
             (collected stdout: {:?}) -- cannot proceed to injection",
            String::from_utf8_lossy(&collected)
        );
        return;
    }
    eprintln!(
        "[process_fork_diag] real-resume: child reached its own readiness marker (VEH registered); \
         giving it a brief moment to reach its park point before suspending"
    );
    // The child prints its marker, THEN calls `park_for_real_resume_injection` -- a short, bounded
    // sleep here is a pragmatic wait for it to actually reach the blocking `WaitForSingleObject`
    // (a handful of instructions after the marker print) rather than racing `SuspendThread` against
    // that brief window. `SuspendThread` on a thread NOT yet in the wait would still be safe (it
    // would simply suspend at whatever instruction it is executing instead), but waiting first keeps
    // the observed state consistent and easy to reason about run-to-run.
    std::thread::sleep(std::time::Duration::from_millis(200));

    let suspend_count = unsafe { SuspendThread(guard.thread) };
    if suspend_count == u32::MAX {
        eprintln!(
            "[process_fork_diag] real-resume: SuspendThread failed, GetLastError={}",
            unsafe { GetLastError() }
        );
        return;
    }

    if !set_child_full_context(guard.thread, &gprs) {
        eprintln!(
            "[process_fork_diag] real-resume: SetThreadContext (full guest context) failed, GetLastError={}",
            unsafe { GetLastError() }
        );
        return;
    }

    eprintln!(
        "[process_fork_diag] real-resume: injected REAL translated guest context Rip={:#x} Rsp={:#x} \
         Rax={:#x} -- resuming into real guest execution on a blank-slate child (no Task reconstruction)",
        gprs.rip, gprs.rsp, gprs.rax
    );

    let child_pid = unsafe { windows_sys::Win32::System::Threading::GetProcessId(guard.process) };
    let debug_attached =
        unsafe { windows_sys::Win32::System::Diagnostics::Debug::DebugActiveProcess(child_pid) }
            != 0;

    let resumed = unsafe { ResumeThread(guard.thread) };
    if resumed == u32::MAX {
        eprintln!(
            "[process_fork_diag] real-resume: post-injection ResumeThread failed, GetLastError={}",
            unsafe { GetLastError() }
        );
        if debug_attached {
            unsafe {
                windows_sys::Win32::System::Diagnostics::Debug::DebugActiveProcessStop(child_pid);
            }
        }
        return;
    }

    if debug_attached {
        observe_real_resume_fault(child_pid);
        unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::DebugActiveProcessStop(child_pid);
        }
    }

    let wait_result = unsafe { WaitForSingleObject(guard.process, 5000) };
    let mut exit_code = 0u32;
    let got_exit_code = unsafe {
        windows_sys::Win32::System::Threading::GetExitCodeProcess(guard.process, &raw mut exit_code)
    };
    eprintln!(
        "[process_fork_diag] real-resume: post-resume WaitForSingleObject={wait_result} exit_code={} ({:#x})",
        if got_exit_code != 0 {
            exit_code.to_string()
        } else {
            "?".to_string()
        },
        exit_code
    );

    // Drain and surface whatever the child itself wrote to its (pipe-captured, stdout+stderr
    // combined) output after the readiness marker -- in particular its OWN VEH's `LITEBOX_VEH_TRACE`
    // output and/or panic message when it hits an unhandled exception, which is exactly the evidence
    // needed to characterize precisely where real guest execution against a blank-slate `Task`
    // breaks down. The child has already exited (or the wait above timed out) by this point, so a
    // short bounded read is safe and will not hang.
    let mut post_injection_output = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let mut avail = 0u32;
        let peek_ok = unsafe {
            windows_sys::Win32::System::Pipes::PeekNamedPipe(
                stdout_read,
                core::ptr::null_mut(),
                0,
                core::ptr::null_mut(),
                &raw mut avail,
                core::ptr::null_mut(),
            )
        };
        if peek_ok == 0 || avail == 0 {
            break;
        }
        let mut read = 0u32;
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::ReadFile(
                stdout_read,
                buf.as_mut_ptr(),
                u32::try_from(buf.len().min(avail as usize)).unwrap_or(0),
                &raw mut read,
                core::ptr::null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            break;
        }
        post_injection_output.extend_from_slice(&buf[..read as usize]);
    }
    if !post_injection_output.is_empty() {
        eprintln!(
            "[process_fork_diag] real-resume: child's own post-injection output:\n{}",
            String::from_utf8_lossy(&post_injection_output)
        );
    }
}

/// Builds the FULL `CONTEXT` (every GPR, `eflags`, `cs`/`ss`, `rsp`) from `gprs` and injects it via
/// `SetThreadContext` -- the same `CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64` flag shape and
/// field mapping `switch_to_guest_ntcontinue` (`lib.rs`) uses for its own, same-process `NtContinue`
/// call, mirrored here as the cross-process equivalent (see `real_resume_and_observe`'s doc comment
/// for why the real function cannot be called directly across the process boundary). Unlike
/// [`set_child_context`] (pass 118/119's minimal-stub injector, which deliberately leaves `Rsp`
/// untouched because its target thread has NEVER run), this DOES set `Rsp` to the real, translated
/// guest stack pointer -- this target thread has already run (it reached the park point via ordinary
/// execution), so the pass-119 loader-thunk hazard (which only applies to a thread's very FIRST-EVER
/// resume) does not apply.
fn set_child_full_context(thread: HANDLE, gprs: &litebox::platform::ForkFullGprSnapshot) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        CONTEXT, CONTEXT_CONTROL_AMD64, CONTEXT_INTEGER_AMD64, SetThreadContext,
    };
    #[allow(
        clippy::cast_possible_truncation,
        reason = "segment selectors are always 16-bit values (USER_CS/USER_DS)"
    )]
    let context = CONTEXT {
        ContextFlags: CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64,
        R15: gprs.r15 as u64,
        R14: gprs.r14 as u64,
        R13: gprs.r13 as u64,
        R12: gprs.r12 as u64,
        Rbp: gprs.rbp as u64,
        Rbx: gprs.rbx as u64,
        R11: gprs.r11 as u64,
        R10: gprs.r10 as u64,
        R9: gprs.r9 as u64,
        R8: gprs.r8 as u64,
        Rax: gprs.rax as u64,
        Rcx: gprs.rcx as u64,
        Rdx: gprs.rdx as u64,
        Rsi: gprs.rsi as u64,
        Rdi: gprs.rdi as u64,
        Rip: gprs.rip as u64,
        Rsp: gprs.rsp as u64,
        EFlags: gprs.eflags as u32,
        SegCs: gprs.cs as u16,
        SegSs: gprs.ss as u16,
        ..unsafe { core::mem::zeroed() }
    };
    unsafe { SetThreadContext(thread, &raw const context) != 0 }
}

/// Bounded debug-event observation loop for [`real_resume_and_observe`] -- unlike
/// [`capture_first_exception_context`] (pass 118/119's version, which also reads a specific
/// diagnostic-stub marker address), this has no stub-specific marker to check: it simply logs every
/// exception's faulting `CONTEXT` (address, code, register state) so the exact failure point of a
/// REAL guest instruction executing against a blank-slate `Task` is captured precisely. Continues
/// every event via `ContinueDebugEvent` either way (the child's own VEH, registered by its own
/// `WindowsUserland::new()`, gets first crack at any exception through the normal, un-debugged
/// dispatch path -- this observer only watches, never handles).
fn observe_real_resume_fault(child_pid: u32) {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        CONTEXT, CONTEXT_CONTROL_AMD64, CONTEXT_INTEGER_AMD64, ContinueDebugEvent, DEBUG_EVENT,
        EXCEPTION_DEBUG_EVENT, GetThreadContext, WaitForDebugEvent,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, THREAD_ALL_ACCESS};

    // `DebugActiveProcess` unconditionally delivers ONE synthetic `EXCEPTION_BREAKPOINT` (the
    // well-known `ntdll!DbgBreakPoint` attach breakpoint every Windows debugger sees immediately
    // on attach, entirely independent of anything this probe's own injected context does) as part
    // of the very first batch of debug events after attaching -- this is NOT a fault produced by
    // the injected guest context, and must be skipped, not reported as this probe's own result.
    let mut skipped_attach_breakpoint = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            eprintln!(
                "[process_fork_diag] real-resume: debug-attach observation timed out with no exception event \
                 (may mean the child is still running, or exited cleanly without a fault)"
            );
            return;
        }
        let mut event: DEBUG_EVENT = unsafe { core::mem::zeroed() };
        let got = unsafe {
            WaitForDebugEvent(
                &raw mut event,
                u32::try_from(remaining.as_millis().min(u128::from(u32::MAX))).unwrap_or(u32::MAX),
            )
        };
        if got == 0 {
            eprintln!(
                "[process_fork_diag] real-resume: WaitForDebugEvent failed/timed out, GetLastError={}",
                unsafe { GetLastError() }
            );
            return;
        }
        if event.dwProcessId != child_pid {
            unsafe {
                ContinueDebugEvent(
                    event.dwProcessId,
                    event.dwThreadId,
                    windows_sys::Win32::Foundation::DBG_CONTINUE,
                );
            }
            continue;
        }
        if event.dwDebugEventCode == EXCEPTION_DEBUG_EVENT {
            let record = unsafe { &event.u.Exception.ExceptionRecord };
            if !skipped_attach_breakpoint
                && record.ExceptionCode == EXCEPTION_BREAKPOINT
                && unsafe { event.u.Exception.dwFirstChance } != 0
            {
                skipped_attach_breakpoint = true;
                eprintln!(
                    "[process_fork_diag] real-resume: skipping DebugActiveProcess's own synthetic \
                     attach breakpoint (addr={:#x}, ntdll!DbgBreakPoint) -- not this probe's own fault",
                    record.ExceptionAddress as usize
                );
                unsafe {
                    ContinueDebugEvent(
                        event.dwProcessId,
                        event.dwThreadId,
                        windows_sys::Win32::Foundation::DBG_CONTINUE,
                    );
                }
                continue;
            }
            eprintln!(
                "[process_fork_diag] real-resume: EXCEPTION_DEBUG_EVENT code={:#x} addr={:#x} \
                 tid={} first_chance={}",
                record.ExceptionCode,
                record.ExceptionAddress as usize,
                event.dwThreadId,
                unsafe { event.u.Exception.dwFirstChance }
            );
            let thread_handle = unsafe { OpenThread(THREAD_ALL_ACCESS, 0, event.dwThreadId) };
            if !thread_handle.is_null() {
                let mut ctx = CONTEXT {
                    ContextFlags: CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64,
                    ..unsafe { core::mem::zeroed() }
                };
                let ok = unsafe { GetThreadContext(thread_handle, &raw mut ctx) };
                eprintln!(
                    "[process_fork_diag] real-resume: faulting-thread CONTEXT ok={} Rip={:#x} Rsp={:#x} \
                     Rax={:#x} Rcx={:#x} Rdx={:#x} Rbx={:#x} Rsi={:#x} Rdi={:#x}",
                    ok != 0,
                    ctx.Rip,
                    ctx.Rsp,
                    ctx.Rax,
                    ctx.Rcx,
                    ctx.Rdx,
                    ctx.Rbx,
                    ctx.Rsi,
                    ctx.Rdi,
                );
                unsafe {
                    CloseHandle(thread_handle);
                }
            }
            // Let the child's OWN VEH (registered by its own `WindowsUserland::new()`) see this
            // exception through the normal, un-debugged dispatch path -- this observer only watches.
            unsafe {
                ContinueDebugEvent(
                    event.dwProcessId,
                    event.dwThreadId,
                    windows_sys::Win32::Foundation::DBG_EXCEPTION_NOT_HANDLED,
                );
            }
            // Keep watching: a VEH-handled exception may be followed by a SECOND, unhandled one
            // (e.g. the guest instruction retried and faulted again, or a different guest
            // instruction faults next) -- bounded by the same 3s deadline as the outer loop.
            continue;
        }
        unsafe {
            ContinueDebugEvent(
                event.dwProcessId,
                event.dwThreadId,
                windows_sys::Win32::Foundation::DBG_CONTINUE,
            );
        }
    }
}

/// Pass 118: injects a translated register context into the still-suspended child's main thread
/// via `SetThreadContext`, pointing `Rip` at a tiny diagnostic stub this function first writes into
/// the child's own memory, then `ResumeThread`s and observes whether the stub's marker word landed
/// in the child's memory AND its exit code matches -- the live proof the CPU actually started
/// executing AT the injected context rather than the loader's normal entry point.
///
/// # Why this does NOT use a stdout-pipe marker (unlike every other probe in this module)
///
/// GENUINE PLATFORM FINDING (pass 118, empirically confirmed): a `CREATE_SUSPENDED` child's initial
/// thread, before it has EVER executed a single instruction, has only its own EXE image and
/// `ntdll.dll` mapped -- confirmed live via a `VirtualQueryEx`-based scan of the child's address
/// space, which found exactly those two images and nothing else (not kernel32.dll, not any other
/// DLL). `kernel32.dll` (and hence `WriteFile`/`GetStdHandle`/every Win32-layer API this module's
/// OTHER probes call from a child that DID reach its own `main()`) is loaded by ntdll's OWN loader
/// code running AS the thread's first execution -- which, for this probe specifically, never
/// happens, because the whole point is to redirect that first execution somewhere else entirely.
/// `K32EnumProcessModules`/`Ex` also fail (`ERROR_PARTIAL_COPY`, persistent, not a timing race) for
/// the same underlying reason: they walk the PEB's `Ldr` list, which is populated by that same
/// loader code. This means the injected stub CANNOT safely call `WriteFile` (or anything else
/// exported only by kernel32) -- only `ntdll.dll` exports are guaranteed present. This probe's stub
/// therefore does the simplest thing that is still genuinely conclusive: write a distinctive marker
/// DWORD into a fixed, known address in the child's own memory (a plain `mov`, no external call at
/// all), then call `NtTerminateProcess` (an ntdll export, always present) with a distinctive exit
/// code. The parent verifies BOTH independently after the child exits: [`INJECTED_STUB_MARKER_WORD`]
/// read back via `ReadProcessMemory` at the known address, AND the process exit code via
/// `GetExitCodeProcess` -- two independent channels, neither of which depends on kernel32 being
/// loaded in the child at all.
fn inject_and_observe(guard: &SuspendedChildGuard, gprs: litebox::platform::ForkGprSnapshot) {
    let Some((stub_addr, marker_addr)) = write_injected_stub(guard.process) else {
        eprintln!(
            "[process_fork_diag] register-inject: failed to write diagnostic stub into child"
        );
        return;
    };

    if !set_child_context(guard.thread, stub_addr, &gprs) {
        eprintln!(
            "[process_fork_diag] register-inject: SetThreadContext failed, GetLastError={}",
            unsafe { GetLastError() }
        );
        return;
    }

    eprintln!(
        "[process_fork_diag] register-inject: injected context Rip={stub_addr:#x} (stub) \
         Rsp left at the child's own loader-established value (pass 119: substituting it crashes \
         the loader's activation-context init thunk before the stub ever runs) Rax={:#x} -- \
         resuming (guest rsp would have been {:#x})",
        gprs.rax, gprs.rsp
    );

    // Temporary pass-118 debugging aid: attach as a debugger BEFORE resuming so a fault inside the
    // stub is caught live (full faulting CONTEXT available) instead of only being visible after the
    // fact as an opaque `STATUS_ACCESS_VIOLATION` exit code. Best-effort -- if `DebugActiveProcess`
    // fails, fall through to the normal (non-debugged) resume+wait path unchanged.
    let child_pid = unsafe { windows_sys::Win32::System::Threading::GetProcessId(guard.process) };
    let debug_attached =
        unsafe { windows_sys::Win32::System::Diagnostics::Debug::DebugActiveProcess(child_pid) }
            != 0;

    let resumed = unsafe { ResumeThread(guard.thread) };
    if resumed == u32::MAX {
        eprintln!(
            "[process_fork_diag] register-inject: ResumeThread failed, GetLastError={}",
            unsafe { GetLastError() }
        );
        if debug_attached {
            unsafe {
                windows_sys::Win32::System::Diagnostics::Debug::DebugActiveProcessStop(child_pid);
            }
        }
        return;
    }

    let mut saw_marker_in_memory = false;
    if debug_attached {
        saw_marker_in_memory =
            capture_first_exception_context(child_pid, guard.process, marker_addr);
        unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::DebugActiveProcessStop(child_pid);
        }
    }

    // Bounded wait for the child to run the stub and self-terminate -- the stub is a handful of
    // instructions with no I/O and no external call target beyond `NtTerminateProcess`, so 2s is
    // generous, not tight.
    let wait_result = unsafe { WaitForSingleObject(guard.process, 2000) };

    let mut exit_code = 0u32;
    let got_exit_code = unsafe {
        windows_sys::Win32::System::Threading::GetExitCodeProcess(guard.process, &raw mut exit_code)
    };
    let saw_expected_exit_code = got_exit_code != 0 && exit_code == INJECTED_STUB_EXIT_CODE;

    if !saw_marker_in_memory || !saw_expected_exit_code {
        // Diagnostic-only: the thread is likely already gone (process terminated on fault), so
        // this GetThreadContext call is expected to fail in the common crash case -- logged either
        // way for completeness, not treated as an additional failure signal.
        use windows_sys::Win32::System::Diagnostics::Debug::{
            CONTEXT, CONTEXT_CONTROL_AMD64, CONTEXT_INTEGER_AMD64, GetThreadContext,
        };
        let mut post_context = CONTEXT {
            ContextFlags: CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64,
            ..unsafe { core::mem::zeroed() }
        };
        let ok = unsafe { GetThreadContext(guard.thread, &raw mut post_context) };
        eprintln!(
            "[process_fork_diag] register-inject: post-mortem GetThreadContext ok={} Rip={:#x} Rsp={:#x}",
            ok != 0,
            post_context.Rip,
            post_context.Rsp
        );
    }

    if saw_marker_in_memory && saw_expected_exit_code {
        eprintln!(
            "[process_fork_diag] register-inject: SUCCESS -- child executed the INJECTED stub \
             (marker word {INJECTED_STUB_MARKER_WORD:#x} observed in child memory at {marker_addr:#x}, \
             exit_code={exit_code:#x} matches expected {INJECTED_STUB_EXIT_CODE:#x}), \
             not its own normal entry point (WaitForSingleObject={wait_result})"
        );
    } else {
        eprintln!(
            "[process_fork_diag] register-inject: FAILURE or INCONCLUSIVE -- \
             saw_marker_in_memory={saw_marker_in_memory} (expected={INJECTED_STUB_MARKER_WORD:#x}) \
             saw_expected_exit_code={saw_expected_exit_code} (exit_code={} ({exit_code:#x}), expected={INJECTED_STUB_EXIT_CODE:#x}) \
             WaitForSingleObject={wait_result}",
            if got_exit_code != 0 {
                exit_code.to_string()
            } else {
                "?".to_string()
            }
        );
    }
}

/// Debugging aid (see [`inject_and_observe`]'s call site): waits, bounded, for debug events on the
/// attached child (a `DebugActiveProcess`-attached process delivers its own process-start/DLL-load
/// noise first), logging the faulting thread's full `CONTEXT` (via `GetThreadContext`, valid while
/// the exception is live and the thread has not been resumed past it) whenever an exception event
/// arrives, and returning whether the injected stub's own `int3` checkpoint (which fires right
/// after its marker-write instruction) was ever hit WITH the marker DWORD correctly observed in
/// memory at that moment -- pass 119 finding: reading the marker only AFTER the child has gone on
/// to call `NtTerminateProcess` and exit (this function's original, pass-118 ordering) can never
/// succeed, since a process's address space is unreadable once it has exited; this function reads
/// it live, at the int3 checkpoint, while the child is still alive and stopped.
/// `DBG_CONTINUE`/`DBG_EXCEPTION_NOT_HANDLED` is passed back via `ContinueDebugEvent` for every
/// event either way (a debugger MUST continue every event before the debuggee's thread can proceed
/// again), so the child's own crash/exit still happens exactly as it would un-debugged -- this is
/// purely an observation layer, not a behavior change.
fn capture_first_exception_context(
    _child_pid: u32,
    child_process: HANDLE,
    marker_addr: usize,
) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        CONTEXT, CONTEXT_CONTROL_AMD64, CONTEXT_INTEGER_AMD64, ContinueDebugEvent, DEBUG_EVENT,
        EXCEPTION_DEBUG_EVENT, GetThreadContext, ReadProcessMemory, WaitForDebugEvent,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, THREAD_ALL_ACCESS};

    let mut saw_marker = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            eprintln!(
                "[process_fork_diag] register-inject: debug-attach observation timed out with no exception event"
            );
            return saw_marker;
        }
        let mut event: DEBUG_EVENT = unsafe { core::mem::zeroed() };
        let got = unsafe {
            WaitForDebugEvent(
                &raw mut event,
                u32::try_from(remaining.as_millis().min(u128::from(u32::MAX))).unwrap_or(u32::MAX),
            )
        };
        if got == 0 {
            eprintln!(
                "[process_fork_diag] register-inject: WaitForDebugEvent failed/timed out, GetLastError={}",
                unsafe { GetLastError() }
            );
            return saw_marker;
        }

        if event.dwDebugEventCode == EXCEPTION_DEBUG_EVENT {
            let record = unsafe { &event.u.Exception.ExceptionRecord };
            let is_breakpoint = record.ExceptionCode == EXCEPTION_BREAKPOINT;
            eprintln!(
                "[process_fork_diag] register-inject: EXCEPTION_DEBUG_EVENT code={:#x} addr={:#x} tid={} is_breakpoint={is_breakpoint}",
                record.ExceptionCode, record.ExceptionAddress as usize, event.dwThreadId
            );
            let thread_handle = unsafe { OpenThread(THREAD_ALL_ACCESS, 0, event.dwThreadId) };
            if !thread_handle.is_null() {
                let mut ctx = CONTEXT {
                    ContextFlags: CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64,
                    ..unsafe { core::mem::zeroed() }
                };
                let ok = unsafe { GetThreadContext(thread_handle, &raw mut ctx) };
                eprintln!(
                    "[process_fork_diag] register-inject: faulting-thread CONTEXT ok={} Rip={:#x} Rsp={:#x} Rax={:#x} Rcx={:#x} Rdx={:#x}",
                    ok != 0,
                    ctx.Rip,
                    ctx.Rsp,
                    ctx.Rax,
                    ctx.Rcx,
                    ctx.Rdx
                );
                unsafe {
                    CloseHandle(thread_handle);
                }
            }
            if is_breakpoint {
                // Read the marker DWORD NOW, while the child is still alive and stopped -- reading
                // it only AFTER the child has gone on to call `NtTerminateProcess` and exit (pass
                // 118's original ordering) is structurally unable to ever observe it, since a
                // process's address space is gone the moment it exits. This int3 checkpoint fires
                // right after the stub's marker-write instruction, so this is the correct, and
                // only reliable, place to observe it.
                let mut marker_readback = 0u32;
                let mut bytes_read = 0usize;
                let read_ok = unsafe {
                    ReadProcessMemory(
                        child_process,
                        marker_addr as *const c_void,
                        (&raw mut marker_readback).cast::<c_void>(),
                        core::mem::size_of::<u32>(),
                        &raw mut bytes_read,
                    )
                };
                if read_ok != 0
                    && bytes_read == core::mem::size_of::<u32>()
                    && marker_readback == INJECTED_STUB_MARKER_WORD
                {
                    saw_marker = true;
                }
                eprintln!(
                    "[process_fork_diag] register-inject: marker readback at int3 checkpoint: read_ok={} value={marker_readback:#x} (expected {INJECTED_STUB_MARKER_WORD:#x})",
                    read_ok != 0
                );
                // Our own `int3` debugging checkpoint (pass 118): confirms execution reached this
                // exact point in the stub. Continue past it (DBG_CONTINUE -- the debugger IS
                // handling this exception, by design) and keep watching for the REAL crash, if any,
                // further along in the stub.
                unsafe {
                    ContinueDebugEvent(
                        event.dwProcessId,
                        event.dwThreadId,
                        windows_sys::Win32::Foundation::DBG_CONTINUE,
                    );
                }
                continue;
            }
            unsafe {
                ContinueDebugEvent(
                    event.dwProcessId,
                    event.dwThreadId,
                    windows_sys::Win32::Foundation::DBG_EXCEPTION_NOT_HANDLED,
                );
            }
            return saw_marker;
        }

        // Every other event kind (process/thread create, DLL load, OUTPUT_DEBUG_STRING, RIP) is
        // uninteresting to this probe -- continue it and keep waiting for the exception (or for the
        // bounded deadline above).
        unsafe {
            ContinueDebugEvent(
                event.dwProcessId,
                event.dwThreadId,
                windows_sys::Win32::Foundation::DBG_CONTINUE,
            );
        }
    }
}

/// The distinctive 4-byte value the injected stub writes into its own memory (at a known,
/// parent-computed address) as ONE of the two independent proofs [`inject_and_observe`] checks --
/// see that function's doc comment for why memory-write + exit-code, rather than a stdout-pipe
/// marker, is used for this specific probe.
const INJECTED_STUB_MARKER_WORD: u32 = 0x1187_1187;

/// The distinctive exit code the injected stub passes to `NtTerminateProcess` -- the SECOND of the
/// two independent proofs [`inject_and_observe`] checks.
const INJECTED_STUB_EXIT_CODE: u32 = 0x1187_2118;

// Layout (all offsets from the reserved stub page's base):
//   0x000..: code
//   0x100..: scratch marker DWORD (4 bytes, zero-initialized by MEM_COMMIT until the stub writes it)
// (pass 119: no scratch-stack region here any more -- the stub runs on the child's own
// loader-established stack; see `set_child_context`'s doc comment.)
const STUB_CODE_OFF: usize = 0x000;
const STUB_MARKER_OFF: usize = 0x100;
const STUB_PAGE_LEN: usize = 0x1000;

/// `EXCEPTION_BREAKPOINT` (`0x80000003`), used by [`capture_first_exception_context`] to
/// distinguish this module's own `int3` debugging checkpoint from a genuine crash.
const EXCEPTION_BREAKPOINT: i32 = 0x8000_0003_u32.cast_signed();

/// Writes a tiny, hand-assembled x86-64 diagnostic stub into a freshly reserved page in `child`,
/// returning `(stub_rip, marker_addr)` on success, or `None` on any Win32 failure.
///
/// The stub does exactly two things, using ONLY `ntdll.dll` export addresses (see
/// [`inject_and_observe`]'s doc comment for why kernel32-exported APIs like `WriteFile` are NOT
/// usable here) resolved relative to the CHILD's own `ntdll.dll` base (kernel32 is not even loaded
/// in the child yet at this point, but per pass 118's own live finding, `ntdll.dll` is -- mapped by
/// kernel-mode process creation before any usermode code runs; see [`find_child_module_base`]'s doc
/// comment):
///
///   1. `mov dword [marker_addr], INJECTED_STUB_MARKER_WORD`
///   2. `NtTerminateProcess(NtCurrentProcess(), INJECTED_STUB_EXIT_CODE)`
///
/// Runs on the child thread's own loader-established stack -- NOT a scratch stack inside this
/// stub's own page (see [`set_child_context`]'s doc comment for pass 119's live-confirmed finding
/// that substituting `Rsp` crashes the loader's own activation-context init thunk before the stub
/// ever runs). The stub's `sub rsp, 0x28` shadow-space reservation for its `NtTerminateProcess`
/// call operates against that real, valid, generously-sized stack.
fn write_injected_stub(child: HANDLE) -> Option<(usize, usize)> {
    let nt_terminate_process_addr = resolve_ntdll_proc(child, c"NtTerminateProcess")?;

    let reserved = unsafe {
        windows_sys::Win32::System::Memory::VirtualAlloc2(
            child,
            core::ptr::null_mut(),
            STUB_PAGE_LEN,
            MEM_RESERVE | MEM_COMMIT,
            windows_sys::Win32::System::Memory::PAGE_EXECUTE_READWRITE,
            core::ptr::null_mut(),
            0,
        )
    };
    if reserved.is_null() {
        eprintln!(
            "[process_fork_diag] register-inject: VirtualAlloc2 (stub page) failed, GetLastError={}",
            unsafe { GetLastError() }
        );
        return None;
    }
    let base = reserved as usize;
    let marker_addr = base + STUB_MARKER_OFF;

    // Hand-assembled x86-64, Win64 calling convention:
    //
    //   movabs rax, marker_addr
    //   mov dword [rax], INJECTED_STUB_MARKER_WORD
    //   sub  rsp, 0x28
    //   mov  ecx, -1                    ; NtCurrentProcess() == (HANDLE)-1
    //   mov  edx, INJECTED_STUB_EXIT_CODE
    //   movabs rax, <nt_terminate_process_addr>
    //   call rax
    //   ud2                             ; NtTerminateProcess(-1, ...) never returns; fault safely if it did
    let mut code = Vec::<u8>::new();
    code.extend_from_slice(&[0x48, 0xB8]); // movabs rax, imm64 (marker_addr)
    code.extend_from_slice(&(marker_addr as u64).to_le_bytes());
    code.extend_from_slice(&[0xC7, 0x00]); // mov dword [rax], imm32
    code.extend_from_slice(&INJECTED_STUB_MARKER_WORD.to_le_bytes());
    code.extend_from_slice(&[0xCC]); // int3 (pass-118 debugging checkpoint: did we get THIS far?)
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 0x28
    code.extend_from_slice(&[0x48, 0x83, 0xC9, 0xFF]); // or rcx, -1 (rcx := 0xFFFFFFFFFFFFFFFF, i.e. NtCurrentProcess())
    code.extend_from_slice(&[0xBA]); // mov edx, imm32
    code.extend_from_slice(&INJECTED_STUB_EXIT_CODE.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xB8]); // movabs rax, imm64 (NtTerminateProcess)
    code.extend_from_slice(&(nt_terminate_process_addr as u64).to_le_bytes());
    code.extend_from_slice(&[0xFF, 0xD0]); // call rax
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2 (unreachable in the success case)
    debug_assert!(STUB_CODE_OFF + code.len() <= STUB_MARKER_OFF);

    let mut written = 0usize;
    let ok = unsafe {
        WriteProcessMemory(
            child,
            (base + STUB_CODE_OFF) as *mut c_void,
            code.as_ptr().cast::<c_void>(),
            code.len(),
            &raw mut written,
        )
    };
    if ok == 0 || written != code.len() {
        eprintln!(
            "[process_fork_diag] register-inject: WriteProcessMemory (stub code) failed, GetLastError={}",
            unsafe { GetLastError() }
        );
        return None;
    }
    // Diagnostic readback (pass 118): confirm the exact bytes landed, ruling out a silent partial
    // write or an `ok != 0` short-write not caught by the `written != code.len()` check above.
    {
        let mut readback = vec![0u8; code.len()];
        let mut n_bytes_read = 0usize;
        let rok = unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
                child,
                (base + STUB_CODE_OFF) as *const c_void,
                readback.as_mut_ptr().cast::<c_void>(),
                readback.len(),
                &raw mut n_bytes_read,
            )
        };
        let matches = rok != 0 && n_bytes_read == code.len() && readback == code;
        eprintln!(
            "[process_fork_diag] register-inject: stub code readback matches written bytes: {matches} ({} bytes)",
            code.len()
        );
        let mut mbi: windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION =
            unsafe { core::mem::zeroed() };
        let n = unsafe {
            windows_sys::Win32::System::Memory::VirtualQueryEx(
                child,
                (base + STUB_CODE_OFF) as *const c_void,
                &raw mut mbi,
                core::mem::size_of::<windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION>(
                ),
            )
        };
        eprintln!(
            "[process_fork_diag] register-inject: stub page VirtualQueryEx -> n={n} State={:#x} Protect={:#x} Type={:#x}",
            mbi.State, mbi.Protect, mbi.Type
        );
    }

    Some((base + STUB_CODE_OFF, marker_addr))
}

/// Finds `module_file_name`'s (e.g. `"ntdll.dll"`) load base in `child` by scanning `VirtualQueryEx`
/// regions directly and matching each distinct `MEM_IMAGE` allocation's mapped filename via
/// `K32GetMappedFileNameW`, rather than `K32EnumProcessModules`/`Ex`.
///
/// GENUINE PLATFORM FINDING (pass 118, empirically confirmed, not assumed): `K32EnumProcessModules`
/// and `K32EnumProcessModulesEx` (even with `LIST_MODULES_ALL`) both fail with `ERROR_PARTIAL_COPY`
/// (299) against a `CREATE_SUSPENDED` child whose initial thread has NEVER run a single instruction
/// yet -- confirmed persistent across a 500ms bounded retry loop, not a transient race. Both K32
/// APIs walk the PEB's `Ldr->InMemoryOrderModuleList`, which is populated by ntdll's OWN loader
/// init code (`LdrInitializeThunk`) running as part of the thread's first execution -- kernel-mode
/// process creation (`NtCreateUserProcess`) maps the EXE image and `ntdll.dll` into the address
/// space BEFORE the thread ever runs (confirmed live: a raw `VirtualQueryEx` scan of a freshly
/// `CREATE_SUSPENDED` child found EXACTLY its own EXE and `ntdll.dll` mapped, and nothing else --
/// notably NOT kernel32.dll, which is loaded by ntdll's usermode loader code, i.e. by that same
/// first-execution-dependent mechanism), but the USERMODE PEB bookkeeping describing those mappings
/// is not written until that first execution happens. A `CREATE_SUSPENDED` thread, by definition,
/// has not done that yet -- so the module LIST is genuinely empty/inconsistent from the K32 APIs'
/// point of view, not merely slow to populate.
///
/// `VirtualQueryEx` + `K32GetMappedFileNameW`, in contrast, only depend on the underlying VAD
/// (kernel virtual-address-descriptor) mapping, which DOES exist from the moment
/// `NtCreateUserProcess` returns -- entirely independent of the PEB/Ldr usermode bookkeeping. This
/// is exactly why this probe's injected stub only ever calls `ntdll.dll`-exported functions
/// ([`write_injected_stub`]'s doc comment): `ntdll.dll` is the ONE module guaranteed present in a
/// `CREATE_SUSPENDED` child's address space this early, found via this same mechanism.
fn find_child_module_base(child: HANDLE, module_file_name: &str) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQueryEx};
    use windows_sys::Win32::System::ProcessStatus::K32GetMappedFileNameW;

    let mut addr: usize = 0;
    let mbi_size = core::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    let mut image_regions_seen = Vec::new();
    // Bounded to a sane number of distinct regions -- a fresh, never-run process's address space
    // has only its own image and ntdll.dll mapped at this point (plus whatever this diagnostic's
    // own group-copy loop force-reserved earlier), nowhere near this bound in practice.
    for _ in 0..4096 {
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
        let n = unsafe { VirtualQueryEx(child, addr as *const c_void, &raw mut mbi, mbi_size) };
        if n == 0 {
            break;
        }
        if mbi.Type == windows_sys::Win32::System::Memory::MEM_IMAGE {
            let mut name_buf = [0u16; 260];
            let len = unsafe {
                K32GetMappedFileNameW(child, mbi.BaseAddress, name_buf.as_mut_ptr(), 260)
            };
            let name = if len > 0 {
                String::from_utf16_lossy(&name_buf[..len as usize])
            } else {
                format!("<K32GetMappedFileNameW failed, GetLastError={}>", unsafe {
                    GetLastError()
                })
            };
            image_regions_seen.push(format!("{:#x}:{name}", mbi.BaseAddress as usize));
            let file_component = name.rsplit(['\\', '/']).next().unwrap_or(&name);
            if file_component.eq_ignore_ascii_case(module_file_name) {
                return Some(mbi.AllocationBase as usize);
            }
        }
        let region_end = (mbi.BaseAddress as usize).wrapping_add(mbi.RegionSize);
        if region_end <= addr {
            // Defensive: avoid an infinite loop if `RegionSize` is ever `0` or wraps.
            break;
        }
        addr = region_end;
    }
    eprintln!(
        "[process_fork_diag] register-inject: {module_file_name} MEM_IMAGE region not found while scanning child's address space; MEM_IMAGE regions seen: {image_regions_seen:?}"
    );
    None
}

/// Resolves `proc_name`'s address in `ntdll.dll` as loaded in the CHILD, via
/// `RVA = parent_addr - parent_ntdll_base; child_addr = child_ntdll_base + RVA` -- valid because a
/// single `ntdll.dll` BINARY's own internal RVA layout is identical across every process that loads
/// it on a given machine/OS build, only the load BASE differs (confirmed live to differ between
/// parent and a `CREATE_SUSPENDED` child on this platform -- see [`find_child_module_base`]'s doc
/// comment). `GetProcAddress` itself is only ever called on the PARENT's own already-loaded
/// `ntdll.dll` (always safe -- the parent is a normal, fully-initialized running process), never on
/// the child.
fn resolve_ntdll_proc(child: HANDLE, proc_name: &core::ffi::CStr) -> Option<usize> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    let parent_ntdll = unsafe { GetModuleHandleA(c"ntdll.dll".as_ptr().cast()) };
    if parent_ntdll.is_null() {
        eprintln!(
            "[process_fork_diag] register-inject: GetModuleHandleA(ntdll.dll) in parent failed, GetLastError={}",
            unsafe { GetLastError() }
        );
        return None;
    }
    let parent_addr = unsafe { GetProcAddress(parent_ntdll, proc_name.as_ptr().cast()) };
    let Some(parent_addr) = parent_addr else {
        eprintln!(
            "[process_fork_diag] register-inject: GetProcAddress({proc_name:?}) in parent failed, GetLastError={}",
            unsafe { GetLastError() }
        );
        return None;
    };
    let parent_ntdll_base = parent_ntdll as usize;
    let child_ntdll_base = find_child_module_base(child, "ntdll.dll")?;
    let rva = (parent_addr as usize) - parent_ntdll_base;
    let child_addr = child_ntdll_base + rva;
    eprintln!(
        "[process_fork_diag] register-inject: parent ntdll={parent_ntdll_base:#x} child ntdll={child_ntdll_base:#x} \
         {proc_name:?} rva={rva:#x} -> child_addr={child_addr:#x}"
    );
    Some(child_addr)
}

/// Sets the still-suspended child thread's `Rip`/`Rsp`/`Rax` via `GetThreadContext` +
/// `SetThreadContext`, using the SAME `ContextFlags` shape (`CONTEXT_CONTROL_AMD64 |
/// CONTEXT_INTEGER_AMD64`) `switch_to_guest`'s own `NtContinue` path (`lib.rs`'s
/// `switch_to_guest_ntcontinue`) and `ThreadHandle::interrupt`'s own cross-thread `SetThreadContext`
/// call already use -- this is deliberately the SAME flag shape a future real cross-process
/// injection would need. Only `Rip` (the stub's address) and `Rax` (the `fork()` return-value
/// register; set for completeness even though the stub's own code never reads it) are overwritten.
/// Starts from a `GetThreadContext`-read base (rather than a bare zeroed `CONTEXT`) so `Rsp`,
/// segment registers, and every other field the stub does not care about retain the loader-
/// established values from the child's own real Windows process startup.
///
/// PASS 119 GENUINE PLATFORM FINDING, empirically confirmed (not the pass 118 CFG/CET hypothesis,
/// which this pass's PE-header inspection of the runner binary REFUTED --
/// `IMAGE_DLLCHARACTERISTICS_GUARD_CF`, 0x4000, is absent from `DllCharacteristics`, and no CET
/// bit is set either; the actual root cause is entirely unrelated to CFG/CET): `Rsp` MUST be left
/// at its loader-established value, never overwritten with a substitute (e.g. the stub's own
/// scratch stack). A `CREATE_SUSPENDED` thread's very FIRST resume -- regardless of what
/// `SetThreadContext` wrote to `Rip` -- unconditionally runs ntdll's own loader-init thunk first
/// (confirmed live: `DebugActiveProcess` observation caught the fault inside an unexported ntdll
/// function, called with `Rcx` pointing at an `ACTIVATION_CONTEXT_DATA`-tagged structure --
/// `"Actx"` magic bytes read back via `ReadProcessMemory` at the crash -- i.e. SxS/manifest
/// activation-context processing, which the loader always performs before ever transferring
/// control to the resumed thread's nominal entry point). That loader-init code reads/writes data
/// at stack-relative offsets from the ORIGINAL, kernel-established `Rsp`; substituting a foreign
/// `Rsp` (even a valid, committed, writable page) corrupts those reads and crashes the loader
/// thunk itself -- BEFORE the injected `Rip` is ever reached, which is exactly the previously
/// unexplained "stub never executes even its first instruction" symptom. Leaving `Rsp` untouched
/// (the original loader-established stack is a real, valid, generously-sized stack -- the stub's
/// own `sub rsp, 0x28` before its `NtTerminateProcess` call works against it unmodified) resolves
/// this completely; live-verified 5/5 runs (see this pass's `FINDINGS.txt` section).
fn set_child_context(
    thread: HANDLE,
    stub_rip: usize,
    gprs: &litebox::platform::ForkGprSnapshot,
) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        CONTEXT, CONTEXT_CONTROL_AMD64, CONTEXT_INTEGER_AMD64, GetThreadContext, SetThreadContext,
    };
    let mut context = CONTEXT {
        ContextFlags: CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64,
        ..unsafe { core::mem::zeroed() }
    };
    let ok = unsafe { GetThreadContext(thread, &raw mut context) };
    if ok == 0 {
        eprintln!(
            "[process_fork_diag] register-inject: GetThreadContext failed, GetLastError={}",
            unsafe { GetLastError() }
        );
        return false;
    }
    eprintln!(
        "[process_fork_diag] register-inject: pre-injection (loader-established) context: Rip={:#x} Rsp={:#x}",
        context.Rip, context.Rsp
    );

    context.Rip = stub_rip as u64;
    context.Rax = gprs.rax as u64;
    context.ContextFlags = CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64;

    let ok = unsafe { SetThreadContext(thread, &raw const context) };
    if ok == 0 {
        return false;
    }

    // Diagnostic readback (pass 118): confirm the CONTEXT we just wrote is what `GetThreadContext`
    // reports back, BEFORE resuming -- isolates "SetThreadContext silently did not take" from
    // "the CPU took the context but then faulted executing the stub itself".
    let mut readback = CONTEXT {
        ContextFlags: CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64,
        ..unsafe { core::mem::zeroed() }
    };
    let ok = unsafe { GetThreadContext(thread, &raw mut readback) };
    if ok != 0 {
        eprintln!(
            "[process_fork_diag] register-inject: readback after SetThreadContext: Rip={:#x} (expected {stub_rip:#x}) Rsp={:#x} (unmodified, loader-established) SegCs={:#x} SegSs={:#x} EFlags={:#x}",
            readback.Rip, readback.Rsp, readback.SegCs, readback.SegSs, readback.EFlags
        );
    }

    true
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
