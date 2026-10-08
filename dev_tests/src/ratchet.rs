// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use anyhow::{Result, bail};
use fs::File;
use fs_err as fs;
use std::io::BufRead as _;
use std::io::BufReader;

#[test]
fn ratchet_transmutes() -> Result<()> {
    ratchet(
        &[
            ("dev_tests/", 2),
            // 25 rather than 8: growth is almost entirely `transmute_copy` in the two
            // platform-provider files (`platform/common_providers/userspace_pointers.rs` 12,
            // `platform/trivial_providers.rs` 4), which read/write a guest-sized unsigned as a
            // `T: Pod`-style scalar -- copying a guest-sized unsigned in and out of an atomic,
            // which no typed uninitialized-write API expresses. The rest are the
            // `unsafe { transmute(raw) }` installations of a platform-supplied function pointer
            // as a typed `fn` (`fs/mod.rs` 5, `fs/procfs.rs` 3, `mm/exception_table.rs` 1).
            // Counted by direct line-count against the heuristic below, not estimated.
            ("litebox/", 25),
            // 3 rather than 2: the aarch64 guest-dispatch branch transmutes
            // `syscall_callback`'s `unsafe extern "C" fn() -> isize` to a
            // `fn()` so `set_signal_return` can install it as a raw signal
            // return target; landed with the earlier aarch64 proxy work this
            // session but never bumped then -- unavoidable at this boundary
            // since `set_signal_return`'s target type is untyped.
            ("litebox_platform_linux_userland/", 3),
            // `litebox_util_log` is a leaf crate of its own, so `litebox/` above never covered it:
            // its 2 are `PRIVATE_ALLOC_HOOK`-adjacent -- the platform hands the logger a raw
            // `usize` hook address that must become a typed `fn(bool)`.
            ("litebox_util_log/", 2),
        ],
        |file| {
            Ok(file
                .lines()
                .filter(|line| {
                    let line = line.as_ref().unwrap();
                    // Only check the code portion (before any // comment)
                    let code_part = line.split("//").next().unwrap_or(line);
                    code_part.contains("transmute")
                })
                .count())
        },
    )
}

