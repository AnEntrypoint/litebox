// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! DRM/KMS "dumb buffer" software display device (`/dev/dri/card0`).
//!
//! Implements the minimal ioctl surface a software-only KMS client (a program using the
//! `DRM_IOCTL_MODE_CREATE_DUMB` path directly, or a toolkit's KMS-without-a-display-server
//! backend, e.g. SDL2's `KMSDRM` driver or Qt's `eglfs`/`linuxfb` in dumb-buffer mode) needs to
//! enumerate a display, allocate a CPU-writable pixel buffer, attach it as a scanout
//! framebuffer, and page-flip it. Modeled on the real Linux kernel's own `drm/vkms` (Virtual
//! KMS) driver -- a software-only DRM device with no real GPU or display hardware, merged
//! upstream since kernel 4.19 -- which proves this exact shape (a well-defined ioctl surface
//! satisfied entirely by software) is a legitimate, minimal target, not a novel one.
//!
//! Exposes exactly one fake connector/CRTC/encoder/plane, matching a single virtual display at
//! a fixed resolution. Struct layouts and ioctl request numbers live in
//! `litebox_common_linux`'s `Drm*`/`DRM_IOCTL_MODE_*` items; see
//! `docs/drm-dumb-buffer-ioctl-reference.md` for the derivation (fetched verbatim from the real
//! kernel `drm.h`/`drm_mode.h`, ioctl numbers independently recomputed and verified against a
//! standalone `_IOWR` encoder, not guessed).
//!
//! # Implemented DRM/KMS Dumb-Buffer & Host Presentation Architecture
//!
//! - **Dumb Buffer Creation & Mapping**: `DRM_IOCTL_MODE_CREATE_DUMB` creates a platform-backed
//!   shared memory buffer. `DRM_IOCTL_MODE_MAP_DUMB` assigns a unique offset which is mapped via
//!   `mmap()` directly into the guest address space.
//! - **Framebuffer & Page Flipping**: `DRM_IOCTL_MODE_ADDFB2` attaches the dumb buffer to a framebuffer handle.
//!   `DRM_IOCTL_MODE_PAGE_FLIP` sets the CRTC scanout framebuffer and triggers every registered flip callback.
//! - **wgpu Host Presentation Pipeline**: When a flip callback is registered via `add_flip_callback` (such as by
//!   `litebox_platform_windows_userland::presentation::Presenter`), every `DRM_IOCTL_MODE_PAGE_FLIP`
//!   transfers the active framebuffer's pixels to the host `wgpu` surface for real window presentation.
//! - **Page-flip Events**: `DRM_MODE_PAGE_FLIP_EVENT` pushes `DrmEventVblank` into the pending queue,
//!   which is read via `pop_flip_event_bytes()` on the DRM device file descriptor.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use litebox::event::Events;
use litebox::event::polling::Pollee;
use litebox::mm::linux::PAGE_SIZE;
use litebox::platform::Instant as _;
use litebox::platform::RawConstPointer;
use litebox_common_linux::{
    DRM_AUTH_MAGIC_VALUE, DRM_CAP_CRTC_IN_VBLANK_EVENT, DRM_CAP_CURSOR_HEIGHT,
    DRM_CAP_CURSOR_WIDTH, DRM_CAP_DUMB_BUFFER, DRM_CAP_PRIME, DRM_CAP_TIMESTAMP_MONOTONIC,
    DRM_CLIENT_CAP_UNIVERSAL_PLANES, DRM_EVENT_FLIP_COMPLETE, DRM_MODE_CONNECTOR_VIRTUAL,
    DRM_MODE_ENCODER_VIRTUAL, DRM_MODE_OBJECT_CONNECTOR, DRM_MODE_OBJECT_PLANE,
    DRM_MODE_PAGE_FLIP_EVENT, DRM_MODE_PROP_BLOB, DRM_MODE_PROP_ENUM, DRM_PRIME_CAP_EXPORT,
    DRM_PRIME_CAP_IMPORT, DrmAuth, DrmEvent, DrmEventVblank, DrmGetCap, DrmModeCardRes,
    DrmModeConnectorSetProperty, DrmModeCreateDumb, DrmModeCrtc, DrmModeCrtcPageFlip,
    DrmModeDestroyDumb, DrmModeFbCmd, DrmModeFbCmd2, DrmModeFbDirtyCmd, DrmModeGetBlob,
    DrmModeGetConnector, DrmModeGetEncoder, DrmModeGetPlane, DrmModeGetPlaneRes,
    DrmModeGetProperty, DrmModeMapDumb, DrmModeModeinfo, DrmModeObjGetProperties,
    DrmModeObjSetProperty, DrmModePropertyEnum, DrmModeSetPlane, DrmSetClientCap, DrmVersion,
    VIRTUAL_CONNECTOR_DPMS_PROP_ID, VIRTUAL_CONNECTOR_DPMS_VALUE, VIRTUAL_CONNECTOR_EDID_BLOB_ID,
    VIRTUAL_CONNECTOR_EDID_PROP_ID, VIRTUAL_PLANE_TYPE_PROP_ID, VIRTUAL_PLANE_TYPE_VALUE,
    errno::Errno,
};
use zerocopy::IntoBytes;

use crate::{ShimPlatform, UserPtr, UserPtrMut};

/// Whether per-ioctl DRM tracing is enabled (set from `LITEBOX_DRM_TRACE=1` by
/// the runner, since this `no_std` crate cannot read the environment itself).
///
/// Off by default and deliberately so: every DRM ioctl passes through
/// `drm_ioctl`, and page flips arrive at vblank rate, so an ungated log floods a
/// real session and has previously throttled the emulator enough to change the
/// behaviour being measured. Turn it on to answer "is the guest still flipping?",
/// the question that separates a compositor that stopped presenting from a client
/// presenting an empty buffer.
pub(crate) fn drm_trace_enabled() -> bool {
    DRM_TRACE.load(core::sync::atomic::Ordering::Relaxed)
}

/// Set once by the runner to turn DRM ioctl tracing on.
pub fn set_drm_trace(enabled: bool) {
    DRM_TRACE.store(enabled, core::sync::atomic::Ordering::Relaxed);
}

static DRM_TRACE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether `DRM_IOCTL_MODE_DIRTYFB` actually presents. See `SharedDrmTable::dirty_fb`.
pub(crate) fn dirty_fb_enabled() -> bool {
    DIRTY_FB_ENABLED.load(core::sync::atomic::Ordering::Relaxed)
}

/// Set by the runner from `LITEBOX_NO_DIRTYFB`, so one binary can be A/B'd with and without
/// DIRTYFB presentation. This crate is `#![no_std]` and cannot read the environment itself --
/// a `std::env` read here compiles to nothing, or fails to compile, rather than silently
/// defaulting; the flag must be pushed in from the runner, which does have an environment.
pub fn set_dirty_fb_enabled(enabled: bool) {
    DIRTY_FB_ENABLED.store(enabled, core::sync::atomic::Ordering::Relaxed);
}

/// Defaults to `true`: DIRTYFB presenting is the intended behaviour, and the env var only
/// turns it OFF. A default of `false` would make an unset runner silently disable it.
static DIRTY_FB_ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// The virtual display's fixed mode. 1920x1080@60 is a reasonable, widely-compatible default
/// for a single software display with no real monitor to query.
const VIRTUAL_WIDTH: u32 = 1920;
const VIRTUAL_HEIGHT: u32 = 1080;
const VIRTUAL_REFRESH_HZ: u32 = 60;

/// Fixed object IDs for the one virtual connector/CRTC/encoder this device exposes (no plane
/// object is exposed yet -- `DRM_IOCTL_MODE_GETPLANE(RESOURCES)`/`SETPLANE` are not implemented
/// in this pass, only the legacy `SETCRTC`/`PAGE_FLIP` scanout path, which every dumb-buffer
/// client already supports as a fallback). Real DRM object IDs are allocated dynamically and are
/// driver-internal opaque values from userspace's perspective -- any stable, non-zero,
/// mutually-distinct set is valid; these are arbitrary but memorable.
const VIRTUAL_CONNECTOR_ID: u32 = 1;
const VIRTUAL_ENCODER_ID: u32 = 2;
const VIRTUAL_CRTC_ID: u32 = 3;
/// The one virtual primary plane this device exposes, tied to [`VIRTUAL_CRTC_ID`] -- matching
/// how the kernel's own `drm/vkms` software driver exposes exactly one primary plane per CRTC.
/// Added alongside [`SharedDrmTable::get_plane_resources`]/[`SharedDrmTable::get_plane`]/
/// [`SharedDrmTable::set_plane`]; some real KMS-using toolkits query the plane API even for a
/// single-plane use case (checking plane capabilities before deciding how to render).
const VIRTUAL_PLANE_ID: u32 = 4;

/// `DRM_FORMAT_XRGB8888` -- the fourcc-code encoding (`'X' | 'R'<<8 | '2'<<16 | '4'<<24`, per
/// `drm_fourcc.h`'s `fourcc_code` macro) for the one pixel format this device's dumb buffers
/// support (see `create_dumb`'s `bpp == 32` case elsewhere in this file). Not independently
/// fetched from `drm_fourcc.h` this pass (see `docs/drm-dumb-buffer-ioctl-reference.md`'s "gaps"
/// section); this is the standard, well-known fourcc encoding for that format.
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

/// A minimal, spec-valid EDID 1.3 block for the virtual connector -- see
/// [`SharedDrmTable::get_prop_blob`]'s own doc comment for why these exact bytes were chosen and
/// how the checksum (byte 127) was derived. No display-timing descriptor blocks are populated;
/// this device's one fixed mode is reported directly via `GETCONNECTOR`, not re-derived from
/// EDID by any client this device has been tested against.
#[rustfmt::skip]
const VIRTUAL_EDID_BLOB: [u8; 128] = [
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x1E, 0x6D, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00,
    0x01, 0x1E, 0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x56,
];

fn virtual_mode() -> DrmModeModeinfo {
    let mut name = [0u8; 32];
    let label = b"virtual-1920x1080\0";
    name[..label.len()].copy_from_slice(label);
    DrmModeModeinfo {
        // A real `clock` value would be `hdisplay * (something) * vrefresh / 1000`-shaped; for a
        // software-only display nothing ever consults this for real timing, so a plausible
        // round number keeps clients that sanity-check "clock != 0" happy without pretending to
        // a real precision this device doesn't have.
        clock: VIRTUAL_WIDTH * VIRTUAL_HEIGHT * VIRTUAL_REFRESH_HZ / 1000,
        hdisplay: VIRTUAL_WIDTH as u16,
        hsync_start: VIRTUAL_WIDTH as u16,
        hsync_end: VIRTUAL_WIDTH as u16,
        htotal: VIRTUAL_WIDTH as u16,
        hskew: 0,
        vdisplay: VIRTUAL_HEIGHT as u16,
        vsync_start: VIRTUAL_HEIGHT as u16,
        vsync_end: VIRTUAL_HEIGHT as u16,
        vtotal: VIRTUAL_HEIGHT as u16,
        vscan: 0,
        vrefresh: VIRTUAL_REFRESH_HZ,
        flags: 0,
        r#type: 0,
        name,
    }
}

/// Occupancy words for a [`SharedDumbBufferSlot`]/[`SharedFbSlot`]. Plain `u32`s, not a `bool`, so
/// a slot's state is a single atomic store every process in the fork family reads identically.
const DRM_SLOT_FREE: u32 = 0;
const DRM_SLOT_USED: u32 = 1;

/// How many live dumb buffers the shared table can name. Sized against what a real KMS client
/// actually allocates (Xorg's `modesetting` driver and weston's DRM backend each hold a shadow
/// buffer plus at most one or two scanout buffers), NOT against what a guest could ask for: the
/// region is one power-of-two allocation out of a 128MiB arena that ~6 concurrent `GlobalState`s
/// already crowd (AGENTS.md Open 3), so every slot here is paid for by every fork family.
const SHARED_DRM_BUFFERS: usize = 32;
/// How many live framebuffers the shared table can name. Higher than the buffer count on purpose:
/// this device has no `DRM_IOCTL_MODE_RMFB`, so a framebuffer is never explicitly removed and its
/// slot is only ever reclaimed by [`SharedDrmTable::reclaim_dead`] once its creator's host process
/// is gone -- matching real Linux, where destroying a buffer leaves any framebuffer referencing it
/// dangling until userspace removes it.
const SHARED_DRM_FRAMEBUFFERS: usize = 48;
/// How many completed-but-unread `DRM_MODE_PAGE_FLIP_EVENT` events the shared ring holds. A client
/// that never reads its flip events loses the oldest first (a real kernel's `drm_event` ring does
/// the same) rather than being failed at `PAGE_FLIP` time.
const SHARED_DRM_FLIP_EVENTS: usize = 32;
/// Width of one entry of [`SharedDrmRegion::flip_events`]: the exact byte image of one
/// `DrmEventVblank`, so what the guest `read()`s is bit-identical to the `VecDeque<DrmEventVblank>`
/// this replaced.
const FLIP_EVENT_BYTES: usize = size_of::<DrmEventVblank>();

