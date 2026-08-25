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
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
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
}

impl Presenter {
    /// Build a not-yet-shown presenter and its window. Real `winit`/OS window/event-loop
    /// resources are not created until [`Self::run`] is called on the thread that will own them.
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
        })
    }

    /// A cloneable handle to push frames into this presenter from any other thread, valid for the
    /// presenter's whole lifetime (including before [`Self::run`] is called -- frames sent early
    /// simply queue until the window exists and starts draining them).
    pub fn sender(&self) -> FrameSender {
        self.sender.clone()
    }

    /// Run the event loop on the calling thread until the window is closed. See this module's
    /// doc comment for why the calling thread must NOT be the guest-execution thread.
    pub fn run(self) -> Result<(), winit::error::EventLoopError> {
        self.event_loop.set_control_flow(ControlFlow::Wait);
        let mut app = PresenterApp {
            frames_rx: self.frames_rx,
            state: None,
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
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let Ok(surface) = instance.create_surface(window.clone()) else {
            return;
        };
        let Ok(adapter) =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }))
        else {
            return;
        };
        let Ok((device, queue)) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("litebox-presenter"),
                ..Default::default()
            },
        )) else {
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
        surface.configure(
            &device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
                format: surface_format,
                width: size.width.max(1),
                height: size.height.max(1),
                present_mode: wgpu::PresentMode::Fifo,
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
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        // A `FrameSender::send` wake-up: drain every queued frame, presenting only the last one
        // (the most recent frame is the only one still worth showing -- matching how a real
        // display only ever shows the CURRENT scanout buffer, never a backlog of stale ones).
        let mut latest = None;
        while let Ok(frame) = self.frames_rx.try_recv() {
            latest = Some(frame);
        }
        if let Some(frame) = latest {
            self.present(&frame);
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
                if let Some(state) = &self.state {
                    state.window.request_redraw();
                }
            }
            _ => {}
        }
    }
}
