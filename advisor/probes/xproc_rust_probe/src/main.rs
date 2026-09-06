//! Live verification of `litebox_platform_windows_userland::xproc_sync` across TWO
//! REAL, SEPARATE Windows processes.
//!
//! This exercises the actual shipped Rust implementation -- not a reimplementation of
//! the protocol -- against a pagefile-backed section mapped into both processes, with
//! many threads in each hammering one lock-protected counter.
//!
//! Invariants checked:
//!   1. Final counter == procs * threads * iters exactly (no lost update).
//!   2. A critical-section occupancy word is observed as exactly 1 by every holder,
//!      which catches genuine mutual-exclusion violations that a counter-only check
//!      can mask (two racing increments can still total correctly by luck).
//!   3. A plain, NON-atomic read-modify-write through a raw pointer also totals
//!      correctly -- this is the real test, since it is correct only if the mutex
//!      genuinely excludes across the process boundary.
//!   4. No wait ever hangs; a lost wakeup manifests as the run never finishing.
//!
//! Throwaway diagnostic, per the project's no-test-files rule. Not a crate test.

use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use litebox_platform_windows_userland::xproc_sync::{CrossProcessEvent, CrossProcessMutex};

const SEC_NAME: &str = "Local\\litebox_xpm_rust_section";
const EVT_NAME: &str = "Local\\litebox_xpm_rust_event";
const RDY_NAME: &str = "Local\\litebox_xpm_rust_ready";

const SECTION_BYTES: usize = 65536;

/// The process-shared payload. `repr(C)` and POD, so it is interpreted identically in
/// both processes even though the section maps at a different address in each.
#[repr(C)]
struct Shared {
    mutex: CrossProcessMutex,
    counter: AtomicI64,
    occupancy: AtomicU32,
    violations: AtomicU32,
    /// Deliberately non-atomic: correct ONLY if the mutex genuinely excludes.
    plain_counter: i64,
}

// --- minimal Win32 bindings, kept local to the probe ------------------------
type Handle = *mut core::ffi::c_void;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileMappingW(
        h: Handle,
        sa: *const u8,
        prot: u32,
        hi: u32,
        lo: u32,
        name: *const u16,
    ) -> Handle;
    fn OpenFileMappingW(access: u32, inherit: i32, name: *const u16) -> Handle;
    fn MapViewOfFile(map: Handle, access: u32, hi: u32, lo: u32, len: usize)
    -> *mut core::ffi::c_void;
    fn GetLastError() -> u32;
    fn GetCurrentProcessId() -> u32;
}

const PAGE_READWRITE: u32 = 0x04;
const FILE_MAP_ALL_ACCESS: u32 = 0x000F_001F;
const INVALID_HANDLE_VALUE: Handle = usize::MAX as Handle;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn map_shared(is_child: bool) -> *mut Shared {
    let name = wide(SEC_NAME);
    let map = unsafe {
        if is_child {
            OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, name.as_ptr())
        } else {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                SECTION_BYTES as u32,
                name.as_ptr(),
            )
        }
    };
    assert!(!map.is_null(), "file mapping failed err={}", unsafe {
        GetLastError()
    });
    let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, SECTION_BYTES) };
    assert!(!view.is_null(), "MapViewOfFile failed err={}", unsafe {
        GetLastError()
    });
    view.cast::<Shared>()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let is_child = args.get(1).map(String::as_str) == Some("child");

    let threads: usize = std::env::var("XPM_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let iters: usize = std::env::var("XPM_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);

    let shared_ptr = map_shared(is_child);
    println!(
        "{}: section mapped at {:p} (pid {})",
        if is_child { "child " } else { "parent" },
        shared_ptr,
        unsafe { GetCurrentProcessId() }
    );

    // Each process opens the SAME named event for itself. No handle ever crosses the
    // boundary -- the property that makes this design fork-mechanism-agnostic (it
    // works identically under CreateProcessW and under RtlCloneUserProcess).
    let event = CrossProcessEvent::open(EVT_NAME).expect("open event");
    let ready = CrossProcessEvent::open(RDY_NAME).expect("open ready event");

    let shared: &Shared = unsafe { &*shared_ptr };

    let mut child = None;
    if is_child {
        assert!(
            ready.wait_timeout(30_000).expect("ready wait"),
            "ready barrier timed out"
        );
    } else {
        // A fresh pagefile-backed section is zero-filled, and FREE == 0, so the mutex
        // is already validly unlocked. Zero explicitly anyway so a rerun against a
        // section left over from a previous run starts clean.
        unsafe {
            std::ptr::write_bytes(shared_ptr.cast::<u8>(), 0, std::mem::size_of::<Shared>());
        }
        let exe = std::env::current_exe().expect("current_exe");
        child = Some(
            std::process::Command::new(exe)
                .arg("child")
                .spawn()
                .expect("spawn child"),
        );
        std::thread::sleep(Duration::from_millis(400));
        ready.signal().expect("release ready barrier");
    }

    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                for _ in 0..iters {
                    let guard = shared.mutex.lock(&event).expect("lock");

                    if shared.occupancy.fetch_add(1, Ordering::AcqRel) != 0 {
                        shared.violations.fetch_add(1, Ordering::AcqRel);
                    }

                    // Deliberately non-atomic read-modify-write through a raw
                    // pointer: the real test of mutual exclusion.
                    unsafe {
                        let p = std::ptr::addr_of!(shared.plain_counter).cast_mut();
                        let v = std::ptr::read_volatile(p);
                        std::ptr::write_volatile(p, v + 1);
                    }
                    shared.counter.fetch_add(1, Ordering::AcqRel);

                    shared.occupancy.fetch_sub(1, Ordering::AcqRel);
                    guard.unlock(&event).expect("unlock");
                }
            });
        }
    });
    let elapsed = start.elapsed();

    let ops = (threads * iters) as f64;
    println!(
        "{}: {} acquire/release pairs in {:.3}s = {:.0} ns/pair ({threads} threads)",
        if is_child { "child " } else { "parent" },
        threads * iters,
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1e9 / ops,
    );

    if let Some(mut c) = child {
        let status = c.wait().expect("wait child");
        let expect = (2 * threads * iters) as i64;
        let got_atomic = shared.counter.load(Ordering::Acquire);
        let got_plain = unsafe { std::ptr::read_volatile(std::ptr::addr_of!(shared.plain_counter)) };
        let viol = shared.violations.load(Ordering::Acquire);

        println!("\n=== RESULT === (threads={threads} iters={iters})");
        println!("child exit         : {status}");
        println!("atomic counter     : {got_atomic}");
        println!("plain counter      : {got_plain}");
        println!("expected           : {expect}");
        println!("excl violations    : {viol}");

        let ok = status.success() && got_atomic == expect && got_plain == expect && viol == 0;
        println!("VERDICT            : {}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            std::process::exit(1);
        }
    }
}
