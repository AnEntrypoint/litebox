# Presenter process and control channel: design spec

Status: design only, no code in this pass. Implements Appendix D of
`advisor/ADVISORY-001-fundamentals.md` (the authoritative source for the
scanout/control-channel idea; this doc makes it concrete enough to build
directly from, and resolves the open questions Appendix D left implicit).

## 0. Why

Today (`litebox_platform_windows_userland/src/presentation.rs`,
`litebox_runner_linux_on_windows_userland/src/lib.rs` `--gui` block, lines
~348-450) the window, wgpu device/surface, and winit event loop all run on a
dedicated thread **inside the same process** as guest syscall emulation.
Frames reach it via an in-process `mpsc::Sender<Frame>` (`FrameSender`,
cloned into `DrmSubsystem`'s flip callback); input reaches the guest via a
closure calling `LinuxShim::push_input_key`/`push_input_rel` directly.
`LITEBOX_DUMP_FRAMES` is a compile-time-wired diagnostic inside this same
callback, decided once at startup from an env var.

Consequences the user's standing requirement and the advisory both name:

- The window cannot be shown/hidden after startup without restarting the
  guest -- there is no "later" control surface, only the one-shot `--gui`
  CLI flag.
- A headless run still pays no GPU/window cost today (confirmed still true
  below), but a `--gui` run's wgpu/winit setup racing DRM startup is a
  same-process crash risk for the GUEST process itself (pass-308/309, cited
  in the advisory) -- a presenter crash currently can take the guest down
  with it, or at least corrupts diagnosis of which subsystem actually failed.
- winit/wgpu/COM state living in the guest process blocks the `RtlCloneUserProcess`-based fork redesign (ADVISORY-001 section 1.2 item 2: the
  clone child must not touch COM/GDI/user32-backed state).

Target: move the window/blit ownership into a separate `litebox-presenter.exe`
process, controlled over a named-pipe protocol, with the framebuffer shared
via a zero-copy Windows section rather than copied through the pipe.

## 1. Process responsibility split

### 1.1 Runner process (`litebox_runner_linux_on_windows_userland.exe`) -- unchanged scope

Owns exactly what it owns today minus presentation:

- Guest syscall emulation, `litebox_shim_linux`, `DrmSubsystem`.
- The scanout **descriptor** (below) and the section that backs it.
- The `ControlServer`: named-pipe listener answering the protocol in
  section 3.
- `LITEBOX_DUMP_FRAMES`'s actual file-writing (moved here from
  `presentation.rs`, per Appendix D4 -- this is what makes headless dumping
  not depend on any presenter existing, already true today by construction
  since the headless code path calls `dump_frame_diagnostic` directly; this
  is preserved, not changed in kind, only relocated).
- evdev injection entry points (`push_input_key`/`push_input_rel`, already
  exposed, `lib.rs` ~381-391) -- now driven by control-channel `key`/`rel`
  messages instead of a direct winit callback closure.

Owns NO Win32 window, NO wgpu/D3D/Vulkan instance, NO winit event loop, NO
COM. This is the change that unblocks the clone-based fork (ADVISORY-001
1.2): the guest+kernel process is presentation-free.

### 1.2 Presenter process (`litebox-presenter.exe`) -- new, separate binary

Owns:

- Win32 window creation (`CreateWindowEx`/message pump) -- same mechanism
  used today via `winit`, kept as-is (see 1.3 below for why).
- The blit path: wgpu `Instance`/`Adapter`/`Device`/`Surface`, forced to the
  `DX12` backend (kept identical to today's `presentation.rs` -- the Vulkan
  swapchain-hang-on-first-frame finding documented there, an
  NVIDIA/AMD-hybrid-laptop-specific WSI issue, is a host/driver fact
  independent of which process runs the surface, so the same override
  carries over unchanged).
- Mapping the shared scanout section (read-only) and uploading its bytes as
  a texture via `write_texture` + `copy_texture_to_texture`, same technique
  `PresenterApp::present` uses today (`presentation.rs` ~417-501) -- kept
  identical, only the frame source changes (a mapped section instead of an
  `mpsc::Receiver<Frame>`).
