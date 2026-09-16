// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! `ControlServer`: the named-pipe listener implementing `docs/presenter-process-design.md`
//! section 3's command grammar, plus the small header section that mirrors the guest's current
//! scanout geometry/`frame_seq` for a presenter process to poll (section 2.4).
//!
//! Started UNCONDITIONALLY (headless or `--gui`/`--gui=hidden`) by `lib.rs`, replacing the old
//! `gui_presenter_thread` closure (section 4.3). Owns no window, no wgpu, no COM -- only pipe I/O,
//! a couple of small Win32 memory-section calls, and `std::process::Command` to spawn
//! `litebox-presenter.exe`.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use litebox_platform_windows_userland::WindowsUserland as Platform;
use litebox_presenter_protocol::pipe::{self, LineReader, PipeHandle};
use litebox_presenter_protocol::reply::{ErrorCode, Reply, ScanoutReply};
use litebox_presenter_protocol::request::Request;
use litebox_shim_linux::{LinuxShim, ShimFS};

use windows_sys::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE,
    UnmapViewOfFile,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE};

/// Layout of the small header section a presenter maps read-only and polls (section 2.4). Kept
/// distinct from the pixel section so header polling never contends with the (potentially much
/// larger) pixel data's own page mappings. `frame_seq` is written last (`Release`) after every
/// other field, and a reader must load it first (`Acquire`) and only trust the other fields after
/// observing it change -- this is what makes a torn read of the geometry fields impossible to
/// observe without also observing a stale `frame_seq` (risk 2: geometry must be re-read on every
/// `frame_seq` change, not just once at `scanout` time).
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

const HEADER_SECTION_SIZE: usize = 4096;

struct HeaderSection {
    handle: HANDLE,
    ptr: *mut c_void,
}
// SAFETY: `handle` is a plain kernel-object identifier (no thread affinity); `ptr` points at a
// process-wide mapped view that every access here treats as shared, cross-thread mutable state
// via explicit atomics/volatile writes, never through a `&mut` that could alias.
unsafe impl Send for HeaderSection {}
unsafe impl Sync for HeaderSection {}

impl HeaderSection {
    fn create() -> std::io::Result<Self> {
        // SAFETY: all arguments are plain values with no aliasing/lifetime requirements; the
        // paging-file-backed (no real file) section this creates matches
        // `litebox_platform_windows_userland`'s own `create_shared_memory` convention.
        let handle = unsafe {
            CreateFileMappingW(
                windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
                core::ptr::null(),
                PAGE_READWRITE,
                0,
                HEADER_SECTION_SIZE as u32,
                core::ptr::null(),
            )
        };
        if handle.is_null() {
            // SAFETY: `GetLastError` has no preconditions.
            return Err(std::io::Error::from_raw_os_error(unsafe {
                GetLastError().cast_signed()
            }));
        }
        // SAFETY: `handle` was just created above with `PAGE_READWRITE`, so a read-write mapping
        // of its full size is valid.
        let ptr = unsafe { MapViewOfFile(handle, FILE_MAP_WRITE | FILE_MAP_READ, 0, 0, HEADER_SECTION_SIZE) };
        if ptr.Value.is_null() {
            // SAFETY: `GetLastError` has no preconditions; `handle` is still owned (not yet
            // wrapped) so closing it here on this error path is correct.
            let err = unsafe { GetLastError() };
            unsafe {
                CloseHandle(handle);
            }
            return Err(std::io::Error::from_raw_os_error(err.cast_signed()));
        }
        Ok(Self {
            handle,
            ptr: ptr.Value,
        })
    }

    fn seq_ptr(&self) -> *const AtomicU64 {
        self.ptr.cast::<HeaderLayout>().cast_const().cast::<AtomicU64>()
    }

