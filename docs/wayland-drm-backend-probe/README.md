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

## Phase 4: a real Wayland CLIENT (`src/client.rs`, `src/combined.rs`) --
## partial success, hit a real, precisely-identified litebox gap

`src/client.rs` is a genuine `wayland-client`-based client (not a protocol
simulation): connects to the compositor's socket, binds
`wl_compositor`+`wl_shm`, allocates a real `memfd_create`-backed shm pool,
attaches a 4x4 XRGB8888 buffer, and commits -- mirroring `docs/x11-libdrm-
client-probe/drmtest.c`'s "ordinary, unmodified real client" shape. It
compiles and links cleanly for musl via the same zig recipe (zero new native
dependencies).

**Getting the client and compositor to run TOGETHER hit an unrelated,
pre-existing litebox blocker**: litebox's runner only launches one top-level
guest process, so a `fork()`+`execv()`-based launcher was tried first --
this crashed the forked child with SIGSEGV every time (confirmed via an
isolated minimal repro: a trivial `fork()`+`execv()`+`waitpid()` sequence
with NO Wayland/Smithay code at all still dies the same way, with both real
`fork()` and `vfork()`). This is PRD row `fork-execve-mallocng-null-meta-
crash` -- litebox's own deepest, most extensively multi-session-investigated
open bug (a musl mallocng null-pointer-deref on `fork()`+`execve()`),
previously characterized around a CPython repro; this probe's isolated,
Rust-only, zero-CPython repro is new evidence the crash is a genuinely
general `fork`+`exec` pattern, not CPython/mallocng-specific in the narrow
sense.

**Worked around by NOT forking at all**: `src/combined.rs` runs the
compositor's event loop on a background `std::thread` and the client on the
main thread, both in ONE process (matching how `--gui`'s own presenter
thread already works) -- no `fork()`/`execv()` anywhere. This got real,
new-territory results: the client genuinely `CONNECTED`, and the compositor
genuinely printed `CLIENT_ACCEPTED` -- the Unix-socket handshake itself
works correctly. It then failed on the client's first real protocol
round-trip:

```
Io error: Invalid argument (os error 22)
thread 'main' panicked at src\combined.rs:...:
roundtrip: registry: Backend(Io(Os { code: 22, kind: InvalidInput, message: "Invalid argument" }))
```

**Root cause, precisely identified** (`litebox_shim_linux/src/syscalls/
net.rs`): `do_sendmsg`/`do_recvmsg` both explicitly reject any `sendmsg`/
`recvmsg` call whose `msg_controllen != 0` --
`log_unsupported!("ancillary data is not supported"); return Err(Errno::
EINVAL)`. Wayland's wire protocol relies on ancillary-data `SCM_RIGHTS`
fd-passing for exactly this kind of request (`wl_shm.create_pool` sends the
pool's memfd as an ancillary-data fd, not as protocol bytes) -- `wayland-
client`'s very first real request after the registry bind hits this
unconditionally-rejected path. This is NOT specific to Wayland: any real
guest program passing fds over a Unix socket (a common, general Linux
pattern -- systemd-style socket activation, container runtimes, X11's own
fd-passing for some extensions, D-Bus) would hit the identical `EINVAL`.

**Not fixed in this pass, precisely scoped for whoever picks it up**: real
`SCM_RIGHTS` support needs (1) parsing `cmsghdr` structures out of the
guest's control buffer on send, (2) translating each ancillary fd from the
SENDING process's descriptor-table entry into a duplicated reference the
Unix-socket `file` object can carry alongside its byte payload (a real data-
plane addition -- `UnixFile`'s send/receive path currently only carries
bytes), and (3) on receive, materializing a NEW fd in the RECEIVING
process's descriptor table and writing the resulting fd number back into the
guest's `cmsghdr` buffer. Getting fd lifetime/ownership wrong here causes
descriptor leaks or use-after-close -- genuinely safety-critical shared
infrastructure, not a quick patch, and out of scope for a single pass on top
of everything else this row has already covered.

**Reproducing this phase**: `src/client.rs` and `src/combined.rs` both build
with `cargo zigbuild --target x86_64-unknown-linux-musl --bin <name>` (same
recipe as above). `wayland-combined` run as a real guest process reproduces
the `CONNECTED`/`CLIENT_ACCEPTED` success and the `sendmsg`/`EINVAL` failure
directly -- no launcher/fork needed for this repro since it's single-process.
