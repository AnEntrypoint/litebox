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
//!   `DRM_IOCTL_MODE_PAGE_FLIP` sets the CRTC scanout framebuffer and triggers the `flip_callback`.
//! - **wgpu Host Presentation Pipeline**: When `set_flip_callback` is registered (such as by
//!   `litebox_platform_windows_userland::presentation::Presenter`), every `DRM_IOCTL_MODE_PAGE_FLIP`
//!   transfers the active framebuffer's pixels to the host `wgpu` surface for real window presentation.
//! - **Page-flip Events**: `DRM_MODE_PAGE_FLIP_EVENT` pushes `DrmEventVblank` into the pending queue,
//!   which is read via `pop_flip_event_bytes()` on the DRM device file descriptor.

use alloc::collections::{BTreeMap, VecDeque};
use core::sync::atomic::{AtomicU32, Ordering};

use litebox::event::Events;
use litebox::event::polling::Pollee;
use litebox::mm::linux::PAGE_SIZE;
use litebox::platform::Instant as _;
use litebox::platform::RawConstPointer;
use litebox_common_linux::{
    DRM_AUTH_MAGIC_VALUE, DRM_CAP_CRTC_IN_VBLANK_EVENT, DRM_CAP_DUMB_BUFFER, DRM_CAP_PRIME,
    DRM_CAP_TIMESTAMP_MONOTONIC, DRM_CLIENT_CAP_UNIVERSAL_PLANES, DRM_EVENT_FLIP_COMPLETE,
    DRM_MODE_CONNECTOR_VIRTUAL, DRM_MODE_ENCODER_VIRTUAL, DRM_MODE_OBJECT_CONNECTOR,
    DRM_MODE_OBJECT_PLANE, DRM_MODE_PAGE_FLIP_EVENT, DRM_MODE_PROP_ENUM, DRM_PRIME_CAP_EXPORT,
    DRM_PRIME_CAP_IMPORT, DrmAuth, DrmEvent, DrmEventVblank, DrmGetCap, DrmModeCardRes,
    DrmModeCreateDumb, DrmModeCrtc, DrmModeCrtcPageFlip, DrmModeDestroyDumb, DrmModeFbCmd2,
    DrmModeGetConnector, DrmModeGetEncoder, DrmModeGetPlane, DrmModeGetPlaneRes,
    DrmModeGetProperty, DrmModeMapDumb, DrmModeModeinfo, DrmModeObjGetProperties,
    DrmModePropertyEnum, DrmModeSetPlane, DrmSetClientCap, DrmVersion, VIRTUAL_PLANE_TYPE_PROP_ID,
    VIRTUAL_PLANE_TYPE_VALUE, errno::Errno,
};
use zerocopy::IntoBytes;

use crate::{ShimPlatform, UserPtr, UserPtrMut};

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
/// Added alongside [`DrmSubsystem::get_plane_resources`]/[`DrmSubsystem::get_plane`]/
/// [`DrmSubsystem::set_plane`]; some real KMS-using toolkits query the plane API even for a
/// single-plane use case (checking plane capabilities before deciding how to render).
const VIRTUAL_PLANE_ID: u32 = 4;

/// `DRM_FORMAT_XRGB8888` -- the fourcc-code encoding (`'X' | 'R'<<8 | '2'<<16 | '4'<<24`, per
/// `drm_fourcc.h`'s `fourcc_code` macro) for the one pixel format this device's dumb buffers
/// support (see `create_dumb`'s `bpp == 32` case elsewhere in this file). Not independently
/// fetched from `drm_fourcc.h` this pass (see `docs/drm-dumb-buffer-ioctl-reference.md`'s "gaps"
/// section); this is the standard, well-known fourcc encoding for that format.
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

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

