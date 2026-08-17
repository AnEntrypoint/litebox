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

/// A raw pipe `HANDLE`, wrapped only so it can cross the `std::thread::spawn` closure in
/// [`DaemonClient::call_with_timeout`] below -- `HANDLE` is a plain `*mut c_void`-shaped integer
/// with no thread affinity for `ReadFile`/`WriteFile` (matching `lib.rs`'s own `PipeHandle`,
/// which asserts the identical `Send` fact for the daemon's server side of the same kind of
/// handle), so this is sound for the same reason.
struct SendableHandle(HANDLE);
// SAFETY: see the struct doc comment -- `ReadFile`/`WriteFile` have no thread-affinity
// requirement, and this handle is used from exactly one thread at a time (the spawned worker,
// synchronized back to the caller via the `mpsc` channel below).
unsafe impl Send for SendableHandle {}

/// Default timeout for a fast round-trip request (`SendInput`/`GetScreen`/`GetHistory`/
/// `ListSessions`/`KillSession`) -- these only ever touch already-live in-memory daemon state, so
/// a slow reply past a few seconds means the daemon itself (or the specific session's reader
/// thread holding a lock this request needs) is genuinely wedged, not merely busy.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for `CreateSession`, which spawns a real guest process (ELF load, rootfs extraction)
/// before the daemon can reply -- given more headroom than
/// [`DEFAULT_CALL_TIMEOUT`] so an ordinarily-slow-but-healthy spawn under load doesn't get
/// mistaken for a hang.
pub const CREATE_SESSION_CALL_TIMEOUT: Duration = Duration::from_secs(30);

impl DaemonClient {
    /// Sends one request and reads back the matching response, matching
    /// `examples/session_client.rs`'s existing `call` helper. Uses [`DEFAULT_CALL_TIMEOUT`] --
    /// callers issuing a [`Request::CreateSession`] should use
    /// [`Self::call_with_timeout`] with [`CREATE_SESSION_CALL_TIMEOUT`] instead, since that
    /// request genuinely needs more time for a healthy reply.
    ///
    /// # Errors
    ///
    /// Returns an error on any pipe I/O failure, if the daemon closes the connection instead of
    /// replying, or if no reply arrives within the timeout (see [`Self::call_with_timeout`]).
    pub fn call(&self, req: &Request) -> std::io::Result<Response> {
        self.call_with_timeout(req, DEFAULT_CALL_TIMEOUT)
    }

    /// Sends one request and reads back the matching response, failing with a
    /// [`std::io::ErrorKind::TimedOut`] error if no reply arrives within `timeout`.
    ///
    /// Why this exists: the underlying pipe I/O (`pipe_io::write_message`/`read_message`) uses
    /// synchronous, unbounded `ReadFile`/`WriteFile` (no `FILE_FLAG_OVERLAPPED` completion on this
    /// handle, matching this crate's deliberately-simple raw-Win32-API style elsewhere) -- if the
    /// daemon is hung (deadlocked, crashed mid-response after writing a partial length prefix, or
    /// blocked because the specific session this request targets is itself wedged in a way that
    /// makes handling THIS request slow), a bare `call` blocks the CLI process forever with no way
    /// out. This directly matters for agent safety: an agent driving `litebox_runner_...
    /// session <subcommand>` in a sandbox must never be able to hang indefinitely regardless of
    /// daemon/session state.
    ///
    /// Implementation: runs the blocking `write_message`+`read_message` round-trip on a background
    /// thread and waits on it via `mpsc::Receiver::recv_timeout`. On timeout, the request thread
    /// is deliberately leaked (not joined) rather than force-killed -- Windows has no safe
    /// mid-`ReadFile` cancellation for a handle used this way, and leaking one thread on the rare
    /// timeout path is a strictly better outcome than the caller hanging forever; the pipe
    /// `HANDLE` itself is closed once by `DaemonClient::drop` regardless of which side (the
    /// leaked thread or the timeout path) "wins," since only one of them ever observes it (the
    /// leaked thread either eventually completes and its result is silently dropped by the
    /// disconnected receiver, or blocks forever on a handle whose owning `DaemonClient` has since
    /// been dropped -- see the field's own closing-on-drop `Drop for DaemonClient` impl, which
    /// still fires normally since it doesn't depend on this thread).
    ///
    /// # Errors
    ///
    /// Returns an error on any pipe I/O failure, if the daemon closes the connection instead of
    /// replying, or a [`std::io::ErrorKind::TimedOut`] error (naming the timeout and suggesting
    /// the session may need to be killed) if no reply arrives in time.
    pub fn call_with_timeout(&self, req: &Request, timeout: Duration) -> std::io::Result<Response> {
        let handle = SendableHandle(self.0);
        let req = req.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let handle = handle;
            let result = crate::pipe_io::write_message(handle.0, &req).and_then(|()| {
                crate::pipe_io::read_message(handle.0)?.ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "daemon closed the connection unexpectedly",
                    )
                })
            });
            // A closed receiver (the timeout path already fired and `rx` was dropped) makes this
            // `send` fail -- that's expected and fine to ignore, matching this call's own "leak
            // the thread, drop its late result" contract described above.
            let _ = tx.send(result);
        });
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(
                std::sync::mpsc::RecvTimeoutError::Timeout
                | std::sync::mpsc::RecvTimeoutError::Disconnected,
            ) => {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "session daemon did not respond within {timeout:?}; it may be \
                         unresponsive -- consider `session kill <id>` for the session this \
                         request targeted, or `session list` to check overall daemon health"
                    ),
                ))
            }
        }
    }
}
