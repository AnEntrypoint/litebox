# `DRM_IOCTL_MODE_DIRTYFB` is not implemented — likely why nothing presents

**Status: analysis only, NOT implemented.** Written so whoever picks this up does not
re-derive it. Endpoint 2 (Debian/XFCE via the litebox display) is NOT achieved.

## Why this matters

`litebox_shim_linux/src/syscalls/drm.rs`, module doc lines 27-32, states the
architecture outright:

> **Framebuffer & Page Flipping**: `DRM_IOCTL_MODE_ADDFB2` attaches the dumb buffer
> to a framebuffer handle. `DRM_IOCTL_MODE_PAGE_FLIP` sets the CRTC scanout
> framebuffer and triggers every registered flip callback.
> **wgpu Host Presentation Pipeline**: ... every `DRM_IOCTL_MODE_PAGE_FLIP`
> transfers the active framebuffer's pixels to the host `wgpu` surface.

So **`PAGE_FLIP` is the only route from guest pixels to the host surface.**

The implemented mode ioctls are exactly: ADDFB, ADDFB2, ATOMIC,
CONNECTOR_SETPROPERTY, CREATE_DUMB, DESTROY_DUMB, GETPLANE, GETPROPBLOB,
GETPROPERTY, GETRESOURCES, MAP_DUMB, OBJ_GETPROPERTIES, PAGE_FLIP, RMFB, SETCRTC.
A grep for `dirtyfb|dirty_fb` across `litebox/src`, `litebox_shim_linux/src` and
`litebox_platform_windows_userland/src` returns **nothing** — not implemented, not
stubbed, not referenced.

Xorg's `modesetting` driver — what we run — uses `DIRTYFB` to flush shadow-buffer
damage when it is *not* page-flipping. With a single framebuffer and no compositor
that is its normal steady state: draw into the shadow, then `DIRTYFB` the damaged
rectangles. It page-flips only when it has multiple buffers to flip between.

That predicts exactly what is observed: a healthy X server, a client that connects
and succeeds (`SETROOT=0`), and **no frame dumps after startup** — because dumps
fire only on `PAGE_FLIP` (`presentation.rs:159`). Nothing is broken in the paint;
the damage signal goes to an ioctl that does not exist.

## Beware: frame dumps are flip-driven, not periodic

There is no timer-based or content-change sampling. A black frame proves nothing
unless a flip is shown to have occurred *after* the paint. In one captured run both
dumps preceded the paint by ~350 log lines with zero after it.

## The implementation (four small edits)

1. `litebox_common_linux/src/lib.rs` — constant, next to `DRM_IOCTL_MODE_PAGE_FLIP`
   (`0xC018_64B0`, nr `0xB0`, 24 bytes). DIRTYFB is nr `0xB1` and also 24 bytes:

       pub const DRM_IOCTL_MODE_DIRTYFB: u32 = 0xC018_64B1;

2. `litebox_common_linux/src/lib.rs` (~1636, beside `DrmModeCrtcPageFlip`) — struct
   `drm_mode_fb_dirty_cmd`:

       pub struct DrmModeFbDirtyCmd {
           pub fb_id: u32, pub flags: u32, pub color: u32,
           pub num_clips: u32, pub clips_ptr: u64,
       }

3. `litebox_common_linux/src/lib.rs` — `IoctlArg` variant (~1877) plus dispatch arm
   (~4184), mirroring `DRM_IOCTL_MODE_PAGE_FLIP => IoctlArg::DrmModePageFlip(...)`.

4. `litebox_shim_linux/src/syscalls/drm.rs` — handler, and its arm in the
   `file.rs:4968` match. The handler is **smaller than `page_flip`**: `page_flip`
   sets `*self.crtc_fb.lock() = Some(req.fb_id)` and then calls
   `self.notify_flip_callback(platform, req.fb_id)`. DIRTYFB needs only the notify —
   the framebuffer is already the current scanout — plus validating `fb_id` exists
   in `self.framebuffers`. The clip rectangles can be ignored initially (present the
   whole framebuffer); correctness first, damage-rect optimisation later.

No changes to the presentation pipeline: it reuses the flip-callback path that is
already proven (it produced the 518,416-pixel X11 root weave).

## Testing

Any client that draws without flipping. `xsetroot -solid` alone may never produce a
flip; `xsetroot -solid navy; xrefresh` forces a root repaint, and `xclock` redraws on
its own timer. Compare captured frames against
`advisor/probes/baseline_xorg_pid1_black.bmp` — a pass needs content the black
baseline cannot produce, and successive captures must *differ* from each other.
