// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Virtual terminal (VT) device ioctl surface (`/dev/tty0`, `/dev/tty1`).
//!
//! Exists to satisfy `seatd`'s own VT-management handshake (see `common/terminal.c`/
//! `seatd/seat.c` in the real `seatd` source, `github.com/kennylevinsen/seatd`), which every
//! real Linux seat/session daemon performs before granting a connected client (e.g. `weston`)
//! access to a DRM device: `seat_update_vt` opens `/dev/tty0` and calls `VT_GETSTATE` to learn
//! which numbered VT is currently active, then `seat_open_client`/`vt_open` opens that specific
//! `/dev/tty<N>` and calls `VT_SETMODE`/`KDSKBMODE`/`KDSETMODE` to claim it before marking the
//! client active. Without a real `/dev/tty0`/`/dev/tty<N>` device at all, `seatd` fails this
//! handshake outright (`Could not open target tty: No such file or directory`) and never grants
//! DRM access, regardless of how correct the DRM device itself is.
//!
//! Litebox has no real console hardware and no real multi-VT switching to perform (there is
//! exactly one guest "seat", one virtual display, and only ever one client), so this is a
//! minimal, protocol-correct but entirely virtual implementation: `/dev/tty0` always reports VT
//! 1 as the active VT, `/dev/tty1` always exists and accepts every ioctl `seatd`'s own call
//! sequence issues against it, and none of the accepted calls (`VT_SETMODE`, `KDSKBMODE`,
//! `KDSETMODE`) have any real switching/keyboard-mode/graphics-mode effect to perform -- they
//! succeed because the state they would otherwise change does not exist for this device to get
//! wrong, mirroring how `DriDevices`/`DrmSubsystem` (`drm.rs`) emulate a DRM device with no real
//! GPU behind it. `VT_ACTIVATE`/`VT_WAITACTIVE` and the rest of the real kernel's `VT_*` surface
//! are deliberately NOT implemented: they are not on `seatd`'s single-seat call path (confirmed
//! by reading `seatd`'s real source directly, not guessed).
//!
//! Standalone Xorg's own VT startup path (`hw/xfree86/os-support/linux/lnx_init.c`'s
//! `parse_vt_settings`) additionally opens `/dev/tty0` and calls `VT_OPENQRY` before
//! `VT_GETSTATE`/`VT_SETMODE` -- `seatd` never does this (it always operates on an
//! already-known VT number, never searching for a free one), so it needed its own handler
//! (`open_qry`) once a real Xorg guest process was exercised against this device for the first
//! time.

use litebox_common_linux::{KD_GRAPHICS, KD_TEXT, VT_AUTO, VtMode, VtStat, errno::Errno};

use crate::{ShimPlatform, UserPtr, UserPtrMut};

/// The one VT number this virtual device ever reports as active. Real Linux VT numbers are
/// 1-based (`/dev/tty1` is the first usable console; `/dev/tty0` is the "whichever is active"
/// alias, never a VT number itself) -- `1` is the only value `VT_GETSTATE` on `/dev/tty0` needs
/// to return for `seatd`'s `seat_update_vt` to then successfully open `/dev/tty1`.
const ACTIVE_VT: u16 = 1;

/// `VT_OPENQRY` on `/dev/tty0` -- reports the number of a free VT. This device has exactly one
/// (`ACTIVE_VT`) and it is always considered free for a new client to claim (there is never a
/// second concurrent VT-owning process to conflict with), so it is the only value ever returned.
pub(crate) fn open_qry<Platform: ShimPlatform>(ptr: UserPtrMut<i32>) -> Result<u32, Errno> {
    ptr.write_at_offset::<Platform>(0, i32::from(ACTIVE_VT))
        .ok_or(Errno::EFAULT)?;
    Ok(0)
}

