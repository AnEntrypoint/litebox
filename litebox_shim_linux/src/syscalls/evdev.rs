// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! evdev keyboard/mouse input device (`/dev/input/event0`).
//!
//! Implements the minimal surface a real Linux input client (a raw `open`+`read` loop, or a
//! toolkit's evdev backend, e.g. SDL2's `evdev`/`KMSDRM` input path) needs to receive real
//! keyboard/mouse events: a queue of real `struct input_event` records the host side pushes in
//! (see [`EvdevSubsystem::push_key`]/[`EvdevSubsystem::push_rel`]), drained by the guest's own
//! `read()` on the device fd, exactly mirroring `drm.rs`'s `DrmSubsystem` but with the data flow
//! reversed: DRM's page-flip callback pushes pixel bytes OUT to the host; this subsystem's push
//! methods bring host input events IN to the guest.
//!
//! Struct layout and event-type/code constants live in `litebox_common_linux`'s `InputEvent`/
//! `EV_*`/`BTN_*`/`REL_*` items; see `docs/evdev-input-event-reference.md` for the derivation
//! (fetched verbatim from the real kernel `input.h`/`input-event-codes.h`, not guessed).
//!
//! # What this pass deliberately does NOT implement
//!
//! - **Capability-query ioctls** (`EVIOCGVERSION`/`EVIOCGID`/`EVIOCGNAME`/`EVIOCGBIT`/`EVIOCGABS`):
//!   a real evdev client typically probes these before trusting a device's reported capabilities,
//!   but a client that already knows what to expect (this pass's own live-witness repro) can read
//!   raw events without them. `EVIOCGBIT` in particular is a *variable-length* ioctl (its exact
//!   request number depends on the caller's buffer size, `_IOC(_IOC_READ, 'E', 0x20+ev, len)`),
//!   needing a different dispatch shape than DRM's fixed-number match arms -- a real, separate
//!   follow-up, not attempted here to keep this pass reviewable.
//! - **Absolute positioning / touch** (`EV_ABS`): only `EV_KEY` (keyboard + mouse buttons) and
//!   `EV_REL` (relative mouse motion/wheel) are emitted -- the minimal shape a desktop-style
//!   pointer needs; touch/tablet/joystick devices use `EV_ABS` and are out of scope.
//! - **Multiple event devices**: exactly one combined keyboard+mouse node (`event0`), matching
//!   `litebox::fs::devices::InputDevice`'s current single-variant enum.

use alloc::collections::VecDeque;

use litebox::event::Events;
use litebox::event::polling::Pollee;
use litebox_common_linux::{EV_SYN, InputEvent, SYN_REPORT};
use zerocopy::IntoBytes;

use crate::ShimPlatform;

/// Bound on how many not-yet-`read()` events this device holds before the oldest is dropped --
/// a real evdev device's kernel-side ring buffer is similarly bounded (`EVDEV_BUFFER_SIZE`,
/// currently 64 in the real kernel); this exists so a guest that never reads input (e.g. a
/// non-interactive program that merely opened the device) can't grow this queue unboundedly from
/// host-side input the guest never asked to receive.
const MAX_QUEUED_EVENTS: usize = 256;

/// State for the one virtual evdev device this shim exposes. See the module doc comment for what
/// is and is not implemented in this pass.
pub(crate) struct EvdevSubsystem<Platform: ShimPlatform> {
    pending_events: litebox::sync::Mutex<Platform, VecDeque<InputEvent>>,
    /// Wakeup mechanism for a guest thread blocked in `poll`/`epoll_wait`/`select` on the evdev
    /// fd waiting for input -- see `DrmSubsystem::flip_pollee`'s doc comment for the identical
    /// bug shape this fixes: `EpollDescriptor::poll`'s `File` arm's `EvdevFd` branch previously
    /// computed on-demand readiness via `has_pending()` but never registered an observer here,
    /// so a client that registers this fd once via `epoll_ctl` and blocks in `epoll_wait` across
    /// many iterations (rather than re-polling synchronously after every event) would never wake
    /// for the second and later queued input events.
    pollee: Pollee<Platform>,
}

