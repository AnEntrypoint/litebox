// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Per-session daemon state: one real litebox guest process (spawned with `--pty-mode`) per
//! session, its pty master byte stream drained into a `litebox_termemu::TerminalEmulator`, per
//! `docs/session-daemon-design.md`'s "Daemon" section.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub struct Session {
    pub program: String,
    child: Child,
    stdin: Mutex<ChildStdin>,
    /// Live rendered-screen state and full scrollback, updated by this session's reader thread as
    /// bytes arrive from the guest's pty-master-over-stdout stream (see
    /// `docs/session-daemon-design.md`'s "Exposing the guest's pty master to the host process" --
    /// option 1: the daemon spawns the guest with `--pty-mode` and treats its real stdout/stdin
    /// pipes as the pty master).
    emulator: Arc<Mutex<litebox_termemu::TerminalEmulator>>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn send_input(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.stdin.lock().unwrap().write_all(bytes)?;
        self.stdin.lock().unwrap().flush()
    }

    pub fn screen(&self) -> (u16, u16, (u16, u16), String) {
        let emu = self.emulator.lock().unwrap();
        let (rows, cols) = emu.size();
        let cursor = emu.cursor_position();
        (rows, cols, cursor, emu.render_screen_plain())
    }

    pub fn history(&self, since: Option<u64>) -> (Vec<u8>, u64) {
        let emu = self.emulator.lock().unwrap();
        let full = emu.history();
        let cursor = full.len() as u64;
        let start = usize::try_from(since.unwrap_or(0).min(cursor)).unwrap_or(usize::MAX);
        (full[start..].to_vec(), cursor)
    }

    pub fn kill(&mut self) -> bool {
        self.child.kill().is_ok()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        if let Some(t) = self.reader_thread.take() {
            let _ = t.join();
        }
    }
}

pub struct Registry {
    sessions: Mutex<HashMap<String, Session>>,
    next_id: AtomicU64,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Spawns a real litebox guest process (`runner_exe --pty-mode --initial-files <rootfs>
    /// <program> [args...]`) with piped stdio, treats its stdout as the pty master's output byte
    /// stream and its stdin as the pty master's input, and starts a background thread draining
    /// that stdout into a fresh `TerminalEmulator`. Reuses exactly the anonymous-pipe stdio
    /// plumbing `std::process::Command::stdout/stdin(Stdio::piped())` already provides (the same
    /// shape `litebox_platform_windows_userland::process_fork`'s own `CreatePipe`-based spawning
    /// produces at the Win32 level) -- no new spawn primitive needed for this leg, matching the
    /// design doc's "option 1" recommendation.
    pub fn create_session(
        &self,
        runner_exe: &str,
        rootfs: &str,
        program: &str,
        args: &[String],
    ) -> std::io::Result<String> {
        let mut child = Command::new(runner_exe)
            .arg("--pty-mode")
            .arg("--initial-files")
            .arg(rootfs)
            .arg(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let mut stdout = child.stdout.take().expect("stdout was piped");

        let emulator = Arc::new(Mutex::new(litebox_termemu::TerminalEmulator::new(
            24, 80, 10_000,
        )));
        let emulator_for_reader = emulator.clone();
        let reader_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        emulator_for_reader.lock().unwrap().feed(&buf[..n]);
                    }
                }
            }
        });

        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        self.sessions.lock().unwrap().insert(
            id.clone(),
            Session {
                program: program.to_string(),
                child,
                stdin: Mutex::new(stdin),
                emulator,
                reader_thread: Some(reader_thread),
            },
        );
        Ok(id)
    }

    pub fn send_input(&self, id: &str, bytes: &[u8]) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|s| s.send_input(bytes).is_ok())
    }

    pub fn screen(&self, id: &str) -> Option<(u16, u16, (u16, u16), String)> {
        self.sessions.lock().unwrap().get(id).map(Session::screen)
    }

    pub fn history(&self, id: &str, since: Option<u64>) -> Option<(Vec<u8>, u64)> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .map(|s| s.history(since))
    }

    pub fn list(&self) -> Vec<(String, String, bool)> {
        self.sessions
            .lock()
            .unwrap()
            .iter_mut()
            .map(|(id, s)| (id.clone(), s.program.clone(), s.is_alive()))
            .collect()
    }

    pub fn kill(&self, id: &str) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .get_mut(id)
            .is_some_and(Session::kill)
    }
}
