// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

fn main() {
    // Just to make sure cargo sets up the `OUT_DIR` environment variable

    // The FIRST guest program of a run executes directly on this process's own real main
    // thread (see `main.rs`'s `run(CliArgs::parse())`) -- unlike every subsequently-cloned
    // guest thread, which `litebox_platform_windows_userland::spawn_thread` already gives an
    // explicit 8 MiB stack (`GUEST_THREAD_STACK_SIZE`, matching real Linux's default `ulimit
    // -s`) via `std::thread::Builder::stack_size`. The process main thread has no equivalent
    // Rust-level knob -- its real stack size is fixed by the PE header's `SizeOfStackReserve`
    // at LINK time (MSVC default 1 MiB), set here via `/STACK` to the same 8 MiB every other
    // guest thread already gets, for the same reason `GUEST_THREAD_STACK_SIZE` exists: keep an
    // undersized real host stack from ever being a needless bottleneck for guest ELF loading.
    // NOTE: this was NOT the fix for a real stack-overflow crash reproduced this pass (a real
    // static-PIE musl guest client only crashed with `--gui` set) -- that crash was isolated
    // live to a wholly different thread (the GUI presenter's own dedicated thread, spawned in
    // `run()`, never the process main thread), and fixed there instead (see
    // `PRESENTER_THREAD_STACK_SIZE` in `src/lib.rs`). This linker-level bump is kept anyway as
    // a real, independently-justified improvement matching `GUEST_THREAD_STACK_SIZE`'s own
    // rationale, not because it was ever proven load-bearing for that specific crash.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        const GUEST_THREAD_STACK_SIZE: u64 = 8 * 1024 * 1024;
        println!(
            "cargo:rustc-link-arg-bin=litebox_runner_linux_on_windows_userland=/STACK:{GUEST_THREAD_STACK_SIZE}"
        );
    }
}
