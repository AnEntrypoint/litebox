/* Repeated-PAGE_FLIP DRM guest probe -- verifies LITEBOX_DUMP_FRAMES' background-writer change
 * (bounded queue + dedicated writer thread, LITEBOX_DUMP_FRAMES_EVERY sampling, and
 * LITEBOX_DUMP_FRAMES_METADATA_ONLY) against a real, repeated flip workload instead of the single
 * flip this repo's existing docs/linux-native-drm-gui-probe/drmgui.c exercises.
 *
 * Same raw-ioctl approach as drmgui.c (struct layouts copied verbatim from litebox's own
 * DrmMode* types / real kernel drm.h), extended to flip DRMGUI_FLIP_COUNT times (default 40) in a
 * tight loop, each flip writing a distinct solid color into the SAME dumb buffer so every dumped
 * frame is visibly distinguishable from its neighbors. DRMGUI_FLIP_DELAY_MS (default 0) sleeps
 * between flips so a host-side observer has a chance to see intermediate frames; 0 stresses the
 * writer thread as hard as this guest can drive it, useful for exercising the bounded-queue
 * backpressure/drop path deliberately.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <sys/ioctl.h>
#include <sys/mman.h>

#define DRM_IOCTL_MODE_GETRESOURCES  0xC04064A0u
#define DRM_IOCTL_MODE_GETCRTC       0xC06864A1u
#define DRM_IOCTL_MODE_SETCRTC       0xC06864A2u
#define DRM_IOCTL_MODE_GETCONNECTOR  0xC05064A7u
#define DRM_IOCTL_MODE_CREATE_DUMB   0xC02064B2u
#define DRM_IOCTL_MODE_MAP_DUMB      0xC01064B3u
#define DRM_IOCTL_MODE_ADDFB2        0xC06864B8u
#define DRM_IOCTL_MODE_PAGE_FLIP     0xC01864B0u

struct drm_mode_card_res {
    uint64_t fb_id_ptr, crtc_id_ptr, connector_id_ptr, encoder_id_ptr;
    uint32_t count_fbs, count_crtcs, count_connectors, count_encoders;
    uint32_t min_width, max_width, min_height, max_height;
};

struct drm_mode_modeinfo {
    uint32_t clock;
    uint16_t hdisplay, hsync_start, hsync_end, htotal, hskew;
    uint16_t vdisplay, vsync_start, vsync_end, vtotal, vscan;
    uint32_t vrefresh;
    uint32_t flags, type;
    char name[32];
};

struct drm_mode_get_connector {
    uint64_t encoders_ptr, modes_ptr, props_ptr, prop_values_ptr;
    uint32_t count_modes, count_props, count_encoders;
    uint32_t encoder_id, connector_id, connector_type, connector_type_id;
    uint32_t connection, mm_width, mm_height, subpixel;
    uint32_t pad;
};

struct drm_mode_crtc {
    uint64_t set_connectors_ptr;
    uint32_t count_connectors;
    uint32_t crtc_id, fb_id, x, y, gamma_size, mode_valid;
    struct drm_mode_modeinfo mode;
};

struct drm_mode_create_dumb {
    uint32_t height, width, bpp, flags, handle, pitch;
    uint64_t size;
};

struct drm_mode_map_dumb {
    uint32_t handle, pad;
    uint64_t offset;
};

struct drm_mode_fb_cmd2 {
    uint32_t fb_id, width, height, pixel_format, flags;
    uint32_t handles[4], pitches[4], offsets[4];
    uint32_t pad;
    uint64_t modifier[4];
};

struct drm_mode_crtc_page_flip {
    uint32_t crtc_id, fb_id, flags, reserved;
    uint64_t user_data;
};

#define DRM_FORMAT_XRGB8888 ((uint32_t)('X' | ('R' << 8) | ('2' << 16) | ('4' << 24)))

static int must(int rc, const char *what) {
    if (rc != 0) {
        fprintf(stderr, "%s FAILED rc=%d errno=%d (%s)\n", what, rc, errno, strerror(errno));
        exit(1);
    }
    return rc;
}

static long env_long(const char *name, long fallback) {
    const char *v = getenv(name);
    if (!v || !*v) return fallback;
    return atol(v);
}

int main(void) {
    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) { perror("open /dev/dri/card0"); return 1; }
    printf("OPEN_OK fd=%d\n", fd);

    struct drm_mode_card_res res = {0};
    must(ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res), "GETRESOURCES(probe)");

    uint32_t connector_ids[4] = {0};
    uint32_t crtc_ids[4] = {0};
    res.connector_id_ptr = (uint64_t)(uintptr_t)connector_ids;
    res.crtc_id_ptr = (uint64_t)(uintptr_t)crtc_ids;
    must(ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res), "GETRESOURCES(fill)");

    struct drm_mode_modeinfo modes[4] = {0};
    struct drm_mode_get_connector conn = {0};
    conn.connector_id = connector_ids[0];
    must(ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &conn), "GETCONNECTOR(probe)");
    conn.modes_ptr = (uint64_t)(uintptr_t)modes;
    must(ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &conn), "GETCONNECTOR(fill)");
    printf("CONNECTOR(fill) mode[0]=%ux%u\n", modes[0].hdisplay, modes[0].vdisplay);

    struct drm_mode_create_dumb create = {0};
    create.width = modes[0].hdisplay ? modes[0].hdisplay : 1920;
    create.height = modes[0].vdisplay ? modes[0].vdisplay : 1080;
    create.bpp = 32;
    must(ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &create), "CREATE_DUMB");
    printf("CREATE_DUMB handle=%u pitch=%u size=%llu\n",
           create.handle, create.pitch, (unsigned long long)create.size);

    struct drm_mode_map_dumb map_req = {0};
    map_req.handle = create.handle;
    must(ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map_req), "MAP_DUMB");

    void *map = mmap(NULL, create.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)map_req.offset);
    if (map == MAP_FAILED) { perror("mmap"); return 1; }

    struct drm_mode_fb_cmd2 fb = {0};
    fb.width = create.width;
    fb.height = create.height;
    fb.pixel_format = DRM_FORMAT_XRGB8888;
    fb.handles[0] = create.handle;
    fb.pitches[0] = create.pitch;
    must(ioctl(fd, DRM_IOCTL_MODE_ADDFB2, &fb), "ADDFB2");
    printf("ADDFB2 fb_id=%u\n", fb.fb_id);

    struct drm_mode_crtc crtc = {0};
    crtc.crtc_id = crtc_ids[0];
    crtc.fb_id = fb.fb_id;
    crtc.set_connectors_ptr = (uint64_t)(uintptr_t)connector_ids;
    crtc.count_connectors = 1;
    crtc.mode = modes[0];
    crtc.mode_valid = 1;
    must(ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &crtc), "SETCRTC");
    printf("SETCRTC_OK\n");

    long flip_count = env_long("DRMGUI_FLIP_COUNT", 40);
    long flip_delay_ms = env_long("DRMGUI_FLIP_DELAY_MS", 0);
    printf("MULTIFLIP_START flip_count=%ld flip_delay_ms=%ld\n", flip_count, flip_delay_ms);

    uint8_t *px = (uint8_t *)map;
    for (long i = 0; i < flip_count; i++) {
        /* Distinct per-flip color so every dumped/logged frame is visibly different -- a
         * distinct_colors_capped64 of 1 on every line would leave sampling/backpressure
         * indistinguishable from a stuck writer replaying the same frame. */
        uint8_t cb = (uint8_t)(i * 5);
        uint8_t cg = (uint8_t)(i * 7);
        uint8_t cr = (uint8_t)(i * 11);
        for (uint64_t p = 0; p < create.size; p += 4) {
            px[p + 0] = cb;
            px[p + 1] = cg;
            px[p + 2] = cr;
            px[p + 3] = 0xFF;
        }
        struct drm_mode_crtc_page_flip flip = {0};
        flip.crtc_id = crtc_ids[0];
        flip.fb_id = fb.fb_id;
        int flip_rc = ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &flip);
        printf("PAGE_FLIP i=%ld rc=%d color=%u,%u,%u\n", i, flip_rc, cb, cg, cr);
        fflush(stdout);
        if (flip_delay_ms > 0) usleep((useconds_t)(flip_delay_ms * 1000));
    }

    printf("MULTIFLIP_DONE\n");
    fflush(stdout);
    close(fd);
    printf("DONE\n");
    return 0;
}
