// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Apply the contents of a raw tar archive (in memory, e.g. produced by
//! [`super::export::export_all`] and serialized to a tar archive by a `std`-capable caller) into
//! any [`FileSystem`] implementation, via that trait's own `mkdir`/`open`/`write`/`symlink`
//! calls.
//!
//! Walks the archive's raw 512-byte POSIX header blocks directly (the same technique
//! `super::tar_ro::TarIndex::new` uses, for the same reason: `tar_no_std::TarArchiveRef::
//! entries()` silently skips every non-regular-file entry, so a symlink in the archive would
//! otherwise be invisible), so this stays `no_std`/`alloc`-only like the rest of this crate --
//! unlike [`super::export`]'s sibling doc comment, this module both reads its input format
//! (tar) and applies it, since (unlike serializing an export, which needs a stream writer)
//! parsing an in-memory byte slice needs no I/O capability at all.

use alloc::format;
use alloc::string::String;

use super::{FileSystem, Mode, OFlags};

const BLOCKSIZE: usize = 512;

#[derive(Debug)]
#[non_exhaustive]
pub enum ImportError {
    Mkdir,
    Symlink,
    Open,
    Write,
    Close,
}

/// Parses `tar_data` as a POSIX tar archive and applies every regular-file, directory, and
/// symlink entry into `fs`, creating ancestor directories as needed.
///
/// `AlreadyExists` from `mkdir` is treated as success (the target filesystem's own default
/// layout may have already created a directory this archive also mentions, e.g. `/tmp`, `/etc`)
/// -- every other error is propagated. Character devices, hardlinks, and FIFOs are skipped
/// (mirroring [`super::tar_ro`]'s own read-only-layer handling); this module supports exactly
/// the entry types [`super::export::export_all`] ever produces.
///
/// # Panics
///
/// Never in practice: the internal header cast can only fail to deref if `tar_data`'s length
/// were shorter than the loop's own bounds check allows, which is structurally impossible given
/// the `while block_index < total_blocks` guard immediately above it.
pub fn import_all<FS: FileSystem>(fs: &FS, tar_data: &[u8]) -> Result<(), ImportError> {
    let mut block_index = 0usize;
    let total_blocks = tar_data.len() / BLOCKSIZE;
    while block_index < total_blocks {
        // SAFETY: `PosixHeader` is `#[repr(C, packed)]` and exactly `BLOCKSIZE` bytes; the loop
        // guard above ensures a full block is available at this offset within `tar_data`.
        let header = unsafe {
            tar_data
                .as_ptr()
                .add(block_index * BLOCKSIZE)
                .cast::<tar_no_std::PosixHeader>()
                .as_ref()
                .unwrap()
        };
        if header.is_zero_block() {
            // One (or, at true end-of-archive, two) all-zero blocks terminate the archive.
            break;
        }
        block_index += 1;

        let Ok(typeflag) = header.typeflag.try_to_type_flag() else {
            continue;
        };
        let Ok(name) = header.name.as_str() else {
            continue;
        };
        let path = normalize(name);
        if path.is_empty() {
            continue;
        }
        let mode = mode_of_modeflags(
            header
                .mode
                .to_flags()
                .unwrap_or(tar_no_std::ModeFlags::empty()),
        );

        match typeflag {
            tar_no_std::TypeFlag::DIRTYPE => match fs.mkdir(&*path, mode) {
                Ok(()) | Err(super::errors::MkdirError::AlreadyExists) => {}
                Err(_) => return Err(ImportError::Mkdir),
            },
            tar_no_std::TypeFlag::SYMTYPE => {
                let Ok(target) = header.linkname.as_str() else {
                    continue;
                };
                fs.symlink(target, &*path)
                    .map_err(|_| ImportError::Symlink)?;
            }
            tar_no_std::TypeFlag::REGTYPE | tar_no_std::TypeFlag::AREGTYPE => {
                let payload_blocks = header.payload_block_count().unwrap_or(0);
                let content_start = block_index * BLOCKSIZE;
                let content_len = header.size.as_number::<usize>().unwrap_or(0);
                let content_end = content_start
                    .saturating_add(content_len)
                    .min(tar_data.len());
                block_index += payload_blocks;

                let contents: &[u8] = tar_data.get(content_start..content_end).unwrap_or(&[]);

                let fd = fs
                    .open(&*path, OFlags::WRONLY | OFlags::CREAT | OFlags::TRUNC, mode)
                    .map_err(|_| ImportError::Open)?;
                let mut written = 0;
                while written < contents.len() {
                    let n = fs
                        .write(&fd, &contents[written..], None)
                        .map_err(|_| ImportError::Write)?;
                    if n == 0 {
                        break;
                    }
                    written += n;
                }
                fs.close(&fd).map_err(|_| ImportError::Close)?;
            }
            _ => {
                // Character devices, hardlinks, FIFOs: not produced by `export_all`, skipped.
                let payload_blocks = header.payload_block_count().unwrap_or(0);
                block_index += payload_blocks;
            }
        }
    }
    Ok(())
}

fn normalize(filename: &str) -> String {
    let trimmed = filename.strip_prefix("./").unwrap_or(filename);
    let trimmed = trimmed.strip_prefix('/').unwrap_or(trimmed);
    format!("/{trimmed}")
}

fn mode_of_modeflags(perms: tar_no_std::ModeFlags) -> Mode {
    use tar_no_std::ModeFlags;
    let mut mode = Mode::empty();
    mode.set(Mode::RUSR, perms.contains(ModeFlags::OwnerRead));
    mode.set(Mode::WUSR, perms.contains(ModeFlags::OwnerWrite));
    mode.set(Mode::XUSR, perms.contains(ModeFlags::OwnerExec));
    mode.set(Mode::RGRP, perms.contains(ModeFlags::GroupRead));
    mode.set(Mode::WGRP, perms.contains(ModeFlags::GroupWrite));
    mode.set(Mode::XGRP, perms.contains(ModeFlags::GroupExec));
    mode.set(Mode::ROTH, perms.contains(ModeFlags::OthersRead));
    mode.set(Mode::WOTH, perms.contains(ModeFlags::OthersWrite));
    mode.set(Mode::XOTH, perms.contains(ModeFlags::OthersExec));
    if mode.is_empty() {
        mode = Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH;
    }
    mode
}
