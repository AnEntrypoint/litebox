//! Full end-to-end probe: a real `wayland-client` connecting to a real, unmodified-shape Wayland
//! compositor built on litebox's DRM emulation, with the commit handler pushing the client's
//! committed `wl_shm` pixels all the way through `push_to_drm_dumb_buffer` (`main.rs`'s own real
//! `CREATE_DUMB`/`MAP_DUMB`/`ADDFB2`/`SETCRTC` sequence against litebox's virtual DRM device) --
//! the one step `combined.rs` deliberately left out (see that file's own doc comment) to keep its
//! protocol-roundtrip repro minimal. This file exists specifically to close PRD row
//! `gui-wayland-compositor-on-drm-future`'s own last-named gap: "witness
//! `Compositor::commit`/`push_to_drm_dumb_buffer` actually copying pixels through".
//!
//! Same single-process, two-thread shape as `combined.rs` (compositor on a background thread,
//! client on the main thread -- no `fork()`/`execve()`, which crashes guest processes for an
//! unrelated, already-tracked reason, see `fork-execve-mallocng-null-meta-crash`). Also carries
//! forward the one real fix `combined.rs`'s own investigation found: the display event source
//! must call `flush_clients()` after `dispatch_clients()`, or the compositor's own queued replies
//! never reach the client and every round-trip after the first silently stalls. `main.rs` itself
//! still lacks this call as of this writing -- harmless there today only because no real client
//! ever reaches a second round-trip against it, but the same gap nonetheless.
use std::sync::Arc;
use std::sync::mpsc;

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

fn flush_stdout() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Real success signal sent back to `main()`: whether `push_to_drm_dumb_buffer` itself reported
/// success (not just "a buffer was committed") -- the actual question this probe answers.
struct Compositor {
    compositor_state: CompositorState,
    shm_state: ShmState,
    drm: DrmDevice,
    committed_once: bool,
    result_tx: mpsc::Sender<(usize, u32, u32, u32, bool)>,
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
                let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
                (bytes.to_vec(), data.width, data.height, data.stride)
            })
            .ok()
        });

        if let Some((pixels, width, height, stride)) = copied {
            println!(
                "COMMIT_SHM_OK bytes={} width={} height={} stride={} first4={:02X?}",
                pixels.len(),
                width,
                height,
                stride,
                &pixels[..4.min(pixels.len())]
            );
            flush_stdout();
            let pushed = push_to_drm_dumb_buffer(&self.drm, &pixels, width as u32, height as u32, stride as u32);
            let _ = self
                .result_tx
                .send((pixels.len(), width as u32, height as u32, stride as u32, pushed));
            self.committed_once = true;
        } else {
            println!("COMMIT_NO_BUFFER");
            flush_stdout();
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

/// Identical to `main.rs`'s own helper -- real backend-layer reuse via `smithay`'s `DrmDevice`,
/// not a hand-rolled ioctl sequence or a second, parallel implementation.
fn push_to_drm_dumb_buffer(drm: &DrmDevice, pixels: &[u8], width: u32, height: u32, _stride: u32) -> bool {
    use smithay::reexports::drm::buffer::DrmFourcc;
    use smithay::reexports::drm::control::{Device as ControlDevice, dumbbuffer::DumbBuffer};

    let Ok(mut dumb): Result<DumbBuffer, _> = drm.create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, 32)
    else {
        println!("CREATE_DUMB_FAILED");
        flush_stdout();
        return false;
    };

    {
        let Ok(mut mapping) = drm.map_dumb_buffer(&mut dumb) else {
            println!("MAP_DUMB_FAILED");
            flush_stdout();
            return false;
        };
        let dest = mapping.as_mut();
        let n = pixels.len().min(dest.len());
        dest[..n].copy_from_slice(&pixels[..n]);
        println!("DUMB_COPY_OK bytes_copied={n} dest_len={}", dest.len());
        flush_stdout();
    }

    // (depth=24, bpp=32): real XRGB8888 color depth is 24 bits (the top byte is padding, not
    // alpha) even though each pixel occupies 32 bits -- litebox's own `add_fb` handler enforces
    // exactly this real-kernel distinction (`req.bpp == 32 && req.depth == 24`) and rejects
    // `depth=32` as a format this single-format device does not support.
    let Ok(fb) = drm.add_framebuffer(&dumb, 24, 32) else {
        println!("ADDFB_FAILED");
        flush_stdout();
        return false;
    };

    let Ok(resources) = drm.resource_handles() else {
        println!("GETRESOURCES_FAILED");
        flush_stdout();
        return false;
    };
    let Some(&crtc) = resources.crtcs().first() else {
        println!("NO_CRTC");
        flush_stdout();
        return false;
    };
    let Some(&connector) = resources.connectors().first() else {
        println!("NO_CONNECTOR");
        flush_stdout();
        return false;
    };

    match drm.set_crtc(crtc, Some(fb), (0, 0), &[connector], None) {
        Ok(()) => {
            println!("SETCRTC_OK fb_id={fb:?}");
            flush_stdout();
            true
        }
        Err(e) => {
            println!("SETCRTC_FAILED {e:?}");
            flush_stdout();
            false
        }
    }
}

