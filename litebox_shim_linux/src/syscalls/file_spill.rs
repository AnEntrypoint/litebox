use alloc::{string::String, vec, vec::Vec};
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use litebox::{
    fs::{Mode, OFlags},
    platform::SystemInfoProvider,
};

use crate::{ShimFS, ShimPlatform, Task};

const SPILLED_PREFIXES: &[&str] = &["/var/lib/apt/", "/var/cache/apt/"];
const SLOT_COUNT: usize = 256;
const PATH_CAPACITY: usize = 192;
const TRANSFER_CHUNK: usize = 256 * 1024;
const LOCK_SPIN_LIMIT: u32 = 400_000_000;

const SLOT_EMPTY: u32 = 0;
const SLOT_LIVE: u32 = 1;
const SLOT_DELETED: u32 = 2;

static LOCALLY_SEEN_GENERATION: [AtomicU64; SLOT_COUNT] = [const { AtomicU64::new(0) }; SLOT_COUNT];

struct Slot {
    state: AtomicU32,
    path_len: AtomicU32,
    path: [AtomicU8; PATH_CAPACITY],
    generation: AtomicU64,
    length: AtomicU64,
}

impl Slot {
    fn new() -> Self {
        Self {
            state: AtomicU32::new(SLOT_EMPTY),
            path_len: AtomicU32::new(0),
            path: core::array::from_fn(|_| AtomicU8::new(0)),
            generation: AtomicU64::new(0),
            length: AtomicU64::new(0),
        }
    }

    fn holds(&self, path: &str) -> bool {
        self.state.load(Ordering::Acquire) != SLOT_EMPTY
            && self.path_len.load(Ordering::Relaxed) as usize == path.len()
            && path
                .bytes()
                .enumerate()
                .all(|(index, byte)| self.path[index].load(Ordering::Relaxed) == byte)
    }

    fn path_string(&self) -> String {
        let length = (self.path_len.load(Ordering::Relaxed) as usize).min(PATH_CAPACITY);
        let bytes: Vec<u8> = self.path[..length]
            .iter()
            .map(|byte| byte.load(Ordering::Relaxed))
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn assign(&self, path: &str) {
        for (index, byte) in path.bytes().enumerate() {
            self.path[index].store(byte, Ordering::Relaxed);
        }
        self.path_len.store(path.len() as u32, Ordering::Relaxed);
    }
}

pub(crate) struct SpillView {
    slot: usize,
    generation: u64,
    length: u64,
    deleted: bool,
}

pub(crate) struct SharedFileSpill {
    lock: AtomicU32,
    slots: [Slot; SLOT_COUNT],
}

pub(crate) enum SpillEdit<'a> {
    Write { start: usize, bytes: &'a [u8] },
    Truncate(usize),
    Reset,
    Replace,
    Remove,
}

impl SpillEdit<'_> {
    fn needs_local_copy(&self, slot_was_live: bool) -> bool {
        match self {
            SpillEdit::Replace => true,
            SpillEdit::Remove => false,
            _ => !slot_was_live,
        }
    }
}

pub(crate) struct AppliedEdit {
    slot: usize,
    previous_generation: u64,
    new_generation: u64,
}

impl SharedFileSpill {
    pub(crate) fn new() -> Self {
        Self {
            lock: AtomicU32::new(0),
            slots: core::array::from_fn(|_| Slot::new()),
        }
    }

    fn locked<R>(&self, body: impl FnOnce() -> R) -> R {
        let mut spins = 0u32;
        while self
            .lock
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spins += 1;
            if spins > LOCK_SPIN_LIMIT {
                break;
            }
            core::hint::spin_loop();
        }
        let result = body();
        self.lock.store(0, Ordering::Release);
        result
    }

    fn find(&self, path: &str) -> Option<usize> {
        self.slots.iter().position(|slot| slot.holds(path))
    }

    fn claim(&self, path: &str) -> Option<usize> {
        if let Some(index) = self.find(path) {
            return Some(index);
        }
        let reusable = |wanted: u32| {
            self.slots
                .iter()
                .position(|slot| slot.state.load(Ordering::Acquire) == wanted)
        };
        let index = reusable(SLOT_EMPTY).or_else(|| reusable(SLOT_DELETED))?;
        let slot = &self.slots[index];
        slot.assign(path);
        slot.length.store(0, Ordering::Relaxed);
        slot.state.store(SLOT_DELETED, Ordering::Release);
        Some(index)
    }

    pub(crate) fn view(&self, path: &str) -> Option<SpillView> {
        if path.len() > PATH_CAPACITY {
            return None;
        }
        self.locked(|| {
            let index = self.find(path)?;
            let slot = &self.slots[index];
            Some(SpillView {
                slot: index,
                generation: slot.generation.load(Ordering::Relaxed),
                length: slot.length.load(Ordering::Relaxed),
                deleted: slot.state.load(Ordering::Relaxed) == SLOT_DELETED,
            })
        })
    }

    pub(crate) fn paths_in_directory(&self, directory: &str) -> Vec<String> {
        self.locked(|| {
            self.slots
                .iter()
                .filter(|slot| slot.state.load(Ordering::Relaxed) != SLOT_EMPTY)
                .map(Slot::path_string)
                .filter(|path| {
                    path.strip_prefix(directory)
                        .is_some_and(|name| !name.is_empty() && !name.contains('/'))
                })
                .collect()
        })
    }

    fn apply<Platform: SystemInfoProvider>(
        &self,
        platform: &Platform,
        path: &str,
        edit: SpillEdit<'_>,
        copy_local_file: impl FnOnce(u32) -> Option<u64>,
    ) -> Option<AppliedEdit> {
        if path.len() > PATH_CAPACITY {
            return None;
        }
        self.locked(|| {
            let index = self.claim(path)?;
            let slot = &self.slots[index];
            let slot_id = index as u32;
            let was_live = slot.state.load(Ordering::Relaxed) == SLOT_LIVE;
            let current_length = slot.length.load(Ordering::Relaxed);
            let new_length = if edit.needs_local_copy(was_live) {
                copy_local_file(slot_id)?
            } else {
                match edit {
                    SpillEdit::Write { start, bytes } => {
                        platform.spill_write(slot_id, start as u64, bytes).then_some(())?;
                        current_length.max((start + bytes.len()) as u64)
                    }
                    SpillEdit::Truncate(length) => {
                        platform.spill_set_len(slot_id, length as u64).then_some(())?;
                        length as u64
                    }
                    SpillEdit::Reset | SpillEdit::Remove | SpillEdit::Replace => {
                        platform.spill_set_len(slot_id, 0).then_some(())?;
                        0
                    }
                }
            };
            let previous_generation = slot.generation.load(Ordering::Relaxed);
            slot.length.store(new_length, Ordering::Relaxed);
            slot.generation.store(previous_generation + 1, Ordering::Relaxed);
            let state = if matches!(edit, SpillEdit::Remove) {
                SLOT_DELETED
            } else {
                SLOT_LIVE
            };
            slot.state.store(state, Ordering::Release);
            Some(AppliedEdit {
                slot: index,
                previous_generation,
                new_generation: previous_generation + 1,
            })
        })
    }
}