/// `VT_GETSTATE` on `/dev/tty0`. Real Linux answers this on ANY open VT fd (not just `tty0`),
/// but `seatd`'s own call sequence only ever issues it against `tty0` (see this module's doc
/// comment), so that is the only path wired up here.
pub(crate) fn get_state<Platform: ShimPlatform>(
    ptr: UserPtrMut<VtStat>,
) -> Result<u32, Errno> {
    let st = VtStat {
        v_active: ACTIVE_VT,
        // Real Linux reports the bitmask of allocated/signal-registered VTs here; this device
        // only ever has the one, and no caller on `seatd`'s call path reads either field (see
        // module doc comment), so a fixed, plausible non-zero value is enough to avoid looking
        // like an uninitialized/empty answer without needing any real tracking.
        v_signal: 0,
        v_state: 1 << ACTIVE_VT,
    };
    ptr.write_at_offset::<Platform>(0, st).ok_or(Errno::EFAULT)?;
    Ok(0)
}

/// `VT_SETMODE` on `/dev/tty1` -- claims (`VT_PROCESS`) or releases (`VT_AUTO`) process-
/// controlled VT switching. There is no real VT-switch event this device could ever raise (see
/// module doc comment), so the requested mode/signals are read (to validate the pointer, the
/// same way a real client's `ioctl()` call would surface `EFAULT` for a bad one) and otherwise
/// discarded rather than stored -- there is no later operation on this device that would ever
/// need to consult them.
pub(crate) fn set_mode<Platform: ShimPlatform>(ptr: UserPtr<VtMode>) -> Result<u32, Errno> {
    let _mode = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
    Ok(0)
}

/// `VT_GETMODE` on `/dev/tty1` -- reads back the VT switching mode. Since `set_mode` above
/// discards whatever a client's own `VT_SETMODE` requested (there is no real switching behavior
/// for the stored value to ever affect), this always reports `VT_AUTO` -- the same "no
/// process-controlled switching in effect" answer a real kernel would give a VT nobody has
/// claimed with `VT_PROCESS`, which is accurate here since no claim is ever actually tracked.
pub(crate) fn get_mode<Platform: ShimPlatform>(ptr: UserPtrMut<VtMode>) -> Result<u32, Errno> {
    let mode = VtMode {
        mode: VT_AUTO,
        waitv: 0,
        relsig: 0,
        acqsig: 0,
        frsig: 0,
    };
    ptr.write_at_offset::<Platform>(0, mode)
        .ok_or(Errno::EFAULT)?;
    Ok(0)
}

/// `VT_ACTIVATE` on `/dev/tty1` -- switches to the given VT (real Linux passes the target VT
/// number as the raw ioctl `arg`, not a pointer). This device has exactly one VT and no real
/// switching to perform (see module doc comment), so any request unconditionally succeeds --
/// there is nothing to validate the requested VT number against, since no other VT could ever
/// exist for this device to reject a request for.
pub(crate) fn activate(_vt: i32) -> Result<u32, Errno> {
    Ok(0)
}

/// `VT_WAITACTIVE` on `/dev/tty1` -- blocks until the given VT becomes active (same raw-`arg`
/// shape as `VT_ACTIVATE`). Since `activate` above completes the "switch" synchronously and
/// unconditionally, the VT it targets is already active by the time any caller could issue this
/// -- an immediate, unconditional success is the correct answer, not a real wait.
pub(crate) fn wait_active(_vt: i32) -> Result<u32, Errno> {
    Ok(0)
}

/// `KDSETMODE` on `/dev/tty1` -- switches between text (`KD_TEXT`) and graphics (`KD_GRAPHICS`)
/// mode. No real console framebuffer exists for this to switch, so any recognized mode value
/// succeeds as a no-op; an unrecognized value is rejected with `EINVAL`, matching real Linux
/// (which validates the mode argument before accepting it).
pub(crate) fn set_mode_kd(mode: i32) -> Result<u32, Errno> {
    if mode == KD_TEXT || mode == KD_GRAPHICS {
        Ok(0)
    } else {
        Err(Errno::EINVAL)
    }
}

/// `KDSKBMODE` on `/dev/tty1` -- switches the VT's keyboard translation mode (raw/mediumraw/
/// Unicode/off). This device has no real keyboard-translation layer of its own (guest input
/// delivery goes through the separate evdev subsystem, `evdev.rs`, entirely independent of any
/// VT's keyboard mode), so every mode value is accepted unconditionally: unlike `KDSETMODE`
/// above, real Linux itself accepts any value here too (silently clamping an out-of-range one),
/// so there is no real `EINVAL` case to preserve.
pub(crate) fn set_kbmode(_mode: i32) -> Result<u32, Errno> {
    Ok(0)
}