/// A single allocated dumb buffer's state.
///
/// Backed by a real platform shared-memory object (the same primitive `MAP_ANONYMOUS|MAP_SHARED`
/// mmaps use, see [`litebox::platform::page_mgmt::PageManagementProvider::create_shared_memory`]),
/// created eagerly at `CREATE_DUMB` time -- not a plain `Vec<u8>`. This is what makes the buffer's
/// pixel content reachable from THREE independent places that must all observe the same bytes:
/// the guest's own later `mmap()` of the `MAP_DUMB` offset (see `sys_mmap`'s DRI-fd branch), and
/// (in a later pass) the host-side wgpu presentation code reading the flipped framebuffer's
/// content directly, with no explicit copy between guest writes and host reads.
struct DumbBuffer<Platform: ShimPlatform> {
    // Stored for the buffer's own record-keeping (e.g. a future `DESTROY_DUMB`/`MAP_DUMB` bounds
    // or format-consistency check) even though nothing reads them back yet -- `pitch`/`size` are
    // the derived values everything downstream actually uses.
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
    #[allow(dead_code)]
    bpp: u32,
    pitch: u32,
    size: usize,
    handle: Platform::SharedMemoryHandle,
    /// The fake `mmap` offset handed out by `DRM_IOCTL_MODE_MAP_DUMB`, if this buffer has been
    /// mapped at least once. Real DRM hands out a fresh, unique fake offset per `MAP_DUMB` call
    /// on the same handle; this device reuses the first one issued, which every real client
    /// tolerates (they don't rely on the offset changing across repeated `MAP_DUMB` calls).
    map_offset: Option<u64>,
}

/// A framebuffer object: an attached (buffer handle, format, geometry) tuple, referenced by
/// `fb_id` from `DRM_IOCTL_MODE_SETCRTC`/`PAGE_FLIP`.
struct Framebuffer {
    width: u32,
    height: u32,
    pixel_format: u32,
    /// The dumb-buffer handle backing plane 0 (the only plane a dumb buffer ever populates).
    handle: u32,
}

