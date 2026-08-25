// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Host-side GUI presentation: a real Linux (X11 or Wayland) window, backed by `wgpu`, that
//! displays pixel buffers a guest DRM client has drawn into (see `litebox_shim_linux::syscalls::
//! drm`'s `DrmSubsystem`). Ported from `litebox_platform_windows_userland::presentation` -- see
//! that module's doc comment for the shared design this mirrors; only the genuinely
//! platform-specific pieces (noted below) differ.
//!
//! # Why a dedicated OS thread (same answer as Windows, different reason)
//!
//! `litebox_runner_linux_userland`'s own main thread calls
//! `litebox_platform_linux_userland::run_thread` directly to execute the guest, blocking until it
//! exits -- there is no spare "main loop" slot for `winit`'s own event loop to share, exactly as on
//! Windows. Unlike macOS' Cocoa (which imposes a HARD OS-level requirement that all windowing/UI
//! code run on the process' first/main thread -- `winit` cannot work around this on that platform),
//! neither X11 nor Wayland's own client libraries impose any such constraint: an X11 display
//! connection or a Wayland client connection is an ordinary socket-backed handle usable from any
//! thread. `winit`'s default refusal to build an `EventLoop` off the main thread on these backends
//! (`EventLoopBuilderExtX11`/`EventLoopBuilderExtWayland`'s `with_any_thread`, both gate the exact
//! same underlying `any_thread` flag) is a conservative cross-platform-compatibility guard, not a
//! reflection of a real X11/Wayland constraint -- confirmed by reading `winit` 0.30.13's own source
//! (`src/platform/x11.rs`, `src/platform/wayland.rs`): both extension traits' doc comments say so
//! explicitly ("to make platform compatibility easier"), matching this crate's Windows counterpart
//! exactly. So, as on Windows, `winit`'s `EventLoop` runs correctly on a plain spawned thread here.
//!
//! # What is NOT yet independently verified (honest limitation, unlike the Windows module)
//!
//! This port has no real X11/Wayland display available in the environment it was written in (no
//! `DISPLAY`, no Wayland socket, no `Xvfb` installed) -- so, unlike `litebox_platform_windows_
//! userland::presentation` (independently screenshot-verified live, twice, with two different
//! solid colors), this module is build-verified only (`cargo check --target x86_64-unknown-linux-
//! gnu`), not run-verified. Two specific things the Windows module needed a live fix for that this
//! port has NOT been able to confirm one way or the other on real Linux windowing:
//!
//! 1. Whether `wgpu`'s default backend set (`Backends::all()`, which on Linux normally resolves to
//!    Vulkan) has any equivalent to the Windows module's forced-DX12 workaround for a
//!    `Surface::get_current_texture()` hang. No such issue is documented against `wgpu`'s Vulkan
//!    backend on Linux, and forcing a backend without a reproduced problem to justify it would be
//!    exactly the kind of unverified guess this project's own discipline forbids -- so this port
//!    deliberately leaves `wgpu::Instance::default()` (every backend `wgpu` can find) rather than
//!    copying Windows' `Backends::DX12` override. If a real Linux host later reproduces a similar
//!    hang, narrow the backend set the same way, with the same live-repro rigor.
//! 2. Whether presenting only from `RedrawRequested` (never directly from `user_event`) is
//!    necessary here the way it was on Windows. It is kept anyway: it is correct on every winit
//!    backend by the crate's own contract (`RedrawRequested` is the only point every backend
//!    guarantees a presentable surface), not a Windows-only workaround, so there is no reason to
//!    special-case it away pending Linux-specific verification.
//!
//! Both are flagged in the `gui-macos-linux-presentation-port` PRD row's follow-up rather than
//! silently assumed identical to Windows.

use std::sync::mpsc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{
    ActiveEventLoop, ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy,
};
use winit::keyboard::{KeyCode, PhysicalKey};
// Either `EventLoopBuilderExtX11` or `EventLoopBuilderExtWayland` would do here -- both traits
// gate the exact same underlying `EventLoopBuilder::platform_specific.any_thread` field (confirmed
// by reading winit's own source, see this module's doc comment), so importing one is sufficient
// regardless of which backend is actually selected at runtime. The X11 trait is used since it is
// the one built even on distros/CI images with no Wayland compositor at all.
use winit::platform::x11::EventLoopBuilderExtX11;
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
/// `KeyCode` is itself platform-independent (based on the physical-key standard, not a native
/// scancode), so the same table is correct on every host, not just the one it was written against.
fn winit_keycode_to_evdev(key: KeyCode) -> Option<u16> {
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
/// (`DrmSubsystem::page_flip`, via `litebox_runner_linux_userland`'s `--gui` wiring). Sending
/// after the presenter's window has closed is a silent no-op.
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
/// window is closed. Call [`Presenter::run`] on a dedicated thread (see this module's doc comment
/// for why); it blocks for the window's entire lifetime.
pub struct Presenter {
    event_loop: EventLoop<()>,
    frames_rx: mpsc::Receiver<Frame>,
    sender: FrameSender,
    input_consumer: Option<Box<dyn Fn(InputSignal) + Send>>,
}

impl Presenter {
    /// Build a not-yet-shown presenter and its window. Real `winit`/OS window/event-loop resources
    /// are not created until [`Self::run`] is called on the thread that will own them.
    ///
    /// `with_any_thread(true)`: see this module's doc comment for why this is safe on X11/Wayland
    /// (a conservative `winit` default, not a real host constraint), unlike the identical-looking
    /// call on macOS, which cannot use this escape hatch at all.
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
    /// presenter's whole lifetime.
    pub fn sender(&self) -> FrameSender {
        self.sender.clone()
    }

    /// Register `consumer` to be called, on the presenter's own event-loop thread, with every real
    /// keyboard/mouse event this window observes from [`Self::run`]'s call onward. See
    /// `litebox_platform_windows_userland::presentation::Presenter::set_input_consumer`'s doc
    /// comment for the full rationale (identical here).
    pub fn set_input_consumer(&mut self, consumer: impl Fn(InputSignal) + Send + 'static) {
        self.input_consumer = Some(Box::new(consumer));
    }

    /// Run the event loop on the calling thread until the window is closed. See this module's doc
    /// comment for why the calling thread must NOT be the guest-execution thread.
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
    /// comment -- the same `resumed()`-vs-first-page-flip race is possible on this platform too
    /// (nothing about it is Windows-specific: it is a race between this thread's own async window
    /// setup and whatever other thread produces the first frame), so the same replay-on-resume
    /// handling is kept.
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
        // `wgpu::Instance::default()`, not a forced backend: see this module's doc comment for why
        // Windows' `Backends::DX12` override is deliberately NOT copied here without a reproduced
        // problem to justify it.
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
        if self.last_frame.is_some() {
            if let Some(state) = &self.state {
                state.window.request_redraw();
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        // See `litebox_platform_windows_userland::presentation::PresenterApp::user_event`'s doc
        // comment for why presentation is deferred to `RedrawRequested` rather than happening
        // directly here -- that reasoning is winit's own cross-backend contract, not specific to
        // the Windows backend that motivated documenting it.
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
                    let dx = (position.x - last_x) as i32;
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
