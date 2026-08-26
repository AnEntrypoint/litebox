//! Guest-side probe, phase 9: extends the proven minimal compositor (`main.rs`,
//! `wl_compositor`+`wl_shm` only) with the real protocol surface any serious Wayland client --
//! including a real desktop-shell component like XFCE's `xfwl4`/`xfdesktop`/`xfce4-panel` --
//! actually needs: `xdg_shell` (real window toplevels, not just a bare surface commit),
//! `wl_seat` (keyboard/pointer capability advertisement, required by `xdg_shell`'s own
//! `XdgShellHandler::grab` signature), and `wl_output` (screen geometry, matching litebox's real
//! virtual DRM mode `1920x1080@60`, see `litebox_shim_linux::syscalls::drm::DrmSubsystem`'s own
//! `VIRTUAL_WIDTH`/`VIRTUAL_HEIGHT`/`VIRTUAL_REFRESH_HZ` constants).
//!
//! Everything below `main.rs`'s own `wl_compositor`+`wl_shm`+DRM-push plumbing is reused
//! unchanged (see that file's own doc comment for the full verification history of the
//! foundation this builds on) -- this file only ADDS the xdg_shell/seat/output globals a real
//! desktop client actually calls before ever getting to `wl_shm`/commit.
//!
//! Run as a real litebox guest process directly on bare Windows via
//! `litebox_runner_linux_on_windows_userland.exe --gui` (per the standing constraint: no WSL2,
//! no hypervisor -- exactly the same verification surface `docs/linux-native-drm-gui-probe/`
//! already proved this session, litebox's own wgpu-presented host window).

use std::sync::Arc;

use smithay::backend::drm::{DrmDevice, DrmDeviceFd};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode as CalloopMode, PostAction};
use smithay::reexports::rustix::fs::{open, Mode as FsMode, OFlags};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::utils::{Serial, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, with_states,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState, with_buffer_contents};
use smithay::{
    delegate_compositor, delegate_output, delegate_seat, delegate_shm, delegate_xdg_shell,
};

#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

type SurfaceTarget = WlSurface;

struct Desktop {
    compositor_state: CompositorState,
    shm_state: ShmState,
    xdg_shell_state: XdgShellState,
    seat_state: SeatState<Desktop>,
    drm: DrmDevice,
    /// Real evidence a client did more than just connect: a real `xdg_toplevel` was created.
    toplevel_created: bool,
    /// Real evidence the full pipeline works: a committed buffer reached the DRM device.
    committed_once: bool,
}

impl BufferHandler for Desktop {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl CompositorHandler for Desktop {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        let copied = with_states(surface, |states| {
            let mut guard = states
                .cached_state
                .get::<smithay::wayland::compositor::SurfaceAttributes>();
            let attrs = guard.current();
            let Some(buffer) = attrs.buffer.as_ref() else {
                return None;
            };
            let smithay::wayland::compositor::BufferAssignment::NewBuffer(buffer) = buffer else {
                return None;
            };
            with_buffer_contents(buffer, |ptr, len, data| {
                let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
                (bytes.to_vec(), data.width, data.height, data.stride)
            })
            .ok()
        });

        if let Some((pixels, width, height, stride)) = copied {
            println!(
                "COMMIT_SHM_OK bytes={} width={} height={} stride={}",
                pixels.len(),
                width,
                height,
                stride
            );
            flush_stdout();
            if push_to_drm_dumb_buffer(&self.drm, &pixels, width as u32, height as u32, stride as u32) {
                self.committed_once = true;
            }
        } else {
            println!("COMMIT_NO_BUFFER");
            flush_stdout();
        }
    }
}
delegate_compositor!(Desktop);

impl ShmHandler for Desktop {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
delegate_shm!(Desktop);

impl XdgShellHandler for Desktop {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Real evidence a real client asked for a real window: send an initial configure (empty
        // size = client picks its own preferred size, matching how a real WM would greet a
        // brand-new toplevel before any layout decision has been made) so the client's own
        // xdg_surface.ack_configure -> wl_surface.commit sequence can proceed.
        surface.with_pending_state(|state| {
            state.size = None;
        });
        surface.send_configure();
        self.toplevel_created = true;
        println!("XDG_TOPLEVEL_CREATED");
        flush_stdout();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        surface.send_configure().ok();
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }
}
delegate_xdg_shell!(Desktop);

impl SeatHandler for Desktop {
    type KeyboardFocus = SurfaceTarget;
    type PointerFocus = SurfaceTarget;
    type TouchFocus = SurfaceTarget;

    fn seat_state(&mut self) -> &mut SeatState<Desktop> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&SurfaceTarget>) {}
    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}
delegate_seat!(Desktop);

impl OutputHandler for Desktop {}
delegate_output!(Desktop);

