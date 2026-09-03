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
    /// `Mutex`-wrapped (not plain `&mut`-accessed) because every `Session` in the registry is
    /// held behind an `Arc` (see `Registry::sessions`' doc comment for why): a caller that needs
    /// `&mut Child` (`is_alive`/`kill`) gets it by locking this instead of requiring exclusive
    /// ownership of the whole `Session`.
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    /// Set when the most recent [`Self::send_input`] call's last byte was a bare `Esc` (`0x1b`)
    /// with nothing after it in that same call -- see [`Self::send_input`]'s doc comment for why
    /// this matters: a SEPARATE, immediately-following `session send` call (a distinct CLI
    /// invocation, a distinct `SendInput` IPC round-trip) hits the identical vi
    /// escape-sequence-disambiguation window a same-call trailing `Esc` does, just split across
    /// two calls instead of one write buffer. Live-measured gap between two such consecutive real
    /// `session send` CLI invocations: ~24ms -- far under the ~100ms window that must elapse
    /// before it's safe to send more bytes, so this flag lets the NEXT call pay that delay itself
    /// rather than requiring the caller to know to add it.
    last_write_ended_with_bare_esc: std::sync::atomic::AtomicBool,
    /// Live rendered-screen state and full scrollback, updated by this session's reader thread as
    /// bytes arrive from the guest's pty-master-over-stdout stream (see
    /// `docs/session-daemon-design.md`'s "Exposing the guest's pty master to the host process" --
    /// option 1: the daemon spawns the guest with `--pty-mode` and treats its real stdout/stdin
    /// pipes as the pty master).
    emulator: Arc<Mutex<litebox_termemu::TerminalEmulator>>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    pub fn is_alive(&self) -> bool {
        matches!(self.child.lock().unwrap().try_wait(), Ok(None))
    }

    /// Writes `bytes` to the session's pty input, splitting the write at every `Esc` (`0x1b`)
    /// byte that is followed by more bytes (in this call, or -- via
    /// `last_write_ended_with_bare_esc` -- carried over from the immediately preceding call),
    /// with a short pause after each split point.
    ///
    /// Why: busybox `vi` (and many other readline/vi-alike raw-mode consumers) cannot tell "the
    /// user pressed Escape alone" from "the user pressed the first byte of an escape sequence
    /// (arrow key, etc.)" without waiting a short interval to see whether more bytes follow --
    /// this is the same ambiguity every real terminal-attached program resolves via an
    /// `ESCDELAY`-style timeout (traditionally tens of milliseconds). A human typing at a real
    /// keyboard naturally produces that gap for free; encoding `<Esc>:wq<Enter>` as one
    /// contiguous byte string in a single `session send` call (this feature's own key-encoding
    /// mini-language, see `keys.rs`) does not, so all of `:wq<CR>` arrives inside vi's
    /// escape-sequence disambiguation window and gets silently swallowed as an unrecognized
    /// sequence instead of being processed as the `Esc` keypress followed by a `:wq` command --
    /// live-reproduced (`PowerShell` `ProcessStartInfo`-driven byte-exact repro against
    /// `--pty-mode` directly, bypassing the daemon entirely) as busybox vi's own
    /// `'?' is not implemented` status-line message, and confirmed via a delay-threshold sweep:
    /// splitting the write with a >=100ms pause after `Esc` reliably avoids it, while <=50ms does
    /// not. 120ms is used here for headroom above the observed 100ms threshold.
    ///
    /// The SAME window is also hit across two SEPARATE `session send` calls (e.g.
    /// `session send <id> "ihello world<Esc>"` immediately followed by
    /// `session send <id> ":wq<Enter>"`, exactly this feature's own definitive `vi` test) -- a
    /// live-measured real CLI-to-CLI gap of ~24ms between two such consecutive invocations is
    /// still far under the ~100ms threshold. `last_write_ended_with_bare_esc` carries that fact
    /// across the call boundary so this call pays the same guard delay before writing anything,
    /// without requiring the CALLER to know to add it (an agent driving this via repeated CLI
    /// invocations has no way to control the inter-call gap precisely).
    ///
    /// This was previously misdiagnosed as specific to the combined `:wq<Enter>` vi command; it
    /// is not `:wq`-specific at all -- any bytes following an `Esc` (same call or the next one)
    /// hit the same window, `:wq<Enter>` merely being the design doc's own definitive end-to-end
    /// test and therefore the case that surfaced it.
    pub fn send_input(&self, bytes: &[u8]) -> std::io::Result<()> {
        const ESC: u8 = 0x1b;
        const POST_ESC_DELAY: std::time::Duration = std::time::Duration::from_millis(120);

        let mut stdin = self.stdin.lock().unwrap();

        if !bytes.is_empty()
            && self
                .last_write_ended_with_bare_esc
                .swap(false, Ordering::Relaxed)
        {
            std::thread::sleep(POST_ESC_DELAY);
        }

        let mut start = 0;
        for (i, &b) in bytes.iter().enumerate() {
            if b == ESC && i + 1 < bytes.len() {
                stdin.write_all(&bytes[start..=i])?;
                stdin.flush()?;
                std::thread::sleep(POST_ESC_DELAY);
                start = i + 1;
            }
        }
        stdin.write_all(&bytes[start..])?;
        stdin.flush()?;

        self.last_write_ended_with_bare_esc
            .store(bytes.last() == Some(&ESC), Ordering::Relaxed);
        Ok(())
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

    pub fn kill(&self) -> bool {
        self.child.lock().unwrap().kill().is_ok()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.lock().unwrap().kill();
        if let Some(t) = self.reader_thread.take() {
            let _ = t.join();
        }
    }
}

