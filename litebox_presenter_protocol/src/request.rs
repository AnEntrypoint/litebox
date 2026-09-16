// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Command grammar, `docs/presenter-process-design.md` section 3.2. One line, space-separated
//! tokens, first token is the command name.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Request/refresh the shared framebuffer mapping.
    Scanout,
    /// Write the current scanout content to a file on the runner's own filesystem.
    Screenshot { path: String },
    Show,
    Hide,
    /// `presenter?` -- query current presenter state (none/hidden/visible).
    PresenterQuery,
    /// `key <evdev_code> <0|1|2>` (0=release, 1=press, 2=repeat).
    Key { code: u16, value: u8 },
    /// `rel <evdev_code> <i32>` (motion/wheel, REL_X/REL_Y/REL_WHEEL).
    Rel { code: u16, value: i32 },
    /// `relmotion <dx:i32> <dy:i32>` -- one 2D cursor movement as a SINGLE evdev report
    /// (`REL_X`, `REL_Y`, one `SYN_REPORT`), matching what real hardware emits for one physical
    /// motion. A presenter MUST send this instead of two `Rel` requests for cursor movement: two
    /// separate `rel` lines each get their own `SYN_REPORT` on the runner side (`push_input_rel`
    /// is called once per line), making a client process the same motion twice -- exactly the
    /// duplicate-`SYN_REPORT` bug class already fixed once for the in-process presentation path
    /// (`litebox_shim_linux::push_input_rel_motion`'s own doc comment) and reintroduced by the
    /// presenter/runner process split until this variant existed.
    RelMotion { dx: i32, dy: i32 },
    /// `abs <evdev_code> <i32>` -- reserved, always replies `err unsupported` today.
    Abs { code: u16, value: i32 },
    /// List guest processes.
    Ps,
    StraceOn,
    StraceOff,
    StraceQuery,
    /// Trigger `print_strace_summary` immediately and stream it back.
    StraceDump,
    /// `frames on <dir>`.
    FramesOn { dir: String },
    FramesOff,
    /// Internal, presenter -> runner unprompted, once after its first successful `scanout` call
    /// and first successful window creation (section 5 risk 3's readiness signal for a blocking
    /// `show`). Never sent by a user-facing caller.
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed request line: {}", self.0)
    }
}
impl std::error::Error for ParseError {}

fn bad(detail: impl Into<String>) -> ParseError {
    ParseError(detail.into())
}

impl Request {
    /// # Errors
    ///
    /// Returns [`ParseError`] if `line` is not a recognized command or has the wrong number/shape
    /// of arguments.
    pub fn parse(line: &str) -> Result<Self, ParseError> {
        let mut tokens = line.trim().split_whitespace();
        let cmd = tokens.next().ok_or_else(|| bad("empty line"))?;
        match cmd {
            "scanout" => Ok(Self::Scanout),
            "screenshot" => {
                let path = tokens.next().ok_or_else(|| bad("screenshot: missing path"))?;
                Ok(Self::Screenshot {
                    path: path.to_owned(),
                })
            }
            "show" => Ok(Self::Show),
            "hide" => Ok(Self::Hide),
            "presenter?" => Ok(Self::PresenterQuery),
            "key" => {
                let code = parse_tok(tokens.next(), "key: missing code")?;
                let value = parse_tok(tokens.next(), "key: missing value")?;
                Ok(Self::Key { code, value })
            }
            "rel" => {
                let code = parse_tok(tokens.next(), "rel: missing code")?;
                let value = parse_tok(tokens.next(), "rel: missing value")?;
                Ok(Self::Rel { code, value })
            }
            "relmotion" => {
                let dx = parse_tok(tokens.next(), "relmotion: missing dx")?;
                let dy = parse_tok(tokens.next(), "relmotion: missing dy")?;
                Ok(Self::RelMotion { dx, dy })
            }
            "abs" => {
                let code = parse_tok(tokens.next(), "abs: missing code")?;
                let value = parse_tok(tokens.next(), "abs: missing value")?;
                Ok(Self::Abs { code, value })
            }
            "ps" => Ok(Self::Ps),
            "strace" => match tokens.next() {
                Some("on") => Ok(Self::StraceOn),
                Some("off") => Ok(Self::StraceOff),
                Some("query") => Ok(Self::StraceQuery),
                Some("dump") => Ok(Self::StraceDump),
                other => Err(bad(format!("strace: unknown sub-command {other:?}"))),
            },
            "frames" => match tokens.next() {
                Some("on") => {
                    let dir = tokens.next().ok_or_else(|| bad("frames on: missing dir"))?;
                    Ok(Self::FramesOn {
                        dir: dir.to_owned(),
                    })
                }
                Some("off") => Ok(Self::FramesOff),
                other => Err(bad(format!("frames: unknown sub-command {other:?}"))),
            },
            "ready" => Ok(Self::Ready),
            other => Err(bad(format!("unknown command {other:?}"))),
        }
    }

    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            Self::Scanout => "scanout".to_owned(),
            Self::Screenshot { path } => format!("screenshot {path}"),
            Self::Show => "show".to_owned(),
            Self::Hide => "hide".to_owned(),
            Self::PresenterQuery => "presenter?".to_owned(),
            Self::Key { code, value } => format!("key {code} {value}"),
            Self::Rel { code, value } => format!("rel {code} {value}"),
            Self::RelMotion { dx, dy } => format!("relmotion {dx} {dy}"),
            Self::Abs { code, value } => format!("abs {code} {value}"),
            Self::Ps => "ps".to_owned(),
            Self::StraceOn => "strace on".to_owned(),
            Self::StraceOff => "strace off".to_owned(),
            Self::StraceQuery => "strace query".to_owned(),
            Self::StraceDump => "strace dump".to_owned(),
            Self::FramesOn { dir } => format!("frames on {dir}"),
            Self::FramesOff => "frames off".to_owned(),
            Self::Ready => "ready".to_owned(),
        }
    }
}

fn parse_tok<T: std::str::FromStr>(tok: Option<&str>, ctx: &str) -> Result<T, ParseError> {
    tok.ok_or_else(|| bad(ctx))?
        .parse()
        .map_err(|_| bad(format!("{ctx}: not a valid number")))
}
