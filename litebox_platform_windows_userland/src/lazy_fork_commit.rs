//! Lazy (reserve-then-commit-on-first-fault) memory population for cross-process
//! `LITEBOX_PROCESS_FORK=1` fork children -- the mechanism 76th-82nd pass's own RAM-crater
//! investigation converged on as the only remaining safe lever (see `AGENTS.md`'s "Where things
//! stand" section as of the pass that added this file).
//!
//! # KNOWN UNSAFE CASE -- do not flip `LITEBOX_LAZY_FORK_COMMIT=1` on for a real boot yet
//!
//! **Verified safe and a real, measured win** for a fork that `execve()`s essentially immediately
//! (the dominant real-world case, ~85% of fork-exec cycles per the 79th pass's own measurement):
//! 5/5 clean runs, both debug and release, `bash -c 'echo hello; sleep 0.2; echo done'`
//! (the `sleep` fork execve's almost instantly) -- correct output, exit 0, real ~3-18x
//! parent-side group-copy speedup measured both builds (debug: 103ms eager vs 34ms mixed / 6
//! groups, 2 lazy; release: 6ms mixed vs a proportionally larger eager baseline).
//!
//! **NOT yet safe for a fork that keeps running WITHOUT `execve()`ing** (e.g. a bash `(...)`
//! subshell, which runs bash's own already-forked, already-copied code directly rather than
//! replacing its address space): 5/5 clean, 100%-reproducible crash,
//! `bash -c '(echo subshell_child; x=inner_var; echo $x); echo parent_after'` -- the subshell
//! prints `subshell_child` then is killed (`/bin/bash: ... Killed`) before `inner_var` ever
//! prints. The crash is a genuine `EXCEPTION_ACCESS_VIOLATION` (code fetch, confirmed via
//! `LITEBOX_DIAG_FATALDUMP=1`) at an address that is NOT inside any range this module reserved
//! lazily -- it lands ~0xff000 bytes above the lazy stack group's own end, inside a separate,
//! small (`0x40000`-byte) group that this run's own classification left on the EAGER
//! `copy_one_group` path, with an unexplained `PAGE_READONLY` (`0x2`) protection that neither
//! `copy_one_group`'s blanket `PAGE_READWRITE` nor the PASS-144 exec-fixup's
//! `PAGE_EXECUTE_READ`/`PAGE_EXECUTE_READWRITE` would ever produce. The obvious "just needs more
//! wall-clock slack before the eager path's own ~100ms of `WriteProcessMemory` calls would have
//! given some other startup step time to finish" theory was tested directly (an artificial
//! `LITEBOX_DIAG_LAZY_FORK_ARTIFICIAL_DELAY_MS` re-added after group reservation, up to 80ms,
//! i.e. matching/exceeding the eager path's own real elapsed time) and REFUTED: the crash is
//! still 5/5 with the delay in place, so it is not a simple "the lazy path finishes too fast"
//! race. Root cause NOT found this pass -- the leading hypothesis (untested, needs a live
//! `cdb -pv` attach per this project's own established practice for this exact class of bug,
//! debug binary, per `AGENTS.md`'s standing constraints on release-binary `cdb` reliability) is
//! an interaction with `fork_verify.rs`'s own watched-code-page/exception machinery, since the
//! crash's `[codewatch]` diagnostic explicitly logged `watched=false` for the faulting page --
//! either a real gap in that recognition, or a hint the true cause is elsewhere entirely.
//!
//! **Because of this, [`lazy_fork_commit_enabled`] stays an explicit opt-in
//! (`LITEBOX_LAZY_FORK_COMMIT=1`), default OFF, and this module's own existence changes NOTHING
//! about the default fork path** (confirmed live, this same pass: 3/3 clean runs of the exact
//! subshell repro above with the env var unset, byte-identical correct output to before this
//! module existed). Do not flip this on for `.wfgy/webtop_stack.sh` or any real boot attempt
//! until the subshell case above is root-caused and fixed -- a real desktop boot forks many
//! long-lived processes (shells, daemons) that do not immediately `execve()`, so this exact bug
//! class would very likely recur on the actual boot path, worse and harder to isolate than this
//! clean, minimal, 100%-reproducible standalone repro.
//!
//! # Why this exists
//!
//! `copy_one_group` (this crate's `process_fork` module) reserves AND fully commits AND
//! byte-copies every fork-carried VMA group EAGERLY, at fork time, in the parent, before the
//! child ever runs a single instruction -- regardless of whether the child immediately
//! `execve()`s (measured: ~85% of fork-exec cycle time wasted on copying data an `execve` throws
//! away moments later) or whether the region is mostly untouched (a guest's 8 MiB stack region is
//! the canonical example: only a handful of pages near the top are ever really touched).
//!
//! This module implements the alternative: for ELIGIBLE groups (see
//! [`classify_lazy_eligible_groups`]'s doc comment for exactly which groups qualify), the parent
//! only `VirtualAlloc2`s the exact span with `MEM_RESERVE` (no `MEM_COMMIT`, no data copied at
//! all) at fork time, and the CHILD installs a Vectored Exception Handler that commits + populates
//! each page LAZILY, the first time the child's own guest code actually touches it (read OR
//! write -- population happens before the access is allowed to complete either way, so there is
//! no window where the child could observe uninitialized memory).
//!
//! # Correctness argument (why this is safe where a timing-based "defer the whole copy" trick is
//! not -- see the 81st pass's entry in `AGENTS.md` for the timing trick this deliberately is NOT)
//!
//! A plain `fork()`ed child has no POSIX guarantee it won't touch its own (logically
//! copy-on-write) memory before its first syscall, so any mechanism that defers population based
//! on TIME or on "has the child made a syscall yet" can race a child that writes immediately.
//! This mechanism defers based on ACCESS, not time: the very first load or store to a byte in a
//! lazily-reserved range takes a hardware page fault (the page genuinely is not present -- Windows
//! reports `EXCEPTION_ACCESS_VIOLATION` the same way it would for any other not-yet-committed
//! `MEM_RESERVE` region), the VEH handler catches that EXACT fault, synchronously populates the
//! page from the parent's real data at that address, and only then returns
//! `EXCEPTION_CONTINUE_EXECUTION`, which re-executes the very same faulting instruction -- now
//! against real, correct data. There is no way for the child to observe stale/zero/uninitialized
//! content, because the hardware itself refuses the access until this handler has run.
//!
//! # Scope of this first landing (deliberately narrow)
//!
//! Only PURE-DATA groups (no `VM_EXEC` byte anywhere in the group's covering `vma_layout`
//! ranges) are made lazy. CODE groups keep using the existing eager `copy_one_group` path
//! unconditionally. This sidesteps the separate, already-fragile EXEC-permission-fixup pass in
//! `process_fork.rs` (`spawn_process_fork_child`'s post-copy `vma_layout` walk, PASS 144) entirely
//! -- that pass only ever iterates VMAs with `VM_EXEC` set, so a group this module claims never
//! touches it. Every page this module commits is committed as blanket `PAGE_READWRITE`, matching
//! `copy_one_group`'s own existing behavior for every non-exec page today (that function has never
//! narrowed permissions for read-only DATA pages either -- see PASS 144's own doc comment: "A
//! region with none of READ/WRITE/EXEC set is left at the group's default `PAGE_READWRITE`"), so
//! this is not a new permissiveness regression relative to the eager path it replaces.
//!
//! Gated behind `LITEBOX_LAZY_FORK_COMMIT=1`, default OFF: with the flag unset,
//! [`classify_lazy_eligible_groups`] always returns an empty set, every group takes the existing
//! eager `copy_one_group` path exactly as before, and the child never installs this module's VEH
//! handler at all (see [`install_if_configured`]'s early return) -- so the entire default/existing
//! behavior of every process this project already relies on is provably untouched by this file's
//! mere existence.
//!
//! # Concurrency
//!
//! Deliberately lock-free: the VEH handler may run on multiple guest threads concurrently
//! faulting on the SAME page. `VirtualAlloc(MEM_COMMIT)` on an already-committed page and
//! `ReadProcessMemory`+copy of the same bytes twice are both idempotent and harmless -- so no
//! "already populated" bit, no mutex, no risk of a fault handler blocking on a lock another
//! faulting thread holds. The lazy-range table itself ([`LAZY_RANGES`]) and the parent handle
//! ([`PARENT_HANDLE`]) are written exactly ONCE, by [`install_if_configured`], before the child
//! resumes any guest execution, and never mutated again -- `OnceLock`'s own publication fence
//! makes that one write visible to every later reader on every thread without further
//! synchronization.

