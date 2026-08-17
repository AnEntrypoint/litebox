// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Thin named-pipe client shared by every `session` CLI subcommand
//! (`litebox_runner_linux_on_windows_userland session start/send/screen/history/list/kill`), per
//! `docs/session-daemon-design.md`'s "CLI subcommand surface" section. Reuses exactly the same
//! `pipe_io`/`protocol` wire format `examples/session_client.rs` already live-verified in phase
//! 2, refactored here so the CLI subcommands and that example both call through one connect/call
//! implementation instead of duplicating the `CreateFileW`-retry-loop logic.
//!
//! Also owns daemon auto-spawn-on-first-use: a caller that finds no daemon listening spawns one
//! detached (`Command::new(runner_exe).arg("--session-daemon")`, matching this doc's "start once,
//! address by ID forever after" requirement) and retries the connect.

use std::ffi::c_void;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_NONE, OPEN_EXISTING};

use crate::protocol::{PIPE_NAME, Request, Response};

fn utf16_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn try_connect_once() -> Option<HANDLE> {
    let name = utf16_nul(PIPE_NAME);
    // SAFETY: `name` is a valid NUL-terminated UTF-16 string alive for this call.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_NONE,
            core::ptr::null(),
            OPEN_EXISTING,
            0,
            core::ptr::null_mut::<c_void>() as HANDLE,
        )
    };
    (handle != INVALID_HANDLE_VALUE).then_some(handle)
}

/// A connected client handle. Closes the pipe connection on drop.
pub struct DaemonClient(HANDLE);

impl Drop for DaemonClient {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid, open handle for the duration of this `DaemonClient`'s
        // lifetime (its only construction site is a successful connect).
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Connects to the daemon's named pipe, auto-spawning a detached daemon process (via
/// `Command::new(runner_exe).arg("--session-daemon")`) if no daemon is currently listening.
///
/// # Errors
///
/// Returns an error if the daemon fails to spawn, or never starts listening within the retry
/// budget (5s total, matching `examples/session_client.rs`'s existing 20x250ms retry loop).
pub fn connect_or_spawn(runner_exe: &str) -> std::io::Result<DaemonClient> {
    if let Some(h) = try_connect_once() {
        return Ok(DaemonClient(h));
    }
    // No daemon listening -- spawn one detached. `--session-daemon` is handled by this same
    // binary's `main()` before `CliArgs` parsing (see `litebox_runner_linux_on_windows_userland`'s
    // `main.rs`), so `runner_exe` (this process's own `current_exe()`, passed in by every CLI
    // subcommand's caller) is both the daemon binary and the per-session guest binary.
    std::process::Command::new(runner_exe)
        .arg("--session-daemon")
        .arg(runner_exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| std::io::Error::other(format!("failed to auto-spawn session daemon: {e}")))?;

    for attempt in 0..20 {
        if let Some(h) = try_connect_once() {
            return Ok(DaemonClient(h));
        }
        if attempt == 19 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "auto-spawned session daemon but never saw it listening on {PIPE_NAME} \
                     after 20 attempts (5s)"
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    unreachable!()
}

impl DaemonClient {
    /// Sends one request and reads back the matching response, matching
    /// `examples/session_client.rs`'s existing `call` helper.
    ///
    /// # Errors
    ///
    /// Returns an error on any pipe I/O failure, or if the daemon closes the connection instead
    /// of replying.
    pub fn call(&self, req: &Request) -> std::io::Result<Response> {
        crate::pipe_io::write_message(self.0, req)?;
        crate::pipe_io::read_message(self.0)?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed the connection unexpectedly",
            )
        })
    }
}
