// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! IPC message shapes for the session daemon's named-pipe surface, per
//! `docs/session-daemon-design.md`'s "IPC surface: daemon <-> CLI" section.
//!
//! Wire format: each message (request or response) is a 4-byte little-endian length prefix
//! followed by that many bytes of JSON -- the simplest framing that composes with a byte-stream
//! transport (a Windows named pipe message doesn't reliably preserve write-call boundaries across
//! every client library), matching the design doc's "length-prefixed JSON" choice.

use serde::{Deserialize, Serialize};

pub const PIPE_NAME: &str = r"\\.\pipe\litebox-session-daemon";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    CreateSession {
        /// Path to the tar archive passed as `--initial-files` (same as a normal
        /// `litebox_runner_linux_on_windows_userland` invocation).
        rootfs: String,
        program: String,
        args: Vec<String>,
    },
    SendInput {
        session_id: String,
        bytes: Vec<u8>,
    },
    GetScreen {
        session_id: String,
    },
    GetHistory {
        session_id: String,
        since: Option<u64>,
    },
    ListSessions,
    KillSession {
        session_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    CreateSession {
        session_id: String,
    },
    SendInput {
        ok: bool,
    },
    GetScreen {
        rows: u16,
        cols: u16,
        cursor: (u16, u16),
        text: String,
    },
    GetHistory {
        bytes: Vec<u8>,
        cursor: u64,
    },
    ListSessions {
        sessions: Vec<SessionSummary>,
    },
    KillSession {
        ok: bool,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub program: String,
    pub alive: bool,
}