fn is_spilled_path(path: &str) -> bool {
    SPILLED_PREFIXES.iter().any(|prefix| path.starts_with(prefix))
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    pub(crate) fn spill_enabled_for(&self, path: &str) -> bool {
        is_spilled_path(path) && self.global.platform.spill_available()
    }

    pub(crate) fn spilled_path_of_fd(&self, raw_fd: usize) -> Option<String> {
        let path = self.files.borrow().lookup_fd_path(raw_fd)?;
        let path = path.to_str().ok()?;
        self.spill_enabled_for(path).then(|| String::from(path))
    }

    pub(crate) fn refresh_from_spill(&self, path: &str) {
        if !self.spill_enabled_for(path) {
            return;
        }
        let Some(view) = self.global.shared_file_spill.view(path) else {
            return;
        };
        let seen = &LOCALLY_SEEN_GENERATION[view.slot];
        if seen.load(Ordering::Acquire) == view.generation {
            return;
        }
        if view.deleted {
            let _ = self.files.borrow().fs.unlink(path);
        } else {
            self.install_spilled_content(path, &view);
        }
        seen.store(view.generation, Ordering::Release);
    }

    pub(crate) fn refresh_spilled_directory(&self, directory: &str) {
        let directory = if directory.ends_with('/') {
            String::from(directory)
        } else {
            alloc::format!("{directory}/")
        };
        if !self.spill_enabled_for(&directory) {
            return;
        }
        for path in self.global.shared_file_spill.paths_in_directory(&directory) {
            self.refresh_from_spill(&path);
        }
    }

    fn install_spilled_content(&self, path: &str, view: &SpillView) {
        self.create_missing_parents(path);
        let files = self.files.borrow();
        let Ok(file) = files.fs.open(
            path,
            OFlags::WRONLY | OFlags::CREAT | OFlags::TRUNC,
            Mode::from_bits_truncate(0o644),
        ) else {
            return;
        };
        let mut chunk = vec![0u8; TRANSFER_CHUNK];
        let mut offset = 0u64;
        while offset < view.length {
            let wanted = chunk.len().min((view.length - offset) as usize);
            let read = self
                .global
                .platform
                .spill_read_at(view.slot as u32, offset, &mut chunk[..wanted]);
            if read == 0 || files.fs.write(&file, &chunk[..read], None).is_err() {
                break;
            }
            offset += read as u64;
        }
        let _ = files.fs.close(&file);
    }

    fn create_missing_parents(&self, path: &str) {
        let files = self.files.borrow();
        for (index, _) in path.match_indices('/').skip(1) {
            let _ = files
                .fs
                .mkdir(&path[..index], Mode::from_bits_truncate(0o755));
        }
    }

    fn copier_of_local_file<'a>(&'a self, path: &'a str) -> impl FnOnce(u32) -> Option<u64> + 'a {
        move |slot| {
            let files = self.files.borrow();
            let file = files.fs.open(path, OFlags::RDONLY, Mode::empty()).ok()?;
            let platform = self.global.platform;
            let mut chunk = vec![0u8; TRANSFER_CHUNK];
            let mut offset = 0u64;
            let mut complete = platform.spill_set_len(slot, 0);
            while complete {
                let Ok(read) = files.fs.read(&file, &mut chunk, Some(offset as usize)) else {
                    complete = false;
                    break;
                };
                if read == 0 {
                    break;
                }
                complete = platform.spill_write(slot, offset, &chunk[..read]);
                offset += read as u64;
            }
            let _ = files.fs.close(&file);
            complete.then_some(offset)
        }
    }

    pub(crate) fn publish_spilled(&self, path: &str, edit: SpillEdit<'_>) {
        if !self.spill_enabled_for(path) {
            return;
        }
        let Some(applied) = self.global.shared_file_spill.apply(
            self.global.platform,
            path,
            edit,
            self.copier_of_local_file(path),
        ) else {
            return;
        };
        let seen = &LOCALLY_SEEN_GENERATION[applied.slot];
        if seen.load(Ordering::Acquire) == applied.previous_generation {
            seen.store(applied.new_generation, Ordering::Release);
        }
    }

    pub(crate) fn publish_spilled_rename(&self, from: &str, to: &str) {
        self.publish_spilled(from, SpillEdit::Remove);
        self.publish_spilled(to, SpillEdit::Replace);
    }
}
