// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Diagnostic instrumentation for the shim: an optional per-syscall summary
//! (`LITEBOX_STRACE_SUMMARY=1`) and an always-on guest process timeline/tree.
//!
//! This module is deliberately free of any `Platform`/`FS` generic parameter so its global
//! state can live in plain `static`s (the `no_std` idiom already used elsewhere in this
//! workspace, e.g. `litebox::sync::lock_tracing::EVENT_RECORDER`), gated at each call site by
//! [`litebox::platform::SystemInfoProvider::env_flag`] rather than a direct host env-var read
//! (this crate is `#![no_std]` and has no other way to observe the host process environment).

use alloc::collections::btree_map::BTreeMap;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Per-syscall accounting, keyed by raw syscall number.
#[derive(Default)]
struct SyscallStats {
    count: u64,
    total_ns: u64,
    max_ns: u64,
    /// Histogram of `Debug`-formatted errno values seen on `Err` results.
    errors: BTreeMap<String, u64>,
}

/// A single "returned an unsupported-style errno" observation, recorded once per distinct
/// `(syscall/subcommand, errno)` pair with the first caller that hit it.
struct UnsupportedHit {
    first_pid: i32,
    first_comm: String,
    count: u64,
}

#[derive(Default)]
struct StraceSummary {
    by_syscall: BTreeMap<usize, SyscallStats>,
    /// Syscall numbers that failed to resolve via `SyscallRequest::try_from_raw` at all,
    /// keyed by raw number, recording the first caller.
    unresolved: BTreeMap<usize, UnsupportedHit>,
    /// `ENOSYS`/`EINVAL`/`ENOTSUP`/`EOPNOTSUPP`/`EPERM` sub-command misses, keyed by a
    /// free-form description (e.g. `"ioctl(TIOCGWINSZ)"`, `"prctl(PR_SET_NAME)"`).
    unsupported_subcommands: BTreeMap<String, UnsupportedHit>,
}

static STRACE_ENABLED: AtomicBool = AtomicBool::new(false);
static STRACE_INIT: AtomicBool = AtomicBool::new(false);
static STRACE_SUMMARY: spin::Mutex<StraceSummary> = spin::Mutex::new(StraceSummary {
    by_syscall: BTreeMap::new(),
    unresolved: BTreeMap::new(),
    unsupported_subcommands: BTreeMap::new(),
});

/// Errno names treated as "this syscall/sub-command isn't really supported" for the
/// dedicated unsupported-commands report section.
const NOTABLE_ERRNOS: &[&str] = &["ENOSYS", "EINVAL", "ENOTSUP", "EOPNOTSUPP", "EPERM"];

/// Call once, early (e.g. from the first syscall dispatch), with a closure performing the
/// platform's `env_flag` lookup for `LITEBOX_STRACE_SUMMARY`. Idempotent, and after the first
/// call costs one acquire load -- the closure is never run again.
pub fn init_strace_summary(enabled: impl FnOnce() -> bool) {
    // The parameter is a closure, not a `bool`, so the caller's `env_flag` lookup is not
    // evaluated on every call. It used to be: an eagerly-evaluated argument made this
    // "idempotent and cheap after the first call" latch cost a full host environment-variable
    // read per syscall per thread, which on Windows means a process-wide critical section and
    // two allocations -- the exact opposite of what the call site's own comment promised. The
    // `swap` is gone for the same reason: an unconditional read-modify-write on a shared cache
    // line, once per syscall on every guest thread, is not free either.
    if STRACE_INIT.load(Ordering::Acquire) {
        return;
    }
    // Racing callers are harmless: they read the same host environment and latch the same value.
    // `ENABLED` is published before `INIT` so a reader that observes the latch also observes the
    // value that was latched.
    STRACE_ENABLED.store(enabled(), Ordering::Release);
    STRACE_INIT.store(true, Ordering::Release);
}

pub fn strace_summary_enabled() -> bool {
    STRACE_ENABLED.load(Ordering::Acquire)
}

