// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The input half of the N_TTY line discipline, as a pure state machine.
//!
//! Bytes written to a pty master (what a terminal emulator sends for keystrokes) pass through this
//! before reaching the slave: `ICRNL`/`INLCR`/`IGNCR` mapping, `ISIG` special characters
//! (`^C`/`^\`/`^Z`), canonical-mode line editing (`VERASE`/`VKILL`/`VWERASE`/`VEOF`), and the
//! `ECHO` family. Keeping it free of I/O lets it be tested exhaustively; the pty layer only
//! executes the returned [`Action`]s.

use alloc::vec::Vec;
use litebox_common_linux::Termios;

pub(crate) const IGNCR: u32 = 0o000200;
pub(crate) const ICRNL: u32 = 0o000400;
pub(crate) const INLCR: u32 = 0o000100;
pub(crate) const ISIG: u32 = 0o000001;
pub(crate) const ICANON: u32 = 0o000002;
pub(crate) const ECHO: u32 = 0o000010;
pub(crate) const ECHOE: u32 = 0o000020;
pub(crate) const ECHOK: u32 = 0o000040;
pub(crate) const ECHONL: u32 = 0o000100;
pub(crate) const ECHOCTL: u32 = 0o001000;
pub(crate) const ECHOKE: u32 = 0o004000;
pub(crate) const IEXTEN: u32 = 0o100000;

const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VEOL: usize = 11;
const VWERASE: usize = 14;
const VLNEXT: usize = 15;
const VEOL2: usize = 16;
const VSUSP: usize = 10;

pub(crate) const SIGINT: i32 = 2;
pub(crate) const SIGQUIT: i32 = 3;
pub(crate) const SIGTSTP: i32 = 20;

/// A line is never allowed to grow past this (`N_TTY_BUF_SIZE`).
const MAX_LINE: usize = 4095;

/// What the pty layer must do in response to one input byte.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Bytes for the slave's read side.
    Deliver(Vec<u8>),
    /// Bytes for the master's read side (echo). Already includes any `\r\n` expansion.
    Echo(Vec<u8>),
    /// Raise this signal on the terminal's foreground process group.
    Signal(i32),
    /// `VEOF` typed on an empty line: the slave's next `read` returns 0.
    Eof,
}

/// The default termios of a fresh Linux pty (`tty_std_termios` as adjusted by `pty_init`).
pub(crate) fn default_termios() -> Termios {
    let mut cc = [0u8; 19];
    cc[VINTR] = 0x03;
    cc[VQUIT] = 0x1c;
    cc[VERASE] = 0x7f;
    cc[VKILL] = 0x15;
    cc[VEOF] = 0x04;
    cc[6] = 1; // VMIN
    cc[8] = 0x11; // VSTART
    cc[9] = 0x13; // VSTOP
    cc[VSUSP] = 0x1a;
    cc[12] = 0x12; // VREPRINT
    cc[13] = 0x0f; // VDISCARD
    cc[VWERASE] = 0x17;
    cc[VLNEXT] = 0x16;
    Termios {
        // ICRNL | IXON | IUTF8
        c_iflag: 0o000400 | 0o002000 | 0o040000,
        // OPOST | ONLCR
        c_oflag: 0o000001 | 0o000004,
        // B38400 | CS8 | CREAD | HUPCL
        c_cflag: 0o000017 | 0o000060 | 0o000200 | 0o002000,
        // ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN
        c_lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
        c_line: 0,
        c_cc: cc,
    }
}

#[derive(Default)]
pub(crate) struct LineDiscipline {
    /// The canonical-mode line under construction (not yet visible to the slave).
    line: Vec<u8>,
    /// The next byte is taken literally (`VLNEXT`).
    literal_next: bool,
}

/// Echo one input character the way `ECHOCTL` does: control characters other than `\t`/`\n`
/// become `^X`.
fn echo_char(out: &mut Vec<u8>, b: u8, t: &Termios) {
    if t.c_lflag & ECHOCTL != 0 && (b < 0x20 || b == 0x7f) && b != b'\t' && b != b'\n' {
        out.push(b'^');
        out.push(if b == 0x7f { b'?' } else { b + 0x40 });
    } else {
        out.push(b);
    }
}

