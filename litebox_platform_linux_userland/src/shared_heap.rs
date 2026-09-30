//! A `#[global_allocator]` whose memory lives in one `MAP_SHARED` arena, so every process in a
//! native-`fork()` family sees the same heap at the same addresses.
//!
//! LiteBox keeps its kernel state (pipes, the in-memory filesystem layer, sockets, futex tables)
//! in ordinary heap objects. With a private heap, `fork()` gives each child its own copy-on-write
//! duplicate and the guest processes stop seeing each other's writes. Placing the whole Rust heap
//! in shared memory makes that state genuinely shared; per-process state (the `Task` itself, the
//! guest's own mappings, thread stacks) never touches the Rust heap's arena and stays private.
//!
//! On by default; `LITEBOX_SHARED_HEAP=0` forwards every call to the system allocator instead.
//! Blocks are power-of-two size classes with per-class free lists, all guarded by one spin lock
//! that itself lives in the arena, so it is honoured across processes.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::alloc::System;

const ARENA_BASE: usize = 0x6000_0000_0000;
const ARENA_SIZE: usize = 64 << 30;
const CLASSES: usize = 48;
const MIN_CLASS: u32 = 4; // 16 bytes
const MAX_ALIGN: usize = 4096;

#[repr(C)]
struct Header {
    lock: AtomicUsize,
    bump: AtomicUsize,
    free: [AtomicUsize; CLASSES],
    /// Host fd of the inherited shared-memory pool (`-1` when there is none).
    pool_fd: AtomicUsize,
    pool_bump: AtomicUsize,
    pool_lock: AtomicUsize,
    pool_table: [PoolEntry; POOL_ENTRIES],
}

/// One named segment of the pool: `key` 0 marks an unused slot.
#[repr(C)]
struct PoolEntry {
    key: AtomicUsize,
    offset: AtomicUsize,
    size: AtomicUsize,
}

const POOL_ENTRIES: usize = 128;
const POOL_SIZE: usize = 64 << 30;
/// Tag on a pool handle: the low bits are the segment's byte offset in the pool memfd.
pub const POOL_HANDLE_TAG: usize = 1 << 62;
const POOL_OFFSET_MASK: usize = (1 << 48) - 1;

const HEADER_SIZE: usize = 4096;
/// Freed blocks at least this big give their pages back to the host.
const RELEASE_MIN: usize = 1 << 20;

const UNINIT: u8 = 0;
const BUSY: u8 = 1;
const READY: u8 = 2;
const DISABLED: u8 = 3;

