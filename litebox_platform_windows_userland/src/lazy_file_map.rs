//! Demand-paged private file mappings.
//!
//! A private file mapping is committed up front (so guest memory stays ordinary `VirtualAlloc`
//! memory, which fork, `mprotect` and unmap already understand) but left `PAGE_NOACCESS` and
//! unfilled. The first touch of each 64 KiB chunk faults into [`lazy_file_veh`], which copies just
//! that chunk from the layer's static backing bytes and applies the mapping's real protection.
//! Untouched chunks therefore never receive file data, so a process's private working set follows
//! the pages it actually uses instead of the size of every library it maps.
//!
//! Commit charge is unchanged (the range is still committed); only resident memory shrinks.

use core::sync::atomic::{AtomicBool, Ordering};
use std::collections::BTreeMap;
use std::ops::Range;
use std::cell::UnsafeCell;
use std::sync::{Mutex, MutexGuard, Once, OnceLock};
use core::sync::atomic::AtomicU32;

use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
    EXCEPTION_POINTERS,
};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{
    GetCurrentThreadId, OpenThread, WaitForSingleObject, THREAD_SYNCHRONIZE,
};
use windows_sys::Win32::System::Memory::{
    MEMORY_BASIC_INFORMATION, PAGE_EXECUTE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_PROTECTION_FLAGS, PAGE_READONLY,
    PAGE_READWRITE, PAGE_WRITECOPY, VirtualProtect, VirtualQuery,
};

const CHUNK_SHIFT: usize = 16;
const CHUNK_SIZE: usize = 1 << CHUNK_SHIFT;
const STATUS_ACCESS_VIOLATION: i32 = 0xC000_0005_u32 as i32;
const ACCESS_KIND_WRITE: usize = 1;
const ACCESS_KIND_EXECUTE: usize = 8;

/// Mappings smaller than this are copied eagerly; the fault bookkeeping would cost more than it saves.
pub(crate) const MIN_LAZY_LEN: usize = 4 * CHUNK_SIZE;

static HAS_RANGES: AtomicBool = AtomicBool::new(false);
static INSTALL_HANDLER: Once = Once::new();
static TABLE: DeadHolderLock = DeadHolderLock::new();
static SOURCES: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

struct LazyRange {
    end: usize,
    source: usize,
    source_len: usize,
    protection: PAGE_PROTECTION_FLAGS,
    first_chunk: usize,
    filled: Vec<bool>,
    /// Which registered source and where in it `source` points, so a fork child (which maps the
    /// same layers at different addresses) can re-derive the pointer.
    origin: Option<(usize, usize)>,
}

impl LazyRange {
    fn chunk_indices(&self) -> Range<usize> {
        self.first_chunk..self.first_chunk + self.filled.len()
    }

    fn is_filled(&self, chunk: usize) -> bool {
        self.filled[chunk - self.first_chunk]
    }
}

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("LITEBOX_LAZY_FILE_MAP").is_some_and(|v| v != "0")
    })
}

fn diagnostics_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LITEBOX_DIAG_LAZY_FILE_MAP").is_some())
}

/// The lazy table is touched from the access-violation handler, and `exit_group` terminates sibling
/// threads outright, so a thread can die while holding it. A `std` mutex would then block every
/// later `forget` (process exit) forever; this lock records its owner thread and lets a waiter
/// take it over once the owner is provably gone. The guarded map is only ever mutated by short,
/// allocation-only operations, so a taken-over table stays usable.
struct DeadHolderLock {
    owner: AtomicU32,
    table: UnsafeCell<BTreeMap<usize, LazyRange>>,
}

// SAFETY: access to `table` is serialized by `owner`.
unsafe impl Sync for DeadHolderLock {}

