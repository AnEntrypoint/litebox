//! Guest-side probe, phase 2: a genuinely minimal Wayland COMPOSITOR (not just the DRM backend
//! layer phase 1 already proved) -- a real Unix-socket-listening `wl_display` that accepts a real
//! Wayland client, advertises `wl_compositor`+`wl_shm`, and on the client's first `wl_surface`
//! commit copies the attached `wl_shm` buffer's pixel bytes into a litebox DRM dumb buffer via the
//! already-proven `backend_drm` layer. See `README.md` for the full verification history and the
//! reproducible musl-linking recipe (phase 1: `cargo check`-only -> phase 1.5: real link+run,
//! commit `0778cb2` -> this phase: wayland_frontend added on top of the same working backend_drm
//! layer, see PRD row `gui-wayland-compositor-on-drm-future`).
//!
//! Empirically confirmed, not assumed: `smithay`'s `wayland_frontend` feature
//! (`["wayland-server", "wayland-protocols", "wayland-protocols-wlr", "wayland-protocols-misc",
//! "tempfile"]` per Smithay's own `Cargo.toml`) pulls in `wayland-server`/`wayland-backend`, but
//! NOT `wayland-backend/server_system` (that's a separate `use_system_lib` feature, not enabled
//! here) -- so `wayland-backend` uses its own pure-Rust wire-protocol implementation, not a real
//! `libwayland-server.so` FFI binding. `cargo check --target x86_64-unknown-linux-musl` and a real
//! `cargo zigbuild --target x86_64-unknown-linux-musl` link (see README) both confirmed this: zero
//! new native-linking dependencies beyond what `backend_drm` alone already required.
//!
//! Deliberately minimal, NOT a full compositor: no `xdg_shell` (window management), no seat/input,
//! no output/layer-shell protocols -- just enough of `wl_compositor`+`wl_shm` for the smallest
//! possible real client (attach one buffer, commit once) to prove the whole pipeline: real
//! Wayland wire protocol -> real `wl_shm` buffer -> real DRM dumb-buffer copy -> (if `--gui` were
//! wired end-to-end by the runner, not attempted in this standalone probe) a host window.
//!
//! **Verification status (live-tested, not assumed)**: compiles AND links cleanly for
//! `x86_64-unknown-linux-musl` (same zig-based recipe as phase 1.5, see `README.md`) -- a real
//! statically-linked ELF64 binary. Run as a real guest process under
//! `litebox_runner_linux_on_windows_userland.exe`, it genuinely binds the Unix socket and prints
//! `LISTENING path=/tmp/litebox-wayland-0` -- and previously hit a REAL, previously-undiscovered
//! litebox gap right after: `calloop`'s epoll backend registers an epoll fd as a member of another
//! epoll set (nested `epoll_ctl(EPOLL_CTL_ADD)` on an epoll fd), which
//! `litebox_shim_linux::syscalls::epoll::EpollDescriptor::poll`'s `EpollDescriptor::Epoll(_file)
//! => unimplemented!()` arm panicked on outright.
//!
//! **Nested-epoll support has since landed and been live-verified against this exact probe**
//! (`litebox_shim_linux::syscalls::epoll`'s `EpollFile` now implements `IOPollable` -- readiness
//! delegates to the inner epoll's own `ready` set, which is exactly what a direct `epoll_wait`
//! caller already polls, so an outer epoll registering an observer there gets woken by precisely
//! the same `ReadySet::push`/`notify_observers` call a direct waiter would; no separate readiness
//! or wakeup machinery needed). Re-running this exact probe now prints `LISTENING` -> `RUNNING`
//! (the line that immediately follows registering the nested-epoll `calloop` source -- previously
//! unreachable) and runs its full 30-second `event_loop.dispatch()` loop (repeatedly exercising
//! the nested-epoll `poll()` path) with zero panic, correctly printing
//! `NO_CLIENT_COMMIT_WITHIN_TIMEOUT` and exiting 1 once no real client connects within the bound
//! -- exactly the expected outcome with no Wayland client available in this environment to
//! connect. Client-connect and pixel-commit verification (this file's actual `Compositor::commit`/
//! `push_to_drm_dumb_buffer` logic) remain the concrete next step for whoever has a real Wayland
//! client to test against; see PRD row `gui-wayland-compositor-on-drm-future`.

use std::sync::Arc;

use smithay::backend::drm::{DrmDevice, DrmDeviceFd};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::rustix::fs::{open, Mode as FsMode, OFlags};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, with_states,
};
use smithay::wayland::shm::{ShmHandler, ShmState, with_buffer_contents};
use smithay::{delegate_compositor, delegate_shm};

/// Per-client bookkeeping Smithay's compositor machinery requires -- just the mandatory
/// `CompositorClientState`, nothing this minimal probe needs beyond it.
#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// The compositor's whole state: the two required Wayland globals, plus the already-proven DRM
/// backend handle from phase 1 to push committed pixels through.
struct Compositor {
    compositor_state: CompositorState,
    shm_state: ShmState,
    drm: DrmDevice,
    /// Set once a client's first commit is observed and successfully copied into a DRM dumb
    /// buffer -- the probe's actual success signal (checked by `main` after the event loop exits).
    committed_once: bool,
}