fn push_to_drm_dumb_buffer(drm: &DrmDevice, pixels: &[u8], width: u32, height: u32, _stride: u32) -> bool {
    use smithay::reexports::drm::buffer::DrmFourcc;
    use smithay::reexports::drm::control::{Device as ControlDevice, dumbbuffer::DumbBuffer};

    let Ok(mut dumb): Result<DumbBuffer, _> = drm.create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, 32)
    else {
        println!("CREATE_DUMB_FAILED");
        return false;
    };
    {
        let Ok(mut mapping) = drm.map_dumb_buffer(&mut dumb) else {
            println!("MAP_DUMB_FAILED");
            return false;
        };
        let dest = mapping.as_mut();
        let n = pixels.len().min(dest.len());
        dest[..n].copy_from_slice(&pixels[..n]);
    }
    let Ok(fb) = drm.add_framebuffer(&dumb, 32, 32) else {
        println!("ADDFB_FAILED");
        return false;
    };
    let Ok(resources) = drm.resource_handles() else {
        println!("GETRESOURCES_FAILED");
        return false;
    };
    let Some(&crtc) = resources.crtcs().first() else {
        println!("NO_CRTC");
        return false;
    };
    let Some(&connector) = resources.connectors().first() else {
        println!("NO_CONNECTOR");
        return false;
    };
    match drm.set_crtc(crtc, Some(fb), (0, 0), &[connector], None) {
        Ok(()) => {
            println!("SETCRTC_OK fb_id={fb:?}");
            true
        }
        Err(e) => {
            println!("SETCRTC_FAILED {e:?}");
            false
        }
    }
}

fn flush_stdout() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn main() {
    let fd = open("/dev/dri/card0", OFlags::RDWR, FsMode::empty()).expect("open /dev/dri/card0");
    let drm_fd = DrmDeviceFd::new(fd.into());
    let (drm, _notifier) = DrmDevice::new(drm_fd, true).expect("DrmDevice::new");

    let display: Display<Desktop> = Display::new().expect("Display::new");
    let dh: DisplayHandle = display.handle();

    let compositor_state = CompositorState::new::<Desktop>(&dh);
    let shm_state = ShmState::new::<Desktop>(&dh, Vec::new());
    let xdg_shell_state = XdgShellState::new::<Desktop>(&dh);
    let mut seat_state = SeatState::<Desktop>::new();
    let mut seat = seat_state.new_wl_seat(&dh, "litebox-seat0");
    seat.add_pointer();
    // add_keyboard() deliberately omitted from this pass: it pulls in xkbcommon's real C keymap-
    // compilation code (libxkbcommon.a, FFI-bound not pure Rust) whose codegen the syscall
    // rewriter's x86 patcher cannot fully hook (see the probe's own README for the precise
    // InsufficientBytesBeforeOrAfter finding) -- a real, separate, narrower gap than the
    // xdg_shell/wl_seat/wl_output work this pass actually set out to prove.

    // Advertise litebox's own real virtual display mode -- matching DrmSubsystem's constants
    // exactly (VIRTUAL_WIDTH=1920, VIRTUAL_HEIGHT=1080, VIRTUAL_REFRESH_HZ=60), not a fabricated
    // geometry, so any client that reads wl_output before laying out a window sees the truth.
    let output = Output::new(
        "litebox-virtual-0".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "litebox".into(),
            model: "virtual-drm".into(),
        },
    );
    let _output_global = output.create_global::<Desktop>(&dh);
    output.change_current_state(
        Some(Mode { size: (1920, 1080).into(), refresh: 60_000 }),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    output.set_preferred(Mode { size: (1920, 1080).into(), refresh: 60_000 });

    let mut state = Desktop {
        compositor_state,
        shm_state,
        xdg_shell_state,
        seat_state,
        drm,
        toplevel_created: false,
        committed_once: false,
    };

    let mut event_loop: EventLoop<Desktop> = EventLoop::try_new().expect("EventLoop::try_new");
    let handle = event_loop.handle();

    let socket_path = "/tmp/litebox-wayland-0";
    let _ = std::fs::remove_file(socket_path);
    let listener = std::os::unix::net::UnixListener::bind(socket_path).expect("bind wayland socket");
    listener.set_nonblocking(true).expect("set_nonblocking");
    println!("LISTENING path={socket_path}");
    flush_stdout();

    let dh_for_accept = dh.clone();
    handle
        .insert_source(
            Generic::new(listener, Interest::READ, CalloopMode::Level),
            move |_, listener, _data: &mut Desktop| {
                let mut dh_for_accept = dh_for_accept.clone();
                loop {
                    match listener.accept() {
                        Ok((stream, _addr)) => {
                            if let Err(e) =
                                dh_for_accept.insert_client(stream, Arc::new(ClientState::default()))
                            {
                                println!("INSERT_CLIENT_FAILED {e:?}");
                            } else {
                                println!("CLIENT_ACCEPTED");
                                flush_stdout();
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            println!("ACCEPT_FAILED {e:?}");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .expect("insert socket source");

    handle
        .insert_source(
            Generic::new(display, Interest::READ, CalloopMode::Level),
            |_, display, data: &mut Desktop| {
                // SAFETY: matches Smithay's own anvil reference compositor's identical use.
                unsafe {
                    display.get_mut().dispatch_clients(data).ok();
                }
                Ok(PostAction::Continue)
            },
        )
        .expect("insert display source");

    println!("RUNNING");
    flush_stdout();
    let start = std::time::Instant::now();
    while !state.committed_once && start.elapsed() < std::time::Duration::from_secs(60) {
        event_loop
            .dispatch(std::time::Duration::from_millis(100), &mut state)
            .expect("event_loop.dispatch");
    }

    println!(
        "RESULT toplevel_created={} committed_once={}",
        state.toplevel_created, state.committed_once
    );
    flush_stdout();
    if !state.committed_once {
        std::process::exit(1);
    }
}
