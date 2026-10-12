// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use alloc::{string::String, vec, vec::Vec};
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use litebox::{
    fs::{Mode, OFlags},
    platform::SystemInfoProvider,
};

use crate::{ShimFS, ShimPlatform, Task};

const SPILLED_PREFIXES: &[&str] = &[
    "/var/lib/apt/",
    "/var/cache/apt/",
    "/tmp/.config/chromium",
    "/tmp/.cache/chromium",
    "/root/.config/chromium",
    "/root/.cache/chromium",
    "/tmp/org.chromium.",
];
const SLOT_COUNT: usize = 1024;
const PATH_CAPACITY: usize = 192;
const TRANSFER_CHUNK: usize = 256 * 1024;
const LOCK_SPIN_LIMIT: u32 = 400_000_000;

/// The token a host process that has no cross-process identity locks under: `current_host_pid()`
/// is `0` on a platform without cross-process delivery, and `0` is this lock's "free" value.
const LOCK_TOKEN_NO_HOST_PID: u32 = u32::MAX;

/// How often a waiter asks whether the holder is still alive. `is_process_alive` is a host query,
/// so it cannot be asked once per spin.
const DEAD_HOLDER_CHECK_EVERY: u32 = 1 << 16;

/// `LITEBOX_SPILL_LOCK_BARGE` re-enables the pre-fix behaviour, cached because `locked` is on the
/// path of every spilled write. 0 = unread, 1 = barge, 2 = strict.
static SPILL_LOCK_BARGE: AtomicU8 = AtomicU8::new(0);

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
    /// An `O_APPEND` write: `local_start` is where this process put the bytes, which is only a
    /// guess at where they belong -- real Linux resolves an append's destination at write time, so
    /// the store does too (see `apply`).
    Append { bytes: &'a [u8], local_start: usize },
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
    /// True when the store did NOT put this edit where this process's own copy put it, so the local
    /// copy is now wrong and must be replaced from the store before it is read again.
    local_diverged: bool,
}

/// How a waiter may become the owner of the shared file-spill lock.
enum LockPolicy {
    /// Wait for the holder to release. A dead holder is recovered from, so this never waits on a
    /// lock that nobody will release.
    Wait,
    /// The pre-fix behaviour, reachable only through `LITEBOX_SPILL_LOCK_BARGE`: give up after
    /// this many spins and enter the critical section WITHOUT owning the lock.
    BargeAfter(u32),
}

enum Acquired {
    Fresh,
    Recovered { dead_owner: u32 },
    /// Still held by `owner`. Only `LockPolicy::BargeAfter` may proceed after this, and doing so
    /// is the interleaving the lock exists to prevent.
    Contended { owner: u32 },
}

fn policy_for(barge: bool) -> LockPolicy {
    if barge {
        LockPolicy::BargeAfter(LOCK_SPIN_LIMIT)
    } else {
        LockPolicy::Wait
    }
}

fn spill_lock_policy<Platform: ShimPlatform>(platform: &Platform) -> LockPolicy {
    match SPILL_LOCK_BARGE.load(Ordering::Relaxed) {
        0 => {
            let barge = platform.env_value("LITEBOX_SPILL_LOCK_BARGE").is_some();
            SPILL_LOCK_BARGE.store(if barge { 1 } else { 2 }, Ordering::Relaxed);
            policy_for(barge)
        }
        1 => policy_for(true),
        _ => policy_for(false),
    }
}

impl SharedFileSpill {
    pub(crate) fn new() -> Self {
        Self {
            lock: AtomicU32::new(0),
            slots: core::array::from_fn(|_| Slot::new()),
        }
    }