thread_local! {
    /// Nesting depth of "allocate privately" scopes on this thread; see [`private_scope`].
    static PRIVATE_DEPTH: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
    /// Whether this thread may allocate from the shared arena. Threads that std spawns start
    /// out `false`: std keeps per-thread bookkeeping in process-wide statics (the stack-overflow
    /// handler registry, for one) that are created and torn down by std code before and after
    /// ours runs, and blocks a native-fork child inherits through such a static would be mutated
    /// by both processes. Guest-visible threads opt in with [`mark_shared_thread`].
    static SHARED_OK: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// Lets (`true`) or stops (`false`) the current thread allocating from the shared arena.
pub fn mark_shared_thread(on: bool) {
    let _ = SHARED_OK.try_with(|s| s.set(on));
}

/// The inherited pool memfd, when the shared heap is active and it could be created.
#[must_use]
pub fn pool_fd() -> Option<usize> {
    if !is_active() {
        return None;
    }
    // SAFETY: `is_active()` implies the arena (and so its header) is mapped.
    let fd = unsafe { &*(ARENA_BASE as *const Header) }
        .pool_fd
        .load(Ordering::Acquire);
    (fd != usize::MAX).then_some(fd)
}

/// Finds (`key != 0`) or reserves a `size`-byte pool segment, returning its pool handle.
/// `key == 0` always reserves a fresh anonymous segment.
#[must_use]
pub fn pool_segment(key: usize, size: usize) -> Option<usize> {
    pool_fd()?;
    let size = size.checked_add(4095)? & !4095;
    // SAFETY: as in `pool_fd`.
    let h = unsafe { &*(ARENA_BASE as *const Header) };
    while h
        .pool_lock
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    let mut found = None;
    if key != 0 {
        found = h
            .pool_table
            .iter()
            .find(|e| e.key.load(Ordering::Relaxed) == key)
            .map(|e| e.offset.load(Ordering::Relaxed));
    }
    if found.is_none() {
        let off = h.pool_bump.load(Ordering::Relaxed);
        if off + size <= POOL_SIZE {
            let slot = if key == 0 {
                None
            } else {
                h.pool_table
                    .iter()
                    .find(|e| e.key.load(Ordering::Relaxed) == 0)
            };
            if key == 0 || slot.is_some() {
                if let Some(e) = slot {
                    e.offset.store(off, Ordering::Relaxed);
                    e.size.store(size, Ordering::Relaxed);
                    e.key.store(key, Ordering::Relaxed);
                }
                h.pool_bump.store(off + size, Ordering::Relaxed);
                found = Some(off);
            }
        }
    }
    h.pool_lock.store(0, Ordering::Release);
    found.map(|off| POOL_HANDLE_TAG | off)
}

/// The byte offset a pool handle names.
#[must_use]
pub fn pool_offset(handle: usize) -> usize {
    handle & POOL_OFFSET_MASK
}

/// Enters (`true`) or leaves (`false`) a scope in which this thread's allocations come from the
/// system heap instead of the shared arena.
///
/// Rust `thread_local!` values (the tracing formatter's line buffer, for one) are heap-allocated
/// lazily by the thread that first touches them. If that block is in the shared arena, a forked
/// child's copy of the thread's TLS points at the PARENT thread's block and both then write into
/// it. Code that keeps per-thread heap state -- logging -- runs inside a private scope so that
/// state is ordinary per-process memory that `fork()` duplicates.
pub fn private_scope(enter: bool) {
    let _ = PRIVATE_DEPTH.try_with(|d| {
        d.set(if enter {
            d.get() + 1
        } else {
            d.get().saturating_sub(1)
        });
    });
}

fn is_private() -> bool {
    PRIVATE_DEPTH.try_with(|d| d.get() > 0).unwrap_or(false)
        || !SHARED_OK.try_with(core::cell::Cell::get).unwrap_or(true)
}

/// Releases the heap lock if the calling thread holds it: only for a thread that is about to
/// die from a panic raised inside an allocator critical section, so the panic message itself can
/// still allocate.
pub fn release_lock_if_held_by_current_thread() {
    if !is_active() {
        return;
    }
    // SAFETY: `is_active()` implies the arena (and so its header) is mapped.
    let h = unsafe { &*(ARENA_BASE as *const Header) };
    // SAFETY: gettid has no arguments and cannot fail.
    let me = unsafe { libc::syscall(libc::SYS_gettid) } as usize;
    let _ = h
        .lock
        .compare_exchange(me, 0, Ordering::Release, Ordering::Relaxed);
}

/// The shared-arena allocator; see the module documentation.
pub struct SharedHeap {
    state: AtomicU8,
}

impl SharedHeap {
    /// A new, not-yet-initialised allocator (initialisation happens on first use).
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(UNINIT),
        }
    }

    fn header() -> &'static Header {
        // SAFETY: only called once `state == READY`, i.e. after `ARENA_BASE` was mapped and the
        // header zero-initialised (fresh anonymous memory is zero).
        unsafe { &*(ARENA_BASE as *const Header) }
    }

    #[cold]
    fn init(&self) -> u8 {
        if self
            .state
            .compare_exchange(UNINIT, BUSY, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            loop {
                let s = self.state.load(Ordering::Acquire);
                if s != BUSY {
                    return s;
                }
                core::hint::spin_loop();
            }
        }
        // On by default: it is what lets native-fork children share kernel state. Set
        // `LITEBOX_SHARED_HEAP=0` to fall back to the private system heap.
        // SAFETY: getenv on a NUL-terminated literal; no allocation involved.
        let v = unsafe { libc::getenv(c"LITEBOX_SHARED_HEAP".as_ptr()) };
        // SAFETY: a non-null getenv result is a valid NUL-terminated string.
        let enabled = v.is_null() || unsafe { *v.cast::<u8>() } != b'0';
        let mut result = DISABLED;
        if enabled {
            // SAFETY: fixed-address anonymous shared mapping; NOREPLACE refuses to clobber.
            let p = unsafe {
                libc::mmap(
                    ARENA_BASE as *mut libc::c_void,
                    ARENA_SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED
                        | libc::MAP_ANONYMOUS
                        | libc::MAP_NORESERVE
                        | libc::MAP_FIXED_NOREPLACE,
                    -1,
                    0,
                )
            };
            if p as usize == ARENA_BASE {
                Self::header()
                    .bump
                    .store(ARENA_BASE + HEADER_SIZE, Ordering::Release);
                // SAFETY: plain memfd_create/ftruncate on a NUL-terminated literal. A sparse
                // memfd is this family's SysV-shm backing: every process inherits the fd, and
                // segments are page-aligned ranges of it, found by key in the arena header.
                let fd =
                    unsafe { libc::memfd_create(c"litebox-shm-pool".as_ptr(), libc::MFD_CLOEXEC) };
                let pool_fd =
                    if fd >= 0 && unsafe { libc::ftruncate(fd, POOL_SIZE as libc::off_t) } == 0 {
                        fd as usize
                    } else {
                        usize::MAX
                    };
                Self::header().pool_fd.store(pool_fd, Ordering::Release);
                result = READY;
                ACTIVE.store(true, Ordering::Release);
                // SAFETY: getenv on a NUL-terminated literal.
                POISON.store(
                    unsafe { !libc::getenv(c"LITEBOX_SHARED_HEAP_POISON".as_ptr()).is_null() },
                    Ordering::Release,
                );
            }
        }
        self.state.store(result, Ordering::Release);
        result
    }

    fn ready(&self) -> bool {
        match self.state.load(Ordering::Acquire) {
            READY => true,
            DISABLED => false,
            _ => self.init() == READY,
        }
    }

    fn class_of(layout: Layout) -> Option<(usize, usize)> {
        if layout.align() > MAX_ALIGN {
            return None;
        }
        let size = layout.size().max(layout.align()).max(1 << MIN_CLASS);
        let size = size.checked_next_power_of_two()?;
        let class = (size.trailing_zeros() - MIN_CLASS) as usize;
        (class < CLASSES).then_some((class, size))
    }

    fn in_arena(ptr: *mut u8) -> bool {
        (ARENA_BASE..ARENA_BASE + ARENA_SIZE).contains(&(ptr as usize))
    }
}

