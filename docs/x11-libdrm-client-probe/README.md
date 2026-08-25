# X11/libdrm client probe

Real end-to-end proof that an ordinary, unmodified libdrm-based C program (the
actual client shape a guest-side X server / SDL2 KMSDRM / Qt eglfs would use,
per the `gui-x11-server-on-drm-future` PRD row's own conclusion) works against
litebox's virtual `/dev/dri/card0` -- built with a *real* cross-compiler and
linked against the *real* `libdrm` library, not a hand-rolled `fcntl.ioctl`
simulation.

## Result (2026-08-25, live-witnessed)

```
OPEN_OK fd=3
VERSION name=litebox date=20260101 desc=litebox virtual DRM/KMS device major=1 minor=0 patch=0
GET_CAP(DUMB_BUFFER) rc=0 value=1
SET_MASTER rc=0
RESOURCES count_connectors=1 count_encoders=1 count_crtcs=1 count_fbs=0
CONNECTOR id=1 connection=1 count_modes=1 width_mm=0 height_mm=0
CRTC id=3 buffer_id=0 width=0 height=0 mode_valid=0
CREATE_DUMB rc=0 handle=1 pitch=256 size=16384
ADDFB2 rc=0 fb_id=1
MAP_DUMB rc=0 offset=4096
MMAP_WRITE_OK addr=0x410000 size=16384
SETCRTC rc=0
DROP_MASTER rc=0
ALL_OK
```

Every real libdrm call in the pipeline succeeded: `drmOpen` (via a plain
`open()`), `drmGetVersion`, `drmGetCap`, `drmSetMaster`, `drmModeGetResources`,
`drmModeGetConnector`, `drmModeGetCrtc`, `drmModeCreateDumbBuffer`,
`drmModeAddFB2`, the `MAP_DUMB` + real `mmap()` write, `drmModeSetCrtc`,
`drmDropMaster`.

**One real gap found along the way**: the legacy `drmModeAddFB` (the
non-planar `DRM_IOCTL_MODE_ADDFB` ioctl, distinct from `ADDFB2`) is not
implemented by `litebox_shim_linux/src/syscalls/drm.rs` -- only `ADDFB2` is.
This probe uses `ADDFB2` (the modern, format-explicit call every current
libdrm-based client prefers anyway), so it isn't a blocker, but a client that
insists on the legacy `ADDFB` ioctl specifically would fail with `EINVAL`. Not
implemented here since no such client is known to require it and `ADDFB2` is
the objectively better API to have client-facing code depend on.

## Why this matters

Two prior research passes concluded the right X11 architecture is a
GUEST-side libdrm client, not a host-side protocol server, and closed a
concrete ioctl gap (`VERSION`/`GET_CAP`/`SET_MASTER`/`DROP_MASTER`, commit
`8609a97`) verified only via a Python `fcntl.ioctl` script -- a real
simulation, but not proof the *actual* `libdrm` C library (its own struct
packing, its own two-call size-probe patterns, its own error-handling
conventions) works correctly against litebox's ioctl implementations. This
probe closes that gap: real `libdrm.so`, real cross-compiled musl ELF, real
guest execution.

## Reproduction recipe

This Windows host has no native musl-gcc/x86_64-linux-musl-gcc, and
`litebox_packager`'s host mode (ELF dependency discovery + syscall rewriting)
is Linux-only (`#[cfg(target_os = "linux")]` in `litebox_packager/src/lib.rs`)
-- so the actual pipeline that worked is:

1. **Get real libdrm + kernel headers**: fetch Alpine's `libdrm`,
   `libdrm-dev`, and `linux-headers` `.apk` packages directly (they're just
   gzip'd tars) from `https://dl-cdn.alpinelinux.org/alpine/edge/main/x86_64/`
   and extract `usr/include/{xf86drm.h,xf86drmMode.h,libdrm/*.h,linux/*}` and
   `usr/lib/libdrm.so.2.134.0`.

2. **Build a musl sysroot**: extract `usr/include`, `usr/lib`, `lib` from
   `litebox-windows-alpine/alpine-rootfs.tar` (it already has a real Alpine
   gcc/musl toolchain's headers, `crt*.o`, `ld-musl-x86_64.so.1`, `libc.so`)
   and layer the libdrm/kernel headers from step 1 on top.

3. **Cross-compile with `clang -target x86_64-linux-musl --sysroot=<sysroot>
   -fuse-ld=lld`** (this host's LLVM/clang+lld, from `scoop install llvm`,
   handles the actual compile/link with no native musl-gcc needed).
   Two real gotchas hit along the way:
   - This host's `ld.lld` build lacks zlib support and cannot read the
     zlib-compressed `.debug_*` sections Alpine's `crt1.o`/`Scrt1.o`/
     `libgcc.a`/etc ship with -- strip them first with `llvm-objcopy
     --strip-debug` into a parallel directory, and point `-B`/`-L` at the
     stripped copies (crt objects are picked up via `-B`'s search path, not
     just `-L`).
   - **Git Bash silently mangles a `-Wl,-dynamic-linker,/lib/ld-musl-x86_64.so.1`
     argument into an absolute Windows path** (`C:/Program Files/Git/lib/...`)
     before it reaches the linker -- this got baked into the binary's own
     `PT_INTERP` and caused a confusing host-side `ENOENT` panic in the
     *runner* (`litebox_runner_linux_on_windows_userland::lib.rs`'s
     `load_program(...).unwrap()`) that looked like a missing-file problem but
     was actually the interpreter path being wrong. Fix: prefix the whole
     `clang` invocation with `MSYS2_ARG_CONV_EXCL="*"` (matches this project's
     established Git-Bash-mangling gotchas elsewhere -- see the tar
     `--owner=0 --group=0` note in project memory).

4. **Rewrite every ELF that will run as a guest** (the test binary itself,
   AND every shared library it links against that makes its own syscalls --
   `libdrm.so.2`, `ld-musl-x86_64.so.1`, `libgcc_s.so.1`) with
   `litebox_syscall_rewriter` (built via `cargo build --release -p
   litebox_syscall_rewriter --bin litebox_syscall_rewriter --features
   std,anyhow,clap` -- the default feature set doesn't include the binary).
   `litebox_packager`'s host mode would normally do this + dependency
   discovery automatically, but it's Linux-only on this host; the rewriter
   binary itself has no such OS gate and works fine standalone.

5. **Append the rewritten binary + rewritten shared libs into a copy of
   `alpine-rootfs.tar`** (`tar --owner=0 --group=0 -rf`, matching this
   session's established tar-append convention -- a later entry shadows an
   earlier one at the same path when the tar is read) at `/lib/` and
   `/usr/lib/` to override the stock (un-rewritten) copies already in the
   rootfs.

6. **Run**: `litebox_runner_linux_on_windows_userland.exe --initial-files
   <rootfs.tar> -- tmp/drmtest-musl3` (no leading `/` on the program path,
   per this project's own established CLI-path gotcha).

## Files here

- `drmtest.c` -- the actual C source. Calls the full pipeline listed above.
  No `#ifdef`s or litebox-specific code -- this is exactly what an ordinary
  guest-side X server / SDL2 KMSDRM backend / Qt eglfs driver would call.

The sysroot, downloaded `.apk` packages, and intermediate build artifacts are
NOT checked in (reconstructible from the recipe above; keeping them out avoids
bloating the repo with a musl libc + kernel headers copy).
