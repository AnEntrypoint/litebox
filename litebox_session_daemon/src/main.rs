// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Standalone daemon binary. Usage: `litebox_session_daemon <path-to-runner-exe>`.

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn main() {
    let runner_exe = std::env::args().nth(1).expect(
        "usage: litebox_session_daemon <path-to-litebox_runner_linux_on_windows_userland.exe>",
    );
    litebox_session_daemon::run_daemon(runner_exe);
}

#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn main() {
    eprintln!("This program is only supported on Windows x86_64");
    std::process::exit(1);
}