impl Default for SharedHeap {
    fn default() -> Self {
        Self::new()
    }
}

/// `LITEBOX_DIAG_BIGALLOC=1`: name (by return-address chain) every arena allocation of 32 MiB or
/// more on stderr; resolve with `addr2line` against `litebox-exe-base`.
pub static BIG_DIAG: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[inline(never)]
fn report_big_alloc(size: usize) {
    let mut buf = [0u8; 512];
    let mut n = 0;
    let mut put = |b: &[u8], buf: &mut [u8; 512], n: &mut usize| {
        for &c in b {
            if *n < buf.len() {
                buf[*n] = c;
                *n += 1;
            }
        }
    };
    let hex = |mut v: usize, out: &mut [u8; 16]| {
        for i in (0..16).rev() {
            out[i] = b"0123456789abcdef"[v & 0xf];
            v >>= 4;
        }
    };
    let mut h = [0u8; 16];
    put(b"[diag-bigalloc] size_mb=", &mut buf, &mut n);
    let mb = size >> 20;
    let mut digits = [0u8; 8];
    let mut nd = 0;
    let mut v = mb;
    loop {
        digits[nd] = b'0' + (v % 10) as u8;
        nd += 1;
        v /= 10;
        if v == 0 || nd == 8 {
            break;
        }
    }
    for i in (0..nd).rev() {
        put(&[digits[i]], &mut buf, &mut n);
    }
    let mut rbp: usize;
    // SAFETY: reads the frame-pointer register only.
    unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp) };
    for _ in 0..12 {
        if rbp < 0x1000 || rbp % 8 != 0 {
            break;
        }
        // SAFETY: best-effort frame-pointer walk on the calling thread's own stack.
        let (next, ret) = unsafe { (*(rbp as *const usize), *((rbp + 8) as *const usize)) };
        put(b" 0x", &mut buf, &mut n);
        hex(ret, &mut h);
        put(&h, &mut buf, &mut n);
        if next <= rbp {
            break;
        }
        rbp = next;
    }
    put(b"\n", &mut buf, &mut n);
    // SAFETY: raw write of a stack buffer to stderr.
    unsafe { libc::write(2, buf.as_ptr().cast(), n) };
}