/// State for the one virtual DRM/KMS device this shim exposes. See the module doc comment for
/// what is and is not implemented in this pass.
pub(crate) struct DrmSubsystem<Platform: ShimPlatform> {
    next_buffer_handle: AtomicU32,
    next_fb_id: AtomicU32,
    next_map_offset: AtomicU32,
    buffers: litebox::sync::Mutex<Platform, BTreeMap<u32, DumbBuffer<Platform>>>,
    framebuffers: litebox::sync::Mutex<Platform, BTreeMap<u32, Framebuffer>>,
    /// The framebuffer currently attached to the virtual CRTC (via `SETCRTC` or `PAGE_FLIP`),
    /// `None` until the guest sets one.
    crtc_fb: litebox::sync::Mutex<Platform, Option<u32>>,
    /// The framebuffer currently attached to [`VIRTUAL_PLANE_ID`] via `SETPLANE`, `None` until
    /// the guest sets one. Deliberately independent of `crtc_fb` (a real primary plane's `fb_id`
    /// and its CRTC's own `fb_id` are two separate pieces of driver state that a real client can
    /// observe diverge, e.g. right after a `SETPLANE` before any `PAGE_FLIP`/`SETCRTC` call) --
    /// this device does not attempt to keep them synchronized, matching real KMS semantics rather
    /// than inventing a coupling the UAPI doesn't promise.
    plane_fb: litebox::sync::Mutex<Platform, Option<u32>>,
    /// Completed-but-not-yet-`read()` page-flip events, in completion order -- popped one at a
    /// time by `read()` on the DRM device fd (see `litebox_shim_linux::syscalls::file::do_read`'s
    /// DRI-fd branch). Only ever grows from [`Self::page_flip`] when the guest requested
    /// `DRM_MODE_PAGE_FLIP_EVENT`; this device completes every flip immediately (no real vsync
    /// timing -- see this module's doc comment), so an event is always ready by the time a client
    /// gets around to reading for it.
    pending_flip_events: litebox::sync::Mutex<Platform, VecDeque<DrmEventVblank>>,
    /// Wakeup mechanism for a guest thread blocked in `poll`/`epoll_wait`/`select` on the DRM
    /// device fd waiting for a page-flip completion event. `page_flip` notifies this whenever it
    /// pushes into [`Self::pending_flip_events`]; `syscalls::epoll::EpollDescriptor::poll`'s `File`
    /// arm registers an observer here (mirroring every other pollable fd kind, e.g. eventfd) so a
    /// compositor's own event loop -- which registers the DRM fd once via `epoll_ctl` and then
    /// blocks in `epoll_wait` across many frames, rather than re-polling synchronously after every
    /// flip -- actually wakes for the second and subsequent flips. Without this, the fd's readiness
    /// was only ever computed on-demand (see [`Self::has_pending_flip_events`]'s doc comment for
    /// the confirmed-live symptom this produced: exactly one `SETCRTC`+`PAGE_FLIP` pair at startup,
    /// then no repaint ever again for the rest of the run).
    flip_pollee: Pollee<Platform>,
    /// Monotonically increasing vblank sequence number, echoed into each flip-completion event's
    /// `sequence` field -- real clients that track it purely to detect drops/reordering see a
    /// plain incrementing counter, matching real Linux's own semantics closely enough for that
    /// use even though this device has no real vblank interrupt driving it.
    next_vblank_sequence: AtomicU32,
    /// Whether this device fd currently holds "DRM master" status (`DRM_IOCTL_SET_MASTER`
    /// granted, not yet released by `DRM_IOCTL_DROP_MASTER`). This virtual device has exactly one
    /// possible client and no real multi-master contention to arbitrate (see [`Self::set_master`]/
    /// [`Self::drop_master`]'s own doc comments), so this is bookkeeping only -- nothing currently
    /// consults it to gate another ioctl, matching how mode-setting here already succeeds
    /// regardless of master status.
    is_master: core::sync::atomic::AtomicBool,
    /// Host-side hook, invoked synchronously at the end of every successful [`Self::page_flip`]
    /// with the now-scanned-out framebuffer's own pixel bytes (a plain, already-copied-out `&[u8]`
    /// -- not the platform-specific shared-memory handle, deliberately: a `Box<dyn Fn(...)>`
    /// capturing `Platform::SharedMemoryHandle` (an associated type projected off
    /// `PageManagementProvider<{PAGE_SIZE}>`) hits a real rustc limitation resolving that
    /// associated type's well-formedness behind a trait object even though `ShimPlatform` already
    /// implies the bound for every concrete use of this struct -- confirmed live, `cargo build`
    /// error `E0277` naming the bound as unsatisfied even on this `impl` block's own untouched
    /// existing methods. Staying byte-based sidesteps that entirely and is also the more honest
    /// interface: a presentation layer only ever needed the pixels, never the handle) plus
    /// `(width, height, pitch, pixel_format)`. `litebox_shim_linux` is platform-agnostic and
    /// cannot depend on a concrete presentation layer (e.g.
    /// `litebox_platform_windows_userland`'s wgpu-backed `Presenter`), so this stays a generic
    /// callback set post-construction (see [`Self::set_flip_callback`]) by whichever runner
    /// binary DOES depend on both crates and wants to actually display flipped frames -- a runner
    /// that never calls the setter (or a non-GUI runner target) simply never invokes it, at zero
    /// cost beyond one extra `Option` check per flip.
    flip_callback: litebox::sync::Mutex<
        Platform,
        Option<alloc::boxed::Box<dyn Fn(&[u8], u32, u32, u32, u32) + Send + Sync>>,
    >,
}

impl<Platform: ShimPlatform> DrmSubsystem<Platform> {
    pub(crate) fn new() -> Self {
        Self {
            // Handles/IDs start at 1: real DRM never hands out handle/id 0 for an actual object
            // (0 is reserved to mean "none"/"invalid" in these ioctls, e.g. `drm_mode_crtc.fb_id
            // == 0` means "no framebuffer attached").
            next_buffer_handle: AtomicU32::new(1),
            next_fb_id: AtomicU32::new(1),
            next_map_offset: AtomicU32::new(1),
            buffers: litebox::sync::Mutex::new(BTreeMap::new()),
            framebuffers: litebox::sync::Mutex::new(BTreeMap::new()),
            crtc_fb: litebox::sync::Mutex::new(None),
            plane_fb: litebox::sync::Mutex::new(None),
            pending_flip_events: litebox::sync::Mutex::new(VecDeque::new()),
            flip_pollee: Pollee::new(),
            next_vblank_sequence: AtomicU32::new(0),
            is_master: core::sync::atomic::AtomicBool::new(false),
            flip_callback: litebox::sync::Mutex::new(None),
        }
    }

