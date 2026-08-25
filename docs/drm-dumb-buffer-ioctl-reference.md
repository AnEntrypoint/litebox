# DRM dumb-buffer ioctl reference

Extracted verbatim (field names/types/order preserved) from the Linux kernel's
`include/uapi/drm/drm_mode.h` (fetched from `torvalds/linux` master), scoped to
the minimal slice needed for a software-only "dumb buffer" DRM/KMS device: one
fake connector + CRTC + plane, buffer alloc/map/destroy, framebuffer attach,
modeset, and page-flip. Source is the exact kernel UAPI struct — litebox's
guest-memory parsing must match these layouts byte-for-byte (this is the ABI
real Linux binaries are compiled against).

Each field keeps its original C type (`__u32`, `__u64`, `__s32`, `char[N]`);
translate to `#[repr(C)]` Rust with the matching fixed-width type
(`u32`/`u64`/`i32`/`[u8; N]`) when implementing — not done here, this is the
struct-shape reference only.

## Buffer lifecycle

### `struct drm_mode_create_dumb`
Request: allocate a dumb (CPU-writable, linear, no GPU) pixel buffer.

```c
struct drm_mode_create_dumb {
    __u32 height;
    __u32 width;
    __u32 bpp;      // bits per pixel; 32 = XRGB8888 is the common case
    __u32 flags;    // must be zero

    __u32 handle;   // OUT: buffer object handle
    __u32 pitch;    // OUT: bytes per scanline
    __u64 size;     // OUT: total buffer size in bytes
};
```
Userspace fills `height`/`width`/`bpp`/`flags`; kernel fills `handle`/`pitch`/`size`.

### `struct drm_mode_map_dumb`
Request: get an mmap-able fake offset for a previously created dumb buffer.

```c
struct drm_mode_map_dumb {
    __u32 handle;   // IN: handle from drm_mode_create_dumb
    __u32 pad;
    __u64 offset;   // OUT: fake offset, passed to mmap(2) on the DRM fd
};
```

### `struct drm_mode_destroy_dumb`
Request: free a dumb buffer.

```c
struct drm_mode_destroy_dumb {
    __u32 handle;
};
```

## Framebuffer attach

### `struct drm_mode_fb_cmd` (legacy, `DRM_IOCTL_MODE_ADDFB`)
```c
struct drm_mode_fb_cmd {
    __u32 fb_id;
    __u32 width;
    __u32 height;
    __u32 pitch;
    __u32 bpp;
    __u32 depth;
    __u32 handle;   // driver-specific handle (the dumb-buffer handle)
};
```

### `struct drm_mode_fb_cmd2` (modern, `DRM_IOCTL_MODE_ADDFB2`/`GETFB2`, preferred)
Up to 4 planes for planar formats; a single-plane dumb buffer uses index 0 only.

```c
struct drm_mode_fb_cmd2 {
    __u32 fb_id;
    __u32 width;
    __u32 height;
    __u32 pixel_format;   // FourCC, e.g. DRM_FORMAT_XRGB8888
    __u32 flags;          // DRM_MODE_FB_INTERLACED | DRM_MODE_FB_MODIFIERS

    __u32 handles[4];     // GEM buffer handle per plane; 0 = unused
    __u32 pitches[4];     // stride in bytes per plane
    __u32 offsets[4];     // byte offset into the buffer per plane
    __u64 modifier[4];    // format modifier per plane; all planes must match
};
```
Flag bits: `DRM_MODE_FB_INTERLACED = 1<<0`, `DRM_MODE_FB_MODIFIERS = 1<<1`.

## Mode/resource enumeration

### `struct drm_mode_modeinfo`
One display mode (timing description).