/// Columns a byte occupied on screen when echoed.
fn echo_width(b: u8, t: &Termios) -> usize {
    if t.c_lflag & ECHOCTL != 0 && (b < 0x20 || b == 0x7f) && b != b'\t' {
        2
    } else {
        1
    }
}

impl LineDiscipline {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed one byte of terminal input through the discipline.
    pub(crate) fn input(&mut self, mut b: u8, t: &Termios) -> Vec<Action> {
        let mut actions = Vec::new();
        let lflag = t.c_lflag;
        let cc = &t.c_cc;

        if self.literal_next {
            self.literal_next = false;
            self.accept(b, t, &mut actions, true);
            return actions;
        }

        // Input mapping.
        if b == b'\r' {
            if t.c_iflag & IGNCR != 0 {
                return actions;
            }
            if t.c_iflag & ICRNL != 0 {
                b = b'\n';
            }
        } else if b == b'\n' && t.c_iflag & INLCR != 0 {
            b = b'\r';
        }

        // Signal characters.
        if lflag & ISIG != 0 {
            let sig = if b == cc[VINTR] && b != 0 {
                Some(SIGINT)
            } else if b == cc[VQUIT] && b != 0 {
                Some(SIGQUIT)
            } else if b == cc[VSUSP] && b != 0 {
                Some(SIGTSTP)
            } else {
                None
            };
            if let Some(sig) = sig {
                self.line.clear();
                if lflag & ECHO != 0 {
                    let mut e = Vec::new();
                    echo_char(&mut e, b, t);
                    actions.push(Action::Echo(e));
                }
                actions.push(Action::Signal(sig));
                return actions;
            }
        }

        if lflag & IEXTEN != 0 && lflag & ICANON != 0 && b == cc[VLNEXT] && b != 0 {
            self.literal_next = true;
            if lflag & ECHO != 0 {
                actions.push(Action::Echo(alloc::vec![b'^', 0x08]));
            }
            return actions;
        }

        self.accept(b, t, &mut actions, false);
        actions
    }