/// A single allocated dumb buffer's state.
///
/// Backed by a real platform shared-memory object (the same primitive `MAP_ANONYMOUS|MAP_SHARED`
/// mmaps use, see [`litebox::platform::page_mgmt::PageManagementProvider::create_shared_memory`]),
/// created eagerly at `CREATE_DUMB` time -- not a plain `Vec<u8>`. This is what makes the buffer's
/// pixel content reachable from THREE independent places that must all observe the same bytes:
/// the guest's own later `mmap()` of the `MAP_DUMB` offset (see `sys_mmap`'s DRI-fd branch), and
/// (in a later pass) the host-side wgpu presentation code reading the flipped framebuffer's
/// content directly, with no explicit copy between guest writes and host reads.
///
/// POINTER-FREE, because this slot lives in [`SharedDrmRegion`], which is allocated from the
/// cross-process shared kernel arena: `GlobalState`'s bytes are mapped at one fixed address in
/// every host process of a fork family, but only its INLINE bytes travel -- a `BTreeMap` root
/// pointer here is a heap address of whichever process created it and is meaningless in a child
/// (the `btree/node.rs:1232` panic `f5d73ff` fixed for `flock_registry`). Hence a fixed slot index
/// instead of a map, the same shape `SharedUnixConnTable`/`SharedFlockTable` already use.
struct SharedDumbBufferSlot<Platform: ShimPlatform> {
    state: AtomicU32,
    /// The GEM handle `CREATE_DUMB` handed out for this buffer -- stored because a fixed slot
    /// index is not the handle, and every ioctl addresses buffers by handle.
    gem_handle: AtomicU32,
    // Stored for the buffer's own record-keeping (e.g. a future `DESTROY_DUMB`/`MAP_DUMB` bounds
    // or format-consistency check) even though nothing reads them back yet -- `pitch`/`size` are
    // the derived values everything downstream actually uses.
    #[allow(dead_code)]
    width: AtomicU32,
    #[allow(dead_code)]
    height: AtomicU32,
    #[allow(dead_code)]
    bpp: AtomicU32,
    pitch: AtomicU32,
    size: AtomicUsize,
    /// `None` doubles as "this slot is free" -- there is no generic way to fabricate a
    /// `Platform::SharedMemoryHandle` for an empty slot.
    ///
    /// `UnsafeCell` because `Platform::SharedMemoryHandle` is an associated type with no atomic
    /// form (it is `usize` on the real platform, but this crate is platform-generic). See this
    /// type's `Sync` impl for why that is sound here.
    handle: UnsafeCell<Option<Platform::SharedMemoryHandle>>,
    /// The fake `mmap` offset handed out by `DRM_IOCTL_MODE_MAP_DUMB`, `0` until this buffer has
    /// been mapped at least once (real fake offsets are page-aligned and start at 1, so 0 is
    /// unrepresentable as a real one). Real DRM hands out a fresh, unique fake offset per `MAP_DUMB`
    /// call on the same handle; this device reuses the first one issued, which every real client
    /// tolerates (they don't rely on the offset changing across repeated `MAP_DUMB` calls).
    map_offset: AtomicU64,
    /// Host pid that created this buffer, so [`SharedDrmTable::reclaim_dead`] can recover slots
    /// whose creator died without issuing `DESTROY_DUMB`.
    owner_host: AtomicU32,
}

// SAFETY: `handle` is the one field that is not already atomic, and every read and write of it
// happens inside `SharedDrmRegion::guard`'s critical section -- the SAME cross-process mutex that
// serializes every other field of the region -- so no two threads, in one host process or across a
// whole fork family, ever touch it unsynchronized. `UnsafeCell` is otherwise `!Sync` unconditionally,
// which would make `GlobalState` `!Sync` too; this is the standard shared-memory state behind an
// arena-resident mutex, and `SharedFlockTable` avoids needing it only because every field it
// mutates happens to have an atomic form.
unsafe impl<Platform: ShimPlatform> Sync for SharedDumbBufferSlot<Platform> {}

impl<Platform: ShimPlatform> SharedDumbBufferSlot<Platform> {
    fn new() -> Self {
        Self {
            state: AtomicU32::new(DRM_SLOT_FREE),
            gem_handle: AtomicU32::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            bpp: AtomicU32::new(0),
            pitch: AtomicU32::new(0),
            size: AtomicUsize::new(0),
            handle: UnsafeCell::new(None),
            map_offset: AtomicU64::new(0),
            owner_host: AtomicU32::new(0),
        }
    }

    /// Reads this slot's shared-memory handle. Callers hold `SharedDrmRegion::guard`.
    fn get_handle(&self) -> Option<Platform::SharedMemoryHandle> {
        // SAFETY: the caller holds the region guard, so no other thread is writing it.
        unsafe { *self.handle.get() }
    }

    /// Writes this slot's shared-memory handle. Callers hold `SharedDrmRegion::guard`.
    fn set_handle(&self, handle: Option<Platform::SharedMemoryHandle>) {
        // SAFETY: as above.
        unsafe { *self.handle.get() = handle };
    }
}

/// Returned by [`SharedDrmTable::scanout_snapshot`] -- the current front buffer's identity, for a
/// host-side control channel (`docs/presenter-process-design.md` section 2) to duplicate the
/// handle into an external presenter process. `pub`, not `pub(crate)`, so it can be re-exported
/// at this crate's root and named by a runner crate outside `litebox_shim_linux`.
pub struct ScanoutSnapshot<Platform: ShimPlatform> {
    pub handle: Platform::SharedMemoryHandle,
    pub size: usize,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub pixel_format: u32,
    pub offset: u64,
    pub seq: u64,
}

/// One slot of [`SharedDrmRegion::framebuffers`]: an attached (buffer handle, format, geometry)
/// tuple, referenced by `fb_id` from `DRM_IOCTL_MODE_SETCRTC`/`PAGE_FLIP`/`DIRTYFB`.
///
/// See [`SharedDumbBufferSlot`] for why every field is inline POD.
struct SharedFbSlot {
    state: AtomicU32,
    /// The `fb_id` this slot answers to -- framebuffers are named by id, not by slot index.
    fb_id: AtomicU32,
    #[allow(dead_code)]
    width: AtomicU32,
    height: AtomicU32,
    pixel_format: AtomicU32,
    /// The dumb-buffer handle backing plane 0 (the only plane a dumb buffer ever populates).
    handle: AtomicU32,
    owner_host: AtomicU32,
}

impl SharedFbSlot {
    fn new() -> Self {
        Self {
            state: AtomicU32::new(DRM_SLOT_FREE),
            fb_id: AtomicU32::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            pixel_format: AtomicU32::new(0),
            handle: AtomicU32::new(0),
            owner_host: AtomicU32::new(0),
        }
    }
}

/// The arena-resident, pointer-free half of the one virtual DRM/KMS device's shared state. See the
/// module doc comment for what is and is not implemented in this pass.
///
/// # Why this is fixed slots and not a `BTreeMap`
///
/// `GlobalState` (which holds [`SharedDrmTable`]) is byte-shared with every cross-process fork
/// child at one fixed address, and `SharedArc<T>` shares only `T`'s INLINE BYTES -- so a
/// `BTreeMap` here hands a child the creating process's heap root pointer, meaningless in the
/// child's own address space. That is exactly the `btree/node.rs:1232` panic `f5d73ff` fixed by
/// moving `flock_registry` per-process, and it is why this device's state could not stay as it
/// was.
///
/// # Why it is NOT per-process (unlike `flock_registry`)
///
/// `/dev/dri/card0` is a real fs device node (`litebox::fs::devices::DRI_CARD0_NODE_INFO`) that a
/// fork child both inherits as a carried `"file"` fd (`raw_fd_subsystem_name` classifies a DRI fd
/// as `"file"`, so `try_cross_process_fork` carries it) and can reopen by path, so a child
/// genuinely reaches this device's buffers and framebuffers -- real DRM device state is per-DEVICE,
/// not per-process. `docs/AGENTS_ARCHIVE_2026-09-18.md:765` records the same conclusion;
/// re-confirmed for this pass before changing anything.
///
/// # What stays per-process
///
/// [`DrmFlipLocal`]: the host-side flip callbacks (`Box<dyn Fn>`) and the flip `Pollee` (a
/// `BTreeMap` of `Weak<dyn Observer>`). Both are process-private by nature -- a closure or an
/// observer registered in one host process cannot run in another -- so they moved to
/// `GlobalStateHandle` rather than being forced in here.
///
/// Every mutable field below is guarded by [`Self::guard`]; the atomics are bumped under it too,
/// so one critical section is the whole concurrency argument.
struct SharedDrmRegion<Platform: ShimPlatform> {
    /// Whether these bytes really are in memory EVERY process of the fork family maps -- the
    /// cross-process shared kernel arena. `false` means the arena was exhausted at construction
    /// and this region is a process-private fallback, so buffers and framebuffers created here
    /// are this host process's own: the device keeps working, it simply is not the one device
    /// real DRM's per-DEVICE semantics describe. Carried in the region (not on
    /// [`SharedDrmTable`]) for exactly the reason `SharedFlockRegion::excludes` is.
    cross_process: AtomicBool,
    /// Serializes every lookup, claim and transition in the whole fork family, so no two processes
    /// can disagree about which slot owns which object. Held for a handful of stores; a holder
    /// killed mid-section is recovered by `RawMutex`'s `holder_pid` dead-holder path.
    guard: litebox::sync::Mutex<Platform, ()>,
    // Handles/IDs start at 1: real DRM never hands out handle/id 0 for an actual object
    // (0 is reserved to mean "none"/"invalid" in these ioctls, e.g. `drm_mode_crtc.fb_id
    // == 0` means "no framebuffer attached").
    next_buffer_handle: AtomicU32,
    next_fb_id: AtomicU32,
    next_map_offset: AtomicU32,
    buffers: [SharedDumbBufferSlot<Platform>; SHARED_DRM_BUFFERS],
    framebuffers: [SharedFbSlot; SHARED_DRM_FRAMEBUFFERS],
    /// The framebuffer currently attached to the virtual CRTC (via `SETCRTC` or `PAGE_FLIP`),
    /// `None` until the guest sets one.
    /// `0` means "no framebuffer attached" (real DRM reserves `fb_id == 0` the same way).
    crtc_fb: AtomicU32,
    /// Whether the virtual CRTC currently has a MODE configured, independent of whether a
    /// framebuffer is currently attached to it. Real DRM's `drmModeGetCrtc` reports the CRTC's
    /// current mode (`mode_valid = 1`) as long as a mode has been set, even mid-modeset when no
    /// framebuffer is scanned out yet (e.g. wlroots' own atomic-modeset-emulation-on-legacy-KMS
    /// path deliberately attaches an "empty" buffer -- `fb_id == 0` -- while transitioning to a
    /// new mode, see `types/output/render.c`'s "Attaching empty buffer to output for modeset").
    /// Previously this device conflated the two (`mode_valid` was derived from `fb_id != 0`), so
    /// that exact empty-buffer modeset transition made a subsequent `GETCRTC` incorrectly report
    /// `mode_valid = 0` -- a real KMS driver never does this, and a client that re-derives its
    /// output dimensions from `GETCRTC`'s reported mode during a modeset would see a bogus "no
    /// mode" answer instead of the mode it just set. Set by [`SharedDrmTable::set_crtc`] whenever
    /// the guest
    /// supplies a mode (`req.mode_valid != 0`), left in whatever state it already had if the
    /// guest calls `SETCRTC` with `mode_valid == 0` purely to change/detach the framebuffer (real
    /// DRM only clears the configured mode via an explicit "set to no mode" `SETCRTC`, i.e.
    /// `mode_valid == 0` in that same call), and cleared only when `mode_valid == 0`.
    crtc_mode_set: AtomicBool,
    /// The framebuffer currently attached to [`VIRTUAL_PLANE_ID`] via `SETPLANE`, `None` until
    /// the guest sets one. Deliberately independent of `crtc_fb` (a real primary plane's `fb_id`
    /// and its CRTC's own `fb_id` are two separate pieces of driver state that a real client can
    /// observe diverge, e.g. right after a `SETPLANE` before any `PAGE_FLIP`/`SETCRTC` call) --
    /// this device does not attempt to keep them synchronized, matching real KMS semantics rather
    /// than inventing a coupling the UAPI doesn't promise.
    /// `0` means the plane is disabled, the real-DRM `SETPLANE` way of detaching.
    plane_fb: AtomicU32,
    /// Completed-but-not-yet-`read()` page-flip events as a fixed ring of RAW EVENT BYTES, in
    /// completion order -- popped one at a time by `read()` on the DRM device fd (see
    /// `litebox_shim_linux::syscalls::file::do_read`'s DRI-fd branch). One slot holds exactly the
    /// byte image a real `read()` must return, so what the guest sees is bit-identical to the
    /// `VecDeque<DrmEventVblank>` this replaced. Only ever appended when the guest requested
    /// `DRM_MODE_PAGE_FLIP_EVENT`; this device completes every flip immediately (no real vsync
    /// timing -- see this module's doc comment), so an event is always ready by the time a client
    /// gets around to reading for it. At capacity the OLDEST is dropped, matching a real kernel's
    /// own event ring (and never failing the guest's `PAGE_FLIP` for a queue it has not drained).
    flip_events: [[AtomicU8; FLIP_EVENT_BYTES]; SHARED_DRM_FLIP_EVENTS],
    /// Index of the oldest live entry in [`Self::flip_events`].
    flip_head: AtomicU32,
    /// How many entries of [`Self::flip_events`] are live.
    flip_count: AtomicU32,
    /// Monotonically increasing vblank sequence number, echoed into each flip-completion event's
    /// `sequence` field -- real clients that track it purely to detect drops/reordering see a
    /// plain incrementing counter, matching real Linux's own semantics closely enough for that
    /// use even though this device has no real vblank interrupt driving it.
    next_vblank_sequence: AtomicU32,
    /// Whether this device fd currently holds "DRM master" status (`DRM_IOCTL_SET_MASTER`
    /// granted, not yet released by `DRM_IOCTL_DROP_MASTER`). This virtual device has exactly one
    /// possible client and no real multi-master contention to arbitrate (see
    /// [`SharedDrmTable::set_master`]/[`SharedDrmTable::drop_master`]'s own doc comments), so this
    /// is bookkeeping only -- nothing currently
    /// consults it to gate another ioctl, matching how mode-setting here already succeeds
    /// regardless of master status.
    is_master: AtomicBool,
    /// Bumped on every successful present (`SETCRTC`/`PAGE_FLIP`/`DIRTYFB`), unconditionally --
    /// registered no matter whether any [`DrmFlipLocal::callbacks`] observer exists, per
    /// `docs/presenter-process-design.md` section 2.2's `ScanoutDescriptor::frame_seq` and section
    /// 4.3/4.4 ("something must always maintain it for `screenshot`/`scanout` to have current data
    /// regardless of whether a presenter is attached"). A plain incrementing counter, not a real
    /// vblank timestamp -- a presenter or control-channel caller
    /// ([`SharedDrmTable::scanout_snapshot`]) polls this to know a new frame exists, exactly like
    /// [`Self::next_vblank_sequence`] but host-readable outside this crate (mirrored into a shared
    /// section by the runner, see the design doc's section 2.4).
    frame_seq: AtomicU64,
}

/// The shared half of the one virtual DRM/KMS device's state, as a `GlobalState` field: a pointer
/// to the arena-resident [`SharedDrmRegion`] (a FIXED address, so valid in every process of the
/// fork family) plus the one flag that says whether that region really is cross-process visible.
/// Nothing here is a heap pointer, which is the entire point -- see that region's doc comment.
pub(crate) struct SharedDrmTable<Platform: ShimPlatform> {
    region: &'static SharedDrmRegion<Platform>,
}