use core::ffi::c_void;
use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::ops::Range;
use std::sync::OnceLock;

use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
    EXCEPTION_POINTERS, ReadProcessMemory,
};
use windows_sys::Win32::System::Memory::{
    MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_EXTENDED_PARAMETER_0,
    MEM_EXTENDED_PARAMETER_1, MEM_RESERVE, MemExtendedParameterAddressRequirements,
    PAGE_READWRITE, VirtualAlloc, VirtualAlloc2,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_VM_READ};

use crate::process_fork::GroupCopyResult;

pub type Handle = windows_sys::Win32::Foundation::HANDLE;

const PAGE_SIZE: usize = 4096;
const VM_EXEC: u32 = 1 << 2;

/// Opt-in flag: `LITEBOX_LAZY_FORK_COMMIT=1` enables lazy reserve-on-fault population for eligible
/// (pure-data) fork-carried groups. Unset (the default) preserves the pre-existing eager
/// `copy_one_group` behavior for EVERY group, unconditionally.
#[must_use]
pub fn lazy_fork_commit_enabled() -> bool {
    std::env::var_os("LITEBOX_LAZY_FORK_COMMIT").is_some()
}

/// Internal-only marker env var: carries the set of group spans the parent reserved (but did NOT
/// commit or copy) as `start-end` hex pairs, comma-separated, e.g. `"7f0000-810000,900000-1100000"`.
/// Consumed by [`install_if_configured`] in the child. Never guest-visible.
pub const FORK_CHILD_LAZY_RANGES_ENV_VAR: &str = "LITEBOX_INTERNAL_FORK_CHILD_LAZY_RANGES";