pub struct Registry {
    /// Each session is `Arc`-wrapped so every accessor below can clone the `Arc` while holding
    /// this map's lock only long enough to look the id up, then drop the map lock before calling
    /// into the session itself -- crucial for anything that can block for a nontrivial duration
    /// (`Session::send_input`'s post-`Esc` pacing delay, see its own doc comment; a slow/wedged
    /// guest process's pipe write blocking on a full OS pipe buffer). Without this, EVERY
    /// session's `list`/`send`/`screen`/`history`/`kill` would stall for as long as the map lock
    /// is held by one in-progress call touching a single, possibly-unrelated session -- a
    /// daemon-wide head-of-line-blocking bug that would defeat this crate's own per-session
    /// isolation design (`docs/session-daemon-design.md`'s "Process model" section) at the
    /// registry layer even though each session's underlying guest process is already isolated.
    sessions: Mutex<HashMap<String, Arc<Session>>>,
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
            Arc::new(Session {
                program: program.to_string(),
                child: Mutex::new(child),
                stdin: Mutex::new(stdin),
                last_write_ended_with_bare_esc: std::sync::atomic::AtomicBool::new(false),
                emulator,
                reader_thread: Some(reader_thread),
            }),
        );
        Ok(id)
    }

    /// Clones the `id`'d session's `Arc` under the map lock and immediately drops the lock --
    /// every accessor below builds on this so a slow/blocking call against one session (see
    /// `sessions`' own doc comment) never stalls any other session's concurrent request.
    fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn send_input(&self, id: &str, bytes: &[u8]) -> bool {
        self.get(id).is_some_and(|s| s.send_input(bytes).is_ok())
    }

    pub fn screen(&self, id: &str) -> Option<(u16, u16, (u16, u16), String)> {
        self.get(id).map(|s| s.screen())
    }

    pub fn history(&self, id: &str, since: Option<u64>) -> Option<(Vec<u8>, u64)> {
        self.get(id).map(|s| s.history(since))
    }

    pub fn list(&self) -> Vec<(String, String, bool)> {
        let snapshot: Vec<(String, Arc<Session>)> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect();
        snapshot
            .into_iter()
            .map(|(id, s)| {
                let alive = s.is_alive();
                (id, s.program.clone(), alive)
            })
            .collect()
    }

    pub fn kill(&self, id: &str) -> bool {
        self.get(id).is_some_and(|s| s.kill())
    }
}
