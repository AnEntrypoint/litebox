// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.
//
// Scratch tool: package a manually-assembled Alpine X11 rootfs (built by unpacking real
// .apk files from dl-cdn.alpinelinux.org, see .wfgy/xorg-alpine-build/) into a
// --initial-files-compatible tar, rewriting every ELF via litebox_syscall_rewriter, and
// reconstructing real symlinks from each source .apk's own tar headers (since the merged
// rootfs directory on Windows lost symlink identity during extraction).

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

struct Entry {
    tar_path: String,
    data: Vec<u8>,
    mode: u32,
    symlink_target: Option<String>,
    is_dir: bool,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!(
            "usage: {} <apks_dir> <rootfs_dir> <output_tar>",
            args[0]
        );
        std::process::exit(2);
    }
    let apks_dir = PathBuf::from(&args[1]);
    let rootfs_dir = PathBuf::from(&args[2]);
    let output = PathBuf::from(&args[3]);

    // Pass 1: read every .apk's data-segment tar headers to recover real modes and
    // symlink targets (Windows extraction can't create symlinks without elevation, so the
    // merged rootfs materializes them as regular file copies).
    let mut modes: BTreeMap<String, u32> = BTreeMap::new();
    let mut symlinks: BTreeMap<String, String> = BTreeMap::new();
    let mut dirs: Vec<String> = Vec::new();

    for entry in std::fs::read_dir(&apks_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("apk") {
            continue;
        }
        let data =
            std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        // An apk is a concatenation of gzip members (sig, .PKGINFO control, data). Walk all
        // gzip members and inspect every tar entry across all of them; we only care about
        // ones with real file paths (skip .PKGINFO/.SIGN.*).
        let mut cursor = std::io::Cursor::new(&data[..]);
        loop {
            let pos = cursor.position();
            if pos as usize >= data.len() {
                break;
            }
            let gz = match flate2::read::GzDecoder::new(&data[pos as usize..]) {
                d => d,
            };
            let mut tar = tar::Archive::new(gz);
            let mut any = false;
            match tar.entries() {
                Ok(entries) => {
                    for e in entries {
                        let mut e = match e {
                            Ok(e) => e,
                            Err(_) => break,
                        };
                        any = true;
                        let hpath = e.path().ok().map(|p| p.to_string_lossy().to_string());
                        let Some(hpath) = hpath else { continue };
                        if hpath.starts_with(".PKGINFO")
                            || hpath.starts_with(".SIGN")
                            || hpath.starts_with(".post")
                            || hpath.starts_with(".pre")
                            || hpath.starts_with(".trigger")
                        {
                            continue;
                        }
                        let mode = e.header().mode().unwrap_or(0o644);
                        let etype = e.header().entry_type();
                        if etype.is_symlink() {
                            if let Ok(Some(target)) = e.link_name() {
                                symlinks.insert(hpath.clone(), target.to_string_lossy().to_string());
                            }
                        } else if etype.is_dir() {
                            dirs.push(hpath.clone());
                        }
                        modes.insert(hpath, mode);
                        // drain any remaining data so the position advances correctly for
                        // multi-entry members (not strictly needed since we don't read data
                        // here, tar::Entries handles seeking).
                        let mut buf = Vec::new();
                        let _ = e.read_to_end(&mut buf);
                    }
                }
                Err(_) => {}
            }
            if !any {
                break;
            }
            // GzDecoder doesn't expose consumed byte count directly through this API easily;
            // fall back to trying the whole remaining buffer as the next member by re-scanning
            // for the next gzip magic (1f 8b) after the current position estimate.
            // Simplify: apk files have exactly 3 members (sig, control, data) OR more if
            // multi-key signed; just try decoding remaining bytes as one more archive pass by
            // searching for next gzip magic.
            let remaining = &data[pos as usize..];
            // Find next gzip magic after byte 2 (skip current header) — best effort.
            let mut next_off = None;
            let mut i = 2usize;
            while i + 1 < remaining.len() {
                if remaining[i] == 0x1f && remaining[i + 1] == 0x8b {
                    next_off = Some(i);
                    break;
                }
                i += 1;
            }
            match next_off {
                Some(off) => cursor.set_position(pos + off as u64),
                None => break,
            }
        }
    }

    eprintln!(
        "Collected {} real modes, {} real symlinks from apk headers",
        modes.len(),
        symlinks.len()
    );

    // Pass 2: walk the merged rootfs directory tree and build tar entries.
    let mut out_entries: Vec<Entry> = Vec::new();
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();

    fn walk(
        dir: &Path,
        root: &Path,
        modes: &BTreeMap<String, u32>,
        symlinks: &BTreeMap<String, String>,
        out: &mut Vec<Entry>,
        seen: &mut BTreeMap<String, ()>,
    ) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let ft = entry.file_type()?;
            if ft.is_dir() {
                out.push(Entry {
                    tar_path: rel.clone(),
                    data: Vec::new(),
                    mode: *modes.get(&rel).unwrap_or(&0o755) & 0o7777,
                    symlink_target: None,
                    is_dir: true,
                });
                seen.insert(rel.clone(), ());
                walk(&path, root, modes, symlinks, out, seen)?;
            } else {
                seen.insert(rel.clone(), ());
                if let Some(target) = symlinks.get(&rel) {
                    out.push(Entry {
                        tar_path: rel,
                        data: Vec::new(),
                        mode: 0o777,
                        symlink_target: Some(target.clone()),
                        is_dir: false,
                    });
                } else {
                    let data = std::fs::read(&path)
                        .with_context(|| format!("reading {}", path.display()))?;
                    let mode = *modes.get(&rel).unwrap_or(&0o644) & 0o7777;
                    out.push(Entry {
                        tar_path: rel,
                        data,
                        mode,
                        symlink_target: None,
                        is_dir: false,
                    });
                }
            }
        }
        Ok(())
    }

    walk(
        &rootfs_dir,
        &rootfs_dir,
        &modes,
        &symlinks,
        &mut out_entries,
        &mut seen,
    )?;

    eprintln!("Walked {} filesystem entries", out_entries.len());

    // Rewrite ELFs in parallel-ish (serial is fine here, few hundred files).
    let mut rewritten_count = 0usize;
    for e in out_entries.iter_mut() {
        if e.symlink_target.is_some() || e.is_dir || e.data.is_empty() {
            continue;
        }
        let mode_exec = e.mode & 0o111 != 0;
        if !mode_exec {
            continue;
        }
        let fake_path = PathBuf::from(&e.tar_path);
        let before_len = e.data.len();
        let rewritten =
            litebox_packager::rewrite_elf(&e.data, &fake_path, false);
        if rewritten.len() != before_len || rewritten != e.data {
            rewritten_count += 1;
        }
        e.data = rewritten;
    }
    eprintln!("Rewrote {rewritten_count} ELF files");

    // Write final tar.
    let f = std::fs::File::create(&output)?;
    let mut builder = tar::Builder::new(f);
    for e in &out_entries {
        let mut header = tar::Header::new_gnu();
        header.set_mode(e.mode);
        header.set_size(e.data.len() as u64);
        header.set_mtime(0);
        if let Some(target) = &e.symlink_target {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_cksum();
            builder.append_link(&mut header, &e.tar_path, target)?;
        } else if e.is_dir {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_cksum();
            builder.append_data(&mut header, &e.tar_path, std::io::empty())?;
        } else {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder.append_data(&mut header, &e.tar_path, &e.data[..])?;
        }
    }
    builder.finish()?;

    eprintln!("Wrote {}", output.display());
    Ok(())
}
