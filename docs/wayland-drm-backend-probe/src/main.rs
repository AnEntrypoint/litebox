//! Guest-side probe: proves `smithay`'s DRM backend (feature `backend_drm` only, no
//! `backend_udev`/`backend_session`/`backend_gbm`) can drive litebox's virtual DRM device using
//! nothing but a plain `open("/dev/dri/card0")` fd -- the exact guest-syscall model litebox uses
//! for every other subsystem. Not a full compositor: proves the backend layer alone works before
//! any Wayland protocol wiring is attempted (see PRD row `gui-wayland-compositor-on-drm-future`).
//!
//! Empirically confirmed, not just read from `Cargo.toml`: an earlier draft of this file
//! referenced `smithay::backend::session::Event` and got a real compile error ("found an item
//! that was configured out ... gated behind the `backend_session` feature") -- direct proof
//! `backend_session` truly is not pulled in transitively by `backend_drm` alone. `cargo tree
//! --target x86_64-unknown-linux-musl` also has zero matches for `udev`/`libseat`/`gbm`.
//!
//! **Linked and RUN-verified for real, closing the ceiling described below** (a later pass, same
//! project): `pip install ziglang` (a self-contained portable Zig, including its own
//! musl-targeting cross-linker) plus `cargo install cargo-zigbuild` gives a genuine
//! `x86_64-unknown-linux-musl` LINKER on a plain Windows host with no WSL/MSYS toolchain needed --
//! `cargo zigbuild --target x86_64-unknown-linux-musl` produces a real static-PIE ELF binary. The
//! only remaining snag: `backend_drm` pulls in `xkbcommon` unconditionally (see `Cargo.toml`'s own
//! dependency graph), which FFI-binds real system libxkbcommon rather than a pure-Rust/dlopen
//! implementation -- resolved by fetching Alpine's own prebuilt musl static archive directly
//! (`https://dl-cdn.alpinelinux.org/alpine/edge/main/x86_64/libxkbcommon-static-*.apk`, a plain
//! gzipped tar, no apk tooling needed to extract `usr/lib/libxkbcommon.a`) and pointing
//! `RUSTFLAGS`/a target `-L` search path at it. The resulting binary was appended to a copy of the
//! project's own known-good `alpine-rootfs.tar` and run as a REAL guest process under
//! `litebox_runner_linux_on_windows_userland.exe --gui`: it printed `connectors=1 crtcs=1
//! encoders=1`, correctly matching litebox's virtual DRM device -- the strongest possible
//! verification of this whole approach, not just a type-check. (This run also surfaced and led to
//! fixing two real litebox bugs unrelated to Smithay itself: a missing `DRM_IOCTL_MODE_
//! OBJ_GETPROPERTIES`/`GETPROPERTY` pair every real libdrm client calls right after
//! `GETCONNECTOR` -- see `litebox_shim_linux::syscalls::drm` -- and a `--gui`-only stack overflow
//! on the GUI presenter's own background thread, unrelated to guest ELF loading -- see
//! `litebox_runner_linux_on_windows_userland`'s `PRESENTER_THREAD_STACK_SIZE`.)
//!
//! Original verification ceiling (superseded above, kept for context): `cargo check --target
//! x86_64-unknown-linux-musl` only. This host has no musl cross-linker (confirmed: no
//! `musl-gcc`/`x86_64-linux-musl-gcc` on PATH, no `~/.cargo/config.toml` target linker override,
//! WSL2 present but not set up as a working build environment) -- `cc` falls back to MinGW's own
//! `x86_64-w64-mingw32` linker, which rejects musl's `-static-pie`/`--eh-frame-hdr` flags.

use smithay::backend::drm::{DrmDevice, DrmDeviceFd};
use smithay::reexports::drm::control::Device as _;
use smithay::reexports::rustix::fs::{open, Mode, OFlags};

fn main() {
    // Ordinary guest syscall -- no udev, no session manager, matching every other litebox
    // subsystem's guest-syscall-against-emulated-backend model. `open` already returns a real
    // `OwnedFd` (rustix's own type), no manual fd juggling needed.
    let fd = open("/dev/dri/card0", OFlags::RDWR, Mode::empty()).expect("open /dev/dri/card0");
    let drm_fd = DrmDeviceFd::new(fd.into());

    let (device, _notifier) =
        DrmDevice::new(drm_fd, true).expect("DrmDevice::new against litebox's virtual DRM device");

    let resources = device
        .resource_handles()
        .expect("get_resources (DRM_IOCTL_MODE_GETRESOURCES)");
    println!(
        "connectors={} crtcs={} encoders={}",
        resources.connectors().len(),
        resources.crtcs().len(),
        resources.encoders().len(),
    );
}
