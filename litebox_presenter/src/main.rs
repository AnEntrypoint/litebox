// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! `litebox-presenter.exe` -- the presenter process, `docs/presenter-process-design.md` section
//! 1.2. Owns the Win32 window and `wgpu`/`winit` presentation code (kept verbatim in
//! `litebox_platform_windows_userland::presentation`, per section 1.3 -- this binary only changes
//! WHICH PROCESS runs it, not how it works). Links against NO guest-shim/kernel crate at all --
//! see that section's own reasoning for why this is what keeps a presenter crash from being able
//! to corrupt guest state even in principle.
//!
//! Frame source: [`presentation::FrameSender`]/[`presentation::Presenter`] are reused completely
//! unmodified from their previous in-process usage (they already accept frames via `send`,
//! regardless of where those bytes come from) -- only what FEEDS `FrameSender::send` changes, from
//! an in-process `DrmSubsystem` flip-callback closure to [`poll_scanout_and_feed`] below, which
//! polls the header section's `frame_seq` (section 2.4) and reads pixel bytes directly out of the
//! zero-copy-duplicated pixel section rather than a callback-supplied `&[u8]`.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use litebox_platform_windows_userland::presentation::{Frame, InputSignal, Presenter};
use litebox_presenter_protocol::pipe::{self, LineReader, PipeHandle};
use litebox_presenter_protocol::reply::{Reply, ReplyHeader, ScanoutReply};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Memory::{FILE_MAP_READ, MapViewOfFile};

/// Header layout mirror -- MUST match `litebox_runner_linux_on_windows_userland::control_server`'s
/// `HeaderLayout` byte-for-byte (both sides map the exact same section). Not shared via a common
/// crate since it is a private wire-compatible detail of exactly one runner/presenter pair, not
/// part of the pipe's own text protocol (`litebox_presenter_protocol`) -- only the numeric handle
/// values and initial geometry travel over the pipe itself (section 2.3); this layout is what
/// those handle values point AT.
#[repr(C)]
struct HeaderLayout {
    frame_seq: u64,
    width: u32,
    height: u32,
    pitch: u32,
    format: u32,
    offset_lo: u32,
    offset_hi: u32,
}

struct MappedSection {
    ptr: *mut c_void,
}
// SAFETY: every access to `ptr` in this file goes through `AtomicU64::from_ptr` (for the header's
// `frame_seq`) or explicit `read_volatile` calls, treating the memory as genuinely shared,
// concurrently-written-by-another-process state, never as a plain Rust reference that could
// alias.
unsafe impl Send for MappedSection {}
unsafe impl Sync for MappedSection {}

fn map_readonly(handle: HANDLE, offset: u64, size: usize) -> MappedSection {
    // SAFETY: `handle` is a section handle just duplicated into this process by the runner
    // (section 2.3), valid for at least `offset + size` bytes; mapping it read-only cannot alias
    // any existing Rust allocation since it is address space Windows itself is choosing fresh.
    let view = unsafe {
        MapViewOfFile(
            handle,
            FILE_MAP_READ,
            (offset >> 32) as u32,
            offset as u32,
            size,
        )
    };
    assert!(!view.Value.is_null(), "MapViewOfFile(read-only) failed");
    MappedSection { ptr: view.Value }
}

fn read_scanout_reply(pipe: &PipeHandle, reader: &mut LineReader) -> ScanoutReply {
    let line = reader
        .read_line(pipe)
        .expect("pipe I/O error reading scanout reply")
        .expect("runner closed the connection before replying to scanout");
    match Reply::parse_header(&line).expect("malformed scanout reply line") {
        ReplyHeader::Ok { tokens } => {
            ScanoutReply::parse(&tokens).expect("malformed scanout reply tokens")
        }
        ReplyHeader::Err { code, detail } => {
            panic!("scanout request failed: {code} {detail}");
        }
    }
}