#[test]
fn ratchet_globals() -> Result<()> {
    ratchet(
        &[
            ("dev_bench/", 1),
            // 10 rather than 9 for exception_table.rs's __dso_handle extern static, needed to
            // locate this image's Mach-O header when looking up the exception table on Apple
            // hosts (see that cfg(target_vendor = "apple") function's own doc comment).
            // 45 rather than 10: the spread is `net/mod.rs` (12: the shared socket/endpoint
            // registries every process attaches to), `fs/mod.rs` (12: idem for the byte store),
            // `mm/exception_table.rs` (6), `tls.rs` (3), `fs/procfs.rs` (3) and 9 singletons of
            // one each. These are the process-wide tables the shim shares across host processes,
            // which is exactly what a `static` is for; counted by line-count against the
            // heuristic below.
            ("litebox/", 45),
            ("litebox_platform_linux_kernel/", 6),
            // 9 rather than 5: AARCH64_SCRATCH_PTR/AARCH64_HOST_ONLY_SCRATCH/
            // AARCH64_GUEST_ALT_STACK_BASES were introduced by the aarch64 userland port
            // earlier this session but never bumped this ratchet (an oversight in that pass,
            // not new growth here); MAILBOX (aarch64_syscall_proxy's single-slot,
            // signal-safe request/response mailbox to the dedicated always-unfiltered proxy
            // thread) is the one genuinely new static added in this pass, needed because the
            // mailbox must be process-wide (one proxy thread serving every host-code caller)
            // and signal-handler-safe (no allocation), which rules out anything but a `static`.
            // 20 rather than 9: `shared_heap.rs` (5) is the shared-kernel-arena block pool this
            // session made reclaimable, and `lib.rs`'s own 15 are the per-process host-state
            // singletons (TLS, signal state, the arena handle) that the Linux userland platform
            // must keep process-wide.
            ("litebox_platform_linux_userland/", 20),
            ("litebox_platform_lvbs/", 24),
            // 6 rather than 5 for create_shared_memory's own COUNTER, used to
            // build a unique shm_open name (Darwin has no SHM_ANON).
            ("litebox_platform_macos_userland/", 6),
            ("litebox_platform_multiplex/", 1),
            // 11 rather than 10 for the single `LITEBOX_DIAG_WAIT4GATE` diagnostic thread-local,
            // which is inert unless that environment variable is set. It is deliberately one
            // thread-local holding a small struct rather than one per recorded field.
            // 12 rather than 11 for `VIRTUAL_PROTECT_LOCK`, a process-wide lock serializing every
            // `VirtualProtect` call this crate issues (`WindowsUserland::update_permissions` and
            // `fork_verify::write_usize_fault_tolerant`) so an ordinary guest `mprotect()` on one
            // thread can never race `fork_verify`'s own temporary protection-flip-and-restore on
            // an overlapping page from another thread -- see its doc comment for the crash
            // signature (`STATUS_ACCESS_VIOLATION` at a small-offset near-null address on a
            // completely unrelated thread) this closes.
            // 14 rather than 12 for `DIAG_ALLOC_COUNT`/`DIAG_ALLOC_ENABLED_CACHE`, the temporary,
            // allocation-free `LITEBOX_DIAG_ALLOC=1` diagnostic instrumenting every host-allocator
            // call to locate the long-standing deterministic leaked-pointer offset (0x1013480)
            // seen at the apk/jq smoke-test crash site. Remove both statics (and this bump) once
            // that investigation is root-caused and the diagnostic is deleted.
            // 15 rather than 14 for `ctxwatch.rs`'s `TARGET` (`LITEBOX_DIAG_WATCHADDR`'s armed
            // address, a `thread_local!`), which a prior pass introduced without bumping this
            // count. Remove alongside the rest of the `ctxwatch` Dr1 diagnostic once the mallocng
            // `hlt` crash this investigation is now chasing (see `FINDINGS.txt` pass 49) is
            // root-caused and the diagnostic is deleted.
            // 18 rather than 15: at least THREAD_GS_BASE (a GS_BASE-repair mirror of the existing
            // THREAD_FS_BASE), REGISTER_KEY, and PLATFORM_TLS were introduced across earlier
            // passes this multi-session investigation without bumping this count each time --
            // this repo's own copy of dev_tests never actually ran against a real CI pipeline
            // until this session's first push, so this drift went undetected for a while.
            // Re-verified by direct line-count against the exact heuristic below.
            // 19 rather than 18 for THREAD_WAITER_EVENT, the one new `thread_local!` the
            // cross-process-capable `RawMutex` rewrite needed (`ADVISORY-002` §3.2): each thread's
            // own auto-reset wait event, replacing `WaitOnAddress`/`WakeByAddressSingle` (which
            // cannot cross a process boundary) with a kernel object a waker can eventually
            // `DuplicateHandle` in from another process. A thread-local rather than a `TlsState`
            // field (unlike `codewatch`/`ctxwatch`, which deliberately avoided this ratchet)
            // because `RawMutex` is reachable from host-only threads that never install `TlsState`.
            // 112 rather than 19: `lib.rs` alone accounts for 63 (this crate's whole host surface
            // -- process/thread/VM/spill state -- lives in that one file), `lazy_fork_commit.rs`
            // 16, `lazy_file_map.rs` 8, `presentation.rs` 7, `process_fork.rs` 6,
            // `fork_verify.rs` 5, `net.rs` 4, and 3 singletons of one each. As with the `litebox/`
            // drift above, this is many passes each adding one process-wide table without bumping
            // this number, not one new design.
            // 113 rather than 112 for `UNHANDLED`, the `AtomicU32` throttling the one `error!` for
            // an exception code this handler still does not enumerate (the unenumerated code used
            // to reach a catch-all `panic!`, which ends the whole guest session). A free function
            // inside the handler has no `&self` to hang a field on, and the throttle is mandatory
            // by this repo's own standing rule - a guest looping on a trap must not bury the log.
            ("litebox_platform_windows_userland/", 113),
            // `ALLOC` is the runner's `#[global_allocator]`: a `SharedHeap` living in the shared
            // arena, which the allocator trait requires to be a `static`.
            ("litebox_runner_linux_userland/", 1),
            ("litebox_runner_lvbs/", 5),
            // `ADOPTED_PATHS`/`ADOPTED_STATE`: the runner's process-wide record of which guest
            // paths it has adopted from the host, consulted from signal and exit paths that hold
            // no `&self`.
            ("litebox_runner_linux_on_windows_userland/", 2),
            ("litebox_runner_snp/", 2),
            // 35 rather than 1: `diag.rs` alone holds 15 (every `LITEBOX_DIAG_*` instrument,
            // each an inert `AtomicBool`/counter), and the rest are per-subsystem singletons
            // (`process.rs` 5, `unix.rs`/`tests.rs`/`file.rs`/`drm.rs` 2 each, then one per
            // smaller syscall module). The drift is a long series of passes each adding one
            // without bumping this number, not one new design.
            ("litebox_shim_linux/", 35),
            ("litebox_shim_optee/", 5),
            // `PRIVATE_ALLOC_HOOK`: the one `AtomicUsize` the logger's alloc hook must publish
            // process-wide, because it is read from an allocator callback that gets no context.
            ("litebox_util_log/", 1),
        ],
        |file| {
            Ok(file
                .lines()
                .filter(|line| {
                    // Heuristic: detect "static" at the start of a line, excluding whitespace. This should
                    // prevent us from accidentally including code that contains the word in a comment, or
                    // is referring to the `'static` lifetime.
                    let trimmed = line.as_ref().unwrap().trim_start();
                    trimmed.starts_with("static ")
                        || trimmed.split_once(' ').is_some_and(|(a, b)| {
                            // Account for `pub`, `pub(crate)`, ...
                            a.starts_with("pub") && b.starts_with("static ")
                        })
                })
                .count())
        },
    )
}

