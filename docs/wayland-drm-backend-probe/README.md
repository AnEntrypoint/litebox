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

**Phase 4** (`src/client.rs`/`src/combined.rs`, commit `05b386d`) built a REAL
`wayland-client`-based client (not a hand-rolled protocol simulation) and got
it genuinely `CONNECTED` + the compositor `CLIENT_ACCEPTED` (the raw
Unix-socket handshake works) before hitting a real litebox gap:
`litebox_shim_linux::syscalls::net`'s `do_sendmsg`/`do_recvmsg` unconditionally
rejected ANY `msg_controllen != 0` (`SCM_RIGHTS` ancillary data) with `EINVAL`
-- not Wayland-specific, any guest program passing fds over a Unix socket hits
this.

**Phase 5** (this pass) implements genuine `SCM_RIGHTS` fd-passing:
`litebox_shim_linux/src/syscalls/unix.rs`'s `Message` now carries an
`AnyDupFd` batch (one variant per litebox fd-enabled subsystem, since a raw
cmsg `int` fd has no type information of its own) delivered atomically with
the message's own first byte, exactly matching real Linux semantics; the
sender resolves + `Descriptors::duplicate`s each donated fd via the same
`run_on_raw_fd` dispatch `dup()`/`fork()` already use, and the receiver
inserts each into ITS OWN fd table via the same `insert_raw_fd` primitive
`socketpair()` already uses. `MSG_CMSG_CLOEXEC` is honored end-to-end.
`net.rs`'s `sys_recvmsg`/`unix.rs`'s `UnixSocket::recvmsg` both needed their
`supported_flags` masks widened to actually accept this flag once it started
being genuinely honored (`wayland-client`'s own `rcv_msg` always sets it,
independent of whether it expects fds on that particular read).

