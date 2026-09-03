//! A real, minimal Wayland CLIENT counterpart to `main.rs`'s compositor -- closes the last
//! verification gap for `gui-wayland-compositor-on-drm-future`: the compositor itself was already
//! proven to run cleanly (nested-epoll fix, commit `620907c`), but nothing in this environment had
//! ever actually connected to it and committed a real buffer.
//!
//! Uses the real `wayland-client` crate (the client-side counterpart to the `wayland-server`
//! Smithay's `wayland_frontend` feature already pulls in and proved musl-compiles/links) -- not a
//! hand-rolled protocol simulation. Connects to the compositor's fixed socket path
//! (`/tmp/litebox-wayland-0`, see `main.rs`), binds `wl_compositor`+`wl_shm`, creates a surface,
//! allocates an anonymous-memfd-backed shm pool, attaches a 4x4 XRGB8888 buffer, and commits --
//! the smallest real client action that exercises `Compositor::commit`'s full pixel-copy path.
//!
//! **Verification status**: compiles and links cleanly for `x86_64-unknown-linux-musl` (same zig
//! recipe as the compositor). Run against a real compositor via `src/combined.rs` (single-process,
//! no `fork()`/`execv()` -- see that file's own doc comment for why a fork-based launcher was
//! tried first and abandoned), it genuinely `CONNECTED` and the compositor genuinely printed
//! `CLIENT_ACCEPTED` -- the raw Unix-socket handshake works. It then hit a real, precisely
//! identified litebox gap on its first real protocol exchange: `sendmsg`'s ancillary-data
//! (`SCM_RIGHTS`, needed for `wl_shm.create_pool`'s fd-passing) path is unconditionally rejected
//! with `EINVAL` (`litebox_shim_linux/src/syscalls/net.rs`'s `do_sendmsg`/`do_recvmsg`, both
//! explicit `if msg_controllen != 0 { ... return Err(Errno::EINVAL) }`) -- see `README.md`'s
//! "Phase 4" section for the full finding and the precise scope of what a real fix needs.

use std::os::unix::io::AsFd;
use std::os::unix::net::UnixStream;

use wayland_client::protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

struct AppState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            println!("GLOBAL name={name} interface={interface} version={version}");
            flush_stdout();
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor =
                        Some(registry.bind::<wl_compositor::WlCompositor, _, _>(name, version.min(4), qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, version.min(1), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for AppState {
    fn event(_: &mut Self, _: &wl_compositor::WlCompositor, _: wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_shm::WlShm, ()> for AppState {
    fn event(_: &mut Self, _: &wl_shm::WlShm, event: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_shm::Event::Format { format } = event {
            println!("SHM_FORMAT {format:?}");
        }
    }
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

/// `println!`'s stdout is fully buffered (not line-buffered) whenever it isn't a real TTY --
/// which litebox's guest stdout, piped through the runner, is not. Without an explicit flush
/// after every print, this process's real, already-happened progress (CONNECTED, COMMITTED,
/// etc) stays invisible in the parent launcher's/host's captured output until process exit,
/// making a genuinely-working client look hung. Every `println!` below is followed by this.
fn flush_stdout() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn main() {
    let socket_path = "/tmp/litebox-wayland-0";

    // Real retry loop: the client and compositor are two independent guest processes (see
    // README for how this probe runs them) -- give the compositor's listener a moment to bind
    // if the client happens to start first, matching how a real Wayland client's WAYLAND_DISPLAY
    // connect would behave if launched racing a compositor's own startup.
    let mut stream = None;
    for _ in 0..50 {
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
        std::process::exit(1);
    };
    println!("CONNECTED");
    flush_stdout();

    let conn = Connection::from_socket(stream).expect("Connection::from_socket");
    let mut event_queue: EventQueue<AppState> = conn.new_event_queue();
    let qh = event_queue.handle();
    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = AppState { compositor: None, shm: None };

    // Round-trip to receive the registry's global advertisements (real wl_display.sync
    // semantics, not a fixed sleep).
    event_queue.roundtrip(&mut state).expect("roundtrip: registry");
    println!("ROUNDTRIP_1_DONE");
    flush_stdout();

    let Some(compositor) = state.compositor.clone() else {
        println!("NO_COMPOSITOR_GLOBAL");
        flush_stdout();
        std::process::exit(1);
    };
    let Some(shm) = state.shm.clone() else {
        println!("NO_SHM_GLOBAL");
        flush_stdout();
        std::process::exit(1);
    };

    // Smallest real buffer: 4x4 XRGB8888, matching the format DrmSubsystem's dumb buffers use.
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
        // Solid color: 0xAABBCCDD repeated -- real, checkable pixel content, not zeros.
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

    // Flush the commit request to the wire, then a final roundtrip so the compositor has a
    // chance to process it and reply (e.g. a wl_buffer.release, if it sends one) before this
    // client exits -- real protocol hygiene, not just "hope the bytes got there".
    event_queue.roundtrip(&mut state).ok();
    println!("DONE");
    flush_stdout();
}

/// A real anonymous, unlinked, shrinkable-to-size shared-memory fd -- exactly what a real
/// Wayland client uses for `wl_shm_pool`, matching `memfd_create(2)`'s actual contract (no
/// tmpfs path, no cleanup needed, closed-on-drop is sufficient). `rustix` isn't already a direct
/// dependency here, so this calls the raw syscall via `libc` instead (already transitively
/// available through `wayland-client`'s own `rustix` dependency at the FFI level, but a direct,
/// explicit raw syscall keeps this file's own dependency footprint minimal and auditable).
fn rustix_memfd() -> Result<std::os::unix::io::OwnedFd, String> {
    use std::os::unix::io::{FromRawFd, OwnedFd};
    // MFD_CLOEXEC = 1. Name is cosmetic (shows up in /proc/self/fd, not otherwise consulted).
    let name = std::ffi::CString::new("litebox-wayland-client-probe").unwrap();
    let ret = unsafe { libc::syscall(libc::SYS_memfd_create, name.as_ptr(), 1u32) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(ret as i32) })
}
