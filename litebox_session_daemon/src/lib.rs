// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Session daemon: a persistent Windows process exposing `CreateSession`/`SendInput`/
//! `GetScreen`/`GetHistory`/`ListSessions`/`KillSession` over a named pipe, per
//! `docs/session-daemon-design.md`. Phase 2 of that design doc's phased plan: the daemon process
//! and its IPC surface. CLI subcommands (`session start`/`send`/`screen`/...) are phase 3+ and not
//! implemented here -- see `examples/session_client.rs` for a direct test client exercising this
//! phase's daemon over the same wire protocol a future CLI would use.

#![cfg(all(target_os = "windows", target_arch = "x86_64"))]
// Every `Mutex::lock().unwrap()` in this crate can only panic on a poisoned mutex (another thread
// already panicked while holding the lock) -- an unrecoverable-process-state condition where
// documenting "may panic" on every accessor would be pure noise, not a caller-actionable contract.
#![allow(clippy::missing_panics_doc)]

pub mod pipe_io;
pub mod protocol;
pub mod session;

use std::sync::Arc;

use protocol::{Request, Response, SessionSummary};
use session::Registry;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE, PIPE_WAIT,
};

/// Wraps a raw pipe `HANDLE`, closing it on drop -- the daemon's per-connection lifetime.
struct PipeHandle(HANDLE);
impl Drop for PipeHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid, open handle for the duration of this `PipeHandle`'s
        // lifetime (its only construction sites are right after a successful create/connect).
        unsafe {
            CloseHandle(self.0);
        }
    }
}
// SAFETY: a Windows named-pipe instance HANDLE has no thread-affinity requirement -- ReadFile/
// WriteFile/CloseHandle are all valid from any thread, matching this crate's one-connection-per-
// client-thread usage (each accepted connection is handled entirely on its own spawned thread,
// never shared across threads concurrently).
unsafe impl Send for PipeHandle {}

fn utf16_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Creates one named-pipe server instance and blocks until a client connects to it.
fn create_and_accept_one_instance() -> std::io::Result<PipeHandle> {
    let name = utf16_nul(protocol::PIPE_NAME);
    // SAFETY: `name` is a valid, NUL-terminated UTF-16 string alive for the duration of this call.
    // All other arguments are plain values with no aliasing/lifetime requirements.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
            windows_sys::Win32::System::Pipes::PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            0,
            core::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // SAFETY: `GetLastError` has no preconditions.
        return Err(std::io::Error::from_raw_os_error(unsafe {
            GetLastError().cast_signed()
        }));
    }
    // This server uses synchronous (blocking) `ReadFile`/`WriteFile` per connection despite
    // `FILE_FLAG_OVERLAPPED` on the handle -- `FILE_FLAG_OVERLAPPED` is set only so
    // `ConnectNamedPipe` below can be interrupted/observed the same way any other overlapped I/O
    // on this handle could be in a future revision; this phase does not use overlapped
    // `ReadFile`/`WriteFile` (see `pipe_io.rs`, which passes a null `OVERLAPPED` pointer -- valid
    // for a synchronous, blocking call even on a handle opened with `FILE_FLAG_OVERLAPPED`, since
    // the flag only changes I/O *semantics* the caller opts into via a non-null `OVERLAPPED`).
    // SAFETY: `handle` was just created above and is a valid pipe server instance handle.
    let ok = unsafe { ConnectNamedPipe(handle, core::ptr::null_mut()) };
    if ok == 0 {
        // SAFETY: `GetLastError` has no preconditions.
        let err = unsafe { GetLastError() };
        if err != ERROR_PIPE_CONNECTED {
            // SAFETY: `handle` is a valid, still-owned handle that hasn't been given to a
            // `PipeHandle` yet.
            unsafe {
                CloseHandle(handle);
            }
            return Err(std::io::Error::from_raw_os_error(err.cast_signed()));
        }
    }
    Ok(PipeHandle(handle))
}

fn handle_connection(handle: PipeHandle, registry: Arc<Registry>, runner_exe: Arc<String>) {
    loop {
        let req: Request = match pipe_io::read_message(handle.0) {
            Ok(Some(r)) => r,
            // Client disconnected cleanly between messages, or a real I/O error -- either way,
            // this connection is done.
            Ok(None) | Err(_) => return,
        };
        let resp = dispatch(&registry, &runner_exe, req);
        if pipe_io::write_message(handle.0, &resp).is_err() {
            return;
        }
    }
}

fn dispatch(registry: &Registry, runner_exe: &str, req: Request) -> Response {
    match req {
        Request::CreateSession {
            rootfs,
            program,
            args,
        } => match registry.create_session(runner_exe, &rootfs, &program, &args) {
            Ok(session_id) => Response::CreateSession { session_id },
            Err(e) => Response::Error {
                message: format!("failed to create session: {e}"),
            },
        },
        Request::SendInput { session_id, bytes } => Response::SendInput {
            ok: registry.send_input(&session_id, &bytes),
        },
        Request::GetScreen { session_id } => match registry.screen(&session_id) {
            Some((rows, cols, cursor, text)) => Response::GetScreen {
                rows,
                cols,
                cursor,
                text,
            },
            None => Response::Error {
                message: format!("no such session: {session_id}"),
            },
        },
        Request::GetHistory { session_id, since } => match registry.history(&session_id, since) {
            Some((bytes, cursor)) => Response::GetHistory { bytes, cursor },
            None => Response::Error {
                message: format!("no such session: {session_id}"),
            },
        },
        Request::ListSessions => Response::ListSessions {
            sessions: registry
                .list()
                .into_iter()
                .map(|(id, program, alive)| SessionSummary { id, program, alive })
                .collect(),
        },
        Request::KillSession { session_id } => Response::KillSession {
            ok: registry.kill(&session_id),
        },
    }
}

/// Runs the daemon's accept loop forever: one thread per connected client, matching the design
/// doc's "one connection per CLI invocation" client shape (a short-lived client connects, sends
/// one request, reads one response, disconnects -- though this phase 2 loop also happily serves a
/// longer-lived test client that sends several requests over one connection, per `read_message`'s
/// natural per-message loop in `handle_connection`).
///
/// `runner_exe` is the path to `litebox_runner_linux_on_windows_userland.exe` used to spawn every
/// session's guest process.
pub fn run_daemon(runner_exe: String) -> ! {
    let registry = Arc::new(Registry::new());
    let runner_exe = Arc::new(runner_exe);
    loop {
        match create_and_accept_one_instance() {
            Ok(handle) => {
                let registry = registry.clone();
                let runner_exe = runner_exe.clone();
                std::thread::spawn(move || handle_connection(handle, registry, runner_exe));
            }
            Err(e) => {
                eprintln!("[litebox_session_daemon] accept failed: {e}, retrying");
            }
        }
    }
}