impl BufferHandler for Compositor {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl CompositorHandler for Compositor {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Real evidence this pipeline works, not a stub: pull the client's actual `wl_shm`
        // pixel bytes out of the committed surface state and hand them to litebox's own,
        // already-proven-live DRM dumb-buffer path (CREATE_DUMB/MAP_DUMB/ADDFB2, verified this
        // session's own `docs/x11-libdrm-client-probe/`) via a real ioctl sequence -- exactly
        // the same mechanism a real guest-side compositor would use to scan out a client's frame.
        let copied = with_states(surface, |states| {
            let mut guard = states.cached_state.get::<smithay::wayland::compositor::SurfaceAttributes>();
            let attrs = guard.current();
            let Some(buffer) = attrs.buffer.as_ref() else {
                return None;
            };
            let smithay::wayland::compositor::BufferAssignment::NewBuffer(buffer) = buffer else {
                return None;
            };
            with_buffer_contents(buffer, |ptr, len, data| {
                // Copy now, while the client's shm pool is still valid -- `with_buffer_contents`
                // only guarantees the pointer inside this closure.
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
        }
    }
}

delegate_compositor!(Compositor);

impl ShmHandler for Compositor {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_shm!(Compositor);

/// Push a client's committed SHM pixels through the already-proven DRM dumb-buffer pipeline:
/// `CREATE_DUMB` -> `MAP_DUMB`+`mmap()` -> copy -> `ADDFB2` -> `SETCRTC`. Uses `smithay`'s own
/// `DrmDevice` handle (the exact object phase 1 already proved enumerates litebox's virtual
/// connector/CRTC correctly), not a hand-rolled ioctl call, so this is real backend-layer reuse,
/// not a second, parallel implementation.
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

/// See `client.rs`'s identical helper for why this is needed: stdout is fully buffered (not
/// line-buffered) whenever it isn't a real TTY, which litebox's guest stdout is not -- without
/// an explicit flush after every print, this process's real progress stays invisible to a
/// parent/host reading its output until process exit, making genuinely-working code look hung.
fn flush_stdout() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn main() {
    let fd = open("/dev/dri/card0", OFlags::RDWR, FsMode::empty()).expect("open /dev/dri/card0");
    let drm_fd = DrmDeviceFd::new(fd.into());
    let (drm, _notifier) = DrmDevice::new(drm_fd, true).expect("DrmDevice::new");

    let display: Display<Compositor> = Display::new().expect("Display::new");
    let dh: DisplayHandle = display.handle();

    let compositor_state = CompositorState::new::<Compositor>(&dh);
    let shm_state = ShmState::new::<Compositor>(&dh, Vec::new());

    let mut state = Compositor {
        compositor_state,
        shm_state,
        drm,
        committed_once: false,
    };

    let mut event_loop: EventLoop<Compositor> = EventLoop::try_new().expect("EventLoop::try_new");
    let handle = event_loop.handle();

    // Real Unix-domain-socket listener at a well-known guest path (no XDG_RUNTIME_DIR machinery
    // needed for this minimal probe -- a real compositor would set WAYLAND_DISPLAY for clients to
    // find it; this probe's own test client below connects to the fixed path directly instead).
    let socket_path = "/tmp/litebox-wayland-0";
    let _ = std::fs::remove_file(socket_path);
    let listener = std::os::unix::net::UnixListener::bind(socket_path).expect("bind wayland socket");
    listener.set_nonblocking(true).expect("set_nonblocking");
    println!("LISTENING path={socket_path}");
    flush_stdout();

    // `DisplayHandle` is a cheap, cloneable handle (an internal Arc) -- captured directly by the
    // accept closure below rather than stored on `Compositor` itself (which would need a
    // self-referential field to hold both the handle and the state it dispatches into).
    let dh_for_accept = dh.clone();
    handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, _data: &mut Compositor| {
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
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, data: &mut Compositor| {
                // SAFETY: `display` is not dropped elsewhere; matches Smithay's own anvil
                // reference compositor's identical use of this exact API.
                unsafe {
                    display.get_mut().dispatch_clients(data).ok();
                }
                Ok(PostAction::Continue)
            },
        )
        .expect("insert display source");

    println!("RUNNING");
    flush_stdout();
    // Bounded run: this is a probe, not a long-lived service -- exit once a real client has
    // connected AND committed a real buffer (success), or after a generous timeout (no client
    // showed up -- still real information, printed below, not silently swallowed).
    let start = std::time::Instant::now();
    while !state.committed_once && start.elapsed() < std::time::Duration::from_secs(30) {
        event_loop
            .dispatch(std::time::Duration::from_millis(100), &mut state)
            .expect("event_loop.dispatch");
    }

    if state.committed_once {
        println!("ALL_OK");
    } else {
        println!("NO_CLIENT_COMMIT_WITHIN_TIMEOUT");
        std::process::exit(1);
    }
}