/// Runs forever on a dedicated thread: polls the header's `frame_seq` (`Acquire`) and, on every
/// change, re-reads geometry from the header (risk 2: a mode change is visible only by re-reading
/// geometry on every `frame_seq` change, never trusting the one-time `scanout` reply after the
/// first frame) and copies `pitch*height` bytes out of the pixel section into a
/// [`FrameSender`]-recycled buffer, then sends it -- exactly the same "copy into a reused `Vec`,
/// then hand to the presenter" shape the old in-process flip-callback used, just sourced from a
/// mapped section instead of a callback-supplied slice.
///
/// Known limitation (disclosed, not silently assumed correct): this does not detect a REAL buffer
/// reallocation (a new, different `pixel_section_handle` value) mid-session -- only geometry
/// changes within the currently-mapped section are handled. `DrmSubsystem`'s virtual display is
/// fixed-resolution in this codebase today (see `drm.rs`'s own module doc comment), so this case
/// does not currently arise; a future mode-change-capable device would need this thread to also
/// periodically re-issue `scanout` and re-map when the handle value changes.
fn poll_scanout_and_feed(
    header: MappedSection,
    pixel: MappedSection,
    sender: litebox_platform_windows_userland::presentation::FrameSender,
) {
    let seq_ptr = header.ptr.cast::<HeaderLayout>().cast::<u64>();
    // SAFETY: `seq_ptr` is validly aligned (page-aligned mapping, `frame_seq` is the struct's
    // first field) and points at memory owned by `header` for this thread's whole lifetime.
    let seq_atomic = unsafe { &*AtomicU64::from_ptr(seq_ptr) };
    let mut last_seq = u64::MAX;
    loop {
        let seq = seq_atomic.load(Ordering::Acquire);
        if seq != last_seq {
            let base = header.ptr.cast::<HeaderLayout>();
            // SAFETY: `base` points at the same live header mapping `seq_atomic` reads from;
            // these fields are written by the runner's own header-publisher thread with the
            // geometry writes ordered-before the `frame_seq` release store we just acquired above,
            // so this read observes a coherent (if possibly one-generation-stale under a torn
            // read, per the design doc's own accepted-tearing note) snapshot.
            let (width, height, pitch) = unsafe {
                (
                    (&raw const (*base).width).read_volatile(),
                    (&raw const (*base).height).read_volatile(),
                    (&raw const (*base).pitch).read_volatile(),
                )
            };
            let len = (pitch as usize) * (height as usize);
            // SAFETY: `pixel.ptr` was mapped for at least this many bytes at connect time (the
            // initial `scanout` reply's own geometry); a genuine reallocation is this function's
            // documented known limitation, not assumed away silently for a size that grew instead.
            let bytes = unsafe { core::slice::from_raw_parts(pixel.ptr.cast::<u8>(), len) };
            let mut owned = sender.take_free_buffer();
            owned.clear();
            owned.extend_from_slice(bytes);
            sender.send(Frame {
                width,
                height,
                pitch,
                bytes: owned,
            });
            last_seq = seq;
        }
        // Matches the runner's own header-publisher cadence (~125Hz) -- polling faster than the
        // writer updates gains nothing.
        std::thread::sleep(Duration::from_millis(8));
    }
}

/// `litebox_runner_linux_on_windows_userland`'s own doc comments (`PRESENTER_THREAD_STACK_SIZE`,
/// now-deleted from that crate) record a confirmed-live `STATUS_STACK_OVERFLOW` in a debug build
/// running `Presenter::new()`/`resumed()`/`Presenter::run()` on a default 1 MiB stack -- wgpu/
/// winit's own deep, heavily-monomorphized call chains are dramatically more stack-hungry
/// unoptimized. That code ran on a SPAWNED thread it could size explicitly; this binary's `main()`
/// is a process's own primary thread, whose stack size is fixed at link time (1 MiB by default on
/// Windows) rather than adjustable via `std::thread::Builder`. Running the same wgpu/winit work
/// here needs the identical mitigation: do the real work on a spawned thread sized the same as
/// before, and let `main()` itself be a thin launcher.
const PRESENTER_THREAD_STACK_SIZE: usize = 256 * 1024 * 1024;

fn main() {
    let handle = std::thread::Builder::new()
        .name("litebox-presenter-main".to_owned())
        .stack_size(PRESENTER_THREAD_STACK_SIZE)
        .spawn(run)
        .expect("failed to spawn litebox-presenter-main thread");
    match handle.join() {
        Ok(()) => {}
        Err(e) => std::panic::resume_unwind(e),
    }
}

