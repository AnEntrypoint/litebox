# Endpoint 2 reached: real painted pixels through litebox's own display path

A real X client drove real, verified pixels to litebox's DRM/wgpu scanout, with
Xorg running as a **forked child** (the case that SIGSEGV'd for most of this
investigation). Verified byte-level in the captured framebuffers, not inferred
from a pixel counter.

## The evidence

`r9.sh` (see `../xorg-fork-segv/`, committed as 61f6e25f) paints the root navy,
forces a repaint, then paints it red and repaints again. Decoding the captured
BMPs directly, sampling at 5/25/50/75/95% through the pixel array:

    frame03_black.bmp   BGRA 00 00 00 00                      black, before any paint
    frame05_navy.bmp    BGR(128, 0, 0) at every sample        NAVY, uniform full-screen
    frame09_red.bmp     BGR(0, 0, 255) at every sample        RED,  uniform full-screen

The exact two colours the script requests, in the order it requests them, filling
the entire 1920x1080 surface. Corroborating, from the same run:

    non_black_pixels = 2073600 = 1920 * 1080 exactly   (0 in every previous run)
    distinct_colors_capped64 = 1                        (a uniform solid fill)
    NAVY=0  REFRESH1=0  RED=0  REFRESH2=0               (every client command succeeded)

The DRM content digest changes with the colour and changes back, which rules out
a monotonic drift artifact:

    97.517s digest=3893184410428063744   navy
    97.712s digest=8695966299811479680   red
   102.888s digest=3893184410428063744   navy again

`DIAG_DIRTYFB` fires six times (`fb_id=2 num_clips=1`), confirming Xorg really
does issue `DRM_IOCTL_MODE_DIRTYFB` and that the handler is on the live path.

## What had to be fixed, all three

    d057bd37  keep a discarded Hint address as a placement floor
              -> a forked child stops having its whole image packed into a ~120MB
                 low window with inter-library gaps as small as one page

    9df01727  never place two searched mappings flush against each other
              -> glibc's `sysmalloc` stops extending the heap into its neighbour;
                 fixes the pid-1 SIGABRT that the placement floor exposed

    2dfc4d5a  implement DRM_IOCTL_MODE_DIRTYFB
              -> damage flushes actually reach the host surface. Without it the
                 only path to the presenter was PAGE_FLIP, which a single-buffer
                 modesetting Xorg with no compositor never performs, so a correct
                 paint had nowhere to go

## Known remaining defect, not fixed here

`xclock` SIGSEGV'd at t=108.27, after the paints:

    rip=0x7feff64c0966  cr2=0x113ae018  error_code=0x6
    mapping overlapping cr2: 0x1138d000..0x113ae000  VM_READ | VM_WRITE

`rip` is `libc+0xa0966` -- the same `sysmalloc` chunk-header write as the pid-1
SIGABRT, and `cr2` is again 0x18 past a mapping's end. So this is the same
adjacency fault class, reached through a placement path the guard gap cannot
govern: `MAP_FIXED`/`Replace` placements never consult `get_unmmaped_area`
(measured at 5 residual abutting pairs of 112 mappings). Xorg and both `xsetroot`
clients survived; only `xclock` died, and only after the paints, so it does not
affect the result above.
