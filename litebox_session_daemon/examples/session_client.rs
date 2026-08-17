// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Live-execution verification harness (not a test file/suite) for phase 2's session daemon: a
//! direct client that connects to a running daemon's named pipe, exercises `CreateSession`/
//! `SendInput`/`GetScreen`/`GetHistory`/`ListSessions`/`KillSession`, and prints what it got back
//! so a human (or an agent) can confirm the daemon is actually driving real litebox guest
//! processes correctly -- the phase 3+ CLI subcommands are a thin wrapper over exactly this same
//! wire protocol.
//!
//! Run: `cargo run -p litebox_session_daemon --example session_client -- <rootfs.tar>`
//! (the daemon itself must already be running: `cargo run -p litebox_session_daemon --
//! <path-to-litebox_runner_linux_on_windows_userland.exe>`)

// `litebox_session_daemon` itself is `#![cfg(all(target_os = "windows", target_arch =
// "x86_64"))]`-gated (a Windows-only daemon), which makes every item this example imports from it
// vanish entirely on any other target/arch -- CI's Linux-hosted jobs (which build the whole
// workspace with `--all-targets --all-features` to catch exactly this kind of accidental
// cross-platform breakage) would otherwise fail with "could not find `protocol`/`pipe_io` in
// `litebox_session_daemon`". The real implementation lives in `windows_impl`, gated the same way
// the crate gates itself; `main` dispatches to it (or to a stub) so this always links as a binary
// example regardless of host platform/arch.

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows_impl {
    use std::ffi::c_void;
    use std::time::Duration;

    use litebox_session_daemon::protocol::{PIPE_NAME, Request, Response};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_NONE, OPEN_EXISTING};

    fn utf16_nul(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn connect() -> HANDLE {
        let name = utf16_nul(PIPE_NAME);
        for attempt in 0..20 {
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
            if handle != INVALID_HANDLE_VALUE {
                return handle;
            }
            assert!(
                attempt != 19,
                "failed to connect to {PIPE_NAME} after 20 attempts -- is the daemon running?"
            );
            std::thread::sleep(Duration::from_millis(250));
        }
        unreachable!()
    }

    fn call(handle: HANDLE, req: &Request) -> Response {
        litebox_session_daemon::pipe_io::write_message(handle, req).expect("write_message failed");
        litebox_session_daemon::pipe_io::read_message(handle)
            .expect("read_message failed")
            .expect("daemon closed the connection unexpectedly")
    }

    pub fn main() {
        let rootfs = std::env::args()
            .nth(1)
            .expect("usage: session_client <rootfs.tar>");

        let handle = connect();
        println!("--- connected to {PIPE_NAME} ---");

        // Test 1: non-interactive /bin/echo, confirm output is retrievable.
        let Response::CreateSession {
            session_id: echo_id,
        } = call(
            handle,
            &Request::CreateSession {
                rootfs: rootfs.clone(),
                program: "/bin/echo".to_string(),
                args: vec!["hello".to_string()],
            },
        )
        else {
            panic!("CreateSession for /bin/echo did not return CreateSession");
        };
        println!("--- created echo session: {echo_id} ---");
        std::thread::sleep(Duration::from_millis(1500));
        let Response::GetHistory { bytes, .. } = call(
            handle,
            &Request::GetHistory {
                session_id: echo_id.clone(),
                since: None,
            },
        ) else {
            panic!("GetHistory did not return GetHistory");
        };
        let history_text = String::from_utf8_lossy(&bytes);
        println!("--- echo session history ---\n{history_text}");
        assert!(
            history_text.contains("hello"),
            "echo session history must contain 'hello', got: {history_text:?}"
        );
        println!("--- TEST 1 (non-interactive /bin/echo) PASSED ---");

        // Test 2: interactive session via /bin/cat (bare interactive busybox `ash` has a known
        // job-control startup gap tracked in docs/session-daemon-design.md's "Known limitations"
        // section -- `cat` is used here as a stand-in interactive program: still a real
        // pty-attached guest process reading stdin and echoing to stdout live, exercising the
        // exact same SendInput/GetScreen code path a shell session would).
        let Response::CreateSession { session_id: sh_id } = call(
            handle,
            &Request::CreateSession {
                rootfs: rootfs.clone(),
                program: "/bin/cat".to_string(),
                args: vec![],
            },
        ) else {
            panic!("CreateSession for /bin/cat did not return CreateSession");
        };
        println!("--- created cat session: {sh_id} ---");
        std::thread::sleep(Duration::from_millis(500));
        let Response::SendInput { ok } = call(
            handle,
            &Request::SendInput {
                session_id: sh_id.clone(),
                bytes: b"echo test from daemon\n".to_vec(),
            },
        ) else {
            panic!("SendInput did not return SendInput");
        };
        assert!(ok, "SendInput to cat session must succeed");
        std::thread::sleep(Duration::from_secs(1));
        let Response::GetScreen { text, .. } = call(
            handle,
            &Request::GetScreen {
                session_id: sh_id.clone(),
            },
        ) else {
            panic!("GetScreen did not return GetScreen");
        };
        println!("--- cat session screen ---\n{text}");
        assert!(
            text.contains("echo test from daemon"),
            "cat session screen must echo back what was sent, got: {text:?}"
        );
        println!("--- TEST 2 (interactive SendInput + GetScreen via /bin/cat) PASSED ---");

        // Test 3: multiple simultaneous sessions, confirm isolation.
        let Response::CreateSession { session_id: cat_a } = call(
            handle,
            &Request::CreateSession {
                rootfs: rootfs.clone(),
                program: "/bin/cat".to_string(),
                args: vec![],
            },
        ) else {
            panic!("CreateSession for session A did not return CreateSession");
        };
        let Response::CreateSession { session_id: cat_b } = call(
            handle,
            &Request::CreateSession {
                rootfs,
                program: "/bin/cat".to_string(),
                args: vec![],
            },
        ) else {
            panic!("CreateSession for session B did not return CreateSession");
        };
        std::thread::sleep(Duration::from_millis(500));
        let Response::SendInput { ok } = call(
            handle,
            &Request::SendInput {
                session_id: cat_a.clone(),
                bytes: b"unique-marker-for-session-A\n".to_vec(),
            },
        ) else {
            panic!("SendInput to session A did not return SendInput");
        };
        assert!(ok);
        std::thread::sleep(Duration::from_secs(1));
        let Response::GetScreen { text: text_a, .. } = call(
            handle,
            &Request::GetScreen {
                session_id: cat_a.clone(),
            },
        ) else {
            panic!("GetScreen A did not return GetScreen");
        };
        let Response::GetScreen { text: text_b, .. } = call(
            handle,
            &Request::GetScreen {
                session_id: cat_b.clone(),
            },
        ) else {
            panic!("GetScreen B did not return GetScreen");
        };
        println!("--- session A screen ---\n{text_a}");
        println!("--- session B screen ---\n{text_b}");
        assert!(
            text_a.contains("unique-marker-for-session-A"),
            "session A must see its own input"
        );
        assert!(
            !text_b.contains("unique-marker-for-session-A"),
            "session B must NOT see session A's input -- isolation violated"
        );
        println!("--- TEST 3 (multi-session isolation) PASSED ---");

        let Response::ListSessions { sessions } = call(handle, &Request::ListSessions) else {
            panic!("ListSessions did not return ListSessions");
        };
        println!("--- ListSessions: {sessions:?} ---");
        assert!(sessions.len() >= 4, "expected at least 4 tracked sessions");

        for id in [echo_id, sh_id, cat_a, cat_b] {
            let Response::KillSession { ok } = call(
                handle,
                &Request::KillSession {
                    session_id: id.clone(),
                },
            ) else {
                panic!("KillSession did not return KillSession");
            };
            println!("--- killed session {id}: ok={ok} ---");
        }

        // SAFETY: `handle` is a valid, still-open handle owned by this function.
        unsafe {
            CloseHandle(handle);
        }
        println!("--- ALL TESTS PASSED ---");
    }
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn main() {
    windows_impl::main();
}

#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn main() {
    eprintln!("This example is only supported on Windows x86_64");
    std::process::exit(1);
}
