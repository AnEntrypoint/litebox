//! Verifies the two deliberate failure behaviours of the primitive actually fire,
//! rather than being untested comments: (a) unlocking a free mutex panics, and
//! (b) dropping a guard while a waiter is blocked panics rather than silently
//! stranding that waiter forever. An unexercised safety net is not a safety net.
//!
//! Throwaway diagnostic. Not a crate test.

use std::sync::atomic::Ordering;

use litebox_platform_windows_userland::xproc_sync::{CrossProcessEvent, CrossProcessMutex};

fn main() {
    let ev = CrossProcessEvent::open(r"Local\litebox_xpm_guardcheck").expect("open");

    // (a) unlocking a mutex that is not held must panic.
    let m = CrossProcessMutex::new();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = m.unlock(&ev);
    }));
    println!("unlock-when-free panics : {}", if r.is_err() { "YES (correct)" } else { "NO  (BUG)" });

    // (b) dropping a guard while the word says CONTENDED must panic, because such a
    // drop cannot signal the event and would strand the waiter.
    let m2 = CrossProcessMutex::new();
    let r2 = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let g = m2.lock(&ev).expect("lock");
        m2.raw_state().store(2, Ordering::Release); // simulate a waiter arriving
        drop(g);
    }));
    println!("drop-with-waiter panics : {}", if r2.is_err() { "YES (correct)" } else { "NO  (BUG)" });

    // (c) the ordinary path must NOT panic.
    let m3 = CrossProcessMutex::new();
    let g = m3.lock(&ev).expect("lock");
    g.unlock(&ev).expect("unlock");
    println!("normal lock/unlock ok   : YES");

    // (d) try_lock exclusion.
    let m4 = CrossProcessMutex::new();
    let g4 = m4.lock(&ev).expect("lock");
    println!("try_lock while held     : {}", if m4.try_lock() { "ACQUIRED (BUG)" } else { "refused (correct)" });
    g4.unlock(&ev).expect("unlock");
    println!("try_lock after release  : {}", if m4.try_lock() { "acquired (correct)" } else { "REFUSED (BUG)" });
    m4.unlock(&ev).expect("unlock");
}
