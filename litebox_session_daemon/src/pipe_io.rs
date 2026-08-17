// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Minimal length-prefixed-JSON framing over a raw Windows named-pipe `HANDLE`, shared by both
//! the daemon's server loop and any client (the CLI, or a direct test client such as
//! `examples/session_client.rs`). Uses `ReadFile`/`WriteFile` directly, matching this workspace's
//! existing raw-Win32-API style for pipe I/O (see `litebox_platform_windows_userland::process_fork`'s
//! `CreatePipe`-based helpers).

use std::io;

use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};

/// Reads exactly `buf.len()` bytes from `handle`, looping over partial `ReadFile` completions.
/// Returns `Ok(false)` on a clean pipe-closed EOF hit at a message boundary (zero bytes read on
/// the very first `ReadFile` of this call), `Err` for any other I/O failure or a short/truncated
/// message (peer closed mid-message).
fn read_exact(handle: HANDLE, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let mut n_read = 0u32;
        // SAFETY: `handle` is a valid, open pipe handle for the duration of this call (guaranteed
        // by every caller in this module), and `buf[filled..]` is a valid, writable slice whose
        // length fits in a `u32` (messages in this protocol are always small).
        let ok = unsafe {
            ReadFile(
                handle,
                buf[filled..].as_mut_ptr(),
                u32::try_from(buf.len() - filled).expect("frame length fits in u32"),
                &raw mut n_read,
                core::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // SAFETY: `GetLastError` has no preconditions.
            let err = unsafe { GetLastError() };
            if filled == 0 && (err == windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE) {
                return Ok(false);
            }
            return Err(io::Error::from_raw_os_error(err.cast_signed()));
        }
        if n_read == 0 {
            if filled == 0 {
                return Ok(false);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "peer closed pipe mid-message",
            ));
        }
        filled += n_read as usize;
    }
    Ok(true)
}

fn write_all(handle: HANDLE, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let mut n_written = 0u32;
        // SAFETY: `handle` is a valid, open pipe handle; `buf` is a valid, readable slice whose
        // length fits in a `u32` (messages in this protocol are always small).
        let ok = unsafe {
            WriteFile(
                handle,
                buf.as_ptr(),
                u32::try_from(buf.len()).expect("frame length fits in u32"),
                &raw mut n_written,
                core::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // SAFETY: `GetLastError` has no preconditions.
            let err = unsafe { GetLastError() };
            return Err(io::Error::from_raw_os_error(err.cast_signed()));
        }
        buf = &buf[n_written as usize..];
    }
    Ok(())
}

/// Writes one length-prefixed JSON frame: a 4-byte little-endian length, then that many bytes of
/// `serde_json`-serialized `msg`.
pub fn write_message<T: serde::Serialize>(handle: HANDLE, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).expect("message always serializes");
    let len = u32::try_from(body.len()).expect("message fits in u32 length prefix");
    write_all(handle, &len.to_le_bytes())?;
    write_all(handle, &body)
}

/// Reads one length-prefixed JSON frame. Returns `Ok(None)` on a clean EOF at a message boundary
/// (the peer disconnected between messages, not mid-message).
pub fn read_message<T: serde::de::DeserializeOwned>(handle: HANDLE) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    if !read_exact(handle, &mut len_buf)? {
        return Ok(None);
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    if !read_exact(handle, &mut body)? {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer closed pipe after length prefix but before message body",
        ));
    }
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
