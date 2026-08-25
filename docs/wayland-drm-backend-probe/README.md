# Wayland/Smithay DRM backend probe

Deliberately **not** a workspace member (has its own `[workspace]` table in
`Cargo.toml`) -- this is a standalone reference crate, not a shipped feature.
It exists to answer one concrete question from PRD row
`gui-wayland-compositor-on-drm-future`: does `smithay`'s DRM backend, built
with **only** the `backend_drm` feature (`default-features = false`, no
`backend_udev`/`backend_session`/`backend_gbm`), actually compile for a musl
target and drive litebox's virtual DRM device using nothing but a plain
`open("/dev/dri/card0")` fd -- the same guest-syscall model litebox uses for
every other subsystem?

**Answer: yes**, confirmed all the way to a real linked binary run as a real
guest process against litebox's virtual DRM device -- see `src/main.rs`'s own
doc comment for the full verification history and the two real litebox bugs
this surfaced and fixed (`DRM_IOCTL_MODE_OBJ_GETPROPERTIES`/`GETPROPERTY`, and
a `--gui` presenter-thread stack overflow in debug builds).

## Reproducing the type-check only

```sh
cd docs/wayland-drm-backend-probe
cargo check --target x86_64-unknown-linux-musl
```

## Reproducing the real link (independently re-verified; this Windows host has
## no native musl-gcc, so a Zig-based cross-linker is used instead)

1. `pip install ziglang` (a self-contained portable Zig, including its own
   musl-targeting cross-linker) and `cargo install cargo-zigbuild`.
2. `cargo-zigbuild` needs a `zig` executable on `PATH` (ziglang ships as a
   Python module, `python -m ziglang`, not a bare `zig.exe`) -- create a small
   `zig.bat`/`zig` shim on `PATH` that forwards to `python -m ziglang %*`.
3. `backend_drm` unconditionally pulls in `xkbcommon`, which FFI-binds a real
   system `libxkbcommon` rather than a pure-Rust/dlopen implementation. Fetch
   Alpine's prebuilt musl static archive directly (a plain gzipped tar, no
   `apk` tooling needed):
   `https://dl-cdn.alpinelinux.org/alpine/edge/main/x86_64/libxkbcommon-static-<version>.apk`
   (check the index at `.../x86_64/` for the current version), extract
   `usr/lib/libxkbcommon.a`, and set `RUSTFLAGS="-L <dir containing the .a>"`.
4. `cargo zigbuild --target x86_64-unknown-linux-musl` produces a real
   statically-linked ELF64 binary at
   `target/x86_64-unknown-linux-musl/debug/wayland-probe`.
5. Append the binary into a copy of the project's `alpine-rootfs.tar` (see
   this session's own established tar-append convention: `tar --owner=0
   --group=0 -rf`) and run it as a real guest process under
   `litebox_runner_linux_on_windows_userland.exe --gui` -- it prints
   `connectors=1 crtcs=1 encoders=1`.

The sysroot/downloaded packages/intermediate build artifacts are not checked
in (same convention as the sibling `docs/x11-libdrm-client-probe/`) -- fully
reconstructible from the recipe above, independently re-verified through step
4 (the link) by re-running it from scratch.
