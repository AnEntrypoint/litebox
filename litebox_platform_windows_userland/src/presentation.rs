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

use std::sync::mpsc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{
    ActiveEventLoop, ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy,
};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::window::{Window, WindowId};

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
#[derive(Clone)]
pub struct FrameSender {
    frames: mpsc::Sender<Frame>,
    // Wakes the event loop to actually process a just-sent frame promptly rather than waiting for
    // the next unrelated OS event (mouse move, timer tick, ...) to happen to pump the loop --
    // `EventLoopProxy::send_event` is `winit`'s own documented mechanism for exactly this, safe to
    // call from any thread.
    wake: EventLoopProxy<()>,
}

impl FrameSender {
    /// Queue `frame` for the next redraw. Never blocks (the channel is unbounded -- a slow
    /// presenter falling behind a fast producer degrades to memory growth, not backpressure that
    /// could stall the guest's own page-flip ioctl; acceptable for this pass's single-producer,
    /// low-frequency-flip usage, worth revisiting if a real workload flips faster than the
    /// presenter can drain).
    pub fn send(&self, frame: Frame) {
        if self.frames.send(frame).is_ok() {
            let _ = self.wake.send_event(());
        }
    }
}

/// Owns the real window, the `wgpu` presentation state, and runs `winit`'s event loop until the
/// window is closed. Call [`Presenter::run`] on a dedicated thread (see this module's doc
/// comment for why); it blocks for the window's entire lifetime.
pub struct Presenter {
    event_loop: EventLoop<()>,
    frames_rx: mpsc::Receiver<Frame>,
    sender: FrameSender,
    input_consumer: Option<Box<dyn Fn(InputSignal) + Send>>,
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
        let event_loop = EventLoopBuilder::default().with_any_thread(true).build()?;
        let wake = event_loop.create_proxy();
        let (frames_tx, frames_rx) = mpsc::channel();
        let sender = FrameSender {
            frames: frames_tx,
            wake,
        };
        Ok(Self {
            event_loop,
            frames_rx,
            sender,
            input_consumer: None,
        })
    }

    /// A cloneable handle to push frames into this presenter from any other thread, valid for the
    /// presenter's whole lifetime (including before [`Self::run`] is called -- frames sent early
    /// simply queue until the window exists and starts draining them).
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
            frames_rx: self.frames_rx,
            state: None,
            last_frame: None,
            input_consumer: self.input_consumer,
            last_cursor_pos: None,
        };
        self.event_loop.run_app(&mut app)
    }
}

struct GpuState {
    window: std::sync::Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_size: winit::dpi::PhysicalSize<u32>,
}

struct PresenterApp {
    frames_rx: mpsc::Receiver<Frame>,
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
    /// Upload `frame`'s pixel bytes as a `wgpu` texture and blit them onto the window surface via
    /// a plain full-screen copy (no shader stage needed -- `wgpu`'s `copy_texture_to_texture`-
    /// adjacent path, via `write_texture` + a trivial blit render pass, matches this pass' scope:
    /// present the guest's own pixels unmodified, no scaling/rotation/color-correction).
    fn present(&mut self, frame: &Frame) {
        let Some(state) = &mut self.state else {
            return;
        };
        let texture = state.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("drm-dumb-buffer-frame"),
            size: wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // XRGB8888 (the DRM format DrmSubsystem advertises) is byte-order BGRX in memory on a
            // little-endian host, which `Bgra8Unorm` matches directly with no channel-swizzle
            // needed on upload.
            format: wgpu::TextureFormat::Bgra8Unorm,
            // COPY_DST for `write_texture`'s upload; COPY_SRC because this texture is later the
            // SOURCE of `copy_texture_to_texture` into the surface (see `present`'s encoder
            // below) -- `TEXTURE_BINDING` is not actually needed for this pass (a plain copy, no
            // shader sampling), kept for a later pass that renders via an actual blit shader
            // instead (needed once frame/surface dimensions can differ and a real sampled resize
            // is required, not just a same-size copy).
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        state.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
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
        // A plain texture-to-texture copy (not a shader-based blit) is sufficient and correct
        // when the frame's own pixel dimensions exactly match the surface's current size, which
        // this pass guarantees (the window is created at the DRM virtual display's own fixed
        // resolution, see `Presenter::new`'s caller). A later pass adding real window resizing
        // independent of the guest's own mode would need an actual sampled blit instead.
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &surface_texture.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: frame.width.min(state.surface_size.width),
                height: frame.height.min(state.surface_size.height),
                depth_or_array_layers: 1,
            },
        );
        let _ = &surface_view;
        state.queue.submit(Some(encoder.finish()));
        surface_texture.present();
    }
}

