#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/mman.h>
#include <xf86drm.h>
#include <xf86drmMode.h>
#include <drm_fourcc.h>

int main(void) {
    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) {
        perror("open /dev/dri/card0");
        return 1;
    }
    printf("OPEN_OK fd=%d\n", fd);

    drmVersionPtr ver = drmGetVersion(fd);
    if (!ver) {
        fprintf(stderr, "drmGetVersion FAILED\n");
        return 1;
    }
    printf("VERSION name=%s date=%s desc=%s major=%d minor=%d patch=%d\n",
           ver->name ? ver->name : "(null)",
           ver->date ? ver->date : "(null)",
           ver->desc ? ver->desc : "(null)",
           ver->version_major, ver->version_minor, ver->version_patchlevel);
    drmFreeVersion(ver);

    uint64_t has_dumb = 0;
    int rc = drmGetCap(fd, DRM_CAP_DUMB_BUFFER, &has_dumb);
    printf("GET_CAP(DUMB_BUFFER) rc=%d value=%llu\n", rc, (unsigned long long)has_dumb);
    if (rc != 0 || has_dumb != 1) {
        fprintf(stderr, "GET_CAP FAILED or unexpected value\n");
        return 1;
    }

    rc = drmSetMaster(fd);
    printf("SET_MASTER rc=%d\n", rc);
    if (rc != 0) {
        fprintf(stderr, "SET_MASTER FAILED\n");
        return 1;
    }

    drmModeResPtr res = drmModeGetResources(fd);
    if (!res) {
        fprintf(stderr, "drmModeGetResources FAILED\n");
        return 1;
    }
    printf("RESOURCES count_connectors=%d count_encoders=%d count_crtcs=%d count_fbs=%d\n",
           res->count_connectors, res->count_encoders, res->count_crtcs, res->count_fbs);

    if (res->count_connectors < 1) {
        fprintf(stderr, "no connectors\n");
        return 1;
    }
    drmModeConnectorPtr conn = drmModeGetConnector(fd, res->connectors[0]);
    if (!conn) {
        fprintf(stderr, "drmModeGetConnector FAILED\n");
        return 1;
    }
    printf("CONNECTOR id=%u connection=%d count_modes=%d width_mm=%u height_mm=%u\n",
           conn->connector_id, conn->connection, conn->count_modes,
           conn->mmWidth, conn->mmHeight);

    if (res->count_crtcs < 1) {
        fprintf(stderr, "no crtcs\n");
        return 1;
    }
    drmModeCrtcPtr crtc = drmModeGetCrtc(fd, res->crtcs[0]);
    if (!crtc) {
        fprintf(stderr, "drmModeGetCrtc FAILED\n");
        return 1;
    }
    printf("CRTC id=%u buffer_id=%u width=%u height=%u mode_valid=%d\n",
           crtc->crtc_id, crtc->buffer_id, crtc->width, crtc->height, crtc->mode_valid);

    /* Full dumb-buffer pipeline */
    struct drm_mode_create_dumb create = {0};
    create.width = 64;
    create.height = 64;
    create.bpp = 32;
    rc = drmIoctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &create);
    printf("CREATE_DUMB rc=%d handle=%u pitch=%u size=%llu\n",
           rc, create.handle, create.pitch, (unsigned long long)create.size);
    if (rc != 0) {
        fprintf(stderr, "CREATE_DUMB FAILED\n");
        return 1;
    }

    uint32_t fb_id = 0;
    uint32_t handles[4] = {create.handle, 0, 0, 0};
    uint32_t pitches[4] = {create.pitch, 0, 0, 0};
    uint32_t offsets[4] = {0, 0, 0, 0};
    rc = drmModeAddFB2(fd, 64, 64, DRM_FORMAT_XRGB8888, handles, pitches, offsets, &fb_id, 0);
    printf("ADDFB2 rc=%d fb_id=%u\n", rc, fb_id);
    if (rc != 0) {
        fprintf(stderr, "ADDFB2 FAILED\n");
        return 1;
    }

    struct drm_mode_map_dumb map_req = {0};
    map_req.handle = create.handle;
    rc = drmIoctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map_req);
    printf("MAP_DUMB rc=%d offset=%llu\n", rc, (unsigned long long)map_req.offset);
    if (rc != 0) {
        fprintf(stderr, "MAP_DUMB FAILED\n");
        return 1;
    }

    void *map = mmap(NULL, create.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, map_req.offset);
    if (map == MAP_FAILED) {
        perror("mmap");
        return 1;
    }
    memset(map, 0xAB, create.size);
    printf("MMAP_WRITE_OK addr=%p size=%llu\n", map, (unsigned long long)create.size);

    rc = drmModeSetCrtc(fd, res->crtcs[0], fb_id, 0, 0, &res->connectors[0], 1, conn->count_modes > 0 ? &conn->modes[0] : NULL);
    printf("SETCRTC rc=%d\n", rc);

    rc = drmDropMaster(fd);
    printf("DROP_MASTER rc=%d\n", rc);

    printf("ALL_OK\n");
    close(fd);
    return 0;
}