/// Unconditionally sets the live enabled state, unlike [`init_strace_summary`] -- that function
/// is a one-shot latch (a second call is a no-op by design, see its own doc comment), so it
/// cannot serve a genuine runtime `strace on`/`strace off` control-channel command
/// (`docs/presenter-process-design.md` section 3.2). This is that missing setter: cheap (one
/// store), safe to call from any thread at any time, and idempotent in the sense that matters for
/// a toggle (setting the same value twice is harmless), not in `init_strace_summary`'s
/// call-once-only sense.
pub fn set_strace_summary_enabled(enabled: bool) {
    STRACE_ENABLED.store(enabled, Ordering::Release);
    STRACE_INIT.store(true, Ordering::Release);
}

/// Record one completed syscall dispatch. `duration_ns` is the wall time spent in
/// `do_syscall`. `err_debug` is `Some(format!("{err:?}"))` on an `Err` result.
pub fn record_syscall(syscall_number: usize, duration_ns: u64, err_debug: Option<String>) {
    if !strace_summary_enabled() {
        return;
    }
    let mut guard = STRACE_SUMMARY.lock();
    let stats = guard.by_syscall.entry(syscall_number).or_default();
    stats.count += 1;
    stats.total_ns += duration_ns;
    if duration_ns > stats.max_ns {
        stats.max_ns = duration_ns;
    }
    if let Some(errno) = err_debug {
        *stats.errors.entry(errno).or_insert(0) += 1;
    }
}

/// Record a raw syscall number that `SyscallRequest::try_from_raw` could not resolve at all.
pub fn record_unresolved_syscall(syscall_number: usize, pid: i32, comm: &str) {
    if !strace_summary_enabled() {
        return;
    }
    let mut guard = STRACE_SUMMARY.lock();
    guard
        .unresolved
        .entry(syscall_number)
        .or_insert_with(|| UnsupportedHit {
            first_pid: pid,
            first_comm: comm.to_string(),
            count: 0,
        })
        .count += 1;
}

/// Record a syscall/ioctl/fcntl/prctl/setsockopt sub-command that returned one of
/// [`NOTABLE_ERRNOS`]. `description` should identify the sub-command, e.g.
/// `"ioctl(0x5413)"` or `"setsockopt(SOL_SOCKET, SO_REUSEPORT)"`. `errno_name` should be the
/// `Debug`-formatted errno (e.g. `"ENOSYS"`).
pub fn record_unsupported_subcommand(description: &str, errno_name: &str, pid: i32, comm: &str) {
    if !strace_summary_enabled() {
        return;
    }
    if !NOTABLE_ERRNOS.contains(&errno_name) {
        return;
    }
    let mut guard = STRACE_SUMMARY.lock();
    let key = alloc::format!("{description} -> {errno_name}");
    guard
        .unsupported_subcommands
        .entry(key)
        .or_insert_with(|| UnsupportedHit {
            first_pid: pid,
            first_comm: comm.to_string(),
            count: 0,
        })
        .count += 1;
}

static SYSCALL_TIMELINE_ENABLED: AtomicBool = AtomicBool::new(false);
static SYSCALL_TIMELINE_INIT: AtomicBool = AtomicBool::new(false);

/// The process names the timeline is currently aimed at, parsed once from the env var's value.
///
/// Empty means the timeline is off; it is never "empty means everything", which is the whole
/// safety property here (see [`SYSCALL_TIMELINE_DEFAULT_COMMS`]).
static SYSCALL_TIMELINE_COMMS: spin::Mutex<Vec<String>> = spin::Mutex::new(Vec::new());

/// The list `LITEBOX_DIAG_SYSCALL_TIMELINE=1` selects, kept so the historical spelling of this
/// flag keeps doing exactly what it used to.
///
/// These are the XFCE components from the "why does this client go silent after CreateWindow"
/// investigation (AGENTS.md's "Rendering/scanout blocker" section), confirmed via X11 protocol
/// decode to create a window, do some property setup, then never issue another X11 request.
const SYSCALL_TIMELINE_DEFAULT_COMMS: &[&str] = &["xfwm4", "xfdesktop", "xfce4-panel", "xfce4-about"];