/// Internal-only marker env var: the parent's own PID, decimal, so the child can `OpenProcess`
/// with `PROCESS_VM_READ` and pull real data from the parent's address space on each lazy fault.
/// Only set (by the parent) when [`FORK_CHILD_LAZY_RANGES_ENV_VAR`] is non-empty. Never
/// guest-visible.
pub const FORK_CHILD_PARENT_PID_ENV_VAR: &str = "LITEBOX_INTERNAL_FORK_CHILD_PARENT_PID";

/// Parent-side: given the SAME `group_relocations`/`vma_layout` `spawn_process_fork_child`
/// already has in hand, returns the subset of `group_relocations` eligible for lazy treatment --
/// empty unless [`lazy_fork_commit_enabled`], and always excluding any group whose covering
/// `vma_layout` range(s) include `VM_EXEC` (see this module's own doc comment for why CODE stays
/// on the eager path). Order matches `group_relocations`' own order; callers use this to decide,
/// per group, whether to call [`reserve_group_lazy`] instead of `copy_one_group`.
#[must_use]
pub fn classify_lazy_eligible_groups(
    group_relocations: &[(Range<usize>, usize)],
    vma_layout: &[(Range<usize>, u32, bool)],
) -> Vec<bool> {
    if !lazy_fork_commit_enabled() {
        return vec![false; group_relocations.len()];
    }
    group_relocations
        .iter()
        .map(|(group, _dest_base)| {
            // Exclude the group the instant ANY overlapping vma_layout range carries VM_EXEC --
            // conservative by design: a group is data-only, and therefore lazy-eligible, only if
            // every vma_layout range touching it agrees.
            !vma_layout
                .iter()
                .any(|(range, flags, _)| ranges_overlap(range, group) && flags & VM_EXEC != 0)
        })
        .collect()
}

fn ranges_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

/// Serializes the given group spans as the `FORK_CHILD_LAZY_RANGES_ENV_VAR` value.
#[must_use]
pub fn serialize_lazy_ranges(ranges: &[Range<usize>]) -> String {
    ranges
        .iter()
        .map(|r| format!("{:x}-{:x}", r.start, r.end))
        .collect::<Vec<_>>()
        .join(",")
}

fn deserialize_lazy_ranges(s: &str) -> Vec<Range<usize>> {
    s.split(',')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let (a, b) = part.split_once('-')?;
            let start = usize::from_str_radix(a, 16).ok()?;
            let end = usize::from_str_radix(b, 16).ok()?;
            (start < end).then_some(start..end)
        })
        .collect()
}

/// Parent-side: reserves (but does NOT commit or copy) `source_group`'s exact span at its exact
/// SOURCE address in the child, mirroring `copy_one_group`'s own step 1 (same
/// `MEM_ADDRESS_REQUIREMENTS`-forced `VirtualAlloc2` call, same hard-fail-on-address-mismatch
/// discipline) but stopping there -- no `WriteProcessMemory`, no per-page loop. The actual
/// population happens later, lazily, in the CHILD, via this module's VEH handler
/// ([`lazy_commit_veh`]), once [`install_if_configured`] has told it which ranges to expect.
#[must_use]
pub fn reserve_group_lazy(child: Handle, source_group: &Range<usize>) -> GroupCopyResult {
    let len = source_group.len();
    let fail = |err: u32| GroupCopyResult {
        source_group: source_group.clone(),
        succeeded: false,
        last_error: err,
    };
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
    let reserved = unsafe {
        VirtualAlloc2(
            child,
            core::ptr::null_mut(),
            len,
            MEM_RESERVE, // NOTE: reserve only -- no MEM_COMMIT, unlike copy_one_group.
            PAGE_READWRITE,
            &raw mut ext_param,
            1,
        )
    };
    if reserved.is_null() {
        return fail(unsafe { windows_sys::Win32::Foundation::GetLastError() });
    }
    if reserved as usize != source_group.start {
        unsafe {
            windows_sys::Win32::System::Memory::VirtualFreeEx(
                child,
                reserved,
                0,
                windows_sys::Win32::System::Memory::MEM_RELEASE,
            );
        }
        return fail(0);
    }
    GroupCopyResult {
        source_group: source_group.clone(),
        succeeded: true,
        last_error: 0,
    }
}

