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
//! Verification ceiling in this environment: `cargo check --target x86_64-unknown-linux-musl`
//! only. This host has no musl cross-linker (confirmed: no `musl-gcc`/`x86_64-linux-musl-gcc` on
//! PATH, no `~/.cargo/config.toml` target linker override, WSL2 present but not set up as a
//! working build environment) -- `cc` falls back to MinGW's own `x86_64-w64-mingw32` linker,
//! which rejects musl's `-static-pie`/`--eh-frame-hdr` flags. `cargo check` still fully resolves
//! and type-checks the real dependency graph AND this file's real Smithay API calls
//! (`DrmDeviceFd::new`/`DrmDevice::new`/`resource_handles`) under the real target triple --
//! linking and live guest-process verification remain for whoever next has a real
//! musl-linking Linux host.

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