fn run() {
    let pipe_name = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: litebox-presenter.exe <pipe-name>");
        std::process::exit(2);
    });

    let pipe = std::sync::Arc::new(
        pipe::connect_client(&pipe_name, Duration::from_secs(10))
            .expect("failed to connect to runner control pipe"),
    );
    let mut reader = LineReader::new();

    pipe::write_line(&pipe, "scanout").expect("failed to write scanout request");
    let scanout = read_scanout_reply(&pipe, &mut reader);

    let header = map_readonly(
        scanout.header_section_handle as HANDLE,
        0,
        4096,
    );
    let pixel = map_readonly(
        scanout.pixel_section_handle as HANDLE,
        scanout.offset,
        (scanout.pitch as usize) * (scanout.height as usize),
    );

    let mut presenter = Presenter::new().expect("failed to create presenter window/event loop");
    // Start hidden: the runner's `show`/`hide` control-channel commands (pushed over this same
    // connection, see the reader thread below) are the sole source of truth for visibility now --
    // `--gui`'s own implicit `show` (docs/presenter-process-design.md section 4.1) arrives as an
    // ordinary pushed `show` line moments after this process is spawned, not as a startup
    // parameter to this binary.
    presenter = presenter.hidden_at_startup();

    let write_lock = std::sync::Arc::new(Mutex::new(()));
    let input_pipe = pipe.clone();
    let input_write_lock = write_lock.clone();
    presenter.set_input_consumer(move |signal| {
        let line = match signal {
            InputSignal::Key(code, value) => format!("key {code} {value}"),
            InputSignal::Rel(code, value) => format!("rel {code} {value}"),
            // No dedicated wire command for a single 2D motion report (section 3.2 only defines
            // `rel` for one evdev code/value pair at a time) -- send it as the two `rel` lines the
            // wire format actually supports; the runner's own `push_input_rel` calls funnel into
            // the same evdev queue either way.
            InputSignal::RelMotion(dx, dy) => {
                let _guard = input_write_lock.lock().expect("pipe write lock poisoned");
                let _ = pipe::write_line(&input_pipe, &format!("rel {} {dx}", litebox_common_linux::REL_X));
                let _ = pipe::write_line(&input_pipe, &format!("rel {} {dy}", litebox_common_linux::REL_Y));
                return;
            }
        };
        let _guard = input_write_lock.lock().expect("pipe write lock poisoned");
        let _ = pipe::write_line(&input_pipe, &line);
    });

    let sender = presenter.sender();

    // Background thread: watches for the runner PUSHING `show`/`hide` unprompted over this same
    // connection (docs/presenter-process-design.md section 3.2's `show`/`hide` semantics -- a
    // caller issuing them to the runner needs the ALREADY-CONNECTED presenter's window toggled,
    // not a new connection). See this crate's module doc comment for why a bare `show`/`hide`
    // line is unambiguous against an `ok`/`err ...` reply to something this process itself sent.
    let watch_pipe = pipe.clone();
    let watch_sender = sender.clone();
    std::thread::Builder::new()
        .name("litebox-presenter-pipe-reader".to_owned())
        .spawn(move || {
            let mut reader = LineReader::new();
            loop {
                match reader.read_line(&watch_pipe) {
                    Ok(Some(line)) => match line.trim() {
                        "show" => watch_sender.set_visible(true),
                        "hide" => watch_sender.set_visible(false),
                        // Everything else is a reply to a `key`/`rel` request this process itself
                        // sent (always `ok`) -- nothing further to do with it.
                        _ => {}
                    },
                    Ok(None) | Err(_) => {
                        // Runner gone (process exited or crashed): nothing left to present to.
                        std::process::exit(0);
                    }
                }
            }
        })
        .expect("failed to spawn litebox-presenter-pipe-reader thread");

    // Background thread: feeds `sender` from the mapped scanout section.
    std::thread::Builder::new()
        .name("litebox-presenter-scanout-poll".to_owned())
        .spawn(move || poll_scanout_and_feed(header, pixel, sender))
        .expect("failed to spawn litebox-presenter-scanout-poll thread");

    // Readiness signal (section 5 risk 3): first successful `scanout` (above) plus event-loop/
    // window construction (`Presenter::new()`, above) are both done by this point -- the real
    // Win32 `Window` itself is created lazily inside `resumed()` on the first tick of
    // `presenter.run()` below, moments from now, so this is a close (not lagging-by-long)
    // approximation of "window created" rather than a signal plumbed out of `resumed()` itself.
    {
        let _guard = write_lock.lock().expect("pipe write lock poisoned");
        pipe::write_line(&pipe, "ready").expect("failed to send ready to runner");
    }

    if let Err(e) = presenter.run() {
        eprintln!("[litebox-presenter] event loop exited with an error: {e}");
    }
}
