// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Host-side GUI presentation: a real Windows window, backed by `wgpu`, that displays pixel
//! buffers a guest DRM client has drawn into (see `litebox_shim_linux::syscalls::drm`'s
//! `DrmSubsystem`).
//!
//! # Why a dedicated OS thread
//!
//! `litebox_runner_linux_on_windows_userland`'s own main thread calls
//! `litebox_platform_windows_userland::run_thread` directly to execute the guest, blocking until
//! it exits -- there is no spare "main loop" slot for `winit`'s own event loop to share. Unlike
//! macOS' Cocoa (which requires its run loop on the process' first/main thread, a hard OS
//! constraint `winit` cannot work around), a Windows message loop is a genuinely PER-THREAD
//! construct (`CreateWindowEx`/`GetMessage`/`DispatchMessage` all operate on whichever thread
//! calls them, unrelated to which thread the process started on) -- so `winit`'s `EventLoop` runs
//! correctly on a plain spawned thread here, coexisting with the guest-execution thread the same
//! way this crate's existing `net.rs` worker and `process_fork.rs` machinery already run
//! independent background threads. This is a genuinely Windows-specific argument; the analogous
//! module for macOS/Linux userland (see the `gui-macos-linux-presentation-port` PRD row) will need
//! its own, different threading story.
//!
//! # What this module does and does not do (this pass)
//!
//! Provides [`Presenter`]: creates a real window and a `wgpu` `Surface` for it, and exposes
//! [`Presenter::sender`] -- a channel a caller elsewhere in the process can use to push a new
//! frame (raw BGRA8/XRGB8888 pixel bytes plus width/height) to be uploaded as a texture and
//! blitted onto the window on the next redraw. **Wiring this to `DrmSubsystem`'s own page-flip
//! handler is a real, separate follow-up** (`litebox_shim_linux` cannot depend on
//! `litebox_platform_windows_userland` the other way around -- the actual connection has to be
//! made by `litebox_runner_linux_on_windows_userland`, which depends on both, threading a
//! [`FrameSender`] through `LinuxShimBuilder`'s construction so `DrmSubsystem::new` can hold one
//! and call it from `page_flip`; not done in this pass to keep this module reviewable and
//! independently verifiable first). This pass is verified by directly calling
//! [`Presenter::sender`]'s `send` with a synthetic test pattern and confirming a real window
//! appears showing it -- not yet by an actual guest DRM client's own drawn frame.

use std::sync::{Arc, Mutex};

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{
    ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy,
};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::window::{Window, WindowId};

/// A `dump_frame_diagnostic` frame handed off to the background writer thread, plus everything
/// the writer needs to reproduce the exact on-disk name/format the flip-path caller would have
/// used if it wrote synchronously.
struct QueuedDumpFrame {
    frame_number: usize,
    width: usize,
    height: usize,
    pitch: usize,
    bytes: Vec<u8>,
    out_path: String,
}

/// Total frames enqueued for background writing (drained or dropped) -- used only to size the
/// eventual "N frames dropped" disclosure; the queue itself is the source of truth for backlog.
static DUMP_FRAMES_ENQUEUED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
/// Frames silently (from the guest's point of view) DROPPED because the bounded queue was full
/// when the flip path tried to enqueue -- counted, never hidden; reported via
/// `dump_frame_diagnostic_report_drops` at end-of-run.
static DUMP_FRAMES_DROPPED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Bounded channel capacity for queued frames awaiting the background BMP writer. Small and
/// fixed: this diagnostic's whole point is to observe guest timing, so a large buffer that lets
/// the guest race arbitrarily far ahead of disk would reintroduce a different, harder-to-reason
/// timing distortion (unbounded memory growth, then a delayed burst of writes) instead of the
/// synchronous-I/O stall this change removes. 4 frames at 1920x1080 BGRA is ~33MB resident, an
/// acceptable fixed cost for a diagnostic-only code path.
const DUMP_FRAMES_QUEUE_CAPACITY: usize = 4;

fn dump_frames_writer_channel() -> &'static std::sync::mpsc::SyncSender<QueuedDumpFrame> {
    static CHANNEL: std::sync::OnceLock<std::sync::mpsc::SyncSender<QueuedDumpFrame>> =
        std::sync::OnceLock::new();
    CHANNEL.get_or_init(|| {
        let (tx, rx) =
            std::sync::mpsc::sync_channel::<QueuedDumpFrame>(DUMP_FRAMES_QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("litebox-dump-frames-writer".to_owned())
            .spawn(move || {
                // All actual disk I/O for `LITEBOX_DUMP_FRAMES` happens here, off the guest's
                // DRM PAGE_FLIP ioctl path entirely -- the flip handler only ever pushes onto the
                // bounded channel above (a cheap, fast, non-blocking-on-disk operation) and
                // returns immediately. This is the fix for the exact hazard `LITEBOX_VEH_TRACE`
                // already documents elsewhere in this project (AGENTS.md, RtlpUnwindPrologue crash
                // section): a diagnostic's own overhead perturbing the guest timing it exists to
                // observe.
                while let Ok(queued) = rx.recv() {
                    write_dump_frame_bmp(&queued);
                }
            })
            .expect("failed to spawn litebox-dump-frames-writer thread");
        tx
    })
}

