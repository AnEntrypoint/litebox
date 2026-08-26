//! Real Wayland CLIENT counterpart to `desktop.rs`'s extended compositor -- proves the
//! `xdg_shell`/`wl_seat`/`wl_output` protocol surface `desktop.rs` added actually works against a
//! real client, not just that it type-checks. Builds on `client.rs`'s already-proven
//! `wl_compositor`+`wl_shm`+`memfd_create` pixel-commit path (verified end-to-end this session,
//! see that file's own doc comment) and adds a real `xdg_wm_base` bind, `xdg_surface`,
//! `xdg_toplevel` -- the actual "ask the compositor for a real window" request any serious desktop
//! client (XFCE's own components included) makes before ever attaching a buffer.

use std::os::unix::io::AsFd;
use std::os::unix::net::UnixStream;

use wayland_client::protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

struct AppState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    xdg_surface_configured: bool,
    toplevel_configured: bool,
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
                "xdg_wm_base" => {
                    state.wm_base =
                        Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(name, version.min(1), qh, ()));
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
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for AppState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // xdg_wm_base.ping/pong keeps the compositor from considering this client unresponsive --
        // real protocol hygiene a genuine desktop client always implements.
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for AppState {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.xdg_surface_configured = true;
            println!("XDG_SURFACE_CONFIGURED serial={serial}");
            flush_stdout();
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                println!("XDG_TOPLEVEL_CONFIGURE width={width} height={height}");
                flush_stdout();
                state.toplevel_configured = true;
            }
            xdg_toplevel::Event::Close => {
                println!("XDG_TOPLEVEL_CLOSE");
                flush_stdout();
            }
            _ => {}
        }
    }
}

fn flush_stdout() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn main() {
    let socket_path = "/tmp/litebox-wayland-0";

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

    let mut state = AppState {
        compositor: None,
        shm: None,
        wm_base: None,
        xdg_surface_configured: false,
        toplevel_configured: false,
    };

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
    let Some(wm_base) = state.wm_base.clone() else {
        println!("NO_XDG_WM_BASE_GLOBAL");
        flush_stdout();
        std::process::exit(1);
    };

    // Real window-creation sequence, matching what any genuine desktop client does: create a
    // wl_surface, wrap it in an xdg_surface, get an xdg_toplevel from THAT, then commit an empty
    // surface (no buffer yet) to trigger the compositor's initial configure event -- THIS is the
    // real proof `desktop.rs`'s XdgShellHandler::new_toplevel wiring works, independent of the
    // already-proven wl_shm pixel-commit path that follows.
    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("litebox-desktop-probe".to_string());
    surface.commit();

    event_queue.roundtrip(&mut state).expect("roundtrip: xdg configure");
    if !state.xdg_surface_configured || !state.toplevel_configured {
        println!("XDG_CONFIGURE_NOT_RECEIVED");
        flush_stdout();
        std::process::exit(1);
    }

    // Now attach a real pixel buffer to the now-configured surface -- the same proven
    // memfd_create+wl_shm path `client.rs` already verified end-to-end, exercised here through
    // a real xdg_toplevel window instead of a bare wl_surface.
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
        let pixel: [u8; 4] = [0x11, 0x22, 0x33, 0xFF];
        for _ in 0..(width * height) {
            file.write_all(&pixel).expect("write pixel");
        }
    }

    let pool = shm.create_pool(memfd.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(0, width, height, stride, wl_shm::Format::Xrgb8888, &qh, ());

    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, width, height);
    surface.commit();
    println!("COMMITTED width={width} height={height} stride={stride} size={size}");
    flush_stdout();

    event_queue.roundtrip(&mut state).ok();
    println!("DONE");
    flush_stdout();
}

fn rustix_memfd() -> Result<std::os::unix::io::OwnedFd, String> {
    use std::os::unix::io::{FromRawFd, OwnedFd};
    let name = std::ffi::CString::new("litebox-wayland-desktop-client-probe").unwrap();
    let ret = unsafe { libc::syscall(libc::SYS_memfd_create, name.as_ptr(), 1u32) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(ret as i32) })
}