- Real keyboard/mouse capture via winit's own `WindowEvent`, translated to
  evdev codes via the SAME `winit_keycode_to_evdev`/`winit_mouse_button_to_evdev`
  tables already in `presentation.rs` (moved, not rewritten) -- forwarded
  over the pipe as `key`/`rel` lines instead of an in-process closure call.
- Its own control-channel CLIENT connection back to the runner (dials the
  pipe the runner listens on).

Owns NOTHING about guest syscall emulation, `DrmSubsystem`, or any
`litebox_shim_linux`/`litebox` core type. It links against
`litebox_platform_windows_userland::presentation` (window/wgpu code, kept in
that crate) and a small new protocol crate (section 3), not against the
shim or kernel crates at all -- this is what keeps a presenter crash from
being able to corrupt guest state even in principle (no shared address
space, no shared Rust ownership).

### 1.3 Why keep winit/wgpu/DX12, not switch mechanism

The task explicitly asks to keep the current underlying mechanism absent a
strong reason to change. There is none here: the design problem is entirely
about which PROCESS owns the window, not what draws into it. Switching to
raw GDI/Direct3D would be a second, unrelated rewrite with its own risk
(losing wgpu's existing surface-capability negotiation, alpha-mode handling
already hard-won per the `Opaque`-alpha-mode comment in `presentation.rs`
~571-591). Keep winit+wgpu+DX12 verbatim, just running in a different
process.

## 2. Zero-copy scanout: exact mechanism

### 2.1 What "zero-copy" means here, precisely

The DRM dumb buffer backing the guest's current front buffer is ALREADY a
platform shared-memory object (a Windows section, per how litebox's dumb
buffers are implemented, ADVISORY-001 D1). "Zero-copy scanout" means: the
presenter maps THAT SAME section (or a small double-buffer pair of them) and
reads pixels directly out of guest-written memory on every present -- no
per-frame IPC message ever carries pixel bytes.

### 2.2 Scanout descriptor (runner-side, updated by `DrmSubsystem`)

```rust
struct ScanoutDescriptor {
    section_handle: RawHandle,   // HANDLE to the section backing the current front buffer
    width: u32,
    height: u32,
    pitch: u32,
    format: u32,                 // DRM fourcc, e.g. DRM_FORMAT_XRGB8888
    buffer_offset: u64,          // byte offset into the section where this buffer's data starts
    frame_seq: AtomicU64,        // bumped on every SETCRTC/PAGE_FLIP/DIRTYFB
}
```

