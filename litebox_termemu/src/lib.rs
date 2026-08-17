// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Standalone VT100/xterm terminal emulation over a pty-master byte stream.
//!
//! This is the foundational slice of the session-daemon feature described in
//! `docs/session-daemon-design.md`: a pure `bytes -> rendered screen` function, with no
//! process/IPC/daemon machinery attached. It wraps the `vt100` crate (a proven, widely used
//! VT100/xterm parser) rather than re-implementing terminal escape-sequence parsing.

/// A live terminal emulator instance: feed it raw bytes as they arrive from a pty master, and
/// query the current rendered screen or accumulated scrollback at any point.
pub struct TerminalEmulator {
    parser: vt100::Parser,
    history: Vec<u8>,
}

impl TerminalEmulator {
    /// Create a new emulator with the given screen size (rows, cols) and scrollback capacity (in
    /// lines, passed through to `vt100::Parser::new`).
    #[must_use]
    pub fn new(rows: u16, cols: u16, scrollback_lines: usize) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, scrollback_lines),
            history: Vec::new(),
        }
    }

    /// Feed a chunk of bytes read from the pty master. Updates both the live screen state and
    /// the raw scrollback history.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.history.extend_from_slice(bytes);
    }

    /// The current rendered screen as plain text, one line per row, matching what a real
    /// terminal would show right now (accounting for cursor movement, screen clears, and
    /// full-screen-app redraws already processed via `feed`).
    #[must_use]
    pub fn render_screen(&self) -> String {
        String::from_utf8_lossy(&self.parser.screen().contents_formatted()).into_owned()
    }

    /// The current rendered screen as plain text with no ANSI formatting codes.
    #[must_use]
    pub fn render_screen_plain(&self) -> String {
        self.parser.screen().contents()
    }

    /// Current cursor position as (row, col), zero-indexed, matching `vt100::Screen`.
    #[must_use]
    pub fn cursor_position(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    /// The screen's size as (rows, cols), matching what this emulator was constructed with (a
    /// full-screen app's `SIGWINCH`-driven resize is not modeled -- see [`Self::new`]).
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    /// The full raw scrollback: every byte ever fed via `feed`, in order.
    #[must_use]
    pub fn history(&self) -> &[u8] {
        &self.history
    }
}