impl<Platform: ShimPlatform> EvdevSubsystem<Platform> {
    pub(crate) fn new() -> Self {
        Self {
            pending_events: litebox::sync::Mutex::new(VecDeque::new()),
            pollee: Pollee::new(),
        }
    }

    /// Register an observer for evdev fd readiness -- see [`Self::pollee`]'s doc comment. Called
    /// from `syscalls::epoll::EpollDescriptor::poll`'s `File` arm's `EvdevFd` branch, exactly
    /// where every other pollable fd kind (e.g. eventfd) registers its own observer.
    pub(crate) fn register_observer(
        &self,
        observer: alloc::sync::Weak<dyn litebox::event::observer::Observer<Events>>,
    ) {
        self.pollee.register_observer(observer, Events::IN);
    }

    /// Push one real event into the queue, followed by a `SYN_REPORT` (real evdev clients expect
    /// every logically-grouped batch of events -- here, always a single event -- to be terminated
    /// by an `EV_SYN`/`SYN_REPORT` marker; without it, a client buffering events until it sees a
    /// sync marker would never see this one). Drops the oldest queued event first if already at
    /// [`MAX_QUEUED_EVENTS`], matching a real kernel ring buffer's overflow behavior (newest
    /// events win, not silently refused).
    fn push(&self, event: InputEvent) {
        let mut events = self.pending_events.lock();
        if events.len() >= MAX_QUEUED_EVENTS {
            events.pop_front();
        }
        events.push_back(event);
        if events.len() >= MAX_QUEUED_EVENTS {
            events.pop_front();
        }
        events.push_back(InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            r#type: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        });
        drop(events);
        self.pollee.notify_observers(Events::IN);
    }

    /// Queue an `EV_KEY` event -- a keyboard key or mouse button transition. `value` is `1`
    /// (pressed), `0` (released), or `2` (auto-repeat, real Linux's own third state for a held
    /// key) matching real evdev semantics exactly; the caller (the host-side window's own
    /// keyboard/mouse-button event handler) is responsible for passing the correct value.
    pub(crate) fn push_key(&self, code: u16, value: i32) {
        self.push(InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            r#type: litebox_common_linux::EV_KEY,
            code,
            value,
        });
    }

    /// Queue an `EV_REL` event -- relative mouse motion (`REL_X`/`REL_Y`) or wheel movement
    /// (`REL_WHEEL`). `value` is the signed delta, matching real evdev semantics.
    pub(crate) fn push_rel(&self, code: u16, value: i32) {
        self.push(InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            r#type: litebox_common_linux::EV_REL,
            code,
            value,
        });
    }

    /// Pop the oldest pending event, if any, encoded as the exact bytes a real `read()` on an
    /// evdev fd would return (real evdev's `read()` contract: whole `struct input_event` records,
    /// never a partial one). `None` means no event is pending -- the caller (see
    /// `syscalls::file::do_read`'s input-fd branch) is responsible for real Linux's actual
    /// `read()`-with-nothing-pending behavior (blocks, or `EAGAIN` if the fd is non-blocking).
    pub(crate) fn pop_event_bytes(&self) -> Option<alloc::vec::Vec<u8>> {
        let event = self.pending_events.lock().pop_front()?;
        let mut bytes = alloc::vec::Vec::with_capacity(size_of::<InputEvent>());
        bytes.extend_from_slice(event.as_bytes());
        Some(bytes)
    }

    /// Whether at least one event is queued -- consulted by `syscalls::epoll::EpollDescriptor`'s
    /// `poll()` (so a guest's `select()`/`poll()`/`epoll_wait()` loop correctly wakes once real
    /// input arrives) and available for a future `read()` non-blocking-fd branch that wants to
    /// choose the right error without popping speculatively.
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending_events.lock().is_empty()
    }
}