    /// Install (or replace) the host-side flip callback -- see [`Self::flip_callback`]'s doc
    /// comment for when/why a runner calls this, and why it takes plain pixel bytes rather than
    /// the platform-specific shared-memory handle. Not part of [`Self::new`] itself since
    /// `litebox_shim_linux` has no presentation layer of its own to default to; a runner that
    /// wants flipped frames actually displayed calls this once, right after
    /// [`crate::LinuxShimBuilder::build`], with a closure that forwards the bytes to its own
    /// window/GPU-surface presentation code.
    pub fn set_flip_callback(
        &self,
        callback: impl Fn(&[u8], u32, u32, u32, u32) + Send + Sync + 'static,
    ) {
        *self.flip_callback.lock() = Some(alloc::boxed::Box::new(callback));
    }

    /// Pop the oldest pending flip-completion event, if any, encoded as the exact bytes a real
    /// `read()` on a DRM device fd would return (a [`DrmEvent`] header immediately followed by
    /// its [`DrmEventVblank`] body -- real DRM's `read()` contract, one or more whole events per
    /// call, never a partial one). `None` means no event is pending -- the caller (see
    /// `syscalls::file::do_read`'s DRI-fd branch) is responsible for real Linux's actual
    /// `read()`-with-nothing-pending behavior (blocks, or `EAGAIN` if the fd is non-blocking).
    pub(crate) fn pop_flip_event_bytes(&self) -> Option<alloc::vec::Vec<u8>> {
        let event = self.pending_flip_events.lock().pop_front()?;
        let mut bytes = alloc::vec::Vec::with_capacity(size_of::<DrmEventVblank>());
        bytes.extend_from_slice(event.as_bytes());
        Some(bytes)
    }

    /// Whether a flip-completion event is currently queued for a DRM device fd's `read()` to pop
    /// -- the readiness half of the same fact [`Self::pop_flip_event_bytes`] consumes. Exists so
    /// `syscalls::epoll::EpollDescriptor::poll`'s `File` arm can report a DRM fd `Events::IN`
    /// exactly when this device genuinely has something to deliver, mirroring
    /// `EvdevSubsystem::has_pending`'s identical role for `/dev/input/event0`. Without this, a
    /// real compositor's own event loop -- which registers the DRM fd with `epoll`/`poll` and
    /// waits for it to become readable BEFORE calling `read()`, rather than reading unconditionally
    /// right after issuing a flip -- never observes a flip-complete notification for any repaint
    /// after its first, since nothing ever marks the fd ready: confirmed live (litebox-xfce-1,
    /// sub-session 28) against a real `weston --backend=drm-backend.so` run, which issued exactly
    /// one `SETCRTC`+`PAGE_FLIP` pair at startup and never repainted again for the rest of a 60+
    /// second run with three live Wayland/X11 client applications attached the entire time.
    pub(crate) fn has_pending_flip_events(&self) -> bool {
        !self.pending_flip_events.lock().is_empty()
    }