/// Encode and write one already-dequeued frame as a `.bmp` file (unchanged format/layout from
/// this diagnostic's original synchronous implementation) -- runs only on the background writer
/// thread, never on the guest's flip path.
fn write_dump_frame_bmp(queued: &QueuedDumpFrame) {
    let width = queued.width;
    let height = queued.height;
    let pitch = queued.pitch;
    let row_bytes = width * 4;
    let pixel_data_size = row_bytes * height;
    let file_header_size = 14;
    let info_header_size = 40;
    let data_offset = file_header_size + info_header_size;
    let file_size = data_offset + pixel_data_size;

    let mut out = Vec::with_capacity(file_size);
    // BITMAPFILEHEADER
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(data_offset as u32).to_le_bytes());
    // BITMAPINFOHEADER
    out.extend_from_slice(&(info_header_size as u32).to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bpp
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB, uncompressed
    out.extend_from_slice(&(pixel_data_size as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes()); // x ppm (~72 dpi)
    out.extend_from_slice(&2835i32.to_le_bytes()); // y ppm
    out.extend_from_slice(&0u32.to_le_bytes()); // colors used
    out.extend_from_slice(&0u32.to_le_bytes()); // important colors
    // BMP rows are stored bottom-to-top.
    for row in (0..height).rev() {
        let row_start = row * pitch;
        let row_end = row_start + row_bytes;
        if let Some(row_bytes_slice) = queued.bytes.get(row_start..row_end) {
            out.extend_from_slice(row_bytes_slice);
        } else {
            out.extend(std::iter::repeat_n(0u8, row_bytes));
        }
    }
    if let Err(e) = std::fs::write(&queued.out_path, &out) {
        eprintln!(
            "[LITEBOX_DUMP_FRAMES] failed to write {} (frame {}): {e}",
            queued.out_path, queued.frame_number
        );
    } else {
        eprintln!(
            "[LITEBOX_DUMP_FRAMES] wrote {} ({file_size} bytes, frame {})",
            queued.out_path, queued.frame_number
        );
    }
}

/// Debugging aid, gated behind `LITEBOX_DUMP_FRAMES`: called from the guest's DRM `PAGE_FLIP`
/// ioctl handler path once per flip. Historically this both computed a per-frame pixel summary
/// AND synchronously wrote an 8.3MB (1920x1080 BGRA) `.bmp` to disk, entirely inside that ioctl
/// handler -- meaning the guest compositor was fully blocked on disk I/O for every single frame it
/// presented (~500MB/s of synchronous writes at 60fps), a real, verified hazard equivalent to the
/// one already documented for `LITEBOX_VEH_TRACE` (AGENTS.md, RtlpUnwindPrologue crash section):
/// the instrument's own overhead was large enough to change the exact guest timing it exists to
/// observe, undermining diagnostic conclusions drawn from it.
///
/// This function now:
/// - samples via `LITEBOX_DUMP_FRAMES_EVERY=N` (default 1 = every frame, unchanged from history)
/// - always computes and logs the same pixel-summary line as before (non-black-pixel count,
///   distinct-color count) -- the metadata this project's own diagnostic history shows was
///   actually consulted in the majority of investigation passes, not the image files themselves
/// - skips the BMP entirely under `LITEBOX_DUMP_FRAMES_METADATA_ONLY=1`
/// - otherwise ENQUEUES the frame onto a small bounded channel and returns immediately; the
///   actual `.bmp` encode+write happens on a dedicated background thread
///   (`dump_frames_writer_channel`), never on this call stack -- so the flip path's own added
///   cost is now one clone into a `Vec` plus a channel push, not a file write
/// - if the writer thread has fallen behind and the bounded queue is full, DROPS the new frame
///   (never blocks the guest) but counts every drop (`DUMP_FRAMES_DROPPED`) for a loud
///   end-of-run disclosure via `dump_frame_diagnostic_report_drops`, rather than silently
///   missing frames
///
/// With every new env var unset, behavior is unchanged from history except that the disk write
/// itself no longer blocks this call -- same file names (`litebox_frame_dump_<n>.bmp` unless
/// `LITEBOX_DUMP_FRAMES_PATH` overrides it), same numbering, same BMP bytes, every frame.
pub fn dump_frame_diagnostic(frame: &Frame) {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let pitch = frame.pitch as usize;

    // AGENTS.md pass 270: number each dumped frame so a run that transitions through multiple
    // distinct states (e.g. black -> real content -> black again, confirmed live this pass) can
    // be inspected frame-by-frame instead of only ever seeing the LAST write, which silently
    // overwrote every earlier, potentially more interesting frame.
    //
    // This counter numbers every FLIP (not every write), matching historical behavior exactly:
    // with `LITEBOX_DUMP_FRAMES_EVERY` unset/1, every flip was previously dumped and numbered
    // sequentially with no gaps -- sampling every Nth flip must still count every flip so the
    // numbering in surviving file names reflects real flip position in the run, not a
    // renumbered-from-zero sample index.
    static FRAME_COUNTER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    let n = FRAME_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    let every: usize = std::env::var("LITEBOX_DUMP_FRAMES_EVERY")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&v: &usize| v > 0)
        .unwrap_or(1);
    if n % every != 0 {
        return;
    }

    let mut non_black_pixels = 0usize;
    let mut distinct_colors = std::collections::HashSet::new();
    for row in 0..height {
        let row_start = row * pitch;
        for col in 0..width {
            let px_start = row_start + col * 4;
            let Some(px) = frame.bytes.get(px_start..px_start + 4) else {
                continue;
            };
            // "Black" means the RGB channels alone, regardless of alpha -- confirmed live
            // (advisor-db cross-session review) that the previous exact-match check against only
            // `[0, 0, 0, 0]` and `[0, 0, 0, 255]` produced a false-positive whole-frame
            // `non_black_pixels` count on a real capture whose actual bytes were `[0, 0, 0, 1]`
            // (visually indistinguishable from black, just an off-by-one alpha value neither
            // exact match caught) -- a scanout framebuffer's alpha byte carries no visual meaning
            // for this diagnostic's own purpose (spotting real drawn RGB content), so it should
            // never be part of the "is this black" test at all.
            if px[0] != 0 || px[1] != 0 || px[2] != 0 {
                non_black_pixels += 1;
            }
            if distinct_colors.len() < 64 {
                distinct_colors.insert([px[0], px[1], px[2], px[3]]);
            }
        }
    }
    eprintln!(
        "[LITEBOX_DUMP_FRAMES] frame {}x{} pitch={} non_black_pixels={} distinct_colors_capped64={}",
        width,
        height,
        pitch,
        non_black_pixels,
        distinct_colors.len()
    );

    if std::env::var_os("LITEBOX_DUMP_FRAMES_METADATA_ONLY").is_some() {
        // Metadata already computed and logged above; skip the copy, encode, and write below
        // entirely -- this is the near-zero-cost mode for the majority case (per the investigation
        // this task was commissioned from) where the pixel-summary line, not the image file, is
        // what's actually consulted.
        return;
    }

    let out_path = std::env::var("LITEBOX_DUMP_FRAMES_PATH")
        .map(|base| format!("{base}.{n}"))
        .unwrap_or_else(|_| format!("litebox_frame_dump_{n}.bmp"));

    let queued = QueuedDumpFrame {
        frame_number: n,
        width,
        height,
        pitch,
        bytes: frame.bytes.clone(),
        out_path,
    };
    DUMP_FRAMES_ENQUEUED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    // `try_send`, never `send`: a full queue means the writer thread has fallen behind, and the
    // whole point of this change is that the guest must never block on disk I/O -- so a full
    // queue drops the new frame rather than stalling the flip path. Every drop is counted, never
    // silent (see `dump_frame_diagnostic_report_drops`).
    if let Err(_dropped) = dump_frames_writer_channel().try_send(queued) {
        let total_dropped = DUMP_FRAMES_DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
        eprintln!(
            "[LITEBOX_DUMP_FRAMES] WARNING: writer backpressure, dropped frame {n} (total dropped so far: {total_dropped})"
        );
    }
}

/// Loud, not silent: report the total count of frames dropped due to writer backpressure. Call
/// once at end-of-run (e.g. right before the runner process exits) so a diagnostic run using
/// `LITEBOX_DUMP_FRAMES` honestly discloses any gap in the on-disk sequence rather than leaving a
/// reader to discover missing frame numbers on their own. A no-op (still logs a zero count) when
/// `LITEBOX_DUMP_FRAMES` was never enabled or no frame was ever dropped.
pub fn dump_frame_diagnostic_report_drops() {
    let enqueued = DUMP_FRAMES_ENQUEUED.load(core::sync::atomic::Ordering::Relaxed);
    let dropped = DUMP_FRAMES_DROPPED.load(core::sync::atomic::Ordering::Relaxed);
    if enqueued == 0 && dropped == 0 {
        return;
    }
    eprintln!(
        "[LITEBOX_DUMP_FRAMES] end of run: {enqueued} frames enqueued for writing, {dropped} dropped due to writer backpressure"
    );
}

/// One frame's worth of pixel content to present: raw bytes in `BGRA8`/`XRGB8888` byte order
/// (matching `DRM_FORMAT_XRGB8888`, the format `DrmSubsystem`'s virtual display advertises), row
/// pitch already applied (i.e. `bytes.len() == pitch * height`, not necessarily `width * 4 *
/// height` if the source buffer had padding).
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bytes: Vec<u8>,
}

