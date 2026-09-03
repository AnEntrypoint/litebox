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

/// Call once, early (e.g. from the first syscall dispatch), with the platform's `env_flag`
/// result for `LITEBOX_STRACE_SUMMARY`. Idempotent and cheap after the first call.
pub fn init_strace_summary(enabled: bool) {
    if !STRACE_INIT.swap(true, Ordering::AcqRel) {
        STRACE_ENABLED.store(enabled, Ordering::Release);
    }
}

pub fn strace_summary_enabled() -> bool {
    STRACE_ENABLED.load(Ordering::Acquire)
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