    /// Register an observer for DRM fd readiness -- see [`Self::flip_pollee`]'s doc comment.
    /// Called from `syscalls::epoll::EpollDescriptor::poll`'s `File` arm's `DriFd` branch, exactly
    /// where every other pollable fd kind (e.g. eventfd) registers its own observer.
    pub(crate) fn register_flip_observer(
        &self,
        observer: alloc::sync::Weak<dyn litebox::event::observer::Observer<Events>>,
    ) {
        self.flip_pollee.register_observer(observer, Events::IN);
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
        let fbs = self.framebuffers.lock();
        let fb_count = u32::try_from(fbs.len()).unwrap_or(u32::MAX);
        if req.count_fbs > 0 && req.fb_id_ptr != 0 {
            let out = UserPtrMut::<u32>::from_usize(req.fb_id_ptr as usize);
            for (i, (fb_id, _)) in fbs.iter().enumerate() {
                if i as u32 >= req.count_fbs {
                    break;
                }
                out.write_at_offset::<Platform>(
                    isize::try_from(i).map_err(|_| Errno::EINVAL)?,
                    *fb_id,
                )
                .ok_or(Errno::EFAULT)?;
            }
        }
        drop(fbs);

        req.count_connectors = 1;
        req.count_encoders = 1;
        req.count_crtcs = 1;
        req.count_fbs = fb_count;
        req.min_width = VIRTUAL_WIDTH;
        req.max_width = VIRTUAL_WIDTH;
        req.min_height = VIRTUAL_HEIGHT;
        req.max_height = VIRTUAL_HEIGHT;
        ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn get_connector(
        &self,
        ptr: UserPtrMut<DrmModeGetConnector>,
    ) -> Result<u32, Errno> {
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
        ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn get_encoder(&self, ptr: UserPtrMut<DrmModeGetEncoder>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.encoder_id != 0 && req.encoder_id != VIRTUAL_ENCODER_ID {
            return Err(Errno::ENOENT);
        }
        req.encoder_id = VIRTUAL_ENCODER_ID;
        req.encoder_type = DRM_MODE_ENCODER_VIRTUAL;
        req.crtc_id = VIRTUAL_CRTC_ID;
        // Bit 0 set = "can drive CRTC index 0", the only CRTC this device has.
        req.possible_crtcs = 0b1;
        req.possible_clones = 0;
        ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn get_crtc(&self, ptr: UserPtrMut<DrmModeCrtc>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.crtc_id != 0 && req.crtc_id != VIRTUAL_CRTC_ID {
            return Err(Errno::ENOENT);
        }
        let fb_id = self.crtc_fb.lock().unwrap_or(0);
        req.crtc_id = VIRTUAL_CRTC_ID;
        req.fb_id = fb_id;
        req.x = 0;
        req.y = 0;
        req.gamma_size = 0;
        if fb_id != 0 {
            req.mode_valid = 1;
            req.mode = virtual_mode();
        } else {
            req.mode_valid = 0;
        }
        ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn set_crtc(
        &self,
        platform: &Platform,
        ptr: UserPtr<DrmModeCrtc>,
    ) -> Result<u32, Errno> {
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.crtc_id != VIRTUAL_CRTC_ID {
            return Err(Errno::ENOENT);
        }
        if req.fb_id != 0 && !self.framebuffers.lock().contains_key(&req.fb_id) {
            return Err(Errno::ENOENT);
        }
        *self.crtc_fb.lock() = if req.fb_id == 0 { None } else { Some(req.fb_id) };
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
        if let Some(fb_id) = *self.crtc_fb.lock() {
            self.notify_flip_callback(platform, fb_id);
        }
        Ok(0)
    }

    /// Shared by [`Self::set_crtc`] and [`Self::page_flip`]: maps `fb_id`'s backing dumb-buffer
    /// memory host-side and forwards it to the installed presentation callback, if any. A no-op
    /// (and a silently-swallowed lookup failure) when no callback is installed or the mapping
    /// fails, matching `page_flip`'s own established "a host-side presentation miss must never
    /// fail the guest's own ioctl" contract.
    fn notify_flip_callback(&self, platform: &Platform, fb_id: u32) {
        if self.flip_callback.lock().is_none() {
            return;
        }
        let Some((handle, size, width, height, pitch, pixel_format)) = ({
            let framebuffers = self.framebuffers.lock();
            framebuffers.get(&fb_id).map(|fb| {
                let buffers = self.buffers.lock();
                let buffer = buffers
                    .get(&fb.handle)
                    .expect("add_fb2 only ever records a handle that exists in self.buffers, and destroy_dumb never removes an fb referencing a destroyed buffer (see destroy_dumb's own doc comment: real Linux leaves dangling fb references, matched deliberately)");
                (buffer.handle, buffer.size, fb.width, fb.height, buffer.pitch, fb.pixel_format)
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
                if let Some(callback) = self.flip_callback.lock().as_ref() {
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
        ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
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
        let plane_fb = *self.plane_fb.lock();
        req.plane_id = VIRTUAL_PLANE_ID;
        req.crtc_id = plane_fb.map_or(0, |_| VIRTUAL_CRTC_ID);
        req.fb_id = plane_fb.unwrap_or(0);
        // Bit 0 set = "can be attached to CRTC index 0", the only CRTC this device has.
        req.possible_crtcs = 0b1;
        req.gamma_size = 0;
        req.count_format_types = 1;
        ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
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
        if req.fb_id == 0 {
            *self.plane_fb.lock() = None;
            return Ok(0);
        }
        if req.crtc_id != VIRTUAL_CRTC_ID {
            return Err(Errno::ENOENT);
        }
        if !self.framebuffers.lock().contains_key(&req.fb_id) {
            return Err(Errno::ENOENT);
        }
        *self.plane_fb.lock() = Some(req.fb_id);
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
        ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    /// `DRM_IOCTL_GET_CAP` -- query a single capability. This device genuinely supports dumb
    /// buffers (the only allocation path it has, see `create_dumb`), so `DRM_CAP_DUMB_BUFFER`
    /// reports `1`. It also reports `DRM_CAP_TIMESTAMP_MONOTONIC` and
    /// `DRM_CAP_CRTC_IN_VBLANK_EVENT` (see those constants' own doc comments) -- real
    /// compositors including weston's and wlroots' DRM backends require these capabilities to
    /// be present just to initialize at all, and `DRM_CAP_PRIME` (see its own doc comment,
    /// handled separately below since its value is a bitmask, not a boolean). Any other
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
        ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
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
        self.is_master
            .store(true, core::sync::atomic::Ordering::Relaxed);
        Ok(0)
    }

    /// `DRM_IOCTL_DROP_MASTER`. See [`Self::set_master`]'s doc comment.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn drop_master(&self) -> Result<u32, Errno> {
        self.is_master
            .store(false, core::sync::atomic::Ordering::Relaxed);
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
    /// reaching any further DRM work. This device has no dynamic KMS properties for a connector
    /// (no DPMS, no EDID blob, nothing a hardware driver would register) -- `count_props = 0` is
    /// the real kernel's own well-defined answer for an object with a genuinely empty property
    /// list, not a truncation.
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
        if req.obj_type == DRM_MODE_OBJECT_CONNECTOR && req.obj_id != VIRTUAL_CONNECTOR_ID {
            return Err(Errno::ENOENT);
        }
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
            ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
            return Ok(0);
        }
        req.count_props = 0;
        ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    /// `DRM_IOCTL_MODE_GETPROPERTY` -- resolve a property ID's name/values.
    ///
    /// This device's plane object reports one real property via [`Self::obj_get_properties`]:
    /// `type` (id [`VIRTUAL_PLANE_TYPE_PROP_ID`]), an enum property whose one real,
    /// on-the-wire value ([`VIRTUAL_PLANE_TYPE_VALUE`]) resolves to the `"Primary"` enum
    /// name -- matching real weston's `plane_type_enums[WDRM_PLANE_TYPE_PRIMARY].name` in
    /// `libweston/backend-drm/kms.c`, which is the exact string real clients compare against
    /// (`drm_property_info_populate`'s `strcmp(prop->enums[l].name, info[j].enum_values[k].name)`
    /// loop), not the raw numeric value. Every other property ID this device could ever be
    /// asked about (there are none, since [`Self::obj_get_properties`] never reports any other
    /// ID) gets a real `ENOENT` (unknown property) rather than an `ENOTTY` that would look like
    /// a missing driver.
    pub(crate) fn get_property(&self, ptr: UserPtrMut<DrmModeGetProperty>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
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
        ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn create_dumb(
        &self,
        platform: &Platform,
        ptr: UserPtrMut<DrmModeCreateDumb>,
    ) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.width == 0 || req.height == 0 || req.bpp == 0 || req.flags != 0 {
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
        let handle = self.next_buffer_handle.fetch_add(1, Ordering::Relaxed);
        self.buffers.lock().insert(
            handle,
            DumbBuffer {
                width: req.width,
                height: req.height,
                bpp: req.bpp,
                pitch,
                size: size_usize,
                handle: shared_handle,
                map_offset: None,
            },
        );
        req.handle = handle;
        req.pitch = pitch;
        req.size = size;
        ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn map_dumb(&self, ptr: UserPtrMut<DrmModeMapDumb>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        let mut buffers = self.buffers.lock();
        let buffer = buffers.get_mut(&req.handle).ok_or(Errno::ENOENT)?;
        let offset = *buffer.map_offset.get_or_insert_with(|| {
            // Real DRM fake offsets are page-aligned, opaque `mmap()` targets in a reserved
            // range distinct from any real memory address. `<< 12` (page-align) keeps this
            // device's offsets shaped the same way without claiming to match the kernel's exact
            // internal allocation scheme (which is driver-private and not part of the UAPI
            // contract clients rely on).
            u64::from(self.next_map_offset.fetch_add(1, Ordering::Relaxed)) << 12
        });
        req.offset = offset;
        ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    /// `DRM_IOCTL_PRIME_HANDLE_TO_FD` support: validate `handle` names a live dumb buffer and
    /// return its fake `mmap` offset (allocating one via the same lazy `get_or_insert_with` path
    /// [`Self::map_dumb`] uses, if this buffer has never been `MAP_DUMB`'d before -- a real
    /// client always maps a dumb buffer before/around exporting it, but nothing in the UAPI
    /// actually requires that ordering, so this covers the export-first case too). The caller
    /// (`syscalls::file`'s ioctl dispatch) uses this offset to tag the freshly opened PRIME fd so
    /// [`crate::syscalls::mm::Task::try_dri_dumb_buffer_mmap`] resolves an `mmap()` of that new fd
    /// back onto this SAME buffer's real shared-memory handle -- see that function's own doc
    /// comment for the tag-then-resolve mechanism.
    pub(crate) fn prime_export_offset(&self, handle: u32) -> Result<u64, Errno> {
        let mut buffers = self.buffers.lock();
        let buffer = buffers.get_mut(&handle).ok_or(Errno::ENOENT)?;
        let offset = *buffer.map_offset.get_or_insert_with(|| {
            u64::from(self.next_map_offset.fetch_add(1, Ordering::Relaxed)) << 12
        });
        Ok(offset)
    }

    pub(crate) fn destroy_dumb(
        &self,
        platform: &Platform,
        ptr: UserPtr<DrmModeDestroyDumb>,
    ) -> Result<u32, Errno> {
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        let mut buffers = self.buffers.lock();
        let Some(buffer) = buffers.remove(&req.handle) else {
            return Err(Errno::ENOENT);
        };
        drop(buffers);
        // Best-effort: `close_shared_memory` is refcounted (see its own doc comment) and the
        // buffer's storage is real host memory that must eventually be released, but a real
        // Linux `DRM_IOCTL_MODE_DESTROY_DUMB` itself has no failure mode userspace can act on
        // either -- match that by not surfacing a platform-level close failure as an ioctl error.
        let _ = platform.close_shared_memory(buffer.handle);
        // A destroyed buffer's framebuffers become dangling references in real Linux too (the
        // kernel does not auto-remove a framebuffer when its backing buffer is destroyed;
        // userspace is responsible for removing the framebuffer first via
        // `DRM_IOCTL_MODE_RMFB`, not implemented in this pass) -- leaving `framebuffers` as-is
        // matches that real-kernel behavior rather than silently diverging from it.
        Ok(0)
    }

    pub(crate) fn add_fb2(&self, ptr: UserPtrMut<DrmModeFbCmd2>) -> Result<u32, Errno> {
        let mut req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.handles[1] != 0 || req.handles[2] != 0 || req.handles[3] != 0 {
            // A dumb buffer is always single-plane; a multi-plane request names a real
            // planar/multi-buffer format this device does not support.
            return Err(Errno::EINVAL);
        }
        let handle = req.handles[0];
        if !self.buffers.lock().contains_key(&handle) {
            return Err(Errno::ENOENT);
        }
        let fb_id = self.next_fb_id.fetch_add(1, Ordering::Relaxed);
        self.framebuffers.lock().insert(
            fb_id,
            Framebuffer {
                width: req.width,
                height: req.height,
                pixel_format: req.pixel_format,
                handle,
            },
        );
        req.fb_id = fb_id;
        ptr.write_at_offset::<Platform>(0, req).ok_or(Errno::EFAULT)?;
        Ok(0)
    }

    pub(crate) fn page_flip(
        &self,
        platform: &Platform,
        boot_time: &<Platform as litebox::platform::TimeProvider>::Instant,
        ptr: UserPtr<DrmModeCrtcPageFlip>,
    ) -> Result<u32, Errno> {
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.crtc_id != VIRTUAL_CRTC_ID {
            return Err(Errno::ENOENT);
        }
        if !self.framebuffers.lock().contains_key(&req.fb_id) {
            return Err(Errno::ENOENT);
        }
        *self.crtc_fb.lock() = Some(req.fb_id);
        self.notify_flip_callback(platform, req.fb_id);
        // This device has no real vsync/vblank interrupt to wait for, so the flip is complete
        // (in the sense a client cares about -- the CRTC now scans out the new framebuffer) the
        // instant this ioctl returns; if the guest asked to be told, queue the completion event
        // immediately rather than modeling any real timing delay.
        if req.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            let sequence = self.next_vblank_sequence.fetch_add(1, Ordering::Relaxed);
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
            self.pending_flip_events.lock().push_back(DrmEventVblank {
                base: DrmEvent {
                    r#type: DRM_EVENT_FLIP_COMPLETE,
                    length: size_of::<DrmEventVblank>() as u32,
                },
                user_data: req.user_data,
                tv_sec,
                tv_usec,
                sequence,
                crtc_id: VIRTUAL_CRTC_ID,
            });
            self.flip_pollee.notify_observers(Events::IN);
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
        self.buffers
            .lock()
            .values()
            .find(|b| b.map_offset == Some(offset))
            .map(|b| (b.handle, b.size))
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
        self.buffers
            .lock()
            .iter()
            .find(|(_, b)| b.map_offset == Some(offset))
            .map(|(handle, _)| *handle)
    }

    /// `DRM_IOCTL_GEM_CLOSE` -- release the CALLER's local reference to a GEM handle. On real
    /// Linux this decrements a per-open-file GEM handle refcount, only actually freeing the
    /// underlying object once every fd-local reference AND every driver-internal reference
    /// (framebuffer attachment, active scanout, ...) drops to zero. This device tracks exactly
    /// one refcount-free handle table (`self.buffers`, indexed by the same handle
    /// `CREATE_DUMB`/`PRIME_FD_TO_HANDLE` hand out) with a single, explicit teardown entry point
    /// (`DRM_IOCTL_MODE_DESTROY_DUMB`, see [`Self::destroy_dumb`]) -- real refcounting has nothing
    /// to model here since there is exactly one client and one reference per handle ever created.
    /// A real no-op success for any handle that currently exists is therefore accurate (the
    /// buffer stays alive exactly as long as it always would -- until `DESTROY_DUMB`), matching
    /// how real GEM_CLOSE on a still-multiply-referenced handle is *also* a no-op from the
    /// caller's observable perspective (the object doesn't disappear underneath a sibling
    /// reference either). An unknown handle still gets a real `ENOENT`, not a fabricated success.
    pub(crate) fn gem_close(&self, handle: u32) -> Result<u32, Errno> {
        if self.buffers.lock().contains_key(&handle) {
            Ok(0)
        } else {
            Err(Errno::ENOENT)
        }
    }
}

/// Suppress an unused-field warning for state genuinely written but not yet read anywhere (the
/// framebuffer format, tracked correctly now so the follow-up wgpu-presentation work has real
/// data to read from, but not consumed by anything in this pass).
#[allow(dead_code)]
impl Framebuffer {
    fn info(&self) -> (u32, u32, u32, u32) {
        (self.width, self.height, self.pixel_format, self.handle)
    }
}