    /// A lock that gives up is not a lock: the store's bytes, lengths and generations are only
    /// coherent while exactly one actor is inside, and the old `locked` ran its body after
    /// `LOCK_SPIN_LIMIT` spins whether or not it had ever acquired anything -- two host processes
    /// then interleaved inside one critical section, which is exactly the interleaving the lock
    /// exists to prevent. The only holder that can fail to release is one that DIED holding the
    /// lock, so ownership now transfers out of a provably-dead host process and out of nothing
    /// else, and the lock word records WHO holds it so that is decidable.
    ///
    /// A live holder is a holder that is still making progress: this critical section is bounded
    /// host I/O, so a waiter spins instead of entering. A waiter of the SAME host pid is another
    /// thread of this process (a multi-threaded guest), whose hold is likewise bounded.
    fn acquire(
        &self,
        me: u32,
        is_alive: &impl Fn(u32) -> bool,
        policy: LockPolicy,
    ) -> Acquired {
        let mut spins = 0u32;
        loop {
            match self
                .lock
                .compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return Acquired::Fresh,
                Err(owner) => {
                    let recoverable = me != LOCK_TOKEN_NO_HOST_PID
                        && owner != LOCK_TOKEN_NO_HOST_PID
                        && spins % DEAD_HOLDER_CHECK_EVERY == 0
                        && !is_alive(owner);
                    if recoverable
                        && self
                            .lock
                            .compare_exchange(owner, me, Ordering::Acquire, Ordering::Relaxed)
                            .is_ok()
                    {
                        return Acquired::Recovered { dead_owner: owner };
                    }
                    spins = spins.wrapping_add(1);
                    if spins % LOCK_SPIN_LIMIT == 0 {
                        litebox_util_log::warn!(
                            owner:% = owner, me:% = me;
                            "spill-lock: a host process has held the shared file-spill lock across a very long wait"
                        );
                    }
                    if let LockPolicy::BargeAfter(limit) = policy {
                        if spins >= limit {
                            return Acquired::Contended { owner };
                        }
                    }
                    core::hint::spin_loop();
                }
            }
        }
    }

    fn locked_body<R>(
        &self,
        me: u32,
        is_alive: &impl Fn(u32) -> bool,
        policy: LockPolicy,
        body: impl FnOnce() -> R,
    ) -> R {
        match self.acquire(me, is_alive, policy) {
            Acquired::Fresh => {}
            Acquired::Recovered { dead_owner } => litebox_util_log::warn!(
                dead_owner:% = dead_owner, me:% = me;
                "spill-lock: a host process died holding the shared file-spill lock, ownership recovered"
            ),
            Acquired::Contended { owner } => litebox_util_log::warn!(
                owner:% = owner, me:% = me;
                "spill-lock: BARGE - running the critical section without owning the lock (LITEBOX_SPILL_LOCK_BARGE)"
            ),
        }
        let result = body();
        self.lock.store(0, Ordering::Release);
        result
    }

    fn locked<Platform: ShimPlatform, R>(
        &self,
        platform: &Platform,
        body: impl FnOnce() -> R,
    ) -> R {
        let me = match platform.current_host_pid() {
            0 => LOCK_TOKEN_NO_HOST_PID,
            pid => pid,
        };
        self.locked_body(
            me,
            &|pid| platform.is_process_alive(pid),
            spill_lock_policy(platform),
            body,
        )
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

    /// Move a file's store row to a new path instead of re-publishing this process's copy of it.
    ///
    /// A rename moves the file, it does not rewrite it, so the store's bytes for `from` ARE the
    /// bytes the renamed file has. Re-publishing this process's local copy instead (`Replace`)
    /// writes whatever this process last saw: another host process that published between this
    /// process's `refresh_from_spill` and its `rename(2)` has its bytes silently reverted, which
    /// real Linux cannot do. Moving the row cannot lose them, and the move happens under the spill
    /// lock, so no writer can interleave with it.
    fn move_slot<Platform: ShimPlatform>(
        &self,
        platform: &Platform,
        from: &str,
        to: &str,
    ) -> Option<AppliedEdit> {
        if from.len() > PATH_CAPACITY || to.len() > PATH_CAPACITY {
            return None;
        }
        self.locked(platform, || {
            let index = self.find(from)?;
            // Renaming onto a live path replaces it, as Linux does: the destination's own row has to
            // go, or two rows would claim the same path and `find` would answer either.
            if let Some(victim) = self.find(to) {
                if victim != index {
                    let slot = &self.slots[victim];
                    slot.path_len.store(0, Ordering::Relaxed);
                    slot.length.store(0, Ordering::Relaxed);
                    slot.state.store(SLOT_DELETED, Ordering::Release);
                }
            }
            let slot = &self.slots[index];
            slot.assign(to);
            let previous_generation = slot.generation.load(Ordering::Relaxed);
            slot.generation.store(previous_generation + 1, Ordering::Relaxed);
            slot.state.store(SLOT_LIVE, Ordering::Release);
            Some(AppliedEdit {
                slot: index,
                previous_generation,
                new_generation: previous_generation + 1,
                local_diverged: false,
            })
        })
    }

    pub(crate) fn view<Platform: ShimPlatform>(
        &self,
        platform: &Platform,
        path: &str,
    ) -> Option<SpillView> {
        if path.len() > PATH_CAPACITY {
            return None;
        }
        self.locked(platform, || {
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

    pub(crate) fn paths_in_directory<Platform: ShimPlatform>(
        &self,
        platform: &Platform,
        directory: &str,
    ) -> Vec<String> {
        self.locked(platform, || {
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

    fn apply<Platform: ShimPlatform>(
        &self,
        platform: &Platform,
        path: &str,
        edit: SpillEdit<'_>,
        copy_local_file: impl FnOnce(u32) -> Option<u64>,
    ) -> Option<AppliedEdit> {
        if path.len() > PATH_CAPACITY {
            return None;
        }
        self.locked(platform, || {
            let index = self.claim(path)?;
            let slot = &self.slots[index];
            let slot_id = index as u32;
            let was_live = slot.state.load(Ordering::Relaxed) == SLOT_LIVE;
            let current_length = slot.length.load(Ordering::Relaxed);
            let mut diverged = false;
            let new_length = if edit.needs_local_copy(was_live) {
                copy_local_file(slot_id)?
            } else {
                match edit {
                    SpillEdit::Write { start, bytes } => {
                        platform.spill_write(slot_id, start as u64, bytes).then_some(())?;
                        current_length.max((start + bytes.len()) as u64)
                    }
                    // Real Linux resolves an `O_APPEND` write's destination AT WRITE TIME, under
                    // the inode's own exclusion, which is why two appending writers never overwrite
                    // each other. The spill lock is that exclusion here: `current_length` is read
                    // and the new length published inside the same critical section, so a sibling
                    // that appends next starts from this write's end, not from a snapshot of it.
                    SpillEdit::Append { bytes, local_start } => {
                        platform
                            .spill_write(slot_id, current_length, bytes)
                            .then_some(())?;
                        diverged = local_start != current_length as usize;
                        current_length + bytes.len() as u64
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
                local_diverged: diverged,
            })
        })
    }
}

fn is_spilled_path(path: &str) -> bool {
    SPILLED_PREFIXES.iter().any(|prefix| path.starts_with(prefix))
}

/// A path the host asked to share on top of [`SPILLED_PREFIXES`]:
/// `LITEBOX_SHARED_WRITE_PREFIXES=/tmp/lk/;/tmp/co/`.
///
/// The built-in list is a fixed guess at which paths a guest writes heavily AND wants shared; a
/// file two host processes both write is only known at run time, so the list is extensible from
/// the host instead of by rebuilding the shim.
fn is_shared_write_path<Platform: SystemInfoProvider>(platform: &Platform, path: &str) -> bool {
    is_spilled_path(path)
        || platform
            .env_value("LITEBOX_SHARED_WRITE_PREFIXES")
            .is_some_and(|prefixes| {
                prefixes
                    .split([';', ':'])
                    .any(|prefix| !prefix.is_empty() && path.starts_with(prefix))
            })
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    pub(crate) fn spill_enabled_for(&self, path: &str) -> bool {
        is_shared_write_path(self.global.platform, path) && self.global.platform.spill_available()
    }

    pub(crate) fn spilled_path_of_fd(&self, raw_fd: usize) -> Option<String> {
        let path = self.files.borrow().lookup_fd_path(raw_fd)?;
        let path = path.to_str().ok()?;
        self.spill_enabled_for(path).then(|| String::from(path))
    }

    /// Pull the shared store's current bytes into this process's private copy before a read or a
    /// write of `raw_fd`.
    ///
    /// A spilled file's authoritative bytes are the store's, and another host process can publish
    /// into that store at any moment; the private copy is otherwise frozen at whatever this process
    /// last opened or wrote. A long-lived reader (a sqlite connection held across many transactions)
    /// then answers every later read from that snapshot and writes back pages computed from it,
    /// which is how a whole worker's rows went missing in `lockapp1 contend` even though its writes
    /// reached the store. `refresh_from_spill` is a no-op unless the store's generation moved on, so
    /// a process only pays for installs caused by SOMEONE ELSE's writes, never its own.
    pub(crate) fn sync_spilled_fd(&self, raw_fd: usize) {
        if let Some(path) = self.spilled_path_of_fd(raw_fd) {
            self.refresh_from_spill(path.as_str());
        }
    }

    pub(crate) fn refresh_from_spill(&self, path: &str) {
        if !self.spill_enabled_for(path) {
            return;
        }
        let Some(view) = self.global.shared_file_spill.view(self.global.platform, path) else {
            return;
        };
        let seen = &LOCALLY_SEEN_GENERATION[view.slot];
        if seen.load(Ordering::Acquire) == view.generation {
            return;
        }
        litebox::fs::with_root_identity(|| {
            if view.deleted {
                let _ = self.files.borrow().fs.unlink(path);
            } else {
                self.install_spilled_content(path, &view);
            }
        });
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
        for path in self
            .global
            .shared_file_spill
            .paths_in_directory(self.global.platform, &directory)
        {
            self.refresh_from_spill(&path);
        }
    }

    fn install_spilled_content(&self, path: &str, view: &SpillView) {
        self.create_missing_parents(path);
        let mut chunk = vec![0u8; TRANSFER_CHUNK];
        let mut offset = 0u64;
        let mut opened: Option<_> = None;
        let files = self.files.borrow();
        while offset < view.length {
            let wanted = chunk.len().min((view.length - offset) as usize);
            let read = self
                .global
                .platform
                .spill_read_at(view.slot as u32, offset, &mut chunk[..wanted]);
            if read == 0 {
                break;
            }
            // The store is asked for bytes BEFORE the local file is truncated, never after: an
            // `O_TRUNC` open followed by a store read that yields nothing would leave the guest
            // holding an empty file while the store still believes it has `view.length` bytes --
            // unrecoverable loss, and `sync_spilled_fd` makes this run on every read, not just at
            // open, so there is no longer a rare path for that to hide behind.
            if opened.is_none() {
                self.create_missing_parents(path);
                let Ok(file) = files.fs.open(
                    path,
                    OFlags::WRONLY | OFlags::CREAT | OFlags::TRUNC,
                    Mode::from_bits_truncate(0o644),
                ) else {
                    return;
                };
                opened = Some(file);
            }
            let Some(file) = opened.as_ref() else {
                return;
            };
            if files.fs.write(file, &chunk[..read], None).is_err() {
                break;
            }
            offset += read as u64;
        }
        if let Some(file) = opened.as_ref() {
            let _ = files.fs.close(file);
        }
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
        move |slot| litebox::fs::with_root_identity(|| {
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
        })
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
        // Advancing `seen` claims "this process's private copy already matches the store". It only
        // does when the copy was current as of `previous_generation` AND the store put this edit
        // where this process put it; otherwise the next read must install the store's bytes, and
        // leaving `seen` behind is exactly what makes `refresh_from_spill` do that.
        if seen.load(Ordering::Acquire) == applied.previous_generation
            && !applied.local_diverged
        {
            seen.store(applied.new_generation, Ordering::Release);
        }
    }

    pub(crate) fn publish_spilled_rename(&self, from: &str, to: &str) {
        if !self.spill_enabled_for(from) && !self.spill_enabled_for(to) {
            return;
        }
        if let Some(applied) = self
            .global
            .shared_file_spill
            .move_slot(self.global.platform, from, to)
        {
            // The move does not touch the bytes, so a local copy that already matched the store
            // still matches it; one that was behind stays behind, so the next read installs.
            let seen = &LOCALLY_SEEN_GENERATION[applied.slot];
            if seen.load(Ordering::Acquire) == applied.previous_generation {
                seen.store(applied.new_generation, Ordering::Release);
            }
            return;
        }
        // No row for `from` (never published): fall back to publishing this process's copy of `to`,
        // which after the rename is the moved file.
        self.publish_spilled(from, SpillEdit::Remove);
        self.publish_spilled(to, SpillEdit::Replace);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: u32 = 91;
    const OTHER: u32 = 77;

    fn nobody_alive(_pid: u32) -> bool {
        false
    }

    fn everybody_alive(_pid: u32) -> bool {
        true
    }

    fn held_by(spill: &SharedFileSpill, owner: u32) {
        spill.lock.store(owner, Ordering::Relaxed);
    }

    #[test]
    fn a_free_lock_is_taken_immediately() {
        let spill = SharedFileSpill::new();
        assert!(matches!(
            spill.acquire(ME, &everybody_alive, LockPolicy::Wait),
            Acquired::Fresh
        ));
        assert_eq!(spill.lock.load(Ordering::Relaxed), ME);
    }

    #[test]
    fn a_dead_holder_is_recovered_from() {
        let spill = SharedFileSpill::new();
        held_by(&spill, OTHER);
        assert!(matches!(
            spill.acquire(ME, &nobody_alive, LockPolicy::Wait),
            Acquired::Recovered { dead_owner: OTHER }
        ));
        assert_eq!(spill.lock.load(Ordering::Relaxed), ME);
    }

    /// The fix: a LIVE holder keeps the lock. `LockedPolicy::BargeAfter` is what reports
    /// "contended" at all -- under `Wait` the caller never gets here, it keeps waiting.
    #[test]
    fn a_live_holder_is_never_entered_on() {
        let spill = SharedFileSpill::new();
        held_by(&spill, OTHER);
        assert!(matches!(
            spill.acquire(ME, &everybody_alive, LockPolicy::BargeAfter(64)),
            Acquired::Contended { owner: OTHER }
        ));
        assert_eq!(
            spill.lock.load(Ordering::Relaxed),
            OTHER,
            "a live holder must still own the lock"
        );
    }

    /// What `LITEBOX_SPILL_LOCK_BARGE` restores, kept as the OFF switch's own regression: the body
    /// runs and observes ANOTHER process's ownership, i.e. two actors inside one critical section.
    #[test]
    fn the_legacy_barge_runs_the_body_unowned() {
        let spill = SharedFileSpill::new();
        held_by(&spill, OTHER);
        let owner_during_body =
            spill.locked_body(ME, &everybody_alive, LockPolicy::BargeAfter(64), || {
                spill.lock.load(Ordering::Relaxed)
            });
        assert_eq!(owner_during_body, OTHER);
    }

    #[test]
    fn the_switch_selects_wait_or_barge() {
        assert!(matches!(policy_for(false), LockPolicy::Wait));
        assert!(matches!(
            policy_for(true),
            LockPolicy::BargeAfter(LOCK_SPIN_LIMIT)
        ));
    }
}
