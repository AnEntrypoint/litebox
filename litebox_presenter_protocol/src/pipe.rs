// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Raw Windows named-pipe transport for the newline-delimited text protocol in
//! [`crate::request`]/[`crate::reply`], per `docs/presenter-process-design.md` section 3.1.
//!
//! Deliberately a plain BYTE-mode pipe (not `PIPE_TYPE_MESSAGE`): the wire format is a byte
//! stream delimited by `\n`, not one message per `WriteFile` call, so byte mode is the correct
//! primitive here (unlike `litebox_session_daemon`'s length-prefixed-JSON protocol, which uses
//! message mode). `ReadFile`/`WriteFile` style matches `litebox_session_daemon::pipe_io` and
//! `litebox_platform_windows_userland::process_fork`'s existing raw-Win32-API convention in this
//! workspace.

use std::io;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::CreateEventW;

/// Issues one overlapped Win32 I/O call (`ReadFile`/`WriteFile`/`ConnectNamedPipe`, all of which
/// share the `(HANDLE, ..., *mut OVERLAPPED) -> BOOL` shape) through a FRESH, PRIVATE `OVERLAPPED`
/// structure with its own manual-reset event, and blocks until that specific operation completes
/// via `GetOverlappedResult`.
///
/// # Why this exists (do not go back to a null `OVERLAPPED` on this handle)
///
/// Every pipe handle here is created with `FILE_FLAG_OVERLAPPED`, but an earlier version of this
/// module still passed a NULL `OVERLAPPED` pointer to every `ReadFile`/`WriteFile` call, believing
/// that made them "ordinary blocking I/O" (a belief `litebox_session_daemon`'s own pipe code
/// states too). That is only true when at most one I/O operation is ever in flight on a given
/// pipe INSTANCE at a time. This module's own `show`/`hide` design is exactly the case where it
/// is not: `litebox_runner_linux_on_windows_userland::control_server` keeps one thread blocked in
/// `ReadFile` on a presenter's connection (waiting for rare `key`/`rel` pushes FROM the presenter)
/// indefinitely, while a SEPARATE thread calls `WriteFile` through
/// `duplicate_into_current_process`'s duplicated handle (to push `show`/`hide` TO the presenter)
/// -- two duplicate handles of the SAME underlying pipe object, used concurrently from different
/// threads. Live verification during the presenter-process-split feature (2026-09-16) found two
/// distinct failure modes from this, in order: (1) with `FILE_FLAG_OVERLAPPED` set but a null
/// `OVERLAPPED` on every call, the pending read spuriously observed `ERROR_BROKEN_PIPE` right
/// after the concurrent write succeeded, even though neither side had actually closed anything;
/// (2) after removing `FILE_FLAG_OVERLAPPED` entirely (to make the handle genuinely synchronous),
/// the concurrent write instead hung FOREVER, queued behind the permanently-pending read at the
/// file-object level (a pending synchronous read on a duplicate handle can starve a synchronous
/// write on another duplicate of the same object -- there is no user-visible primitive to avoid
/// that once queued). A private `OVERLAPPED`/event PER CALL is the actual fix: real overlapped
/// I/O explicitly supports multiple simultaneously-pending operations on one pipe object (that is
/// the feature's whole purpose), so the pending read and the concurrent write no longer contend
/// for anything at all.
fn overlapped_call(handle: HANDLE, issue: impl FnOnce(*mut OVERLAPPED) -> i32) -> io::Result<u32> {
    // SAFETY: no name, no security attributes, manual-reset, initially-unset -- all plain values
    // with no aliasing/lifetime requirements.
    let event = unsafe { CreateEventW(core::ptr::null(), 1, 0, core::ptr::null()) };
    if event.is_null() {
        // SAFETY: `GetLastError` has no preconditions.
        return Err(io::Error::from_raw_os_error(unsafe {
            GetLastError().cast_signed()
        }));
    }
    let mut overlapped: OVERLAPPED = unsafe { core::mem::zeroed() };
    overlapped.hEvent = event;
    // SAFETY: `overlapped` is valid and outlives the I/O it issues -- this function does not
    // return until `GetOverlappedResult` below reports that operation complete.
    let immediate = issue(&raw mut overlapped);
    let result = (|| -> io::Result<u32> {
        if immediate == 0 {
            // SAFETY: `GetLastError` has no preconditions.
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(err.cast_signed()));
            }
        }
        let mut transferred = 0u32;
        // SAFETY: `handle` is the same handle passed into `issue`; `overlapped` is the exact
        // structure just passed to it; a nonzero `bWait` blocks until this operation (and no
        // other) completes, per this structure's own private event.
        let ok = unsafe { GetOverlappedResult(handle, &overlapped, &raw mut transferred, 1) };
        if ok == 0 {
            // SAFETY: `GetLastError` has no preconditions.
            return Err(io::Error::from_raw_os_error(unsafe {
                GetLastError().cast_signed()
            }));
        }
        Ok(transferred)
    })();
    // SAFETY: `event` was created fresh above; nothing else references it.
    unsafe {
        CloseHandle(event);
    }
    result
}