**Live-verified against this exact probe, independently re-verified twice**
(a baseline re-test with the fix reverted reproduced the OLD failure --
immediate `EINVAL` right after `CLIENT_ACCEPTED` -- confirming the fix is
what changed the behavior, not something else): with the fix, the client's
FIRST `recvmsg` (reading the compositor's reply to `get_registry`) now
genuinely succeeds -- 24 real bytes delivered, `MSG_CMSG_CLOEXEC` accepted,
no `EINVAL` -- real forward progress past where every prior pass stopped.

**A second, SEPARATE, pre-existing gap was isolated (not fixed) in getting
this far**: after that first successful exchange, the compositor's
`calloop` event loop never processes anything more, timing out 20s later
even though the client has follow-up requests queued. Root-caused via a
temporary diagnostic on `EpollFile::check_io_events` (phase 3's own nested-
epoll fix): it is called exactly ONCE more after the first exchange (going
`empty=false` then `empty=true`), then NEVER AGAIN for the remaining ~19.5s
despite `event_loop.dispatch()` being called roughly 200 times (every
100ms) -- the nested epoll's readiness is checked once, correctly reflects
"nothing pending" at that instant, and is then never re-checked even though
`calloop` keeps calling `dispatch()`. This means phase 3's fix answers "is
the inner epoll ready right now" correctly, but something in how `calloop`'s
own polling loop re-arms/re-registers its interest in the nested epoll fd
across repeated `dispatch()` calls isn't triggering a fresh check -- worth
investigating `register_observer`'s interaction with repeated `poll()` calls
using a FRESH observer each time (real `epoll_wait` semantics: readiness is
re-evaluated from scratch on every call, not just the first). This is
NOT a regression from phase 5 -- confirmed via the same before/after
comparison above, phase 5's fix change is what let the client get far enough
to exercise this path at all; it was structurally unreachable before.
**Concrete next step**: trace `calloop`'s own `Poll`/registration lifecycle
against litebox's `PollSet`/`Observer` registration to find where a repeated
`dispatch()` call fails to re-arm interest in an already-registered nested
epoll fd.

**Phase 6 (this pass): the basic nested-epoll re-arming mechanism itself was
tested directly and RULED OUT as the cause** (`litebox_shim_linux/src/
syscalls/epoll.rs`'s new `test_nested_epoll_readiness_rechecked_across_
separate_waits` test, kept as a permanent regression test). Built a minimal,
self-contained reproduction using the crate's own existing `TestPlatform`
test harness (no musl/zig/guest-process pipeline needed): one `EpollFile`
nested inside another via `EPOLL_CTL_ADD` (exactly `calloop`'s own pattern),
an eventfd registered on the inner epoll, and TWO SEPARATE `wait()` calls
(matching real `epoll_wait()`/`dispatch()` semantics -- not one continuous
wait) with the eventfd fired fresh between them. **This passes cleanly**:
the outer epoll correctly observes the nested epoll's readiness on BOTH the
first AND the second, independent wait call. This rules out the simplest
hypothesis (a fundamentally broken observer re-registration or re-arming
mechanism in the core nested-epoll code from phase 3) -- the basic
mechanism genuinely works for a plain eventfd source across repeated,
separate waits, with or without draining the inner epoll's own ready queue
in between.

**This means the real bug is more specific to calloop's actual usage
pattern** than the core mechanism -- candidates not yet tested: (a) whether
calloop issues `EPOLL_CTL_MOD` (not just the original `ADD`) to re-arm its
own interest each cycle, and whether `mod_interest`'s observer
re-registration has a subtle difference from `add_interest`'s; (b) whether
the specific fd kind that becomes ready inside the real compositor's inner
epoll (a `Unix` socket fd, not a plain eventfd) has a readiness-reporting
quirk `EpollDescriptor::poll`'s `Unix` arm doesn't share with `Eventfd`;
(c) a genuine timing/threading race specific to the real combined
client+compositor-on-separate-threads process shape that a single-threaded
unit test can't reproduce. Whoever continues this should extend the new
test to substitute a `Unix` socket fd for the eventfd (closer to the real
compositor's shape) before re-attempting a live guest-process reproduction.

**Phase 7 (this pass): hypothesis (b), the `Unix`-socket-specific readiness
path, was tested directly and ALSO RULED OUT** (`litebox_shim_linux/src/
syscalls/epoll.rs`'s new
`test_nested_epoll_readiness_rechecked_across_separate_waits_unix_socket`
test, kept as a second permanent regression test alongside phase 6's
eventfd version). Same exact scenario as phase 6, with a real Unix
socketpair (`UnixSocket::new_connected_pair`, `EpollDescriptor::Unix`) as
the inner epoll's ready source instead of an eventfd: nest the receiving
socket inside the inner epoll via `EPOLL_CTL_ADD`, TWO SEPARATE `wait()`
calls with a fresh `sendto` between them (not draining via the inner
epoll's own `wait()`, mirroring how calloop itself reads its own fds
directly). **This also passes cleanly** -- the outer epoll correctly
observes the nested epoll's readiness via the socket on both the first and
second, independent wait call. Validated as a real, sensitive test (not a
false-passing one) by temporarily sabotaging `EpollFile::check_io_events`
to always return `Events::empty()` and confirming BOTH the eventfd and
socket variants then hang (blocking on a readiness event that never
arrives) rather than silently passing -- reverted immediately after
confirming, no code left in this state.

**Two of the three candidates from phase 6 are now ruled out with hard
evidence, and the third (`EPOLL_CTL_MOD`) is now also structurally
unlikely.** Read `litebox_shim_linux/src/syscalls/epoll.rs`'s `mod_interest`
directly (the `EPOLL_CTL_MOD` handler) side-by-side with `add_interest`
(the `EPOLL_CTL_ADD` handler, already exercised cleanly by both passing
tests): both call the exact same `file.poll(global, mask,
Some(observer_weak))` re-registration step, with `mod_interest` additionally
just updating the stored mask/flags/data first -- no structural difference
in HOW the observer gets re-registered between `ADD` and `MOD`. This makes
candidate (a) unlikely to be the real cause (not fully eliminated without
tracing real `calloop` source or a live `EpollOp`-logging diagnostic against
the combined probe, but no longer the most promising lead).

**The remaining, now-primary suspect is candidate (c): a genuine multi-thread
timing race** specific to the real client-and-compositor-on-separate-threads
shape (`src/combined.rs`) that no single-threaded unit test (both of this
pass's tests, and phase 6's, ran everything on one thread) can reproduce
structurally. Whoever continues this should either (1) write a genuinely
multi-threaded unit test -- two real `std::thread`s independently driving
the outer `wait()` calls and the socket writes concurrently, with realistic
interleaving/timing, rather than this session's fully sequential
single-thread tests -- or (2) trace `combined.rs`'s exact thread-handoff
points live (a temporary diagnostic logging exactly when the compositor
thread's `dispatch()` calls happen relative to the client thread's writes)
to look for a genuine missed-wakeup window between the two.

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