```c
struct drm_mode_modeinfo {
    __u32 clock;         // pixel clock in kHz
    __u16 hdisplay;
    __u16 hsync_start;
    __u16 hsync_end;
    __u16 htotal;
    __u16 hskew;
    __u16 vdisplay;
    __u16 vsync_start;
    __u16 vsync_end;
    __u16 vtotal;
    __u16 vscan;

    __u32 vrefresh;      // approximate vertical refresh rate in Hz

    __u32 flags;         // DRM_MODE_FLAG_* bitmask
    __u32 type;          // DRM_MODE_TYPE_* bitmask
    char name[32];       // DRM_DISPLAY_MODE_LEN == 32
};
```

### `struct drm_mode_card_res` (`DRM_IOCTL_MODE_GETRESOURCES`)
Top-level enumeration: counts + pointers to arrays of object IDs.

```c
struct drm_mode_card_res {
    __u64 fb_id_ptr;
    __u64 crtc_id_ptr;
    __u64 connector_id_ptr;
    __u64 encoder_id_ptr;
    __u32 count_fbs;
    __u32 count_crtcs;
    __u32 count_connectors;
    __u32 count_encoders;
    __u32 min_width;
    __u32 max_width;
    __u32 min_height;
    __u32 max_height;
};
```
Two-call pattern: caller zeroes the `count_*` fields (or the ptrs) to learn
sizes, then allocates arrays and calls again with `*_ptr` pointing at them.

### `struct drm_mode_get_connector` (`DRM_IOCTL_MODE_GETCONNECTOR`)
```c
struct drm_mode_get_connector {
    __u64 encoders_ptr;       // -> __u32[] of encoder object IDs
    __u64 modes_ptr;          // -> struct drm_mode_modeinfo[]
    __u64 props_ptr;          // -> __u32[] of property IDs
    __u64 prop_values_ptr;    // -> __u64[] of property values

    __u32 count_modes;
    __u32 count_props;
    __u32 count_encoders;

    __u32 encoder_id;         // current encoder
    __u32 connector_id;
    __u32 connector_type;     // DRM_MODE_CONNECTOR_* (e.g. VIRTUAL = 15)
    __u32 connector_type_id;  // per-type instance number, not an object ID

    __u32 connection;         // enum drm_connector_status
    __u32 mm_width;           // physical width, mm
    __u32 mm_height;          // physical height, mm
    __u32 subpixel;           // enum subpixel_order

    __u32 pad;                // must be zero
};
```
Same two-call (probe-size, then fill) pattern as `drm_mode_card_res`.
`DRM_MODE_CONNECTOR_VIRTUAL = 15` is the natural connector type for a
software-only virtual display (no real physical connector to claim).

### `struct drm_mode_get_encoder` (`DRM_IOCTL_MODE_GETENCODER`)
```c
struct drm_mode_get_encoder {
    __u32 encoder_id;
    __u32 encoder_type;     // DRM_MODE_ENCODER_* (VIRTUAL = 5 exists)

    __u32 crtc_id;

    __u32 possible_crtcs;   // bitmask
    __u32 possible_clones;  // bitmask
};
```

## CRTC

### `struct drm_mode_crtc` (`DRM_IOCTL_MODE_GETCRTC` / `DRM_IOCTL_MODE_SETCRTC`)
```c
struct drm_mode_crtc {
    __u64 set_connectors_ptr;  // -> __u32[] (SETCRTC only)
    __u32 count_connectors;

    __u32 crtc_id;
    __u32 fb_id;                // framebuffer currently/to-be scanned out

    __u32 x;                    // x offset into the framebuffer
    __u32 y;                    // y offset into the framebuffer

    __u32 gamma_size;
    __u32 mode_valid;           // whether `mode` below is set
    struct drm_mode_modeinfo mode;
};
```

### `struct drm_mode_get_plane` (`DRM_IOCTL_MODE_GETPLANE`)
```c
struct drm_mode_get_plane {
    __u32 plane_id;             // IN

    __u32 crtc_id;               // OUT: current CRTC
    __u32 fb_id;                  // OUT: current fb

    __u32 possible_crtcs;         // OUT: bitmask, bit N = CRTC index N
    __u32 gamma_size;              // OUT: never used

    __u32 count_format_types;      // two-call size-probe pattern
    __u64 format_type_ptr;          // -> __u32[] of supported FourCC formats
};
```

