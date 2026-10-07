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

/// Sends `scanout` and returns the parsed reply, retrying every 250ms on `err bad_state` (the
/// guest has not attached a framebuffer yet -- an entirely normal state for a presenter that
/// connected before the guest got around to any `SETCRTC`/`PAGE_FLIP`/`ADDFB2` call, e.g.
/// `--gui`/`--gui=hidden` at startup racing the guest's own boot) up to `retry_budget`. The
/// runner's own `show` handler applies a 5s timeout waiting for this process's `ready` line
/// (section 5 risk 3), so in the common "guest never draws anything" case THAT timeout fires
/// first and reports a clean `err` back to whoever asked for `show` -- this loop's own longer
/// budget exists only so a presenter started well before a slow-booting guest (e.g.
/// `--gui=hidden` at process launch) does not need to be told to retry from outside.
///
/// # Panics
///
/// Panics on any other `err` code (a real, non-transient failure) or if `retry_budget` elapses.
fn scanout_with_retry(
    pipe: &PipeHandle,
    write_lock: &Mutex<()>,
    reader: &mut LineReader,
    retry_budget: Duration,
) -> ScanoutReply {
    let deadline = std::time::Instant::now() + retry_budget;
    loop {
        {
            let _guard = write_lock.lock().expect("pipe write lock poisoned");
            pipe::write_line(pipe, "scanout").expect("failed to write scanout request");
        }
        let line = reader
            .read_line(pipe)
            .expect("pipe I/O error reading scanout reply")
            .expect("runner closed the connection before replying to scanout");
        match Reply::parse_header(&line).expect("malformed scanout reply line") {
            ReplyHeader::Ok { tokens } => {
                return ScanoutReply::parse(&tokens).expect("malformed scanout reply tokens");
            }
            ReplyHeader::Err {
                code: litebox_presenter_protocol::reply::ErrorCode::BadState,
                ..
            } => {
                if std::time::Instant::now() >= deadline {
                    panic!(
                        "no framebuffer attached after {retry_budget:?} of retrying scanout -- \
                         giving up"
                    );
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            ReplyHeader::Err { code, detail } => {
                panic!("scanout request failed: {code} {detail}");
            }
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
    // This binary spawns several worker threads (the connection reader/scanout-handshake thread,
    // the scanout-poll-and-feed thread) whose failure means the whole process has nothing further
    // to usefully do -- Rust's default behavior for a panic on a NON-main thread is to unwind and
    // terminate only that one thread, silently leaving every other thread (including the winit
    // event loop on the thread spawned just below) running with no one left driving the
    // connection. Confirmed live: this exact silent-half-death was produced during this feature's
    // own verification -- a `--gui=hidden` run whose guest exited before ever drawing anything
    // left `litebox-presenter.exe` running forever as an orphaned zombie process after the runner
    // exited, because `scanout_with_retry`'s "runner closed the connection" panic only killed its
    // own spawned thread, not the process. Installing a process-wide panic hook that exits after
    // the default hook prints is the standard fix for "a panic on any thread should end this
    // program", matching how a single-threaded program's panic already behaves.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        std::process::exit(1);
    }));

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

    // Event-loop/window construction happens FIRST, before scanout even succeeds once --
    // `--gui=hidden`'s whole point (section 4.1) is "the process and its window exist... so a
    // LATER show is fast, no cold-start wgpu/window-creation cost", which requires the window to
    // exist even while the guest has not yet attached any framebuffer (a presenter spawned at
    // process launch will usually race a slow-booting guest). Blocking window creation on the
    // first successful `scanout` -- this function's original shape -- would have left NO window
    // at all during that race, silently defeating that guarantee.
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
            // `relmotion` is the dedicated wire command for one 2D motion report (protocol
            // `Request::RelMotion`) -- sending it as two separate `rel` lines instead would give
            // each its own `SYN_REPORT` on the runner side (`push_input_rel` is per-line), making
            // a client process the same motion twice. See `Request::RelMotion`'s own doc comment.
            InputSignal::RelMotion(dx, dy) => {
                format!("relmotion {dx} {dy}")
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
    //
    // This SAME thread also performs the initial `scanout` handshake (with retry, see
    // `scanout_with_retry`'s doc comment) before falling into the forever show/hide watch loop --
    // both are reads on this one connection, and a Windows named pipe has no way to distinguish
    // "a reply to what I just sent" from "an unprompted push from the peer" at the transport
    // level (see this crate's own module doc comment), so exactly one thread must own every read
    // on this connection for its whole lifetime, never two racing readers.
    let watch_pipe = pipe.clone();
    let watch_sender = sender.clone();
    let watch_write_lock = write_lock.clone();
    std::thread::Builder::new()
        .name("litebox-presenter-pipe-reader".to_owned())
        .spawn(move || {
            let mut reader = LineReader::new();

            // 60s: generous relative to the runner's own 5s `show` timeout (see
            // `scanout_with_retry`'s doc comment) -- this budget only matters for a presenter
            // spawned well before the guest draws anything at all, e.g. `--gui=hidden` at process
            // launch racing a slow boot.
            let scanout = scanout_with_retry(
                &watch_pipe,
                &watch_write_lock,
                &mut reader,
                Duration::from_secs(60),
            );
            let header = map_readonly(scanout.header_section_handle as HANDLE, 0, 4096);
            let pixel = map_readonly(
                scanout.pixel_section_handle as HANDLE,
                scanout.offset,
                (scanout.pitch as usize) * (scanout.height as usize),
            );
            // Feeds `watch_sender` from the mapped scanout section on its own thread -- this
            // never touches the pipe (it only reads the mapped section and calls
            // `FrameSender::send`, an in-process channel), so it cannot race this thread's own
            // pipe reads below.
            let feed_sender = watch_sender.clone();
            std::thread::Builder::new()
                .name("litebox-presenter-scanout-poll".to_owned())
                .spawn(move || poll_scanout_and_feed(header, pixel, feed_sender))
                .expect("failed to spawn litebox-presenter-scanout-poll thread");

            // Readiness signal (section 5 risk 3): first successful `scanout` (just above) plus
            // event-loop/window construction (`Presenter::new()`, before this thread was spawned)
            // are both done by this point -- the real Win32 `Window` itself is created lazily
            // inside `resumed()` on the first tick of `presenter.run()`, which by now has already
            // been running on the main presenter thread for as long as `scanout_with_retry` took,
            // so this is a close (not lagging-by-long) approximation of "window created" rather
            // than a signal plumbed out of `resumed()` itself.
            {
                let _guard = watch_write_lock.lock().expect("pipe write lock poisoned");
                pipe::write_line(&watch_pipe, "ready").expect("failed to send ready to runner");
            }

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

    if let Err(e) = presenter.run() {
        eprintln!("[litebox-presenter] event loop exited with an error: {e}");
    }
}
