//! `inotify`: change notifications for files and directories.
//!
//! An instance is an ordinary pipe. The guest holds the read end and reads packed
//! `struct inotify_event` records from it; the kernel side keeps the write end, as a handle
//! independent of any one process's descriptor table, in a registry every process shares. The
//! file-system syscalls (`create`, `write`, `unlink`, `rename`, ...) call
//! [`Task::inotify_notify`], which appends a record to each instance that watches the path or its
//! parent directory. Everything a guest can change goes through those syscalls, so no host file
//! system needs watching.

use alloc::{collections::BTreeMap, string::String, vec::Vec};
use core::ffi::c_char;
use core::sync::atomic::Ordering;

use litebox::fs::OFlags;
use litebox::pipes::{DetachedPipeEnd, Flags};
use litebox::platform::RawConstPointer as _;
use litebox_common_linux::errno::Errno;

use crate::{ShimFS, ShimPlatform, Task, UserPtr};

pub(crate) const IN_ACCESS: u32 = 0x0000_0001;
pub(crate) const IN_MODIFY: u32 = 0x0000_0002;
pub(crate) const IN_ATTRIB: u32 = 0x0000_0004;
pub(crate) const IN_CLOSE_WRITE: u32 = 0x0000_0008;
pub(crate) const IN_CLOSE_NOWRITE: u32 = 0x0000_0010;
pub(crate) const IN_OPEN: u32 = 0x0000_0020;
pub(crate) const IN_MOVED_FROM: u32 = 0x0000_0040;
pub(crate) const IN_MOVED_TO: u32 = 0x0000_0080;
pub(crate) const IN_CREATE: u32 = 0x0000_0100;
pub(crate) const IN_DELETE: u32 = 0x0000_0200;
pub(crate) const IN_DELETE_SELF: u32 = 0x0000_0400;
pub(crate) const IN_MOVE_SELF: u32 = 0x0000_0800;
const IN_Q_OVERFLOW: u32 = 0x0000_4000;
const IN_IGNORED: u32 = 0x0000_8000;
const IN_ONLYDIR: u32 = 0x0100_0000;
const IN_DONT_FOLLOW: u32 = 0x0200_0000;
const IN_EXCL_UNLINK: u32 = 0x0400_0000;
const IN_MASK_CREATE: u32 = 0x1000_0000;
const IN_MASK_ADD: u32 = 0x2000_0000;
pub(crate) const IN_ISDIR: u32 = 0x4000_0000;
const IN_ONESHOT: u32 = 0x8000_0000;
/// The event bits a watch can ask for.
const IN_ALL_EVENTS: u32 = 0x0000_0FFF;

/// Bytes in one event record's fixed header (`wd`, `mask`, `cookie`, `len`).
const EVENT_HEADER: usize = 16;

pub(crate) struct InotifyWatch {
    wd: i32,
    path: String,
    mask: u32,
}

pub(crate) struct InotifyInstance<Platform: ShimPlatform> {
    writer: DetachedPipeEnd<Platform>,
    watches: Vec<InotifyWatch>,
    next_wd: i32,
}

/// Instance id -> instance, shared by every process.
pub(crate) type InotifyRegistry<Platform> = BTreeMap<u32, InotifyInstance<Platform>>;

/// Entry metadata on the guest-visible read end, naming its instance.
#[derive(Clone, Copy)]
pub(crate) struct InotifyId(u32);