    /// Writes the current snapshot into the header, geometry first, `frame_seq` last with
    /// `Release` ordering -- see [`HeaderLayout`]'s own doc comment for why the order matters.
    fn publish(&self, width: u32, height: u32, pitch: u32, format: u32, offset: u64, seq: u64) {
        let base = self.ptr.cast::<HeaderLayout>();
        // SAFETY: `base` points at a `HEADER_SECTION_SIZE`-byte mapped view (>= `size_of::<HeaderLayout>()`)
        // owned by this `HeaderSection` for its whole lifetime; only this method and
        // `AtomicU64::from_ptr` below ever touch it, and this method is only ever called from the
        // single dedicated polling thread (see `spawn_header_publisher`), so no concurrent writer
        // exists to race against.
        unsafe {
            (&raw mut (*base).width).write_volatile(width);
            (&raw mut (*base).height).write_volatile(height);
            (&raw mut (*base).pitch).write_volatile(pitch);
            (&raw mut (*base).format).write_volatile(format);
            (&raw mut (*base).offset_lo).write_volatile(offset as u32);
            (&raw mut (*base).offset_hi).write_volatile((offset >> 32) as u32);
        }
        // SAFETY: `seq_ptr()` is validly aligned (the section is page-aligned, `frame_seq` is the
        // struct's first field) and points at live, owned memory for this object's lifetime.
        unsafe { &*self.seq_ptr() }.store(seq, Ordering::Release);
    }
}

impl Drop for HeaderSection {
    fn drop(&mut self) {
        // SAFETY: `self.ptr`/`self.handle` were established together in `create` and never
        // handed to anything that outlives this `HeaderSection`.
        unsafe {
            UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.ptr,
            });
            CloseHandle(self.handle);
        }
    }
}

/// The one currently-registered presenter connection, if any -- see this module's doc comment for
/// why a fresh, same-process-duplicated `HANDLE` (not a shared reference to the connection
/// handler's own owned [`PipeHandle`]) is what gets stored here: it lets `show`/`hide` write into
/// the presenter's connection from a different thread with no risk of writing through a value
/// some OTHER thread has already closed and Windows has since recycled.
struct PresenterConn(PipeHandle);

pub struct Shared<FS: ShimFS> {
    shim: LinuxShim<Platform, FS>,
    header: HeaderSection,
    presenter: Mutex<Option<PresenterConn>>,
    presenter_ready: AtomicBool,
    presenter_visible: AtomicBool,
    presenter_exe: PathBuf,
    pipe_name: String,
    frames_enabled: AtomicBool,
    frames_dir: Mutex<Option<PathBuf>>,
    /// Whether the `LITEBOX_DUMP_FRAMES`-equivalent flip-callback observer has EVER been
    /// registered in this process's lifetime -- see [`ensure_frames_callback_registered`] for why
    /// this must stay a one-way latch, never re-checked against `frames_enabled` itself.
    frames_callback_registered: AtomicBool,
}

/// Registers the frame-dump flip-callback observer the first time it is ever needed (either at
/// startup, if `LITEBOX_DUMP_FRAMES` was set, or lazily on the first runtime `frames on`), and
/// never again. This one-time-registration-then-flag-gated-body split is what keeps a plain
/// headless run that never asks for frame dumping at all exactly as cheap as before this change:
/// `DrmSubsystem::notify_flip_callback` maps and hands back the WHOLE pixel buffer on every flip
/// as soon as ANY observer is registered, regardless of what that observer's body then does with
/// it, so the only way to add zero cost to the "never asked for it" case is to never call
/// `add_drm_flip_callback` for this purpose at all in that case (`docs/presenter-process-design.md`
/// section 4.4). Once dumping has been asked for even once, the observer (and its per-flip
/// mapping cost) stays registered for the rest of this process's life -- there is no
/// `remove_flip_callback` primitive, matching every other observer registered this way.
fn ensure_frames_callback_registered<FS: ShimFS>(shared: &Arc<Shared<FS>>) {
    if shared
        .frames_callback_registered
        .swap(true, Ordering::AcqRel)
    {
        return; // already registered by a previous call.
    }
    let shared_for_closure = shared.clone();
    shared.shim.add_drm_flip_callback(move |bytes, width, height, pitch, _pixel_format| {
        if !shared_for_closure.frames_enabled.load(Ordering::Acquire) {
            return;
        }
        let frame = litebox_platform_windows_userland::presentation::Frame {
            width,
            height,
            pitch,
            bytes: bytes.to_vec(),
        };
        litebox_platform_windows_userland::presentation::dump_frame_diagnostic(&frame);
    });
}

