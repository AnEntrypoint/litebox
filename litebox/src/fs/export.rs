// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Recursively walk any [`FileSystem`] into an in-memory list of entries, for snapshotting a
//! writable upper layer (e.g. [`super::in_mem`]) to a durable format such as a tar archive.
//!
//! This module only builds the entry list -- it stays `no_std`/`alloc`-only, matching the rest of
//! this crate. Serializing the entries to an actual archive format (which needs an I/O-capable
//! writer) is the caller's responsibility.

use alloc::string::String;
use alloc::vec::Vec;

use super::{FileSystem, FileType, Mode, OFlags};

/// A single exported file-system entry.
pub struct ExportedEntry {
    /// Absolute path, e.g. `/etc/resolv.conf`.
    pub path: String,
    pub file_type: FileType,
    pub mode: Mode,
    /// File contents (regular files only; empty for directories/symlinks/devices).
    pub contents: Vec<u8>,
    /// Symlink target (symlinks only).
    pub symlink_target: Option<String>,
}

/// Recursively walk `fs` starting at `/`, returning every entry reachable from the root.
///
/// Character devices are skipped (they have no meaningful exportable content and are expected to
/// be recreated structurally by whatever consumes the export, e.g. `/dev` in a fresh guest boot).
pub fn export_all<FS: FileSystem>(fs: &FS) -> Result<Vec<ExportedEntry>, ExportError> {
    let mut out = Vec::new();
    walk(fs, "/", &mut out)?;
    Ok(out)
}

#[derive(Debug)]
#[non_exhaustive]
pub enum ExportError {
    Open,
    ReadDir,
    Read,
    FileStatus,
    ReadLink,
    Close,
}

fn walk<FS: FileSystem>(
    fs: &FS,
    dir_path: &str,
    out: &mut Vec<ExportedEntry>,
) -> Result<(), ExportError> {
    let dir_fd = fs
        .open(dir_path, OFlags::RDONLY, Mode::empty())
        .map_err(|_| ExportError::Open)?;
    let entries = fs.read_dir(&dir_fd).map_err(|_| ExportError::ReadDir)?;
    fs.close(&dir_fd).map_err(|_| ExportError::Close)?;

    for entry in entries {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        let child_path = if dir_path.ends_with('/') {
            alloc::format!("{dir_path}{}", entry.name)
        } else {
            alloc::format!("{dir_path}/{}", entry.name)
        };

        match entry.file_type {
            FileType::Directory => {
                let status = fs
                    .file_status(&*child_path)
                    .map_err(|_| ExportError::FileStatus)?;
                out.push(ExportedEntry {
                    path: child_path.clone(),
                    file_type: FileType::Directory,
                    mode: status.mode,
                    contents: Vec::new(),
                    symlink_target: None,
                });
                walk(fs, &child_path, out)?;
            }
            FileType::RegularFile => {
                let status = fs
                    .file_status(&*child_path)
                    .map_err(|_| ExportError::FileStatus)?;
                let fd = fs
                    .open(&*child_path, OFlags::RDONLY, Mode::empty())
                    .map_err(|_| ExportError::Open)?;
                let mut contents = alloc::vec![0u8; status.size];
                let mut total_read = 0;
                while total_read < contents.len() {
                    let n = fs
                        .read(&fd, &mut contents[total_read..], None)
                        .map_err(|_| ExportError::Read)?;
                    if n == 0 {
                        break;
                    }
                    total_read += n;
                }
                contents.truncate(total_read);
                fs.close(&fd).map_err(|_| ExportError::Close)?;
                out.push(ExportedEntry {
                    path: child_path,
                    file_type: FileType::RegularFile,
                    mode: status.mode,
                    contents,
                    symlink_target: None,
                });
            }
            FileType::Symlink => {
                let status = fs
                    .file_status(&*child_path)
                    .map_err(|_| ExportError::FileStatus)?;
                let target = fs
                    .read_link(&*child_path)
                    .map_err(|_| ExportError::ReadLink)?;
                out.push(ExportedEntry {
                    path: child_path,
                    file_type: FileType::Symlink,
                    mode: status.mode,
                    contents: Vec::new(),
                    symlink_target: Some(target),
                });
            }
            FileType::CharacterDevice => {
                // Not exported: recreated structurally by the consumer, not by content.
            }
        }
    }
    Ok(())
}