/// The per-process half of the one virtual DRM/KMS device's state -- see [`SharedDrmRegion`]'s
/// "what stays per-process". Lives on `GlobalStateHandle`, NOT on `GlobalState`: both fields are
/// process-private by construction, so a byte-shared copy of them is meaningless in a fork child
/// (the same defect class `f5d73ff` moved `flock_registry` out of).
pub(crate) struct DrmFlipLocal<Platform: ShimPlatform> {
    /// Host-side frame observers, invoked synchronously at the end of every successful present
    /// with the now-scanned-out framebuffer's own pixel bytes (a plain, already-copied-out `&[u8]`
    /// -- not the platform-specific shared-memory handle, deliberately: a `Box<dyn Fn(...)>`
    /// capturing `Platform::SharedMemoryHandle` hits a real rustc limitation resolving that
    /// associated type's well-formedness behind a trait object -- confirmed live, `cargo build`
    /// error `E0277` naming the bound as unsatisfied even on this `impl` block's own untouched
    /// existing methods. Staying byte-based sidesteps that entirely and is also the more honest
    /// interface: a presentation layer only ever needed the pixels, never the handle) plus
    /// `(width, height, pitch, pixel_format)`. `litebox_shim_linux` is platform-agnostic and
    /// cannot depend on a concrete presentation layer (e.g.
    /// `litebox_platform_windows_userland`'s wgpu-backed `Presenter`), so this stays an empty list
    /// until whichever runner binary DOES depend on both crates installs one (see
    /// [`Self::add_flip_callback`]) -- a runner with no GUI story simply never installs any, at
    /// zero cost beyond one length check per flip.
    ///
    /// A LIST, not a single slot: frame observers are genuinely independent subsystems and more
    /// than one is routinely wanted at once (a `--gui` window presenting to the user AND a
    /// `LITEBOX_DUMP_FRAMES` capture recording the exact same frames). When this was a single
    /// `Option`, installing either one silently REPLACED the other, so the two modes were mutually
    /// exclusive by construction -- you could watch the display or capture it, never both, which
    /// is precisely when capture matters most. Appending keeps each observer independent: one that
    /// hangs cannot deprive the others of frames.
    callbacks: litebox::sync::Mutex<Platform, alloc::vec::Vec<DrmFlipCallback>>,
    /// Wakeup mechanism for a guest thread blocked in `poll`/`epoll_wait`/`select` on the DRM
    /// device fd waiting for a page-flip completion event. `page_flip` notifies this whenever it
    /// appends to the shared ring; `syscalls::epoll::EpollDescriptor::poll`'s `File` arm registers
    /// an observer here (mirroring every other pollable fd kind, e.g. eventfd) so a compositor's
    /// own event loop -- which registers the DRM fd once via `epoll_ctl` and then blocks in
    /// `epoll_wait` across many frames, rather than re-polling synchronously after every flip --
    /// actually wakes for the second and subsequent flips (confirmed-live symptom when it did not:
    /// exactly one `SETCRTC`+`PAGE_FLIP` pair at startup, then no repaint ever again for the rest
    /// of the run).
    ///
    /// PER-PROCESS, and that is a real, disclosed narrowing: a `Pollee` is a `BTreeMap` of
    /// `Weak<dyn Observer>`, there is no pointer-free arena form of it, and inventing one is a
    /// separate piece of work. Presents are pushed by the host process that owns the presenter and
    /// consumed by that same process's guest tasks, so the narrowed case is a cross-process fork
    /// child polling `/dev/dri/card0` itself: it still gets correct `read()` results and correct
    /// readiness from the shared ring, just no cross-process poll wakeup.
    pollee: Pollee<Platform>,
}

/// One host-side frame observer -- see [`DrmFlipLocal::callbacks`].
type DrmFlipCallback = alloc::boxed::Box<dyn Fn(&[u8], u32, u32, u32, u32) + Send + Sync>;

impl<Platform: ShimPlatform> SharedDrmTable<Platform> {
    /// Allocates the region in the cross-process SHARED kernel arena and initializes it.
    ///
    /// Runs exactly once per fork family, from `LinuxShimBuilder::build`'s create branch: every
    /// other process in the family attaches to the already-built `GlobalState` and never calls this
    /// (the same create-vs-attach split `SharedUnixConnTable::new` and `SharedFlockTable::new`
    /// rely on). Byte cost: one `shared_kernel_arena_alloc_bytes` of
    /// `size_of::<SharedDrmRegion<Platform>>()`, which that allocator rounds up to a power of two
    /// with an 8KiB floor -- see [`Self::new_in`] for the debug log that reports the exact figure.
    pub(crate) fn new(platform: &Platform) -> Self {
        let layout = core::alloc::Layout::new::<SharedDrmRegion<Platform>>();
        let arena = platform.shared_kernel_arena_alloc_bytes(layout);
        let (ptr, shared) = match arena {
            Some(ptr) => (ptr.cast::<SharedDrmRegion<Platform>>(), true),
            None => {
                // Arena exhausted: still never a panic (AGENTS.md's standing rule -- the host
                // process IS the whole guest session). Leak a process-private allocation so every
                // later access stays memory-safe, and let `shared` say so.
                litebox_util_log::error!(
                    bytes:% = layout.size();
                    "shared drm table: shared kernel arena exhausted; /dev/dri/card0 objects are \
                     visible to this host process only"
                );
                (
                    core::ptr::NonNull::new(unsafe { alloc::alloc::alloc_zeroed(layout) })
                        .expect("shared drm table: fallback allocation failed")
                        .cast::<SharedDrmRegion<Platform>>(),
                    false,
                )
            }
        };
        Self::new_in(ptr, shared)
    }

    /// Builds the table over caller-owned memory, which [`Self::new`] takes from the shared arena.
    fn new_in(ptr: core::ptr::NonNull<SharedDrmRegion<Platform>>, shared: bool) -> Self {
        // SAFETY (every write): `ptr` names one contiguous, uninitialized `SharedDrmRegion` sized
        // by [`Self::new`]'s `layout`, and writing a freshly built value into uninitialized memory
        // is what `write` is for. Initializing through the pointer ONE FIELD AT A TIME is also the
        // whole point: a `SharedDrmRegion` value is several KiB of stack at once, and `GlobalState`
        // is built by value on a stack that is already near its documented limit.
        unsafe {
            core::ptr::addr_of_mut!((*ptr.as_ptr()).cross_process).write(AtomicBool::new(shared));
            core::ptr::addr_of_mut!((*ptr.as_ptr()).guard).write(litebox::sync::Mutex::new(()));
            core::ptr::addr_of_mut!((*ptr.as_ptr()).next_buffer_handle).write(AtomicU32::new(1));
            core::ptr::addr_of_mut!((*ptr.as_ptr()).next_fb_id).write(AtomicU32::new(1));
            core::ptr::addr_of_mut!((*ptr.as_ptr()).next_map_offset).write(AtomicU32::new(1));
            for i in 0..SHARED_DRM_BUFFERS {
                core::ptr::addr_of_mut!((*ptr.as_ptr()).buffers[i])
                    .write(SharedDumbBufferSlot::new());
            }
            for i in 0..SHARED_DRM_FRAMEBUFFERS {
                core::ptr::addr_of_mut!((*ptr.as_ptr()).framebuffers[i]).write(SharedFbSlot::new());
            }
            core::ptr::addr_of_mut!((*ptr.as_ptr()).crtc_fb).write(AtomicU32::new(0));
            core::ptr::addr_of_mut!((*ptr.as_ptr()).crtc_mode_set).write(AtomicBool::new(false));
            core::ptr::addr_of_mut!((*ptr.as_ptr()).plane_fb).write(AtomicU32::new(0));
            core::ptr::addr_of_mut!((*ptr.as_ptr()).flip_events).write(core::array::from_fn(
                |_| core::array::from_fn(|_| AtomicU8::new(0)),
            ));
            core::ptr::addr_of_mut!((*ptr.as_ptr()).flip_head).write(AtomicU32::new(0));
            core::ptr::addr_of_mut!((*ptr.as_ptr()).flip_count).write(AtomicU32::new(0));
            core::ptr::addr_of_mut!((*ptr.as_ptr()).next_vblank_sequence).write(AtomicU32::new(0));
            core::ptr::addr_of_mut!((*ptr.as_ptr()).is_master).write(AtomicBool::new(false));
            core::ptr::addr_of_mut!((*ptr.as_ptr()).frame_seq).write(AtomicU64::new(0));
        }
        litebox_util_log::debug!(
            region_bytes:% = core::alloc::Layout::new::<SharedDrmRegion<Platform>>().size(),
            shared:% = shared;
            "shared drm table: region initialized"
        );
        Self {
            // SAFETY: `ptr` is non-null and suitably aligned, and the region at it was just
            // initialized field-by-field above. `'static` is sound because this allocation is
            // arena-backed (or a deliberately leaked one) and never reclaimed, and nothing else
            // holds a reference to it.
            region: unsafe { &*ptr.as_ptr() },
        }
    }

    /// Whether this table's objects are visible to every process of the fork family -- see
    /// [`SharedDrmRegion::cross_process`].
    pub(crate) fn is_shared(&self) -> bool {
        self.region.cross_process.load(Ordering::Acquire)
    }

    /// Index of the slot holding dumb buffer `gem_handle`. Callers hold `guard`.
    fn buffer_slot(&self, gem_handle: u32) -> Option<usize> {
        (0..SHARED_DRM_BUFFERS).find(|i| {
            let slot = &self.region.buffers[*i];
            slot.state.load(Ordering::Acquire) == DRM_SLOT_USED
                && slot.gem_handle.load(Ordering::Acquire) == gem_handle
        })
    }

    /// Index of the slot holding the dumb buffer a prior `MAP_DUMB` handed out `offset` for.
    /// Callers hold `guard`.
    fn buffer_slot_by_map_offset(&self, offset: u64) -> Option<usize> {
        (0..SHARED_DRM_BUFFERS).find(|i| {
            let slot = &self.region.buffers[*i];
            slot.state.load(Ordering::Acquire) == DRM_SLOT_USED
                && slot.map_offset.load(Ordering::Acquire) == offset
        })
    }

    /// Index of the slot holding framebuffer `fb_id`. Callers hold `guard`.
    fn fb_slot(&self, fb_id: u32) -> Option<usize> {
        (0..SHARED_DRM_FRAMEBUFFERS).find(|i| {
            let slot = &self.region.framebuffers[*i];
            slot.state.load(Ordering::Acquire) == DRM_SLOT_USED
                && slot.fb_id.load(Ordering::Acquire) == fb_id
        })
    }

    /// How many framebuffers currently exist, for `GETRESOURCES`' `count_fbs`. Callers hold
    /// `guard`.
    fn fb_count(&self) -> usize {
        self.region
            .framebuffers
            .iter()
            .filter(|s| s.state.load(Ordering::Acquire) == DRM_SLOT_USED)
            .count()
    }

    /// Reclaims every slot whose owning host process is gone, so a fork family that outlived the
    /// process which created the objects does not leak their capacity for the rest of the session.
    /// Callers hold `guard`. This is the same dead-pid reclamation `SharedFlockTable` already does.
    fn reclaim_dead(&self, platform: &Platform) {
        for i in 0..SHARED_DRM_BUFFERS {
            let slot = &self.region.buffers[i];
            let owner = slot.owner_host.load(Ordering::Acquire);
            if owner != 0 && !platform.is_process_alive(owner) {
                slot.state.store(DRM_SLOT_FREE, Ordering::Release);
                slot.set_handle(None);
                slot.map_offset.store(0, Ordering::Release);
                slot.owner_host.store(0, Ordering::Release);
            }
        }
        for i in 0..SHARED_DRM_FRAMEBUFFERS {
            let slot = &self.region.framebuffers[i];
            let owner = slot.owner_host.load(Ordering::Acquire);
            if owner != 0 && !platform.is_process_alive(owner) {
                slot.state.store(DRM_SLOT_FREE, Ordering::Release);
                slot.fb_id.store(0, Ordering::Release);
                slot.owner_host.store(0, Ordering::Release);
            }
        }
    }

    /// Claims a free buffer slot for `gem_handle`, reclaiming dead owners first if the table is
    /// full. `None` means the table genuinely cannot represent another buffer. Callers hold
    /// `guard`.
    fn claim_buffer(&self, platform: &Platform) -> Option<usize> {
        if let Some(i) = (0..SHARED_DRM_BUFFERS)
            .find(|i| self.region.buffers[*i].state.load(Ordering::Acquire) == DRM_SLOT_FREE)
        {
            return Some(i);
        }
        self.reclaim_dead(platform);
        let i = (0..SHARED_DRM_BUFFERS)
            .find(|i| self.region.buffers[*i].state.load(Ordering::Acquire) == DRM_SLOT_FREE)?;
        Some(i)
    }

    /// Claims a free framebuffer slot for `fb_id`. See [`Self::claim_buffer`]. Callers hold `guard`.
    fn claim_fb(&self, platform: &Platform) -> Option<usize> {
        if let Some(i) = (0..SHARED_DRM_FRAMEBUFFERS)
            .find(|i| self.region.framebuffers[*i].state.load(Ordering::Acquire) == DRM_SLOT_FREE)
        {
            return Some(i);
        }
        self.reclaim_dead(platform);
        let i = (0..SHARED_DRM_FRAMEBUFFERS).find(|i| {
            self.region.framebuffers[*i].state.load(Ordering::Acquire) == DRM_SLOT_FREE
        })?;
        Some(i)
    }

    /// Returns `gem_handle`'s fake `mmap` offset, allocating one the first time -- the lazy
    /// allocation both `map_dumb` and `prime_export_offset` want (a real client always maps a dumb
    /// buffer before/around exporting it, but nothing in the UAPI requires that ordering, so the
    /// export-first case has to work too). `None` = no such buffer.
    fn map_offset_for(&self, gem_handle: u32) -> Option<u64> {
        let _guard = self.region.guard.lock();
        let i = self.buffer_slot(gem_handle)?;
        let slot = &self.region.buffers[i];
        let existing = slot.map_offset.load(Ordering::Acquire);
        if existing != 0 {
            return Some(existing);
        }
        // Real DRM fake offsets are page-aligned, opaque `mmap()` targets in a reserved range
        // distinct from any real memory address. `<< 12` (page-align) keeps this device's offsets
        // shaped the same way without claiming to match the kernel's exact internal allocation
        // scheme (which is driver-private and not part of the UAPI contract clients rely on).
        let fresh = u64::from(self.region.next_map_offset.fetch_add(1, Ordering::Relaxed)) << 12;
        slot.map_offset.store(fresh, Ordering::Release);
        Some(fresh)
    }