Updated from the existing flip/setcrtc code paths in `DrmSubsystem` (today
these call `set_drm_flip_callback`'s registered closure with raw bytes;
after this change they instead update `ScanoutDescriptor` fields plus bump
`frame_seq`, and no longer copy `bytes.to_vec()` -- eliminating the one copy
`lib.rs` line 423 does today). Both `DIRTYFB` (no handle change) and a real
`PAGE_FLIP`/`SETCRTC` (handle or offset may change) are covered by the same
struct; a "double-buffered" upgrade (Appendix D1's own noted future step) is
just adding a second `section_handle`/`buffer_offset` pair and having
`frame_seq`'s low bit select which -- deferred, first version accepts
tearing exactly as Appendix D1 says.

### 2.3 Handle transfer across the process boundary: `DuplicateHandle`, once at `scanout` time

This is the concrete mechanism Appendix D gestures at ("duplicate the
section handle into the caller's process") -- spelled out fully:

1. The presenter connects to the named pipe (`CreateFile` on
   `\\.\pipe\litebox-<runner pid>`) and sends `scanout`.
2. Windows named pipes expose the connecting client's process id via
   `GetNamedPipeClientProcessId` (server side) once a connection is
   established -- the runner calls this on accept to learn the presenter's
   PID with no separate handshake message needed.
3. The runner opens the presenter process with
   `OpenProcess(PROCESS_DUP_HANDLE, FALSE, presenter_pid)`.
4. The runner calls
   `DuplicateHandle(GetCurrentProcess(), section_handle, presenter_process_handle, &mut dup_handle, 0, FALSE, DUPLICATE_SAME_ACCESS)`
   to produce a HANDLE value valid **in the presenter's own handle table**
   (this is the standard, well-documented way to hand a kernel object to an
   unrelated process with no admin rights required -- unlike
   `bInheritHandle` inheritance, which only works for a handle open at
   *spawn* time in a parent/child relationship; here the presenter may be
   spawned first and connect later, or be a pre-existing long-lived process
   that reconnects, so inheritance-at-spawn is not assumed).
5. The runner replies over the pipe with the **numeric value** of
   `dup_handle` (a `usize`/`u64`) plus width/height/pitch/format/offset/seq.
   This numeric value is meaningless in the runner's own process and MUST
   NOT be used there again -- it is only valid inside the presenter's
   handle table, which is exactly why duplication must happen after the
   server learns the client's PID, not before.
6. The presenter calls `MapViewOfFile`-equivalent (`OpenFileMappingW` is NOT
   needed since it already has a raw section HANDLE from step 5 --
   `MapViewOfFile(dup_handle, FILE_MAP_READ, ...)` directly) to map the
   section read-only into its own address space.
7. On every subsequent frame, the presenter does NOT re-map or
   re-duplicate anything unless `scanout`'s reply on a later poll reports a
   **different** `section_handle` value (a real buffer reallocation, e.g. a
   mode change) -- it just re-reads bytes at `buffer_offset` for the
   `pitch*height` region, keyed by watching `frame_seq` change (section
   2.4). This is the "one Handle duplicated once at setup" the task asks
   for; the pipe carries only the numeric handle and geometry once per
   buffer-identity change, never pixel bytes.
8. Presenter maps the section `FILE_MAP_READ`-only (never write) -- this is
   the whole reason no reader/writer lock is needed on the presenter side;
   see section 2.5 for why partial-read tearing is acceptable and how a
   future double-buffer removes even that.

### 2.4 Frame-change signaling without a copy or a guest-blocking lock

Two options, pick (a) for the first version per Appendix D3's own text
("poll a `frame_seq` mirror ... or wait on a named event"):

**(a) Poll `frame_seq` at the window's own vsync/redraw cadence.** The
`frame_seq` field lives in a tiny separate shared HEADER section (not the
pixel section itself -- kept distinct so header polling never contends
with the (potentially much larger) pixel data's own page mappings), mapped
`FILE_MAP_READ` by the presenter once at `scanout` time exactly like the
pixel section. The presenter's own `RedrawRequested`/frame-pacing loop
(already driven by wgpu's `Fifo` present mode today) reads the header's
`frame_seq` on each tick; if it changed since the last presented frame, it
re-reads the pixel section and re-uploads via `write_texture`. This needs
NO synchronization primitive shared across processes at all: `frame_seq` is
a single `AtomicU64` the runner writes with `Ordering::Release` after
finishing the section write, the presenter reads with `Ordering::Acquire`.
A torn read of the FULL PIXEL BUFFER (runner mid-write while presenter
mid-read) can happen -- this is the same tearing Appendix D1 already
accepts by design ("tearing is acceptable for a first version"), not a new
risk this design introduces.

**(b) Named kernel event, `SetEvent` on flip.** Lower latency, avoids
polling entirely, but reintroduces a cross-process synchronization object
the DESIGN QUESTION explicitly worries about ("a lock that could deadlock
the guest's own render loop"). Concretely: the runner's flip path would
call `SetEvent` on a named event object (`\\.\...` namespace,
`CreateEventW` with a name, opened by the presenter via `OpenEventW`) --
this is safe (SetEvent never blocks the setter) as long as the runner NEVER
calls `WaitForSingleObject` waiting for the presenter to consume a frame;
it must be fire-and-forget, exactly matching how a real DRM vblank
interrupt is fire-and-forget from the kernel's perspective. Flagged as a
possible follow-up once polling's latency is measured and found
insufficient -- not needed for a first version, and deliberately deferred
to avoid introducing a cross-process wait primitive before there's a
measured need for one.

Recommendation: ship (a) first. It has no deadlock surface by construction
(the guest/runner never waits on anything the presenter controls), matches
Appendix D3's own primary suggestion, and vkms's own real-hardware model
(section 1.3 of the advisory: "scanout happens every vblank regardless of
flips") is itself poll-shaped, not interrupt-shaped.

### 2.5 Why this can never deadlock the guest's render loop

The full data-flow the guest's page-flip ioctl handler participates in,
under this design:

1. Guest thread calls `PAGE_FLIP`/`SETCRTC`/`DIRTYFB`.
2. `DrmSubsystem`'s handler updates `ScanoutDescriptor` fields (plain
   memory writes to host-owned struct fields, already computed) and does
   `frame_seq.fetch_add(1, Release)`.
3. Handler returns to the guest immediately. No wait, no lock acquire that
   a presenter or control-channel thread could be holding.

The presenter is never in this call path at all -- it is a passive reader
polling on its own schedule (2.4a) or receiving a fire-and-forget signal
(2.4b, if ever adopted). There is no primitive here a slow, hung, or
crashed presenter could be holding that the guest's flip path waits on.
This is a structural property of the design (the guest writes and moves on;
the presenter reads whenever it reads), not something that needs runtime
enforcement.

## 3. Control-channel protocol

### 3.1 Transport and framing

Named pipe, `\\.\pipe\litebox-<runner-pid>` on Windows (matches Appendix
D2's own name; the Unix-socket equivalent for the Linux/macOS presenter
port is future work, out of scope here, and is not assumed symmetric with
Windows beyond "some local IPC transport" since neither platform's
presenter exists yet per the macOS/Linux memory note).

Framing: **newline-delimited, one command per line, one reply per line**,
UTF-8 text, for every command except the (rare, large) binary payload case
-- chosen over a length-prefixed binary struct because:

- Every payload that's actually large (pixel bytes) is handled by the
  zero-copy section, NEVER by the pipe itself -- so the pipe's own message
  sizes are always small (a command name, a few integers, a path string),
  where length-prefix framing's main benefit (avoiding a byte-by-byte
  delimiter scan on a large blob) doesn't apply.
- Text framing is trivially debuggable with a plain named-pipe client
  during development/verification (matches this project's own standing
  emphasis on scriptable, no-guesswork tooling) -- a raw text line can be
  typed by hand or from a one-line PowerShell test script; a binary struct
  cannot.
- `screenshot`'s reply needs a mix of a text status line and (optionally,
  see 3.3) a raw byte blob -- handled by a length-prefixed binary segment
  ONLY for that one reply, not as the general framing rule (see 3.3).

Each line: space-separated tokens, first token is the command name. A reply
line's first token is `ok` or `err`; an `err` reply's second token is a
short machine-stable error code (`bad_state`, `not_found`, `unsupported`,
`io_error`), remaining tokens are a human-readable detail.

One connection per presenter process, held open for its whole lifetime
(not reconnected per command) -- matches a real device's control plane
being a persistent session, and avoids the PID-relearning/re-duplication
complexity of tearing the connection down between commands.

### 3.2 Commands

**`scanout`** -- request/refresh the shared framebuffer mapping.
Request: `scanout`
Reply: `ok <pixel_section_handle> <header_section_handle> <w> <h> <pitch> <format> <offset> <seq>`
(both handles are numeric values valid only in the CALLER's process, per
section 2.3). Re-issuing `scanout` later (e.g. after a mode change) gets a
fresh reply; the presenter compares `pixel_section_handle` against what it
already has and only re-maps if it changed.

**`screenshot <path>`** -- write the CURRENT scanout content to a file on
the runner's own filesystem (the runner does the encoding, not the
presenter -- this is what makes `screenshot` work identically whether or
not a presenter process currently exists, see 3.4).
Request: `screenshot <absolute-path.png-or-bmp>`
Reply: `ok <path> <non_black_pixels> <distinct_colors_capped64>` or
`err io_error <detail>`. Reuses the exact counting logic already in
`dump_frame_diagnostic` (`presentation.rs` ~58-95), relocated to the runner
per 1.1.

**`show`** / **`hide`** -- toggle window visibility live.
Request: `show` or `hide`.
Reply: `ok` (idempotent -- `show` on an already-visible window, or `hide`
on an already-hidden one, both just reply `ok` with no error). If no
presenter process is currently connected, `show` SPAWNS one (see section 4)
and blocks its reply until that presenter has connected, called `scanout`,
and created its window -- so a caller doing `show` then immediately
`screenshot` never races presenter startup. `hide` on a connection that
never had a presenter is `err not_found`.
A `presenter?` query command additionally reports current state:
Reply: `ok none` / `ok hidden` / `ok visible`.

**`key <evdev_code> <0|1|2>`** -- inject a synthetic key event (0=release,
1=press, 2=repeat, matching evdev's real three-state value already used in
`presentation.rs`'s own `KeyboardInput` handler).
Request: `key <u16> <0|1|2>`. Reply: `ok`.
Also `rel <evdev_code> <i32>` (motion/wheel, REL_X/REL_Y/REL_WHEEL) and
`abs <evdev_code> <i32>` (reserved for future absolute-positioning devices
-- not implemented by any current shim entry point, so `abs` replies
`err unsupported` until that lands; declared now so the protocol doesn't
need a breaking change later).
These work identically whether sent by an interactively-connected real
presenter (winit forwarding real Win32 input) or by a script driving the
pipe directly with no presenter/window at all -- both paths funnel through
the SAME runner-side `key`/`rel` handler that calls
`push_input_key`/`push_input_rel`, so scripted input injection for
automated testing needs no window to exist.

**`ps`** -- list guest processes.
Request: `ps`.
Reply: `ok <n>` followed by `n` lines `<pid> <ppid> <comm>`, proxying
`litebox_shim_linux::diag::print_process_tree`'s existing data (`diag.rs`
~218-231+) rather than reimplementing process enumeration -- that function
takes an `eprint` closure today; give it an alternate sink that collects
lines into the pipe reply instead of writing to stderr.

**`strace <on|off|query> [pid]`** -- toggle/query
`LITEBOX_STRACE_SUMMARY`-style diagnostics at runtime rather than only at
startup via env var.
Request: `strace on` / `strace off` / `strace query`.
Reply for `on`/`off`: `ok`. Reply for `query`: `ok <on|off>`. This calls
`litebox_shim_linux::diag::init_strace_summary(bool)` (already exists,
`diag.rs` line 61, described as "idempotent and cheap after the first
call" -- exactly the property a runtime toggle needs) instead of only
being driven by the env var read once at process start. The per-pid
filter argument in the request shape is reserved (current
`init_strace_summary` is process-global, not per-pid) -- accepted but
ignored with a logged warning until per-pid scoping is implemented; this
is flagged as a real gap in section 5, not silently pretended-supported.
A `strace dump` variant triggers `print_strace_summary` immediately (rather
than only at process exit as today) and streams its text back as
`ok <n-lines>` followed by `n` text lines.

**`frames <on|off> <dir>`** -- runtime equivalent of `LITEBOX_DUMP_FRAMES`.
Request: `frames on <dir>` / `frames off`.
Reply: `ok`. Sets a runtime flag the relocated (per 1.1) frame-dump code
checks on every flip, replacing today's env-var-read-once-at-startup gate
with a mutable runtime flag (an `AtomicBool` plus a `Mutex<Option<PathBuf>>`
for the directory) so a long-running session can be told to start/stop
dumping without a restart. `LITEBOX_DUMP_FRAMES` as a startup env var
CONTINUES to work unchanged (see 3.4/5) -- it just becomes sugar for
calling this same runtime flag's initial value at startup, not a separate
code path.

### 3.3 The one binary-payload exception

`screenshot`'s file-write variant above needs no binary reply at all (it
writes to a path and reports counts). For a future "return bytes directly"
variant (not required by this pass, flagged for completeness): the request
`screenshot -` (path `-` meaning "stdout the bytes over the pipe") gets a
reply `ok - <byte-length>\n` followed immediately by exactly `<byte-length>`
raw bytes with no delimiter, then normal newline-delimited framing resumes.
This is the sole place the wire format leaves line-oriented framing, and
only because encoding+returning a PNG inline is occasionally more
convenient for a remote caller than a shared filesystem path. Not needed
for local same-host verification (the primary use case per
`feedback_use_dump_frames_not_screenshots.md`), so deferred; the
line-oriented `screenshot <path>` form covers this session's actual
workflow.

## 4. Runner-side spawn and flag wiring

### 4.1 Flag semantics

- No `--gui` flag at all: **headless, exactly as today.** No presenter
  process is ever spawned. `ControlServer` still starts (a caller can still
  connect and issue `screenshot`/`ps`/`strace`/`frames`/`key` — all of
  which work with no window, per their own descriptions above), but `show`
  is the only command that would ever cause a presenter to spawn, and
  nothing calls it automatically. This is the same "headless never pays
  GPU/window setup cost unless asked" property `--gui`'s absence already
  guarantees today (confirmed above: the current headless path registers
  `dump_frame_diagnostic` directly with no `Presenter` construction at
  all) -- preserved exactly, not weakened.
- `--gui`: spawns `litebox-presenter.exe` at startup AND immediately issues
  it an implicit `show` (matches today's behavior: passing `--gui` shows a
  window right away). Equivalent to headless startup followed immediately
  by a `show` control-channel call to a freshly spawned presenter.
- `--gui=hidden` (new): spawns `litebox-presenter.exe` at startup but does
  NOT show it -- the process and its window exist (so a later `show` is
  fast, no cold-start wgpu/window-creation cost) but nothing is visible.
  Matches Appendix D2's own text exactly ("window exists, not visible").

### 4.2 Spawn mechanism

The runner spawns `litebox-presenter.exe` via `std::process::Command`
(plain child process, not `RtlCloneUserProcess` -- that clone mechanism is
specific to the FORK redesign for GUEST processes in ADVISORY-001 1.2 and
is unrelated here; the presenter is an ordinary Windows process spawned the
ordinary way), passing the pipe name (`\\.\pipe\litebox-<runner-pid>`) as a
command-line argument. The presenter connects as pipe CLIENT; the runner is
always the pipe SERVER (`CreateNamedPipe`, one server, potentially
multiple future clients such as a diagnostic script alongside a real
presenter -- though the current design only ever spawns one presenter, a
second/third named-pipe CLIENT for `screenshot`/`ps`/`key` from an
external verification script works with no extra design since those
commands don't require the caller to BE the presenter).

The runner does not wait synchronously for the presenter to fully start up
before returning control to its own main flow UNLESS the caller issued
`show` (see 3.2's `show` semantics: `show`'s own reply blocks until ready).
`--gui`'s implicit `show` therefore has the same blocking-until-ready
property interactive callers get.

### 4.3 Runner-side code replaced

Everything in `lib.rs` ~363-437 (the `gui_presenter_thread` closure: thread
spawn, `Presenter::new()`, `set_input_consumer`, `sender_tx`/`sender_rx`
handoff, `set_drm_flip_callback` registration) is DELETED and replaced by:
(a) `ControlServer` startup (always, headless or not), (b) if `--gui`/
`--gui=hidden`, a `Command::new("litebox-presenter.exe").spawn()` call plus
(for `--gui` only) one `show` request over the newly-accepted connection.
`DrmSubsystem`'s flip callback registration changes from "push a `Frame`
copy into an mpsc channel" to "update `ScanoutDescriptor` fields + bump
`frame_seq`" (section 2.2) -- registered unconditionally now (headless or
not), since the descriptor update is cheap and something must always
maintain it for `screenshot`/`scanout` to have current data regardless of
whether a presenter is attached.

### 4.4 Headless stays exactly headless -- explicit confirmation

Requirement 3 in the task asks this to be explicitly confirmed, not
assumed: with no `--gui`/`--gui=hidden` flag, `litebox-presenter.exe` is
NEVER spawned (section 4.1). The `ScanoutDescriptor` update on every flip
is a few atomic/plain-field writes into runner-owned memory (already
computed data, no format conversion, no section-copy) -- effectively free,
and strictly cheaper than today's headless path, which already does a
`bytes.to_vec()` copy into a `Frame` on every flip when `LITEBOX_DUMP_FRAMES`
is set (`lib.rs` line 423) and nothing at all otherwise. No wgpu
`Instance`, no window, no COM is ever touched by the runner process under
any flag combination -- that capability now lives exclusively in
`litebox-presenter.exe`, a process that (per 4.1) is never spawned unless
`--gui`/`--gui=hidden`/an explicit `show` asks for it.

## 5. Open questions and risks (not hand-waved)

1. **Handle duplication needs `PROCESS_DUP_HANDLE` on the target, not
   admin rights.** Verified conceptually against documented Win32 behavior
   (`OpenProcess(PROCESS_DUP_HANDLE, ...)` on a same-user, non-elevated
   sibling process succeeds under normal desktop-user token rules -- no
   admin/SeDebugPrivilege needed when both processes run as the same user,
   which is the litebox dev/CI use case). NOT yet verified live on this
   host the way ADVISORY-001's own clone/APC probes were (`clone_probe.c`,
   `apc_probe.c`) -- a small `dup_handle_probe.c`-style probe (spawn a
   child, open a section in the parent, `DuplicateHandle` into the child,
   have the child map and read it) should be run before implementation
   begins, not assumed. If the runner ever runs elevated while the
   presenter does not (or vice versa), duplication can fail even
   same-user -- flag this as a real deployment constraint (both processes
   must run at the same integrity level) rather than something this design
   silently papers over.
2. **Section growth/mode-change races.** If the guest changes DRM mode
   (different width/height/pitch) between the presenter's last `scanout`
   reply and its next texture upload, the presenter could read a
   stale-sized region. Mitigation: `frame_seq`'s change is the presenter's
   only trigger to re-read geometry too (re-fetch width/height/pitch from
   the header section on every `frame_seq` change, not just once at
   `scanout` time) -- the header section (2.4) must carry geometry
   alongside `frame_seq`, not just the sequence number, specifically so a
   mode change is visible without a fresh `scanout` round-trip. Written
   into section 2.4's header-section description above; call out here
   because it's easy to implement "geometry only from the one-time
   `scanout` reply" and be wrong under a live mode change.
3. **A presenter that never calls `scanout` after `show`'s spawn.** `show`
   blocking until "ready" (4.2) needs a concrete readiness signal -- defined
   here as: the presenter's first successful `scanout` call plus first
   successful window creation, both acked back to the runner over the SAME
   pipe connection via an internal (not user-facing) `ready` line the
   presenter sends unprompted once both are done. If the presenter process
   fails to start (missing exe, crashed before connecting), `show` must
   time out (proposed: 5s, matching this project's existing preference for
   explicit, disclosed timeouts over indefinite blocking) and reply
   `err io_error presenter did not start`.
4. **Show/hide interacting with a currently-blocked present call.**
   Because presentation is poll-driven from the presenter's OWN
   `RedrawRequested` loop (2.4a) rather than pushed into it, `hide` is just
   `ShowWindow(hwnd, SW_HIDE)` -- it does not need to interrupt an in-flight
   `present()` call at all; wgpu's `surface.get_current_texture()`/
   `present()` calls are unaffected by the window's visibility state on
   Windows (a hidden window's swapchain still presents, it's simply not
   composited to the screen -- standard DWM behavior). No special
   cancellation/interruption logic is needed. Flagging this explicitly
   because Appendix D3 lists "handles hide/show from the pipe" as if it
   needed real synchronization; concretely it does not, given the
   poll-driven design -- this is a genuine simplification worth stating
   plainly rather than building unneeded interlocking.
5. **Backward compatibility with `LITEBOX_DUMP_FRAMES`-based headless
   verification.** This session's own standing workflow
   (`feedback_use_dump_frames_not_screenshots.md`) depends on
   `LITEBOX_DUMP_FRAMES=1` writing numbered `.bmp` files with a
   `non_black_pixels` stderr line, with NO control-channel client involved
   at all (a script just sets the env var and reads stderr/files after the
   run). Section 3.2's `frames on/off` command is ADDITIVE, not a
   replacement: the env var must continue to seed the SAME runtime flag at
   startup (4.1/`frames`'s own description already states this) so a
   caller doing nothing but setting `LITEBOX_DUMP_FRAMES=1` before running
   the runner sees byte-identical behavior to today, including with no
   `--gui` flag and no presenter ever spawned. This must be verified live
   (not just argued) once implemented, by running today's exact
   `LITEBOX_DUMP_FRAMES=1` headless recipe against the new build and
   diffing the resulting `.bmp` files' `non_black_pixels` counts against a
   pre-change baseline run.
6. **Presenter crash isolation, unverified claim.** Section 1.2 asserts a
   presenter crash cannot corrupt guest state "since there's no shared
   address space" -- true for MEMORY corruption, but a presenter crash
   WHILE the pipe connection is torn down mid-command (e.g. runner is
   inside a `show` call waiting for the `ready` line, per risk 3) needs the
   runner's pipe-read to fail cleanly (`ERROR_BROKEN_PIPE`) and turn into
   an `err` reply to whatever ORIGINAL caller asked for `show`, not a
   runner-side hang. This is standard named-pipe error handling, not a new
   primitive, but must be implemented (a caller doing `show` must never
   block forever because the presenter died mid-handshake) rather than
   assumed free.
7. **Multiple simultaneous control-channel clients.** The protocol as
   specified supports one presenter plus, separately, script-driven callers
   issuing `screenshot`/`ps`/`key` etc. concurrently (section 4.2 already
   notes this needs no extra design for READ-only/inject-only commands).
   `show`/`hide` from two different callers racing is unspecified here --
   proposed resolution (not yet built): last-writer-wins, `show`/`hide` are
   idempotent by design (3.2) so a race just means the window ends in
   whichever state the LAST call requested, which is an acceptable,
   simple semantic; flagged so a future implementer doesn't need to
   invent a locking scheme for this.

## 6. Verification plan (mirrors Appendix D5, made concrete against this doc's exact commands)

1. Headless: no `--gui`, `LITEBOX_DUMP_FRAMES=1` set, run a guest DRM
   client; confirm `.bmp` files + `non_black_pixels` stderr lines appear
   exactly as today, with `litebox-presenter.exe` never spawned (checked
   via `tasklist`/process enumeration during the run).
2. Headless + control channel: same run, no `LITEBOX_DUMP_FRAMES`, a
   separate script connects to the pipe and issues `screenshot <path>`
   every 5s; confirm output files change as the guest renders, with no
   presenter process ever existing.
3. `--gui=hidden`: presenter process exists (`tasklist` shows it), no
   window visible (`EnumWindows`/`IsWindowVisible` false), `screenshot`
   still works (reads the section directly on the runner side, unaffected
   by presenter visibility).
4. `show` then `hide`: window appears, human/`IsWindowVisible` confirms;
   `hide` removes it from view while `screenshot`'s counts keep changing
   (guest still rendering, confirming risk 4's claim that hide doesn't
   interrupt presentation).
5. Kill `litebox-presenter.exe` via Task Manager mid-run: guest process
   unaffected (still responds to `ps`/`screenshot`), a subsequent `show`
   spawns a fresh presenter that reconnects and resumes displaying current
   content.
6. `key`/`rel` injection with no presenter running at all (pure headless):
   confirm the guest (e.g. an `xterm` under Xvfb) receives and processes
   the injected input, proving input injection doesn't require a window.
