// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Reply grammar, `docs/presenter-process-design.md` section 3.1/3.2. First token is `ok` or
//! `err`; an `err` reply's second token is a short machine-stable error code, remaining tokens a
//! human-readable detail. Some replies (`ps`, `strace dump`) are followed by `n` additional plain
//! text lines, per each command's own description -- [`MultiLine`] carries those.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    BadState,
    NotFound,
    Unsupported,
    IoError,
}

impl ErrorCode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadState => "bad_state",
            Self::NotFound => "not_found",
            Self::Unsupported => "unsupported",
            Self::IoError => "io_error",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bad_state" => Some(Self::BadState),
            "not_found" => Some(Self::NotFound),
            "unsupported" => Some(Self::Unsupported),
            "io_error" => Some(Self::IoError),
            _ => None,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One reply: either the single `ok`/`err ...` line most commands produce, or that line plus a
/// fixed number of unstructured follow-up text lines (`ps`, `strace dump`) -- the count is always
/// stated in the first line (`ok <n>`) so a reader knows exactly how many more lines to pull off
/// the connection before the next reply/command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Ok,
    OkTokens(Vec<String>),
    OkLines(Vec<String>),
    Err { code: ErrorCode, detail: String },
}

impl Reply {
    #[must_use]
    pub fn err(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self::Err {
            code,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn ok_tokens(tokens: Vec<String>) -> Self {
        Self::OkTokens(tokens)
    }

    /// Renders this reply as the sequence of lines to write to the pipe, in order: the header
    /// line first, then (for [`Self::OkLines`]) each follow-up line, with the header itself
    /// already carrying the follow-up count as its last-or-only numeric token per each command's
    /// own grammar (callers build that count into the tokens they pass to
    /// [`Self::ok_tokens`]/construct [`Self::OkLines`] with).
    #[must_use]
    pub fn to_lines(&self) -> Vec<String> {
        match self {
            Self::Ok => vec!["ok".to_owned()],
            Self::OkTokens(tokens) => {
                let mut line = String::from("ok");
                for t in tokens {
                    line.push(' ');
                    line.push_str(t);
                }
                vec![line]
            }
            Self::OkLines(lines) => {
                let mut out = Vec::with_capacity(lines.len() + 1);
                out.push(format!("ok {}", lines.len()));
                out.extend(lines.iter().cloned());
                out
            }
            Self::Err { code, detail } => {
                if detail.is_empty() {
                    vec![format!("err {code}")]
                } else {
                    vec![format!("err {code} {detail}")]
                }
            }
        }
    }

    /// Parses a header line (`ok ...` / `err ...`) into its status and raw token list. Does not
    /// consume any follow-up lines -- callers that expect a multi-line reply (`ps`, `strace dump`)
    /// read `n` from the first token and pull that many more lines themselves via
    /// [`crate::pipe::LineReader`].
    ///
    /// # Errors
    ///
    /// Returns an error string if `line` starts with neither `ok` nor `err`.
    pub fn parse_header(line: &str) -> Result<ReplyHeader, String> {
        let mut tokens = line.trim().split_whitespace();
        match tokens.next() {
            Some("ok") => Ok(ReplyHeader::Ok {
                tokens: tokens.map(str::to_owned).collect(),
            }),
            Some("err") => {
                let code_tok = tokens.next().unwrap_or("io_error");
                let code = ErrorCode::parse(code_tok).unwrap_or(ErrorCode::IoError);
                let detail = tokens.collect::<Vec<_>>().join(" ");
                Ok(ReplyHeader::Err { code, detail })
            }
            _ => Err(format!("malformed reply line: {line:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyHeader {
    Ok { tokens: Vec<String> },
    Err { code: ErrorCode, detail: String },
}

/// `presenter?`'s reply payload -- `ok none` / `ok hidden` / `ok visible`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenterState {
    None,
    Hidden,
    Visible,
}

impl PresenterState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Hidden => "hidden",
            Self::Visible => "visible",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "hidden" => Some(Self::Hidden),
            "visible" => Some(Self::Visible),
            _ => None,
        }
    }
}

/// `scanout`'s reply payload, decoded from an `ok`
/// `<pixel_section_handle> <header_section_handle> <w> <h> <pitch> <format> <offset> <seq>` line.
/// Both handle fields are numeric values valid ONLY in the caller's own process (section 2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanoutReply {
    pub pixel_section_handle: u64,
    pub header_section_handle: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub format: u32,
    pub offset: u64,
    pub seq: u64,
}

impl ScanoutReply {
    #[must_use]
    pub fn to_tokens(self) -> Vec<String> {
        vec![
            self.pixel_section_handle.to_string(),
            self.header_section_handle.to_string(),
            self.width.to_string(),
            self.height.to_string(),
            self.pitch.to_string(),
            self.format.to_string(),
            self.offset.to_string(),
            self.seq.to_string(),
        ]
    }

    #[must_use]
    pub fn parse(tokens: &[String]) -> Option<Self> {
        if tokens.len() != 8 {
            return None;
        }
        Some(Self {
            pixel_section_handle: tokens[0].parse().ok()?,
            header_section_handle: tokens[1].parse().ok()?,
            width: tokens[2].parse().ok()?,
            height: tokens[3].parse().ok()?,
            pitch: tokens[4].parse().ok()?,
            format: tokens[5].parse().ok()?,
            offset: tokens[6].parse().ok()?,
            seq: tokens[7].parse().ok()?,
        })
    }
}