/// One real keyboard/mouse-button transition or relative-motion event, already translated into
/// Linux evdev's own `(type, code, value)` shape (see `litebox_common_linux`'s `EV_*`/`KEY_*`/
/// `BTN_*`/`REL_*` constants) -- the caller (`litebox_runner_linux_on_windows_userland`) forwards
/// these directly into `LinuxShim::push_input_key`/`push_input_rel` with no further translation.
pub enum InputSignal {
    /// `(code, value)` for an `EV_KEY` event -- a keyboard key or mouse button, `value` 1
    /// (pressed) or 0 (released).
    Key(u16, i32),
    /// `(code, value)` for an `EV_REL` event -- relative motion, `value` the signed delta.
    Rel(u16, i32),
    /// `(dx, dy)` for one 2D mouse movement, to be delivered as a SINGLE evdev report
    /// (`REL_X`, `REL_Y`, one `SYN_REPORT`) rather than two separately-synced ones. Real
    /// hardware groups the axes of one physical motion into one report; splitting them makes a
    /// client run its pointer-motion path twice and briefly act on an X-only position the user
    /// never pointed at. Either delta may be zero (that axis is then omitted).
    RelMotion(i32, i32),
}

/// Translate a `winit` physical key into its Linux evdev `KEY_*` code, where a real, verified
/// mapping exists (see `litebox_common_linux`'s own `KEY_*` constants for which keys are
/// covered). `None` for any key outside that covered set -- silently dropped by the caller,
/// matching how a real keyboard simply has no key to send for a code this device doesn't map.
fn winit_keycode_to_evdev(key: KeyCode) -> Option<u16> {
    // Enumerating all ~80 `KEY_*` constants by name would hurt readability far more than it helps
    // -- matches this crate family's own `#[allow]`-on-deliberate-exception convention elsewhere.
    #[allow(clippy::wildcard_imports)]
    use litebox_common_linux::*;
    Some(match key {
        KeyCode::Escape => KEY_ESC,
        KeyCode::Digit1 => KEY_1,
        KeyCode::Digit2 => KEY_2,
        KeyCode::Digit3 => KEY_3,
        KeyCode::Digit4 => KEY_4,
        KeyCode::Digit5 => KEY_5,
        KeyCode::Digit6 => KEY_6,
        KeyCode::Digit7 => KEY_7,
        KeyCode::Digit8 => KEY_8,
        KeyCode::Digit9 => KEY_9,
        KeyCode::Digit0 => KEY_0,
        KeyCode::Minus => KEY_MINUS,
        KeyCode::Equal => KEY_EQUAL,
        KeyCode::Backspace => KEY_BACKSPACE,
        KeyCode::Tab => KEY_TAB,
        KeyCode::KeyQ => KEY_Q,
        KeyCode::KeyW => KEY_W,
        KeyCode::KeyE => KEY_E,
        KeyCode::KeyR => KEY_R,
        KeyCode::KeyT => KEY_T,
        KeyCode::KeyY => KEY_Y,
        KeyCode::KeyU => KEY_U,
        KeyCode::KeyI => KEY_I,
        KeyCode::KeyO => KEY_O,
        KeyCode::KeyP => KEY_P,
        KeyCode::BracketLeft => KEY_LEFTBRACE,
        KeyCode::BracketRight => KEY_RIGHTBRACE,
        KeyCode::Enter => KEY_ENTER,
        KeyCode::ControlLeft => KEY_LEFTCTRL,
        KeyCode::KeyA => KEY_A,
        KeyCode::KeyS => KEY_S,
        KeyCode::KeyD => KEY_D,
        KeyCode::KeyF => KEY_F,
        KeyCode::KeyG => KEY_G,
        KeyCode::KeyH => KEY_H,
        KeyCode::KeyJ => KEY_J,
        KeyCode::KeyK => KEY_K,
        KeyCode::KeyL => KEY_L,
        KeyCode::Semicolon => KEY_SEMICOLON,
        KeyCode::Quote => KEY_APOSTROPHE,
        KeyCode::Backquote => KEY_GRAVE,
        KeyCode::ShiftLeft => KEY_LEFTSHIFT,
        KeyCode::Backslash => KEY_BACKSLASH,
        KeyCode::KeyZ => KEY_Z,
        KeyCode::KeyX => KEY_X,
        KeyCode::KeyC => KEY_C,
        KeyCode::KeyV => KEY_V,
        KeyCode::KeyB => KEY_B,
        KeyCode::KeyN => KEY_N,
        KeyCode::KeyM => KEY_M,
        KeyCode::Comma => KEY_COMMA,
        KeyCode::Period => KEY_DOT,
        KeyCode::Slash => KEY_SLASH,
        KeyCode::ShiftRight => KEY_RIGHTSHIFT,
        KeyCode::AltLeft => KEY_LEFTALT,
        KeyCode::Space => KEY_SPACE,
        KeyCode::CapsLock => KEY_CAPSLOCK,
        KeyCode::F1 => KEY_F1,
        KeyCode::F2 => KEY_F2,
        KeyCode::F3 => KEY_F3,
        KeyCode::F4 => KEY_F4,
        KeyCode::F5 => KEY_F5,
        KeyCode::F6 => KEY_F6,
        KeyCode::F7 => KEY_F7,
        KeyCode::F8 => KEY_F8,
        KeyCode::F9 => KEY_F9,
        KeyCode::F10 => KEY_F10,
        KeyCode::F11 => KEY_F11,
        KeyCode::F12 => KEY_F12,
        KeyCode::ControlRight => KEY_RIGHTCTRL,
        KeyCode::AltRight => KEY_RIGHTALT,
        KeyCode::Home => KEY_HOME,
        KeyCode::ArrowUp => KEY_UP,
        KeyCode::PageUp => KEY_PAGEUP,
        KeyCode::ArrowLeft => KEY_LEFT,
        KeyCode::ArrowRight => KEY_RIGHT,
        KeyCode::End => KEY_END,
        KeyCode::ArrowDown => KEY_DOWN,
        KeyCode::PageDown => KEY_PAGEDOWN,
        KeyCode::Insert => KEY_INSERT,
        KeyCode::Delete => KEY_DELETE,
        _ => return None,
    })
}

/// Translate a `winit` mouse button into its Linux evdev `BTN_*` code. `None` for any button
/// outside the common three (a real mouse can report more, e.g. `BTN_SIDE`/`BTN_EXTRA` for back/
/// forward buttons -- out of scope for this pass).
fn winit_mouse_button_to_evdev(button: MouseButton) -> Option<u16> {
    match button {
        MouseButton::Left => Some(litebox_common_linux::BTN_LEFT),
        MouseButton::Right => Some(litebox_common_linux::BTN_RIGHT),
        MouseButton::Middle => Some(litebox_common_linux::BTN_MIDDLE),
        _ => None,
    }
}

/// The sending half of the frame channel: clone and hand out to whatever produces frames (in a
/// later pass, `DrmSubsystem::page_flip`). Sending after the presenter's window has closed is a
/// silent no-op (matching how writing to a closed real display would simply have no visible
/// effect, rather than being a caller-visible error condition to handle).
/// A control message delivered to the presenter's event loop from any other thread.
///
/// The window's VISIBILITY is runtime state, not a startup decision. A guest GUI must be able to
/// run with no window on screen and have one shown later (and hidden again) without restarting
/// the guest or disturbing the frame pipeline: the display is an observer of the guest, never a
/// prerequisite for it. Page-flips, frame capture, and input all keep working while hidden --
/// only presentation stops, which is exactly what an unmapped display does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenterCommand {
    /// A frame was queued; drain and redraw. (Previously the event type was `()`, carrying only
    /// this meaning implicitly.)
    Wake,
    /// Show (`true`) or hide (`false`) the window. Idempotent -- setting the current state again
    /// is harmless, so callers need not track it.
    SetVisible(bool),
}

