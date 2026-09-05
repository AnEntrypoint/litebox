// Minimal DRM page-flip probe: drives the virtual /dev/dri/card0 scanout path end-to-end
// WITHOUT weston, Xwayland, or any part of XFCE, turning a multi-minute full-stack launch into
// a ~1s test of the display pipeline itself.
//
// Why this exists: every existing way to make a page-flip happen in this project runs the whole
// GUI stack, so a display-path change could only be tested behind an unrelated, currently-open
// GUI blocker. This isolates the ONE behaviour -- guest draws, guest flips, host observers see
// the frame -- so the presentation half can be verified on its own.
//
// It fills the framebuffer with a KNOWN, non-uniform test pattern (vertical colour bands plus a
// solid marker block in the top-left) rather than a flat colour, so a captured frame proves the
// real guest bytes arrived: a flat fill is indistinguishable from a cleared/zeroed buffer, and
// pixel COUNTS alone cannot tell one painter from another (a lesson this project has already
// paid for once -- see the "pixel count does not identify the painter" note).
//
// Build ON THE HOST (no guest toolchain needed -- both guest compilers are broken, see README):
//   clang --target=x86_64-unknown-linux-gnu -nostdlib -nostdinc -ffreestanding \
//         -fno-stack-protector -static -O1 -o drm_flip_probe drm_flip_probe.c
//
// Run as the runner's TOP-LEVEL program from a tar layer (never via `sh -c`):
//   --initial-files <layer>.tar /drm_flip_probe
// Optional argv[1] = number of flips (default 3).
//
// Prints one line per stage, so a failure names its own syscall instead of needing a bisect.
// Exit code 0 = every stage succeeded.

typedef long i64;
typedef unsigned long u64;
typedef unsigned int u32;

static i64 sys1(i64 n, i64 a) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a) : "rcx", "r11", "memory"); return r; }
static i64 sys3(i64 n, i64 a, i64 b, i64 c) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory"); return r; }
static i64 sys6(i64 n, i64 a, i64 b, i64 c, i64 d, i64 e, i64 f) { i64 r; register i64 r10 __asm__("r10") = d, r8 __asm__("r8") = e, r9 __asm__("r9") = f; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9) : "rcx", "r11", "memory"); return r; }

#define SYS_write 1
#define SYS_open 2
#define SYS_close 3
#define SYS_mmap 9
#define SYS_ioctl 16
#define SYS_exit 60

static unsigned slen(const char *s) { unsigned n = 0; while (s[n]) n++; return n; }
static void out(const char *s) { sys3(SYS_write, 1, (i64)s, slen(s)); }
static void outn(i64 v) {
    char b[24]; int i = 23; b[i--] = 0;
    int neg = v < 0; u64 u = neg ? (u64)(-v) : (u64)v;
    if (!u) b[i--] = '0';
    while (u) { b[i--] = (char)('0' + u % 10); u /= 10; }
    if (neg) b[i--] = '-';
    out(&b[i + 1]);
}

// --- DRM ioctl ABI (from drm.h / drm_mode.h; only the fields this probe uses) -----------------
#define DRM_IOCTL_MODE_CREATE_DUMB  0xc02064b2
#define DRM_IOCTL_MODE_MAP_DUMB     0xc01064b3
// ADDFB2, not the legacy ADDFB: litebox implements only the former (`add_fb2` in drm.rs), and a
// legacy `DRM_IOCTL_MODE_ADDFB` (0xc01c64ae) returns EINVAL as an unsupported ioctl. Worth
// knowing when reading a client's failure: that EINVAL reads like a bad argument rather than a
// missing feature. No client in this tree actually calls legacy ADDFB (weston and every probe
// here use ADDFB2), so it is an unproven gap, not a known blocker.
#define DRM_IOCTL_MODE_ADDFB2       0xc06864b8
#define DRM_IOCTL_MODE_PAGE_FLIP    0xc01864b0

#define DRM_FORMAT_XRGB8888 0x34325258  /* fourcc 'XR24' -- the one format dumb buffers support */

struct create_dumb { u32 height, width, bpp, flags; u32 handle, pitch; u64 size; };
struct map_dumb    { u32 handle, pad; u64 offset; };
// Mirrors `DrmModeFbCmd2` in litebox_common_linux/src/lib.rs, including the explicit 4-byte pad
// the C ABI inserts before the 8-byte-aligned `modifier` array.
struct addfb2 { u32 fb_id, width, height, pixel_format, flags;
                u32 handles[4], pitches[4], offsets[4]; u32 pad; u64 modifier[4]; };
struct page_flip   { u32 crtc_id, fb_id, flags, reserved; u64 user_data; };

#define PAGE_FLIP_EVENT 0x01
#define VCRTC 3   /* VIRTUAL_CRTC_ID in litebox_shim_linux/src/syscalls/drm.rs */