/// Call once, early, with a closure performing the platform's `env_value` lookup for
/// `LITEBOX_DIAG_SYSCALL_TIMELINE`. Idempotent, same lazy-latch pattern as
/// [`init_strace_summary`], including why the argument is a closure.
///
/// # Why this takes a value rather than a flag
///
/// The target list used to be a `const` of four hard-coded XFCE process names, deliberately
/// fixed "so this stays a targeted diagnostic rather than growing back into the every-process
/// firehose that OOM'd the host once already". The bound was right; hard-coding it was not. It
/// made the single most useful instrument in the shim answer questions about exactly one desktop
/// -- pointing it at a silent `mate-session` meant editing this file and rebuilding, which is a
/// thing you only discover you need in the middle of the investigation that needs it.
///
/// The value form keeps the bound exactly as strong: the list is still an explicit enumeration of
/// process names, still never a wildcard, and an unset variable still traces nothing. All that
/// changes is WHO writes the list -- the person running the investigation instead of whoever last
/// edited this constant.
///
/// - unset or empty -> off
/// - `1` / `true` / `yes` / `on` -> [`SYSCALL_TIMELINE_DEFAULT_COMMS`], the historical behavior
/// - anything else -> a comma-separated list of `comm` names, e.g. `mate-session,marco,caja`
pub fn init_syscall_timeline(value: impl FnOnce() -> Option<String>) {
    if SYSCALL_TIMELINE_INIT.load(Ordering::Acquire) {
        return;
    }
    let comms: Vec<String> = match value() {
        None => Vec::new(),
        Some(v) if matches!(v.trim(), "1" | "true" | "yes" | "on") => SYSCALL_TIMELINE_DEFAULT_COMMS
            .iter()
            .map(|s| String::from(*s))
            .collect(),
        Some(v) => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
    };
    SYSCALL_TIMELINE_ENABLED.store(!comms.is_empty(), Ordering::Release);
    *SYSCALL_TIMELINE_COMMS.lock() = comms;
    SYSCALL_TIMELINE_INIT.store(true, Ordering::Release);
}