/// `\\.\pipe\litebox-<runner-pid>`, exactly `docs/presenter-process-design.md` section 3.1's
/// name (matches Appendix D2's own choice).
#[must_use]
pub fn pipe_name(runner_pid: u32) -> String {
    format!(r"\\.\pipe\litebox-{runner_pid}")
}

fn utf16_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Wraps a raw pipe `HANDLE`, closing it on drop.
pub struct PipeHandle(HANDLE);

impl Drop for PipeHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid, open handle for the duration of this `PipeHandle`'s
        // lifetime -- its only construction sites below hold a just-created/just-connected handle.
        unsafe {
            CloseHandle(self.0);
        }
    }
}
// SAFETY: a Windows named-pipe instance HANDLE has no thread-affinity requirement --
// ReadFile/WriteFile/CloseHandle are all valid from any thread (matches
// `litebox_session_daemon`'s identical `PipeHandle`/`SendableHandle` reasoning).
unsafe impl Send for PipeHandle {}
// SAFETY: sharing a `&PipeHandle` across threads (e.g. behind an `Arc`, as
// `litebox_presenter` does to let one thread read while others write) is sound because the
// underlying Win32 calls have no thread affinity; callers are responsible for their own
// serialization between concurrent writers (a `Mutex` guarding the write path) exactly as they
// would be with any other raw OS handle shared this way -- `Sync` here asserts no ADDITIONAL
// unsafety beyond what `&HANDLE` sharing already implies, not that concurrent unsynchronized
// writes are safe (they are not, for the same reason two threads calling `write()` on the same
// fd with no coordination would interleave).
unsafe impl Sync for PipeHandle {}

impl PipeHandle {
    #[must_use]
    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// Wraps an already-open pipe `HANDLE` this call takes ownership of (it will be closed on
    /// drop). Used when a second, independent handle VALUE for the same underlying pipe instance
    /// has already been produced (e.g. via `DuplicateHandle` within one process), so it can be
    /// stored and closed independently of the original.
    ///
    /// # Safety
    ///
    /// `handle` must be a valid, currently-open handle that nothing else will close or continue
    /// to use after this call.
    #[must_use]
    pub unsafe fn from_raw(handle: HANDLE) -> Self {
        Self(handle)
    }
}

/// Creates one named-pipe server instance and blocks until a client connects to it. One instance
/// per accepted connection, matching `docs/presenter-process-design.md` section 4.2's "one server,
/// potentially multiple clients" shape -- the caller loops calling this again after each
/// connection ends to accept the next one.
///
/// # Errors
///
/// Returns an error if pipe creation or the connect wait fails.
pub fn create_and_accept_one_instance(name: &str) -> io::Result<PipeHandle> {
    let wide = utf16_nul(name);
    // SAFETY: `wide` is a valid, NUL-terminated UTF-16 string alive for this call. All other
    // arguments are plain values with no aliasing/lifetime requirements.
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            0,
            core::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // SAFETY: `GetLastError` has no preconditions.
        return Err(io::Error::from_raw_os_error(unsafe {
            GetLastError().cast_signed()
        }));
    }
    // See `overlapped_call`'s own doc comment for why every I/O call on this handle (including
    // this one) goes through it rather than passing a null `OVERLAPPED` pointer. A client that
    // connected between `CreateNamedPipeW` and this call (`ERROR_PIPE_CONNECTED`) is the one
    // "error" that means success here, exactly as the non-overlapped version of this code treated
    // it.
    if let Err(e) = overlapped_call(handle, |ov| unsafe { ConnectNamedPipe(handle, ov) }) {
        if e.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
            // SAFETY: `handle` is a valid, still-owned handle not yet given to a `PipeHandle`.
            unsafe {
                CloseHandle(handle);
            }
            return Err(e);
        }
    }
    Ok(PipeHandle(handle))
}

/// Server-side: the PID of the process connected to `handle`, per
/// `docs/presenter-process-design.md` section 2.3 step 2 -- learned from the pipe itself with no
/// separate handshake message needed, immediately after accept.
///
/// # Errors
///
/// Returns an error if the underlying `GetNamedPipeClientProcessId` call fails.
pub fn client_process_id(handle: &PipeHandle) -> io::Result<u32> {
    let mut pid: u32 = 0;
    // SAFETY: `handle.raw()` is a valid, connected pipe server instance handle; `pid` is a valid
    // `u32` out-pointer.
    let ok = unsafe { GetNamedPipeClientProcessId(handle.raw(), &raw mut pid) };
    if ok == 0 {
        // SAFETY: `GetLastError` has no preconditions.
        return Err(io::Error::from_raw_os_error(unsafe {
            GetLastError().cast_signed()
        }));
    }
    Ok(pid)
}

