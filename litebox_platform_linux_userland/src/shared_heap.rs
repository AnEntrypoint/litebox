//! A `#[global_allocator]` whose memory lives in one `MAP_SHARED` arena, so every process in a
//! native-`fork()` family sees the same heap at the same addresses.
//!
//! LiteBox keeps its kernel state (pipes, the in-memory filesystem layer, sockets, futex tables)
//! in ordinary heap objects. With a private heap, `fork()` gives each child its own copy-on-write
//! duplicate and the guest processes stop seeing each other's writes. Placing the whole Rust heap
//! in shared memory makes that state genuinely shared; per-process state (the `Task` itself, the
//! guest's own mappings, thread stacks) never touches the Rust heap's arena and stays private.
//!
//! Opt-in (`LITEBOX_SHARED_HEAP=1`): with it unset every call forwards to the system allocator.
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
}

const HEADER_SIZE: usize = 4096;

const UNINIT: u8 = 0;
const BUSY: u8 = 1;
const READY: u8 = 2;
const DISABLED: u8 = 3;

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
        // SAFETY: getenv on a NUL-terminated literal; no allocation involved.
        let enabled = unsafe { !libc::getenv(c"LITEBOX_SHARED_HEAP".as_ptr()).is_null() };
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
                result = READY;
                ACTIVE.store(true, Ordering::Release);
                // SAFETY: getenv on a NUL-terminated literal.
                POISON.store(unsafe { !libc::getenv(c"LITEBOX_SHARED_HEAP_POISON".as_ptr()).is_null() }, Ordering::Release);
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
            if spins == 200_000_000 {
                let owner = h.lock.load(Ordering::Relaxed);
                let msg = std::format!(
                    "shared heap lock stuck: tid {me} waiting, owner tid {owner}\n"
                );
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
        if !self.ready() {
            return unsafe { System.alloc(layout) };
        }
        let Some((class, size)) = Self::class_of(layout) else {
            return unsafe { System.alloc(layout) };
        };
        let h = Self::header();
        let _g = Guard::lock(h);
        let head = h.free[class].load(Ordering::Relaxed);
        if head != 0 {
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
        let (class, _) = Self::class_of(layout).expect("arena block has a size class");
        let h = Self::header();
        let _g = Guard::lock(h);
        if POISON.load(Ordering::Relaxed) {
            // SAFETY: the block is `size` bytes and no longer in use.
            unsafe { core::ptr::write_bytes(ptr, 0xDD, Self::class_of(layout).unwrap().1) };
        }
        // SAFETY: the block is at least 16 bytes and no longer in use.
        unsafe { *(ptr as *mut usize) = h.free[class].load(Ordering::Relaxed) };
        h.free[class].store(ptr as usize, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        if !Self::in_arena(ptr) && !self.ready_for(new_layout) {
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