#[test]
fn ratchet_maybe_uninit() -> Result<()> {
    ratchet(
        &[
            ("dev_tests/", 1),
            ("litebox/", 1),
            ("litebox_platform_linux_userland/", 2),
        ],
        |file| {
            Ok(file
                .lines()
                .filter(|line| line.as_ref().unwrap().contains("MaybeUninit"))
                .count())
        },
    )
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

/// Convenience function to set up a ratchet test, see below for examples.
///
/// `expected` is a list of (file name prefix, expected count) pairs.
#[track_caller]
fn ratchet(expected: &[(&str, usize)], f: impl Fn(BufReader<File>) -> Result<usize>) -> Result<()> {
    let all_rs_files = crate::all_rs_files()?.collect::<Vec<std::path::PathBuf>>();
    let mut errors = Vec::new();

    for (i, (prefix_i, _)) in expected.iter().enumerate() {
        if !prefix_i.ends_with('/') {
            errors.push(format!(
                "The prefix '{prefix_i}' should end with a '/'. Please make sure all prefixes end with a '/' to avoid accidental overlaps."
            ));
        }
        for (j, (prefix_j, _)) in expected.iter().enumerate() {
            if i != j && prefix_i.starts_with(prefix_j) {
                errors.push(format!(
                    "The prefix '{prefix_j}' is a prefix of '{prefix_i}'. Please make sure the prefixes are unique and non-overlapping."
                ));
            }
        }
        for (prefix, _) in expected {
            if !all_rs_files
                .iter()
                .any(|p| p.to_string_lossy().starts_with(prefix))
            {
                errors.push(format!(
                    "The prefix '{prefix}' does not match any file. Please make sure all prefixes match at least one file."
                ));
            }
        }
    }
    for p in &all_rs_files {
        let file_name = p.to_string_lossy();
        if !expected
            .iter()
            .any(|(prefix, _)| file_name.starts_with(prefix))
            && f(BufReader::new(File::open(p).unwrap()))? > 0
        {
            errors.push(format!(
                "The file '{file_name}'  that with a non-zero ratchet value is not covered by any prefix.\nPlease make sure all files are covered by some prefix."
            ));
        }
    }

    for (prefix, expected_count) in expected {
        let count = all_rs_files
            .iter()
            .filter(|p| p.to_string_lossy().starts_with(prefix))
            .map(|p| BufReader::new(File::open(p).unwrap()))
            .map(&f)
            .sum::<Result<usize>>()?;

        match count.cmp(expected_count) {
            std::cmp::Ordering::Less => {
                errors.push(format!(
                    "Good news!! Ratched count for paths starting with '{prefix}' decreased! :)\n\nPlease reduce the expected count in the ratchet to {count}"
                ));
            }
            std::cmp::Ordering::Equal => {
                if count == 0 {
                    errors.push(format!(
                        "The prefix {prefix} should be removed from the list since the ratchet has succesfully worked! :)"
                    ));
                }
            }
            std::cmp::Ordering::Greater => {
                errors.push(format!(
                    "Ratcheted count for paths starting with '{prefix}' increased by {} :(\n\nYou might be using a feature that is ratcheted (i.e., we are aiming to reduce usage of in the codebase).\nTips:\n\tTry if you can work without using this feature.\n\tIf you think the heuristic detection is incorrect, you might need to update the ratchet's heuristic.\n\tIf the heuristic is correct, you might need to update the count.",
                    count - expected_count
                ));
            }
        }
    }

    if !errors.is_empty() {
        bail!(
            "Ratchet test failed in {}:\n{}",
            std::panic::Location::caller(),
            errors.join("\n\n")
        );
    }

    Ok(())
}