### `struct drm_mode_get_plane_res` (`DRM_IOCTL_MODE_GETPLANERESOURCES`)
```c
struct drm_mode_get_plane_res {
    __u64 plane_id_ptr;   // -> __u32[]
    __u32 count_planes;
};
```

### `struct drm_mode_set_plane` (`DRM_IOCTL_MODE_SETPLANE`)
```c
struct drm_mode_set_plane {
    __u32 plane_id;
    __u32 crtc_id;
    __u32 fb_id;      // fb object contains surface format type
    __u32 flags;      // DRM_MODE_PRESENT_TOP_FIELD / _BOTTOM_FIELD

    __s32 crtc_x;     // signed: dest location may be partially off-screen
    __s32 crtc_y;
    __u32 crtc_w;
    __u32 crtc_h;

    __u32 src_x;      // 16.16 fixed point
    __u32 src_y;
    __u32 src_h;
    __u32 src_w;
};
```

## Page flip

### `struct drm_mode_crtc_page_flip` (`DRM_IOCTL_MODE_PAGE_FLIP`)
```c
struct drm_mode_crtc_page_flip {
    __u32 crtc_id;
    __u32 fb_id;
    __u32 flags;       // DRM_MODE_PAGE_FLIP_EVENT (0x01) / _ASYNC (0x02)
    __u32 reserved;    // must be zero
    __u64 user_data;   // echoed back in the vblank event's user_data field
};
```

### `struct drm_mode_crtc_page_flip_target` (extended variant, same ioctl family)
```c
struct drm_mode_crtc_page_flip_target {
    __u32 crtc_id;
    __u32 fb_id;
    __u32 flags;       // adds TARGET_ABSOLUTE (0x4) / TARGET_RELATIVE (0x8)
    __u32 sequence;    // repurposes `reserved`; target vblank sequence
    __u64 user_data;
};
```
Completion notification: userspace `poll()`/`read()`s the DRM device fd
itself; a completed flip appears as a `struct drm_event_vblank` with
`DRM_EVENT_FLIP_COMPLETE` type (definition lives in `drm.h`, not fetched this
pass — see gap note below). This is the same fd-readiness model litebox
already uses for pty/socket fds.

## Relevant flag/constant values seen in this header

```c
#define DRM_DISPLAY_MODE_LEN        32
#define DRM_CONNECTOR_NAME_LEN      32

#define DRM_MODE_CONNECTOR_VIRTUAL  15   // best-fit connector type, no real display
#define DRM_MODE_ENCODER_VIRTUAL    5    // matching encoder type

#define DRM_MODE_FB_INTERLACED      (1<<0)
#define DRM_MODE_FB_MODIFIERS       (1<<1)

#define DRM_MODE_PAGE_FLIP_EVENT    0x01
#define DRM_MODE_PAGE_FLIP_ASYNC    0x02
#define DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE 0x4
#define DRM_MODE_PAGE_FLIP_TARGET_RELATIVE 0x8

// Object-type magic numbers (used in drm_mode_obj_get_properties etc.)
#define DRM_MODE_OBJECT_CRTC       0xcccccccc
#define DRM_MODE_OBJECT_CONNECTOR  0xc0c0c0c0
#define DRM_MODE_OBJECT_ENCODER    0xe0e0e0e0
#define DRM_MODE_OBJECT_MODE       0xdededede
#define DRM_MODE_OBJECT_PROPERTY   0xb0b0b0b0
#define DRM_MODE_OBJECT_FB         0xfbfbfbfb
#define DRM_MODE_OBJECT_BLOB       0xbbbbbbbb
#define DRM_MODE_OBJECT_PLANE      0xeeeeeeee
#define DRM_MODE_OBJECT_ANY        0
```

## Ioctl request numbers (fetched from `include/uapi/drm/drm.h`, `torvalds/linux` master)