struct TableGuard(&'static DeadHolderLock);

impl DeadHolderLock {
    const fn new() -> Self {
        Self {
            owner: AtomicU32::new(0),
            table: UnsafeCell::new(BTreeMap::new()),
        }
    }

    fn lock(&'static self) -> TableGuard {
        // SAFETY: plain query of the calling thread's id.
        let me = unsafe { GetCurrentThreadId() };
        let mut spins = 0u32;
        loop {
            if self
                .owner
                .compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return TableGuard(self);
            }
            spins += 1;
            if spins % 64 == 0 {
                let holder = self.owner.load(Ordering::Relaxed);
                if holder != 0
                    && !thread_alive(holder)
                    && self
                        .owner
                        .compare_exchange(holder, me, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                {
                    return TableGuard(self);
                }
                std::thread::sleep(core::time::Duration::from_micros(200));
            } else {
                core::hint::spin_loop();
            }
        }
    }
}

fn thread_alive(tid: u32) -> bool {
    // SAFETY: the handle is closed before returning; a thread that cannot be opened is gone.
    unsafe {
        let handle = OpenThread(THREAD_SYNCHRONIZE, 0, tid);
        if handle.is_null() {
            return false;
        }
        let alive = WaitForSingleObject(handle, 0) != 0;
        CloseHandle(handle);
        alive
    }
}

impl core::ops::Deref for TableGuard {
    type Target = BTreeMap<usize, LazyRange>;
    fn deref(&self) -> &Self::Target {
        // SAFETY: this guard owns the lock.
        unsafe { &*self.0.table.get() }
    }
}

impl core::ops::DerefMut for TableGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: this guard owns the lock.
        unsafe { &mut *self.0.table.get() }
    }
}

impl Drop for TableGuard {
    fn drop(&mut self) {
        self.0.owner.store(0, Ordering::Release);
    }
}

fn table() -> TableGuard {
    TABLE.lock()
}

fn chunk_span(start: usize, range: &LazyRange, chunk: usize) -> Range<usize> {
    start.max(chunk << CHUNK_SHIFT)..range.end.min((chunk + 1) << CHUNK_SHIFT)
}

fn fill_chunk(start: usize, range: &mut LazyRange, chunk: usize) {
    if range.is_filled(chunk) {
        return;
    }
    let span = chunk_span(start, range, chunk);
    let offset = span.start - start;
    let copy_len = range.source_len.saturating_sub(offset).min(span.len());
    let mut previous = 0;
    // SAFETY: `span` lies inside a committed region this module registered; `source` is a
    // `'static` slice that outlives the process.
    unsafe {
        VirtualProtect(
            span.start as *const _,
            span.len(),
            PAGE_READWRITE,
            &mut previous,
        );
        core::ptr::copy_nonoverlapping(
            (range.source + offset) as *const u8,
            span.start as *mut u8,
            copy_len,
        );
        if range.protection != PAGE_READWRITE {
            VirtualProtect(
                span.start as *const _,
                span.len(),
                range.protection,
                &mut previous,
            );
        }
    }
    range.filled[chunk - range.first_chunk] = true;
    if diagnostics_enabled() {
        eprintln!(
            "[lazy_file_map] pid={} filled chunk {:#x}..{:#x}",
            std::process::id(),
            span.start,
            span.end
        );
    }
}

fn fill_all(start: usize, range: &mut LazyRange) {
    for chunk in range.chunk_indices() {
        fill_chunk(start, range, chunk);
    }
}

fn split_at(map: &mut BTreeMap<usize, LazyRange>, addr: usize) {
    let Some((&start, range)) = map.range_mut(..addr).next_back() else {
        return;
    };
    if addr >= range.end {
        return;
    }
    let consumed = addr - start;
    let last_chunk = (range.end - 1) >> CHUNK_SHIFT;
    let upper_first = addr >> CHUNK_SHIFT;
    let upper = LazyRange {
        end: range.end,
        source: range.source + consumed,
        source_len: range.source_len.saturating_sub(consumed),
        protection: range.protection,
        first_chunk: upper_first,
        filled: range.filled[upper_first - range.first_chunk..=last_chunk - range.first_chunk]
            .to_vec(),
        origin: range.origin.map(|(index, offset)| (index, offset + consumed)),
    };
    range.end = addr;
    range
        .filled
        .truncate(((addr - 1) >> CHUNK_SHIFT) - range.first_chunk + 1);
    map.insert(addr, upper);
}

fn push_merged(out: &mut Vec<Range<usize>>, next: Range<usize>) {
    if next.is_empty() {
        return;
    }
    match out.last_mut() {
        Some(last) if last.end == next.start => last.end = next.end,
        _ => out.push(next),
    }
}

