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

**Phase 2** extends the same crate with `smithay`'s `wayland_frontend`
feature -- a genuinely minimal Wayland COMPOSITOR (real Unix socket,
`wl_compositor`+`wl_shm` globals, copies a committed client buffer's pixels
into a DRM dumb buffer via the already-proven phase-1 backend).
`wayland_frontend` compiles AND links cleanly for musl with the same recipe
(empirically confirmed: it pulls in `wayland-server`/`wayland-backend`'s own
pure-Rust wire-protocol implementation, NOT `wayland-backend/server_system`'s
real `libwayland-server.so` FFI binding -- zero new native-linking
dependencies). Run as a real guest process, it genuinely binds the socket and
prints `LISTENING path=/tmp/litebox-wayland-0` -- and previously hit a REAL,
previously-undiscovered litebox gap right after: `calloop`'s epoll backend
nests an epoll fd inside another epoll set, which
`litebox_shim_linux::syscalls::epoll::EpollDescriptor::poll`'s
`EpollDescriptor::Epoll(_file) => unimplemented!()` arm panicked on outright.

**Phase 3** (this pass) implements the fix: `EpollFile` now implements
`litebox::event::IOPollable` (`register_observer` delegates to the epoll's
own `ready.pollee`; `check_io_events` reports `IN` whenever `ready.entries`
is non-empty), and `EpollDescriptor::poll`'s `Epoll` arm calls it via the
same `entry_handle`/`with_entry` pattern every other fd kind already uses --
no separate readiness or wakeup machinery needed, since the inner epoll's
own `ready` set is exactly what a direct `epoll_wait` caller already polls,
so an outer epoll registering an observer there is woken by precisely the
same `ReadySet::push`/`notify_observers` call a direct waiter would receive.
Live-verified against this exact probe: re-running it now prints `LISTENING`
-> `RUNNING` (the line immediately after registering the nested-epoll
`calloop` source -- previously unreachable) and completes its full
30-second `event_loop.dispatch()` loop with zero panic, correctly printing
`NO_CLIENT_COMMIT_WITHIN_TIMEOUT` and exiting 1 (no real Wayland client was
available in this environment to actually connect and commit a buffer --
that remains the concrete next step for whoever has one).

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

**Phase 2/3 (this file's current `main.rs`, wayland_frontend added, nested
epoll fixed)** prints `LISTENING path=/tmp/litebox-wayland-0` then `RUNNING`,
runs its full 30-second `event_loop.dispatch()` loop with zero panic, and
prints `NO_CLIENT_COMMIT_WITHIN_TIMEOUT` (exit 1) once no real client
connects within the bound -- the current, real verification ceiling: the
nested-epoll gap that previously blocked this is fixed and live-verified,
but no real Wayland client is available in this environment to actually
connect and exercise `Compositor::commit`/`push_to_drm_dumb_buffer`.

The sysroot/downloaded packages/intermediate build artifacts are not checked
in (same convention as the sibling `docs/x11-libdrm-client-probe/`) -- fully
reconstructible from the recipe above, independently re-verified through step
4 (the link) by re-running it from scratch, and through step 6 (the guest
run) by reproducing the exact `LISTENING` then nested-epoll-panic sequence.
