// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Newline-delimited command/reply wire grammar and named-pipe transport shared by
//! `litebox_runner_linux_on_windows_userland`'s `ControlServer` and `litebox-presenter.exe`, per
//! `docs/presenter-process-design.md` section 3. A small, dependency-light crate (only
//! `windows-sys`, matching this workspace's existing convention -- see `litebox_session_daemon`)
//! so either side can depend on just the wire format without pulling in the other's guest-shim or
//! wgpu/winit dependencies.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

pub mod pipe;
pub mod reply;
pub mod request;

pub use pipe::pipe_name;
pub use reply::{ErrorCode, PresenterState, Reply, ReplyHeader, ScanoutReply};
pub use request::{ParseError, Request};
