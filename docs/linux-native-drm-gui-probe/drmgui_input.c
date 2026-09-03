/* Combined DRM + evdev guest test -- same DRM pipeline as drmgui.c (which
 * stays untouched, already proven and referenced elsewhere), but replaces the
 * blind sleep(8) with a real select()+read() loop on /dev/input/event0,
 * matching the exact live-witness pattern this session already used to
 * verify evdev input injection on Windows. Confirms real synthetic X11 input
 * (via xdotool) reaches the guest through litebox's evdev emulation on
 * native Linux, closing the platform-parity gap with the Windows verification.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <time.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/select.h>

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

/* litebox's own emulated evdev record layout (litebox_common_linux::InputEvent):
 * tv_sec/tv_usec as u64 (not the real kernel's timeval), type/code as u16, value i32. */
struct input_event_litebox {
    uint64_t tv_sec;
    uint64_t tv_usec;
    uint16_t type;
    uint16_t code;
    int32_t value;
};

#define EV_SYN 0x00
#define EV_KEY 0x01
#define EV_REL 0x02

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

    struct drm_mode_create_dumb create = {0};
    create.width = modes[0].hdisplay ? modes[0].hdisplay : 1920;
    create.height = modes[0].vdisplay ? modes[0].vdisplay : 1080;
    create.bpp = 32;
    must(ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &create), "CREATE_DUMB");

    struct drm_mode_map_dumb map_req = {0};
    map_req.handle = create.handle;
    must(ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map_req), "MAP_DUMB");

    void *map = mmap(NULL, create.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)map_req.offset);
    if (map == MAP_FAILED) { perror("mmap"); return 1; }

    uint8_t *px = (uint8_t *)map;
    for (uint64_t i = 0; i < create.size; i += 4) {
        px[i + 0] = 0xFF; /* B */
        px[i + 1] = 0x00; /* G */
        px[i + 2] = 0xFF; /* R -- magenta, distinct from drmgui.c's own probe colors */
        px[i + 3] = 0xFF; /* X */
    }
    printf("MMAP_WRITE_OK\n");

    struct drm_mode_fb_cmd2 fb = {0};
    fb.width = create.width;
    fb.height = create.height;
    fb.pixel_format = DRM_FORMAT_XRGB8888;
    fb.handles[0] = create.handle;
    fb.pitches[0] = create.pitch;
    must(ioctl(fd, DRM_IOCTL_MODE_ADDFB2, &fb), "ADDFB2");

    struct drm_mode_crtc crtc = {0};
    crtc.crtc_id = crtc_ids[0];
    crtc.fb_id = fb.fb_id;
    crtc.set_connectors_ptr = (uint64_t)(uintptr_t)connector_ids;
    crtc.count_connectors = 1;
    crtc.mode = modes[0];
    crtc.mode_valid = 1;
    must(ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &crtc), "SETCRTC");
    printf("SETCRTC_OK -- window should now be visible\n");
    fflush(stdout);

    /* Real evdev live-witness loop, matching this session's own established
     * Windows-verification pattern: open /dev/input/event0, select()+read(),
     * decode real injected events, exit once a real EV_KEY/EV_REL + its
     * SYN_REPORT is seen, or after a bounded timeout. */
    int evfd = open("/dev/input/event0", O_RDONLY);
    if (evfd < 0) { perror("open /dev/input/event0"); close(fd); return 1; }
    printf("EVDEV_OPEN_OK fd=%d\n", evfd);
    fflush(stdout);

    int seen_data_event = 0;
    time_t start = time(NULL);
    int result = 1;
    while (time(NULL) - start < 25) {
        fd_set rfds;
        FD_ZERO(&rfds);
        FD_SET(evfd, &rfds);
        struct timeval tv = {1, 0};
        int rc = select(evfd + 1, &rfds, NULL, NULL, &tv);
        if (rc < 0) { perror("select"); break; }
        if (rc == 0) continue;

        struct input_event_litebox ev;
        ssize_t n = read(evfd, &ev, sizeof(ev));
        if (n != (ssize_t)sizeof(ev)) {
            fprintf(stderr, "READ_SHORT n=%zd errno=%d\n", n, errno);
            continue;
        }
        const char *tname = ev.type == EV_SYN ? "EV_SYN" : ev.type == EV_KEY ? "EV_KEY" : ev.type == EV_REL ? "EV_REL" : "EV_?";
        printf("EVENT type=%s(%u) code=%u value=%d\n", tname, ev.type, ev.code, ev.value);
        fflush(stdout);

        /* Only EV_KEY counts as "the deliberately-injected event this test is waiting for" --
         * window-manager-driven cursor motion (e.g. windowactivate warping the pointer) is a
         * real EV_REL the guest legitimately sees, but not what a caller injecting a specific
         * keypress is testing for. Require EV_KEY specifically to avoid a false-positive exit
         * before real injected input has had a chance to arrive. */
        if (ev.type == EV_KEY) {
            seen_data_event = 1;
        }
        if (ev.type == EV_SYN && ev.code == 0 && seen_data_event) {
            printf("EVDEV_GOT_REAL_EVENT_AND_SYNC\n");
            fflush(stdout);
            result = 0;
            break;
        }
    }
    if (result != 0) {
        printf("EVDEV_TIMEOUT_NO_EVENT\n");
        fflush(stdout);
    }

    close(evfd);
    close(fd);
    printf("DONE\n");
    return result;
}