fn duplicate_into_current_process(handle: HANDLE) -> std::io::Result<HANDLE> {
    let mut dup: HANDLE = core::ptr::null_mut();
    // SAFETY: `handle` is a valid, currently-open handle in this process; `dup` is a valid
    // out-pointer.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &raw mut dup,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        // SAFETY: `GetLastError` has no preconditions.
        return Err(std::io::Error::from_raw_os_error(unsafe {
            GetLastError().cast_signed()
        }));
    }
    Ok(dup)
}

/// Duplicates `handle` (owned by this process) into `target_pid`'s handle table, per
/// `docs/presenter-process-design.md` section 2.3 -- no admin rights or `SeDebugPrivilege`
/// needed for a same-user, non-elevated sibling process. Returns the numeric value, meaningful
/// ONLY inside `target_pid`'s own process.
fn duplicate_into_process(handle: HANDLE, target_pid: u32) -> std::io::Result<u64> {
    // SAFETY: `target_pid` is a plain value; no preconditions beyond what `OpenProcess` itself
    // documents.
    let target = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, target_pid) };
    if target.is_null() {
        // SAFETY: `GetLastError` has no preconditions.
        return Err(std::io::Error::from_raw_os_error(unsafe {
            GetLastError().cast_signed()
        }));
    }
    let mut dup: HANDLE = core::ptr::null_mut();
    // SAFETY: `handle` is a valid, currently-open handle in this process; `target` was just
    // opened above with `PROCESS_DUP_HANDLE`; `dup` is a valid out-pointer.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            target,
            &raw mut dup,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    // SAFETY: `target` is a still-owned, valid handle not used again after this.
    unsafe {
        CloseHandle(target);
    }
    if ok == 0 {
        // SAFETY: `GetLastError` has no preconditions.
        return Err(std::io::Error::from_raw_os_error(unsafe {
            GetLastError().cast_signed()
        }));
    }
    Ok(dup as u64)
}

/// Runs forever on a dedicated thread, polling [`LinuxShim::drm_scanout_snapshot`] (a cheap
/// lock+`BTreeMap` lookup, never a buffer mapping) and re-publishing the header whenever `seq`
/// advances. Deliberately NOT implemented as a `DrmSubsystem` flip-callback observer: that
/// mechanism (`add_drm_flip_callback`) unconditionally maps and hands back the WHOLE pixel
/// buffer's bytes on every flip once ANY observer is registered (see
/// `DrmSubsystem::notify_flip_callback`) -- registering one here to maintain the header would
/// impose that same per-flip mapping cost on every headless run with no presenter and no
/// `LITEBOX_DUMP_FRAMES`, directly contradicting section 4.4's "effectively free... strictly
/// cheaper than today's headless path" requirement. Polling `scanout_snapshot()` on its own
/// timer avoids that entirely: no buffer is ever mapped just to maintain the header.
fn spawn_header_publisher<FS: ShimFS>(shared: Arc<Shared<FS>>) {
    std::thread::Builder::new()
        .name("litebox-control-header".to_owned())
        .spawn(move || {
            let mut last_seq = u64::MAX;
            loop {
                if let Some(snap) = shared.shim.drm_scanout_snapshot()
                    && snap.seq != last_seq
                {
                    shared.header.publish(
                        snap.width,
                        snap.height,
                        snap.pitch,
                        snap.pixel_format,
                        snap.offset,
                        snap.seq,
                    );
                    last_seq = snap.seq;
                }
                // ~125Hz: comfortably above any real display refresh rate this virtual device
                // will ever be driven at, cheap (one mutex lock plus a `BTreeMap` lookup per
                // tick), and matches the design doc's own "poll at the window's own vsync/redraw
                // cadence" framing from the OTHER (presenter-side) end of this same header.
                std::thread::sleep(Duration::from_millis(8));
            }
        })
        .expect("failed to spawn litebox-control-header thread");
}