fn event_record(wd: i32, mask: u32, cookie: u32, name: Option<&str>) -> Vec<u8> {
    let name_len = name.map_or(0, |n| (n.len() + 1).next_multiple_of(EVENT_HEADER));
    let mut out = Vec::with_capacity(EVENT_HEADER + name_len);
    out.extend_from_slice(&wd.to_ne_bytes());
    out.extend_from_slice(&mask.to_ne_bytes());
    out.extend_from_slice(&cookie.to_ne_bytes());
    out.extend_from_slice(&(name_len as u32).to_ne_bytes());
    if let Some(name) = name {
        out.extend_from_slice(name.as_bytes());
        out.resize(EVENT_HEADER + name_len, 0);
    }
    out
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    pub(crate) fn sys_inotify_init1(&self, flags: u32) -> Result<u32, Errno> {
        let flags = OFlags::from_bits(flags).ok_or(Errno::EINVAL)?;
        if flags.intersects((OFlags::CLOEXEC | OFlags::NONBLOCK).complement()) {
            return Err(Errno::EINVAL);
        }
        let pipe = self.global.create_linux_pipe(flags)?;
        // Event delivery must never block a file-system syscall, whatever the guest's own end does.
        let _ = self
            .global
            .pipes()
            .update_flags(&pipe.writer, Flags::NON_BLOCKING, true);
        let writer = self.global.pipes().detach_end(&pipe.writer);
        let _ = self.global.close_linux_pipe(&pipe.writer);
        let Ok(writer) = writer else {
            let _ = self.global.close_linux_pipe(&pipe.reader);
            return Err(Errno::EMFILE);
        };
        let id = {
            let mut registry = self.global.inotify.lock();
            let id = registry.keys().next_back().map_or(1, |last| last + 1);
            registry.insert(
                id,
                InotifyInstance {
                    writer,
                    watches: Vec::new(),
                    next_wd: 1,
                },
            );
            id
        };
        {
            let mut dt = self.global.litebox.descriptor_table_mut();
            let _ = dt.set_entry_metadata(&pipe.reader, InotifyId(id));
        }
        let files = self.files.borrow();
        match files.insert_raw_fd(pipe.reader) {
            Ok(raw) => Ok(u32::try_from(raw).unwrap_or(u32::MAX)),
            Err(reader) => {
                let _ = self.global.close_linux_pipe(&reader);
                self.global.inotify.lock().remove(&id);
                Err(Errno::EMFILE)
            }
        }
    }

    /// The instance id behind `fd`, or `EINVAL` when `fd` is not an inotify descriptor.
    fn inotify_instance_of(&self, fd: i32) -> Result<u32, Errno> {
        let raw = usize::try_from(fd).map_err(|_| Errno::EBADF)?;
        let files = self.files.borrow();
        files
            .run_on_raw_fd(
                raw,
                |_| Err(Errno::EINVAL),
                |_| Err(Errno::EINVAL),
                |pipe_fd| {
                    self.global
                        .litebox
                        .descriptor_table()
                        .with_metadata(pipe_fd, |id: &InotifyId| id.0)
                        .map_err(|_| Errno::EINVAL)
                },
                |_| Err(Errno::EINVAL),
                |_| Err(Errno::EINVAL),
                |_| Err(Errno::EINVAL),
                |_| Err(Errno::EINVAL),
                |_| Err(Errno::EINVAL),
                |_| Err(Errno::EINVAL),
                |_| Err(Errno::EINVAL),
            )?
    }

    pub(crate) fn sys_inotify_add_watch(
        &self,
        fd: i32,
        pathname: UserPtr<c_char>,
        mask: u32,
    ) -> Result<u32, Errno> {
        let id = self.inotify_instance_of(fd)?;
        let events = mask & IN_ALL_EVENTS;
        if events == 0 || (mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0) {
            return Err(Errno::EINVAL);
        }
        let path = pathname.to_cstring::<Platform>().ok_or(Errno::EFAULT)?;
        let abs = self.resolve_path(path.as_c_str())?;
        let abs = abs.to_str().map_err(|_| Errno::EINVAL)?.trim_end_matches('/');
        let abs = if abs.is_empty() { "/" } else { abs };
        // The path must exist (and be a directory under `IN_ONLYDIR`).
        let status = self
            .files
            .borrow()
            .fs
            .file_status(abs)
            .map_err(Errno::from)?;
        if mask & IN_ONLYDIR != 0 && status.file_type != litebox::fs::FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        let mut registry = self.global.inotify.lock();
        let instance = registry.get_mut(&id).ok_or(Errno::EINVAL)?;
        if let Some(existing) = instance.watches.iter_mut().find(|w| w.path == abs) {
            if mask & IN_MASK_CREATE != 0 {
                return Err(Errno::EEXIST);
            }
            existing.mask = if mask & IN_MASK_ADD != 0 {
                existing.mask | mask
            } else {
                mask
            };
            return Ok(existing.wd.unsigned_abs());
        }
        let wd = instance.next_wd;
        instance.next_wd += 1;
        instance.watches.push(InotifyWatch {
            wd,
            path: String::from(abs),
            mask,
        });
        self.global.inotify_watching.fetch_add(1, Ordering::Relaxed);
        Ok(wd.unsigned_abs())
    }

    pub(crate) fn sys_inotify_rm_watch(&self, fd: i32, wd: i32) -> Result<(), Errno> {
        let id = self.inotify_instance_of(fd)?;
        let mut registry = self.global.inotify.lock();
        let instance = registry.get_mut(&id).ok_or(Errno::EINVAL)?;
        let Some(pos) = instance.watches.iter().position(|w| w.wd == wd) else {
            return Err(Errno::EINVAL);
        };
        instance.watches.remove(pos);
        self.global.inotify_watching.fetch_sub(1, Ordering::Relaxed);
        let _ = instance
            .writer
            .write(&self.wait_cx(), &event_record(wd, IN_IGNORED, 0, None));
        Ok(())
    }

    /// Tells every instance watching `path` (or its parent directory) that `event` happened to
    /// it. `path` is absolute and normalized; `is_dir` marks the subject a directory.
    ///
    /// `event` is one of the `IN_*` event bits. Watches on the parent see it with the entry's
    /// name; a watch on the path itself sees it without a name, with deletion and rename mapped to
    /// their `*_SELF` forms.
    pub(crate) fn inotify_notify(&self, path: &str, event: u32, is_dir: bool, cookie: u32) {
        if self.global.inotify_watching.load(Ordering::Relaxed) == 0 {
            return;
        }
        let (dir, name) = match path.rfind('/') {
            Some(0) => ("/", &path[1..]),
            Some(i) => (&path[..i], &path[i + 1..]),
            None => return,
        };
        let dir_flag = if is_dir { IN_ISDIR } else { 0 };
        let self_event = match event {
            IN_DELETE => IN_DELETE_SELF,
            IN_MOVED_FROM => IN_MOVE_SELF,
            IN_MOVED_TO | IN_CREATE => 0,
            other => other,
        };
        let mut registry = self.global.inotify.lock();
        let mut dead = Vec::new();
        for (id, instance) in registry.iter_mut() {
            let mut gone = Vec::new();
            let mut writes: Vec<Vec<u8>> = Vec::new();
            for watch in &instance.watches {
                if watch.path == dir && watch.mask & event != 0 {
                    writes.push(event_record(watch.wd, event | dir_flag, cookie, Some(name)));
                    if watch.mask & IN_ONESHOT != 0 {
                        gone.push(watch.wd);
                    }
                } else if watch.path == path && self_event != 0 && watch.mask & self_event != 0 {
                    writes.push(event_record(watch.wd, self_event | dir_flag, 0, None));
                    if watch.mask & IN_ONESHOT != 0 || self_event == IN_DELETE_SELF {
                        gone.push(watch.wd);
                    }
                }
            }
            for wd in &gone {
                writes.push(event_record(*wd, IN_IGNORED, 0, None));
            }
            let before = instance.watches.len();
            instance.watches.retain(|w| !gone.contains(&w.wd));
            let removed = before - instance.watches.len();
            if removed > 0 {
                self.global.inotify_watching.fetch_sub(removed, Ordering::Relaxed);
            }
            for bytes in writes {
                match instance.writer.write(&self.wait_cx(), &bytes) {
                    Ok(_) => {}
                    Err(litebox::pipes::errors::WriteError::WouldBlock) => {
                        let _ = instance
                            .writer
                            .write(&self.wait_cx(), &event_record(-1, IN_Q_OVERFLOW, 0, None));
                        break;
                    }
                    Err(_) => {
                        // The guest closed its end: nothing will ever read this instance again.
                        dead.push(*id);
                        break;
                    }
                }
            }
        }
        for id in dead {
            if let Some(instance) = registry.remove(&id) {
                self.global
                    .inotify_watching
                    .fetch_sub(instance.watches.len(), Ordering::Relaxed);
            }
        }
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Whether any inotify watch exists: file-system syscalls skip all event work when not.
    #[inline]
    pub(crate) fn inotify_is_watching(&self) -> bool {
        self.global.inotify_watching.load(Ordering::Relaxed) != 0
    }

    /// [`Self::inotify_notify`] for an absolute path held as a C string.
    pub(crate) fn inotify_path(&self, path: &core::ffi::CStr, event: u32, is_dir: bool) {
        if !self.inotify_is_watching() {
            return;
        }
        if let Ok(path) = path.to_str() {
            self.inotify_notify(path, event, is_dir, 0);
        }
    }

    /// [`Self::inotify_notify`] for the file behind the open descriptor `raw_fd`.
    pub(crate) fn inotify_fd(&self, raw_fd: usize, event: u32) {
        if !self.inotify_is_watching() {
            return;
        }
        let path = self.files.borrow().lookup_fd_path(raw_fd);
        if let Some(path) = path {
            self.inotify_path(&path, event, false);
        }
    }

    /// A fresh cookie pairing the two halves of one rename.
    pub(crate) fn inotify_cookie() -> u32 {
        static NEXT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }
}

/// `IN_EXCL_UNLINK` and `IN_DONT_FOLLOW` change only which events an already-unlinked or
/// symlinked target produces; neither is modelled, both are accepted.
#[allow(dead_code, reason = "documented no-op flags")]
const _ACCEPTED_FLAGS: u32 = IN_EXCL_UNLINK | IN_DONT_FOLLOW;