fn access_now_allowed(addr: usize, kind: usize) -> bool {
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: querying an arbitrary address is always sound.
    let queried = unsafe {
        VirtualQuery(
            addr as *const _,
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if queried == 0 || info.Protect & PAGE_GUARD != 0 {
        return false;
    }
    let allowed: &[PAGE_PROTECTION_FLAGS] = match kind {
        ACCESS_KIND_WRITE => &[
            PAGE_READWRITE,
            PAGE_WRITECOPY,
            PAGE_EXECUTE_READWRITE,
            PAGE_EXECUTE_WRITECOPY,
        ],
        ACCESS_KIND_EXECUTE => &[
            PAGE_EXECUTE,
            PAGE_EXECUTE_READ,
            PAGE_EXECUTE_READWRITE,
            PAGE_EXECUTE_WRITECOPY,
        ],
        _ => &[
            PAGE_READONLY,
            PAGE_READWRITE,
            PAGE_WRITECOPY,
            PAGE_EXECUTE_READ,
            PAGE_EXECUTE_READWRITE,
            PAGE_EXECUTE_WRITECOPY,
        ],
    };
    allowed.contains(&info.Protect)
}

unsafe extern "system" fn lazy_file_veh(info: *mut EXCEPTION_POINTERS) -> i32 {
    if !HAS_RANGES.load(Ordering::Acquire) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // SAFETY: the OS hands the handler valid exception pointers.
    let record = unsafe { &*(*info).ExceptionRecord };
    if record.ExceptionCode != STATUS_ACCESS_VIOLATION || record.NumberParameters < 2 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let kind = record.ExceptionInformation[0];
    let addr = record.ExceptionInformation[1];
    let mut map = table();
    let Some((&start, range)) = map.range_mut(..=addr).next_back() else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    if addr >= range.end {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let chunk = addr >> CHUNK_SHIFT;
    if range.is_filled(chunk) {
        // Another thread may have filled this chunk between the fault and taking the lock.
        return if access_now_allowed(addr, kind) {
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        };
    }
    fill_chunk(start, range, chunk);
    EXCEPTION_CONTINUE_EXECUTION
}

/// Turns the freshly created, committed, read-write `range` into an unfilled lazy mapping of
/// `source`. Returns `false` (leaving the range untouched) when it cannot be armed.
pub(crate) fn register(range: Range<usize>, source: &'static [u8]) -> bool {
    if range.len() < MIN_LAZY_LEN || !range.start.is_multiple_of(4096) {
        return false;
    }
    let mut previous = 0;
    // SAFETY: the caller just created this range as committed read-write memory.
    let armed = unsafe {
        VirtualProtect(
            range.start as *const _,
            range.len(),
            PAGE_NOACCESS,
            &mut previous,
        )
    };
    if armed == 0 {
        return false;
    }
    INSTALL_HANDLER.call_once(|| {
        // SAFETY: registering a process-wide handler; it only claims faults inside registered ranges.
        unsafe { AddVectoredExceptionHandler(1, Some(lazy_file_veh)) };
    });
    if diagnostics_enabled() {
        eprintln!(
            "[lazy_file_map] pid={} armed {:#x}..{:#x} ({} KiB, source {} KiB)",
            std::process::id(),
            range.start,
            range.end,
            range.len() >> 10,
            source.len() >> 10
        );
    }
    let first_chunk = range.start >> CHUNK_SHIFT;
    let last_chunk = (range.end - 1) >> CHUNK_SHIFT;
    table().insert(
        range.start,
        LazyRange {
            end: range.end,
            source: source.as_ptr() as usize,
            source_len: source.len(),
            protection: PAGE_READWRITE,
            first_chunk,
            filled: vec![false; last_chunk - first_chunk + 1],
            origin: locate_source(source),
        },
    );
    HAS_RANGES.store(true, Ordering::Release);
    true
}

/// Records `protection` as the final protection of every lazy range inside `range` and returns the
/// sub-ranges that must actually be re-protected now: everything except still-unfilled chunks,
/// which pick the new protection up when they are filled.
pub(crate) fn permission_update_ranges(
    range: Range<usize>,
    protection: PAGE_PROTECTION_FLAGS,
) -> Vec<Range<usize>> {
    if !HAS_RANGES.load(Ordering::Acquire) {
        return vec![range];
    }
    let mut map = table();
    split_at(&mut map, range.start);
    split_at(&mut map, range.end);
    let starts: Vec<usize> = map.range(range.clone()).map(|(&s, _)| s).collect();
    let mut out = Vec::new();
    let mut cursor = range.start;
    for start in starts {
        let lazy = map.get_mut(&start).expect("start listed just above");
        push_merged(&mut out, cursor..start);
        lazy.protection = protection;
        if protection == PAGE_NOACCESS {
            fill_all(start, lazy);
        }
        for chunk in lazy.chunk_indices() {
            if lazy.is_filled(chunk) {
                push_merged(&mut out, chunk_span(start, lazy, chunk));
            }
        }
        cursor = lazy.end;
    }
    push_merged(&mut out, cursor..range.end);
    out
}

/// Drops lazy bookkeeping for `range` (unmapped or about to be replaced).
pub(crate) fn forget(range: Range<usize>) {
    if !HAS_RANGES.load(Ordering::Acquire) {
        return;
    }
    let mut map = table();
    split_at(&mut map, range.start);
    split_at(&mut map, range.end);
    let inside: Vec<usize> = map.range(range).map(|(&s, _)| s).collect();
    for start in inside {
        map.remove(&start);
    }
}

fn sources() -> MutexGuard<'static, Vec<(usize, usize)>> {
    SOURCES.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Declares that `data` is source number `index`. The numbering must be the same in every process
/// of a fork tree (the layer order of the image), because fork children find their sources by it.
pub fn register_source(index: usize, data: &'static [u8]) {
    let mut all = sources();
    if all.len() <= index {
        all.resize(index + 1, (0, 0));
    }
    all[index] = (data.as_ptr() as usize, data.len());
}

fn locate_source(slice: &[u8]) -> Option<(usize, usize)> {
    let start = slice.as_ptr() as usize;
    sources().iter().enumerate().find_map(|(index, &(base, len))| {
        (len != 0 && start >= base && start + slice.len() <= base + len)
            .then_some((index, start - base))
    })
}

/// Prepares this process's lazy ranges for a cross-process fork and returns the path of a
/// descriptor file the child re-arms them from. Ranges whose source cannot be identified are
/// filled now so the ordinary page copy carries them. Unfilled chunks are `PAGE_NOACCESS`, hence
/// unreadable, hence skipped by that copy: the child gets zero pages it re-arms, not data.
pub(crate) fn export_for_fork() -> Option<String> {
    if !HAS_RANGES.load(Ordering::Acquire) {
        return None;
    }
    let mut map = table();
    let mut lines = String::new();
    for (&start, lazy) in map.iter_mut() {
        if lazy.origin.is_none() {
            fill_all(start, lazy);
        }
        let Some((index, offset)) = lazy.origin else {
            continue;
        };
        if lazy.filled.iter().all(|&filled| filled) {
            continue;
        }
        let bits: String = lazy.filled.iter().map(|&f| if f { '1' } else { '0' }).collect();
        lines.push_str(&format!(
            "{start:x} {:x} {index} {offset:x} {:x} {:x} {bits}
",
            lazy.end, lazy.source_len, lazy.protection
        ));
    }
    drop(map);
    if lines.is_empty() {
        return None;
    }
    static SEQUENCE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "litebox-lazyfile-{}-{}.txt",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, lines).ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// Child side of [`export_for_fork`]: re-arms every described range against this process's own
/// mapping of the same source, then deletes the file.
pub fn adopt_from_file(path: &str) {
    let Ok(text) = std::fs::read_to_string(path) else {
        panic!("lazy file map descriptor {path} unreadable");
    };
    let _ = std::fs::remove_file(path);
    let hex = |field: &str| usize::from_str_radix(field, 16).expect("descriptor hex field");
    INSTALL_HANDLER.call_once(|| {
        // SAFETY: registering a process-wide handler; it only claims faults inside registered ranges.
        unsafe { AddVectoredExceptionHandler(1, Some(lazy_file_veh)) };
    });
    let mut map = table();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let [start, end, index, offset, source_len, protection, bits] = fields[..] else {
            panic!("malformed lazy file map descriptor line: {line}");
        };
        let (start, end) = (hex(start), hex(end));
        let index: usize = index.parse().expect("descriptor source index");
        let offset = hex(offset);
        let (base, total) = sources()
            .get(index)
            .copied()
            .filter(|&(_, total)| total != 0)
            .unwrap_or_else(|| panic!("lazy file map source {index} not registered in fork child"));
        assert!(offset + hex(source_len) <= total, "lazy file map source {index} shorter in child");
        let mut lazy = LazyRange {
            end,
            source: base + offset,
            source_len: hex(source_len),
            protection: hex(protection) as PAGE_PROTECTION_FLAGS,
            first_chunk: start >> CHUNK_SHIFT,
            filled: bits.chars().map(|bit| bit == '1').collect(),
            origin: Some((index, offset)),
        };
        for chunk in lazy.chunk_indices() {
            if !lazy.is_filled(chunk) {
                let span = chunk_span(start, &lazy, chunk);
                let mut previous = 0;
                // SAFETY: the parent's group copy committed this whole range in this process.
                unsafe {
                    VirtualProtect(span.start as *const _, span.len(), PAGE_NOACCESS, &mut previous);
                }
            }
        }
        lazy.filled.shrink_to_fit();
        map.insert(start, lazy);
    }
    HAS_RANGES.store(true, Ordering::Release);
}