fn spawn_presenter_process(shared: &Shared<impl ShimFS>) -> std::io::Result<()> {
    // Explicit `Stdio::null()`, not the default inherited stdio: the presenter is an independent
    // background process (its whole point, per section 1.2, is that it shares no address space
    // or lifetime coupling with the runner) -- inheriting the runner's own stdout/stderr handles
    // means, if those happen to be redirected (a log file, a pipe to another tool), the presenter
    // keeps that redirection's write end open for as long as IT runs, which can stall the
    // runner-side redirection from ever seeing EOF/closing cleanly even after the runner itself
    // exits (confirmed live during this change's own verification: a `--gui=hidden` run under
    // `-RedirectStandardOutput`/`-RedirectStandardError` produced truncated runner output because
    // the still-running presenter held the same pipe open). Matches
    // `litebox_session_daemon::client::connect_or_spawn`'s identical `Stdio::null()` choice for
    // its own auto-spawned, independent daemon process.
    std::process::Command::new(&shared.presenter_exe)
        .arg(&shared.pipe_name)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

/// Blocks (polling every 50ms) until a presenter is registered and has sent `ready`, or `timeout`
/// elapses. Matches section 5 risk 3's proposed 5s default -- callers of this function pass that
/// in explicitly.
fn wait_for_presenter_ready(shared: &Shared<impl ShimFS>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if shared.presenter_ready.load(Ordering::Acquire) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn push_to_presenter(shared: &Shared<impl ShimFS>, line: &str) -> Result<(), Reply> {
    let guard = shared.presenter.lock().expect("presenter mutex poisoned");
    match &*guard {
        Some(PresenterConn(handle)) => pipe::write_line(handle, line).map_err(|e| {
            Reply::err(ErrorCode::IoError, format!("presenter write failed: {e}"))
        }),
        None => Err(Reply::err(ErrorCode::NotFound, "no presenter connected")),
    }
}

fn handle_show<FS: ShimFS>(shared: &Arc<Shared<FS>>) -> Reply {
    if shared.presenter.lock().expect("presenter mutex poisoned").is_none() {
        if let Err(e) = spawn_presenter_process(shared) {
            return Reply::err(
                ErrorCode::IoError,
                format!("failed to spawn litebox-presenter.exe: {e}"),
            );
        }
        // 5s default per section 5 risk 3.
        if !wait_for_presenter_ready(shared, Duration::from_secs(5)) {
            return Reply::err(ErrorCode::IoError, "presenter did not start");
        }
    } else if !wait_for_presenter_ready(shared, Duration::from_secs(5)) {
        return Reply::err(ErrorCode::IoError, "presenter did not start");
    }
    match push_to_presenter(shared, "show") {
        Ok(()) => {
            shared.presenter_visible.store(true, Ordering::Release);
            Reply::Ok
        }
        Err(e) => e,
    }
}

fn handle_hide<FS: ShimFS>(shared: &Arc<Shared<FS>>) -> Reply {
    match push_to_presenter(shared, "hide") {
        Ok(()) => {
            shared.presenter_visible.store(false, Ordering::Release);
            Reply::Ok
        }
        Err(e) => e,
    }
}

fn handle_scanout<FS: ShimFS>(shared: &Arc<Shared<FS>>, client_pid: u32) -> Reply {
    let Some(snap) = shared.shim.drm_scanout_snapshot() else {
        return Reply::err(ErrorCode::BadState, "no framebuffer attached yet");
    };
    // `ScanoutSnapshot::handle` is `Platform::SharedMemoryHandle = usize` on Windows, which is
    // exactly the raw `CreateFileMappingW` `HANDLE` value cast to `usize` (see that type's own
    // doc comment in `litebox_platform_windows_userland`).
    let pixel_handle = snap.handle as HANDLE;
    let pixel_dup = match duplicate_into_process(pixel_handle, client_pid) {
        Ok(v) => v,
        Err(e) => {
            return Reply::err(
                ErrorCode::IoError,
                format!("DuplicateHandle(pixel section) failed: {e}"),
            );
        }
    };
    let header_dup = match duplicate_into_process(shared.header.handle, client_pid) {
        Ok(v) => v,
        Err(e) => {
            return Reply::err(
                ErrorCode::IoError,
                format!("DuplicateHandle(header section) failed: {e}"),
            );
        }
    };
    Reply::ok_tokens(
        ScanoutReply {
            pixel_section_handle: pixel_dup,
            header_section_handle: header_dup,
            width: snap.width,
            height: snap.height,
            pitch: snap.pitch,
            format: snap.pixel_format,
            offset: snap.offset,
            seq: snap.seq,
        }
        .to_tokens(),
    )
}

fn handle_screenshot<FS: ShimFS>(shared: &Arc<Shared<FS>>, path: &str) -> Reply {
    let Some(snap) = shared.shim.drm_scanout_snapshot() else {
        return Reply::err(ErrorCode::BadState, "no framebuffer attached yet");
    };
    // SAFETY: `snap.handle` is a live section handle at least `snap.size` bytes long, owned by
    // the guest's `DrmSubsystem` for as long as the buffer exists (which it does right now, since
    // we just read this snapshot from it under its own lock); mapping our OWN temporary read-only
    // view of it here is exactly what a presenter process would otherwise do with a duplicated
    // copy of the same handle value -- the runner needs no duplication since it already owns the
    // handle outright.
    let view = unsafe {
        MapViewOfFile(
            snap.handle as HANDLE,
            FILE_MAP_READ,
            (snap.offset >> 32) as u32,
            snap.offset as u32,
            snap.size,
        )
    };
    if view.Value.is_null() {
        // SAFETY: `GetLastError` has no preconditions.
        let err = unsafe { GetLastError() };
        return Reply::err(
            ErrorCode::IoError,
            format!("failed to map scanout section for screenshot: win32 error {err}"),
        );
    }
    // SAFETY: `view.Value` was just established above as a valid read-only mapping of
    // `snap.size` bytes; it is not unmapped until immediately after this slice's last use.
    let bytes = unsafe { core::slice::from_raw_parts(view.Value.cast::<u8>(), snap.size) };
    let (non_black_pixels, distinct_colors) = litebox_platform_windows_userland::presentation::count_pixel_stats(
        snap.width as usize,
        snap.height as usize,
        snap.pitch as usize,
        bytes,
    );
    let encoded = litebox_platform_windows_userland::presentation::encode_bmp(
        snap.width as usize,
        snap.height as usize,
        snap.pitch as usize,
        bytes,
    );
    // SAFETY: `view.Value` is exactly the range mapped above; every reader (`count_pixel_stats`/
    // `encode_bmp`, both already returned) has finished by this point.
    unsafe {
        UnmapViewOfFile(view);
    }
    let write_result = std::fs::write(path, &encoded);
    match write_result {
        Ok(()) => Reply::ok_tokens(vec![
            path.to_owned(),
            non_black_pixels.to_string(),
            distinct_colors.to_string(),
        ]),
        Err(e) => Reply::err(ErrorCode::IoError, format!("{path}: {e}")),
    }
}

fn handle_ps() -> Reply {
    let mut buf = String::new();
    litebox_shim_linux::diag::print_process_tree(|s| buf.push_str(s));
    let lines: Vec<String> = buf.lines().map(str::to_owned).collect();
    Reply::OkLines(lines)
}

fn handle_strace(req: &Request) -> Reply {
    match req {
        Request::StraceOn => {
            litebox_shim_linux::diag::set_strace_summary_enabled(true);
            Reply::Ok
        }
        Request::StraceOff => {
            litebox_shim_linux::diag::set_strace_summary_enabled(false);
            Reply::Ok
        }
        Request::StraceQuery => Reply::ok_tokens(vec![
            (if litebox_shim_linux::diag::strace_summary_enabled() {
                "on"
            } else {
                "off"
            })
            .to_owned(),
        ]),
        Request::StraceDump => {
            let mut buf = String::new();
            litebox_shim_linux::diag::print_strace_summary(|s| buf.push_str(s));
            let lines: Vec<String> = buf.lines().map(str::to_owned).collect();
            Reply::OkLines(lines)
        }
        _ => unreachable!("handle_strace only ever called with a Strace* request"),
    }
}

fn handle_frames<FS: ShimFS>(shared: &Arc<Shared<FS>>, req: &Request) -> Reply {
    match req {
        Request::FramesOn { dir } => {
            *shared.frames_dir.lock().expect("frames_dir mutex poisoned") =
                Some(PathBuf::from(dir));
            shared.frames_enabled.store(true, Ordering::Release);
            ensure_frames_callback_registered(shared);
            Reply::Ok
        }
        Request::FramesOff => {
            shared.frames_enabled.store(false, Ordering::Release);
            Reply::Ok
        }
        _ => unreachable!("handle_frames only ever called with a Frames* request"),
    }
}

fn dispatch<FS: ShimFS>(
    shared: &Arc<Shared<FS>>,
    client_pid: u32,
    became_presenter: &mut bool,
    req: Request,
) -> Reply {
    match req {
        Request::Scanout => handle_scanout(shared, client_pid),
        Request::Screenshot { path } => handle_screenshot(shared, &path),
        Request::Show => handle_show(shared),
        Request::Hide => handle_hide(shared),
        Request::PresenterQuery => {
            let connected = shared.presenter.lock().expect("presenter mutex poisoned").is_some();
            let state = if !connected {
                "none"
            } else if shared.presenter_visible.load(Ordering::Acquire) {
                "visible"
            } else {
                "hidden"
            };
            Reply::ok_tokens(vec![state.to_owned()])
        }
        Request::Key { code, value } => {
            shared.shim.push_input_key(code, i32::from(value));
            Reply::Ok
        }
        Request::Rel { code, value } => {
            shared.shim.push_input_rel(code, value);
            Reply::Ok
        }
        Request::RelMotion { dx, dy } => {
            shared.shim.push_input_rel_motion(dx, dy);
            Reply::Ok
        }
        Request::Abs { .. } => Reply::err(ErrorCode::Unsupported, "abs is reserved, not yet implemented"),
        Request::Ps => handle_ps(),
        Request::StraceOn | Request::StraceOff | Request::StraceQuery | Request::StraceDump => {
            handle_strace(&req)
        }
        Request::FramesOn { .. } | Request::FramesOff => handle_frames(shared, &req),
        Request::Ready => {
            *became_presenter = true;
            shared.presenter_ready.store(true, Ordering::Release);
            Reply::Ok
        }
    }
}

fn handle_connection<FS: ShimFS>(shared: Arc<Shared<FS>>, pipe: PipeHandle) {
    let Ok(client_pid) = pipe::client_process_id(&pipe) else {
        return;
    };
    let mut reader = LineReader::new();
    let mut became_presenter = false;
    loop {
        let line = match reader.read_line(&pipe) {
            Ok(Some(l)) => l,
            Ok(None) | Err(_) => break,
        };
        let reply = match Request::parse(&line) {
            Ok(req) => dispatch(&shared, client_pid, &mut became_presenter, req),
            Err(e) => Reply::err(ErrorCode::IoError, e.0),
        };
        // This connection just became the registered presenter (its most recent request was
        // `ready`): hand a fresh, independently-owned duplicate of `pipe`'s handle to `Shared` so
        // `show`/`hide` from OTHER connections can write into it, then continue this loop reading
        // further requests (`key`/`rel`) from `pipe` directly, unlocked -- see this module's doc
        // comment for why the two handle values are kept independent.
        if became_presenter && shared.presenter.lock().expect("presenter mutex poisoned").is_none() {
            if let Ok(dup) = duplicate_into_current_process(pipe.raw()) {
                *shared.presenter.lock().expect("presenter mutex poisoned") =
                    Some(PresenterConn(
                        // SAFETY: `dup` was just freshly duplicated above by
                        // `duplicate_into_current_process`; nothing else holds or will close this
                        // specific value.
                        unsafe { PipeHandle::from_raw(dup) },
                    ));
            }
        }
        for l in reply.to_lines() {
            if pipe::write_line(&pipe, &l).is_err() {
                became_presenter = false; // fall through to cleanup below
                break;
            }
        }
    }
    if became_presenter {
        *shared.presenter.lock().expect("presenter mutex poisoned") = None;
        shared.presenter_ready.store(false, Ordering::Release);
        shared.presenter_visible.store(false, Ordering::Release);
    }
}

/// `--gui`/`--gui=hidden` startup per section 4.1: spawns `litebox-presenter.exe` and, for
/// [`crate::GuiMode::Shown`] only, blocks (up to 5s, matching [`handle_show`]) until it is
/// connected and ready, then pushes `show`. For [`crate::GuiMode::Hidden`] the process and window
/// are created (so a LATER `show` is fast) but this call returns as soon as the spawn itself
/// succeeds -- it does not block on `ready`, matching `--gui=hidden`'s own "no cold-start cost on
/// the later show" rationale (a caller that wants to confirm readiness first can issue
/// `presenter?`/`show` itself).
///
/// # Errors
///
/// Returns an error if spawning `litebox-presenter.exe` fails, or (for
/// [`crate::GuiMode::Shown`]) if it never becomes ready within 5s.
pub fn spawn_and_maybe_show<FS: ShimFS>(shared: &Arc<Shared<FS>>, mode: crate::GuiMode) -> std::io::Result<()> {
    spawn_presenter_process(shared)?;
    match mode {
        crate::GuiMode::Hidden => Ok(()),
        crate::GuiMode::Shown => {
            if !wait_for_presenter_ready(shared, Duration::from_secs(5)) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "litebox-presenter.exe did not become ready within 5s",
                ));
            }
            match push_to_presenter(shared, "show") {
                Ok(()) => {
                    shared.presenter_visible.store(true, Ordering::Release);
                    Ok(())
                }
                Err(Reply::Err { detail, .. }) => Err(std::io::Error::other(detail)),
                Err(_) => Err(std::io::Error::other("show failed")),
            }
        }
    }
}