    fn accept(&mut self, b: u8, t: &Termios, actions: &mut Vec<Action>, literal: bool) {
        let lflag = t.c_lflag;
        let cc = &t.c_cc;

        if lflag & ICANON == 0 {
            actions.push(Action::Deliver(alloc::vec![b]));
            if lflag & ECHO != 0 {
                let mut e = Vec::new();
                if b == b'\n' {
                    e.extend_from_slice(b"\r\n");
                } else {
                    echo_char(&mut e, b, t);
                }
                actions.push(Action::Echo(e));
            }
            return;
        }

        if !literal {
            if b == cc[VERASE] && b != 0 {
                if let Some(gone) = self.line.pop()
                    && lflag & ECHO != 0
                {
                    let mut e = Vec::new();
                    if lflag & ECHOE != 0 {
                        for _ in 0..echo_width(gone, t) {
                            e.extend_from_slice(b"\x08 \x08");
                        }
                    } else {
                        echo_char(&mut e, b, t);
                    }
                    actions.push(Action::Echo(e));
                }
                return;
            }
            if b == cc[VWERASE] && b != 0 && lflag & IEXTEN != 0 {
                let mut e = Vec::new();
                while self.line.last() == Some(&b' ') || self.line.last() == Some(&b'\t') {
                    let gone = self.line.pop().unwrap();
                    for _ in 0..echo_width(gone, t) {
                        e.extend_from_slice(b"\x08 \x08");
                    }
                }
                while let Some(&last) = self.line.last() {
                    if last == b' ' || last == b'\t' {
                        break;
                    }
                    self.line.pop();
                    for _ in 0..echo_width(last, t) {
                        e.extend_from_slice(b"\x08 \x08");
                    }
                }
                if lflag & ECHO != 0 && !e.is_empty() {
                    actions.push(Action::Echo(e));
                }
                return;
            }
            if b == cc[VKILL] && b != 0 {
                let mut e = Vec::new();
                if lflag & ECHOKE != 0 {
                    for gone in self.line.iter() {
                        for _ in 0..echo_width(*gone, t) {
                            e.extend_from_slice(b"\x08 \x08");
                        }
                    }
                } else if lflag & ECHOK != 0 {
                    e.extend_from_slice(b"\r\n");
                }
                self.line.clear();
                if lflag & ECHO != 0 && !e.is_empty() {
                    actions.push(Action::Echo(e));
                }
                return;
            }
            if b == cc[VEOF] && b != 0 {
                if self.line.is_empty() {
                    actions.push(Action::Eof);
                } else {
                    actions.push(Action::Deliver(core::mem::take(&mut self.line)));
                }
                return;
            }
        }

        let is_eol = b == b'\n' || (!literal && ((b == cc[VEOL] && b != 0) || (b == cc[VEOL2] && b != 0)));
        if self.line.len() >= MAX_LINE && !is_eol {
            return;
        }
        self.line.push(b);
        if is_eol {
            actions.push(Action::Deliver(core::mem::take(&mut self.line)));
            if lflag & ECHO != 0 || (b == b'\n' && lflag & ECHONL != 0) {
                actions.push(Action::Echo(if b == b'\n' {
                    alloc::vec![b'\r', b'\n']
                } else {
                    let mut e = Vec::new();
                    echo_char(&mut e, b, t);
                    e
                }));
            }
        } else if lflag & ECHO != 0 {
            let mut e = Vec::new();
            echo_char(&mut e, b, t);
            actions.push(Action::Echo(e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(ld: &mut LineDiscipline, t: &Termios, s: &[u8]) -> Vec<Action> {
        s.iter().flat_map(|&b| ld.input(b, t)).collect()
    }

    #[test]
    fn canonical_line_is_delivered_on_newline_and_echoed() {
        let t = default_termios();
        let mut ld = LineDiscipline::new();
        let a = feed(&mut ld, &t, b"hi\r");
        assert_eq!(
            a,
            alloc::vec![
                Action::Echo(b"h".to_vec()),
                Action::Echo(b"i".to_vec()),
                Action::Deliver(b"hi\n".to_vec()),
                Action::Echo(b"\r\n".to_vec()),
            ]
        );
    }

    #[test]
    fn erase_edits_the_pending_line() {
        let t = default_termios();
        let mut ld = LineDiscipline::new();
        let a = feed(&mut ld, &t, b"ab\x7fc\n");
        assert!(a.contains(&Action::Echo(b"\x08 \x08".to_vec())));
        assert!(a.contains(&Action::Deliver(b"ac\n".to_vec())));
    }

    #[test]
    fn ctrl_c_signals_and_discards_the_line() {
        let t = default_termios();
        let mut ld = LineDiscipline::new();
        let a = feed(&mut ld, &t, b"abc\x03\n");
        assert!(a.contains(&Action::Signal(SIGINT)));
        assert!(a.contains(&Action::Echo(b"^C".to_vec())));
        assert!(a.contains(&Action::Deliver(b"\n".to_vec())));
    }

    #[test]
    fn eof_on_empty_line_is_reported_and_on_partial_line_flushes() {
        let t = default_termios();
        let mut ld = LineDiscipline::new();
        assert_eq!(feed(&mut ld, &t, b"\x04"), alloc::vec![Action::Eof]);
        let a = feed(&mut ld, &t, b"x\x04");
        assert!(a.contains(&Action::Deliver(b"x".to_vec())));
    }

    #[test]
    fn raw_mode_passes_bytes_straight_through() {
        let mut t = default_termios();
        t.c_lflag = 0;
        t.c_iflag = 0;
        let mut ld = LineDiscipline::new();
        assert_eq!(
            feed(&mut ld, &t, b"a\r\x03"),
            alloc::vec![
                Action::Deliver(b"a".to_vec()),
                Action::Deliver(b"\r".to_vec()),
                Action::Deliver(b"\x03".to_vec()),
            ]
        );
    }

    #[test]
    fn readline_style_mode_echoes_nothing_but_still_maps_signals_when_isig() {
        let mut t = default_termios();
        t.c_lflag = ISIG;
        t.c_iflag = 0;
        let mut ld = LineDiscipline::new();
        let a = feed(&mut ld, &t, b"a\x03");
        assert_eq!(
            a,
            alloc::vec![Action::Deliver(b"a".to_vec()), Action::Signal(SIGINT)]
        );
    }
}