    /// Appends one flip-completion event to the shared ring, dropping the OLDEST if it is already
    /// full (a real kernel's own event ring behaves the same way -- a client that never reads must
    /// not be able to fail the presenting process's `PAGE_FLIP`). Callers hold `guard`.
    fn flip_push(&self, event: &DrmEventVblank) {
        let count = self.region.flip_count.load(Ordering::Acquire) as usize;
        let head = self.region.flip_head.load(Ordering::Acquire) as usize;
        let idx = if count == SHARED_DRM_FLIP_EVENTS {
            // Full: overwrite the oldest in place and advance the head past it.
            let idx = head;
            self.region.flip_head.store(
                ((head + 1) % SHARED_DRM_FLIP_EVENTS) as u32,
                Ordering::Release,
            );
            idx
        } else {
            let idx = (head + count) % SHARED_DRM_FLIP_EVENTS;
            self.region
                .flip_count
                .store((count + 1) as u32, Ordering::Release);
            idx
        };
        let bytes = event.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            self.region.flip_events[idx][i].store(*b, Ordering::Release);
        }
    }

    /// Pops the oldest flip-completion event's exact byte image. Callers hold `guard`.
    fn flip_pop(&self) -> Option<[u8; FLIP_EVENT_BYTES]> {
        let count = self.region.flip_count.load(Ordering::Acquire) as usize;
        if count == 0 {
            return None;
        }
        let head = self.region.flip_head.load(Ordering::Acquire) as usize;
        let mut out = [0u8; FLIP_EVENT_BYTES];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = self.region.flip_events[head][i].load(Ordering::Acquire);
        }
        self.region.flip_head.store(
            ((head + 1) % SHARED_DRM_FLIP_EVENTS) as u32,
            Ordering::Release,
        );
        self.region
            .flip_count
            .store((count - 1) as u32, Ordering::Release);
        Some(out)
    }

    /// A snapshot of the current front buffer's identity for a host-side control channel to hand
    /// off to an external presenter process (`docs/presenter-process-design.md` section 2.2/2.3).
    /// Reads the exact same `crtc_fb`/`framebuffers`/`buffers` bookkeeping
    /// [`Self::notify_flip_callback`] already consults, but returns the platform's own
    /// [`ShimPlatform::SharedMemoryHandle`] itself rather than mapping it and handing out bytes --
    /// unlike [`DrmFlipLocal::callbacks`] (a `Box<dyn Fn>` that cannot carry this associated type
    /// across a trait object, see that field's own doc comment), a plain generic method call from
    /// a caller that already knows the concrete `Platform` type (the runner) has no such
    /// limitation. `None` until the guest has attached a framebuffer to the CRTC at least once.
    pub(crate) fn scanout_snapshot(&self) -> Option<ScanoutSnapshot<Platform>> {
        let _guard = self.region.guard.lock();
        let fb_id = self.region.crtc_fb.load(Ordering::Acquire);
        if fb_id == 0 {
            return None;
        }
        let fb_slot = self.fb_slot(fb_id)?;
        let fb = &self.region.framebuffers[fb_slot];
        let buffer = &self.region.buffers[self.buffer_slot(fb.handle.load(Ordering::Acquire))?];
        Some(ScanoutSnapshot {
            handle: buffer.get_handle()?,
            size: buffer.size.load(Ordering::Acquire),
            width: fb.width.load(Ordering::Acquire),
            height: fb.height.load(Ordering::Acquire),
            pitch: buffer.pitch.load(Ordering::Acquire),
            pixel_format: fb.pixel_format.load(Ordering::Acquire),
            // Single-buffer dumb-buffer allocation: the whole section backs one buffer starting
            // at byte 0 (see `create_dumb`). A future double-buffered upgrade (design doc section
            // 2.2's own noted future step) is the only case that would ever make this non-zero.
            offset: 0,
            seq: self.region.frame_seq.load(Ordering::Acquire),
        })
    }

    /// Pop the oldest pending flip-completion event, if any, encoded as the exact bytes a real
    /// `read()` on a DRM device fd would return (a [`DrmEvent`] header immediately followed by
    /// its [`DrmEventVblank`] body -- real DRM's `read()` contract, one or more whole events per
    /// call, never a partial one). `None` means no event is pending -- the caller (see
    /// `syscalls::file::do_read`'s DRI-fd branch) is responsible for real Linux's actual
    /// `read()`-with-nothing-pending behavior (blocks, or `EAGAIN` if the fd is non-blocking).
    pub(crate) fn pop_flip_event_bytes(&self) -> Option<alloc::vec::Vec<u8>> {
        let _guard = self.region.guard.lock();
        let event = self.flip_pop()?;
        let mut bytes = alloc::vec::Vec::with_capacity(FLIP_EVENT_BYTES);
        bytes.extend_from_slice(&event);
        Some(bytes)
    }

    /// Whether a flip-completion event is currently queued for a DRM device fd's `read()` to pop
    /// -- the readiness half of the same fact [`Self::pop_flip_event_bytes`] consumes. Exists so
    /// `syscalls::epoll::EpollDescriptor::poll`'s `File` arm can report a DRM fd `Events::IN`
    /// exactly when this device genuinely has something to deliver, mirroring
    /// `SharedEvdevTable::has_pending`'s identical role for `/dev/input/event0`. Without this, a
    /// real compositor's own event loop -- which registers the DRM fd with `epoll`/`poll` and
    /// waits for it to become readable BEFORE calling `read()`, rather than reading unconditionally
    /// right after issuing a flip -- never observes a flip-complete notification for any repaint
    /// after its first, since nothing ever marks the fd ready: confirmed live (litebox-xfce-1,
    /// sub-session 28) against a real `weston --backend=drm-backend.so` run, which issued exactly
    /// one `SETCRTC`+`PAGE_FLIP` pair at startup and never repainted again for the rest of a 60+
    /// second run with three live Wayland/X11 client applications attached the entire time.
    pub(crate) fn has_pending_flip_events(&self) -> bool {
        let _guard = self.region.guard.lock();
        self.region.flip_count.load(Ordering::Acquire) != 0
    }

    pub(crate) fn get_resources(&self, ptr: UserPtrMut<DrmModeCardRes>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;

        // Two-call size-probe pattern: if the caller supplied a buffer (non-zero count and
        // non-null ptr), fill it; regardless, always report the true counts back. A real client
        // that under-sized its buffer against a previous probe gets a short/incomplete fill,
        // matching this device's single-object-per-category invariant (it never has more than
        // one connector/encoder/CRTC/framebuffer to report, so this can never actually
        // truncate).
        if req.count_connectors > 0 && req.connector_id_ptr != 0 {
            let out = UserPtrMut::<u32>::from_usize(req.connector_id_ptr as usize);
            out.write_at_offset::<Platform>(0, VIRTUAL_CONNECTOR_ID)
                .ok_or(Errno::EFAULT)?;
        }
        if req.count_encoders > 0 && req.encoder_id_ptr != 0 {
            let out = UserPtrMut::<u32>::from_usize(req.encoder_id_ptr as usize);
            out.write_at_offset::<Platform>(0, VIRTUAL_ENCODER_ID)
                .ok_or(Errno::EFAULT)?;
        }
        if req.count_crtcs > 0 && req.crtc_id_ptr != 0 {
            let out = UserPtrMut::<u32>::from_usize(req.crtc_id_ptr as usize);
            out.write_at_offset::<Platform>(0, VIRTUAL_CRTC_ID)
                .ok_or(Errno::EFAULT)?;
        }
        let _guard = self.region.guard.lock();
        let fb_count = u32::try_from(self.fb_count()).unwrap_or(u32::MAX);
        if req.count_fbs > 0 && req.fb_id_ptr != 0 {
            let out = UserPtrMut::<u32>::from_usize(req.fb_id_ptr as usize);
            let mut i: u32 = 0;
            for slot in self.region.framebuffers.iter() {
                if i >= req.count_fbs {
                    break;
                }
                if slot.state.load(Ordering::Acquire) != DRM_SLOT_USED {
                    continue;
                }
                out.write_at_offset::<Platform>(
                    isize::try_from(i).map_err(|_| Errno::EINVAL)?,
                    slot.fb_id.load(Ordering::Acquire),
                )
                .ok_or(Errno::EFAULT)?;
                i += 1;
            }
        }
        drop(_guard);

        req.count_connectors = 1;
        req.count_encoders = 1;
        req.count_crtcs = 1;
        req.count_fbs = fb_count;
        req.min_width = VIRTUAL_WIDTH;
        req.max_width = VIRTUAL_WIDTH;
        req.min_height = VIRTUAL_HEIGHT;
        req.max_height = VIRTUAL_HEIGHT;
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn get_connector(&self, ptr: UserPtrMut<DrmModeGetConnector>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.connector_id != 0 && req.connector_id != VIRTUAL_CONNECTOR_ID {
            return Err(Errno::ENOENT);
        }
        if req.count_encoders > 0 && req.encoders_ptr != 0 {
            let out = UserPtrMut::<u32>::from_usize(req.encoders_ptr as usize);
            out.write_at_offset::<Platform>(0, VIRTUAL_ENCODER_ID)
                .ok_or(Errno::EFAULT)?;
        }
        if req.count_modes > 0 && req.modes_ptr != 0 {
            let out = UserPtrMut::<DrmModeModeinfo>::from_usize(req.modes_ptr as usize);
            out.write_at_offset::<Platform>(0, virtual_mode())
                .ok_or(Errno::EFAULT)?;
        }
        req.count_encoders = 1;
        req.count_modes = 1;
        req.count_props = 0;
        req.connector_id = VIRTUAL_CONNECTOR_ID;
        req.encoder_id = VIRTUAL_ENCODER_ID;
        req.connector_type = DRM_MODE_CONNECTOR_VIRTUAL;
        req.connector_type_id = 1;
        // `1` = `DRM_MODE_CONNECTED`: this virtual display is always "plugged in", matching how
        // a software-only device (e.g. `vkms`) has nothing to report as physically disconnected.
        req.connection = 1;
        req.mm_width = 0;
        req.mm_height = 0;
        litebox_util_log::warn!(
            count_modes:? = req.count_modes,
            count_encoders:? = req.count_encoders,
            encoders_ptr:? = req.encoders_ptr,
            connection:? = req.connection,
            connector_type:? = req.connector_type;
            "drm-ioctl: GETCONNECTOR reply"
        );
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn get_encoder(&self, ptr: UserPtrMut<DrmModeGetEncoder>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        litebox_util_log::warn!(
            encoder_id:? = req.encoder_id;
            "drm-ioctl: GETENCODER request"
        );
        if req.encoder_id != 0 && req.encoder_id != VIRTUAL_ENCODER_ID {
            return Err(Errno::ENOENT);
        }
        req.encoder_id = VIRTUAL_ENCODER_ID;
        req.encoder_type = DRM_MODE_ENCODER_VIRTUAL;
        req.crtc_id = VIRTUAL_CRTC_ID;
        // Bit 0 set = "can drive CRTC index 0", the only CRTC this device has.
        req.possible_crtcs = 0b1;
        req.possible_clones = 0;
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn get_crtc(&self, ptr: UserPtrMut<DrmModeCrtc>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.crtc_id != 0 && req.crtc_id != VIRTUAL_CRTC_ID {
            return Err(Errno::ENOENT);
        }
        let _guard = self.region.guard.lock();
        req.crtc_id = VIRTUAL_CRTC_ID;
        req.fb_id = self.region.crtc_fb.load(Ordering::Acquire);
        req.x = 0;
        req.y = 0;
        req.gamma_size = 0;
        if self.region.crtc_mode_set.load(Ordering::Acquire) {
            req.mode_valid = 1;
            req.mode = virtual_mode();
        } else {
            req.mode_valid = 0;
        }
        litebox_util_log::warn!(
            fb_id:? = req.fb_id,
            mode_valid:? = req.mode_valid,
            hdisplay:? = req.mode.hdisplay,
            vdisplay:? = req.mode.vdisplay;
            "drm-ioctl: GETCRTC reply"
        );
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn set_crtc(
        &self,
        platform: &Platform,
        local: &DrmFlipLocal<Platform>,
        ptr: UserPtr<DrmModeCrtc>,
    ) -> Result<u32, Errno> {
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.crtc_id != VIRTUAL_CRTC_ID {
            return Err(Errno::ENOENT);
        }
        {
            let _guard = self.region.guard.lock();
            if req.fb_id != 0 && self.fb_slot(req.fb_id).is_none() {
                return Err(Errno::ENOENT);
            }
        }
        // This device advertises exactly ONE mode (`VIRTUAL_WIDTH`x`VIRTUAL_HEIGHT`, see
        // `GETCONNECTOR`'s `modes` array and `min_width`/`max_width`/`min_height`/`max_height` in
        // `get_resources`) -- there is no real hardware behind this to somehow honor a different
        // one. Real DRM validates a `mode_valid` SETCRTC's requested mode against what the CRTC
        // can actually drive and returns `EINVAL` for one it can't (a real client already knows to
        // fall back on that error); this device previously stored ANY `hdisplay`/`vdisplay` the
        // guest asked for unconditionally and always reported success, silently accepting a
        // resolution it cannot actually scan out at. That mismatch then had to be quietly absorbed
        // downstream (the host presenter's own frame/surface-size handling) instead of ever being
        // reported to the one place -- the guest -- that could have chosen a supported mode
        // instead. Reject it here, loudly, the same way real DRM would.
        if req.mode_valid != 0
            && (u32::from(req.mode.hdisplay) != VIRTUAL_WIDTH
                || u32::from(req.mode.vdisplay) != VIRTUAL_HEIGHT)
        {
            litebox_util_log::error!(
                requested_hdisplay:? = req.mode.hdisplay,
                requested_vdisplay:? = req.mode.vdisplay,
                advertised_width:% = VIRTUAL_WIDTH,
                advertised_height:% = VIRTUAL_HEIGHT;
                "drm-ioctl: SETCRTC rejected -- requested mode does not match the device's one advertised mode"
            );
            return Err(Errno::EINVAL);
        }
        litebox_util_log::warn!(
            fb_id:? = req.fb_id,
            mode_valid:? = req.mode_valid,
            hdisplay:? = req.mode.hdisplay,
            vdisplay:? = req.mode.vdisplay,
            count_connectors:? = req.count_connectors;
            "drm-ioctl: SETCRTC request"
        );
        let attached_fb = {
            let _guard = self.region.guard.lock();
            self.region.crtc_fb.store(
                if req.fb_id == 0 { 0 } else { req.fb_id },
                Ordering::Release,
            );
            // Track mode-configured state independent of `fb_id` -- see
            // [`SharedDrmRegion::crtc_mode_set`]'s own doc comment for why conflating the two
            // previously misreported `GETCRTC` during a legitimate empty-buffer modeset
            // transition.
            if req.mode_valid != 0 {
                self.region.crtc_mode_set.store(true, Ordering::Release);
            } else if req.fb_id == 0 {
                // A real "disable this CRTC" SETCRTC (no mode, no framebuffer) genuinely clears
                // the configured mode too -- distinct from an empty-buffer-during-modeset call,
                // which still carries `mode_valid != 0` and is handled by the branch above.
                self.region.crtc_mode_set.store(false, Ordering::Release);
            }
            self.region.crtc_fb.load(Ordering::Acquire)
        };
        // A legacy (non-atomic) client's own repaint loop -- weston's DRM backend with a shadow
        // framebuffer included, confirmed live this session -- commonly re-attaches its updated
        // framebuffer via repeated `SETCRTC` calls rather than `PAGE_FLIP` once initial modesetting
        // has already happened once: `PAGE_FLIP` itself is defined to require a CRTC that already
        // has a framebuffer attached (real `drmModePageFlip`'s own contract), so a client is free to
        // keep using `SETCRTC` for every subsequent frame instead. `page_flip`'s own host-side
        // presentation callback exists precisely to forward whatever the guest most recently
        // scanned out to a `--gui` runner's window -- restricting that forwarding to `PAGE_FLIP`
        // alone silently drops every frame a `SETCRTC`-only repaint loop produces, leaving the host
        // window black even while the guest compositor is genuinely running and correctly updating
        // its own (litebox-emulated) CRTC state. Fire the identical callback here too, whenever this
        // call actually attaches a real framebuffer (not the `fb_id == 0` detach case, which has
        // nothing to present).
        if attached_fb != 0 {
            self.notify_flip_callback(platform, local, attached_fb);
        }
        Ok(0)
    }

    /// Shared by [`Self::set_crtc`] and [`Self::page_flip`]: maps `fb_id`'s backing dumb-buffer
    /// memory host-side and forwards it to the installed presentation callback, if any. A no-op
    /// (and a silently-swallowed lookup failure) when no callback is installed or the mapping
    /// fails, matching `page_flip`'s own established "a host-side presentation miss must never
    /// fail the guest's own ioctl" contract.
    ///
    /// `callback_count == 0` alone never skips this: `--gui`/`LITEBOX_DUMP_FRAMES` (what actually
    /// populates `flip_callbacks`) and `LITEBOX_DRM_TRACE` (what the digest/scanout-byte logging
    /// below is gated on, via `drm_trace_enabled()`) are two independent diagnostics with no
    /// reason to depend on each other -- a trace-only run with neither presenter nor frame-dump
    /// registered a single callback would otherwise see every `diag-drm-*` line below silently
    /// vanish, indistinguishable from "no presents happened" (see this fn's own fix history for
    /// the LYING INSTRUMENTS writeup this corrects).
    fn notify_flip_callback(
        &self,
        platform: &Platform,
        local: &DrmFlipLocal<Platform>,
        fb_id: u32,
    ) {
        // Unconditional: see this field's own doc comment for why this must not be gated behind
        // `callback_count == 0` the way the rest of this function is.
        self.region.frame_seq.fetch_add(1, Ordering::Release);
        let callback_count = local.callbacks.lock().len();
        litebox_util_log::debug!(callback_count:? = callback_count; "drm-diag: notify_flip_callback called");
        if callback_count == 0 && !drm_trace_enabled() {
            return;
        }
        let Some((handle, size, width, height, pitch, pixel_format)) = ({
            let _guard = self.region.guard.lock();
            self.fb_slot(fb_id).and_then(|fb_i| {
                let fb = &self.region.framebuffers[fb_i];
                // No `.expect(...)`: a framebuffer whose backing buffer was destroyed dangles in
                // real Linux too (see `destroy_dumb`'s own doc comment), and a panic here would
                // take down the whole host process -- which IS the guest session (AGENTS.md).
                // `destroy_dumb` deliberately leaves such a framebuffer behind, so warn and skip the
                // present rather than kill the session: this callback reports nothing to the guest.
                let Some(b_i) = self.buffer_slot(fb.handle.load(Ordering::Acquire)) else {
                    litebox_util_log::warn!(
                        fb_id:? = fb_id, handle:? = fb.handle.load(Ordering::Acquire);
                        "drm-ioctl: framebuffer references a destroyed dumb buffer, skipping flip"
                    );
                    return None;
                };
                let buffer = &self.region.buffers[b_i];
                buffer.get_handle().map(|handle| {
                    (
                        handle,
                        buffer.size.load(Ordering::Acquire),
                        fb.width.load(Ordering::Acquire),
                        fb.height.load(Ordering::Acquire),
                        buffer.pitch.load(Ordering::Acquire),
                        fb.pixel_format.load(Ordering::Acquire),
                    )
                })
            })
        }) else {
            return;
        };
        match platform.map_shared_memory(
            handle,
            0..size,
            litebox::platform::page_mgmt::MemoryRegionPermissions::READ,
            litebox::platform::page_mgmt::FixedAddressBehavior::Hint,
        ) {
            Ok(mapped_ptr) => {
                let addr = mapped_ptr.as_usize();
                // SAFETY: `map_shared_memory` just returned this exact `addr`/`size` as a freshly
                // established, readable mapping of `handle`'s real backing storage; nothing else in
                // this function (or reachable from the callback, which only receives a `&[u8]`
                // slice, not the address) can invalidate it before the `unmap_shared_memory` call
                // immediately below.
                let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
                // Log the address this fb is mapped at, so a range destroyed by
                // allocate_pages (see diag-reclaim) can be matched against it.
                if drm_trace_enabled() {
                    litebox_util_log::debug!(
                        fb_id:% = fb_id, addr:% = addr, size:% = size;
                        "diag-drm-fb-addr"
                    );
                }
                // Sample the SHARED BACKING STORE directly (this mapping was just
                // established fresh from `handle`, so it is not a stale view). A black
                // captured frame with non-zero bytes here would mean the capture path is
                // at fault; zero bytes here means the backing store itself really was
                // wiped, and the search moves to whatever wiped it.
                if drm_trace_enabled() {
                    let n = bytes.len();
                    let nz = bytes.iter().filter(|b| **b != 0).count();
                    // A cheap content fingerprint. If this never changes flip after
                    // flip, the guest is not drawing into the buffer at all -- which
                    // would mean the blackout is a compositing problem (no visible
                    // surface, so a legitimately black frame) rather than memory being
                    // destroyed. That distinction decides whether this is a litebox bug
                    // at all, so it is worth measuring before any more memory work.
                    let mut sum: u64 = 0;
                    for (i, b) in bytes.iter().enumerate().step_by(4096) {
                        sum = sum.wrapping_mul(31).wrapping_add((*b as u64) ^ (i as u64));
                    }
                    litebox_util_log::debug!(fb_id:% = fb_id, digest:% = sum; "diag-drm-digest");
                    litebox_util_log::debug!(
                        fb_id:% = fb_id,
                        bytes_len:% = n,
                        nonzero_bytes:% = nz,
                        first8:? = &bytes[..core::cmp::min(8, n)],
                        mid8:? = &bytes[n / 2..core::cmp::min(n / 2 + 8, n)];
                        "diag-drm-scanout-bytes"
                    );
                }
                if drm_trace_enabled() {
                    // Sample the SOURCE bytes right at this fresh mapping, before the callback
                    // sees them -- distinguishes "the shared buffer itself is zero" (a real
                    // wipe/coherency bug upstream of this function) from "the buffer holds real
                    // content but something downstream of this function drops it" (a capture/
                    // presentation-path bug instead). Counting only a bounded prefix keeps this
                    // cheap enough to run on every flip while `LITEBOX_DRM_TRACE=1` is set.
                    let sample_len = bytes.len().min(4096);
                    let non_zero_in_sample =
                        bytes[..sample_len].iter().filter(|b| **b != 0).count();
                    litebox_util_log::debug!(
                        fb_id:% = fb_id, handle:? = handle, addr:% = addr, size:% = size,
                        sample_len:% = sample_len, non_zero_in_sample:% = non_zero_in_sample;
                        "diag-drm-flip-source-bytes"
                    );
                }
                // Every observer sees the same frame. The SAFETY note below depends on all of
                // them having returned before the mapping is torn down, which this loop
                // guarantees exactly as the previous single call did.
                for callback in local.callbacks.lock().iter() {
                    callback(bytes, width, height, pitch, pixel_format);
                }
                // SAFETY: `addr..addr+size` is exactly the range just mapped above, and the
                // callback (the only other holder of a reference into it) has already returned by
                // this point -- no other code can still be reading through it.
                let _ = unsafe { platform.unmap_shared_memory(addr..addr + size) };
            }
            Err(_) => {
                // A host-side presentation window failing to see one frame is not a reason to fail
                // the guest's own ioctl -- see `page_flip`'s identical reasoning at its own
                // corresponding `Err` arm.
            }
        }
    }

    pub(crate) fn get_plane_resources(
        &self,
        ptr: UserPtrMut<DrmModeGetPlaneRes>,
    ) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        // Two-call size-probe pattern, same as `get_resources`: this device has exactly one
        // plane, so a short-sized caller buffer can never actually truncate.
        if req.count_planes > 0 && req.plane_id_ptr != 0 {
            let out = UserPtrMut::<u32>::from_usize(req.plane_id_ptr as usize);
            out.write_at_offset::<Platform>(0, VIRTUAL_PLANE_ID)
                .ok_or(Errno::EFAULT)?;
        }
        req.count_planes = 1;
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn get_plane(&self, ptr: UserPtrMut<DrmModeGetPlane>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.plane_id != 0 && req.plane_id != VIRTUAL_PLANE_ID {
            return Err(Errno::ENOENT);
        }
        // Two-call size-probe pattern for `format_type_ptr`, same shape as `get_connector`'s
        // `modes_ptr`/`encoders_ptr` handling. This device's plane only ever carries the one
        // format its dumb buffers support.
        if req.count_format_types > 0 && req.format_type_ptr != 0 {
            let out = UserPtrMut::<u32>::from_usize(req.format_type_ptr as usize);
            out.write_at_offset::<Platform>(0, DRM_FORMAT_XRGB8888)
                .ok_or(Errno::EFAULT)?;
        }
        let _guard = self.region.guard.lock();
        let plane_fb = self.region.plane_fb.load(Ordering::Acquire);
        req.plane_id = VIRTUAL_PLANE_ID;
        req.crtc_id = if plane_fb == 0 { 0 } else { VIRTUAL_CRTC_ID };
        req.fb_id = plane_fb;
        // Bit 0 set = "can be attached to CRTC index 0", the only CRTC this device has.
        req.possible_crtcs = 0b1;
        req.gamma_size = 0;
        req.count_format_types = 1;
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn set_plane(&self, ptr: UserPtr<DrmModeSetPlane>) -> Result<u32, Errno> {
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.plane_id != VIRTUAL_PLANE_ID {
            return Err(Errno::ENOENT);
        }
        // `fb_id == 0` is the real-DRM-defined way to disable a plane (detach whatever
        // framebuffer it currently shows); any other `fb_id` must name a real framebuffer object,
        // same validation `set_crtc` already applies to its own `fb_id` field.
        let _guard = self.region.guard.lock();
        if req.fb_id == 0 {
            self.region.plane_fb.store(0, Ordering::Release);
            return Ok(0);
        }
        if req.crtc_id != VIRTUAL_CRTC_ID {
            return Err(Errno::ENOENT);
        }
        if self.fb_slot(req.fb_id).is_none() {
            return Err(Errno::ENOENT);
        }
        self.region.plane_fb.store(req.fb_id, Ordering::Release);
        Ok(0)
    }

    /// `DRM_IOCTL_VERSION` -- the first ioctl every real libdrm-based client calls (`drmOpen`
    /// itself does this internally). Two-call size-probe pattern for the three trailing
    /// `(len, ptr)` string pairs, same shape as `get_resources`'s object-ID arrays: a caller with
    /// `name_len == 0` (or a null `name` pointer) only learns the true length; a caller with a
    /// real buffer gets it filled, truncated to whichever is smaller (matching the real kernel's
    /// own `drm_copy_field` behavior -- a short caller buffer is not an error).
    pub(crate) fn version(&self, ptr: UserPtrMut<DrmVersion>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;

        const NAME: &[u8] = b"litebox";
        const DATE: &[u8] = b"20260101";
        const DESC: &[u8] = b"litebox virtual DRM/KMS device";

        fn fill_field<Platform: ShimPlatform>(
            user_ptr: u64,
            user_len: u64,
            value: &[u8],
        ) -> Result<u64, Errno> {
            if user_len > 0 && user_ptr != 0 {
                let n = (user_len as usize).min(value.len());
                let out = UserPtrMut::<u8>::from_usize(user_ptr as usize);
                out.write_slice_at_offset::<Platform>(0, &value[..n])
                    .ok_or(Errno::EFAULT)?;
            }
            Ok(value.len() as u64)
        }

        req.version_major = 1;
        req.version_minor = 0;
        req.version_patchlevel = 0;
        req.name_len = fill_field::<Platform>(req.name, req.name_len, NAME)?;
        req.date_len = fill_field::<Platform>(req.date, req.date_len, DATE)?;
        req.desc_len = fill_field::<Platform>(req.desc, req.desc_len, DESC)?;
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    /// `DRM_IOCTL_GET_CAP` -- query a single capability. This device genuinely supports dumb
    /// buffers (the only allocation path it has, see `create_dumb`), so `DRM_CAP_DUMB_BUFFER`
    /// reports `1`. It also reports `DRM_CAP_TIMESTAMP_MONOTONIC` and
    /// `DRM_CAP_CRTC_IN_VBLANK_EVENT` (see those constants' own doc comments) -- real
    /// compositors including weston's and wlroots' DRM backends require these capabilities to
    /// be present just to initialize at all, and `DRM_CAP_PRIME` (see its own doc comment,
    /// handled separately below since its value is a bitmask, not a boolean). `DRM_CAP_CURSOR_WIDTH`
    /// / `DRM_CAP_CURSOR_HEIGHT` report the real kernel's own hardcoded default of `64` (see their
    /// own doc comment for why `0` here is not a safe "unsupported" fallback the way it is for
    /// every other capability below -- xf86-video-modesetting takes a successful `0` at face
    /// value, allocates a 0x0 cursor dumb buffer, and a later unconditional, unchecked
    /// `dumb_bo_map` on the resulting `NULL` `cursor_bo` SIGSEGVs during `EnterVT`). Any other
    /// capability (dumb-buffer preferred-depth,
    /// async page-flip, atomic modesetting, etc.) reports `0` (unsupported), the real kernel's own
    /// behavior for a capability a driver never registered, rather than fabricating support this
    /// device does not actually have.
    pub(crate) fn get_cap(&self, ptr: UserPtrMut<DrmGetCap>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        req.value = if req.capability == DRM_CAP_DUMB_BUFFER
            || req.capability == DRM_CAP_TIMESTAMP_MONOTONIC
            || req.capability == DRM_CAP_CRTC_IN_VBLANK_EVENT
        {
            1
        } else if req.capability == DRM_CAP_CURSOR_WIDTH || req.capability == DRM_CAP_CURSOR_HEIGHT
        {
            64
        } else if req.capability == DRM_CAP_PRIME {
            // wlroots' `check_drm_features()` treats neither import nor export bit set as
            // fatal (see `DRM_CAP_PRIME`'s own doc comment) -- report both so backend
            // creation proceeds. `DRM_PRIME_CAP_EXPORT` is now backed by a real
            // `DRM_IOCTL_PRIME_HANDLE_TO_FD` handler (see [`Self::prime_export_offset`]);
            // `DRM_PRIME_CAP_IMPORT` stays aspirational (no `DRM_IOCTL_PRIME_FD_TO_HANDLE`
            // ioctl is implemented, since litebox never reaches a code path -- client-side
            // dma-buf import -- that would exercise it).
            DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT
        } else {
            0
        };
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    /// `DRM_IOCTL_SET_CLIENT_CAP` -- opt into a single `DRM_CLIENT_CAP_*` behavior. This device's
    /// plane API has no primary/overlay/cursor distinction and always exposes its one virtual
    /// plane regardless (see [`DRM_CLIENT_CAP_UNIVERSAL_PLANES`]'s own doc comment), so there is
    /// no actual state to track for that capability: it is accepted unconditionally. Every other
    /// `DRM_CLIENT_CAP_*` (atomic modesetting, stereo 3D, etc.) reports `EINVAL`, the real
    /// kernel's response to a capability the driver never registered -- this device's mode-setting
    /// is the legacy `SETCRTC`/`PAGE_FLIP` API only, so claiming e.g. atomic support here would be
    /// a lie a client could act on (calling `DRM_IOCTL_MODE_ATOMIC`, which does not exist here).
    pub(crate) fn set_client_cap(&self, ptr: UserPtr<DrmSetClientCap>) -> Result<u32, Errno> {
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        match req.capability {
            DRM_CLIENT_CAP_UNIVERSAL_PLANES => Ok(0),
            _ => Err(Errno::EINVAL),
        }
    }

    /// `DRM_IOCTL_SET_MASTER` -- real DRM enforces single-master-per-device for mode-setting
    /// ioctls; this device has exactly one possible client (litebox has no concept of a second
    /// concurrent guest process opening the same virtual `/dev/dri/card0` today) and does not
    /// gate any of its own mode-setting ioctls on master status, so this always succeeds. Tracked
    /// (`is_master`) purely so a client that queries its own master status back gets a consistent
    /// answer, not because anything else currently depends on it.
    // `Result<u32, Errno>` here always resolves to `Ok` (this virtual device has no real
    // multi-master contention to reject a caller over) but matches every other handler's
    // signature so `drm_ioctl`'s dispatch match stays uniform -- not a real "unnecessarily
    // wrapped" case, just a shared interface.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn set_master(&self) -> Result<u32, Errno> {
        self.region.is_master.store(true, Ordering::Relaxed);
        Ok(0)
    }

    /// `DRM_IOCTL_DROP_MASTER`. See [`Self::set_master`]'s doc comment.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn drop_master(&self) -> Result<u32, Errno> {
        self.region.is_master.store(false, Ordering::Relaxed);
        Ok(0)
    }

    /// `DRM_IOCTL_GET_MAGIC` -- see [`DRM_IOCTL_GET_MAGIC`]'s own doc comment for why this
    /// device always hands back the same fixed magic value rather than a real per-client
    /// random one.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn get_magic(&self, ptr: UserPtrMut<DrmAuth>) -> Result<u32, Errno> {
        ptr.write_at_offset::<Platform>(
            0,
            DrmAuth {
                magic: DRM_AUTH_MAGIC_VALUE,
            },
        )
        .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    /// `DRM_IOCTL_AUTH_MAGIC` -- see [`DRM_IOCTL_GET_MAGIC`]'s doc comment. Any magic value
    /// is accepted: this device has exactly one possible client, so there is no real
    /// mismatched-magic case to reject (real DRM's `EINVAL` here signals "no client is
    /// authenticated under that magic," which cannot happen when `GET_MAGIC` unconditionally
    /// returns the one fixed value this device will ever hand out).
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn auth_magic(&self, _ptr: UserPtr<DrmAuth>) -> Result<u32, Errno> {
        Ok(0)
    }

    /// `DRM_IOCTL_MODE_OBJ_GETPROPERTIES` -- discovered missing via a real libdrm client
    /// (`smithay`'s `backend_drm`, `docs/wayland-drm-backend-probe/`) failing outright on this
    /// exact call immediately after `GETCONNECTOR` succeeded: unimplemented before this, every
    /// real client following the standard connector-properties-query sequence failed before
    /// reaching any further DRM work.
    ///
    /// The connector object reports two real properties, `DPMS` and `EDID` (see
    /// [`Self::connector_set_property`]/[`Self::get_prop_blob`]'s own doc comments) -- an
    /// earlier revision of this device reported `count_props = 0` unconditionally here, a valid
    /// real-kernel-accurate answer for an object with a genuinely empty property list in the
    /// abstract, but wlroots' legacy (non-atomic) output-commit path (reached via labwc,
    /// `backend/drm/legacy.c`) unconditionally tries to read EDID and set DPMS as part of a
    /// normal commit and treats their absence as fatal (`Failed to parse EDID` /
    /// `Failed to set DPMS property: Invalid argument`, confirmed live), hanging indefinitely
    /// with zero frames ever presented rather than crashing outright. Real hardware DRM
    /// connectors always have at least these two properties on legacy KMS, so reporting them is
    /// the more real-kernel-accurate response, not a fabrication.
    ///
    /// The plane object DOES report one real property (`type` = `"Primary"`, see
    /// [`Self::get_property`]'s own doc comment): real legacy (non-atomic) universal-planes
    /// clients -- including weston's `drm-backend.so`, per `drm_output_find_special_plane` in
    /// `libweston/backend-drm/drm.c` -- discard any plane whose `type` property can't be
    /// resolved to `WDRM_PLANE_TYPE_PRIMARY`, so a plane reporting zero properties (this
    /// device's prior behavior) was silently invisible to that discovery path even though
    /// `GETPLANE`/`GETPLANERESOURCES` correctly enumerated it.
    pub(crate) fn obj_get_properties(
        &self,
        ptr: UserPtrMut<DrmModeObjGetProperties>,
    ) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        // `DRM_MODE_OBJECT_ANY` (`0`, real kernel `drm_mode.h` value) lets a caller query an
        // object's properties without knowing/caring which KMS object type it is -- wlroots'
        // `backend/drm/drm.c` (`check_drm_features()`/`scan_drm_connectors()`) issues a second
        // `OBJ_GETPROPERTIES` pass against the plane it already discovered via `GETPLANE` using
        // this wildcard type rather than repeating `DRM_MODE_OBJECT_PLANE`, confirmed live: it
        // silently treats the resulting empty property set as "primary plane not found" and
        // aborts backend creation with no specific log line (`backend/backend.c`'s generic
        // "Failed to create DRM backend"), which is why matching only the exact typed variant
        // above previously made backend creation fail silently even though `DRM_MODE_OBJECT_
        // PLANE`-typed queries for the same `obj_id` succeeded correctly.
        if req.obj_type == DRM_MODE_OBJECT_PLANE
            || (req.obj_type == 0 && req.obj_id == VIRTUAL_PLANE_ID)
        {
            if req.obj_id != VIRTUAL_PLANE_ID {
                return Err(Errno::ENOENT);
            }
            // Two-call size-probe pattern, same shape as `get_plane_resources`: this plane has
            // exactly one property, so a short-sized caller buffer can never actually truncate.
            if req.count_props > 0 && req.props_ptr != 0 && req.prop_values_ptr != 0 {
                UserPtrMut::<u32>::from_usize(req.props_ptr as usize)
                    .write_at_offset::<Platform>(0, VIRTUAL_PLANE_TYPE_PROP_ID)
                    .ok_or(Errno::EFAULT)?;
                UserPtrMut::<u64>::from_usize(req.prop_values_ptr as usize)
                    .write_at_offset::<Platform>(0, VIRTUAL_PLANE_TYPE_VALUE)
                    .ok_or(Errno::EFAULT)?;
            }
            req.count_props = 1;
            ptr.write_at_offset::<Platform>(0, req)
                .ok_or(Errno::EFAULT)?;
            return Ok(0);
        }
        if req.obj_type == DRM_MODE_OBJECT_CONNECTOR
            || (req.obj_type == 0 && req.obj_id == VIRTUAL_CONNECTOR_ID)
        {
            if req.obj_id != VIRTUAL_CONNECTOR_ID {
                return Err(Errno::ENOENT);
            }
            // Two-call size-probe pattern, same shape as the plane branch above: this connector
            // has exactly two properties (DPMS, EDID), so a short-sized caller buffer can never
            // actually truncate.
            if req.count_props > 0 && req.props_ptr != 0 && req.prop_values_ptr != 0 {
                let props = UserPtrMut::<u32>::from_usize(req.props_ptr as usize);
                let values = UserPtrMut::<u64>::from_usize(req.prop_values_ptr as usize);
                props
                    .write_at_offset::<Platform>(0, VIRTUAL_CONNECTOR_DPMS_PROP_ID)
                    .ok_or(Errno::EFAULT)?;
                values
                    .write_at_offset::<Platform>(0, VIRTUAL_CONNECTOR_DPMS_VALUE)
                    .ok_or(Errno::EFAULT)?;
                props
                    .write_at_offset::<Platform>(1, VIRTUAL_CONNECTOR_EDID_PROP_ID)
                    .ok_or(Errno::EFAULT)?;
                values
                    .write_at_offset::<Platform>(1, u64::from(VIRTUAL_CONNECTOR_EDID_BLOB_ID))
                    .ok_or(Errno::EFAULT)?;
            }
            req.count_props = 2;
            ptr.write_at_offset::<Platform>(0, req)
                .ok_or(Errno::EFAULT)?;
            return Ok(0);
        }
        req.count_props = 0;
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    /// `DRM_IOCTL_MODE_CONNECTOR_SETPROPERTY` -- the legacy (pre-atomic) per-connector
    /// property-set ioctl. wlroots' `backend/drm/legacy.c` uses this specifically to set DPMS
    /// (`DRM_MODE_DPMS_ON`/`_STANDBY`/`_SUSPEND`/`_OFF`) as part of every legacy output commit;
    /// with this ioctl entirely unhandled, it fell through to `EINVAL` and labwc hung
    /// indefinitely waiting on a commit that could never succeed (confirmed live: "Failed to set
    /// DPMS property: Invalid argument" followed by "Failed to commit frame", zero frames ever
    /// presented).
    ///
    /// Accepted and reported back as a no-op success, never enforced: this single-address-space
    /// shim has no real display-power hardware whose behavior a DPMS transition would need to
    /// change, so there is nothing for `_OFF`/`_STANDBY`/`_SUSPEND` to actually do -- the virtual
    /// display stays "on" regardless, exactly the same "accept but don't enforce, since there is
    /// no real corresponding hardware/threat distinction here" pattern this codebase already
    /// applies to `memfd` sealing (see `syscalls::file::do_fcntl`'s `FcntlArg::ADD_SEALS`/
    /// `GET_SEALS` handling). Only the DPMS property ID is accepted; any other `prop_id` gets a
    /// real `ENOENT` (unknown property), matching [`Self::get_property`]'s own precedent.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn connector_set_property(
        &self,
        ptr: UserPtr<DrmModeConnectorSetProperty>,
    ) -> Result<u32, Errno> {
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.connector_id != VIRTUAL_CONNECTOR_ID {
            return Err(Errno::ENOENT);
        }
        if req.prop_id != VIRTUAL_CONNECTOR_DPMS_PROP_ID {
            return Err(Errno::ENOENT);
        }
        Ok(0)
    }

    /// `DRM_IOCTL_MODE_OBJ_SETPROPERTY` -- the generic, object-type-carrying property-set ioctl.
    /// See [`DRM_IOCTL_MODE_OBJ_SETPROPERTY`]'s own doc comment: this is the ioctl `drm-rs`'s
    /// `Device::set_property` (used by `smithay`'s `backend_drm`, confirmed live via
    /// `docs/wayland-drm-backend-probe/`) actually sends, distinct from the legacy
    /// connector-only [`Self::connector_set_property`] above. Same accept-as-a-no-op semantics
    /// as that sibling function (no real DPMS hardware state exists for this single-
    /// address-space shim to change), narrowed to exactly the one real (object, property) pair
    /// this device advertises as settable today: the virtual connector's `DPMS` property. Any
    /// other object/property combination gets a real `ENOENT`, matching every other
    /// unknown-ID path in this file.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn obj_set_property(
        &self,
        ptr: UserPtr<DrmModeObjSetProperty>,
    ) -> Result<u32, Errno> {
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.obj_type != DRM_MODE_OBJECT_CONNECTOR || req.obj_id != VIRTUAL_CONNECTOR_ID {
            return Err(Errno::ENOENT);
        }
        if req.prop_id != VIRTUAL_CONNECTOR_DPMS_PROP_ID {
            return Err(Errno::ENOENT);
        }
        Ok(0)
    }

    /// `DRM_IOCTL_MODE_GETPROPBLOB` -- resolve a blob property's raw bytes. The only blob this
    /// device exposes is the virtual connector's synthesized EDID (see
    /// [`VIRTUAL_CONNECTOR_EDID_BLOB_ID`]), returned via the standard two-call size-probe
    /// pattern (a `length`-only probe call, followed by a caller-sized-buffer call).
    ///
    /// The 128 bytes below are a minimal, spec-valid EDID 1.3 block -- the fixed header magic
    /// (`00 FF FF FF FF FF FF 00`), a fabricated-but-well-formed manufacturer ID/product code/
    /// serial (real values, not zeroed, since some parsers reject an all-zero block outright),
    /// EDID version `1.3`, and a correct checksum (byte 127, chosen so all 128 bytes sum to `0
    /// mod 256`, the one hard requirement every EDID parser -- including wlroots'
    /// `backend/drm/util.c` -- actually validates). No real display-timing descriptor blocks are
    /// populated: this device already reports its one fixed mode ([`VIRTUAL_WIDTH`] x
    /// [`VIRTUAL_HEIGHT`] @ [`VIRTUAL_REFRESH_HZ`]) directly via `GETCONNECTOR`'s `modes` array,
    /// which is what real clients actually use to pick a mode -- the EDID's only job here is to
    /// exist and parse successfully, not to redundantly describe a mode a client would prefer
    /// over the one already offered.
    pub(crate) fn get_prop_blob(&self, ptr: UserPtrMut<DrmModeGetBlob>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.blob_id != VIRTUAL_CONNECTOR_EDID_BLOB_ID {
            return Err(Errno::ENOENT);
        }
        if req.length > 0 && req.data != 0 {
            let len = core::cmp::min(req.length as usize, VIRTUAL_EDID_BLOB.len());
            let dest = UserPtrMut::<u8>::from_usize(req.data as usize);
            for (i, byte) in VIRTUAL_EDID_BLOB[..len].iter().enumerate() {
                dest.write_at_offset::<Platform>(i as isize, *byte)
                    .ok_or(Errno::EFAULT)?;
            }
        }
        req.length = VIRTUAL_EDID_BLOB.len() as u32;
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    /// `DRM_IOCTL_MODE_GETPROPERTY` -- resolve a property ID's name/values.
    ///
    /// This device reports three real properties across [`Self::obj_get_properties`]'s object
    /// branches: the plane's `type` (id [`VIRTUAL_PLANE_TYPE_PROP_ID`]), and the connector's
    /// `DPMS` (id [`VIRTUAL_CONNECTOR_DPMS_PROP_ID`]) and `EDID` (id
    /// [`VIRTUAL_CONNECTOR_EDID_PROP_ID`]). wlroots' `backend/drm/properties.c` resolves each
    /// `OBJ_GETPROPERTIES`-reported ID through exactly this ioctl before deciding how to use it
    /// (confirmed live: "Failed to get property 101/102 of DRM object 1" was this device's
    /// exact prior failure mode when only the plane's `type` property was recognized here, even
    /// though `obj_get_properties` itself already reported the connector's DPMS/EDID IDs --
    /// `OBJ_GETPROPERTIES` and per-ID `GETPROPERTY` are two separate ioctls with two separate
    /// property tables in a real driver, and this device's table needs every ID it ever
    /// advertises, not just the ones a caller happens to exercise directly). Every other
    /// property ID this device could ever be asked about (there are none) gets a real `ENOENT`
    /// (unknown property) rather than an `ENOTTY` that would look like a missing driver.
    pub(crate) fn get_property(&self, ptr: UserPtrMut<DrmModeGetProperty>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.prop_id == VIRTUAL_CONNECTOR_DPMS_PROP_ID {
            const NAME: &[u8] = b"DPMS";
            req.name = [0u8; 32];
            req.name[..NAME.len()].copy_from_slice(NAME);
            req.flags = DRM_MODE_PROP_ENUM;
            // Real DPMS is a 4-valued enum (On/Standby/Suspend/Off); this device only ever
            // reports/accepts "On" (see [`Self::connector_set_property`]'s own doc comment for
            // why), so only that one enum entry is populated -- a real client resolving DPMS by
            // name (rather than assuming the value's meaning) still finds exactly the entry it
            // needs.
            if req.count_values > 0 && req.values_ptr != 0 {
                UserPtrMut::<u64>::from_usize(req.values_ptr as usize)
                    .write_at_offset::<Platform>(0, VIRTUAL_CONNECTOR_DPMS_VALUE)
                    .ok_or(Errno::EFAULT)?;
            }
            if req.count_enum_blobs > 0 && req.enum_blob_ptr != 0 {
                const ENUM_NAME: &[u8] = b"On";
                let mut name = [0u8; 32];
                name[..ENUM_NAME.len()].copy_from_slice(ENUM_NAME);
                UserPtrMut::<DrmModePropertyEnum>::from_usize(req.enum_blob_ptr as usize)
                    .write_at_offset::<Platform>(
                        0,
                        DrmModePropertyEnum {
                            value: VIRTUAL_CONNECTOR_DPMS_VALUE,
                            name,
                        },
                    )
                    .ok_or(Errno::EFAULT)?;
            }
            req.count_values = 1;
            req.count_enum_blobs = 1;
            ptr.write_at_offset::<Platform>(0, req)
                .ok_or(Errno::EFAULT)?;
            return Ok(0);
        }
        if req.prop_id == VIRTUAL_CONNECTOR_EDID_PROP_ID {
            const NAME: &[u8] = b"EDID";
            req.name = [0u8; 32];
            req.name[..NAME.len()].copy_from_slice(NAME);
            req.flags = DRM_MODE_PROP_BLOB;
            // A blob property's "value" is the blob ID itself, resolved separately via
            // `DRM_IOCTL_MODE_GETPROPBLOB` (see [`Self::get_prop_blob`]) -- there is no enum
            // table for a blob-typed property, matching real kernel semantics.
            if req.count_values > 0 && req.values_ptr != 0 {
                UserPtrMut::<u64>::from_usize(req.values_ptr as usize)
                    .write_at_offset::<Platform>(0, u64::from(VIRTUAL_CONNECTOR_EDID_BLOB_ID))
                    .ok_or(Errno::EFAULT)?;
            }
            req.count_values = 1;
            req.count_enum_blobs = 0;
            ptr.write_at_offset::<Platform>(0, req)
                .ok_or(Errno::EFAULT)?;
            return Ok(0);
        }
        if req.prop_id != VIRTUAL_PLANE_TYPE_PROP_ID {
            return Err(Errno::ENOENT);
        }
        const NAME: &[u8] = b"type";
        req.name = [0u8; 32];
        req.name[..NAME.len()].copy_from_slice(NAME);
        req.flags = DRM_MODE_PROP_ENUM;
        // Two-call size-probe pattern for `values_ptr`/`enum_blob_ptr`, same shape as every
        // other variable-length query this device implements: this property has exactly one
        // legacy value slot and one enum entry, so a short-sized caller buffer never truncates.
        if req.count_values > 0 && req.values_ptr != 0 {
            UserPtrMut::<u64>::from_usize(req.values_ptr as usize)
                .write_at_offset::<Platform>(0, VIRTUAL_PLANE_TYPE_VALUE)
                .ok_or(Errno::EFAULT)?;
        }
        if req.count_enum_blobs > 0 && req.enum_blob_ptr != 0 {
            const ENUM_NAME: &[u8] = b"Primary";
            let mut name = [0u8; 32];
            name[..ENUM_NAME.len()].copy_from_slice(ENUM_NAME);
            UserPtrMut::<DrmModePropertyEnum>::from_usize(req.enum_blob_ptr as usize)
                .write_at_offset::<Platform>(
                    0,
                    DrmModePropertyEnum {
                        value: VIRTUAL_PLANE_TYPE_VALUE,
                        name,
                    },
                )
                .ok_or(Errno::EFAULT)?;
        }
        req.count_values = 1;
        req.count_enum_blobs = 1;
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn create_dumb(
        &self,
        platform: &Platform,
        ptr: UserPtrMut<DrmModeCreateDumb>,
    ) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.width == 0 || req.height == 0 || req.bpp == 0 || req.flags != 0 {
            litebox_util_log::warn!(
                width:? = req.width, height:? = req.height, bpp:? = req.bpp, flags:? = req.flags;
                "drm-ioctl: CREATE_DUMB rejected"
            );
            return Err(Errno::EINVAL);
        }
        let bytes_per_pixel = req.bpp.div_ceil(8);
        let Some(pitch) = req.width.checked_mul(bytes_per_pixel) else {
            return Err(Errno::EINVAL);
        };
        let Some(size) = u64::from(pitch).checked_mul(u64::from(req.height)) else {
            return Err(Errno::EINVAL);
        };
        let Ok(size_usize) = usize::try_from(size) else {
            return Err(Errno::ENOMEM);
        };
        // Real shared-memory objects are only ever mapped/committed in whole pages; round the
        // requested buffer size up so the later `mmap()` bridge (see `sys_mmap`'s DRI-fd branch)
        // can map an exact whole-page range covering the buffer without any pages spilling past
        // the object's own real size.
        let page_aligned_size = size_usize.next_multiple_of(PAGE_SIZE);
        let shared_handle = platform
            .create_shared_memory(page_aligned_size)
            .map_err(|_| Errno::ENOMEM)?;
        let _guard = self.region.guard.lock();
        let handle = self
            .region
            .next_buffer_handle
            .fetch_add(1, Ordering::Relaxed);
        let owner = platform.current_host_pid();
        // Table full (after dead-owner reclaim): real DRM answers a GEM object it cannot allocate
        // with ENOMEM, and so does this. Never a panic.
        let Some(i) = self.claim_buffer(platform) else {
            drop(_guard);
            let _ = platform.close_shared_memory(shared_handle);
            litebox_util_log::error!(
                capacity:% = SHARED_DRM_BUFFERS, cross_process:% = self.is_shared();
                "drm-ioctl: CREATE_DUMB refused -- shared drm buffer table full"
            );
            return Err(Errno::ENOMEM);
        };
        let slot = &self.region.buffers[i];
        slot.gem_handle.store(handle, Ordering::Release);
        slot.width.store(req.width, Ordering::Release);
        slot.height.store(req.height, Ordering::Release);
        slot.bpp.store(req.bpp, Ordering::Release);
        slot.pitch.store(pitch, Ordering::Release);
        slot.size.store(size_usize, Ordering::Release);
        slot.set_handle(Some(shared_handle));
        slot.map_offset.store(0, Ordering::Release);
        slot.owner_host.store(owner, Ordering::Release);
        slot.state.store(DRM_SLOT_USED, Ordering::Release);
        drop(_guard);
        req.handle = handle;
        req.pitch = pitch;
        req.size = size;
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn map_dumb(&self, ptr: UserPtrMut<DrmModeMapDumb>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        let offset = self.map_offset_for(req.handle).ok_or(Errno::ENOENT)?;
        req.offset = offset;
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    /// `DRM_IOCTL_PRIME_HANDLE_TO_FD` support: validate `handle` names a live dumb buffer and
    /// return its fake `mmap` offset (allocating one via the same lazy path
    /// [`Self::map_dumb`] uses, if this buffer has never been `MAP_DUMB`'d before -- a real
    /// client always maps a dumb buffer before/around exporting it, but nothing in the UAPI
    /// actually requires that ordering, so this covers the export-first case too). The caller
    /// (`syscalls::file`'s ioctl dispatch) uses this offset to tag the freshly opened PRIME fd so
    /// [`crate::syscalls::mm::Task::try_dri_dumb_buffer_mmap`] resolves an `mmap()` of that new fd
    /// back onto this SAME buffer's real shared-memory handle -- see that function's own doc
    /// comment for the tag-then-resolve mechanism.
    pub(crate) fn prime_export_offset(&self, handle: u32) -> Result<u64, Errno> {
        self.map_offset_for(handle).ok_or(Errno::ENOENT)
    }

    pub(crate) fn destroy_dumb(
        &self,
        platform: &Platform,
        ptr: UserPtr<DrmModeDestroyDumb>,
    ) -> Result<u32, Errno> {
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        let _guard = self.region.guard.lock();
        let Some(i) = self.buffer_slot(req.handle) else {
            return Err(Errno::ENOENT);
        };
        let slot = &self.region.buffers[i];
        let taken = slot.get_handle();
        slot.state.store(DRM_SLOT_FREE, Ordering::Release);
        slot.set_handle(None);
        slot.map_offset.store(0, Ordering::Release);
        slot.owner_host.store(0, Ordering::Release);
        drop(_guard);
        let Some(buffer) = taken else {
            return Err(Errno::ENOENT);
        };
        // Best-effort: `close_shared_memory` is refcounted (see its own doc comment) and the
        // buffer's storage is real host memory that must eventually be released, but a real
        // Linux `DRM_IOCTL_MODE_DESTROY_DUMB` itself has no failure mode userspace can act on
        // either -- match that by not surfacing a platform-level close failure as an ioctl error.
        let _ = platform.close_shared_memory(buffer);
        // A destroyed buffer's framebuffers become dangling references in real Linux too (the
        // kernel does not auto-remove a framebuffer when its backing buffer is destroyed;
        // userspace is responsible for removing the framebuffer first via
        // `DRM_IOCTL_MODE_RMFB`, not implemented in this pass) -- leaving `framebuffers` as-is
        // matches that real-kernel behavior rather than silently diverging from it.
        Ok(0)
    }

    /// `DRM_IOCTL_MODE_ADDFB` (v1, legacy) -- attach a dumb buffer as a scanout framebuffer using
    /// the pre-multi-plane request shape (`bpp`/`depth` instead of a fourcc `pixel_format`).
    ///
    /// Real userspace still sends this: a live capture of `xf86-video-modesetting` driving this
    /// exact device showed it calling `DRM_IOCTL_MODE_ADDFB` (ioctl nr `0xAE`, 28-byte
    /// `struct drm_mode_fb_cmd`) directly, with NO preceding `DRM_IOCTL_MODE_ADDFB2` attempt at
    /// all for that mode-set -- `drmmode_do_addfb`'s real fallback path, not a dead legacy call.
    /// Before this handler existed, that ioctl fell through to the generic unmatched-ioctl case
    /// and returned `EINVAL` with no framebuffer created, which is exactly the `failed to add fb
    /// -22` / `(EE) modeset(0): failed to set mode: Invalid argument` failure this fixes.
    pub(crate) fn add_fb(
        &self,
        platform: &Platform,
        ptr: UserPtrMut<DrmModeFbCmd>,
    ) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        // This device's dumb buffers are always 32bpp/depth-24 `XRGB8888` (see `create_dumb`'s
        // own `bpp == 32` case and `DRM_FORMAT_XRGB8888`'s doc comment) -- the legacy ADDFB
        // request has no fourcc field, so bpp/depth are the only way a caller can name a
        // format, and rejecting any other combination here (rather than silently reinterpreting
        // the buffer's bytes under a format the caller didn't ask for) matches how a real kernel
        // driver that only implements one hardware format would answer this ioctl.
        if req.bpp != 32 || req.depth != 24 {
            return Err(Errno::EINVAL);
        }
        let handle = req.handle;
        {
            let _guard = self.region.guard.lock();
            if self.buffer_slot(handle).is_none() {
                return Err(Errno::ENOENT);
            }
        }
        litebox_util_log::warn!(
            width:? = req.width, height:? = req.height, pitch:? = req.pitch,
            bpp:? = req.bpp, depth:? = req.depth, handle:? = handle;
            "drm-ioctl: ADDFB (v1) request"
        );
        let _guard = self.region.guard.lock();
        let fb_id = self.region.next_fb_id.fetch_add(1, Ordering::Relaxed);
        let owner = platform.current_host_pid();
        let Some(i) = self.claim_fb(platform) else {
            litebox_util_log::error!(
                capacity:% = SHARED_DRM_FRAMEBUFFERS, cross_process:% = self.is_shared();
                "drm-ioctl: ADDFB (v1) refused -- shared drm framebuffer table full"
            );
            return Err(Errno::ENOMEM);
        };
        let slot = &self.region.framebuffers[i];
        slot.fb_id.store(fb_id, Ordering::Release);
        slot.width.store(req.width, Ordering::Release);
        slot.height.store(req.height, Ordering::Release);
        slot.pixel_format
            .store(DRM_FORMAT_XRGB8888, Ordering::Release);
        slot.handle.store(handle, Ordering::Release);
        slot.owner_host.store(owner, Ordering::Release);
        slot.state.store(DRM_SLOT_USED, Ordering::Release);
        drop(_guard);
        req.fb_id = fb_id;
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn add_fb2(
        &self,
        platform: &Platform,
        ptr: UserPtrMut<DrmModeFbCmd2>,
    ) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.handles[1] != 0 || req.handles[2] != 0 || req.handles[3] != 0 {
            // A dumb buffer is always single-plane; a multi-plane request names a real
            // planar/multi-buffer format this device does not support.
            return Err(Errno::EINVAL);
        }
        let handle = req.handles[0];
        {
            let _guard = self.region.guard.lock();
            if self.buffer_slot(handle).is_none() {
                return Err(Errno::ENOENT);
            }
        }
        litebox_util_log::warn!(
            width:? = req.width, height:? = req.height, pixel_format:? = req.pixel_format,
            handle:? = handle;
            "drm-ioctl: ADDFB2 request"
        );
        let _guard = self.region.guard.lock();
        let fb_id = self.region.next_fb_id.fetch_add(1, Ordering::Relaxed);
        let owner = platform.current_host_pid();
        let Some(i) = self.claim_fb(platform) else {
            litebox_util_log::error!(
                capacity:% = SHARED_DRM_FRAMEBUFFERS, cross_process:% = self.is_shared();
                "drm-ioctl: ADDFB2 refused -- shared drm framebuffer table full"
            );
            return Err(Errno::ENOMEM);
        };
        let slot = &self.region.framebuffers[i];
        slot.fb_id.store(fb_id, Ordering::Release);
        slot.width.store(req.width, Ordering::Release);
        slot.height.store(req.height, Ordering::Release);
        slot.pixel_format.store(req.pixel_format, Ordering::Release);
        slot.handle.store(handle, Ordering::Release);
        slot.owner_host.store(owner, Ordering::Release);
        slot.state.store(DRM_SLOT_USED, Ordering::Release);
        drop(_guard);
        req.fb_id = fb_id;
        ptr.write_at_offset::<Platform>(0, req)
            .ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    /// `DRM_IOCTL_MODE_DIRTYFB` -- the guest reports damaged regions of the framebuffer it is
    /// ALREADY scanning out, instead of flipping to a different one.
    ///
    /// # Why this exists
    ///
    /// Presentation here is driven solely by `PAGE_FLIP` (see this module's own header): every
    /// flip hands the active framebuffer's pixels to the host `wgpu` surface. But Xorg's
    /// `modesetting` driver only page-flips when it has a second buffer to flip to. With a single
    /// framebuffer and no compositor -- the configuration this runs in -- its steady state is to
    /// draw into its shadow buffer and flush the damage with THIS ioctl. Unhandled, the guest
    /// paints continuously and nothing ever reaches the display: a healthy X server, a client that
    /// connects and succeeds, and no frames after startup.
    ///
    /// # Why it is smaller than `page_flip`
    ///
    /// `page_flip` must retarget the CRTC (`crtc_fb.store(req.fb_id)`) because the
    /// scanout framebuffer is CHANGING. A damage flush does not change it -- `fb_id` is already
    /// the current scanout -- so this only has to notify the presenter. Deliberately does NOT
    /// touch `crtc_fb`: doing so on a stale or unrelated `fb_id` would silently repoint the
    /// scanout, which is exactly the "different fb being scanned out than the one being drawn
    /// into" failure `page_flip`'s own `diag-drm-flip` comment exists to distinguish.
    ///
    /// Clip rectangles (`clips_ptr`/`num_clips`) are ignored: the whole framebuffer is presented.
    /// A superset of the damaged region is always correct, merely not minimal.
    ///
    /// Set `LITEBOX_NO_DIRTYFB=1` to make this a no-op returning success, so one binary can be
    /// A/B'd with and without it (matching `LITEBOX_NO_PLACEMENT_FLOOR` /
    /// `LITEBOX_NO_MAPPING_GUARD_GAP`).
    pub(crate) fn dirty_fb(
        &self,
        platform: &Platform,
        local: &DrmFlipLocal<Platform>,
        ptr: UserPtr<DrmModeFbDirtyCmd>,
    ) -> Result<u32, Errno> {
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        // Always-on, `error!` level: every repro runs `LITEBOX_LOG=error`, and this line answers
        // "does Xorg actually issue DIRTYFB at all" -- the question that decides whether this
        // path matters. A diagnostic that is invisible at the level investigations actually run
        // at has repeatedly cost real time in this codebase.
        litebox_util_log::debug!(
            fb_id:% = req.fb_id, num_clips:% = req.num_clips;
            "DIAG_DIRTYFB"
        );
        {
            let _guard = self.region.guard.lock();
            if self.fb_slot(req.fb_id).is_none() {
                return Err(Errno::ENOENT);
            }
        }
        if !dirty_fb_enabled() {
            return Ok(0);
        }
        self.notify_flip_callback(platform, local, req.fb_id);
        Ok(0)
    }

    pub(crate) fn page_flip(
        &self,
        platform: &Platform,
        local: &DrmFlipLocal<Platform>,
        boot_time: &<Platform as litebox::platform::TimeProvider>::Instant,
        ptr: UserPtr<DrmModeCrtcPageFlip>,
    ) -> Result<u32, Errno> {
        litebox_util_log::debug!("drm-diag: page_flip called");
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.crtc_id != VIRTUAL_CRTC_ID {
            return Err(Errno::ENOENT);
        }
        {
            let _guard = self.region.guard.lock();
            if self.fb_slot(req.fb_id).is_none() {
                return Err(Errno::ENOENT);
            }
            self.region.crtc_fb.store(req.fb_id, Ordering::Release);
        }
        // Log WHICH framebuffer is being scanned out. A screen that goes black while
        // flips continue is either the same fb losing its contents (a mapping or
        // coherency problem) or a different fb being scanned out than the one being
        // drawn into (a surface-ownership problem) -- and only the fb id distinguishes
        // them.
        if drm_trace_enabled() {
            litebox_util_log::debug!(
                fb_id:% = req.fb_id, crtc_id:% = req.crtc_id;
                "diag-drm-flip"
            );
        }
        self.notify_flip_callback(platform, local, req.fb_id);
        // This device has no real vsync/vblank interrupt to wait for, so the flip is complete
        // (in the sense a client cares about -- the CRTC now scans out the new framebuffer) the
        // instant this ioctl returns; if the guest asked to be told, queue the completion event
        // immediately rather than modeling any real timing delay.
        if req.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            let sequence = self
                .region
                .next_vblank_sequence
                .fetch_add(1, Ordering::Relaxed);
            // Report the CURRENT guest-monotonic time (the same clock domain `CLOCK_MONOTONIC`
            // reads via `gettime_as_duration`, i.e. `platform.now().duration_since(boot_time)`),
            // not a fixed `0`/1970 epoch. Confirmed live via a full XFCE repro trace
            // (`.wfgy/xfce-build/stackfix_debug1.log`): weston computes its own next repaint
            // deadline FROM this timestamp (`vblank_time + refresh_interval`), and reported it as
            // wildly, consistently "abnormal: -11541 msec" -- i.e. ~11 SECONDS in the past --
            // exactly matching how far into its real monotonic uptime weston already was when a
            // fixed `tv_sec=0` was compared against a genuine `now()` far past that. This silently
            // starves weston's own frame-callback dispatch to clients (weston-desktop-shell's own
            // startup path waits for its first `wl_callback.done` before drawing anything), which
            // is the most likely explanation for the long-standing "desktop-shell never creates a
            // `wl_surface`" symptom this session's own wire-level Wayland decoding independently
            // found and left unresolved. This device still has no real vsync/vblank interrupt to
            // model any genuine timing delay against (the flip is complete the instant this ioctl
            // returns, same as before) -- only the REPORTED timestamp changes, not the timing
            // model itself.
            let elapsed = platform.now().duration_since(boot_time);
            #[allow(
                clippy::cast_possible_truncation,
                reason = "a guest uptime that overflows u32 seconds (~136 years) is not a real scenario"
            )]
            let tv_sec = elapsed.as_secs() as u32;
            let tv_usec = elapsed.subsec_micros();
            let event = DrmEventVblank {
                base: DrmEvent {
                    r#type: DRM_EVENT_FLIP_COMPLETE,
                    length: size_of::<DrmEventVblank>() as u32,
                },
                user_data: req.user_data,
                tv_sec,
                tv_usec,
                sequence,
                crtc_id: VIRTUAL_CRTC_ID,
            };
            {
                let _guard = self.region.guard.lock();
                self.flip_push(&event);
            }
            local.pollee.notify_observers(Events::IN);
        }
        Ok(0)
    }

    /// Look up a dumb buffer by the fake `mmap` offset a prior `MAP_DUMB` call handed out for
    /// it, for `sys_mmap`'s DRI-fd branch to resolve a guest's own `mmap(fd, ..., offset)` call
    /// against. Returns the buffer's real shared-memory handle and its exact byte size (NOT the
    /// page-rounded allocation size -- the caller rounds up itself, matching how `create_dumb`
    /// already rounds the underlying `create_shared_memory` request).
    pub(crate) fn lookup_by_map_offset(
        &self,
        offset: u64,
    ) -> Option<(Platform::SharedMemoryHandle, usize)> {
        let _guard = self.region.guard.lock();
        let i = self.buffer_slot_by_map_offset(offset)?;
        let slot = &self.region.buffers[i];
        Some((slot.get_handle()?, slot.size.load(Ordering::Acquire)))
    }

    /// `DRM_IOCTL_PRIME_FD_TO_HANDLE` support: given a real Linux dma-buf fd, real Linux resolves
    /// (or, for a genuinely foreign fd, imports) it to a driver-local GEM handle. This device has
    /// no real dma-buf subsystem (see [`Self::prime_export_offset`]'s own doc comment) and no
    /// foreign-fd import path -- the only fds ever presented back here are ones this SAME
    /// device's own [`Self::prime_export_offset`] most recently exported (confirmed live: wlroots'
    /// `drm_dumb.c` calls `PRIME_HANDLE_TO_FD` then immediately `PRIME_FD_TO_HANDLE` on the fd it
    /// just received, to obtain a GEM handle for the newly-imported buffer object, matching the
    /// real kernel's own self-import round-trip). Given the exported fd's own tagged `map_offset`
    /// (looked up by the caller via [`DrmPrimeFdMarker`] in `syscalls::file`, mirroring
    /// `try_dri_dumb_buffer_mmap`'s identical resolution), this returns the SAME original
    /// dumb-buffer handle the export started from -- correct for a self-import, since there is
    /// only ever one buffer object involved, not a fresh second one.
    pub(crate) fn lookup_handle_by_map_offset(&self, offset: u64) -> Option<u32> {
        let _guard = self.region.guard.lock();
        let i = self.buffer_slot_by_map_offset(offset)?;
        Some(self.region.buffers[i].gem_handle.load(Ordering::Acquire))
    }

    /// `DRM_IOCTL_GEM_CLOSE` -- release the CALLER's local reference to a GEM handle. On real
    /// Linux this decrements a per-open-file GEM handle refcount, only actually freeing the
    /// underlying object once every fd-local reference AND every driver-internal reference
    /// (framebuffer attachment, active scanout, ...) drops to zero. This device tracks exactly
    /// one refcount-free handle table (`SharedDrmRegion::buffers`, indexed by the same handle
    /// `CREATE_DUMB`/`PRIME_FD_TO_HANDLE` hand out) with a single, explicit teardown entry point
    /// (`DRM_IOCTL_MODE_DESTROY_DUMB`, see [`Self::destroy_dumb`]) -- real refcounting has nothing
    /// to model here since there is exactly one client and one reference per handle ever created.
    /// A real no-op success for any handle that currently exists is therefore accurate (the
    /// buffer stays alive exactly as long as it always would -- until `DESTROY_DUMB`), matching
    /// how real GEM_CLOSE on a still-multiply-referenced handle is *also* a no-op from the
    /// caller's observable perspective (the object doesn't disappear underneath a sibling
    /// reference either). An unknown handle still gets a real `ENOENT`, not a fabricated success.
    pub(crate) fn gem_close(&self, handle: u32) -> Result<u32, Errno> {
        let _guard = self.region.guard.lock();
        if self.buffer_slot(handle).is_some() {
            Ok(0)
        } else {
            Err(Errno::ENOENT)
        }
    }
}

impl<Platform: ShimPlatform> DrmFlipLocal<Platform> {
    pub(crate) fn new() -> Self {
        Self {
            callbacks: litebox::sync::Mutex::new(alloc::vec::Vec::new()),
            pollee: Pollee::new(),
        }
    }

    /// Append a host-side frame observer -- see [`Self::callbacks`]'s doc comment for when/why a
    /// runner calls this, and why it takes plain pixel bytes rather than the platform-specific
    /// shared-memory handle. Not part of [`crate::LinuxShimBuilder::build`] itself since
    /// `litebox_shim_linux` has no presentation layer of its own to default to; a runner that
    /// wants flipped frames actually displayed calls this once, right after the shim is built,
    /// with a closure that forwards the bytes to its own window/GPU-surface presentation code.
    /// Observers are ADDITIVE: calling this twice installs two, both invoked per flip in
    /// registration order. It does not replace a previously-installed one.
    pub fn add_flip_callback(
        &self,
        callback: impl Fn(&[u8], u32, u32, u32, u32) + Send + Sync + 'static,
    ) {
        self.callbacks
            .lock()
            .push(alloc::boxed::Box::new(callback) as DrmFlipCallback);
    }

    /// Register an observer for DRM fd readiness -- see [`Self::pollee`]'s doc comment. Called
    /// from `syscalls::epoll::EpollDescriptor::poll`'s `File` arm's `DriFd` branch, exactly where
    /// every other pollable fd kind (e.g. eventfd) registers its own observer.
    pub(crate) fn register_flip_observer(
        &self,
        observer: alloc::sync::Weak<dyn litebox::event::observer::Observer<Events>>,
    ) {
        self.pollee.register_observer(observer, Events::IN);
    }
}
