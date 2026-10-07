// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! `litebox-presenter.exe` -- a TARGET DISPATCHER, nothing more. The real program is [`win`].
//!
//! Why this file is not the program: the presenter owns a Win32 window, `wgpu`/`winit`
//! presentation and a Windows named pipe, and the two crates that give it those
//! (`litebox_presenter_protocol`, and `litebox_platform_windows_userland::presentation`) are
//! themselves `#![cfg(all(target_os = "windows", target_arch = "x86_64"))]` -- an EMPTY crate and
//! an absent module everywhere else. So a `main.rs` that uses them at the top level does not
//! compile on Linux or macOS at all (5 x E0432/E0433), and because `litebox_presenter` sits in
//! the workspace's `default-members`, that one Windows-only binary takes a plain `cargo build`
//! down with it on every other target -- `build`, `clippy --all-targets` and `nextest` alike.
//!
//! Compiling the program only where it could actually run keeps the whole workspace green on
//! Linux, and running the binary there says why rather than being silently missing.

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod win;

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn main() {
    win::main()
}

/// Everywhere else: still a buildable binary, so the workspace builds, but there is nothing for
/// it to present.
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn main() {
    eprintln!(
        "litebox-presenter: this binary is Windows/x86_64-only (Win32 window + wgpu/winit + \
         Windows named pipes); nothing to present on this target"
    );
    std::process::exit(1);
}
