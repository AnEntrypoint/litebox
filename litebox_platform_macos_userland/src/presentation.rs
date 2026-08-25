// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Host-side GUI presentation: a real macOS (Cocoa/AppKit) window, backed by `wgpu`, that
//! displays pixel buffers a guest DRM client has drawn into (see `litebox_shim_linux::syscalls::
//! drm`'s `DrmSubsystem`). Ported from `litebox_platform_windows_userland::presentation` -- see
//! that module's doc comment for the shared design this mirrors; the threading architecture below
//! is the one genuinely platform-specific piece that does NOT carry over unchanged.
//!
//! # Why this crate's threading arrangement is the INVERSE of Windows/Linux userland
//!
//! On Windows and Linux userland, the runner's own main thread calls `run_thread` (guest
//! execution) directly and blocking, so the presenter's `winit` event loop gets its own dedicated
//! background thread instead -- safe on those platforms because neither a Win32 message loop nor
//! an X11/Wayland client connection is tied to the process's first/main thread (confirmed by
//! reading `winit`'s own `EventLoopBuilderExtWindows`/`EventLoopBuilderExtX11`/
//! `EventLoopBuilderExtWayland` source, each exposing a `with_any_thread` escape hatch whose own
//! doc comments describe the main-thread-only default as a conservative cross-platform
//! compatibility guard, not a real constraint on those backends).
//!
//! Cocoa/AppKit is different in kind, not degree: `winit`'s own macOS backend has NO
//! `with_any_thread` equivalent at all (confirmed by reading `winit` 0.30.13's
//! `src/platform/macos.rs`: unlike the Windows/X11/Wayland platform modules, it defines no such
//! trait, and its `EventLoop` construction path asserts a real `objc2_foundation::
//! MainThreadMarker` -- Apple's own compile-time/runtime witness that the calling code is
//! genuinely on the process's first thread, which cannot be fabricated off that thread). This is
//! documented Apple platform behavior, not a `winit` limitation to work around: AppKit's own
//! `NSApplication`/`NSWindow`/run-loop machinery is specified to require the main thread, and
//! violating it produces real, silent corruption or crashes on real hardware, not a catchable
//! error `winit` could report instead.
//!
//! **The consequence for this module's own API is the inverse of `litebox_platform_windows_
//! userland::presentation::Presenter`: [`Presenter::run`] must be called from the actual process
//! main thread (the same thread `fn main()` starts on), and whatever this platform's own
//! `run_thread` (guest execution) is must move to a background thread instead** -- the opposite of
//! how `litebox_runner_linux_on_windows_userland`/`litebox_runner_linux_userland` are structured
//! today. A future macOS runner (see this module's own "What this pass does NOT do" section below)
//! would need a shape like:
//!
//! ```ignore
//! // Illustrative only -- no macOS runner crate exists yet to actually call this.
//! let presenter = Presenter::new();
//! let sender = presenter.sender();
//! std::thread::spawn(move || {
//!     // Guest execution moves here, off the main thread -- the inverse of the Windows/Linux
//!     // userland runners, where run_thread stays on main and the presenter gets the background
//!     // thread.
//!     unsafe { litebox_platform_macos_userland::guest::run_thread(shim, ctx) };
//! });
//! presenter.run().expect("run presenter event loop"); // blocks the real main thread
//! ```
//!
//! # What this pass does NOT do (honest scope limit, distinct from the Linux userland port)
//!
//! This module is written and type-checked (`cargo check --target aarch64-apple-darwin`,
//! genuinely exercises the full type checker, not just a syntax parse -- confirmed working in this
//! environment even without a linked-binary macOS toolchain) but:
//!
//! 1. **There is no macOS runner crate to wire it into.** Unlike `litebox_runner_linux_on_windows_
//!    userland` and `litebox_runner_linux_userland`, no `litebox_runner_linux_on_macos_userland`
//!    (or similarly named) crate exists in this workspace that constructs a `LinuxShimBuilder`,
//!    loads a guest program, and calls a `run_thread`. This module cannot be wired to a real `--gui`
//!    CLI flag the way the Linux userland port was, because there is nothing to add that flag to.
//! 2. **Guest entry itself is not implemented on this platform.** `litebox_platform_macos_
//!    userland::guest::run_thread` (see that module's own doc comment and `docs/macos.md`'s
//!    "Remaining work" section) is a documented stub that logs an error and returns without
//!    executing any guest code -- the aarch64 context-switch/trampoline/TPIDR_EL0-anchor work is
//!    real, separate, unstarted work, unrelated to GUI presentation. So even with a runner crate,
//!    there is currently no guest execution on this platform for a page-flip to ever originate
//!    from.
//! 3. **No real macOS host is available in this environment to run-verify against**, and no
//!    `codesign`/JIT-entitlement tooling either (`docs/macos.md`'s W^X section) -- both would be
//!    required even once (1) and (2) are done.
//!
//! This module therefore ports exactly what is genuinely portable today (the `wgpu`/pixel-upload/
//! input-translation logic, which is platform-agnostic, and the `Presenter` API shape, adapted to
//! Cocoa's real main-thread constraint) and stops there, rather than fabricating a runner wiring
//! that cannot actually run. See the `gui-macos-linux-presentation-port` PRD row's own follow-up.

