/* Raw-ioctl DRM guest test -- no libdrm dependency, matches litebox's own
 * DrmMode* struct layouts (litebox_common_linux/src/lib.rs) and real kernel
 * drm.h/drm_mode.h struct layouts verbatim. Exercises the full dumb-buffer
 * pipeline: open -> GETRESOURCES -> GETCONNECTOR -> GETCRTC -> CREATE_DUMB ->
 * MAP_DUMB+mmap -> write solid-color pixels -> ADDFB2 -> SETCRTC (page-flip
 * equivalent for the first frame) -> PAGE_FLIP -> sleep so a --gui host
 * window has time to actually render before the process exits.
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

int main(void) {
    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) { perror("open /dev/dri/card0"); return 1; }
    printf("OPEN_OK fd=%d\n", fd);

    struct drm_mode_card_res res = {0};
    must(ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res), "GETRESOURCES(probe)");
    printf("RESOURCES count_connectors=%u count_crtcs=%u count_encoders=%u\n",
           res.count_connectors, res.count_crtcs, res.count_encoders);

    uint32_t connector_ids[4] = {0};
    uint32_t crtc_ids[4] = {0};
    res.connector_id_ptr = (uint64_t)(uintptr_t)connector_ids;
    res.crtc_id_ptr = (uint64_t)(uintptr_t)crtc_ids;
    must(ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res), "GETRESOURCES(fill)");
    printf("RESOURCES(fill) connector[0]=%u crtc[0]=%u\n", connector_ids[0], crtc_ids[0]);

    struct drm_mode_modeinfo modes[4] = {0};
    struct drm_mode_get_connector conn = {0};
    conn.connector_id = connector_ids[0];
    must(ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &conn), "GETCONNECTOR(probe)");
    printf("CONNECTOR connection=%u count_modes=%u\n", conn.connection, conn.count_modes);
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
    printf("MAP_DUMB offset=%llu\n", (unsigned long long)map_req.offset);

    void *map = mmap(NULL, create.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)map_req.offset);
    if (map == MAP_FAILED) { perror("mmap"); return 1; }

    /* Solid color (XRGB8888, byte order B,G,R,X per litebox's own DRM_FORMAT_XRGB8888 use).
     * Overridable via DRMGUI_COLOR=B,G,R env var for a second, distinct-color verification run. */
    uint8_t cb = 0x00, cg = 0xFF, cr = 0x00;
    const char *colenv = getenv("DRMGUI_COLOR");
    if (colenv) {
        unsigned b, g, r;
        if (sscanf(colenv, "%u,%u,%u", &b, &g, &r) == 3) {
            cb = (uint8_t)b; cg = (uint8_t)g; cr = (uint8_t)r;
        }
    }
    uint8_t *px = (uint8_t *)map;
    for (uint64_t i = 0; i < create.size; i += 4) {
        px[i + 0] = cb;
        px[i + 1] = cg;
        px[i + 2] = cr;
        px[i + 3] = 0xFF;
    }
    printf("MMAP_WRITE_OK addr=%p size=%llu (color b=%u g=%u r=%u)\n", map, (unsigned long long)create.size, cb, cg, cr);

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

    struct drm_mode_crtc_page_flip flip = {0};
    flip.crtc_id = crtc_ids[0];
    flip.fb_id = fb.fb_id;
    int flip_rc = ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &flip);
    printf("PAGE_FLIP rc=%d\n", flip_rc);

    printf("ALL_OK -- sleeping 8s for host window to render\n");
    fflush(stdout);
    sleep(8);
    close(fd);
    printf("DONE\n");
    return 0;
}