// ---- Child-side state, populated exactly once by install_if_configured ----

static LAZY_RANGES: OnceLock<Vec<Range<usize>>> = OnceLock::new();
/// Cached `OpenProcess(PROCESS_VM_READ, ..., parent_pid)` handle, as a raw pointer value (0 =
/// unset). `HANDLE` (`isize`/`*mut c_void`-shaped) is not itself `Sync`, so it is stored as an
/// `AtomicIsize` and cast back to `Handle` at each use -- read-only after
/// [`install_if_configured`]'s single write.
static PARENT_HANDLE: AtomicIsize = AtomicIsize::new(0);
/// Diagnostic-only (`LITEBOX_DIAG_LAZY_FORK_COMMIT=1`): total lazy faults this process's own
/// [`lazy_commit_veh`] has serviced, and how many of those found the parent's data unreadable
/// (left zero-filled). Never consulted for correctness -- purely observational counters for
/// verifying the mechanism actually engages during a real fork (as opposed to every group being
/// skipped entirely because the child happened to `execve` before touching anything).
static LAZY_FAULTS_SERVICED: AtomicUsize = AtomicUsize::new(0);
static LAZY_FAULTS_ZERO_FILLED: AtomicUsize = AtomicUsize::new(0);

/// Child-side: reads [`FORK_CHILD_LAZY_RANGES_ENV_VAR`]/[`FORK_CHILD_PARENT_PID_ENV_VAR`] (set by
/// the parent only when [`lazy_fork_commit_enabled`] and at least one group qualified) and, if
/// present, opens a `PROCESS_VM_READ` handle to the parent and installs [`lazy_commit_veh`] as the
/// FIRST handler in this process' VEH chain (`AddVectoredExceptionHandler(1, ..)` prepends).
///
/// MUST be called before ANY guest code in this child touches a lazily-reserved address --
/// i.e. before `run_thread`/`adopt_forked_process` resumes real guest execution. Call site:
/// `litebox_runner_linux_on_windows_userland`'s fork-child startup path, immediately next to
/// where `FORK_CHILD_VMA_LAYOUT_ENV_VAR` is parsed (same env-var-based bootstrap channel, same
/// "before guest execution resumes" timing guarantee that channel already relies on).
///
/// A no-op (does not touch the VEH chain at all) when the env var is absent/empty -- this is what
/// makes the whole mechanism provably inert for every non-opted-in fork and every ordinary
/// (non-fork) process.
pub fn install_if_configured() {
    let Some(ranges_line) = std::env::var(FORK_CHILD_LAZY_RANGES_ENV_VAR).ok() else {
        return;
    };
    let ranges = deserialize_lazy_ranges(&ranges_line);
    if ranges.is_empty() {
        return;
    }
    let Some(parent_pid) = std::env::var(FORK_CHILD_PARENT_PID_ENV_VAR)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
    else {
        eprintln!(
            "[lazy_fork_commit] {FORK_CHILD_LAZY_RANGES_ENV_VAR} set but {FORK_CHILD_PARENT_PID_ENV_VAR} \
             missing/unparseable -- disabling lazy commit for this child (falling back to whatever \
             the eager path already wrote, which is nothing for these ranges: they will fault as \
             genuine access violations). This should never happen outside a bug in the parent's own \
             spawn_process_fork_child."
        );
        return;
    };
    let parent_handle = unsafe { OpenProcess(PROCESS_VM_READ, 0, parent_pid) };
    if parent_handle.is_null() {
        eprintln!(
            "[lazy_fork_commit] OpenProcess(PROCESS_VM_READ, parent_pid={parent_pid}) FAILED \
             GetLastError={} -- disabling lazy commit for this child",
            unsafe { windows_sys::Win32::Foundation::GetLastError() }
        );
        return;
    }
    PARENT_HANDLE.store(parent_handle as isize, Ordering::SeqCst);
    if std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some() {
        eprintln!(
            "[lazy_fork_commit] install_if_configured: registering VEH for {} range(s), parent_pid={parent_pid}, parent_handle={parent_handle:p}: {ranges:?}",
            ranges.len()
        );
    }
    let _ = LAZY_RANGES.set(ranges);
    let installed = unsafe { AddVectoredExceptionHandler(1, Some(lazy_commit_veh)) };
    if std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some() {
        eprintln!("[lazy_fork_commit] AddVectoredExceptionHandler returned {installed:p}");
    }
}