struct Guard<'a>(&'a Header);

impl<'a> Guard<'a> {
    fn lock(h: &'a Header) -> Self {
        // The lock word holds the owner's kernel thread id, so a stuck lock names its holder.
        // SAFETY: gettid has no arguments and cannot fail.
        let me = unsafe { libc::syscall(libc::SYS_gettid) } as usize;
        let mut spins = 0u64;
        while h
            .lock
            .compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spins += 1;
            if spins % 200_000 == 0 {
                // A holder that died (killed, or crashed mid-allocation) can never release. The
                // heap's critical sections are single pointer pushes/pops, so taking the lock
                // over from a vanished owner leaves the free lists consistent.
                let owner = h.lock.load(Ordering::Relaxed);
                if owner != 0 && owner != me {
                    // SAFETY: tkill with signal 0 only probes for the thread's existence.
                    let gone = unsafe { libc::syscall(libc::SYS_tkill, owner as libc::c_long, 0) }
                        == -1
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
                    if gone
                        && h.lock
                            .compare_exchange(owner, me, Ordering::Acquire, Ordering::Relaxed)
                            .is_ok()
                    {
                        return Self(h);
                    }
                }
            }
            if spins == 200_000_000 {
                let owner = h.lock.load(Ordering::Relaxed);
                let msg =
                    std::format!("shared heap lock stuck: tid {me} waiting, owner tid {owner}\n");
                // SAFETY: raw write of a stack buffer to stderr.
                unsafe { libc::write(2, msg.as_ptr().cast(), msg.len()) };
                // SAFETY: abort never returns.
                unsafe { libc::abort() };
            }
            core::hint::spin_loop();
        }
        Self(h)
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0.lock.store(0, Ordering::Release);
    }
}