/// Whether a presenter is currently connected -- used by `lib.rs` at end-of-run to wait for an
/// interactively-shown presenter's window to be closed by the user (mirroring the old
/// `gui_presenter_thread` join's "keep the process alive until the window closes" behavior) before
/// the runner process itself exits.
#[must_use]
pub fn is_presenter_connected<FS: ShimFS>(shared: &Shared<FS>) -> bool {
    shared.presenter.lock().expect("presenter mutex poisoned").is_some()
}

/// Starts the `ControlServer` accept loop on a dedicated thread and returns immediately --
/// matches `docs/presenter-process-design.md` section 4.3's "always" requirement (headless or
/// not). `presenter_exe` is the path to `litebox-presenter.exe` used for every `show`-triggered
/// spawn.
pub fn start<FS: ShimFS>(shim: LinuxShim<Platform, FS>, presenter_exe: PathBuf) -> std::io::Result<Arc<Shared<FS>>> {
    let runner_pid = std::process::id();
    let pipe_name = pipe::pipe_name(runner_pid);
    let header = HeaderSection::create()?;
    let shared = Arc::new(Shared {
        shim,
        header,
        presenter: Mutex::new(None),
        presenter_ready: AtomicBool::new(false),
        presenter_visible: AtomicBool::new(false),
        presenter_exe,
        pipe_name: pipe_name.clone(),
        frames_enabled: AtomicBool::new(std::env::var_os("LITEBOX_DUMP_FRAMES").is_some()),
        frames_dir: Mutex::new(None),
        frames_callback_registered: AtomicBool::new(false),
    });
    if std::env::var_os("LITEBOX_DUMP_FRAMES").is_some() {
        // Exact same registration timing as before this change (this function runs at the same
        // point in `lib.rs`'s startup sequence the old, now-deleted `LITEBOX_DUMP_FRAMES` block
        // did), so a `LITEBOX_DUMP_FRAMES=1` run's behavior is unchanged (section 5 risk 5).
        ensure_frames_callback_registered(&shared);
    }
    spawn_header_publisher(shared.clone());
    let accept_shared = shared.clone();
    std::thread::Builder::new()
        .name("litebox-control-server".to_owned())
        .spawn(move || loop {
            match pipe::create_and_accept_one_instance(&pipe_name) {
                Ok(handle) => {
                    let shared = accept_shared.clone();
                    std::thread::spawn(move || handle_connection(shared, handle));
                }
                Err(e) => {
                    eprintln!("[litebox-control-server] accept failed: {e}, retrying");
                }
            }
        })
        .expect("failed to spawn litebox-control-server thread");
    Ok(shared)
}
