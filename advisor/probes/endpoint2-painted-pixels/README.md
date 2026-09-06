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

## Scope of this result -- what it does and does not demonstrate

Stated plainly so "endpoint 2 achieved" is not read as more than it is.

DEMONSTRATED: a single guest X client painting the root window, end to end, through
litebox's own DRM/wgpu path, with Xorg as a forked child. Verified independently
twice (see below).

NOT DEMONSTRATED: a stable multi-client desktop. A full XFCE session runs many
clients concurrently, and the `xclock` fault recorded at the bottom of this file is
very likely what they would hit -- fixed-placement (`MAP_FIXED`) adjacency, which
the general mapping guard gap structurally cannot reach. Treat this as the first
pixel, not a working desktop.

Suggested next work item if anyone continues: `brk`/`sysmalloc` placement
specifically. The fault family is now 3-for-3 the same glibc function
(`libc+0xa0b98` forked-child SIGSEGV, `libc+0xa0966` pid-1 SIGABRT,
`libc+0xa0966` again for `xclock`), and the remaining exposure is exactly the
fixed-placement adjacency a search-side gap cannot govern.

## Independent verification

The colour sequence was decoded twice, by two sessions, from the same captured
BMPs, without one taking the other's word for it.

    this session   frames 3 / 5 / 9    sampled at 5/25/50/75/95% of the pixel array
    sdv            frames 0-3 / 4-7 / 8-11, 2,080 points spread across the full
                   1920x1080 surface for three representative frames

sdv's is the stronger check and it came back `distinct=1` with 100% coverage of the
expected colour in every case: 0 non-black before the paint, 2080/2080 navy,
2080/2080 red. So this is a genuinely uniform full-screen fill, not a corner
artifact or a partial blit. Frame numbering differs by one between the two decodes
(different sample points); nothing turns on it.

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

## Scoping the residual `xclock` SIGSEGV (not fixed; follow-up item)

Traced far enough to characterise it, deliberately not fixed tonight.

The overrun mapping (`0x1138d000..0x113ae000`, 135168 bytes) is itself one of a
long series of **`behavior=Replace` (MAP_FIXED) allocations, all exactly 135168
bytes**, that the guest issues repeatedly while `xclock` runs -- one was placed
at t=108.250, 19 ms before the fault at t=108.269. Sorting every 135168-byte
`Replace` placement in the run and checking neighbours shows they routinely
**abut and even overlap** each other:

    ABUT:    287408128
    ABUT:    287543296
    OVERLAP: 287592448  prev end 287678464
    OVERLAP: 287678464  prev end 287727616
    OVERLAP: 287715328  prev end 287813632
    ...

`cr2` lands 24 bytes (`0x18`) into the region past the mapping's end -- the same
`sysmalloc` chunk-header write (`libc+0xa0966`) as the earlier pid-1 SIGABRT.

Why the guard gap cannot help here: `MAP_FIXED`/`Replace` placements go where the
guest names and never consult `get_unmmaped_area`, so no policy in the search can
separate them. Real Linux behaves the same way. The interesting part is not the
adjacency but the **overlaps** -- a fixed-address request landing inside a live
mapping is not something a correct guest should be doing, so the next question for
whoever picks this up is what issues these repeated same-sized `MAP_FIXED` requests
and whether litebox is reporting the wrong result to an earlier one (an `mmap`
whose return address is not what the guest asked for, or a stale VMA that makes a
subsequent fixed request look free). That is a different investigation from the two
placement bugs fixed tonight, with a different shape.

Impact is limited: `Xorg` and both `xsetroot` clients survived the whole run, and
`xclock` died only after both paints had already been captured.