// SAFETY: blocks handed out are disjoint, suitably aligned, and stay valid until `dealloc`.
unsafe impl GlobalAlloc for SharedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.ready() || is_private() {
            return unsafe { System.alloc(layout) };
        }
        let Some((class, size)) = Self::class_of(layout) else {
            return unsafe { System.alloc(layout) };
        };
        if size >= (32 << 20) && BIG_DIAG.load(Ordering::Relaxed) {
            report_big_alloc(size);
        }
        let h = Self::header();
        let _g = Guard::lock(h);
        let head = h.free[class].load(Ordering::Relaxed);
        if head != 0 {
            if POISON.load(Ordering::Relaxed) && size >= 32 {
                // SAFETY: the block is free and `size` bytes; debug-only read.
                let s = unsafe { core::slice::from_raw_parts((head + 16) as *const u8, size - 16) };
                if s.iter().any(|&b| b != 0xDD) {
                    drop(_g);
                    panic!("shared heap: write to freed block {head:#x} (size class {size})");
                }
            }
            if POISON.load(Ordering::Relaxed) && size >= 32 {
                // Clear the double-free marker so a block handed out and freed again untouched
                // is not mistaken for one freed twice.
                // SAFETY: the block is `size >= 32` bytes and owned by this call now.
                unsafe { core::ptr::write_bytes((head + 16) as *mut u8, 0, 8) };
            }
            // SAFETY: a free block's first word stores the next free block.
            let next = unsafe { *(head as *const usize) };
            h.free[class].store(next, Ordering::Relaxed);
            return head as *mut u8;
        }
        let align = size.min(MAX_ALIGN);
        let start = (h.bump.load(Ordering::Relaxed) + align - 1) & !(align - 1);
        let end = start + size;
        if end > ARENA_BASE + ARENA_SIZE {
            return core::ptr::null_mut();
        }
        h.bump.store(end, Ordering::Relaxed);
        start as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if !Self::in_arena(ptr) {
            return unsafe { System.dealloc(ptr, layout) };
        }
        let (class, size) = Self::class_of(layout).expect("arena block has a size class");
        if size >= RELEASE_MIN {
            // A freed big block would otherwise stay resident in the shared arena forever: the
            // free list keeps it for reuse but nothing returns its pages. Hand every page except
            // the first (which holds the free-list link) back to the host now, while the block is
            // still exclusively ours; a later reuse simply reads zeros.
            // SAFETY: the range lies inside this block, which the caller has just given up.
            unsafe {
                libc::madvise(ptr.add(4096).cast(), size - 4096, libc::MADV_REMOVE);
            }
        }
        let h = Self::header();
        let _g = Guard::lock(h);
        if POISON.load(Ordering::Relaxed) {
            let size = Self::class_of(layout).unwrap().1;
            // A block already carrying the poison pattern past its free-list link was freed
            // before: this is a double free, caught at the second `dealloc`.
            // SAFETY: the block is `size` bytes; reading it here is a debug-only check.
            let already = size >= 32
                && unsafe { core::slice::from_raw_parts(ptr.add(16), 8) }
                    .iter()
                    .all(|&b| b == 0xDD);
            if already {
                drop(_g);
                panic!("shared heap: double free of {ptr:p} (size class {size})");
            }
            // SAFETY: the block is `size` bytes and no longer in use.
            unsafe { core::ptr::write_bytes(ptr, 0xDD, size) };
        }
        // SAFETY: the block is at least 16 bytes and no longer in use.
        unsafe { *(ptr as *mut usize) = h.free[class].load(Ordering::Relaxed) };
        h.free[class].store(ptr as usize, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        // A private block stays private however it grows: it was made private on purpose.
        if !Self::in_arena(ptr) {
            return unsafe { System.realloc(ptr, layout, new_size) };
        }
        if Self::in_arena(ptr)
            && let (Some((c0, _)), Some((c1, _))) =
                (Self::class_of(layout), Self::class_of(new_layout))
            && c0 == c1
        {
            return ptr;
        }
        let new = unsafe { self.alloc(new_layout) };
        if !new.is_null() {
            unsafe {
                core::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
        }
        new
    }
}

impl SharedHeap {
    /// Whether a block of `layout` would be served from the arena (as opposed to the system).
    fn ready_for(&self, layout: Layout) -> bool {
        self.ready() && Self::class_of(layout).is_some()
    }
}

/// Whether the shared-arena heap is in use in this process (`LITEBOX_SHARED_HEAP` was set and the
/// arena mapped). It is decided on the first allocation, long before any caller can ask.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

static POISON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