/// A single-slot mailbox: at most one pending frame at a time. A new `send` simply overwrites
/// whatever was there, silently dropping a stale frame the presenter hadn't gotten to yet -- a
/// display only ever wants the MOST RECENT scanout content, so queuing behind a slow present both
/// wastes memory unboundedly under a fast producer (the previous unbounded `mpsc::channel`'s
/// actual failure mode) and means what's on screen lags what the guest most recently drew. This
/// replaces that channel outright rather than wrapping it: a bounded capacity-1 channel would
/// still need this same "drop what's already queued, keep only newest" logic on the sender side,
/// so a plain `Mutex<Option<Frame>>` slot is the more direct implementation of the same contract.
struct FrameSlot {
    pending: Mutex<Option<Frame>>,
    /// A small free-list of `Vec<u8>` allocations the producer can reclaim instead of allocating
    /// fresh -- see [`FrameSender::recycle_buffer`]/[`FrameSender::take_free_buffer`]. Bounded at
    /// a handful of entries (never actually grows past ~2-3 in practice: at most one frame is ever
    /// "in flight" between a `send` overwriting the slot and the presenter picking it up, plus
    /// whatever `last_frame` is currently holding), so this is not an unbounded-growth risk the
    /// way the frame backlog itself used to be.
    free_buffers: Mutex<Vec<Vec<u8>>>,
}

/// Never let the free-list itself grow without bound -- a producer that races far ahead of the
/// presenter (e.g. `--gui-hidden` with nothing draining `last_frame` for a while) could otherwise
/// return more buffers than are ever reused, turning the "avoid an allocation" optimization into
/// its own slow memory leak. A handful is already far more than the realistic 1-2 ever
/// simultaneously alive (see `FrameSlot::free_buffers`'s own doc comment).
const MAX_FREE_BUFFERS: usize = 4;

#[derive(Clone)]
pub struct FrameSender {
    slot: Arc<FrameSlot>,
    // Wakes the event loop to actually process a just-sent frame promptly rather than waiting for
    // the next unrelated OS event (mouse move, timer tick, ...) to happen to pump the loop --
    // `EventLoopProxy::send_event` is `winit`'s own documented mechanism for exactly this, safe to
    // call from any thread.
    wake: EventLoopProxy<PresenterCommand>,
}

impl FrameSender {
    /// Queue `frame` as the next thing to present, replacing (not queuing behind) any frame
    /// already pending. Never blocks on the presenter's own pace. Whatever frame this call
    /// overwrites (if any) has its `Vec<u8>` reclaimed into the free-list -- see
    /// [`Self::take_free_buffer`] -- rather than simply dropped, so a steady-state producer that
    /// calls `take_free_buffer` before every `send` allocates a fresh `Vec` only on the very first
    /// flip (or after a genuine resolution change), not on every single one.
    pub fn send(&self, frame: Frame) {
        let previous = {
            let mut pending = self.slot.pending.lock().expect("frame slot mutex poisoned");
            pending.replace(frame)
        };
        if let Some(mut stale) = previous {
            let mut free = self
                .slot
                .free_buffers
                .lock()
                .expect("free buffer list mutex poisoned");
            if free.len() < MAX_FREE_BUFFERS {
                stale.bytes.clear();
                free.push(stale.bytes);
            }
        }
        let _ = self.wake.send_event(PresenterCommand::Wake);
    }

    /// Reclaim a previously-used `Vec<u8>` buffer to write a new frame's bytes into, if one is
    /// available, avoiding a fresh heap allocation for the overwhelmingly common case of a fixed
    /// guest resolution flipping repeatedly. Returns an empty `Vec` (a normal, cheap allocation on
    /// its very first real push) when the free-list is empty -- there is always a first frame, and
    /// after a genuine resolution change the old buffer's capacity would need reallocating anyway
    /// via `Vec::resize`/`extend`, exactly as a fresh `Vec` would.
    ///
    /// This does NOT eliminate the copy out of the guest's DRM dumb-buffer mapping -- see this
    /// module's own doc comment and `litebox_shim_linux::syscalls::drm::notify_flip_callback`'s
    /// SAFETY note: that mapping is torn down the instant every registered flip callback returns,
    /// while a sent `Frame` is presented much later, asynchronously, on a different thread. A copy
    /// out of the mapping is genuinely required for correctness; what this removes is the
    /// PER-FRAME ALLOCATION that copy previously always paid for, by reusing a `Vec` this same
    /// pipeline already owns instead of asking the allocator for a brand new one every single flip.
    #[must_use]
    pub fn take_free_buffer(&self) -> Vec<u8> {
        self.slot
            .free_buffers
            .lock()
            .expect("free buffer list mutex poisoned")
            .pop()
            .unwrap_or_default()
    }

    /// Show or hide the window at runtime, from any thread. A no-op if the presenter's window has
    /// already closed, matching `send`'s own "writing to a closed display simply has no effect"
    /// contract rather than surfacing an error the caller cannot act on.
    ///
    /// Hiding does NOT pause the guest, page-flips, frame capture, or input delivery -- the guest
    /// keeps rendering into its virtual display exactly as before, and `LITEBOX_DUMP_FRAMES` keeps
    /// capturing. This is what makes headless and headed the same running system rather than two
    /// modes chosen at startup.
    pub fn set_visible(&self, visible: bool) {
        let _ = self.wake.send_event(PresenterCommand::SetVisible(visible));
    }
}

/// Owns the real window, the `wgpu` presentation state, and runs `winit`'s event loop until the
/// window is closed. Call [`Presenter::run`] on a dedicated thread (see this module's doc
/// comment for why); it blocks for the window's entire lifetime.
pub struct Presenter {
    event_loop: EventLoop<PresenterCommand>,
    slot: Arc<FrameSlot>,
    sender: FrameSender,
    input_consumer: Option<Box<dyn Fn(InputSignal) + Send>>,
    /// Whether the window is mapped when it is first created. `false` gives a fully-running
    /// presenter with nothing on screen -- the guest renders, frames are captured, and a later
    /// `FrameSender::set_visible(true)` reveals the current contents.
    start_visible: bool,
}

impl Presenter {
    /// Build a not-yet-shown presenter and its window. Real `winit`/OS window/event-loop
    /// resources are not created until [`Self::run`] is called on the thread that will own them.
    ///
    /// `with_any_thread(true)`: `winit` refuses `EventLoop::new()` off the process' main thread by
    /// default -- a conservative guard that genuinely matters on platforms like macOS (Cocoa's
    /// hard main-thread requirement) but is not a real constraint on Windows (see this module's
    /// own doc comment: a Windows message loop is genuinely per-thread). Confirmed live: the
    /// default constructor panics with exactly this "significant cross-platform compatibility
    /// hazard" message when `Presenter::new()` runs on the dedicated thread
    /// `litebox_runner_linux_on_windows_userland` spawns for it (required, since that binary's own
    /// main thread is permanently occupied running the guest via `run_thread`).
    pub fn new() -> Result<Self, winit::error::EventLoopError> {
        let event_loop = EventLoop::<PresenterCommand>::with_user_event()
            .with_any_thread(true)
            .build()?;
        let wake = event_loop.create_proxy();
        let slot = Arc::new(FrameSlot {
            pending: Mutex::new(None),
            free_buffers: Mutex::new(Vec::new()),
        });
        let sender = FrameSender {
            slot: slot.clone(),
            wake,
        };
        Ok(Self {
            event_loop,
            slot,
            sender,
            input_consumer: None,
            // Visible by default: `--gui` means "show me a window".
            start_visible: true,
        })
    }

