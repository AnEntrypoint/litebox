//! Isolated cost of the cross-process mutex's acquire/release, and a head-to-head
//! against the obvious alternative design (a named kernel `Mutex` object).
//!
//! Single-process, single-threaded, genuinely uncontended: this measures the fast
//! path only -- the atomic CAS and the atomic swap -- with no critical-section work
//! at all, which the two-process hammer probe cannot isolate.
//!
//! The comparison matters because a named kernel `Mutex` (`CreateMutexW` /
//! `WaitForSingleObject` / `ReleaseMutex`) is the other correct cross-process design,
//! and is far simpler. The question this answers is whether the hybrid's extra
//! complexity actually buys anything measurable.
//!
//! Throwaway diagnostic. Not a crate test.

use std::time::Instant;

use litebox_platform_windows_userland::xproc_sync::{CrossProcessEvent, CrossProcessMutex};

type Handle = *mut core::ffi::c_void;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(sa: *const u8, initial_owner: i32, name: *const u16) -> Handle;
    fn ReleaseMutex(h: Handle) -> i32;
    fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
    fn CloseHandle(h: Handle) -> i32;
    fn GetLastError() -> u32;
}

const INFINITE: u32 = 0xFFFF_FFFF;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

const N: u64 = 2_000_000;

fn main() {
    // --- the hybrid primitive under test -----------------------------------
    let event = CrossProcessEvent::open("Local\\litebox_xpm_bench_event").expect("open event");
    let mutex = CrossProcessMutex::new();

    // Warm up so neither measurement pays first-touch / branch-predictor cost.
    for _ in 0..100_000 {
        mutex.lock(&event).expect("lock").unlock(&event).expect("unlock");
    }

    let t = Instant::now();
    for _ in 0..N {
        let g = mutex.lock(&event).expect("lock");
        g.unlock(&event).expect("unlock");
    }
    let hybrid = t.elapsed();

    // --- the alternative: a genuine named kernel Mutex object ---------------
    let name = wide("Local\\litebox_xpm_bench_kernel_mutex");
    let km = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    assert!(!km.is_null(), "CreateMutexW failed err={}", unsafe {
        GetLastError()
    });

    for _ in 0..100_000 {
        unsafe {
            WaitForSingleObject(km, INFINITE);
            ReleaseMutex(km);
        }
    }

    let t = Instant::now();
    for _ in 0..N {
        unsafe {
            WaitForSingleObject(km, INFINITE);
            ReleaseMutex(km);
        }
    }
    let kernel = t.elapsed();

    unsafe {
        CloseHandle(km);
    }

    // --- a std::sync::Mutex reference point (in-process only, for scale) ----
    let sm = std::sync::Mutex::new(0u64);
    for _ in 0..100_000 {
        drop(sm.lock().unwrap());
    }
    let t = Instant::now();
    for _ in 0..N {
        drop(sm.lock().unwrap());
    }
    let stdm = t.elapsed();

    let ns = |d: std::time::Duration| d.as_secs_f64() * 1e9 / N as f64;

    println!("uncontended acquire+release, {N} iterations, single thread:\n");
    println!(
        "  CrossProcessMutex (hybrid)   : {:7.1} ns/pair",
        ns(hybrid)
    );
    println!(
        "  named kernel Mutex object    : {:7.1} ns/pair  ({:.1}x slower)",
        ns(kernel),
        ns(kernel) / ns(hybrid)
    );
    println!(
        "  std::sync::Mutex (in-proc)   : {:7.1} ns/pair  (reference, NOT cross-process)",
        ns(stdm)
    );
}
