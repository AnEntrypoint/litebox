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

---

# IMPLEMENTED (commit 2dfc4d5a) — verification plan

Implemented exactly as designed above. **Not yet verified against a live guest.**

Design point confirmed while writing it: `notify_flip_callback` (drm.rs:613) resolves
`handle`/`size`/`width`/`height`/`pitch`/`pixel_format` from `fb_id` alone, via
`self.framebuffers` then `self.buffers`. It never reads `crtc_fb`. So the handler
declining to touch `crtc_fb` is *correct*, not merely safe — a damage flush must not
retarget the scanout.

## Use the digest, not the frame dumps

`drm.rs:668-676`, inside `notify_flip_callback`, already emits at **error** level:

    litebox_util_log::error!(fb_id, digest = sum; "diag-drm-digest");
    litebox_util_log::error!(fb_id, bytes_len, nonzero_bytes, first8 = ...);

It samples the framebuffer and reports a content digest plus a nonzero-byte count on
every notify. This is strictly better than the BMP dumps for this question: it fires
per-notify (so DIRTYFB-driven presents are captured even when the flip-driven dump
path never triggers), it measures content directly, and it survives
`LITEBOX_LOG=error`. **It is gated behind `drm_trace_enabled()`, so the run needs
`LITEBOX_DRM_TRACE=1`.**

## The test

`r9.sh` (in this directory). Xorg runs as a *forked child* — deliberately the case the
`placement_floor` fix repaired, not the easy pid-1 path. It polls for
`/tmp/.X11-unix/X0` rather than sleeping a fixed time. Every client is a plain
`fork+exec`: no wrapper, no retry loop, no second runner.

    phase 1  xsetroot -solid navy ; xrefresh
    phase 2  xsetroot -solid red  ; xrefresh    (a DIFFERENT colour, so a real
                                                 capture must differ from phase 1)
    phase 3  xclock for 15s                     (redraws on its own timer, not just
                                                 a property setter)

Run:

    LITEBOX_LOG=error LITEBOX_DRM_TRACE=1 LITEBOX_DUMP_FRAMES=1 <runner> --unstable \
      --oci-image linuxserver/webtop:debian-xfce --gui-hidden \
      --resume-from r9.tar -- /bin/sh /r9.sh

## Pre-registered readings

| observation | conclusion |
|---|---|
| `DIAG_DIRTYFB` lines appear | Xorg does issue DIRTYFB; the handler is on the live path |
| `DIAG_DIRTYFB` never appears | **hypothesis REFUTED** — Xorg is not using DIRTYFB and the real gap is elsewhere. Report as such. |
| digest CHANGES phase 1 → 2 | pixels genuinely reaching the presenter; the display path works end to end |
| digest constant, `nonzero_bytes=0` | presents happening but content black — a third, different problem |
| `DIAG_DIRTYFB` present, no digest lines, **both env vars set** | `notify_flip_callback` early-returned at drm.rs:616 with zero registered callbacks — a real finding |
| `DIAG_DIRTYFB` present, no digest lines, **either var missing** | **test configuration error, not a result** — re-run with both |

## Both env vars are REQUIRED, for different reasons

`LITEBOX_DRM_TRACE=1` gates the digest lines themselves (`drm_trace_enabled()`).

`LITEBOX_DUMP_FRAMES=1` is *not* optional either. The runner registers a flip
observer only inside `if std::env::var_os("LITEBOX_DUMP_FRAMES").is_some()`
(`litebox_runner_linux_on_windows_userland/src/lib.rs:836`) — independent of
`--gui`, since flip observers are additive. Without it a `--gui-hidden` run can have
*zero* registered callbacks, and `notify_flip_callback` early-returns at
`drm.rs:616` **before** reaching the digest code at `:668`. DIRTYFB could then be
working perfectly, `DIAG_DIRTYFB` printing, and the digest still silent — because
nothing registered to observe it.

**The general rule, and this is the fifth instance tonight** (log levels, the
`no_std` cfg gate, two emit sites sharing one message string, flip-driven dumps, and
now this): before trusting the *absence* of a diagnostic, verify every condition on
the path that produces it — not just the one it is named for.

Phase 3's `xclock &` is a background fork, i.e. the case `placement_floor` repaired.
If phase 3 alone fails while 1-2 pass, look there first.