    /// A cloneable handle to push frames into this presenter from any other thread, valid for the
    /// presenter's whole lifetime (including before [`Self::run`] is called -- frames sent early
    /// simply queue until the window exists and starts draining them).
    /// Start with the window hidden. Combined with [`FrameSender::set_visible`] this makes the
    /// window a runtime-toggleable view of a guest that is already running, rather than something
    /// the guest's GUI depends on existing.
    #[must_use]
    pub fn hidden_at_startup(mut self) -> Self {
        self.start_visible = false;
        self
    }

    pub fn sender(&self) -> FrameSender {
        self.sender.clone()
    }

    /// Register `consumer` to be called, on the presenter's own event-loop thread, with every
    /// real keyboard/mouse event this window observes from [`Self::run`]'s call onward. Call
    /// before [`Self::run`] -- there is no queue-until-registered semantics (unlike [`Frame`]
    /// delivery): a real input device produces events whether or not anything is listening, and
    /// keyboard/mouse events are far higher-frequency than page-flips, so unbounded queuing
    /// before a slow-to-register consumer would be a real memory-growth risk. `consumer` runs
    /// inline on the event-loop thread (not its own spawned thread) since real callers (see
    /// `litebox_runner_linux_on_windows_userland`) only ever do a cheap, non-blocking
    /// `LinuxShim::push_input_key`/`push_input_rel` call here -- a caller doing real work should
    /// spawn its own thread/queue internally rather than blocking this window's own event pump.
    pub fn set_input_consumer(&mut self, consumer: impl Fn(InputSignal) + Send + 'static) {
        self.input_consumer = Some(Box::new(consumer));
    }

    /// Run the event loop on the calling thread until the window is closed. See this module's
    /// doc comment for why the calling thread must NOT be the guest-execution thread.
    pub fn run(self) -> Result<(), winit::error::EventLoopError> {
        self.event_loop.set_control_flow(ControlFlow::Wait);
        let mut app = PresenterApp {
            start_visible: self.start_visible,
            slot: self.slot,
            state: None,
            last_frame: None,
            input_consumer: self.input_consumer,
            last_cursor_pos: None,
        };
        self.event_loop.run_app(&mut app)
    }
}

/// How many pooled presentation textures to round-robin between. 2 would already remove the
/// per-frame allocation; 3 gives the GPU queue one extra frame of slack so `write_texture` on the
/// next-up texture never has to wait on the previous frame's own blit still reading from a texture
/// that would otherwise be reused too soon (a real risk with only 2 under `Mailbox`, where the
/// presenting side can run further ahead of vblank than under `Fifo`).
const TEXTURE_POOL_SIZE: usize = 3;

/// A pooled GPU texture that receives one frame's pixel bytes via `write_texture`, sized to
/// exactly the frame dimensions it was (re)built for. Recreated only when a NEW frame's own
/// width/height differs from what the pool was last built for (extremely rare in practice -- the
/// guest's virtual display resolution is fixed -- but not assumed impossible), never on a host
/// window resize; resizing changes the SURFACE, not the source texture the guest's frame is
/// uploaded into.
struct SourceTexture {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

struct GpuState {
    window: std::sync::Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_size: winit::dpi::PhysicalSize<u32>,
    /// The exact configuration the surface was created with, kept so a `Resized` event can
    /// re-apply it with only the dimensions changed. Rebuilding it from `get_capabilities()`
    /// on every resize risks silently picking a DIFFERENT format/alpha mode than the one
    /// deliberately chosen at startup (see the `Opaque` alpha-mode note in `resumed`).
    surface_config: wgpu::SurfaceConfiguration,
    /// The sampled-blit render pipeline: a full-screen triangle sampling `SourceTexture`'s view
    /// and writing into the surface's own texture, scaling rather than cropping when the frame's
    /// pixel dimensions and the surface's current size differ (see `present`'s doc comment for
    /// why a plain `copy_texture_to_texture` is no longer correct now the window is resizable).
    blit_pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Pooled source textures, created once here (or on an actual resize) rather than one fresh
    /// `create_texture` per frame. Round-robined in `present` via `next_pool_index`.
    texture_pool: Vec<SourceTexture>,
    /// Which pooled texture entry `present` writes into next. Wraps modulo `texture_pool.len()`.
    next_pool_index: usize,
    /// The frame dimensions `texture_pool` was built for; a frame with different dimensions
    /// forces a one-time pool rebuild (see `SourceTexture`'s own doc comment).
    pool_frame_size: (u32, u32),
    /// Set once `present` has logged the first frame/surface-size mismatch it observes, so the
    /// warning fires exactly once (not per-frame spam) yet is never silently swallowed the way the
    /// old `min()`-crop math made this exact condition invisible. A mismatch here is diagnostic
    /// evidence of a guest that page-flipped at a mode the surface wasn't sized for -- see
    /// `notify_flip_callback`'s own SETCRTC-mode-validation companion fix in
    /// `litebox_shim_linux::syscalls::drm`.
    logged_size_mismatch: bool,
}

/// Builds (or rebuilds) `TEXTURE_POOL_SIZE` pooled `Bgra8Unorm` source textures sized exactly
/// `(width, height)`, each with its own bind group against `layout`/`sampler` -- called once at
/// surface-configuration time and again only when a frame's own dimensions change (never on a
/// host window resize alone, see `SourceTexture`'s doc comment).
fn build_texture_pool(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    width: u32,
    height: u32,
) -> Vec<SourceTexture> {
    (0..TEXTURE_POOL_SIZE)
        .map(|i| {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("drm-dumb-buffer-frame-pool"),
                size: wgpu::Extent3d {
                    width: width.max(1),
                    height: height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                // XRGB8888 (the DRM format DrmSubsystem advertises) is byte-order BGRX in memory
                // on a little-endian host, which `Bgra8Unorm` matches directly with no
                // channel-swizzle needed on upload.
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("drm-frame-blit-bind-group"),
                layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            });
            let _ = i;
            SourceTexture { texture, bind_group }
        })
        .collect()
}

const BLIT_SHADER: &str = r"
struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Full-screen triangle: 3 vertices covering the whole clip-space square with no vertex buffer,
// the standard no-geometry way to sample a source texture across an entire render target.
@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var out: VertexOutput;
    let x = f32(i32(vertex_index) - 1);
    let y = f32(i32(vertex_index & 1u) * 2 - 1);
    out.clip_position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, 1.0 - (y + 1.0) * 0.5);
    return out;
}

@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, in.uv);
}
";