int main(int argc, char **argv) {
    int flips = 3;
    if (argc > 1) {
        flips = 0;
        for (const char *p = argv[1]; *p >= '0' && *p <= '9'; p++) flips = flips * 10 + (*p - '0');
        if (flips <= 0) flips = 3;
    }

    i64 fd = sys3(SYS_open, (i64)"/dev/dri/card0", 2 /*O_RDWR*/, 0);
    out("open /dev/dri/card0 -> "); outn(fd); out("\n");
    if (fd < 0) { out("FAIL: no DRM device\n"); sys1(SYS_exit, 1); }

    struct create_dumb cd = {0};
    cd.width = 1920; cd.height = 1080; cd.bpp = 32;
    i64 r = sys3(SYS_ioctl, fd, DRM_IOCTL_MODE_CREATE_DUMB, (i64)&cd);
    out("CREATE_DUMB -> "); outn(r); out(" handle="); outn(cd.handle);
    out(" pitch="); outn(cd.pitch); out(" size="); outn((i64)cd.size); out("\n");
    if (r < 0) { out("FAIL: CREATE_DUMB\n"); sys1(SYS_exit, 2); }

    struct addfb2 fb = {0};
    fb.width = cd.width; fb.height = cd.height; fb.pixel_format = DRM_FORMAT_XRGB8888;
    fb.handles[0] = cd.handle; fb.pitches[0] = cd.pitch;
    r = sys3(SYS_ioctl, fd, DRM_IOCTL_MODE_ADDFB2, (i64)&fb);
    out("ADDFB2 -> "); outn(r); out(" fb_id="); outn(fb.fb_id); out("\n");
    if (r < 0) { out("FAIL: ADDFB2\n"); sys1(SYS_exit, 3); }

    struct map_dumb md = {0}; md.handle = cd.handle;
    r = sys3(SYS_ioctl, fd, DRM_IOCTL_MODE_MAP_DUMB, (i64)&md);
    out("MAP_DUMB -> "); outn(r); out(" offset="); outn((i64)md.offset); out("\n");
    if (r < 0) { out("FAIL: MAP_DUMB\n"); sys1(SYS_exit, 4); }

    i64 p = sys6(SYS_mmap, 0, (i64)cd.size, 3 /*RW*/, 1 /*MAP_SHARED*/, fd, (i64)md.offset);
    out("mmap -> "); outn(p); out("\n");
    if (p < 0 && p > -4096) { out("FAIL: mmap\n"); sys1(SYS_exit, 5); }
    volatile u32 *px = (volatile u32 *)p;

    for (int f = 0; f < flips; f++) {
        // A DISTINCT, non-uniform pattern per flip. Vertical bands vary by x so a captured frame
        // can be checked for real structure, and the band colour shifts per flip so successive
        // frames are distinguishable from one frame captured repeatedly.
        for (u32 y = 0; y < cd.height; y++) {
            volatile u32 *row = (volatile u32 *)((char *)px + (u64)y * cd.pitch);
            for (u32 x = 0; x < cd.width; x++) {
                u32 band = ((x / 120) + (u32)f) & 7;
                // XRGB8888: 0x00RRGGBB
                row[x] = ((band & 1) ? 0x00FF0000u : 0u)
                       | ((band & 2) ? 0x0000FF00u : 0u)
                       | ((band & 4) ? 0x000000FFu : 0u);
            }
        }
        // Solid white marker block, top-left 64x64: an unmistakable orientation/liveness cue in a
        // captured .bmp, and it proves row 0 is reached (a pitch error usually skews or drops it).
        for (u32 y = 0; y < 64; y++) {
            volatile u32 *row = (volatile u32 *)((char *)px + (u64)y * cd.pitch);
            for (u32 x = 0; x < 64; x++) row[x] = 0x00FFFFFFu;
        }

        struct page_flip pf = {0};
        pf.crtc_id = VCRTC; pf.fb_id = fb.fb_id; pf.flags = PAGE_FLIP_EVENT; pf.user_data = (u64)f;
        r = sys3(SYS_ioctl, fd, DRM_IOCTL_MODE_PAGE_FLIP, (i64)&pf);
        out("PAGE_FLIP "); outn(f); out(" -> "); outn(r); out("\n");
        if (r < 0) { out("FAIL: PAGE_FLIP\n"); sys3(SYS_close, fd, 0, 0); sys1(SYS_exit, 6); }
    }

    out("FLIPS_DONE="); outn(flips); out("\n");
    out("DRM_FLIP_PROBE_OK\n");
    sys3(SYS_close, fd, 0, 0);
    sys1(SYS_exit, 0);
    __builtin_unreachable();
}

// `_start` is entered with `rsp` pointing at `argc` and, per the SysV ABI, NOT with the 16-byte
// alignment a normal function entry guarantees. The compiler is free to emit aligned SSE stores
// (`movaps`) for local struct zeroing, which fault on a misaligned stack -- and that fault
// surfaces as a SIGSEGV with `cr2=0x0`, which reads exactly like a null-pointer dereference and
// sends you looking for a bug that isn't there. So capture the incoming stack pointer, then
// align `rsp` before calling into any C.
__attribute__((naked)) void _start(void) {
    __asm__ volatile(
        // arg1 = original stack pointer (argc, then argv[]); then 16-byte align before any call.
        "movq %rsp, %rdi\n"
        "andq $-16, %rsp\n"
        "callq _start_c\n"
    );
}

void _start_c(long *sp) {
    main((int)sp[0], (char **)&sp[1]);
    sys1(SYS_exit, 0);
    __builtin_unreachable();
}
