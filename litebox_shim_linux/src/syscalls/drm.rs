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
//! # What this pass deliberately does NOT implement
//!
//! - **Page-flip completion events**: `DRM_IOCTL_MODE_PAGE_FLIP` succeeds immediately and does
//!   not queue a real `DRM_EVENT_FLIP_COMPLETE` event for later `read()`. A client that
//!   requested `DRM_MODE_PAGE_FLIP_EVENT` and then blocks in `poll()`/`read()` waiting for that
//!   event will hang. This needs the DRM device fd's own read/poll readiness wired through
//!   litebox's `Pollee`/`Events` machinery (the same primitive `pty.rs` already uses) -- a real,
//!   separate follow-up, not attempted here to keep this pass reviewable.
//! - **`mmap()` of a dumb buffer's fake offset actually resolving to the buffer's real bytes**:
//!   `DRM_IOCTL_MODE_MAP_DUMB` returns a real, uniquely-allocated fake offset, but nothing yet
//!   wires that offset into `sys_mmap`'s file-backed-mapping path so a guest's own
//!   `mmap(fd, ..., offset)` call actually maps the buffer's storage. The buffer storage itself
//!   (this module's `Vec<u8>`) is real and correctly sized/tracked; only the mmap bridge is
//!   missing. A real follow-up, not faked here.
//! - **wgpu-backed host presentation**: out of scope for this pass entirely (a separate PRD
//!   row); no pixels drawn by a guest client are yet visible anywhere on the host.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU32, Ordering};

use litebox_common_linux::{
    DRM_MODE_CONNECTOR_VIRTUAL, DRM_MODE_ENCODER_VIRTUAL, DrmModeCardRes, DrmModeCrtc,
    DrmModeCrtcPageFlip, DrmModeCreateDumb, DrmModeDestroyDumb, DrmModeFbCmd2, DrmModeGetConnector,
    DrmModeGetEncoder, DrmModeMapDumb, DrmModeModeinfo, errno::Errno,
};

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
struct DumbBuffer {
    width: u32,
    height: u32,
    bpp: u32,
    pitch: u32,
    /// Real, correctly-sized pixel storage. Not yet reachable via `mmap()` -- see this module's
    /// doc comment.
    storage: alloc::vec::Vec<u8>,
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
    buffers: litebox::sync::Mutex<Platform, BTreeMap<u32, DumbBuffer>>,
    framebuffers: litebox::sync::Mutex<Platform, BTreeMap<u32, Framebuffer>>,
    /// The framebuffer currently attached to the virtual CRTC (via `SETCRTC` or `PAGE_FLIP`),
    /// `None` until the guest sets one.
    crtc_fb: litebox::sync::Mutex<Platform, Option<u32>>,
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
        }
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

    pub(crate) fn set_crtc(&self, ptr: UserPtr<DrmModeCrtc>) -> Result<u32, Errno> {
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.crtc_id != VIRTUAL_CRTC_ID {
            return Err(Errno::ENOENT);
        }
        if req.fb_id != 0 && !self.framebuffers.lock().contains_key(&req.fb_id) {
            return Err(Errno::ENOENT);
        }
        *self.crtc_fb.lock() = if req.fb_id == 0 { None } else { Some(req.fb_id) };
        Ok(0)
    }

    pub(crate) fn create_dumb(&self, ptr: UserPtrMut<DrmModeCreateDumb>) -> Result<u32, Errno> {
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
        let handle = self.next_buffer_handle.fetch_add(1, Ordering::Relaxed);
        self.buffers.lock().insert(
            handle,
            DumbBuffer {
                width: req.width,
                height: req.height,
                bpp: req.bpp,
                pitch,
                storage: alloc::vec![0u8; size_usize],
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

    pub(crate) fn destroy_dumb(&self, ptr: UserPtr<DrmModeDestroyDumb>) -> Result<u32, Errno> {
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        let mut buffers = self.buffers.lock();
        if buffers.remove(&req.handle).is_none() {
            return Err(Errno::ENOENT);
        }
        drop(buffers);
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

    pub(crate) fn page_flip(&self, ptr: UserPtr<DrmModeCrtcPageFlip>) -> Result<u32, Errno> {
        let req = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        if req.crtc_id != VIRTUAL_CRTC_ID {
            return Err(Errno::ENOENT);
        }
        if !self.framebuffers.lock().contains_key(&req.fb_id) {
            return Err(Errno::ENOENT);
        }
        *self.crtc_fb.lock() = Some(req.fb_id);
        // See this module's doc comment: a real `DRM_MODE_PAGE_FLIP_EVENT` completion event is
        // not queued here. The flip itself (updating which framebuffer the CRTC scans out) is
        // real and immediate; only the asynchronous completion NOTIFICATION is stubbed.
        Ok(0)
    }
}

/// Suppress unused-field warnings for state genuinely written but not yet read anywhere (the
/// buffer dimensions/pitch and framebuffer format, tracked correctly now so the follow-up
/// mmap-bridge and wgpu-presentation work has real data to read from, but not consumed by
/// anything in this pass).
#[allow(dead_code)]
impl DumbBuffer {
    fn dimensions(&self) -> (u32, u32, u32, u32) {
        (self.width, self.height, self.bpp, self.pitch)
    }
}
#[allow(dead_code)]
impl Framebuffer {
    fn info(&self) -> (u32, u32, u32, u32) {
        (self.width, self.height, self.pixel_format, self.handle)
    }
}