struct PresenterApp {
    /// See [`Presenter::hidden_at_startup`]. Applied to the window at creation time in
    /// `resumed()`; runtime changes arrive as [`PresenterCommand::SetVisible`] instead.
    start_visible: bool,
    slot: Arc<FrameSlot>,
    state: Option<GpuState>,
    /// The most recently received frame, kept regardless of whether [`Self::state`] exists yet.
    /// `winit`'s `resumed()` callback (which creates the real window/`wgpu` device/surface) fires
    /// asynchronously on the event-loop thread, genuinely racing a guest's own DRM page-flip on a
    /// completely different thread -- a frame sent before `resumed()` has run would otherwise be
    /// silently dropped by [`Self::present`]'s own `state.is_none()` early return, with no later
    /// retry once state DOES become ready. Confirmed live: a real guest program's very first
    /// page-flip (issued immediately after `CREATE_DUMB`/`ADDFB2`/`SETCRTC`, with no delay) landed
    /// before this thread's `resumed()` had fired, producing a genuinely blank white window
    /// (winit's own pre-content background) despite every ioctl succeeding correctly and the
    /// frame bytes being byte-for-byte correct. `resumed()` now replays this field once its own
    /// setup completes, and every future frame keeps updating it the same way `RedrawRequested`'s
    /// resize-driven re-presents already needed to survive a surface reconfigure.
    last_frame: Option<Frame>,
    /// See [`Presenter::set_input_consumer`]'s doc comment -- `None` for a caller that never
    /// registered one (a presenter-only use with no guest input wiring, e.g. `presenter_smoke`),
    /// in which case keyboard/mouse events are observed by `winit` but simply have nowhere to go.
    input_consumer: Option<Box<dyn Fn(InputSignal) + Send>>,
    /// The cursor's last-seen position, for deriving `EV_REL` deltas from `winit`'s
    /// absolute-position `CursorMoved` events -- see `window_event`'s own handler.
    last_cursor_pos: Option<(f64, f64)>,
}

impl PresenterApp {
    /// Upload `frame`'s pixel bytes into a pooled `wgpu` texture (round-robined, never freshly
    /// allocated per frame -- see `GpuState::texture_pool`) and render a sampled full-screen blit
    /// from it into the current surface texture.
    ///
    /// A plain `copy_texture_to_texture` (this module's previous approach) is only correct when
    /// the frame's own pixel dimensions exactly match the surface's current size. That is NO
    /// LONGER guaranteed: the window is resizable (see `window_event`'s `Resized` arm) completely
    /// independent of the guest's own fixed DRM virtual-display resolution, so frame size and
    /// surface size can genuinely differ at present time. A plain copy can only CROP in that case
    /// (as the old `min(frame, surface)` extent did) -- it has no notion of scaling. This sampled
    /// blit reads through a normalized `uv` computed from the vertex shader's own full-screen
    /// triangle, so it naturally stretches the source texture to fill whatever the surface's
    /// current size is, matching what a real display's own scanout-to-monitor scaling would do
    /// rather than silently truncating the picture.
    fn present(&mut self, frame: &Frame) {
        let Some(state) = &mut self.state else {
            return;
        };
        if !state.logged_size_mismatch
            && (frame.width, frame.height) != (state.surface_size.width, state.surface_size.height)
        {
            // Loud, one-time (not per-frame spam): the frame's own pixel dimensions no longer
            // match the surface's current size. Before this pass that was an invisible top-left
            // crop; now the sampled blit scales instead, so it displays correctly either way, but
            // the mismatch itself is still worth surfacing -- it's evidence of exactly the same
            // "guest requested an unsupported mode" class of bug as an unvalidated `SETCRTC` (see
            // `litebox_shim_linux::syscalls::drm::set_crtc`'s own companion validation), not
            // something to silently paper over just because scaling now makes it look fine.
            eprintln!(
                "[presenter-diag] frame size {}x{} differs from surface size {}x{} -- scaling, not cropping (see set_crtc mode validation)",
                frame.width, frame.height, state.surface_size.width, state.surface_size.height
            );
            state.logged_size_mismatch = true;
        }
        if state.pool_frame_size != (frame.width, frame.height) {
            state.texture_pool = build_texture_pool(
                &state.device,
                &state.bind_group_layout,
                &state.sampler,
                frame.width,
                frame.height,
            );
            state.pool_frame_size = (frame.width, frame.height);
            state.next_pool_index = 0;
        }
        let pool_len = state.texture_pool.len();
        let index = state.next_pool_index;
        state.next_pool_index = (index + 1) % pool_len;
        let entry = &state.texture_pool[index];
        state.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &entry.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(frame.pitch),
                rows_per_image: Some(frame.height),
            },
            wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
        );
        let Ok(surface_texture) = state.surface.get_current_texture() else {
            return;
        };
        let surface_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = state
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("drm-frame-blit-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &surface_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&state.blit_pipeline);
            pass.set_bind_group(0, &entry.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        state.queue.submit(Some(encoder.finish()));
        surface_texture.present();
    }
}

