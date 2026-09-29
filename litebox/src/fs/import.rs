// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Apply a raw in-memory tar archive (e.g. [`super::export::export_all`]'s entries, serialized to
//! tar by a `std`-capable caller) into any [`FileSystem`], via that trait's own
//! `mkdir`/`open`/`write`/`symlink` calls. Walks the raw 512-byte POSIX header blocks directly
//! (`tar_no_std`'s own iterator skips non-regular entries), staying `no_std`/`alloc`-only.

use alloc::format;
use alloc::string::String;

use super::{FileSystem, Mode, OFlags};

const BLOCKSIZE: usize = 512;

#[derive(Debug)]
#[non_exhaustive]
pub enum ImportError {
    Mkdir,
    MakeFifo,
    Symlink,
    /// Opening the named file for writing failed.
    Open(alloc::string::String, super::errors::OpenError),
    Write,
    Close,
}

/// Parses `tar_data` as a POSIX tar archive and applies every regular-file, directory, FIFO and
/// symlink entry into `fs`, creating ancestor directories as needed. `AlreadyExists` from `mkdir`
/// is treated as success (the target filesystem may already provide `/tmp`, `/etc`); every other
/// error is propagated. Character devices and hardlinks are skipped.
///
/// # Panics
///
/// Never in practice: the internal header cast can only fail to deref if `tar_data`'s length
/// were shorter than the loop's own bounds check allows, which is structurally impossible given
/// the `while block_index < total_blocks` guard immediately above it.
///
/// One entry failing does not stop the import: every other entry is still applied and the first
/// error is returned at the end. (A cross-process fork child re-exports everything it adopted, so
/// one stale unwritable file used to discard all of the child's real writes.)
pub fn import_all<FS: FileSystem>(fs: &FS, tar_data: &[u8]) -> Result<(), ImportError> {
    super::with_root_identity(|| import_all_as_root(fs, tar_data))
}

fn import_all_as_root<FS: FileSystem>(fs: &FS, tar_data: &[u8]) -> Result<(), ImportError> {
    let mut first_error: Option<ImportError> = None;
    let mut record = |r: Result<(), ImportError>| {
        if let Err(e) = r
            && first_error.is_none()
        {
            first_error = Some(e);
        }
    };
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
        let owner_user = header.uid.as_number::<u32>().ok().and_then(|id| u16::try_from(id).ok());
        let owner_group = header.gid.as_number::<u32>().ok().and_then(|id| u16::try_from(id).ok());

        match typeflag {
            tar_no_std::TypeFlag::DIRTYPE => match fs.mkdir(&*path, mode) {
                Ok(()) | Err(super::errors::MkdirError::AlreadyExists) => {}
                Err(_) => record(Err(ImportError::Mkdir)),
            },
            tar_no_std::TypeFlag::FIFOTYPE => match fs.make_fifo(&*path, mode) {
                Ok(()) | Err(super::errors::MkdirError::AlreadyExists) => {}
                Err(_) => record(Err(ImportError::MakeFifo)),
            },
            tar_no_std::TypeFlag::SYMTYPE => {
                let Ok(target) = header.linkname.as_str() else {
                    continue;
                };
                match fs.symlink(target, &*path) {
                    Ok(()) => {}
                    // Already present: replace it -- cross-process `fork()` children re-export
                    // everything adopted from the parent, and erroring here silently lost the
                    // child's real writes. See gm mutable fs-import-symlink-replace-fork.
                    Err(super::errors::SymlinkError::AlreadyExists) => {
                        let _ = fs.unlink(&*path);
                        record(fs.symlink(target, &*path).map_err(|_| ImportError::Symlink));
                    }
                    Err(_) => record(Err(ImportError::Symlink)),
                }
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
                record(import_file(fs, &path, mode, contents));
            }
            _ => {
                // Character devices and hardlinks: not produced by `export_all`, skipped.
                let payload_blocks = header.payload_block_count().unwrap_or(0);
                block_index += payload_blocks;
                continue;
            }
        }
        if !matches!(typeflag, tar_no_std::TypeFlag::SYMTYPE) {
            let _ = fs.chown(&*path, owner_user, owner_group);
            let _ = fs.chmod(&*path, mode);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Writes one regular file. The import applies another process's writes to the one shared
/// filesystem, so a file whose mode forbids writing (e.g. Xvfb's 0444 `/tmp/.X1-lock`, re-exported
/// by every child) is made writable for the write and its mode restored afterward.
fn import_file<FS: FileSystem>(
    fs: &FS,
    path: &str,
    mode: Mode,
    contents: &[u8],
) -> Result<(), ImportError> {
    let flags = OFlags::WRONLY | OFlags::CREAT | OFlags::TRUNC;
    let (fd, restore_mode) = match fs.open(path, flags, mode) {
        Ok(fd) => (fd, false),
        Err(super::errors::OpenError::AccessNotAllowed) => {
            fs.chmod(path, mode | Mode::WUSR)
                .map_err(|_| ImportError::Open(String::from(path), super::errors::OpenError::AccessNotAllowed))?;
            let fd = fs
                .open(path, flags, mode)
                .map_err(|e| ImportError::Open(String::from(path), e))?;
            (fd, true)
        }
        Err(e) => return Err(ImportError::Open(String::from(path), e)),
    };
    let mut result = Ok(());
    let mut written = 0;
    while written < contents.len() {
        match fs.write(&fd, &contents[written..], None) {
            Ok(0) => break,
            Ok(n) => written += n,
            Err(_) => {
                result = Err(ImportError::Write);
                break;
            }
        }
    }
    if fs.close(&fd).is_err() && result.is_ok() {
        result = Err(ImportError::Close);
    }
    if restore_mode {
        let _ = fs.chmod(path, mode);
    }
    result
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
    mode.set(Mode::SUID, perms.contains(ModeFlags::SetUID));
    mode.set(Mode::SGID, perms.contains(ModeFlags::SetGID));
    mode.set(Mode::SVTX, perms.contains(ModeFlags::TSVTX));
    if mode.is_empty() {
        mode = Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH;
    }
    mode
}
