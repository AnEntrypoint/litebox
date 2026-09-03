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

use litebox_common_linux::{KD_GRAPHICS, KD_TEXT, VtMode, VtStat, errno::Errno};

use crate::{ShimPlatform, UserPtr, UserPtrMut};

/// The one VT number this virtual device ever reports as active. Real Linux VT numbers are
/// 1-based (`/dev/tty1` is the first usable console; `/dev/tty0` is the "whichever is active"
/// alias, never a VT number itself) -- `1` is the only value `VT_GETSTATE` on `/dev/tty0` needs
/// to return for `seatd`'s `seat_update_vt` to then successfully open `/dev/tty1`.
const ACTIVE_VT: u16 = 1;

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