/// Client-side: connects to the runner's control pipe, retrying while the server has not yet
/// called `ConnectNamedPipe` (`ERROR_PIPE_BUSY`/`ERROR_FILE_NOT_FOUND`) until `timeout` elapses.
///
/// # Errors
///
/// Returns an error if the pipe never becomes connectable within `timeout`.
pub fn connect_client(name: &str, timeout: Duration) -> io::Result<PipeHandle> {
    let wide = utf16_nul(name);
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: `wide` is a valid, NUL-terminated UTF-16 string alive for this call.
        let handle = unsafe {
            windows_sys::Win32::Storage::FileSystem::CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_NONE,
                core::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                core::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(PipeHandle(handle));
        }
        if Instant::now() >= deadline {
            // SAFETY: `GetLastError` has no preconditions.
            return Err(io::Error::from_raw_os_error(unsafe {
                GetLastError().cast_signed()
            }));
        }
        // SAFETY: `wide` is valid for this call; a null timeout pointer plus `NMPWAIT_USE_DEFAULT_WAIT`-equivalent
        // constant is not used here -- pass a short bounded wait so the outer loop keeps checking `deadline`.
        unsafe {
            WaitNamedPipeW(wide.as_ptr(), 250);
        }
    }
}

/// Buffers partial reads so a caller can pull one `\n`-delimited line at a time from a byte-mode
/// pipe, whose `ReadFile` calls have no message-boundary relationship to the writer's line
/// boundaries.
pub struct LineReader {
    buf: Vec<u8>,
    start: usize,
}

impl LineReader {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(4096),
            start: 0,
        }
    }

    /// Reads and returns the next `\n`-delimited line (the trailing `\n`, and any `\r` right
    /// before it, stripped). Returns `Ok(None)` on a clean pipe-closed EOF with no partial line
    /// pending.
    ///
    /// # Errors
    ///
    /// Returns an error on any pipe I/O failure, or an `UnexpectedEof` if the peer closes
    /// mid-line.
    pub fn read_line(&mut self, handle: &PipeHandle) -> io::Result<Option<String>> {
        loop {
            if let Some(pos) = self.buf[self.start..].iter().position(|&b| b == b'\n') {
                let line_end = self.start + pos;
                let mut line = self.buf[self.start..line_end].to_vec();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.start = line_end + 1;
                if self.start == self.buf.len() {
                    self.buf.clear();
                    self.start = 0;
                }
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            // No full line buffered -- compact then read more.
            if self.start > 0 {
                self.buf.drain(0..self.start);
                self.start = 0;
            }
            let mut chunk = [0u8; 4096];
            // See `overlapped_call`'s own doc comment for why this goes through it rather than
            // passing a null `OVERLAPPED` pointer: this read is often left pending for a long time
            // (waiting for a rare `key`/`rel` push from a presenter, or for `show`/`hide` on the
            // client side) while another thread writes through a duplicate of this same handle,
            // and only a private per-call `OVERLAPPED`/event lets both proceed independently.
            let read_result = overlapped_call(handle.raw(), |ov| {
                // SAFETY: `handle.raw()` is a valid, open pipe handle; `chunk` is a valid writable
                // buffer for the duration of this call, which `overlapped_call` blocks until
                // that call's own `GetOverlappedResult` reports complete.
                unsafe {
                    ReadFile(
                        handle.raw(),
                        chunk.as_mut_ptr(),
                        chunk.len() as u32,
                        core::ptr::null_mut(),
                        ov,
                    )
                }
            });
            let n_read = match read_result {
                Ok(n) => n,
                Err(e) => {
                    if self.buf.is_empty()
                        && e.raw_os_error()
                            == Some(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE as i32)
                    {
                        return Ok(None);
                    }
                    return Err(e);
                }
            };
            if n_read == 0 {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "peer closed pipe mid-line",
                ));
            }
            self.buf.extend_from_slice(&chunk[..n_read as usize]);
        }
    }
}

impl Default for LineReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Writes one line, appending `\n`.
///
/// # Errors
///
/// Returns an error on any pipe I/O failure.
pub fn write_line(handle: &PipeHandle, line: &str) -> io::Result<()> {
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    let mut remaining: &[u8] = &buf;
    while !remaining.is_empty() {
        // See `overlapped_call`'s own doc comment: this write may run concurrently with a long-
        // pending read on a duplicate of this same handle (`show`/`hide` pushed to a presenter
        // while that connection's own reader thread is blocked waiting for a rare `key`/`rel`),
        // so it must go through a private per-call `OVERLAPPED`/event rather than a null pointer.
        let n_written = overlapped_call(handle.raw(), |ov| {
            // SAFETY: `handle.raw()` is a valid, open pipe handle; `remaining` is a valid readable
            // slice for the duration of this call, which `overlapped_call` blocks until that
            // call's own `GetOverlappedResult` reports complete.
            unsafe {
                WriteFile(
                    handle.raw(),
                    remaining.as_ptr(),
                    remaining.len() as u32,
                    core::ptr::null_mut(),
                    ov,
                )
            }
        })?;
        remaining = &remaining[n_written as usize..];
    }
    Ok(())
}