/// Vectored Exception Handler: claims an `EXCEPTION_ACCESS_VIOLATION` iff the faulting address
/// falls within a range [`install_if_configured`] registered for THIS process -- every other
/// fault (including every fault this process's own pre-existing `fork_verify`/
/// `vectored_exception_handler` machinery is responsible for) is declined via
/// `EXCEPTION_CONTINUE_SEARCH`, letting it fall through to the next handler in the chain
/// UNCHANGED. This handler never touches `ContextRecord`/registers at all -- it only commits
/// memory and copies bytes, then lets the CPU naturally re-execute the original faulting
/// instruction, which is what makes it safe to run before whatever handler
/// `AddVectoredExceptionHandler(1, ..)` had previously prepended (this process's own
/// `vectored_exception_handler_entry`, if this is a real guest-hosting fork child).
static VEH_ENTRY_COUNT_DIAG: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn lazy_commit_veh(info: *mut EXCEPTION_POINTERS) -> i32 {
    let diag = std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some();
    if diag {
        let n = VEH_ENTRY_COUNT_DIAG.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 40 {
            let rec = unsafe { &*(*info).ExceptionRecord };
            let addr = rec.ExceptionInformation.get(1).copied().unwrap_or(0);
            eprintln!(
                "[lazy_fork_commit] VEH entry #{n}: code={:#x} addr={addr:#x} ranges_set={}",
                rec.ExceptionCode,
                LAZY_RANGES.get().is_some()
            );
        }
    }
    let Some(ranges) = LAZY_RANGES.get() else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let rec = unsafe { &*(*info).ExceptionRecord };
    const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
    if rec.ExceptionCode.cast_unsigned() != EXCEPTION_ACCESS_VIOLATION {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let fault_addr = rec.ExceptionInformation[1];
    let Some(_matched) = ranges.iter().find(|r| r.contains(&fault_addr)) else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let page_addr = fault_addr & !(PAGE_SIZE - 1);

    let committed =
        unsafe { VirtualAlloc(page_addr as *mut c_void, PAGE_SIZE, MEM_COMMIT, PAGE_READWRITE) };
    if committed.is_null() {
        // Genuinely can't service this fault (e.g. host OOM) -- decline rather than pretend
        // success; the guest sees a real SIGSEGV via whatever this process's normal unhandled-AV
        // path does, which is the honest outcome when memory cannot be provided.
        return EXCEPTION_CONTINUE_SEARCH;
    }

    let parent_handle = PARENT_HANDLE.load(Ordering::SeqCst) as Handle;
    if !parent_handle.is_null() {
        let mut buf = [0u8; PAGE_SIZE];
        let mut read_len: usize = 0;
        let ok = unsafe {
            ReadProcessMemory(
                parent_handle,
                page_addr as *const c_void,
                buf.as_mut_ptr().cast(),
                PAGE_SIZE,
                &mut read_len,
            )
        };
        if ok != 0 && read_len == PAGE_SIZE {
            unsafe {
                core::ptr::copy_nonoverlapping(buf.as_ptr(), page_addr as *mut u8, PAGE_SIZE);
            }
        } else {
            LAZY_FAULTS_ZERO_FILLED.fetch_add(1, Ordering::Relaxed);
        }
        // else (zero-filled case): unreadable in the parent (real, unmapped padding) -- leave the
        // freshly-committed page zero-filled, matching copy_one_group's own existing behavior for
        // unreadable pages.
    }
    let n = LAZY_FAULTS_SERVICED.fetch_add(1, Ordering::Relaxed) + 1;
    if std::env::var_os("LITEBOX_DIAG_LAZY_FORK_COMMIT").is_some() && n <= 40 {
        eprintln!(
            "[lazy_fork_commit] fault #{n} serviced: page={page_addr:#x} zero_filled_so_far={}",
            LAZY_FAULTS_ZERO_FILLED.load(Ordering::Relaxed)
        );
    }

    EXCEPTION_CONTINUE_EXECUTION
}