impl ApplicationHandler<PresenterCommand> for PresenterApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let window_attrs = Window::default_attributes()
            .with_title("litebox virtual display")
            .with_visible(self.start_visible)
            .with_inner_size(winit::dpi::PhysicalSize::new(1920u32, 1080u32));
        // The guest's display is a compile-time constant (`VIRTUAL_WIDTH`/`VIRTUAL_HEIGHT`,
        // 1920x1080) with no hotplug or mode-change path, and `present()` CLIPS the guest
        // framebuffer into the surface rather than scaling it. So the visible area is a 1:1
        // top-left crop, and a window smaller than the guest leaves the bottom/right undrawn
        // while RELATIVE motion still moves the guest cursor into it -- the pointer disappears
        // somewhere the user cannot see.
        //
        // Locking the window to 1920x1080 makes tracking exact but is NOT acceptable on its own:
        // on a smaller desktop (this host is 1536x864) that window does not fit, putting 36% of
        // the guest display permanently off-screen with no way to reach it. That trades one real
        // problem for a worse one.
        //
        // So the window stays RESIZABLE and `Resized` keeps `surface_size` truthful. What that
        // buys: the crop always matches what is actually on screen, so cursor position and
        // visible pixels never disagree about the region they share. What it cannot fix without
        // a guest mode-change path: guest display area outside the window is still unreachable
        // by sight, though relative motion can still move the cursor there. Rescaling deltas is
        // NOT the answer -- it would make pointer speed depend on window size (x2.00 at 960px
        // wide, x0.75 at 2560px) and still would not reveal the hidden region.
        let Ok(window) = event_loop.create_window(window_attrs) else {
            return;
        };
        let window = std::sync::Arc::new(window);
        // `Backends::DX12`, not `wgpu::Instance::default()`'s full auto-detected set: confirmed
        // live on this host (NVIDIA/AMD hybrid laptop GPU, Windows) that the Vulkan backend's
        // swapchain reproducibly hangs `Surface::get_current_texture()` indefinitely (no error, no
        // timeout, no further progress) on a freshly created window's very first frame -- even
        // when called correctly from `RedrawRequested` with a confirmed-visible, non-minimized
        // window, and independent of `PresentMode` (`Immediate` and `Fifo` both hang identically)
        // or which physical GPU wgpu selects (reproduces on both the AMD iGPU and the NVIDIA
        // dGPu). Forcing DX12 makes the identical repro present correctly on the first frame,
        // every time. This is a genuine Vulkan WSI/driver-level swapchain-acquire issue on this
        // host class, not anything under this module's own control to fix via configuration.
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..Default::default()
        });
        eprintln!("[presenter-diag] creating surface");
        let Ok(surface) = instance.create_surface(window.clone()) else {
            eprintln!("[presenter-diag] create_surface FAILED");
            return;
        };
        eprintln!("[presenter-diag] requesting adapter");
        let adapter_result =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }));
        eprintln!("[presenter-diag] request_adapter returned: {}", adapter_result.is_ok());
        let Ok(adapter) = adapter_result else {
            eprintln!("[presenter-diag] request_adapter FAILED: {:?}", adapter_result.err());
            return;
        };
        eprintln!("[presenter-diag] requesting device");
        let device_result =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("litebox-presenter"),
                ..Default::default()
            }));
        eprintln!("[presenter-diag] request_device returned: {}", device_result.is_ok());
        let Ok((device, queue)) = device_result else {
            eprintln!("[presenter-diag] request_device FAILED: {:?}", device_result.err());
            return;
        };
        eprintln!("[presenter-diag] device+queue obtained, continuing setup");
        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| *f == wgpu::TextureFormat::Bgra8Unorm)
            .unwrap_or(caps.formats[0]);
        // `Fifo` is the only present mode every wgpu surface is required to support (the wgpu spec
        // guarantees this) -- kept as the fallback. Prefer `Mailbox` when the adapter/surface
        // actually lists it: `Fifo` blocks the presenting thread until vblank, while `Mailbox`
        // lets the guest-facing/render side keep working without that block, replacing (never
        // queuing behind) a not-yet-presented frame the same way this module's own frame slot
        // does one layer up (see `FrameSlot`). Never assumed universally available -- queried from
        // `caps.present_modes` fresh here, per the wgpu spec's own "only Fifo is guaranteed" rule.
        let present_mode = caps
            .present_modes
            .iter()
            .copied()
            .find(|m| *m == wgpu::PresentMode::Mailbox)
            .unwrap_or(wgpu::PresentMode::Fifo);
        eprintln!(
            "[presenter-diag] present_mode={present_mode:?} (available: {:?})",
            caps.present_modes
        );
        // AGENTS.md pass 270: this previously took `caps.alpha_modes[0]` -- whatever alpha mode
        // the GPU/driver happens to report FIRST, with no preference for `Opaque`. Live evidence
        // (a real weston/XFCE frame captured via `LITEBOX_DUMP_FRAMES`, byte-inspected directly)
        // showed genuine, distinct, non-default pixel content (uniform RGB=0 with a low but
        // non-zero alpha, 0x13/255) being copied into this surface correctly by `present()`'s own
        // plain `copy_texture_to_texture` (which does not itself blend), yet appearing visually
        // black in the actual displayed window -- consistent with the SURFACE ITSELF being
        // configured in a non-opaque alpha mode, letting Windows' own compositor (DWM) blend the
        // low-alpha content against whatever is behind the window instead of showing it as-is.
        // `Opaque` is what a real DRM scanout always is (the whole reason DRM's own dumb-buffer
        // format doesn't even carry a meaningful alpha channel for display purposes -- `XRGB8888`,
        // not `ARGB8888`) -- explicitly prefer it here, falling back to whatever the GPU actually
        // offers only if `Opaque` genuinely isn't supported (extremely unlikely on any real
        // Windows GPU/driver, but `caps.alpha_modes` is not guaranteed non-empty of `Opaque`
        // specifically by the wgpu spec).
        let alpha_mode = caps
            .alpha_modes
            .iter()
            .copied()
            .find(|m| *m == wgpu::CompositeAlphaMode::Opaque)
            .unwrap_or(caps.alpha_modes[0]);
        eprintln!("[presenter-diag] configuring surface, alpha_mode={alpha_mode:?} (available: {:?})", caps.alpha_modes);
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);
        eprintln!("[presenter-diag] surface configured, setting up blit pipeline + texture pool");

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("drm-frame-blit-bind-group-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        // Linear filtering: the source (guest) resolution and the surface (window) size can now
        // genuinely differ (see `present`'s doc comment), so this blit is a real scale, not
        // always a 1:1 copy -- linear gives a reasonably smooth result in either direction
        // (upscale or downscale) with no extra passes, at negligible cost next to the bandwidth
        // this whole change removes.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("drm-frame-blit-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("drm-frame-blit-shader"),
            source: wgpu::ShaderSource::Wgsl(BLIT_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("drm-frame-blit-pipeline-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("drm-frame-blit-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        // Pool created at a placeholder 1x1 size here; `present`'s own `pool_frame_size` check
        // rebuilds it at the real frame dimensions the moment the first frame arrives (frame size
        // is not known until then -- the guest's DRM virtual-display resolution is not read by
        // this module at surface-creation time). This still satisfies "created once at
        // configuration time, not per frame": the rebuild-on-mismatch path fires at most once in
        // the overwhelmingly common case of a fixed guest resolution, exactly like a real resize.
        let texture_pool = build_texture_pool(&device, &bind_group_layout, &sampler, 1, 1);
        eprintln!("[presenter-diag] surface configured, resumed() about to return");
        self.state = Some(GpuState {
            window,
            surface,
            device,
            queue,
            surface_size: size,
            surface_config,
            blit_pipeline,
            bind_group_layout,
            sampler,
            texture_pool,
            next_pool_index: 0,
            pool_frame_size: (1, 1),
            logged_size_mismatch: false,
        });
        // Request a redraw of whatever frame arrived before this setup finished (see
        // `last_frame`'s own doc comment for why this race is real, not hypothetical, and
        // `user_event`'s doc comment for why presentation itself happens in `RedrawRequested`,
        // never here directly) -- without this, a guest whose first page-flip lands early keeps a
        // permanently blank window until its NEXT flip, which may be much later or may never come
        // for a single-frame guest program.
        if self.last_frame.is_some()
            && let Some(state) = &self.state
        {
            state.window.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: PresenterCommand) {
        if let PresenterCommand::SetVisible(visible) = event {
            // Visibility is applied here, on the event-loop thread, because `winit` requires
            // window methods to be called from the thread that owns the loop -- the whole reason
            // this travels as an event rather than being a direct method call from the caller.
            if let Some(state) = &self.state {
                state.window.set_visible(visible);
                if visible {
                    // A window shown after frames have already arrived would otherwise stay blank
                    // until the guest's NEXT page-flip, which for an idle desktop can be a long
                    // time (an idle compositor legitimately flips zero times). Redraw immediately
                    // so showing the window presents the current scanout contents.
                    state.window.request_redraw();
                }
            }
            return;
        }
        // A `FrameSender::send` wake-up: the slot already holds only the most recent frame (see
        // `FrameSlot`'s own doc comment -- coalescing now happens on the SEND side, not by
        // draining a queue here), so just take it. Deliberately does NOT call `present()`
        // directly: `Surface::get_current_texture()` genuinely blocked (confirmed live,
        // AMD/Vulkan/Windows: reproducibly hung inside that one call with no error, no timeout, no
        // further progress) when invoked from an arbitrary event-loop callback rather than from
        // the window's own `RedrawRequested` -- every real wgpu+winit example routes presentation
        // through `RedrawRequested` for exactly this reason (it is the point `winit`'s own
        // platform backend guarantees the swapchain is in a presentable state), never from a
        // `user_event`/custom-event handler. This just stores the frame and asks the window to
        // redraw; `window_event`'s `RedrawRequested` arm does the actual `present()` call.
        let latest = self.slot.pending.lock().expect("frame slot mutex poisoned").take();
        if let Some(frame) = latest {
            if std::env::var_os("LITEBOX_DUMP_FRAMES").is_some() {
                dump_frame_diagnostic(&frame);
            }
            self.last_frame = Some(frame);
            if let Some(state) = &self.state {
                state.window.request_redraw();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(new_size) => {
                // Without this the surface stays configured at its ORIGINAL size forever: the
                // swapchain textures keep the old dimensions while the OS window does not, so
                // presentation is stretched/clipped by the compositor and `surface_size` (used
                // to bound the frame copy below) describes a window that no longer exists.
                //
                // This also keeps mouse tracking correct. `CursorMoved`'s handler scales its
                // window-pixel delta by `frame resolution / surface_size` so cursor motion
                // always covers the same fraction of the window that it covered on screen (see
                // that handler's own doc comment) -- but that scale factor is only correct while
                // `surface_size` actually matches the window. A stale value would scale against a
                // window that no longer exists, desyncing cursor tracking from the real window
                // the same way a stale render size would desync what's on screen.
                let Some(state) = &mut self.state else {
                    return;
                };
                if new_size.width == 0 || new_size.height == 0 {
                    // A minimised window reports 0x0; configuring a zero-sized surface is
                    // invalid, so keep the last good configuration until it is restored.
                    return;
                }
                state.surface_size = new_size;
                state.surface_config.width = new_size.width;
                state.surface_config.height = new_size.height;
                state.surface.configure(&state.device, &state.surface_config);
                state.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                // See `user_event`'s doc comment for why presentation happens HERE, not when a
                // frame first arrives: this is the one callback `winit` guarantees runs with the
                // surface in a state where `get_current_texture()` won't block.
                if let Some(frame) = self.last_frame.take() {
                    self.present(&frame);
                    self.last_frame = Some(frame);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let Some(consumer) = &self.input_consumer else {
                    return;
                };
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let Some(evdev_code) = winit_keycode_to_evdev(code) else {
                    return;
                };
                let value = match event.state {
                    // `winit` collapses OS-level auto-repeat into repeated `Pressed` events with
                    // `event.repeat == true` set, unlike real evdev's own three-state
                    // (0=released/1=pressed/2=repeat) `value` -- map that flag onto evdev's
                    // actual repeat value rather than sending a second, indistinguishable "press".
                    ElementState::Pressed if event.repeat => 2,
                    ElementState::Pressed => 1,
                    ElementState::Released => 0,
                };
                consumer(InputSignal::Key(evdev_code, value));
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(consumer) = &self.input_consumer else {
                    return;
                };
                let Some(evdev_code) = winit_mouse_button_to_evdev(button) else {
                    return;
                };
                let value = match state {
                    ElementState::Pressed => 1,
                    ElementState::Released => 0,
                };
                consumer(InputSignal::Key(evdev_code, value));
            }
            WindowEvent::CursorMoved { position, .. } => {
                let Some(consumer) = &self.input_consumer else {
                    return;
                };
                // Real evdev `EV_REL` motion is a signed DELTA since the last event, not an
                // absolute position (that's `EV_ABS`, not emitted by this pass -- see the module
                // doc comment). `winit`'s own `CursorMoved` reports the new absolute position, so
                // the delta is derived here against the last-seen position, matching what a real
                // mouse's own relative-motion sensor would have reported for the same movement.
                // `present()` CLIPS the guest framebuffer into the surface rather than scaling
                // it (see `present`'s own doc comment): the visible area is
                // `min(frame, surface_size)` guest pixels, top-left-anchored -- NOT the window's
                // full pixel size when the window is larger than the guest, and NOT the guest's
                // full resolution when the window is smaller. `CursorMoved` reports positions in
                // WINDOW pixels regardless of window size, so sending window-pixel deltas
                // straight through only tracks correctly when the window happens to exactly match
                // the guest's own resolution; at any other size the cursor drifts out of sync
                // with where the user is actually pointing. Scale the delta by
                // visible-guest-pixels-over-window-pixels (NOT guest-resolution-over-window-size
                // -- that direction was tried first and is backwards: for a window SMALLER than
                // the guest, scaling by the full guest/window ratio sends a delta that overshoots
                // past the visible crop into guest area that isn't drawn at all, exactly
                // reproducing the bug this fix exists to remove) so "move all the way across the
                // window" always means "move all the way across the CROP actually on screen",
                // matching the user's own requirement that on-screen mouse position track the
                // window regardless of shape.
                let (scale_x, scale_y) = self.last_frame.as_ref().map_or((1.0, 1.0), |frame| {
                    self.state.as_ref().map_or((1.0, 1.0), |state| {
                        let visible_w = frame.width.min(state.surface_size.width);
                        let visible_h = frame.height.min(state.surface_size.height);
                        (
                            f64::from(visible_w) / f64::from(state.surface_size.width.max(1)),
                            f64::from(visible_h) / f64::from(state.surface_size.height.max(1)),
                        )
                    })
                });
                if let Some((last_x, last_y)) = self.last_cursor_pos {
                    let scaled_x = position.x * scale_x;
                    let scaled_y = position.y * scale_y;
                    // A real mouse's per-event motion never approaches a delta anywhere near
                    // `i32`'s range, so this narrowing is exact in practice, not a real precision
                    // loss to guard against.
                    #[allow(clippy::cast_possible_truncation)]
                    let dx = (scaled_x - last_x) as i32;
                    #[allow(clippy::cast_possible_truncation)]
                    let dy = (scaled_y - last_y) as i32;
                    // ONE report for one physical movement, not two: see `InputSignal::RelMotion`.
                    // A zero delta on an axis is omitted by the receiver, and an all-zero move
                    // queues nothing at all.
                    if dx != 0 || dy != 0 {
                        consumer(InputSignal::RelMotion(dx, dy));
                    }
                    // Advance the reference by the whole GUEST pixels ACTUALLY SENT (already
                    // scaled), not by the raw scaled position. The cast above truncates toward
                    // zero, so storing `scaled_x`/`scaled_y` directly would discard the
                    // sub-pixel remainder permanently. Carrying it forward makes motion lossless:
                    // successive sub-pixel moves accumulate until they cross a whole pixel
                    // instead of vanishing.
                    //
                    // Measured against the previous behaviour: 25 moves of 0.4px (10px of real
                    // motion) delivered ZERO pixels to the guest; 10 moves of 3.7px (37px real)
                    // delivered 30px. Slow mouse movement was silently dropped entirely.
                    self.last_cursor_pos = Some((last_x + f64::from(dx), last_y + f64::from(dy)));
                } else {
                    self.last_cursor_pos = Some((position.x * scale_x, position.y * scale_y));
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let Some(consumer) = &self.input_consumer else {
                    return;
                };
                // Real `REL_WHEEL` steps are small signed integers (one physical detent = 1);
                // `winit`'s `LineDelta` already reports in that same unit on Windows (one visible
                // notch of a real mouse wheel = 1.0), so a straight cast (not a scale) is correct.
                // `PixelDelta` (high-resolution trackpad/precision-scroll input) has no clean
                // 1:1 mapping to discrete evdev wheel steps and is dropped rather than guessed at.
                if let winit::event::MouseScrollDelta::LineDelta(_, y) = delta {
                    // A real wheel's single-event step count is always tiny; see `dx`/`dy`'s
                    // identical rationale just above.
                    #[allow(clippy::cast_possible_truncation)]
                    let steps = y as i32;
                    if steps != 0 {
                        consumer(InputSignal::Rel(litebox_common_linux::REL_WHEEL, steps));
                    }
                }
            }
            _ => {}
        }
    }
}
