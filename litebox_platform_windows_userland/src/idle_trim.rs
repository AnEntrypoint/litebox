//! Hands an idle host process's physical pages back to Windows.
//!
//! A desktop session is dozens of host processes, most of them asleep (daemons, panels, idle
//! terminals). Their private pages stay in each working set until Windows is short of memory, and
//! by then the whole machine is already thrashing. Emptying the working set of a process that
//! burned almost no CPU lets the pages move to the standby/modified lists at once, so they count
//! as available memory and come back with a cheap soft fault if the process wakes up again.

use std::sync::Once;
use std::time::Duration;

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::ProcessStatus::{
    EmptyWorkingSet, GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

const CHECK_INTERVAL: Duration = Duration::from_secs(4);
const IDLE_CPU_PERCENT_TENTHS: u64 = 60;
const MIN_WORKING_SET_BYTES: usize = 6 << 20;
const FILETIME_UNITS_PER_SECOND: u64 = 10_000_000;

static START: Once = Once::new();

fn cpu_units() -> u64 {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: querying the calling process's own times.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    let join =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    join(kernel) + join(user)
}

fn working_set_bytes() -> usize {
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { core::mem::zeroed() };
    counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: `counters` is a valid, correctly sized out structure.
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    counters.WorkingSetSize
}

pub(crate) fn start() {
    if std::env::var_os("LITEBOX_IDLE_TRIM").is_some_and(|value| value == "0") {
        return;
    }
    START.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("litebox-idle-trim".to_owned())
            .spawn(|| {
                let mut last = cpu_units();
                loop {
                    std::thread::sleep(CHECK_INTERVAL);
                    let now = cpu_units();
                    let used = now.saturating_sub(last);
                    last = now;
                    let budget = CHECK_INTERVAL.as_secs()
                        * FILETIME_UNITS_PER_SECOND
                        * IDLE_CPU_PERCENT_TENTHS
                        / 1000;
                    if std::env::var_os("LITEBOX_DIAG_IDLE_TRIM").is_some() {
                        eprintln!(
                            "[idle_trim] pid={} used={used} budget={budget} ws={}",
                            std::process::id(),
                            working_set_bytes()
                        );
                    }
                    if used <= budget && working_set_bytes() >= MIN_WORKING_SET_BYTES {
                        // SAFETY: trimming the calling process's own working set is always valid.
                        unsafe { EmptyWorkingSet(GetCurrentProcess()) };
                        last = cpu_units();
                    }
                }
            });
    });
}