Encoding: `DRM_IOCTL_BASE = 'd'` (0x64); `DRM_IOWR(nr, type)` = standard Linux
`_IOWR(DRM_IOCTL_BASE, nr, type)`. Verbatim, confirmed live via fetch (not
guessed):

```c
#define DRM_IOCTL_MODE_GETRESOURCES      DRM_IOWR(0xA0, struct drm_mode_card_res)
#define DRM_IOCTL_MODE_GETCRTC           DRM_IOWR(0xA1, struct drm_mode_crtc)
#define DRM_IOCTL_MODE_SETCRTC           DRM_IOWR(0xA2, struct drm_mode_crtc)
#define DRM_IOCTL_MODE_GETENCODER        DRM_IOWR(0xA6, struct drm_mode_get_encoder)
#define DRM_IOCTL_MODE_GETCONNECTOR      DRM_IOWR(0xA7, struct drm_mode_get_connector)
#define DRM_IOCTL_MODE_CREATE_DUMB       DRM_IOWR(0xB2, struct drm_mode_create_dumb)
#define DRM_IOCTL_MODE_MAP_DUMB          DRM_IOWR(0xB3, struct drm_mode_map_dumb)
#define DRM_IOCTL_MODE_DESTROY_DUMB      DRM_IOWR(0xB4, struct drm_mode_destroy_dumb)
#define DRM_IOCTL_MODE_GETPLANERESOURCES DRM_IOWR(0xB5, struct drm_mode_get_plane_res)
#define DRM_IOCTL_MODE_GETPLANE          DRM_IOWR(0xB6, struct drm_mode_get_plane)
#define DRM_IOCTL_MODE_SETPLANE          DRM_IOWR(0xB7, struct drm_mode_set_plane)
#define DRM_IOCTL_MODE_ADDFB2            DRM_IOWR(0xB8, struct drm_mode_fb_cmd2)
#define DRM_IOCTL_MODE_PAGE_FLIP         DRM_IOWR(0xB0, struct drm_mode_crtc_page_flip)
```

`_IOWR` itself packs `(dir=3) | (size << 16) | (type='d' << 8) | nr` per the
standard Linux ioctl encoding (`asm-generic/ioctl.h`) -- litebox's ioctl
dispatch must decode the raw ioctl number the guest passes into these same
`(type, nr, size, dir)` fields to route correctly, exactly as it already does
for `TCGETS`/`FIONBIO` etc.

## Page-flip completion event format (from `drm.h`)

```c
struct drm_event {
    __u32 type;
    __u32 length;
};

struct drm_event_vblank {
    struct drm_event base;
    __u64 user_data;
    __u32 tv_sec;
    __u32 tv_usec;
    __u32 sequence;
    __u32 crtc_id;  // 0 on older kernels that don't support this
};

#define DRM_EVENT_VBLANK        0x01
#define DRM_EVENT_FLIP_COMPLETE 0x02
```
A completed page-flip appears as a `read()` on the DRM device fd returning a
`struct drm_event_vblank` with `base.type == DRM_EVENT_FLIP_COMPLETE`,
`base.length == sizeof(struct drm_event_vblank)`, and `user_data` echoing
whatever the flip request's own `user_data` field carried.

## Remaining gaps — not needed for the dumb-buffer-only first slice

- **DRM_FORMAT_* FourCC constants** (`drm_fourcc.h`, not fetched) — only
  `DRM_FORMAT_XRGB8888` is needed for a first slice; its value is the
  standard fourcc-code encoding (`'X'|'R'<<8|'G'<<16|'B'<<24`), not
  independently confirmed via fetch this pass.
- **GEM-generic ioctls** (`DRM_IOCTL_GEM_CLOSE` = `DRM_IOW(0x09, struct
  drm_gem_close)`, confirmed present in the same `drm.h` fetch) — needed for
  fully-correct buffer teardown; deferred to a later pass since a dumb
  buffer's handle can be freed via `DRM_IOCTL_MODE_DESTROY_DUMB` alone for
  this first slice's purposes.
