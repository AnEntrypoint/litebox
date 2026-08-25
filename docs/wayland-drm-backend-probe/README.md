# Wayland/Smithay DRM backend probe

Deliberately **not** a workspace member (has its own `[workspace]` table in
`Cargo.toml`) -- this is a standalone reference crate, not a shipped feature.

**Phase 1** (commits `ec2e0ae`, `0778cb2`) answered: does `smithay`'s DRM
backend, built with **only** the `backend_drm` feature (no
`backend_udev`/`backend_session`/`backend_gbm`), actually compile, link, and
run against litebox's virtual DRM device? **Yes**, confirmed all the way to a
real linked binary run as a real guest process -- see `src/main.rs`'s own doc
comment for the full history and the two real litebox bugs this surfaced and
fixed (`DRM_IOCTL_MODE_OBJ_GETPROPERTIES`/`GETPROPERTY`, and a `--gui`
presenter-thread stack overflow in debug builds).

**Phase 2** (this pass) extends the same crate with `smithay`'s
`wayland_frontend` feature -- a genuinely minimal Wayland COMPOSITOR (real
Unix socket, `wl_compositor`+`wl_shm` globals, copies a committed client
buffer's pixels into a DRM dumb buffer via the already-proven phase-1
backend). `wayland_frontend` compiles AND links cleanly for musl with the
same recipe (empirically confirmed: it pulls in `wayland-server`/
`wayland-backend`'s own pure-Rust wire-protocol implementation, NOT
`wayland-backend/server_system`'s real `libwayland-server.so` FFI binding --
zero new native-linking dependencies). Run as a real guest process, it
genuinely binds the socket and prints `LISTENING path=/tmp/litebox-wayland-0`
-- but then hits a REAL, previously-undiscovered litebox gap: `calloop`'s
epoll backend nests an epoll fd inside another epoll set, which
`litebox_shim_linux::syscalls::epoll::EpollDescriptor::poll`'s
`EpollDescriptor::Epoll(_file) => unimplemented!()` arm panics on outright
(confirmed via a live backtrace). This is real, non-trivial work (correct
nested-epoll semantics touch shared polling machinery every litebox
subsystem depends on) -- not attempted as a rushed fix here. Client-connect
and pixel-commit verification remain blocked on this until nested epoll
support lands.

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
5. Rewrite it with `litebox_syscall_rewriter` (`cargo build --release -p
   litebox_syscall_rewriter --bin litebox_syscall_rewriter --features
   std,anyhow,clap`, then `litebox_syscall_rewriter <bin> -o <bin>.hooked` --
   no shared-lib rewriting needed, this binary is statically linked).
6. Append the `.hooked` binary into a copy of the project's
   `alpine-rootfs.tar` (`tar --owner=0 --group=0 -rf`) at the path it will run
   from (e.g. `tmp/wayland-probe`) and run it as a real guest process under
   `litebox_runner_linux_on_windows_userland.exe --initial-files <tar> --
   tmp/wayland-probe`.

**Phase 1 (backend_drm only, no wayland_frontend)** prints `connectors=1
crtcs=1 encoders=1` and exits cleanly.

**Phase 2 (this file's current `main.rs`, wayland_frontend added)** prints
`LISTENING path=/tmp/litebox-wayland-0` then panics on litebox's nested-epoll
gap (see above) before a client can connect -- this is the current, real
verification ceiling for phase 2, not a link/compile failure.

The sysroot/downloaded packages/intermediate build artifacts are not checked
in (same convention as the sibling `docs/x11-libdrm-client-probe/`) -- fully
reconstructible from the recipe above, independently re-verified through step
4 (the link) by re-running it from scratch, and through step 6 (the guest
run) by reproducing the exact `LISTENING` then nested-epoll-panic sequence.