impl ApplicationHandler for PresenterApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let window_attrs = Window::default_attributes()
            .with_title("litebox virtual display")
            .with_inner_size(winit::dpi::PhysicalSize::new(1920u32, 1080u32));
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
        let Ok(surface) = instance.create_surface(window.clone()) else {
            return;
        };
        let Ok(adapter) =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }))
        else {
            return;
        };
        let Ok((device, queue)) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("litebox-presenter"),
                ..Default::default()
            }))
        else {
            return;
        };
        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| *f == wgpu::TextureFormat::Bgra8Unorm)
            .unwrap_or(caps.formats[0]);
        // `Fifo` is the only present mode every wgpu surface is required to support (the wgpu spec
        // guarantees this); no measured need for `Immediate`/`Mailbox`'s lower latency in this
        // module's own use case (a guest's DRM page-flip rate, not a real-time renderer).
        let present_mode = wgpu::PresentMode::Fifo;
        surface.configure(
            &device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
                format: surface_format,
                width: size.width.max(1),
                height: size.height.max(1),
                present_mode,
                desired_maximum_frame_latency: 2,
                alpha_mode: caps.alpha_modes[0],
                view_formats: vec![],
            },
        );
        self.state = Some(GpuState {
            window,
            surface,
            device,
            queue,
            surface_size: size,
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

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        // A `FrameSender::send` wake-up: drain every queued frame, keeping only the last one (the
        // most recent frame is the only one still worth showing -- matching how a real display
        // only ever shows the CURRENT scanout buffer, never a backlog of stale ones). Deliberately
        // does NOT call `present()` directly: `Surface::get_current_texture()` genuinely blocked
        // (confirmed live, AMD/Vulkan/Windows: reproducibly hung inside that one call with no
        // error, no timeout, no further progress) when invoked from an arbitrary event-loop
        // callback rather than from the window's own `RedrawRequested` -- every real wgpu+winit
        // example routes presentation through `RedrawRequested` for exactly this reason (it is the
        // point `winit`'s own platform backend guarantees the swapchain is in a presentable
        // state), never from a `user_event`/custom-event handler. This just stores the frame and
        // asks the window to redraw; `window_event`'s `RedrawRequested` arm does the actual
        // `present()` call.
        let mut latest = None;
        while let Ok(frame) = self.frames_rx.try_recv() {
            latest = Some(frame);
        }
        if let Some(frame) = latest {
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
                if let Some((last_x, last_y)) = self.last_cursor_pos {
                    // A real mouse's per-event motion never approaches a delta anywhere near
                    // `i32`'s range, so this narrowing is exact in practice, not a real precision
                    // loss to guard against.
                    #[allow(clippy::cast_possible_truncation)]
                    let dx = (position.x - last_x) as i32;
                    #[allow(clippy::cast_possible_truncation)]
                    let dy = (position.y - last_y) as i32;
                    if dx != 0 {
                        consumer(InputSignal::Rel(litebox_common_linux::REL_X, dx));
                    }
                    if dy != 0 {
                        consumer(InputSignal::Rel(litebox_common_linux::REL_Y, dy));
                    }
                }
                self.last_cursor_pos = Some((position.x, position.y));
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