/// Emit one already-formatted timeline line straight to the guest's stderr.
///
/// # Why not `litebox_util_log::error!`
///
/// Because that is gated on a SECOND, unrelated environment variable. The timeline's lines went
/// through the `log` macros, which reach the runner's `tracing` subscriber -- and that subscriber
/// is installed with `.with_env_var("LITEBOX_LOG")`, so with `LITEBOX_LOG` unset the level filter
/// discards every line before it is written. The result is the worst possible failure mode for an
/// instrument: `LITEBOX_DIAG_SYSCALL_TIMELINE=mate-session` is accepted, the filter matches, the
/// emit site runs -- and nothing appears. Silence from a diagnostic reads as "the thing I am
/// looking for did not happen", which is precisely the wrong conclusion, and it cost a full
/// webtop boot to find out otherwise.
///
/// A diagnostic gated by its own variable must not depend on an unrelated one to produce output.
/// The rest of this codebase already settled that: `diag_raw_print_proc_sys_open_miss` and
/// `diag_raw_print_dev_open_miss` both write to stderr through
/// [`litebox::platform::StdioProvider::write_to`], which is why THEIR output shows up
/// unconditionally. This is the same path.
///
/// Unlike those two, this one is not allocation-free -- the caller has already used `format!` to
/// build the line, and unlike an open-path miss this only ever runs from ordinary syscall
/// dispatch, where allocation is fine.
pub fn emit_timeline_line<Platform: litebox::platform::StdioProvider>(
    platform: &Platform,
    line: &str,
) {
    let _ = platform.write_to(litebox::platform::StdioOutStream::Stderr, line.as_bytes());
    let _ = platform.write_to(litebox::platform::StdioOutStream::Stderr, b"
");
}

pub fn syscall_timeline_enabled() -> bool {
    SYSCALL_TIMELINE_ENABLED.load(Ordering::Acquire)
}

/// Whether `comm` (the raw, NUL-padded `[u8; 16]`-shaped process name, as read from
/// `Task::comm`) is one the timeline is aimed at.
///
/// A prefix match, because real Linux truncates `comm` to 15 bytes + NUL and this project's own
/// `comm` field mirrors that -- so a target name longer than 15 bytes is still matched on the
/// bytes that survive the truncation, rather than silently never matching.
pub fn is_syscall_timeline_target_comm(comm: &[u8]) -> bool {
    if !syscall_timeline_enabled() {
        return false;
    }
    let end = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
    let trimmed = &comm[..end];
    SYSCALL_TIMELINE_COMMS
        .lock()
        .iter()
        .any(|target| trimmed == target.as_bytes() || target.as_bytes().starts_with(trimmed))
}

/// Optional narrowing filter for the `litebox_diag::socket_read` payload-preview diagnostic
/// (`syscalls/file.rs`'s two `read()`/AF_UNIX `recvfrom` DIAG sites, 73rd pass). That
/// diagnostic already lives on its own dedicated tracing target (cheap when the target is not
/// enabled at all), but once a caller DOES enable it via `LITEBOX_LOG` -- which is what a real
/// X11/D-Bus wire-capture investigation needs for the whole boot -- it fires for every process
/// that reads a socket fd, not just the one process under investigation. During the
/// desktop-boot fork storm (15-22 concurrent processes, several of them touching X11/D-Bus
/// sockets) that is real, avoidable cost: each firing hex-formats up to 4096 bytes (a
/// multi-KB allocation) and pushes it through the tracing dispatcher, repeated per read, per
/// process.
///
/// 74th pass: same fix shape as [`init_syscall_timeline`]/[`is_syscall_timeline_target_comm`]
/// above (same module, same lazy-latch, same explicit-enumeration-never-wildcard safety
/// property) -- checked BEFORE the hex-formatting work happens, not after, so a caller who
/// only cares about e.g. `xfwm4`'s own reads pays nothing for every other process's socket
/// traffic. Unset (the default) leaves the diagnostic exactly as it was: gated solely by
/// whether `LITEBOX_LOG` enables the `litebox_diag::socket_read` target, for every fd on every
/// process -- this is purely an opt-in narrowing, never a behavior change for an existing
/// capture that does not set the new variable.
static SOCKET_READ_FILTER_ENABLED: AtomicBool = AtomicBool::new(false);
static SOCKET_READ_FILTER_INIT: AtomicBool = AtomicBool::new(false);
static SOCKET_READ_FILTER_COMMS: spin::Mutex<Vec<String>> = spin::Mutex::new(Vec::new());

/// Call once, early, with a closure performing the platform's `env_value` lookup for
/// `LITEBOX_DIAG_SOCKET_READ_TARGET`.
///
/// - unset or empty -> no filter: every process's socket reads are eligible (matches the
///   diagnostic's pre-74th-pass behavior exactly).
/// - anything set -> a comma-separated list of `comm` names (e.g. `xfwm4` or
///   `xfwm4,dbus-daemon`); only a matching process's socket reads emit the payload preview.
pub fn init_socket_read_filter(value: impl FnOnce() -> Option<String>) {
    if SOCKET_READ_FILTER_INIT.load(Ordering::Acquire) {
        return;
    }
    let comms: Vec<String> = match value() {
        None => Vec::new(),
        Some(v) => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
    };
    SOCKET_READ_FILTER_ENABLED.store(!comms.is_empty(), Ordering::Release);
    *SOCKET_READ_FILTER_COMMS.lock() = comms;
    SOCKET_READ_FILTER_INIT.store(true, Ordering::Release);
}

/// Whether `comm` should have its socket-read payload preview emitted. `true` whenever no
/// filter was ever configured (the default, backward-compatible "log everyone" behavior);
/// once a filter IS configured, only a listed `comm` (same prefix-match rule as
/// [`is_syscall_timeline_target_comm`], same reason) passes.
pub fn is_socket_read_target_comm(comm: &[u8]) -> bool {
    if !SOCKET_READ_FILTER_ENABLED.load(Ordering::Acquire) {
        return true;
    }
    let end = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
    let trimmed = &comm[..end];
    SOCKET_READ_FILTER_COMMS
        .lock()
        .iter()
        .any(|target| trimmed == target.as_bytes() || target.as_bytes().starts_with(trimmed))
}

/// Public wrapper around [`syscall_name`] for `lib.rs`'s per-syscall timeline trace (the
/// original is private since it was only ever used inside this module's own summary printer
/// before now).
pub fn syscall_name_pub(number: usize) -> String {
    syscall_name(number)
}

fn syscall_name(number: usize) -> String {
    #[cfg(target_arch = "x86_64")]
    {
        ::syscalls::Sysno::new(number)
            .map(|s| s.name().to_string())
            .unwrap_or_else(|| alloc::format!("sysno_{number}"))
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        alloc::format!("sysno_{number}")
    }
}

/// Prints the accumulated per-syscall summary to stderr via the platform's stdio, if enabled.
/// Called once at runner exit.
pub fn print_strace_summary(mut eprint: impl FnMut(&str)) {
    if !strace_summary_enabled() {
        return;
    }
    let guard = STRACE_SUMMARY.lock();

    eprint("\n=== LITEBOX_STRACE_SUMMARY: per-syscall table ===\n");
    eprint("syscall(number)                     count    total_ns      max_ns  errors\n");
    let mut rows: Vec<(&usize, &SyscallStats)> = guard.by_syscall.iter().collect();
    rows.sort_by(|a, b| b.1.total_ns.cmp(&a.1.total_ns));
    for (num, stats) in rows {
        let name = syscall_name(*num);
        let mut errs = String::new();
        for (errno, count) in &stats.errors {
            if !errs.is_empty() {
                errs.push_str(", ");
            }
            errs.push_str(&alloc::format!("{errno}={count}"));
        }
        eprint(&alloc::format!(
            "{name}({num})  count={count} total_ns={total_ns} max_ns={max_ns} errors=[{errs}]\n",
            count = stats.count,
            total_ns = stats.total_ns,
            max_ns = stats.max_ns,
        ));
    }

    if !guard.unresolved.is_empty() {
        eprint("\n=== LITEBOX_STRACE_SUMMARY: unresolved syscall numbers ===\n");
        for (num, hit) in &guard.unresolved {
            eprint(&alloc::format!(
                "syscall_number={num} count={count} first_caller={comm}[{pid}]\n",
                count = hit.count,
                comm = hit.first_comm,
                pid = hit.first_pid,
            ));
        }
    }

    eprint("\n=== LITEBOX_STRACE_SUMMARY: unsupported sub-commands (ENOSYS/EINVAL/ENOTSUP/EOPNOTSUPP/EPERM) ===\n");
    for (desc, hit) in &guard.unsupported_subcommands {
        eprint(&alloc::format!(
            "{desc} count={count} first_caller={comm}[{pid}]\n",
            count = hit.count,
            comm = hit.first_comm,
            pid = hit.first_pid,
        ));
    }
}

// ---------------------------------------------------------------------------------------------
// Process timeline + tree (always-on; item 3 of the advisor-db diagnostics spec).
// ---------------------------------------------------------------------------------------------

struct ProcessTreeEntry {
    ppid: i32,
    comm: String,
}

static PROCESS_TREE: spin::Mutex<BTreeMap<i32, ProcessTreeEntry>> =
    spin::Mutex::new(BTreeMap::new());
static PROCESS_SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns a monotonically increasing sequence number for ordering timeline events relative to
/// each other (this crate has no `no_std`-safe way to capture a process-start-relative
/// timestamp without threading a `Platform`-generic `Instant` through this module -- see
/// `diag.rs`'s module doc comment; a sequence number gives the same relative-ordering value for
/// the "time since process start" field the spec asks for).
pub fn next_seq() -> u64 {
    PROCESS_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Record (or update) a pid -> (ppid, comm) edge for the exit-time process-tree dump.
pub fn record_process(pid: i32, ppid: i32, comm: &str) {
    let mut guard = PROCESS_TREE.lock();
    guard.insert(
        pid,
        ProcessTreeEntry {
            ppid,
            comm: comm.to_string(),
        },
    );
}

/// Prints the pid -> ppid process tree built from every `record_process` call this run, at
/// runner exit.
pub fn print_process_tree(mut eprint: impl FnMut(&str)) {
    let guard = PROCESS_TREE.lock();
    if guard.is_empty() {
        return;
    }
    eprint("\n=== LITEBOX process tree (pid -> ppid, comm) ===\n");
    for (pid, entry) in guard.iter() {
        eprint(&alloc::format!(
            "pid={pid} ppid={ppid} comm={comm}\n",
            ppid = entry.ppid,
            comm = entry.comm,
        ));
    }
}
