//! Runs the compositor (`main.rs`'s logic) on a background thread and the client (`client.rs`'s
//! logic) on the main thread, both inside ONE process -- avoiding `fork()`+`execv()` entirely.
//!
//! A `fork()`+`execv()`-based launcher was tried first (spawn the compositor and client as two
//! separate guest processes sharing this guest's filesystem/socket namespace, since litebox's
//! runner only starts one top-level guest program) -- every forked child crashed with SIGSEGV
//! immediately after `execv()`, confirmed via a minimal isolated repro with NO Wayland/Smithay
//! code at all: a trivial `fork()`+`execv()`+`waitpid()` sequence dies the exact same way
//! (`waitpid`'s status decodes to signal 11), with both real `fork()` and `vfork()`. This matches
//! PRD row `fork-execve-mallocng-null-meta-crash`, litebox's own deepest, most extensively
//! multi-session-investigated open bug (a musl mallocng null-pointer-deref on `fork()`+
//! `execve()`), previously characterized around a CPython repro -- this is new evidence it's a
//! genuinely general `fork`+`exec` pattern, not CPython/mallocng-specific in the narrow sense. Not
//! something to attempt fixing within this probe's scope. A single-process, multi-THREAD design
//! sidesteps it entirely: no fork, no execve, just `std::thread::spawn`, already proven safe and
//! working elsewhere in this project (e.g. `--gui`'s own presenter thread).
//!
//! **Verification status (updated, see "phase 8" in `README.md` for the full story)**: run as a
//! real guest process, the client genuinely `CONNECTED` and the compositor genuinely printed
//! `CLIENT_ACCEPTED` -- the raw Unix-socket handshake works. `sendmsg`'s `SCM_RIGHTS` gap (real,
//! previously found here) is now fixed in litebox itself (`litebox_shim_linux::syscalls::net`,
//! commit `1a2470c4`). What looked like a SEPARATE, deeper litebox epoll bug after that fix
//! landed ("the outer epoll's readiness for the nested compositor epoll is checked once, then
//! never again, even though `calloop` keeps calling `dispatch()`") turned out NOT to be a litebox
//! bug at all: this file's own `display` source callback called `dispatch_clients` but never
//! `flush_clients` -- `dispatch_clients` only processes requests already read off the wire, it
//! never itself writes the server's own queued REPLIES back out. The client's `roundtrip()`
//! legitimately blocked forever on bytes the server had silently buffered and never sent; once
//! the compositor's own 20s timeout elapsed and its thread exited, the client saw a genuine
//! `Broken pipe`. Confirmed live via a temporary diagnostic (fully reverted): the display source
//! fired exactly once (`dispatch_clients` returned `Ok(2)`), never again across ~191 further
//! `dispatch()` calls over 20s -- adding `flush_clients()` after `dispatch_clients()` resolved
//! this completely; the client now reaches `ROUNDTRIP_1_DONE` and the three real globals
//! (`wl_compositor`/`wl_subcompositor`/`wl_shm`) are received correctly.
//!
//! **Current real blocker** (confirmed live, a genuinely NEW gap, not a re-tread): `memfd_create`
//! (via raw `SYS_memfd_create`, `wl_shm.create_pool`'s own buffer-backing mechanism) is not
//! implemented in litebox at all -- `MEMFD_FAILED Function not implemented (os error 38)`. This
//! is real, separate future work (a whole syscall implementation, not a quick patch layered onto
//! everything else this row has already covered) -- not attempted here.
//!
//! This file deliberately omits the DRM dumb-buffer push (`main.rs`'s `push_to_drm_dumb_buffer`,
//! unreachable here since no commit is ever received) to keep this specific repro minimal and
//! focused on the client-connect/protocol-roundtrip question.

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

struct Compositor {
    compositor_state: CompositorState,
    shm_state: ShmState,
    // Kept alive (open device fd) but not read: this minimal repro's `commit` handler reports a
    // client's shm buffer directly rather than pushing through `main.rs`'s DRM dumb-buffer path
    // (unreachable here, see this file's own doc comment), but the fd must still outlive the
    // event loop, so the field stays even though nothing reads it back.
    #[allow(dead_code)]
    drm: DrmDevice,
    committed_once: bool,
    result_tx: mpsc::Sender<(usize, u32, u32, u32)>,
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
            println!("COMMIT_SHM_OK bytes={} width={} height={} stride={} first4={:02X?}", pixels.len(), width, height, stride, &pixels[..4.min(pixels.len())]);
            flush_stdout();
            let _ = self.result_tx.send((pixels.len(), width as u32, height as u32, stride as u32));
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

fn run_compositor(result_tx: mpsc::Sender<(usize, u32, u32, u32)>) {
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

    let socket_path = "/tmp/litebox-wayland-combined-0";
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
                            if let Err(e) = dh_for_accept.insert_client(stream, std::sync::Arc::new(ClientState::default())) {
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
                    // Real fix (was the actual "readiness never re-checked" root cause, see this
                    // file's own doc comment): `dispatch_clients` only processes requests already
                    // read off the socket -- it never itself writes queued REPLIES back out.
                    // Without this call, the server silently buffers its own responses forever;
                    // the client's `roundtrip()` blocks on bytes that were never sent, and once
                    // the compositor's own timeout elapses and the thread exits, the client sees a
                    // `Broken pipe`. This looked exactly like a litebox epoll bug (readiness
                    // checked once then never again) because the SYMPTOM was identical -- the
                    // client legitimately has nothing more to receive, so nothing on litebox's own
                    // socket-readiness side was ever actually wrong.
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
        let name = std::ffi::CString::new("litebox-wayland-combined-client").unwrap();
        let ret = unsafe { libc::syscall(libc::SYS_memfd_create, name.as_ptr(), 1u32) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(ret as i32) })
    }

    let socket_path = "/tmp/litebox-wayland-combined-0";
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
        let pixel: [u8; 4] = [0xDD, 0xCC, 0xBB, 0xAA];
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
    println!("COMBINED_START");
    flush_stdout();

    let (tx, rx) = mpsc::channel();
    let compositor_thread = std::thread::spawn(move || run_compositor(tx));

    // Real settle delay before the client attempts its first connect -- the client's own
    // bounded retry loop covers a compositor that isn't ready yet either way.
    std::thread::sleep(std::time::Duration::from_millis(300));
    run_client();

    match rx.recv_timeout(std::time::Duration::from_secs(15)) {
        Ok((bytes, w, h, stride)) => {
            println!("RESULT_OK bytes={bytes} width={w} height={h} stride={stride}");
        }
        Err(e) => {
            println!("RESULT_TIMEOUT {e:?}");
        }
    }
    flush_stdout();

    let _ = compositor_thread.join();
    println!("COMBINED_DONE");
    flush_stdout();
}
