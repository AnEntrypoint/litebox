// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Key-encoding mini-language for `session send`, per
//! `docs/session-daemon-design.md`'s "Key-encoding mini-language for `session send`" section.
//!
//! Parses a CLI argument string left-to-right into raw bytes to write to a session's pty:
//! anything outside `<...>` is literal UTF-8; a `<Name>` tag decodes to the named control
//! sequence. Platform-independent (no Windows dependency), so it can be exercised from any
//! target, including this workspace's Linux-hosted CI jobs.

/// Decodes a `session send` key-string into the raw bytes to write to the pty.
///
/// # Errors
///
/// Returns a human-readable message identifying the malformed tag (unclosed `<`, unknown tag
/// name, or an out-of-range `<C-x>`/hex escape) so a CLI caller can report exactly what's wrong
/// with the string it passed, rather than a generic parse failure.
pub fn encode(input: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c != '<' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        // `<<` is the literal-`<` escape.
        if let Some(&(_, '<')) = chars.peek() {
            chars.next();
            out.push(b'<');
            continue;
        }
        // Find the matching `>`, collecting the tag's inner text.
        let mut tag = String::new();
        let mut closed = false;
        for (_, tc) in chars.by_ref() {
            if tc == '>' {
                closed = true;
                break;
            }
            tag.push(tc);
        }
        if !closed {
            return Err(format!(
                "unclosed '<' at byte offset {start} (tag text so far: {tag:?}) -- \
                 every '<' must be closed with a matching '>', or escaped as '<<' for a literal '<'"
            ));
        }
        out.extend_from_slice(&decode_tag(&tag)?);
    }
    Ok(out)
}

fn decode_tag(tag: &str) -> Result<Vec<u8>, String> {
    match tag {
        "Esc" | "Escape" => return Ok(vec![0x1b]),
        "Enter" | "CR" | "Return" => return Ok(vec![b'\r']),
        "LF" | "NL" => return Ok(vec![b'\n']),
        "Tab" => return Ok(vec![b'\t']),
        "Backspace" | "BS" => return Ok(vec![0x7f]),
        "Space" => return Ok(vec![b' ']),
        "Up" => return Ok(b"\x1b[A".to_vec()),
        "Down" => return Ok(b"\x1b[B".to_vec()),
        "Right" => return Ok(b"\x1b[C".to_vec()),
        "Left" => return Ok(b"\x1b[D".to_vec()),
        "Home" => return Ok(b"\x1b[H".to_vec()),
        "End" => return Ok(b"\x1b[F".to_vec()),
        "PageUp" | "PgUp" => return Ok(b"\x1b[5~".to_vec()),
        "PageDown" | "PgDn" => return Ok(b"\x1b[6~".to_vec()),
        "Delete" | "Del" => return Ok(b"\x1b[3~".to_vec()),
        "Insert" | "Ins" => return Ok(b"\x1b[2~".to_vec()),
        _ => {}
    }
    // `<C-x>` -- control byte for a-z (Ctrl-A=0x01 .. Ctrl-Z=0x1a), plus a few common
    // non-alphabetic control-key aliases real terminals also send this way.
    if let Some(rest) = tag.strip_prefix("C-") {
        let mut cs = rest.chars();
        if let (Some(c), None) = (cs.next(), cs.next()) {
            if c.is_ascii_alphabetic() {
                let upper = c.to_ascii_uppercase() as u8;
                return Ok(vec![upper - b'A' + 1]);
            }
            match c {
                '[' => return Ok(vec![0x1b]),
                '\\' => return Ok(vec![0x1c]),
                ']' => return Ok(vec![0x1d]),
                '^' => return Ok(vec![0x1e]),
                '_' => return Ok(vec![0x1f]),
                '@' => return Ok(vec![0x00]),
                '?' => return Ok(vec![0x7f]),
                _ => {}
            }
        }
        return Err(format!(
            "unrecognized control-key tag '<{tag}>' -- expected '<C-x>' with a single letter a-z \
             (or one of [ \\ ] ^ _ @ ?), got '{rest}'"
        ));
    }
    // `<0xNN>` -- literal hex-escape fallback for anything not covered above.
    if let Some(hex) = tag.strip_prefix("0x").or_else(|| tag.strip_prefix("0X")) {
        return u8::from_str_radix(hex, 16)
            .map(|b| vec![b])
            .map_err(|e| format!("invalid hex escape '<{tag}>': {e}"));
    }
    Err(format!(
        "unrecognized key tag '<{tag}>' -- known tags: Esc, Enter/CR, LF/NL, Tab, Backspace/BS, \
         Space, Up, Down, Left, Right, Home, End, PageUp/PgUp, PageDown/PgDn, Delete/Del, \
         Insert/Ins, C-<letter> (or C-[ C-\\ C-] C-^ C-_ C-@ C-?), 0x<hex>, or '<<' for a literal '<'"
    ))
}