use std::sync::mpsc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
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
/// Linux evdev's own `(type, code, value)` shape -- see `litebox_platform_windows_userland::
/// presentation::InputSignal`'s doc comment (identical contract, this is the same shim boundary).
pub enum InputSignal {
    /// `(code, value)` for an `EV_KEY` event -- a keyboard key or mouse button, `value` 1
    /// (pressed) or 0 (released).
    Key(u16, i32),
    /// `(code, value)` for an `EV_REL` event -- relative motion, `value` the signed delta.
    Rel(u16, i32),
}

/// Translate a `winit` physical key into its Linux evdev `KEY_*` code. Identical mapping table to
/// `litebox_platform_windows_userland::presentation::winit_keycode_to_evdev` -- `winit`'s
/// `KeyCode` is itself platform-independent, so the same table is correct on every host.
fn winit_keycode_to_evdev(key: KeyCode) -> Option<u16> {
    // Enumerating all ~80 `KEY_*` constants by name would hurt readability far more than it helps
    // -- matches this crate's own `#[allow]`-on-deliberate-exception convention elsewhere.
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

/// Translate a `winit` mouse button into its Linux evdev `BTN_*` code.
fn winit_mouse_button_to_evdev(button: MouseButton) -> Option<u16> {
    match button {
        MouseButton::Left => Some(litebox_common_linux::BTN_LEFT),
        MouseButton::Right => Some(litebox_common_linux::BTN_RIGHT),
        MouseButton::Middle => Some(litebox_common_linux::BTN_MIDDLE),
        _ => None,
    }
}

/// The sending half of the frame channel: clone and hand out to whatever produces frames
/// (`DrmSubsystem::page_flip`, once a real macOS runner exists to wire it). Sending after the
/// presenter's window has closed is a silent no-op.
#[derive(Clone)]
pub struct FrameSender {
    frames: mpsc::Sender<Frame>,
    wake: EventLoopProxy<()>,
}

impl FrameSender {
    /// Queue `frame` for the next redraw. Never blocks (unbounded channel -- see
    /// `litebox_platform_windows_userland::presentation::FrameSender::send`'s identical rationale).
    pub fn send(&self, frame: Frame) {
        if self.frames.send(frame).is_ok() {
            let _ = self.wake.send_event(());
        }
    }
}

/// Owns the real window, the `wgpu` presentation state, and runs `winit`'s event loop until the
/// window is closed.
///
/// # Main-thread requirement (the one real difference from the Windows/Linux userland API)
///
/// Both [`Presenter::new`] and [`Presenter::run`] must be called from the process's actual main
/// thread -- see this module's own doc comment for why Cocoa allows no exception to this, unlike
/// every other platform this project targets. There is deliberately no `with_any_thread`-style
/// escape hatch here: `winit` itself provides none for this backend (confirmed by reading its
/// source), so this API does not pretend to offer one either.
pub struct Presenter {
    event_loop: EventLoop<()>,
    frames_rx: mpsc::Receiver<Frame>,
    sender: FrameSender,
    input_consumer: Option<Box<dyn Fn(InputSignal) + Send>>,
}

impl Presenter {
    /// Build a not-yet-shown presenter and its window. Real `winit`/OS window/event-loop resources
    /// are not created until [`Self::run`] is called.
    ///
    /// # Panics
    ///
    /// `winit`'s own `EventLoop::new()` panics (via its internal `MainThreadMarker` assertion) if
    /// called off the process's actual main thread -- there is no way for this function to turn
    /// that into a recoverable `Result` the way `Presenter::new`'s Windows/Linux-userland
    /// counterparts can with `with_any_thread(false)`'s ordinary error path, because Cocoa's
    /// violation is a real precondition failure, not a configurable policy.
    pub fn new() -> Result<Self, winit::error::EventLoopError> {
        let event_loop = EventLoop::new()?;
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

    /// A cloneable handle to push frames into this presenter from any other thread (including the
    /// background thread guest execution has to move to on this platform -- see this module's own
    /// doc comment), valid for the presenter's whole lifetime.
    pub fn sender(&self) -> FrameSender {
        self.sender.clone()
    }

    /// Register `consumer` to be called, on the presenter's own main-thread event loop, with every
    /// real keyboard/mouse event this window observes from [`Self::run`]'s call onward. See
    /// `litebox_platform_windows_userland::presentation::Presenter::set_input_consumer`'s doc
    /// comment for the full rationale (identical here); call before [`Self::run`].
    pub fn set_input_consumer(&mut self, consumer: impl Fn(InputSignal) + Send + 'static) {
        self.input_consumer = Some(Box::new(consumer));
    }

    /// Run the event loop on the calling thread until the window is closed.
    ///
    /// # Panics
    ///
    /// Must be called from the process's actual main thread -- see this module's own doc comment
    /// and [`Self::new`]'s panic section. Unlike Windows/Linux userland, this is NOT the thread a
    /// macOS runner's guest execution (`run_thread`) should run on; guest execution has to move to
    /// a dedicated background thread instead, the inverse of those runners' arrangement.
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
    /// See `litebox_platform_windows_userland::presentation::PresenterApp::last_frame`'s doc
    /// comment -- the same `resumed()`-vs-first-page-flip race is possible on this platform too, so
    /// the same replay-on-resume handling is kept.
    last_frame: Option<Frame>,
    input_consumer: Option<Box<dyn Fn(InputSignal) + Send>>,
    last_cursor_pos: Option<(f64, f64)>,
}

impl PresenterApp {
    /// Upload `frame`'s pixel bytes as a `wgpu` texture and blit them onto the window surface via
    /// a plain full-screen copy. Identical to `litebox_platform_windows_userland::presentation::
    /// PresenterApp::present` -- this whole method is platform-agnostic `wgpu` code.
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
            format: wgpu::TextureFormat::Bgra8Unorm,
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
        // `wgpu::Instance::default()`: on macOS this resolves to the Metal backend, `wgpu`'s only
        // real backend on this platform (its Vulkan support there is itself a MoltenVK/Vulkan-on-
        // Metal translation layer `wgpu` does not select by default) -- no Windows-style forced-
        // backend override is applicable or needed here.
        let instance = wgpu::Instance::default();
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
        if self.last_frame.is_some()
            && let Some(state) = &self.state
        {
            state.window.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        // See `litebox_platform_windows_userland::presentation::PresenterApp::user_event`'s doc
        // comment for why presentation is deferred to `RedrawRequested` -- winit's own cross-
        // backend contract, not Windows-specific.
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
                if let Some((last_x, last_y)) = self.last_cursor_pos {
                    // A real mouse's own relative-motion sensor cannot report a single-event
                    // delta anywhere near `i32`'s range, so this narrowing is exact in practice,
                    // not a real precision loss to guard against.
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