fn run_compositor(result_tx: mpsc::Sender<(usize, u32, u32, u32, bool)>) {
    let fd = open("/dev/dri/card0", OFlags::RDWR, FsMode::empty()).expect("open /dev/dri/card0");
    let drm_fd = DrmDeviceFd::new(fd.into());
    let (drm, _notifier) = DrmDevice::new(drm_fd, true).expect("DrmDevice::new");

    let display: Display<Compositor> = Display::new().expect("Display::new");
    let dh: DisplayHandle = display.handle();
    let compositor_state = CompositorState::new::<Compositor>(&dh);
    let shm_state = ShmState::new::<Compositor>(&dh, Vec::new());

    let mut state = Compositor { compositor_state, shm_state, drm, committed_once: false, result_tx };

    let mut event_loop: EventLoop<Compositor> = EventLoop::try_new().expect("EventLoop::try_new");
    let handle = event_loop.handle();

    let socket_path = "/tmp/litebox-wayland-drm-0";
    let _ = std::fs::remove_file(socket_path);
    let listener = std::os::unix::net::UnixListener::bind(socket_path).expect("bind wayland socket");
    listener.set_nonblocking(true).expect("set_nonblocking");
    println!("LISTENING path={socket_path}");
    flush_stdout();

    let dh_for_accept = dh.clone();
    handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, _data: &mut Compositor| {
                let mut dh_for_accept = dh_for_accept.clone();
                loop {
                    match listener.accept() {
                        Ok((stream, _addr)) => {
                            if let Err(e) = dh_for_accept.insert_client(stream, Arc::new(ClientState::default())) {
                                println!("INSERT_CLIENT_FAILED {e:?}");
                            } else {
                                println!("CLIENT_ACCEPTED");
                            }
                            flush_stdout();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            println!("ACCEPT_FAILED {e:?}");
                            flush_stdout();
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
                unsafe {
                    display.get_mut().dispatch_clients(data).ok();
                    // See this file's own doc comment: without this, queued replies never reach
                    // the client and the second round-trip stalls forever.
                    display.get_mut().flush_clients().ok();
                }
                Ok(PostAction::Continue)
            },
        )
        .expect("insert display source");

    println!("RUNNING");
    flush_stdout();
    let start = std::time::Instant::now();
    while !state.committed_once && start.elapsed() < std::time::Duration::from_secs(20) {
        event_loop.dispatch(std::time::Duration::from_millis(100), &mut state).expect("dispatch");
    }
    if !state.committed_once {
        println!("COMPOSITOR_TIMEOUT");
        flush_stdout();
    }
}

fn run_client() {
    use std::os::unix::io::AsFd;
    use std::os::unix::net::UnixStream;

    use wayland_client::protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface};
    use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

    struct AppState {
        compositor: Option<wl_compositor::WlCompositor>,
        shm: Option<wl_shm::WlShm>,
    }
    impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
        fn event(state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
            if let wl_registry::Event::Global { name, interface, version } = event {
                println!("GLOBAL name={name} interface={interface} version={version}");
                flush_stdout();
                match interface.as_str() {
                    "wl_compositor" => state.compositor = Some(registry.bind::<wl_compositor::WlCompositor, _, _>(name, version.min(4), qh, ())),
                    "wl_shm" => state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, version.min(1), qh, ())),
                    _ => {}
                }
            }
        }
    }
    impl Dispatch<wl_compositor::WlCompositor, ()> for AppState {
        fn event(_: &mut Self, _: &wl_compositor::WlCompositor, _: wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_shm::WlShm, ()> for AppState {
        fn event(_: &mut Self, _: &wl_shm::WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_shm_pool::WlShmPool, ()> for AppState {
        fn event(_: &mut Self, _: &wl_shm_pool::WlShmPool, _: wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_buffer::WlBuffer, ()> for AppState {
        fn event(_: &mut Self, _: &wl_buffer::WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_surface::WlSurface, ()> for AppState {
        fn event(_: &mut Self, _: &wl_surface::WlSurface, _: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }

    fn rustix_memfd() -> Result<std::os::unix::io::OwnedFd, String> {
        use std::os::unix::io::{FromRawFd, OwnedFd};
        let name = std::ffi::CString::new("litebox-wayland-drm-client").unwrap();
        let ret = unsafe { libc::syscall(libc::SYS_memfd_create, name.as_ptr(), 1u32) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(ret as i32) })
    }

    let socket_path = "/tmp/litebox-wayland-drm-0";
    let mut stream = None;
    for _ in 0..80 {
        match UnixStream::connect(socket_path) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
    let Some(stream) = stream else {
        println!("CONNECT_FAILED");
        flush_stdout();
        return;
    };
    println!("CONNECTED");
    flush_stdout();

    let conn = Connection::from_socket(stream).expect("Connection::from_socket");
    let mut event_queue: EventQueue<AppState> = conn.new_event_queue();
    let qh = event_queue.handle();
    let display = conn.display();
    let _registry = display.get_registry(&qh, ());
    let mut state = AppState { compositor: None, shm: None };

    event_queue.roundtrip(&mut state).expect("roundtrip: registry");
    println!("ROUNDTRIP_1_DONE");
    flush_stdout();

    let Some(compositor) = state.compositor.clone() else {
        println!("NO_COMPOSITOR_GLOBAL");
        flush_stdout();
        return;
    };
    let Some(shm) = state.shm.clone() else {
        println!("NO_SHM_GLOBAL");
        flush_stdout();
        return;
    };

    // Distinctive, DRM-pipeline-specific pixel pattern (different from `combined.rs`'s own test
    // pattern) so this probe's own live output is unambiguously traceable end to end.
    let (width, height) = (4i32, 4i32);
    let stride = width * 4;
    let size = (stride * height) as usize;

    let memfd = rustix_memfd().unwrap_or_else(|e| {
        println!("MEMFD_FAILED {e}");
        flush_stdout();
        std::process::exit(1);
    });
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = std::fs::File::from(memfd.try_clone().expect("clone memfd"));
        file.set_len(size as u64).expect("set_len");
        file.seek(SeekFrom::Start(0)).expect("seek");
        let pixel: [u8; 4] = [0x11, 0x22, 0x33, 0x44];
        for _ in 0..(width * height) {
            file.write_all(&pixel).expect("write pixel");
        }
    }

    let pool = shm.create_pool(memfd.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(0, width, height, stride, wl_shm::Format::Xrgb8888, &qh, ());
    let surface = compositor.create_surface(&qh, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, width, height);
    surface.commit();
    println!("COMMITTED width={width} height={height} stride={stride} size={size}");
    flush_stdout();

    event_queue.roundtrip(&mut state).ok();
    println!("DONE");
    flush_stdout();
}

fn main() {
    println!("COMBINED_DRM_START");
    flush_stdout();

    let (tx, rx) = mpsc::channel();
    let compositor_thread = std::thread::spawn(move || run_compositor(tx));

    std::thread::sleep(std::time::Duration::from_millis(300));
    run_client();

    match rx.recv_timeout(std::time::Duration::from_secs(15)) {
        Ok((bytes, w, h, stride, pushed)) => {
            println!("RESULT_OK bytes={bytes} width={w} height={h} stride={stride} drm_pushed={pushed}");
        }
        Err(e) => {
            println!("RESULT_TIMEOUT {e:?}");
        }
    }
    flush_stdout();

    let _ = compositor_thread.join();
    println!("COMBINED_DRM_DONE");
    flush_stdout();
}
