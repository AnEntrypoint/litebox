// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Live-execution verification harness (not a test file/suite): spawns the real
//! `litebox_runner_linux_on_windows_userland.exe`, feeds its raw stdout bytes into
//! `litebox_termemu::TerminalEmulator`, and prints the rendered screen + history so a human
//! (or an agent) can eyeball that the output matches what the guest program actually produced.
//! Run: `cargo run -p litebox_termemu --example verify_live -- <runner.exe> <rootfs.tar>`

use std::io::Read as _;
use std::process::{Command, Stdio};

fn main() {
    let mut args = std::env::args().skip(1);
    let runner = args
        .next()
        .expect("usage: verify_live <runner.exe> <rootfs.tar>");
    let rootfs = args
        .next()
        .expect("usage: verify_live <runner.exe> <rootfs.tar>");

    let mut child = Command::new(&runner)
        .args([
            "--initial-files",
            &rootfs,
            "/bin/sh",
            "-c",
            "printf 'a\\033[31mRED\\033[0mb\\r\\nline2'",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn litebox runner");

    let mut raw = Vec::new();
    child
        .stdout
        .take()
        .expect("child stdout not piped")
        .read_to_end(&mut raw)
        .expect("failed to read child stdout");
    let status = child.wait().expect("failed to wait on child");
    assert!(status.success(), "guest process exited with {status:?}");

    println!("--- raw captured bytes ({} bytes) ---", raw.len());
    println!("{}", String::from_utf8_lossy(&raw));

    let mut term = litebox_termemu::TerminalEmulator::new(24, 80, 1000);
    term.feed(&raw);

    println!("--- rendered screen plain (litebox_termemu + vt100) ---");
    println!("{}", term.render_screen_plain());
    println!("--- rendered screen formatted (ANSI-preserving) ---");
    println!("{}", term.render_screen());
    let (row, col) = term.cursor_position();
    println!("--- cursor position: row={row} col={col} ---");
    println!(
        "--- history length matches raw bytes: {} ---",
        term.history() == raw.as_slice()
    );
}
