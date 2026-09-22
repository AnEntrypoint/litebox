// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Restrict this crate to only work on Windows. For now, we are restricting this to only x86-64
// Windows, but we _may_ allow for more in the future, if we find it useful to do so.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

extern crate alloc;

pub mod control_server;
pub mod session_cli;

/// The standard Linux executable search path, prepended to a forwarded `PATH` in `main` below.
/// Without this, `--forward-env` handing the guest a purely Windows-flavored PATH means the
/// guest's own `/bin`, `/usr/bin`, etc. are never searched at all -- confirmed live: a top-level
/// program given as an absolute path (e.g. `/usr/bin/npm`) runs fine, but that same program's OWN
/// internal PATH-relative lookups (a shell script's `npx`, `npx`'s own `cowsay`) fail with `not
/// found` even though the binaries are genuinely present, because nothing in the guest's PATH
/// ever pointed at the directories they live in.
const LINUX_DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Stack size for the OS thread that runs the FIRST guest program's initial (`execve`-time)
/// thread. Must match `litebox_platform_windows_userland`'s own `GUEST_THREAD_STACK_SIZE` (the
/// size every LATER `clone()`-spawned guest thread already gets via
/// `std::thread::Builder::stack_size`, per that constant's doc comment: "Guest code... runs
/// directly on this real Windows thread's own stack -- there is no separate emulated guest-stack
/// region", so host-side call frames incurred while emulating the guest -- not just the guest's
/// own `rsp`-addressed memory, which is separately and correctly sized by
/// `litebox_shim_linux::loader::DEFAULT_STACK_SIZE` -- share this same real, native stack).
///
/// Without this, the very first guest program's initial thread ran inline on whatever OS thread
/// called [`run`] below -- for a normal (non-`--session-daemon`) invocation, that is this
/// process's own main thread, whose real stack is Rust's ~1 MiB Windows default, not 8 MiB.
/// Confirmed live: `weston --backend=drm-backend.so --use-pixman` crashes with a genuine host
/// `STATUS_STACK_OVERFLOW` (Rust's own "thread '<unknown>' has overflowed its stack" message)
/// immediately after selecting its Pixman renderer, on the main thread specifically -- zero
/// `clone()` syscalls occur before the crash, ruling out the already-correctly-sized spawned-
/// thread path entirely. This mirrors an identical, already-fixed bug in this exact crate for the
/// `--gui` presenter thread (see `PRESENTER_THREAD_STACK_SIZE` below) -- winit/wgpu's stack-hungry
/// call chains overflowed that thread's default 1 MiB budget the same way pixman's own stack-
/// hungry initialization overflows this one.
const INITIAL_GUEST_THREAD_STACK_SIZE: usize = 32 * 1024 * 1024;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use litebox_platform_windows_userland::WindowsUserland as Platform;
use memmap2::Mmap;
use std::path::{Path, PathBuf};

/// `--gui`'s value, per `docs/presenter-process-design.md` section 4.1.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuiMode {
    /// `--gui` (no value, via `default_missing_value`) or `--gui=shown`: spawn the presenter and
    /// show its window immediately.
    Shown,
    /// `--gui=hidden`: spawn the presenter at startup but leave its window not visible.
    Hidden,
}

/// Run Linux programs with LiteBox on unmodified Windows.
///
/// The program binary and all its dependencies must be provided inside a tar
/// archive via `--initial-files`. The program path refers to a path inside the
/// tar archive.
#[derive(Parser, Debug)]
pub struct CliArgs {
    /// The program and arguments passed to it (e.g., `/bin/ls --color`).
    ///
    /// The program path refers to a path inside the tar archive provided via
    /// `--initial-files`. All binaries must be pre-rewritten with the syscall
    /// rewriter.
    #[arg(required = true, trailing_var_arg = true, value_hint = clap::ValueHint::CommandWithArguments)]
    pub program_and_arguments: Vec<String>,
    /// Environment variables passed to the program (`K=V` pairs; can be invoked multiple times)
    #[arg(long = "env")]
    pub environment_variables: Vec<String>,
    /// Forward the existing environment variables
    #[arg(long = "forward-env")]
    pub forward_environment_variables: bool,
    /// Allow using unstable options
    #[arg(short = 'Z', long = "unstable")]
    pub unstable: bool,
    /// Tar archive containing the program and its shared libraries.
    ///
    /// All ELF binaries should be pre-rewritten with the syscall rewriter
    /// (e.g., via `litebox-packager`).
    #[arg(
        long = "initial-files",
        value_name = "PATH_TO_TAR",
        value_hint = clap::ValueHint::FilePath,
        required_unless_present = "oci_image",
        conflicts_with = "oci_image"
    )]
    pub initial_files: Option<PathBuf>,

    /// Pull an OCI container image reference directly at boot time and use it as the rootfs,
    /// instead of a pre-built `--initial-files` tar. NO real host directory is ever created for
    /// the image's rootfs: layers are pulled into memory, merged (OCI whiteout-aware) and
    /// syscall-rewritten entirely in memory, and mounted straight into the guest's read-only tar
    /// filesystem backend -- see `litebox::fs::tar_ro::TarRo::from_layers` and
    /// `litebox_packager::oci::pull_layers_in_memory`. This replaces the ahead-of-time
    /// `litebox-packager --oci-image` + `--initial-files` two-step pipeline for the common case;
    /// that pipeline still works unchanged for callers that want a pre-built, reusable tar file.
    /// Only public (anonymous) registries are currently supported.
    #[arg(long = "oci-image", value_name = "IMAGE_REF", conflicts_with = "initial_files")]
    pub oci_image: Option<String>,
    /// After the program exits, export the writable upper layer (every file the guest created or
    /// modified during this run) to a tar archive at this path, so a later run can resume from it
    /// via `--resume-from`.
    #[arg(long = "export-writable-layer", value_name = "PATH_TO_TAR", value_hint = clap::ValueHint::FilePath)]
    pub export_writable_layer: Option<PathBuf>,
    /// Seed the writable upper layer from a tar archive previously produced by
    /// `--export-writable-layer`, resuming a prior session's on-disk state instead of starting
    /// from an empty upper layer.
    #[arg(long = "resume-from", value_name = "PATH_TO_TAR", value_hint = clap::ValueHint::FilePath)]
    pub resume_from: Option<PathBuf>,
    /// Attach the launched program to a fresh pty as its controlling terminal (mirroring glibc's
    /// `login_tty()`: `setsid()` + `TIOCSCTTY` + `dup2` onto fds 0/1/2), and forward the pty
    /// master's byte stream to/from this process's own real stdout/stdin.
    ///
    /// This is the session-daemon feature's guest-side half (see
    /// `docs/session-daemon-design.md`): unlike plain stdio (which routes straight through to
    /// this process's real Windows stdio handles, and thus is not a real tty when the daemon
    /// spawns this process with piped, non-console stdio), a pty gives the guest program raw-mode
    /// terminal semantics (`TCSETS`/`ioctl` succeed, matching a real Linux pty) regardless of
    /// whether this process's own stdio is a console. The session daemon spawns this process with
    /// `--pty-mode` and reads its stdout / writes its stdin exactly as if they were a pty master's
    /// byte stream.
    #[arg(long = "pty-mode")]
    pub pty_mode: bool,

    /// Spawn `litebox-presenter.exe` (a separate process -- see
    /// `docs/presenter-process-design.md`) and open a real host window displaying the guest's
    /// `/dev/dri/card0` DRM output in it. Opt-in, since most invocations (scripted CLI usage, the
    /// common case this runner otherwise serves) have no GUI content to show and should never
    /// have a window pop up unexpectedly.
    ///
    /// Bare `--gui` shows the window immediately at startup (`GuiMode::Shown`, the
    /// `default_missing_value` below). `--gui=hidden` spawns the presenter process (so a LATER
    /// `show` control-channel call is fast, no cold-start wgpu/window-creation cost) but leaves
    /// its window not visible -- matching `docs/presenter-process-design.md` section 4.1 exactly.
    /// This exists because a guest's GUI must not depend on a window existing: a desktop session
    /// can boot, render, and be captured (`LITEBOX_DUMP_FRAMES`, or a `screenshot` control-channel
    /// call) headlessly, then be revealed later -- headless and headed become the same running
    /// system observed differently, rather than two modes chosen before the guest starts.
    ///
    /// No `--gui` at all is headless exactly as today: `litebox-presenter.exe` is never spawned,
    /// though the control channel still starts and answers `screenshot`/`ps`/`strace`/`frames`/
    /// `key`/`rel` with no window -- only an explicit `show` (from a caller, or this flag) ever
    /// spawns one.
    #[arg(
        long = "gui",
        value_enum,
        num_args = 0..=1,
        default_missing_value = "shown"
    )]
    pub gui: Option<GuiMode>,

    /// Deprecated spelling of `--gui=hidden`, kept so existing scripts using this flag keep
    /// working unchanged. Prefer `--gui=hidden`.
    #[arg(long = "gui-hidden", hide = true)]
    pub gui_hidden: bool,

    /// Publish a port from the guest to the host, `host_port:guest_port` (mirrors `docker run
    /// -p`). Can be given multiple times. Binds a real `127.0.0.1:<host_port>` listener on the
    /// host and forwards every accepted connection into the guest's virtual network at
    /// `<guest_port>` -- the inbound counterpart to this runner's existing transparent *outbound*
    /// NAT (guest-initiated `curl`/`apk`/etc already just work; a guest-run **server**, e.g. a web
    /// UI, needs this explicit opt-in instead, exactly like a NAT gateway with no configured
    /// port-forwarding rule cannot otherwise be reached from outside).
    ///
    /// A bare `port` is shorthand for `port:port`. Implemented via
    /// `litebox_platform_windows_userland`'s `net` module (see its module doc comment for the
    /// full inbound-forwarding design); threaded down via the `LITEBOX_PUBLISH` environment
    /// variable the gateway already reads (`host:guest` pairs, comma-separated), so this flag is
    /// just this runner's user-facing surface for that same mechanism.
    #[arg(long = "publish", short = 'p', value_name = "HOST_PORT:GUEST_PORT")]
    pub publish: Vec<String>,
}

struct MmappedFile {
    data: &'static [u8],
    abs_path: PathBuf,
}

/// Best-effort extraction of a human-readable message from a caught panic payload (a bare
/// `Box<dyn Any + Send>` as handed back by `std::panic::catch_unwind`), for logging -- used by
/// the `net_worker` loops' own panic recovery (see their doc comments for why a panic there must
/// be caught rather than allowed to kill the thread).
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Memory-maps `path` read-only instead of copying its bytes into a private heap
/// buffer, so the OS page cache transparently shares the physical pages across
/// every concurrent runner process reading the same rootfs archive -- mirrors
/// `litebox_runner_linux_userland`'s identical helper.
fn mmapped_file(path: impl AsRef<Path>) -> Result<MmappedFile> {
    let path = path.as_ref();
    let abs_path = std::path::absolute(path)
        .map_err(|e| anyhow!("Could not get absolute path for {}: {}", path.display(), e))?;
    let file = std::fs::File::open(&abs_path)?;
    let data = {
        // SAFETY: We assume that the file given to us is not going to change _externally_ while in
        // the middle of execution. Since we are mapping it as read-only and mapping it only once,
        // we are not planning to change it either. With both these in mind, this call is safe.
        //
        // We need to leak the `Mmap` object, so that it stays alive until the end of the program,
        // rather than being unmapped at function finish (i.e., to get the `'static` lifetime).
        Box::leak(Box::new(unsafe { Mmap::map(&file) }.map_err(|e| {
            anyhow!("Could not read tar file at {}: {}", path.display(), e)
        })?))
    };
    Ok(MmappedFile { data, abs_path })
}

/// Set up the root-owned identity and FHS scaffolding every fresh in-mem upper layer needs before
/// it can back a real guest process's filesystem -- shared by `run()`'s own bootstrap and the
/// `LITEBOX_PROCESS_FORK=1` cross-process child's `diag_process_fork_globalstate_probe`
/// filesystem reconstruction, so the two never drift apart (see `layered.rs:243`'s
/// `unimplemented!` and this function's own inline comments for why each step here matters).
fn initialize_root_in_mem_layer<Platform: litebox::sync::RawSyncPrimitivesProvider>(
    in_mem: &mut litebox::fs::in_mem::FileSystem<Platform>,
) {
    // The guest's persistent identity is root, matching `Platform::init_task`'s credentials (for
    // `run()`) or `task_params`'s credentials (for the process-fork child's adopted `Task`) and
    // matching how a real container's initial process runs (a fresh OCI rootfs's `/`, `/etc`,
    // `/lib`, etc. are root-owned at mode 0755, not world-writable). Without this, `getuid()`
    // would report root while the file system's own permission checks still enforced a
    // mismatched non-root identity, breaking any program (e.g. `apk`) that needs to write into
    // the rootfs's root-owned directories -- for the process-fork child specifically, this
    // mismatch is what previously hit `unimplemented!("{e} when setting up ancestor dirs")` at
    // `litebox/src/fs/layered.rs:243` (a `MkdirError::NoWritePerms` this in-mem layer's own
    // `mkdir` returns once `apk`'s file-migration-from-the-read-only-tar-layer path reaches a
    // root-owned ancestor directory).
    in_mem.set_default_user(0, 0);
    in_mem.with_root_privileges(|fs| {
        use litebox::fs::FileSystem as _;
        fs.mkdir(
            "/tmp",
            litebox::fs::Mode::RWXU | litebox::fs::Mode::RWXG | litebox::fs::Mode::RWXO,
        )
        .unwrap();
        fs.chown("/tmp", Some(1000), Some(1000)).unwrap();

        // `/dev/shm` on real Linux is its own tmpfs mount, not part of devtmpfs (the fixed,
        // read-only-shaped `{stdin,stdout,null,urandom,...}` set `litebox::fs::devices::Devices`
        // provides at `/dev` -- see that module's own doc comment): a plain writable directory
        // whose files are always real shared memory, which is exactly what a `/tmp`-shaped
        // in-mem directory already gives every OTHER file created under it except for the
        // `MAP_SHARED|PROT_WRITE` real-backing part (see `syscalls::file::MemfdMarker` and
        // `syscalls::mm::try_memfd_mmap`'s own doc comments for why an ordinary in-mem file can't
        // support that directly). Mode 1777 (world-writable + sticky bit) matches real Linux's
        // `/dev/shm` exactly -- multiple unrelated users/processes must be able to create files
        // here, but only the owner of a given file (or root) may unlink someone else's. Without
        // this directory existing at all, glibc's `shm_open("/name", ...)` (which opens
        // `/dev/shm/name` under the hood -- there is no real `shm_open` syscall) fails at the
        // very first `open()` with `ENOENT`, before ever reaching the `MAP_SHARED` gap: confirmed
        // via `advisor/probes/shm_probe.c`, which reproduced exactly this `ENOENT` against the
        // canonical XFCE layer (no `/dev/shm` tar entry, no synthesized directory here either) --
        // this is the blocker AGENTS.md documents as labwc's shm-keymap-allocation crash under
        // the stock `linuxserver/webtop:alpine-mate` image's real Wayland/DRM (labwc) path.
        //
        // `/dev` itself must exist as a real ancestor directory IN THIS SAME in-mem layer before
        // `/dev/shm` can be created under it -- the `/dev` a guest normally sees is synthesized
        // entirely by the separate `Devices` composer mount in `default_fs` below (this
        // function's own in-mem layer knows nothing about that), so without this the `mkdir`
        // below panics with `PathError::MissingComponent` (confirmed live: first attempt at this
        // fix, before adding this `mkdir("/dev", ...)`, crashed exactly this way). Mode 0755
        // root-owned matches real Linux's own `/dev`.
        fs.mkdir("/dev", litebox::fs::Mode::RWXU | litebox::fs::Mode::RGRP | litebox::fs::Mode::ROTH)
            .unwrap();
        fs.mkdir(
            "/dev/shm",
            litebox::fs::Mode::RWXU
                | litebox::fs::Mode::RWXG
                | litebox::fs::Mode::RWXO
                | litebox::fs::Mode::SVTX,
        )
        .unwrap();

        // Standard FHS directories that tools like `apk` expect to already exist
        // (e.g. `apk` opens a log file under `/var/log`) but which don't survive
        // as empty-directory entries when an OCI image's rootfs is scanned into a
        // file-based tar (an empty directory has no file contents, so it produces
        // no tar entry, and `TarRo`'s directory tree is inferred purely from file
        // paths -- see litebox/src/fs/tar_ro.rs).
        // `/var/lib` and `/var/lib/xkb` are added to this same list for the identical reason:
        // `/var/lib/xkb` exists in the read-only tar layer (it ships a `README.compiled`), but a
        // NEW file inside a directory that exists ONLY in the read-only layer has nowhere to
        // land -- `TarRo::open_file_at` (litebox/src/fs/tar_ro.rs) refuses a writable open of a
        // tar-layer directory, so `xkbcomp` (spawned by `Xorg` to compile the keyboard keymap)
        // fails to create `/var/lib/xkb/server-0.xkm`, which `Xorg` treats as fatal ("Failed to
        // activate virtual core keyboard"). Confirmed live: this was the concrete blocker after
        // the DRM_CAP_CURSOR_WIDTH/HEIGHT and legacy ADDFB fixes let `Xorg` boot against
        // `linuxserver/webtop:debian-xfce`. `/var/lib` must precede `/var/lib/xkb` in this list
        // (same ancestor-ordering requirement as `/dev` before `/dev/shm` above) or `mkdir`
        // panics with `PathError::MissingComponent`.
        for dir in [
            "/run",
            "/var",
            "/var/log",
            "/var/cache",
            "/var/tmp",
            "/var/lib",
            "/var/lib/xkb",
        ] {
            fs.mkdir(
                dir,
                litebox::fs::Mode::RWXU | litebox::fs::Mode::RWXG | litebox::fs::Mode::RWXO,
            )
            .unwrap();
        }

        // A container's `/etc/resolv.conf` normally comes from the *host* runtime at
        // container-start (e.g. Docker bind-mounts the host's own resolver config in), not
        // from the image itself -- a plain OCI rootfs like this one has no such file. Without
        // it, DNS-using tools (`apk`, `wget`, ...) have no configured nameserver at all and
        // fail immediately rather than reaching the network. Point at a public resolver
        // reachable through the platform's NAT gateway, mirroring what a real container
        // runtime would inject.
        //
        // `/etc` itself isn't created here (it comes from the tar layer composed in later),
        // so create it in this in-mem layer too, matching the `/tmp`, `/run`, etc. pattern
        // above.
        fs.mkdir(
            "/etc",
            litebox::fs::Mode::RWXU | litebox::fs::Mode::RGRP | litebox::fs::Mode::ROTH,
        )
        .unwrap();
        let resolv_conf = fs
            .open(
                "/etc/resolv.conf",
                litebox::fs::OFlags::WRONLY | litebox::fs::OFlags::CREAT,
                litebox::fs::Mode::RUSR
                    | litebox::fs::Mode::WUSR
                    | litebox::fs::Mode::RGRP
                    | litebox::fs::Mode::ROTH,
            )
            .unwrap();
        fs.write(
            &resolv_conf,
            b"nameserver 8.8.8.8\nnameserver 1.1.1.1\n",
            None,
        )
        .unwrap();
        fs.close(&resolv_conf).unwrap();
    });
}

/// A `tracing_subscriber` writer that flushes `std::io::stderr()` after every write.
///
/// Without this, `tracing_subscriber::fmt()`'s default writer goes through ordinary
/// `std::io::stderr()`, which Rust's standard library block-buffers whenever stderr is NOT a
/// live console (any redirected file, anonymous pipe, or `.NET`/other process-launcher capture)
/// -- flushed only on process exit, never per line. Confirmed live: a `.NET`
/// `Process`-redirected run received literally zero bytes of litebox's own log output over a full
/// 30 real seconds of active, high-volume (`LITEBOX_LOG=debug`) logging, with the entire log only
/// appearing once the process was killed. This is a SEPARATE code path from
/// `litebox_platform_windows_userland`'s own raw-`WriteFile`-based guest-process stdout (already
/// fixed for exactly this reason) -- that fix never covered litebox's OWN tracing output, which is
/// the vast majority of every diagnostic capture this project's own investigations rely on.
struct FlushingStderr;

impl std::io::Write for FlushingStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = std::io::stderr().write(buf)?;
        std::io::stderr().flush()?;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

/// Run Linux programs with LiteBox on unmodified Windows
///
/// # Panics
///
/// Can panic if any particulars of the environment are not set up as expected. Ideally, would not
/// panic. If it does actually panic, then ping the authors of LiteBox, and likely a better error
/// message could be thrown instead.
/// Held for a `litebox_runner_linux_on_windows_userland` process's entire lifetime to make
/// concurrent boots on this host structurally impossible. See `acquire_boot_lock`'s own doc
/// comment for why this exists, and why liveness is tied to a held OS file handle rather than to
/// this type's `Drop`.
struct BootLock {
    /// The exclusively-held lockfile handle. Windows closes it -- and so releases the lock --
    /// when this process exits by ANY route: a normal `std::process::exit` (which calls
    /// `ExitProcess` and runs no destructors at all), a panic, a hard kill by an external
    /// low-memory watchdog, or an outright crash. That is the entire point: this runner reaches
    /// none of its exits by unwinding, so a `Drop`-based release is unreachable by construction
    /// (see `acquire_boot_lock`).
    ///
    /// `None` only for the `LITEBOX_ALLOW_CONCURRENT_BOOT` escape hatch, which holds no lock.
    _file: Option<std::fs::File>,
}

/// Acquire the single, host-wide boot lock, refusing to proceed if another live boot already
/// holds it.
///
/// The lock IS the lockfile's own open handle, held with a share mode that admits readers but no
/// second writer, so a competing acquirer is refused by the OS with `ERROR_SHARING_VIOLATION`.
/// Liveness is therefore a property the kernel maintains, not one this code has to infer.
///
/// It is deliberately NOT a `Drop` guard plus an mtime heartbeat, which is what this function
/// used to be. That arrangement was broken in two independent ways, both confirmed live rather
/// than reasoned about:
///
/// 1. `run` terminates via `std::process::exit`, which on Windows calls `ExitProcess` directly
///    and runs no destructors (`main`'s own doc comment records this, having confirmed it against
///    this exact binary and toolchain). So `BootLock::drop` never ran on the ordinary success
///    path and every clean run leaked its lockfile -- the previous doc comment's claim that the
///    lock was "dropped, and the lockfile removed, on any exit path via the guard's Drop impl"
///    was simply false.
/// 2. Because the leaked file's heartbeat thread died with the process, the stale-by-age fallback
///    then blocked the NEXT boot for the full staleness window (five minutes) after every
///    successful run. Observed directly: a clean `busybox ls` run exited with status 0 and left
///    behind a lock that refused the very next boot.
///
/// A held handle fixes both at once, and needs no heartbeat thread, no staleness window, and no
/// PID-liveness check -- the previous implementation's doc comment rejected a PID check as
/// needing "a new dependency for a single Win32 API call", but with an OS-held handle that
/// question never has to be asked, so no dependency is needed either.
fn acquire_boot_lock() -> Result<BootLock> {
    use std::os::windows::fs::OpenOptionsExt as _;

    // Explicit, narrow escape hatch for deliberate multi-runner testing (e.g. an Xorg server in
    // one runner and an X client connecting to it via --publish in another -- both need to be
    // their own pid 1, so a single-runner arrangement cannot express this). The caller is
    // responsible for the resulting memory footprint (a full OCI image load is several GB); this
    // does not relax anything else about the lock's own correctness, it just skips acquiring it.
    if std::env::var_os("LITEBOX_ALLOW_CONCURRENT_BOOT").is_some() {
        return Ok(BootLock { _file: None });
    }

    // Resolve against the executable's own directory, not the process's current working
    // directory: the old `Path::new(".litebox-cache")` was cwd-relative, so two runner
    // invocations launched from different directories (or even the same binary invoked via a
    // relative vs. absolute path) each got their own `.litebox-cache/boot.lock` and happily ran
    // concurrently -- confirmed live (2026-09-06): a peer session launched a second runner from a
    // different cwd and had two full boots live simultaneously with zero code change, which is
    // exactly the "concurrent boots produce symptoms indistinguishable from a real hang or crash"
    // scenario this lock exists to prevent. Anchoring to the exe's own directory makes the lock
    // host-wide for any normal invocation of this binary, matching what its own error message
    // already promises.
    let lock_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".litebox-cache");
    std::fs::create_dir_all(&lock_dir)
        .with_context(|| format!("failed to create lock directory {}", lock_dir.display()))?;
    let lock_path = lock_dir.join("boot.lock");

    // Readers admitted, a second writer refused. Sharing READ is what lets the error path below
    // still read the current holder's PID out of the file while the holder keeps the lock.
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    // `ERROR_SHARING_VIOLATION` -- the OS refusing this open precisely because another live
    // process holds the file. This, not a timestamp, is the lock's contention signal.
    const ERROR_SHARING_VIOLATION: i32 = 32;

    // `create(true)` + `truncate(true)`, deliberately not `create_new(true)`: a lockfile left on
    // disk by a previous run is not itself the lock (the handle is), so an unheld leftover file
    // must be reclaimed silently rather than mistaken for contention.
    match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .share_mode(FILE_SHARE_READ)
        .open(&lock_path)
    {
        Ok(mut f) => {
            use std::io::Write as _;
            // Recorded purely so a human -- or the error message below, running in the other
            // process -- can see who holds the lock. Nothing about the lock's correctness
            // depends on this value.
            let _ = writeln!(f, "{}", std::process::id());
            let _ = f.flush();
            Ok(BootLock { _file: Some(f) })
        }
        Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
            let holder = std::fs::read_to_string(&lock_path).unwrap_or_default();
            let holder = holder.trim().to_owned();
            let holder = if holder.is_empty() {
                "unknown".to_owned()
            } else {
                holder
            };
            Err(anyhow!(
                "another litebox_runner_linux_on_windows_userland boot is already in progress \
                 on this host (lock held by pid {holder}, {}) -- concurrent boots silently \
                 starve each other (host memory/CPU contention) and produce symptoms \
                 indistinguishable from a real hang or crash; wait for it to finish. The lock \
                 is the lockfile's open handle, so it is released automatically the moment that \
                 process exits, however it exits -- if this persists, that process is genuinely \
                 still alive, and deleting the file will NOT release the lock",
                lock_path.display(),
            ))
        }
        Err(e) => {
            Err(e).with_context(|| format!("failed to acquire boot lock at {}", lock_path.display()))
        }
    }
}

/// Install the `LITEBOX_LOG` tracing subscriber.
///
/// Idempotent (`try_init`), and public because a cross-process `fork()` child never reaches
/// [`run`] -- it takes `main`'s diagnostic-resume-child branch instead. Without calling this there
/// too, a child produces no log output at all, which is exactly backwards: the child is where the
/// interesting half of a cross-process fork happens, and "no lines from the child" reads as
/// "nothing happened" rather than "logging was never switched on".
/// What the log filter is when `LITEBOX_LOG` is unset.
///
/// `EnvFilter`'s own default (what `from_env_lossy()` gives with no directive supplied) is
/// `ERROR` and nothing else, which silently discarded EVERY `warn!` in the tree -- 111 call sites,
/// including ones deliberately written to report real, silent degradation: `claim_range` giving up
/// a possibly-still-live memory claim when `CLAIMED_RANGES` fills (a hang seconds later was the
/// only evidence it had happened), an `open` refusing an unsupported flag, `insert_mapping`
/// rejecting an out-of-bounds range. A warning nobody can see is not a warning, and this project
/// has repeatedly paid for that: `MAX_CLAIMS`'s own doc comment reconstructs eviction pressure
/// after the fact from hang symptoms, because the event itself was logged below the visible level.
///
/// `fork_verify` is held at `error` here, and only it. Its warnings are genuinely per-instruction
/// -- `on_single_step` runs for every single-stepped instruction during a fork heal and warns on
/// each stale-pointer detection -- so including it would reintroduce exactly the log-volume
/// regression that `MAX_CLAIMS`'s doc comment records breaking a live weston session's timing.
/// `LITEBOX_LOG=litebox_platform_windows_userland::fork_verify=warn` turns it back on when a fork
/// heal is what's being investigated.
const DEFAULT_LOG_FILTER: &str = "warn,litebox_platform_windows_userland::fork_verify=error";

pub fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_writer(|| FlushingStderr)
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .with_level(true)
        .with_env_filter(
            // Read `LITEBOX_LOG` here rather than via `with_env_var`/`from_env_lossy`, because
            // the default is a MULTI-directive filter (see `DEFAULT_LOG_FILTER`) and
            // `with_default_directive` accepts only a single `Directive`. An empty value is
            // treated as unset, so `LITEBOX_LOG=` does not silence everything by accident.
            tracing_subscriber::EnvFilter::builder().parse_lossy(
                std::env::var("LITEBOX_LOG")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_LOG_FILTER.to_owned()),
            ),
        )
        .try_init();
}

pub fn run(cli_args: CliArgs) -> Result<()> {
    // One fixed, well-known tar path for this whole boot tree's shared filesystem continuity --
    // see `process_fork::CONTAINER_FS_SNAPSHOT_ENV_VAR`'s own doc comment for the full "approximates
    // a real container's one shared mount namespace" reasoning. Set ONLY if not already inherited:
    // the FIRST process in the tree establishes it (a real file, genuinely unique to this boot --
    // named from this process's own pid, which cannot collide with a concurrent, unrelated boot's
    // own first process), and `Command`'s default env inheritance carries the SAME value to every
    // fork/exec-collision descendant automatically, so every one of them resolves to the identical
    // path without it ever being re-derived or passed explicitly.
    if std::env::var_os(
        litebox_platform_windows_userland::process_fork::CONTAINER_FS_SNAPSHOT_ENV_VAR,
    )
    .is_none()
    {
        let path = std::env::temp_dir().join(format!(
            "litebox-container-fs-{}.tar",
            std::process::id()
        ));
        unsafe {
            std::env::set_var(
                litebox_platform_windows_userland::process_fork::CONTAINER_FS_SNAPSHOT_ENV_VAR,
                &path,
            );
        }
    }

    // `litebox` is `#![no_std]` and cannot read an environment variable itself, so the runner
    // forwards this one on its behalf, here -- before any guest mapping is placed. It disables the
    // inter-mapping guard gap (see `litebox::mm::linux::MAPPING_GUARD_GAP`) so the gap can be A/B'd
    // on ONE binary, the same way `LITEBOX_NO_PLACEMENT_FLOOR` is handled on the platform side.
    // Comparing two separately-built binaries confounds the measurement with every unrelated
    // difference between them, which has already produced at least one wrong conclusion tonight.
    litebox::mm::linux::set_mapping_guard_gap_disabled(
        std::env::var_os("LITEBOX_NO_MAPPING_GUARD_GAP").is_some(),
    );
    // Same arrangement, for the copy-on-write file-mapping fast path: the shim is `no_std` and
    // cannot read the environment itself. Defaults OFF -- see `set_cow_mmap_enabled`'s own doc
    // comment for the measurement showing the path is both lossy (it silently zero-fills the
    // flanks of a destroyed CoW view, which for a shared library is real content -- it is what
    // makes `import pixelflux` fail to resolve a symbol that is present on disk) and, per
    // AGENTS.md's own closed investigation, of no practical benefit on real images.
    litebox_shim_linux::set_cow_mmap_enabled(std::env::var_os("LITEBOX_COW_MMAP").is_some());
    // Real boot lock, not a remembered rule: two concurrent litebox_runner processes
    // sharing this host silently starve each other (host memory/CPU contention),
    // producing a symptom -- truncated log, no crash, no exit -- that is
    // indistinguishable from a real hang or regression. Every prior session had this
    // as a documented discipline everyone was supposed to remember and check for
    // manually; that discipline was repeatedly violated under pressure across many
    // concurrent sessions/forks this project has run, each time producing a real,
    // costly misdiagnosis (an "intermittent" bug that was actually just contention).
    // A held file lock makes concurrent boots structurally impossible instead of a
    // rule to remember. Held for this process's entire lifetime: the lock is the
    // lockfile's own open OS handle, which Windows closes on every exit path this
    // process actually takes -- including `std::process::exit`/`ExitProcess` and a
    // hard kill, neither of which runs any destructor. See `acquire_boot_lock`.
    //
    // EXCEPT for one deliberate, narrow case: a child spawned by `ForkChildVerificationProvider::
    // spawn_exec_collision_child` (see `process_fork::EXEC_COLLISION_CHILD_ENV_VAR`'s own doc
    // comment) is one synchronous continuation of the SAME boot, not a second, independent one --
    // the parent thread is blocked waiting for it, contending for nothing. Taking the lock there
    // would make every such child hit the still-live parent's own lock and exit immediately with
    // this very lock's "another boot is already in progress" error, confirmed live.
    let _boot_lock = if std::env::var_os(
        litebox_platform_windows_userland::process_fork::EXEC_COLLISION_CHILD_ENV_VAR,
    )
    .is_some()
    {
        None
    } else {
        Some(acquire_boot_lock()?)
    };

    // The shim is `no_std` and cannot read the environment itself, so translate
    // `LITEBOX_DRM_TRACE=1` here. This logs every DRM ioctl at its single dispatch
    // point, which is what answers "is the guest still page-flipping?" -- the
    // question that separates a compositor that stopped presenting from a client
    // presenting an empty buffer.
    litebox_shim_linux::syscalls::set_drm_trace(
        std::env::var_os("LITEBOX_DRM_TRACE").is_some(),
    );

    litebox_shim_linux::syscalls::set_input_trace(
        std::env::var_os("LITEBOX_INPUT_TRACE").is_some(),
    );

    // Same `no_std` reason as `set_drm_trace` above: the shim cannot read the environment, so
    // translate `LITEBOX_NO_DIRTYFB=1` here. Note the sense -- the flag DISABLES DIRTYFB
    // presentation, so an unset environment leaves it ENABLED, which is the intended behaviour.
    // Gating it this way lets one binary be A/B'd with and without DIRTYFB, matching
    // `LITEBOX_NO_PLACEMENT_FLOOR` and `LITEBOX_NO_MAPPING_GUARD_GAP`.
    litebox_shim_linux::syscalls::set_dirty_fb_enabled(
        std::env::var_os("LITEBOX_NO_DIRTYFB").is_none(),
    );

    litebox_platform_windows_userland::install_memcpy_watch_from_env();

    init_logging();

    // Two mutually-exclusive rootfs sources (enforced by clap's `conflicts_with` on both args):
    // a pre-built `--initial-files` tar (host-mmapped, unchanged from before), or a live
    // `--oci-image` reference pulled and merged entirely in memory at this exact point, with NO
    // real host directory ever created for its rootfs -- see `litebox_packager::oci`'s
    // `pull_layers_in_memory`/`rewrite_layer_elfs` and `TarRo::from_layers`. Both paths converge
    // on the same `Cow<'static, [u8]>` layer list before the shared `default_fs`-family call
    // below, so everything downstream of rootfs construction (in-mem upper layer, resume-from
    // import, guest boot) is identical regardless of which source was used.
    enum RootfsSource {
        Tar { mmap: MmappedFile },
        OciLayers {
            layers: Vec<std::borrow::Cow<'static, [u8]>>,
            resolved_layers_json: String,
        },
    }

    let rootfs_source = if let Some(image_ref) = &cli_args.oci_image {
        eprintln!("Pulling OCI image (runtime, in-memory): {image_ref}");
        // `pull_layers_in_memory_with_resolved_digests` pulls, decompresses, AND rewrites each
        // layer's ELFs one layer at a time internally -- never holding more than one layer's
        // raw+rewritten bytes at once. Rewriting again here would be redundant (and
        // re-introduce the same all-layers-at-once memory spike this function was changed to
        // avoid).
        let (pulled, resolved_layers_json) =
            litebox_packager::oci::pull_layers_in_memory_with_resolved_digests(image_ref, true)
                .map_err(|e| anyhow!("failed to pull OCI image {image_ref}: {e}"))?;
        // Carry the REFERENCE across a cross-process `fork()`, the way the `--initial-files` path
        // carries its tar path. A child re-execs with no command line of its own, so without this
        // it arrives with no rootfs source at all and cannot `execve` anything. See
        // `FORK_CHILD_OCI_IMAGE_ENV_VAR` for why the reference is the right thing to hand over
        // rather than a materialised rootfs.
        //
        // Also carry the ALREADY-RESOLVED layer digest list, so every fork child can skip its
        // own manifest fetch entirely -- see `FORK_CHILD_OCI_LAYER_DIGESTS_ENV_VAR`'s doc comment
        // for the measured cost (2.2-3.1s per fork) this removes.
        unsafe {
            std::env::set_var(
                litebox_platform_windows_userland::process_fork::FORK_CHILD_OCI_IMAGE_ENV_VAR,
                image_ref,
            );
            std::env::set_var(
                litebox_platform_windows_userland::process_fork::FORK_CHILD_OCI_LAYER_DIGESTS_ENV_VAR,
                &resolved_layers_json,
            );
        }
        RootfsSource::OciLayers {
            layers: pulled.layers,
            resolved_layers_json,
        }
    } else {
        let tar_file = cli_args
            .initial_files
            .as_ref()
            .expect("clap required_unless_present=oci_image guarantees this is Some");
        if tar_file.extension().and_then(|x| x.to_str()) != Some("tar") {
            anyhow::bail!("Expected a .tar file, found {}", tar_file.display());
        }
        // Pass 136: carry this run's tar path across the CreateProcessW-spawned diagnostic-fork-
        // child boundary (inherited automatically via `lpEnvironment: null`) so a child built with
        // `LITEBOX_DIAG_PROCESS_FORK_GLOBALSTATE=1` can remount the SAME rootfs a real
        // GlobalState-reconstruction probe needs -- see
        // `process_fork::FORK_CHILD_TAR_PATH_ENV_VAR`'s doc comment. Set unconditionally (cheap,
        // a single env var) rather than gated behind the probe's own flag, matching this module's
        // existing precedent of computing cheap diagnostic inputs unconditionally while gating
        // only the logging/behavior that consumes them. Not meaningful for the `--oci-image`
        // path (no single on-disk tar file exists to remount), so left unset there.
        if let Ok(abs_tar) = std::path::absolute(tar_file) {
            unsafe {
                std::env::set_var(
                    litebox_platform_windows_userland::process_fork::FORK_CHILD_TAR_PATH_ENV_VAR,
                    &abs_tar,
                );
            }
        }
        if let Some(export_path) = &cli_args.export_writable_layer {
            // `tar_file` stays memory-mapped for this process's entire lifetime (see
            // `mmapped_file` below), and Windows generally refuses to open a file
            // for writing while a mapping of it is still active. Catch the
            // self-defeating case of exporting onto the same file being read from
            // with a clear error up front, rather than a confusing failure deep in
            // `export_writable_layer` after the whole guest session has already run.
            let initial_files_abs = std::path::absolute(tar_file).map_err(|e| {
                anyhow!(
                    "Could not get absolute path for {}: {}",
                    tar_file.display(),
                    e
                )
            })?;
            let export_path_abs = std::path::absolute(export_path).map_err(|e| {
                anyhow!(
                    "Could not get absolute path for {}: {}",
                    export_path.display(),
                    e
                )
            })?;
            if initial_files_abs == export_path_abs {
                anyhow::bail!(
                    "--export-writable-layer must not point at the same file as --initial-files \
                     ({}): the rootfs archive stays memory-mapped for the whole run, so exporting \
                     onto it would try to overwrite a file that's still open for reading",
                    initial_files_abs.display()
                );
            }
        }
        // Memory-mapped, not heap-copied: every concurrent runner process reading
        // the same rootfs archive shares its physical pages via the OS page cache
        // instead of each holding a private copy.
        RootfsSource::Tar {
            mmap: mmapped_file(tar_file)?,
        }
    };

    // `--publish` must be set BEFORE `Platform::new()` -- more precisely, before anything ever
    // touches `net_gateway` (the `OnceLock<NatGateway>` field) -- since the NAT gateway reads
    // `LITEBOX_PUBLISH` exactly once, at its own lazy-init time, to decide which host listeners to
    // spawn (see `litebox_platform_windows_userland::net`'s module doc comment). `Platform::new()`
    // itself never touches networking, but setting this first, unconditionally, keeps this code
    // from depending on that staying true.
    if !cli_args.publish.is_empty() {
        // SAFETY: single-threaded at this point in `run` (no guest thread, no worker thread, no
        // presenter thread has been spawned yet) -- no concurrent reader of the environment exists.
        unsafe {
            std::env::set_var("LITEBOX_PUBLISH", cli_args.publish.join(","));
        }
    }

    let platform = Platform::new();
    if let RootfsSource::Tar { mmap } = &rootfs_source {
        // Register the rootfs tar's host-mmapped bytes as CoW-eligible (see
        // `TarRo::get_static_backing_data`'s doc comment and
        // `WindowsUserland::try_allocate_cow_pages`): every regular file served out of this tar
        // (e.g. `/bin/busybox`, reached through however many symlinks) can now take the fast
        // CoW-mmap path on exec instead of `do_mmap_file_memcpy`'s page-by-page `sys_read` loop.
        // Mirrors `litebox_runner_linux_userland`'s identical `register_cow_region` call for its
        // own `tar_data`. Not applicable to the `--oci-image` path: those layer bytes are
        // heap-owned (`Cow::Owned`), which `get_static_backing_data` already correctly reports as
        // CoW-ineligible.
        platform.register_cow_region(mmap.data, mmap.abs_path.clone());
    }
    let shim_builder = litebox_shim_linux::LinuxShimBuilder::new(platform);
    let litebox = shim_builder.litebox();

    // The program path is a Unix-style path inside the merged rootfs. Owned (not a borrow of
    // `cli_args`) so it can cross into the spawned initial-guest-thread closures below (see
    // `INITIAL_GUEST_THREAD_STACK_SIZE`'s doc comment) with a `'static` bound.
    let prog_path = cli_args.program_and_arguments[0].clone();

    let initial_file_system = {
        let mut in_mem = litebox::fs::in_mem::FileSystem::new(litebox);
        initialize_root_in_mem_layer(&mut in_mem);
        if let Some(resume_from) = &cli_args.resume_from {
            in_mem.with_root_privileges(|fs| {
                // Best-effort, not a hard requirement: `--resume-from` is no longer only a
                // human operator's own, presumed-good archive -- `spawn_exec_collision_child`/
                // the cross-process fork path now also pass it internally, pointing at the boot
                // tree's shared "latest" filesystem snapshot (see `CONTAINER_FS_SNAPSHOT_ENV_VAR`'s
                // own doc comment), which can legitimately not exist yet (the very first spawn in
                // a fresh boot) or be caught mid-`rename` by a concurrent sibling publishing its
                // own update. A missing or malformed archive there is exactly as recoverable as
                // never having had one -- the process already starts from a correct, empty upper
                // layer otherwise -- so this degrades to that rather than taking the whole process
                // down over a best-effort continuity mechanism's own race.
                if let Err(e) = import_writable_layer(fs, resume_from) {
                    eprintln!(
                        "warning: failed to import --resume-from archive {}: {e} -- starting from \
                         the base rootfs instead",
                        resume_from.display()
                    );
                }
            });
        }

        match rootfs_source {
            RootfsSource::Tar { mmap } => shim_builder.default_fs(in_mem, mmap.data.into()),
            RootfsSource::OciLayers {
                layers,
                resolved_layers_json,
            } => match read_merged_rootfs_index_cache(&resolved_layers_json) {
                Some(entries) => {
                    shim_builder.default_fs_multi_layer_with_cached_merge(in_mem, layers, entries)
                }
                None => {
                    let (fs, freshly_built) = shim_builder.default_fs_multi_layer(in_mem, layers);
                    if let Some(entries) = &freshly_built {
                        write_merged_rootfs_index_cache(&resolved_layers_json, entries);
                    }
                    fs
                }
            }
        }
    };
    let initial_file_system = std::sync::Arc::new(initial_file_system);

    let shim = shim_builder.build();

    // `ControlServer`: named-pipe listener answering `docs/presenter-process-design.md` section 3's
    // command grammar (scanout/screenshot/show/hide/presenter?/key/rel/abs/ps/strace/frames).
    // Started UNCONDITIONALLY -- headless or `--gui`/`--gui=hidden` -- per section 4.3: a caller
    // can `screenshot`/`ps`/`strace`/`frames`/`key`/`rel` with no window regardless, and nothing
    // about starting this listener touches a window, wgpu, or COM (see `control_server.rs`'s own
    // module doc comment). This replaces the old `gui_presenter_thread` closure entirely: the
    // window/wgpu/winit code that used to run on a thread INSIDE this process now lives in a
    // separate `litebox-presenter.exe` process (section 1.2), connected to over this same pipe.
    let presenter_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join("litebox-presenter.exe")))
        .unwrap_or_else(|| PathBuf::from("litebox-presenter.exe"));
    let control = control_server::start(shim.clone(), presenter_exe)
        .expect("failed to start ControlServer (named-pipe listener)");

    // `--gui-hidden` is a deprecated alias for `--gui=hidden` (kept for existing scripts, see the
    // field's own doc comment); merge the two into the one `GuiMode` section 4.1 actually
    // specifies. `--gui-hidden` wins if somehow both are given, since it is the more conservative
    // (non-visible) choice.
    let gui_mode = if cli_args.gui_hidden {
        Some(GuiMode::Hidden)
    } else {
        cli_args.gui
    };
    if let Some(mode) = gui_mode
        && let Err(e) = control_server::spawn_and_maybe_show(&control, mode)
    {
        litebox_util_log::warn!(error:? = e; "failed to start GUI presenter");
    }

    // Spawn a background worker that drives real network I/O (via the in-process userspace NAT
    // gateway, see `litebox_platform_windows_userland::net`) so guest sockets can actually reach
    // the outside world. No Administrator privileges or driver are required: the gateway proxies
    // guest TCP/UDP flows to real, unprivileged Winsock sockets rather than creating a virtual
    // network adapter.
    let shutdown = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let net_shim = shim.clone();
    let net_worker = std::thread::spawn(move || {
        const DEFAULT_TIMEOUT: core::time::Duration = core::time::Duration::from_micros(100);
        const MAX_TIMEOUT: core::time::Duration = core::time::Duration::from_millis(1);
        while !shutdown_clone.load(core::sync::atomic::Ordering::Relaxed) {
            let timeout = loop {
                // `perform_network_interaction` can panic deep inside smoltcp (confirmed live,
                // 2026-09-18: `"handle does not refer to a valid socket"`, `socket_set.rs:116`,
                // reached via a stale `SocketHandle` a dead-holder `reset_after_poisoning()` call
                // elsewhere didn't know this process was still holding). Left uncaught, that panic
                // unwinds this whole thread and kills it permanently -- this process's networking
                // never runs again, and since `Network` is shared across the whole fork family,
                // every OTHER process's own `net_worker` panics on the same stale handle in turn
                // the next time it ticks, until every worker has died and networking silently
                // stops platform-wide (live-confirmed: this exact panic was the LAST log line
                // before a genuine, permanent full-boot stall). Catch it, force the same recovery
                // `net_lock`'s own dead-holder path already performs (see
                // `LinuxShim::force_reset_network_after_panic`'s doc comment), and keep the loop
                // (and this process's networking) alive instead.
                let advice = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    net_shim.perform_network_interaction()
                }));
                match advice {
                    Ok(litebox::net::PlatformInteractionReinvocationAdvice::CallAgainImmediately) => {}
                    Ok(litebox::net::PlatformInteractionReinvocationAdvice::WaitOnDeviceOrSocketInteraction { timeout }) => {
                        break timeout;
                    }
                    Err(payload) => {
                        let panic_msg = panic_payload_message(&payload);
                        litebox_util_log::error!(
                            panic_msg:% = panic_msg;
                            "net_worker: caught a panic inside perform_network_interaction -- \
                             forcing Network::reset_after_poisoning() recovery instead of letting \
                             it kill this thread's networking permanently"
                        );
                        net_shim.force_reset_network_after_panic();
                        break None;
                    }
                }
            };
            platform.wait_on_tun(Some(timeout.unwrap_or(DEFAULT_TIMEOUT).min(MAX_TIMEOUT)));
        }
        // Final flush
        while net_shim
            .perform_network_interaction()
            .call_again_immediately()
        {}
    });

    let argv = cli_args
        .program_and_arguments
        .iter()
        .map(|x| std::ffi::CString::new(x.bytes().collect::<Vec<u8>>()).unwrap())
        .collect();
    let envp: Vec<_> = cli_args
        .environment_variables
        .iter()
        .map(|x| std::ffi::CString::new(x.bytes().collect::<Vec<u8>>()).unwrap())
        .collect();
    let envp = if cli_args.forward_environment_variables {
        envp.into_iter()
            .chain(std::env::vars().map(|(k, v)| {
                // Windows' own env var names are case-insensitive but reported with whatever
                // original casing was set -- notably `Path` (mixed case), never `PATH`. Linux
                // env var lookups (including the guest's own PATH-based executable search) are
                // case-SENSITIVE, so forwarding `Path` verbatim reaches the guest as a completely
                // different, useless variable while the `PATH` Linux tools actually look up is
                // never set at all -- confirmed live: `sh: <cmd>: not found` for any locally-
                // installed binary (e.g. after `npm install`) despite the install itself
                // succeeding, because the guest's `execve`/shell PATH search had nothing to
                // search. Normalize this one, specific, known-mismatched name rather than
                // case-folding every forwarded variable, which could needlessly collide two
                // differently-cased Windows variables that mean different things on Linux.
                //
                // Also prepend the standard Linux search path (see `LINUX_DEFAULT_PATH` above):
                // the forwarded value is the HOST's Windows `Path`, whose `C:\...` entries are
                // meaningless to the guest -- without this prefix, forwarding PATH at all is
                // strictly worse than not forwarding it, since it shadows the guest's own
                // otherwise-implicit default search locations with a value that matches nothing.
                if k.eq_ignore_ascii_case("PATH") {
                    let v = format!("{LINUX_DEFAULT_PATH}:{v}");
                    std::ffi::CString::new(
                        "PATH"
                            .bytes()
                            .chain(*b"=")
                            .chain(v.bytes())
                            .collect::<Vec<u8>>(),
                    )
                    .unwrap()
                } else {
                    std::ffi::CString::new(
                        k.bytes().chain(*b"=").chain(v.bytes()).collect::<Vec<u8>>(),
                    )
                    .unwrap()
                }
            }))
            .collect()
    } else {
        envp
    };

    let fs_for_export = cli_args
        .export_writable_layer
        .is_some()
        .then(|| initial_file_system.clone());

    // Teach the platform how to hand a cross-process `fork()` child this process's writable layer.
    //
    // Registered unconditionally: it costs one boxed closure, and the platform only ever calls it
    // when a cross-process fork actually happens (`LITEBOX_PROCESS_FORK=1`). Uses the SAME tar
    // writer `--export-writable-layer` uses, whose reader is the `import_writable_layer` the child
    // calls -- see `FORK_CHILD_PARENT_LAYER_ENV_VAR` for why the child needs this at all.
    {
        let fs_for_fork = initial_file_system.clone();
        litebox_platform_windows_userland::process_fork::register_parent_writable_layer_exporter(
            Box::new(move |path| {
                export_writable_layer(&fs_for_fork, path).map_err(|e| format!("{e}"))
            }),
        );
    }

    let exit_code = if cli_args.pty_mode {
        let init_task = platform.init_task();

        // `LinuxShimEntrypoints` is deliberately `!Send` (see its own doc comment: "The task
        // should not be moved once it's bound to a platform thread so that we preserve the
        // ability to use TLS in the future") -- so `load_program_attach_pty` (which produces it)
        // must run on the SAME thread that goes on to call `run_thread` with it, not before a
        // thread hop. See `INITIAL_GUEST_THREAD_STACK_SIZE`'s doc comment for why that thread
        // must not be this function's caller: on a normal invocation that's this process's own
        // main thread, whose real stack is Rust's ~1 MiB Windows default, not the 8 MiB a guest
        // program is entitled to assume. `pty_id` is sent back out over a channel as soon as it's
        // known, so the two forwarding threads below can start without waiting for the guest to
        // finish running.
        let (pty_id_tx, pty_id_rx) = std::sync::mpsc::channel();
        let inner_shim = shim.clone();
        let guest_thread = std::thread::Builder::new()
            .stack_size(INITIAL_GUEST_THREAD_STACK_SIZE)
            .spawn(move || {
                // See the identical call (and its doc comment) in the non-pty-mode branch below
                // for why this is needed here, from inside the spawned closure, not before.
                litebox_platform_windows_userland::set_current_thread_guest_pid(init_task.pid);
                let (program, pty_id) = inner_shim
                    .load_program_attach_pty(initial_file_system, init_task, &prog_path, argv, envp)
                    .unwrap();
                pty_id_tx.send(pty_id).expect("receiver dropped");
                unsafe {
                    litebox_platform_windows_userland::run_thread(
                        program.entrypoints,
                        &mut litebox_common_linux::PtRegs::default(),
                    );
                }
                program.process.wait()
            })
            .expect("failed to spawn initial guest thread");
        let pty_id = pty_id_rx.recv().expect("guest thread dropped pty_id sender");

        // Two forwarding threads, mirroring `net_worker`'s existing "background thread pumping
        // shim-internal I/O" pattern above: one drains the pty master's output to this process's
        // real stdout, one copies this process's real stdin to the pty master's input. Both use
        // `LinuxShim::pty_master_read`/`pty_master_write`, which need no `Task` in scope and are
        // safe to call from any thread concurrently with `run_thread` running the guest below (see
        // those methods' doc comments in `litebox_shim_linux`).
        let out_shim = shim.clone();
        // `LITEBOX_GUEST_STDOUT_FILE`, if set, routes the guest's own pty output to a dedicated
        // file instead of this process's real stdout -- opt-in, since the default (sharing stdout
        // with litebox's own `tracing_subscriber` output, see `FlushingStderrWriter` above) is
        // what every existing caller/script still expects. Exists because a caller that redirects
        // both this process's stdout AND stderr into the SAME file (`> out.log 2>&1`, the shape
        // every `--gui`-less debugging repro in this project's own history has used) previously
        // had no way to read a crashing guest program's own diagnostic output cleanly: this
        // forwarder and litebox's own log writer are two independent threads racing to append to
        // the same fd with no shared line-buffering, so their bytes interleave mid-line/mid-ANSI-
        // escape in the combined file -- confirmed live, chasing a labwc/wlroots abort where
        // wlroots' own `wlr_log` diagnostic lines (which would have named the exact failing
        // dimension/mode) were unrecoverable from the combined log for exactly this reason. With
        // this set, the guest's raw bytes land in their own file, so "what did the guest program
        // actually print" is a plain read of that file instead of manual byte-level
        // reconstruction.
        let guest_stdout_file = std::env::var_os("LITEBOX_GUEST_STDOUT_FILE").map(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("failed to open LITEBOX_GUEST_STDOUT_FILE")
        });
        let stdout_forwarder = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut stdout = std::io::stdout();
            let mut guest_file = guest_stdout_file;
            loop {
                match out_shim.pty_master_read(pty_id, &mut buf) {
                    // `Ok(0)`: guest exited and the pty hung up. `Err`: a real read failure. Both
                    // end this forwarding loop the same way.
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        use std::io::Write as _;
                        let ok = if let Some(file) = guest_file.as_mut() {
                            file.write_all(&buf[..n]).is_ok() && file.flush().is_ok()
                        } else {
                            stdout.write_all(&buf[..n]).is_ok() && stdout.flush().is_ok()
                        };
                        if !ok {
                            break;
                        }
                    }
                }
            }
        });
        let in_shim = shim.clone();
        let stdin_forwarder = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut stdin = std::io::stdin();
            loop {
                use std::io::Read as _;
                match stdin.read(&mut buf) {
                    // `Ok(0)`: real stdin hit EOF. `Err`: a real read failure. Both end this
                    // forwarding loop the same way.
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if in_shim.pty_master_write(pty_id, &buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let exit_code = guest_thread.join().expect("initial guest thread panicked");

        // The guest has exited: its slave-side fds are gone, so the pty's real Linux hangup
        // semantics (`PtyEnd::drop`/`GlobalState::hangup_slave`, already exercised by this
        // module's own test suite) shut down the master's read side, which unblocks
        // `stdout_forwarder`'s `pty_master_read` with `Ok(0)`/`Err`. `stdin_forwarder` reading
        // this process's own real stdin has no such natural unblock (a daemon-piped stdin with no
        // more bytes coming just blocks forever), so it's deliberately not joined -- it exits on
        // its own once the process itself exits (`std::process::exit` below tears down every
        // thread unconditionally), matching how a real terminal's write-side simply stops
        // mattering once the session it was feeding is gone.
        let _ = stdout_forwarder.join();
        drop(stdin_forwarder);

        exit_code
    } else {
        let init_task = platform.init_task();
        // See `INITIAL_GUEST_THREAD_STACK_SIZE`'s doc comment: `load_program` (which produces the
        // deliberately `!Send` `LinuxShimEntrypoints`) must run on the SAME thread that goes on to
        // call `run_thread` with it -- so both happen inside the spawned thread, not before it.
        std::thread::Builder::new()
            .stack_size(INITIAL_GUEST_THREAD_STACK_SIZE)
            .spawn(move || {
                // Give this ROOT/initial guest process's own OS thread its `CURRENT_GUEST_PID`
                // identity directly, from the thread itself -- see `set_current_thread_guest_pid`'s
                // doc comment for the full story and the confirmed-live bug this fixes.
                litebox_platform_windows_userland::set_current_thread_guest_pid(init_task.pid);
                // Deliberately NOT `.unwrap()`/`.expect()` here: on this codebase's Windows
                // target, panicking on this specific spawned thread has been observed to
                // overflow the stack during unwind (confirmed live, AGENTS.md's "Reproduction
                // commands" section -- a leading `/` on the program path makes this fail with a
                // real `ENOENT`, but the panic-unwind path masks it behind an opaque "thread
                // '<unknown>' has overflowed its stack" with zero indication of the real cause).
                // Print the real error and exit cleanly instead of unwinding through whatever
                // is fragile on this thread.
                let program = match shim.load_program(
                    initial_file_system,
                    init_task,
                    &prog_path,
                    argv,
                    envp,
                ) {
                    Ok(program) => program,
                    Err(e) => {
                        eprintln!("failed to load program {prog_path:?}: {e:?}");
                        std::process::exit(1);
                    }
                };
                unsafe {
                    litebox_platform_windows_userland::run_thread(
                        program.entrypoints,
                        &mut litebox_common_linux::PtRegs::default(),
                    );
                }
                program.process.wait()
            })
            .expect("failed to spawn initial guest thread")
            .join()
            .expect("initial guest thread panicked")
    };

    if let Some(export_path) = &cli_args.export_writable_layer {
        let fs = fs_for_export.expect("fs_for_export set whenever export_writable_layer is set");
        export_writable_layer(&fs, export_path)
            .unwrap_or_else(|e| panic!("failed to write --export-writable-layer archive: {e}"));
    }

    shutdown.store(true, core::sync::atomic::Ordering::Relaxed);
    // `wait_on_tun`'s timeout is always capped to `MAX_TIMEOUT` (1ms), so the worker re-checks
    // `shutdown` frequently even while otherwise idle; the join below returns promptly.
    let _ = net_worker.join();

    // `--gui`/`--gui=hidden`: keep the process (and the presenter's window, if shown) alive until
    // the user closes it, matching real desktop application behavior -- the guest program that
    // drew the window's content has already exited by this point (this line only runs after
    // `program.process.wait()` above), exactly like a real X11/Wayland client disconnecting from
    // the display server does not close the server or its windows. `litebox-presenter.exe` is now
    // a genuinely separate process, not a thread here, so there is no `JoinHandle` to wait on;
    // instead poll for its control-channel connection to end (it closes its own connection right
    // before exiting on `WindowEvent::CloseRequested`), which is the cross-process equivalent of
    // the old thread join.
    if gui_mode.is_some() {
        while control_server::is_presenter_connected(&control) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    // Loud, not silent: disclose any `LITEBOX_DUMP_FRAMES` frames dropped due to background-
    // writer backpressure before the process tears everything down (`std::process::exit` below
    // does not run destructors, so this must happen here, not in a `Drop` impl).
    litebox_platform_windows_userland::presentation::dump_frame_diagnostic_report_drops();

    std::process::exit(exit_code)
}

/// Pass 136 -- STEP 1 of pass 135's four-step plan: prove a REAL, standalone `GlobalState` (the
/// entire shim-wide runtime state pass 135 found blocks wiring process-based fork into
/// production -- `LiteBox`/`PageManager`, filesystem+tar mount, network subsystem, futex
/// manager, pipes, unix-socket table, elf-patch-cache, flock registry, pty registry) can be
/// constructed a SECOND time, standalone, inside a process-fork diagnostic child that has
/// already gone through pass 114's proven-safe `WindowsUserland::new()` init with pre-populated
/// foreign memory present -- WITHOUT yet trying to make its contents match the parent's actual
/// live state (that is a later step; see `scratchpad/jqrepro/FINDINGS.txt` PASS 136). Gated
/// behind `LITEBOX_DIAG_PROCESS_FORK_GLOBALSTATE=1`
/// (`process_fork::diag_process_fork_globalstate_enabled`); a complete no-op otherwise. Lives in
/// THIS crate (not `litebox_platform_windows_userland`, which has no dependency on
/// `litebox_shim_linux` and so cannot reference `LinuxShimBuilder` at all) because this is the
/// only crate in the dependency graph that has both the shim-construction API and the
/// `--initial-files` tar path.
///
/// Only ever called from `main()`'s diagnostic-resume-child branch, BEFORE
/// `run_diagnostic_resume_child()` (which, for the real-resume gate combination, parks its thread
/// and never returns) -- never from the normal (non-fork-diagnostic) `run()` path,
/// and never in a way that feeds back into the real, unmodified thread-based `do_clone` fork
/// path this crate's normal execution still uses exclusively.
pub fn diag_process_fork_globalstate_probe() {
    // ROOT-CAUSE FIX (2026-09-17): everything this function goes on to do -- rebuilding the
    // rootfs, constructing `GlobalState`, adopting the parent's `PageManager`, and above all
    // `run_thread_with_fork_verification`'s real guest execution (every syscall emulated on this
    // same call stack, including whatever host-side call frames X11 client-library init incurs
    // for something like `xset q`) -- used to run inline on whatever OS thread called this
    // function. Per `main()`'s dispatch, that is THIS PROCESS'S OWN PRIMARY THREAD for a
    // `CreateProcessW`-spawned cross-process-fork child, whose stack is Windows' ordinary main-
    // thread default (~1 MiB), never widened by anything analogous to `INITIAL_GUEST_THREAD_
    // STACK_SIZE`'s explicit `std::thread::Builder::stack_size` call on the ordinary (non-fork)
    // guest-launch path (see that constant's doc comment for the identical defect, already fixed
    // there: "the very first guest program's initial thread ran inline on whatever OS thread
    // called `run()`... whose real stack is Rust's ~1 MiB Windows default"). This function's own
    // heavier, syscall-emulation-heavy guest execution never got the same fix.
    //
    // Live-confirmed root cause of the pre-existing, load-scaling stack-overflow class
    // `AGENTS.md`'s "unix_addr_table presence sharing" section documents: a real
    // `.wfgy/webtop_stack.sh` boot under `LITEBOX_PROCESS_FORK=1` hit `xset q`'s forked process
    // dying with a genuine host `STATUS_STACK_OVERFLOW` (Rust's own "thread 'main' has overflowed
    // its stack" guard-page message) BEFORE it ever reached `connect()` -- 122 identical
    // occurrences by `XVFB_FAILED`, proven via a controlled `git stash`/rebuild/re-run A/B to be
    // completely unrelated to any same-session code (patched and clean-`main` builds hit the
    // identical count). Every other guest-work-capable thread in this codebase already gets
    // `INITIAL_GUEST_THREAD_STACK_SIZE`/`GUEST_THREAD_STACK_SIZE` (32 MiB) via an explicit
    // `.stack_size()` call; this was the one guest-execution path in the entire cross-process-fork
    // machinery that never got it, because it runs on a freshly `CreateProcessW`-spawned
    // process's own primary thread rather than a `std::thread::Builder`-spawned one.
    //
    // Fix: spawn a dedicated thread with the same stack size every other guest-executing thread
    // gets, and block this call until it finishes. `diag_process_fork_task_resume_probe`'s success
    // path calls `std::process::exit` directly, which terminates the WHOLE process regardless of
    // which thread calls it -- so `.join()`'s return value is only ever actually observed on an
    // early-return/error path that never reached real guest execution (missing rootfs env vars,
    // a failed rootfs rebuild, etc.), matching this function's pre-existing early-return contract
    // exactly.
    std::thread::Builder::new()
        .stack_size(INITIAL_GUEST_THREAD_STACK_SIZE)
        .spawn(diag_process_fork_globalstate_probe_inner)
        .expect("failed to spawn cross-process fork child's guest-execution thread")
        .join()
        .expect("cross-process fork child's guest-execution thread panicked");
}

fn diag_process_fork_globalstate_probe_inner() {
    if !litebox_platform_windows_userland::process_fork::diag_process_fork_globalstate_enabled() {
        return;
    }
    // Investigative timing only (LITEBOX_DIAG_FORK_TIMING=1): breaks down where a cross-process
    // fork child's startup time actually goes, to tell the rootfs-re-merge cost apart from
    // Platform::new()'s cold-start cost -- see docs/track-b-fork-fix-progress.md's per-fork
    // overhead entries. `t0` is this function's own entry, the earliest point a child-specific
    // clock can start (process creation itself, and the re-exec/CreateProcessW machinery before
    // this, are not covered).
    let diag_timing = std::env::var_os("LITEBOX_DIAG_FORK_TIMING").is_some();
    let t0 = std::time::Instant::now();
    macro_rules! diag_elapsed {
        ($label:expr) => {
            if diag_timing {
                eprintln!("[diag-fork-timing] {} at {:?}", $label, t0.elapsed());
            }
        };
    }
    // The child's read-only rootfs, from whichever source this run booted with. It re-execs with
    // no command line of its own, so both arrive by environment: a `--initial-files` tar as a path
    // to mmap, an `--oci-image` as the REFERENCE to re-derive from the digest-keyed layer cache the
    // parent has already warmed.
    //
    // Only the tar case existed. On the `--oci-image` path a child therefore arrived with no
    // rootfs at all and could not `execve` anything -- which is why enabling `LITEBOX_PROCESS_FORK`
    // on an OCI-booted webtop broke it outright (`XVFB_FAILED`, `DBUS_FAILED`) while the same build
    // without it reached a running desktop. See `FORK_CHILD_OCI_IMAGE_ENV_VAR`.
    let oci_ref = std::env::var(
        litebox_platform_windows_userland::process_fork::FORK_CHILD_OCI_IMAGE_ENV_VAR,
    )
    .ok()
    .filter(|v| !v.is_empty());
    let tar_path = std::env::var_os(
        litebox_platform_windows_userland::process_fork::FORK_CHILD_TAR_PATH_ENV_VAR,
    )
    .filter(|v| !v.is_empty());

    let layer_digests_json = std::env::var(
        litebox_platform_windows_userland::process_fork::FORK_CHILD_OCI_LAYER_DIGESTS_ENV_VAR,
    )
    .ok()
    .filter(|v| !v.is_empty());

    let tar_layers: Vec<std::borrow::Cow<'static, [u8]>> = if let Some(image_ref) = &oci_ref {
        eprintln!(
            "[process_fork_diag] globalstate-probe (child): rebuilding rootfs from OCI image {image_ref}"
        );
        // Layers are read from the on-disk digest+rewriter-version cache rather than the
        // network, so this is a local read of bytes the parent has already produced -- the two
        // processes agree by construction because they run the same code over the same digests,
        // with no separate artifact to keep in sync. When the parent's already-resolved digest
        // list arrived too (the normal case -- see `FORK_CHILD_OCI_LAYER_DIGESTS_ENV_VAR`), skip
        // this child's own manifest fetch entirely; it would just re-discover the identical
        // digests the parent already resolved, at the cost of a real, unconditional multi-second
        // network round-trip (measured, see that constant's own doc comment). Falls back to the
        // ordinary manifest-fetching path if the digest list didn't arrive for some reason
        // (e.g. an older parent build), rather than failing this fork outright.
        let pull_result = if let Some(digests_json) = &layer_digests_json {
            litebox_packager::oci::pull_layers_with_known_digests(image_ref, digests_json, true)
        } else {
            litebox_packager::oci::pull_layers_in_memory(image_ref, true)
        };
        match pull_result {
            Ok(pulled) => {
                diag_elapsed!("rootfs layers ready (pull_layers_in_memory returned)");
                pulled.layers
            }
            Err(e) => {
                eprintln!(
                    "[process_fork_diag] globalstate-probe (child): failed to rebuild rootfs from {image_ref}: {e}"
                );
                return;
            }
        }
    } else if let Some(tar_path) = &tar_path {
        eprintln!(
            "[process_fork_diag] globalstate-probe (child): attempting standalone GlobalState construction"
        );
        match mmapped_file(tar_path) {
            Ok(f) => vec![f.data.into()],
            Err(e) => {
                eprintln!(
                    "[process_fork_diag] globalstate-probe (child): failed to mmap tar at {}: {e}",
                    PathBuf::from(tar_path).display()
                );
                return;
            }
        }
    } else {
        eprintln!(
            "[process_fork_diag] globalstate-probe (child): no rootfs source arrived via {} or {}, skipping",
            litebox_platform_windows_userland::process_fork::FORK_CHILD_OCI_IMAGE_ENV_VAR,
            litebox_platform_windows_userland::process_fork::FORK_CHILD_TAR_PATH_ENV_VAR
        );
        return;
    };

    let platform = Platform::new();
    diag_elapsed!("Platform::new() returned");
    let shim_builder = litebox_shim_linux::LinuxShimBuilder::new(platform);
    let litebox = shim_builder.litebox();

    // This cross-process child's adopted `Task` carries `uid: 0, euid: 0, gid: 0, egid: 0`
    // (`diag_process_fork_task_resume_probe`'s `task_params` below), matching the real bootstrap
    // process's own root identity -- so this freshly-reconstructed in-mem upper layer needs the
    // SAME root-identity/FHS-scaffolding setup `run()`'s own bootstrap performs, via the shared
    // `initialize_root_in_mem_layer` helper, or a write into a root-owned ancestor directory
    // inherited from the real bootstrap's rootfs (e.g. apk migrating a file up from the
    // read-only tar layer) fails `in_mem::FileSystem::mkdir`'s permission check with
    // `MkdirError::NoWritePerms`, which `FileSystem::mkdir_migrating_ancestor_dirs`'s caller has
    // no handling for besides `unimplemented!` (`litebox/src/fs/layered.rs:243`).
    let mut in_mem = litebox::fs::in_mem::FileSystem::new(litebox);
    initialize_root_in_mem_layer(&mut in_mem);

    // Adopt the parent's writable layer, so this child's filesystem is the one `fork()` promises
    // rather than a pristine rootfs. Without it a child cannot see anything the parent -- or an
    // earlier sibling, whose own export the parent has already imported -- has written; see
    // `FORK_CHILD_PARENT_LAYER_ENV_VAR` for the exact `/init` failure that exposed this.
    //
    // Best-effort in the same way the child-to-parent export is: a missing or malformed archive
    // degrades to the base rootfs rather than aborting a child that is otherwise ready to run.
    // `with_root_privileges` because this child's adopted `Task` runs as root and the entries
    // being restored are root-owned, matching `--resume-from`'s own import above.
    if let Some(parent_layer) = std::env::var_os(
        litebox_platform_windows_userland::process_fork::FORK_CHILD_PARENT_LAYER_ENV_VAR,
    )
        // Empty means "this fork had nothing to hand over", which the spawner writes explicitly
        // rather than omitting -- see `FORK_CHILD_PARENT_LAYER_ENV_VAR`'s push site for why
        // omitting it would instead resurrect a grandparent's stale path.
        && !parent_layer.is_empty()
    {
        let parent_layer = PathBuf::from(&parent_layer);
        let writable_layer_size = std::fs::metadata(&parent_layer).map(|m| m.len()).ok();
        in_mem.with_root_privileges(|fs| match import_writable_layer(fs, &parent_layer) {
            Ok(()) => eprintln!(
                "[process_fork_diag] globalstate-probe (child): adopted the parent's writable layer from {}",
                parent_layer.display()
            ),
            Err(e) => eprintln!(
                "[process_fork_diag] globalstate-probe (child): could not adopt the parent's writable layer from {}: {e}",
                parent_layer.display()
            ),
        });
        diag_elapsed!(format!(
            "writable layer imported (size={} bytes)",
            writable_layer_size.unwrap_or(0)
        ));
        // Do NOT delete `parent_layer`: it is (almost always) the ONE canonical
        // `CONTAINER_FS_SNAPSHOT_ENV_VAR` path shared by the whole boot tree, not a fresh
        // single-consumer temp file -- see `FORK_CHILD_PARENT_LAYER_ENV_VAR`'s own doc comment.
        // Several children spawned in the same narrow window are routinely handed the identical
        // path; deleting it here after just ONE of them imports race-deletes the file out from
        // under every other sibling/cousin still waiting to open it (`could not adopt the
        // parent's writable layer ...: The system cannot find the file specified. (os error 2)`,
        // confirmed live 2026-09-17). The old "single-use, this child is its only reader" premise
        // predates `publish_as_container_fs_snapshot` centralizing every export onto one
        // canonical path; it no longer holds. Leaving the file in place is safe and matches
        // `run()`'s own `--resume-from` import above, which never deleted it either -- every
        // future exporter atomically replaces it in place (`rename`/`copy` over the same path).
    }

    // `default_fs` is `default_fs_multi_layer` with a one-element list, so a tar and an OCI layer
    // stack converge here exactly as they do in `run()`. `layer_digests_json` is only `Some` on
    // the real `--oci-image` multi-layer path (see its own binding above) -- that's the SAME
    // input the top-level `run()` boot used to key its own merged-index cache entry, so a fork
    // child reads the exact entry its ancestor (or an earlier sibling) already populated instead
    // of re-parsing+re-folding all `tar_layers` from scratch. See `merged_rootfs_index_cache_key`/
    // `MergedRootfsIndexCache*`'s own doc comments for the full mechanism and the measured cost
    // this removes (3.2-3.5s of the ~3.9s this child otherwise spends before it can even resume
    // guest execution).
    let fs = match &layer_digests_json {
        Some(digests_json) => match read_merged_rootfs_index_cache(digests_json) {
            Some(entries) => {
                shim_builder.default_fs_multi_layer_with_cached_merge(in_mem, tar_layers, entries)
            }
            None => {
                let (fs, freshly_built) =
                    shim_builder.default_fs_multi_layer(in_mem, tar_layers);
                if let Some(entries) = &freshly_built {
                    write_merged_rootfs_index_cache(digests_json, entries);
                }
                fs
            }
        },
        None => shim_builder.default_fs_multi_layer(in_mem, tar_layers).0,
    };
    diag_elapsed!("default_fs_multi_layer returned (rootfs indexed/merged)");
    let fs = std::sync::Arc::new(fs);

    // This child is itself a fork parent for any child IT goes on to spawn, and it never reaches
    // `run()` -- so without registering here, its own children would be handed nothing and would
    // start from the base rootfs, losing everything this process and its ancestors had written.
    // Same registration `run()` performs, over this child's own filesystem.
    {
        let fs_for_fork = fs.clone();
        litebox_platform_windows_userland::process_fork::register_parent_writable_layer_exporter(
            Box::new(move |path| {
                export_writable_layer(&fs_for_fork, path).map_err(|e| format!("{e}"))
            }),
        );
    }

    // `LinuxShimBuilder::build()` is the exact call pass 135 identified as the sole construction
    // site of `GlobalState`, exercised here a SECOND time within this same host OS process
    // lifetime (the diagnostic child's own, freshly re-exec'd process) -- proving construction
    // itself does not collide with anything the pass 111/112 memory-copy step already populated
    // in this address space, independent of whether its CONTENTS end up matching the parent
    // (out of scope this pass; see FINDINGS.txt PASS 136's "what pass 137 should do" section).
    let shim = shim_builder.build::<litebox_shim_linux::DefaultFS<Platform>>();

    eprintln!(
        "[process_fork_diag] globalstate-probe (child): GlobalState constructed successfully, no crash/hang/error"
    );
    // Diagnostic-only (`LITEBOX_DIAG_GLOBALSTATE_SHARE_PROBE=1`), the child-side half of the
    // decisive live cross-process `GlobalState` create-vs-attach proof -- see
    // `litebox_shim_linux::syscalls::process::Task::try_cross_process_fork`'s matching parent-side
    // sentinel bump for the full mechanism. A genuinely ATTACHED `GlobalState` observes the
    // parent's post-bump value here; an independently-constructed one shows the pristine
    // `next_thread_id: 2.into()` a fresh `GlobalState` always starts from -- the two are never
    // confusable (the bump delta is 100,000).
    if std::env::var_os("LITEBOX_DIAG_GLOBALSTATE_SHARE_PROBE").is_some() {
        eprintln!(
            "[globalstate_share_probe] child observed next_thread_id={}",
            shim.diag_next_thread_id()
        );
    }
    // Diagnostic-only (`LITEBOX_DIAG_UNIX_ADDR_PRESENCE_PROBE=1`), the child-side half of the
    // decisive live cross-process `SharedUnixAddrPresenceTable` proof -- see
    // `litebox_shim_linux::syscalls::process::Task::try_cross_process_fork`'s matching parent-side
    // inserts. `before` is expected `Some(parent_pid)` even for a plain independent copy (the
    // parent registered it before spawning); `after` is the decisive one -- only a genuinely
    // ATTACHED (not merely consistently-addressed) table observes a key the parent registered
    // AFTER this child process already existed. `0` is `UNIX_ADDR_KIND_PATH` (kept as a bare
    // literal here since `litebox_shim_linux`'s presence-table internals are deliberately
    // `pub(crate)`, not exported to this diagnostic-only caller).
    if std::env::var_os("LITEBOX_DIAG_UNIX_ADDR_PRESENCE_PROBE").is_some() {
        let before = shim.diag_unix_addr_presence_lookup(0, b"PRESENCE_PROBE_BEFORE");
        let after = shim.diag_unix_addr_presence_lookup(0, b"PRESENCE_PROBE_AFTER");
        eprintln!(
            "[unix_addr_presence_probe] child observed before={before:?} after={after:?}"
        );
    }
    diag_elapsed!("GlobalState built, handing off to vmem-adopt-probe");

    diag_process_fork_vmem_adopt_probe(platform, &shim, fs, t0);
}

/// Pass 137's `Vmem`/`PageManager`-adoption probe, gated behind
/// `LITEBOX_DIAG_PROCESS_FORK_VMEM_ADOPT=1` (layered on the pass-136 `GLOBALSTATE` gate, which is
/// what constructs the `LiteBox` handed in here); a complete no-op otherwise.
///
/// The `GlobalState` pass 136 proved constructible in this child carries a `PageManager` built by
/// the ordinary `PageManager::new` -- empty, describing nothing, and normally grown by an ELF load
/// that a fork child must not perform. This probe instead builds a SECOND, independent
/// `PageManager` via `litebox::mm::PageManager::new_adopting_existing_memory`, whose bookkeeping
/// describes the guest memory that pass 111/112's `WriteProcessMemory` step ALREADY placed in this
/// process's address space at the parent's own addresses -- allocating, reserving and copying
/// nothing. It then verifies, region by region, that the reconstruction round-trips the parent's
/// real layout (boundaries, raw `VmFlags`, file-backing) and that the program break matches.
///
/// Purely observational: the adopted `PageManager` is dropped at the end of this function and is
/// never installed into `shim`, never used to build a `Task`, and never resumed into -- those are
/// pass 138+'s job (FINDINGS.txt pass 135 STEP 4 item 3). Nothing here can feed back into the
/// real, unmodified thread-based `do_clone` fork path.
fn diag_process_fork_vmem_adopt_probe(
    platform: &'static Platform,
    shim: &litebox_shim_linux::LinuxShim<Platform, litebox_shim_linux::DefaultFS<Platform>>,
    fs: std::sync::Arc<litebox_shim_linux::DefaultFS<Platform>>,
    t0: std::time::Instant,
) {
    use litebox_platform_windows_userland::process_fork as pf;

    if !pf::diag_process_fork_vmem_adopt_enabled() {
        return;
    }
    let diag_timing = std::env::var_os("LITEBOX_DIAG_FORK_TIMING").is_some();
    macro_rules! diag_elapsed {
        ($label:expr) => {
            if diag_timing {
                eprintln!("[diag-fork-timing] {} at {:?}", $label, t0.elapsed());
            }
        };
    }
    let litebox = shim.litebox();
    let Some(line) = std::env::var_os(pf::FORK_CHILD_VMA_LAYOUT_ENV_VAR) else {
        eprintln!(
            "[process_fork_diag] vmem-adopt-probe (child): no VMA layout arrived via {}, skipping",
            pf::FORK_CHILD_VMA_LAYOUT_ENV_VAR
        );
        return;
    };
    let Some(line) = line.to_str().map(str::to_owned) else {
        eprintln!(
            "[process_fork_diag] vmem-adopt-probe (child): VMA layout env var is not valid UTF-8, skipping"
        );
        return;
    };
    let Some(relocations) = litebox::mm::AddressRelocations::deserialize_for_diagnostic(&line)
    else {
        eprintln!(
            "[process_fork_diag] vmem-adopt-probe (child): failed to parse VMA layout line (len={}), skipping",
            line.len()
        );
        return;
    };

    // SOURCE coordinates == this child's own coordinates: the whole design of the process-based
    // fork is that the child's reservations are forced to the parent's own bases, so the
    // source-to-destination translation is the identity here (FINDINGS.txt passes 111/112/122).
    let expected = relocations.vma_layout();
    let heap_top = relocations.heap_top();
    eprintln!(
        "[process_fork_diag] vmem-adopt-probe (child): adopting {} pre-populated region(s), brk={heap_top:#x}",
        expected.len()
    );

    // The SAME `ALIGN` the shim's own `PageManager` uses (`LinuxShim::page_manager`'s
    // `PageManager<Platform, PAGE_SIZE>`), so this reconstruction is directly comparable to the
    // real one a future pass would install in its place.
    let (page_manager, adopted, shared) = litebox::mm::PageManager::<
        Platform,
        { litebox::mm::linux::PAGE_SIZE },
    >::new_adopting_existing_memory(
        litebox, expected.iter().cloned(), heap_top
    );
    diag_elapsed!("PageManager::new_adopting_existing_memory returned");

    let (tracked_count, tracked_brk) = page_manager.tracked_region_summary();
    let tracked = page_manager.tracked_regions();

    // Region-by-region equality, not merely a count: the point of this probe is proving the
    // reconstructed bookkeeping MATCHES the parent's real layout, boundaries and permissions
    // included (the CONTENTS at those addresses are already correct by construction, having been
    // `WriteProcessMemory`'d there verbatim -- this is the Rust-level bookkeeping catching up).
    //
    // 45th pass (2026-09-22): `VM_SHARED` regions are now deliberately EXCLUDED from `tracked`
    // (see `Vmem::new_adopting_existing_memory`'s own doc comment for why adopting them with a
    // dangling `shared_handle: None` was a live, reproducible crash) -- exclude them here too, or
    // this probe's own comparison would misreport every single run as a MISMATCH for an outcome
    // that is now the intended, correct behavior rather than a real discrepancy.
    //
    // 52nd pass (2026-09-22): this filter went stale the moment the 46th pass gave `PROT_NONE`
    // regions (no `VM_READ`/`VM_WRITE`/`VM_EXEC` bit set) the exact same "skip adoption" treatment
    // as `VM_SHARED` in `Vmem::new_adopting_existing_memory` (see that function's own doc comment,
    // "PROT_NONE regions get the SAME treatment as VM_SHARED") -- this filter was never updated to
    // match, so it kept every `PROT_NONE` region in `sorted_expected` while `tracked` (built by the
    // now-46th-pass-aware real code) correctly has none. A live full `webtop_stack.sh` boot
    // (`.wfgy/webtop_release_boot6.log`) showed this stale gap firing as "MISMATCH -- 34 differing
    // region(s), count 34 vs 117" on every single cross-process fork child for the whole run --
    // alarming-looking, but a process with many threads (thread-stack guard pages) and many mmap'd
    // shared libraries (glibc arena/malloc guard gaps) genuinely has dozens of legitimate
    // `PROT_NONE` regions, so a large true count of them (83 here) was never a sign that real
    // memory adoption was broken -- it was this diagnostic-only comparison comparing the real,
    // correctly-filtered `tracked` set against a stale, unfiltered `expected` set. Filtering
    // `PROT_NONE` out here too makes the comparison match what the real adoption code actually
    // does, instead of reporting a phantom mismatch on every boot.
    let mut sorted_expected: Vec<_> = expected
        .iter()
        .filter(|(_, flag_bits, _)| {
            let flags = litebox::mm::linux::VmFlags::from_bits_truncate(*flag_bits);
            !flags.contains(litebox::mm::linux::VmFlags::VM_SHARED)
                && !flags
                    .intersection(litebox::mm::linux::VmFlags::VM_ACCESS_FLAGS)
                    .is_empty()
        })
        .cloned()
        .collect();
    sorted_expected.sort_by_key(|(r, _, _)| r.start);
    let layout_matches = tracked == sorted_expected;
    let mismatches = sorted_expected
        .iter()
        .zip(tracked.iter())
        .filter(|(a, b)| a != b)
        .count();

    eprintln!(
        "[process_fork_diag] vmem-adopt-probe (child): adopted={adopted} (of which VM_SHARED={shared}), \
         tracked={tracked_count}, expected={}, brk={tracked_brk:#x} (expected {heap_top:#x})",
        sorted_expected.len()
    );
    if layout_matches && tracked_brk == heap_top {
        eprintln!(
            "[process_fork_diag] vmem-adopt-probe (child): VMA layout adoption VERIFIED -- every \
             region's boundaries, flags and file-backing round-trip exactly, no allocation performed"
        );
    } else {
        eprintln!(
            "[process_fork_diag] vmem-adopt-probe (child): VMA layout adoption MISMATCH -- \
             {mismatches} differing region(s), count {tracked_count} vs {}, brk {tracked_brk:#x} vs {heap_top:#x}",
            sorted_expected.len()
        );
    }

    // Live cross-process `SharedArc<T>` proof (`LITEBOX_DIAG_SHARED_ARC_PROBE=1`), a no-op
    // otherwise: this child process's own startup never implicitly touches the shared kernel
    // arena anymore (ordinary `GlobalAlloc` traffic was reverted off it -- see `SLAB_ALLOC`'s doc
    // comment in `litebox_platform_windows_userland`), so this explicit call is what actually
    // exercises `SharedArc::attach` on the child side. Self-gated; see
    // `shared_arc_probe_child_attach`'s own doc comment for why it must be called explicitly here
    // rather than from inside the shared-heap init path.
    litebox_platform_windows_userland::shared_arc_probe_child_attach();
    diag_elapsed!("vmem-adopt-probe verification done, handing off to task-resume-probe");
    diag_process_fork_task_resume_probe(platform, shim, fs, page_manager, relocations, t0);
}

/// Pass 139's in-process `Task`-resume probe, gated behind
/// `LITEBOX_DIAG_PROCESS_FORK_TASK_RESUME=1` (layered on the pass-137 `VMEM_ADOPT` gate, which is
/// what supplies `page_manager` here).
///
/// Where passes 118-122/138 injected a translated register context into an externally-suspended
/// thread via cross-process `SetThreadContext` -- proven (pass 138) to fault immediately on the
/// guest's very first syscall, since the target thread never ran `spawn_thread`/`thread_start`/
/// `run_thread_arch`'s own init -- this probe instead builds a real `Task` locally (via
/// `LinuxShim::adopt_forked_process`, using this pass's freshly-adopted `page_manager`) and calls
/// the PUBLIC `litebox_platform_windows_userland::run_thread` entry point directly, on THIS
/// thread, exactly the same function the real, unmodified, non-fork initial-process-load path
/// already calls. This establishes `run_thread_arch`'s own init (`TlsState`'s `HOST_SP`/
/// `HOST_BP`, the `syscall_callback` return-address contract) the normal way, in-process, with no
/// cross-process register injection needed for this leg at all.
fn diag_process_fork_task_resume_probe(
    platform: &'static Platform,
    shim: &litebox_shim_linux::LinuxShim<Platform, litebox_shim_linux::DefaultFS<Platform>>,
    fs: std::sync::Arc<litebox_shim_linux::DefaultFS<Platform>>,
    page_manager: litebox::mm::PageManager<Platform, { litebox::mm::linux::PAGE_SIZE }>,
    relocations: litebox::mm::AddressRelocations,
    t0: std::time::Instant,
) {
    use litebox_platform_windows_userland::process_fork as pf;

    if !pf::diag_process_fork_task_resume_enabled() {
        return;
    }
    let diag_timing = std::env::var_os("LITEBOX_DIAG_FORK_TIMING").is_some();
    macro_rules! diag_elapsed {
        ($label:expr) => {
            if diag_timing {
                eprintln!("[diag-fork-timing] {} at {:?}", $label, t0.elapsed());
            }
        };
    }
    diag_elapsed!("task-resume-probe entered");
    let Some(line) = std::env::var_os(pf::FORK_CHILD_GPRS_ENV_VAR) else {
        eprintln!(
            "[process_fork_diag] task-resume-probe (child): no register snapshot arrived via {}, skipping",
            pf::FORK_CHILD_GPRS_ENV_VAR
        );
        return;
    };
    let Some(line) = line.to_str() else {
        eprintln!(
            "[process_fork_diag] task-resume-probe (child): register snapshot env var is not valid UTF-8, skipping"
        );
        return;
    };
    let Some(gprs) = pf::deserialize_full_gprs(line) else {
        eprintln!(
            "[process_fork_diag] task-resume-probe (child): failed to parse register snapshot line (len={}), skipping",
            line.len()
        );
        return;
    };

    // Stdio-only, single-thread, freshly-"execve'd"-looking process shape -- mirrors the same
    // credentials/pid/ppid a real forked child would carry. pid==tid matches `load_program`'s own
    // bootstrap-process convention (a single-threaded process's tid equals its pid).
    let pid = std::process::id().cast_signed();
    let task_params = litebox_common_linux::TaskParams {
        pid,
        ppid: pid,
        uid: 0,
        euid: 0,
        gid: 0,
        egid: 0,
    };
    let fs_for_export = fs.clone();
    let entrypoints = shim.adopt_forked_process(fs, task_params, page_manager);

    // Reopen the regular-file fds the parent held.
    //
    // No bridge needed, unlike a pipe: this child's filesystem IS the parent's (its writable layer
    // arrived with the spawn), so the same path at the same offset is the same file. Enough for
    // the case that matters -- a shell that saved its own script fd out of the way before forking.
    // See `litebox::platform::ForkInheritedFile` for what a reopen preserves and what it does not.
    // Recreate the eventfds the parent held. Cheaper than a file: no reopen and no bridge, since
    // an eventfd is a counter and two behaviour bits. See `litebox::platform::ForkInheritedEventfd`
    // for what a recreate preserves (everything a wakeup fd needs) and what it does not (a counter
    // genuinely SHARED with the parent).
    if let Some(spec) = std::env::var_os(pf::FORK_CHILD_EVENTFDS_ENV_VAR)
        && let Some(spec) = spec.to_str()
    {
        for item in spec.split(',').filter(|s| !s.is_empty()) {
            let mut parts = item.split(':');
            let parsed = (|| {
                let fd = i32::from_str_radix(parts.next()?, 16).ok()?;
                let count = u64::from_str_radix(parts.next()?, 16).ok()?;
                let flags = u32::from_str_radix(parts.next()?, 16).ok()?;
                Some((fd, count, flags))
            })();
            let Some((fd, count, flags)) = parsed else {
                eprintln!(
                    "[process_fork_diag] task-resume-probe (child): unparseable inherited-eventfd entry {item:?}, guest fd will be missing"
                );
                continue;
            };
            match entrypoints.install_eventfd_at_fd(fd, count, flags) {
                Some(()) => eprintln!(
                    "[process_fork_diag] task-resume-probe (child): guest fd {fd} recreated as an eventfd (count={count}, flags={flags:#x})"
                ),
                None => eprintln!(
                    "[process_fork_diag] task-resume-probe (child): could not recreate eventfd at guest fd {fd}, it will be missing"
                ),
            }
        }
    }

    if let Some(spec) = std::env::var_os(pf::FORK_CHILD_FILE_FDS_ENV_VAR)
        && let Some(spec) = spec.to_str()
    {
        for item in spec.split(',').filter(|s| !s.is_empty()) {
            let mut parts = item.split(':');
            let parsed = (|| {
                let fd = i32::from_str_radix(parts.next()?, 16).ok()?;
                let offset = u64::from_str_radix(parts.next()?, 16).ok()?;
                let flags = u32::from_str_radix(parts.next()?, 16).ok()?;
                let path = String::from_utf8(pf::hex_decode(parts.next()?)?).ok()?;
                Some((fd, offset, flags, path))
            })();
            let Some((fd, offset, flags, path)) = parsed else {
                eprintln!(
                    "[process_fork_diag] task-resume-probe (child): unparseable inherited-file entry {item:?}, guest fd will be missing"
                );
                continue;
            };
            match entrypoints.install_file_at_fd(fd, &path, flags, offset) {
                Some(()) => eprintln!(
                    "[process_fork_diag] task-resume-probe (child): guest fd {fd} reopened on {path} at offset {offset}"
                ),
                None => eprintln!(
                    "[process_fork_diag] task-resume-probe (child): could not reopen {path} at guest fd {fd}, it will be missing"
                ),
            }
        }
    }

    // Rebuild the guest pipe fds this child could not inherit.
    //
    // `adopt_forked_process` hands back a fresh, stdio-only fd table -- correct, because litebox's
    // pipes are in-memory `ringbuf` objects with no OS handle behind them and genuinely cannot
    // cross a process boundary. What DID cross is a real Windows pipe per fd, inherited from the
    // parent via `CreateProcessW`, whose handle values and directions arrived in this child's
    // environment block (`FORK_CHILD_PIPE_FDS_ENV_VAR`). So for each one: create a fresh local
    // litebox pipe, put the end the guest will USE at the fd number it expects, and run a host
    // thread bridging the other end to the inherited Windows handle.
    //
    // Must happen HERE: before `run_thread_with_fork_verification` consumes `entrypoints`, and on
    // this thread, because `LinuxShimEntrypoints` is deliberately `!Send`.
    if let Some(spec) = std::env::var_os(pf::FORK_CHILD_PIPE_FDS_ENV_VAR)
        && let Some(spec) = spec.to_str()
    {
        for item in spec.split(',').filter(|s| !s.is_empty()) {
            let mut parts = item.split(':');
            let parsed = (|| {
                let fd = parts.next()?.parse::<i32>().ok()?;
                let handle = usize::from_str_radix(parts.next()?, 16).ok()?;
                let dir = pf::ChildPipeEnd::from_tag(parts.next()?)?;
                Some((fd, handle, dir))
            })();
            let Some((fd, handle, dir)) = parsed else {
                eprintln!(
                    "[process_fork_diag] task-resume-probe (child): unparseable inherited-pipe entry {item:?}, guest fd will be missing"
                );
                continue;
            };
            // The guest gets the end it will USE; the host pump gets the other one.
            let host_end = match dir {
                pf::ChildPipeEnd::ChildWrites => entrypoints.install_pipe_write_end_at_fd(fd),
                pf::ChildPipeEnd::ChildReads => entrypoints.install_pipe_read_end_at_fd(fd),
            };
            let Some(host_end) = host_end else {
                eprintln!(
                    "[process_fork_diag] task-resume-probe (child): could not install a pipe at guest fd {fd}, it will be missing"
                );
                continue;
            };
            eprintln!(
                "[process_fork_diag] task-resume-probe (child): guest fd {fd} rebuilt over inherited Windows pipe handle {handle:#x} (child {})",
                match dir {
                    pf::ChildPipeEnd::ChildWrites => "writes",
                    pf::ChildPipeEnd::ChildReads => "reads",
                }
            );
            let pump_shim = shim.clone();
            // Live-caught (2026-09-21): this pump thread's `detached_pipe_read`/`detached_pipe_write`
            // calls route through the SAME shim/`Task`-adjacent machinery ordinary guest execution
            // does (`WaitState`/blocking-wait plumbing), but -- unlike every OTHER guest-work-capable
            // thread in this codebase (`INITIAL_GUEST_THREAD_STACK_SIZE` at `run()`'s own
            // `guest_thread`, `diag_process_fork_globalstate_probe`'s dedicated thread just above)
            // -- this one used the bare `std::thread::spawn` default (Windows' ~1 MiB), the EXACT
            // same defect class `diag_process_fork_globalstate_probe`'s own doc comment already
            // root-caused and fixed for its sibling thread on 2026-09-17. Live-reproduced: a
            // cross-process-fork child piping into another (`env | grep`, or any subshell wrapping
            // one) hit a real host `STATUS_STACK_OVERFLOW` ("thread '<unknown>' has overflowed its
            // stack") specifically inside a pipe-carrying fork's bootstrap, non-deterministically
            // (reproduces reliably once concurrent cross-process children are already competing for
            // the host, matching the shape of the real `webtop_stack.sh` boot's `xfce4-session`
            // launch racing the selkies-bind-watchdog/tail-f loops) -- exactly the kind of
            // load-dependent stack pressure a too-small default stack produces, not a logic bug in
            // the pump loop itself. Fixed the same way as its sibling: an explicit, generous stack.
            std::thread::Builder::new()
                .stack_size(INITIAL_GUEST_THREAD_STACK_SIZE)
                .spawn(move || {
                let mut buf = [0u8; 4096];
                match dir {
                    // Drain what the guest wrote into the inherited handle, then close it: that
                    // is what gives the parent's own pump a zero-byte read, and hence the guest on
                    // the far side its EOF. `detached_pipe_read` blocks in the guest pipe's own
                    // wait machinery and returns 0 once the guest has closed every writer.
                    pf::ChildPipeEnd::ChildWrites => {
                        // `chunk_num`/`total_relayed` (added during the pipe-relay-sigpipe
                        // investigation, 2026-09-16): kept as a permanent, low-volume trace point
                        // -- this only prints once, on the terminal chunk of this pipe's lifetime,
                        // not per-chunk. When a relay hop fails partway, knowing exactly how many
                        // bytes/chunks it had already relayed cleanly narrows "which side closed
                        // and when" far faster than `n` alone, as this investigation itself needed
                        // live to distinguish a genuine early failure from the ordinary EOF shape.
                        let mut total_relayed = 0u64;
                        let mut chunk_num = 0u64;
                        while let Some(n) = pump_shim.detached_pipe_read(&host_end, &mut buf) {
                            chunk_num += 1;
                            let write_ok =
                                n != 0 && pf::write_all_to_inherited_handle(handle, &buf[..n]);
                            if !write_ok {
                                eprintln!(
                                    "[process_fork_diag] pipe pump (child, fd {fd}, handle={handle:#x}): stream ended (n={n}), chunk={chunk_num} total_relayed_before_this_chunk={total_relayed}, closing the inherited handle to deliver EOF upstream"
                                );
                                break;
                            }
                            total_relayed += n as u64;
                        }
                    }
                    // Fill the guest's pipe from the inherited handle. Dropping `host_end` at the
                    // end releases the local write end, so the guest's `read` sees EOF once the
                    // parent's side is done.
                    pf::ChildPipeEnd::ChildReads => {
                        loop {
                            let n = pf::read_from_inherited_handle(handle, &mut buf);
                            if n == 0 {
                                eprintln!(
                                    "[process_fork_diag] pipe pump (child, fd {fd}): upstream closed, releasing the guest pipe's write end so the guest sees EOF"
                                );
                                break;
                            }
                            let mut off = 0usize;
                            while off < n {
                                match pump_shim.detached_pipe_write(&host_end, &buf[off..n]) {
                                    Some(0) | None => break,
                                    Some(w) => off += w,
                                }
                            }
                        }
                    }
                }
                drop(host_end);
                // Safety: this thread is the sole owner of `handle`, and closes it exactly once.
                unsafe { pf::close_inherited_handle(handle) };
            })
                .expect("failed to spawn cross-process fork child's pipe pump thread");
        }
    }

    let mut ctx = litebox_common_linux::PtRegs {
        r15: gprs.r15,
        r14: gprs.r14,
        r13: gprs.r13,
        r12: gprs.r12,
        rbp: gprs.rbp,
        rbx: gprs.rbx,
        r11: gprs.r11,
        r10: gprs.r10,
        r9: gprs.r9,
        r8: gprs.r8,
        rax: gprs.rax,
        rcx: gprs.rcx,
        rdx: gprs.rdx,
        rsi: gprs.rsi,
        rdi: gprs.rdi,
        orig_rax: gprs.orig_rax,
        rip: gprs.rip,
        cs: gprs.cs,
        eflags: gprs.eflags,
        rsp: gprs.rsp,
        ss: gprs.ss,
    };

    // Pass 143: this child's guest thread is a BRAND-NEW OS thread in a BRAND-NEW OS process --
    // unlike the thread-based fork path (`ThreadInitState::ForkedChild`'s own `sys_arch_prctl
    // (ArchPrctlArg::SetFs(fs_base))` call, `litebox_shim_linux/src/syscalls/process.rs`), nothing
    // has ever propagated the parent's `%fs` base (backing the guest's TLS pointer) to this
    // thread. Without this, the guest's very first FS-relative access (musl issues one
    // essentially immediately after `fork()` returns) dereferences FS base 0 and crashes on the
    // guest's first instruction -- exactly the 100%-reproducible `addr=0x0` SIGSEGV pass 142
    // root-caused. `gprs.fs_base` carries the SAME already-translated-and-self-pointer-fixed-up
    // value `do_clone`'s own `fs_base` computation derives for the thread-based path (see that
    // computation's doc comment for the ABI self-referential-pointer fixup, already applied on
    // the parent's side, in the parent's own address space, before this child was ever spawned --
    // `WriteProcessMemory`-visible to this child by construction). Set it the SAME way the
    // thread-based path does: through the platform's own `ArchSpecificProvider`, which internally
    // calls `wrfsbase` for the CURRENT thread -- this call runs ON the child's own host thread,
    // exactly where `%fs` needs to be programmed.
    //
    // Deliberately reuses the SAME `platform` reference `diag_process_fork_globalstate_probe`
    // already constructed via its one `Platform::new()` call, rather than calling `Platform::new()`
    // again here: `WindowsUserland::new()` is a genuine full re-init (per-call
    // `AddVectoredExceptionHandler` registration, a fresh console-resize-watcher thread spawn, and
    // -- the actual bug this fixes -- `WindowsUserland::init_thread_fs_base()`, which unconditionally
    // resets `THREAD_FS_BASE` to 0 on the calling thread). A second `Platform::new()` call here would
    // reset the very FS base this call is trying to set, immediately before setting it, but any code
    // between here and `run_thread` (or Windows' own periodic FS_BASE-to-0 reset, see this module's
    // VEH repair mechanism) reading `THREAD_FS_BASE` via a stale second copy of platform-internal
    // state would still observe 0 -- exactly the symptom live-observed before this fix (the child's
    // own `[veh]` trace line read `thread_fs_base=0x7feffffebb28`, a leftover loader-thread-local
    // value from a LATER spurious `Platform::new()` call in this function, never this call's actual
    // `gprs.fs_base` value).
    litebox::platform::ArchSpecificProvider::set_arch_specific_register(
        platform,
        &litebox::platform::ArchSpecificRegister::FsBase,
        gprs.fs_base,
    )
    .expect("cross-process fork child: failed to set FS base before resuming guest code");

    // Pass 156: `run()`'s bootstrap spawns a background `net_worker` thread that repeatedly calls
    // `perform_network_interaction()` to drive the guest's smoltcp `Network` state machine --
    // this is what actually turns a guest socket syscall into a real packet reaching the
    // `WindowsUserland` NAT gateway (`litebox_platform_windows_userland::net`) and pumps replies
    // (e.g. DNS UDP responses) back into the guest's socket buffers. The NAT gateway itself is a
    // separate, `OnceLock`-lazily-initialized per-`Platform` thread (`net::NatGateway::new`) that
    // starts fine on its own the first time any `IPInterfaceProvider` method is called on this
    // child's freshly-constructed `platform` -- but nothing upstream of this call ever spawned
    // the `net_worker` counterpart in the process-fork child's bootstrap
    // (`diag_process_fork_globalstate_probe`/`diag_process_fork_vmem_adopt_probe` never touch
    // networking at all), so a process-forked child's guest DNS/socket traffic was enqueued into
    // `Network`'s internal state but never actually polled/flushed, matching pass 155's
    // "NAT gateway starts but DNS still fails" observation exactly. Spawn the SAME worker here,
    // mirroring `run()`'s own construction verbatim, so this child's guest execution gets the
    // same continuous network pump the default (non-process-fork) path always had.
    let net_shim = shim.clone();
    // Same `INITIAL_GUEST_THREAD_STACK_SIZE` fix as the pipe-pump thread above, same class of
    // defect (a `std::thread::spawn` default-stack thread doing guest-work-adjacent work in this
    // fork-child bootstrap) -- fixed proactively alongside it rather than waiting for its own
    // separate live repro, since it is spawned from the identical bootstrap under the identical
    // concurrent-fork host load this session live-caught overflowing the pipe-pump thread.
    std::thread::Builder::new()
        .stack_size(INITIAL_GUEST_THREAD_STACK_SIZE)
        .spawn(move || {
        const DEFAULT_TIMEOUT: core::time::Duration = core::time::Duration::from_micros(100);
        const MAX_TIMEOUT: core::time::Duration = core::time::Duration::from_millis(1);
        loop {
            let timeout = loop {
                // Same panic-recovery discipline as `run()`'s own `net_worker` -- see its doc
                // comment. A cross-process-fork child is exactly where this was first
                // live-confirmed to matter (2026-09-18: this worker's own panic was the LAST
                // thing ever logged before a genuine, permanent full-boot stall).
                let advice = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    net_shim.perform_network_interaction()
                }));
                match advice {
                    Ok(litebox::net::PlatformInteractionReinvocationAdvice::CallAgainImmediately) => {}
                    Ok(litebox::net::PlatformInteractionReinvocationAdvice::WaitOnDeviceOrSocketInteraction { timeout }) => {
                        break timeout;
                    }
                    Err(payload) => {
                        let panic_msg = panic_payload_message(&payload);
                        litebox_util_log::error!(
                            panic_msg:% = panic_msg;
                            "net_worker (fork child): caught a panic inside perform_network_interaction -- \
                             forcing Network::reset_after_poisoning() recovery instead of letting \
                             it kill this thread's networking permanently"
                        );
                        net_shim.force_reset_network_after_panic();
                        break None;
                    }
                }
            };
            platform.wait_on_tun(Some(timeout.unwrap_or(DEFAULT_TIMEOUT).min(MAX_TIMEOUT)));
        }
    })
        .expect("failed to spawn cross-process fork child's net_worker thread");

    eprintln!(
        "[process_fork_diag] task-resume-probe (child, winpid={}): built Task, set fs_base={:#x}, calling \
         run_thread with rip={:#x} rsp={:#x} -- entering real guest execution",
        std::process::id(),
        gprs.fs_base,
        ctx.rip,
        ctx.rsp
    );
    diag_elapsed!("fd/pipe rebuild + net_worker spawn done, about to call run_thread_with_fork_verification");
    // Arm the SAME post-fork stale-pointer verification the real, working thread-based fork path
    // arms via `Task::init`'s `ThreadInitState::ForkedChild` branch (`begin_fork_child_
    // verification`, litebox_shim_linux/src/syscalls/process.rs) -- this cross-process child never
    // goes through that dispatch (it is built via `adopt_forked_process`, not `do_clone`'s
    // same-process path), so without this call `fork_verify` never engages here and any stale
    // pointer left over from the `WriteProcessMemory` copy (the same class of hazard the
    // thread-based path's own verification exists to heal) goes completely unrepaired.
    //
    // Pass 143: calling `begin_fork_child_verification` HERE, before `run_thread`, was itself a
    // silent no-op -- `fork_verify::begin`'s effect only takes hold once `get_tls_ptr()` returns
    // `Some`, which is only true from partway through `run_thread`'s OWN internals onward (see
    // `run_thread_with_fork_verification`'s doc comment in `litebox_platform_windows_userland`).
    // Use that dedicated entry point instead, which arms fork_verify at exactly the right point in
    // the sequence -- after TLS install, before the guest is ever resumed.
    let process = entrypoints.process();
    // NOTE (2026-09-17 investigation): a live A/B (this call vs. plain `run_thread` with
    // `fork_verify` never armed at all) proved `fork_verify`'s single-step machinery is NOT the
    // cause of this session's stack-overflow investigation (identical overflow, same location,
    // with or without it) -- the real root cause was `GlobalStateHandle.litebox` reading a
    // cross-process-stale pointer (see that struct's doc comment). `fork_verify` stays wired
    // exactly as pass 143 designed it: it does real, live-needed stale-pointer healing for
    // whatever the group-relocations copy doesn't cover, independent of this fix.
    unsafe {
        litebox_platform_windows_userland::run_thread_with_fork_verification(
            entrypoints,
            &mut ctx,
            std::sync::Arc::new(relocations),
        );
    }
    eprintln!(
        "[process_fork_diag] task-resume-probe (child): run_thread returned (guest thread terminated)"
    );
    diag_elapsed!("run_thread_with_fork_verification returned (guest execution complete)");

    // Pass 142: this child process only ever exists as a `LITEBOX_PROCESS_FORK=1` cross-process
    // fork() child (or this same probe's pre-existing diagnostic use, which never previously
    // reached this point live) -- there is no other reason a `CreateProcessW`-spawned re-exec of
    // this binary would take the task-resume path. Falling through to this function's caller and
    // an ordinary `main()` return would exit with Windows code 0 regardless of the guest's real
    // exit status, discarding exactly the information `sys_wait4`'s cross-process branch (see
    // `litebox_shim_linux::syscalls::process::decode_cross_process_wait_status`) needs to report
    // correctly to the parent. `wait_for_encoded_cross_process_exit_status` blocks for the real
    // status (already available immediately -- `run_thread` only returns once the guest's last
    // thread has terminated) and encodes it via the SAME scheme `sys_wait4` decodes.
    let encoded = process.wait_for_encoded_cross_process_exit_status();

    // Pass 157: export this child's writable filesystem-layer deltas (every file it created or
    // modified during its run, e.g. `apk`'s installed packages) to the SAME deterministic,
    // pid-keyed path the parent's `sys_wait4` cross-process branch reads back from once it
    // observes this exit -- closing the gap pass 156 (STEP 7/8) documented: a cross-process
    // child's filesystem writes previously vanished with its own process, invisible to the
    // parent's subsequent `&&`/pipeline commands. Best-effort: if the tar path never arrived (the
    // env var missing, or a write failure), the child still exits with its real, correctly
    // encoded status below -- a lost filesystem export degrades to today's pre-pass-157 behavior
    // rather than blocking this process's own exit.
    // `FORK_CHILD_TAR_PATH_ENV_VAR` is unset for an `--oci-image` boot (no single on-disk tar file
    // exists to name this export after -- see that env var's own doc comment); `cross_process_
    // writable_export_path` only needs SOME path to derive a filename stem from, so a fixed
    // placeholder stands in for it there. Without this arm, every OCI-booted cross-process fork
    // child silently skipped this export entirely -- not a narrower, disclosed trade-off, a real
    // gap this pass closes, found while auditing this exact mechanism for `spawn_exec_collision_
    // child`'s own writable-layer continuity.
    let tar_path_for_naming = std::env::var_os(pf::FORK_CHILD_TAR_PATH_ENV_VAR)
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os(pf::FORK_CHILD_OCI_IMAGE_ENV_VAR)
                .map(|_| std::path::PathBuf::from("oci-image"))
        });
    if let Some(tar_path) = tar_path_for_naming {
        let export_path = pf::cross_process_writable_export_path(&tar_path, std::process::id());
        match export_writable_layer(&fs_for_export, &export_path) {
            Ok(()) => {
                eprintln!(
                    "[process_fork_diag] task-resume-probe (child): exported writable layer to {}",
                    export_path.display()
                );
                // Also publish as the boot tree's shared "latest" snapshot -- see
                // `CONTAINER_FS_SNAPSHOT_ENV_VAR`'s own doc comment -- so a LATER sibling
                // (fork or exec-collision, anywhere in the tree) sees this child's writes even
                // if the parent's own `wait4` has not yet reaped it. `export_path` itself must
                // survive intact for the PARENT's own later `wait4`-time read
                // (`cross_process_writable_export_path` recomputes this exact deterministic path
                // from this child's pid), so this copies to a fresh scratch path first and lets
                // `publish_as_container_fs_snapshot` perform the actual publish via its atomic
                // rename -- NOT a raw `std::fs::copy` straight onto the shared path, which used to
                // let a concurrent importer (another fork child's `globalstate-probe`) open the
                // shared file mid-overwrite and read a torn tar (`failed to read tar entry:
                // numeric field was not a number`, confirmed live 2026-09-17, one occurrence in
                // ~180 adopts). See `publish_as_container_fs_snapshot`'s own doc comment for why
                // every writer of this shared path must route through it.
                if std::env::var_os(
                    litebox_platform_windows_userland::process_fork::CONTAINER_FS_SNAPSHOT_ENV_VAR,
                )
                .is_some()
                {
                    let scratch = std::env::temp_dir().join(format!(
                        "litebox-container-fs-publish-{}.tar",
                        std::process::id()
                    ));
                    if std::fs::copy(&export_path, &scratch).is_ok() {
                        let _ = litebox_platform_windows_userland::process_fork::publish_as_container_fs_snapshot(scratch);
                    } else {
                        let _ = std::fs::remove_file(&scratch);
                    }
                }
            }
            Err(e) => eprintln!(
                "[process_fork_diag] task-resume-probe (child): failed to export writable layer to {}: {e}",
                export_path.display()
            ),
        }
    }

    diag_elapsed!("writable layer exported, about to call std::process::exit");
    eprintln!(
        "[process_fork_diag] task-resume-probe (child): exiting with encoded status {encoded:#x}"
    );
    std::process::exit(encoded.cast_signed());
}

/// Format version for the on-disk merged-rootfs-index cache this module reads/writes (bump
/// whenever `litebox::fs::tar_ro::{encode,decode}_merged_live_entries`'s wire format -- or
/// anything else about what this cache stores -- changes shape; mirrors
/// `litebox_syscall_rewriter::REWRITER_CACHE_VERSION`'s own bump discipline one cache layer down
/// the pipeline).
///
/// # Why this cache exists
///
/// Measured live (`LITEBOX_DIAG_FORK_TIMING=1`, a real `debian-xfce` boot,
/// `LITEBOX_PROCESS_FORK=1`): `default_fs_multi_layer returned (rootfs indexed/merged)` landed at
/// 3.9-4.1s from a cross-process fork child's own start, of which `rootfs layers ready` (all 17
/// per-layer cache entries served from the EXISTING `.litebox-cache` disk cache) landed at only
/// 0.58-0.67s -- meaning `TarIndex::from_layers`'s own tar-parse-plus-whiteout-fold, not the
/// layer bytes themselves, is 3.2-3.5s of EVERY SINGLE fork's startup, the dominant share by far.
/// A live boot's WM-startup phase alone showed 12+ forks in well under a minute, each one
/// independently, redundantly re-deriving the EXACT SAME merge result the very first process in
/// the boot tree already computed -- concurrently-alive fork children were observed each holding
/// ~800 MiB of working set for this, with host free RAM falling ~2.4 GiB in that same window
/// (`docs/AGENTS_ARCHIVE_2026-09-22.md`, 56th pass). Since the base OCI layers never change
/// within one boot, this merge is a pure, deterministic function of the resolved layer digest
/// list -- exactly the kind of repeated, avoidable work `.litebox-cache`'s existing per-layer
/// cache already exists to eliminate one stage earlier in this same pipeline. This cache applies
/// the identical idea one stage later: cache the MERGE's own result, not just its raw inputs.
const MERGED_ROOTFS_INDEX_CACHE_VERSION: u32 = 1;

/// Build a compact, filesystem-safe cache-file identifier from `resolved_layers_json` (the same
/// already-resolved-digest-list JSON string both `run()`'s own boot and every cross-process fork
/// child already carry -- see `FORK_CHILD_OCI_LAYER_DIGESTS_ENV_VAR`). Not itself a correctness
/// boundary: `read_merged_rootfs_index_cache` embeds and re-checks the FULL JSON string inside
/// the cache file before trusting its contents, so a hash collision here can only ever cause an
/// extra cache miss, never a wrong-data cache hit.
fn merged_rootfs_index_cache_key(resolved_layers_json: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    resolved_layers_json.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn merged_rootfs_index_cache_path(cache_key: &str) -> std::path::PathBuf {
    std::path::Path::new(".litebox-cache").join(format!(
        "mergedidx_{cache_key}_v{MERGED_ROOTFS_INDEX_CACHE_VERSION}.bin"
    ))
}

/// Look up a previously cached, whiteout-resolved rootfs merge for `cache_key`
/// ([`merged_rootfs_index_cache_key`]'s output). `None` on ANY doubt -- missing file, I/O error,
/// malformed contents, or (belt-and-suspenders against a hash collision in the cache_key itself)
/// an embedded key that doesn't byte-for-byte match `resolved_layers_json` -- exactly the same
/// "any doubt is a cache miss" discipline `litebox_packager::oci::cache::read_cached_layer`
/// already applies one stage earlier in this pipeline. A miss just means the caller pays the real
/// `TarRo::from_layers` cost this pass, same as if this cache didn't exist.
fn read_merged_rootfs_index_cache(
    resolved_layers_json: &str,
) -> Option<Vec<litebox::fs::tar_ro::MergedLiveEntry>> {
    let diag = std::env::var_os("LITEBOX_DIAG_FORK_TIMING").is_some();
    let cache_key = merged_rootfs_index_cache_key(resolved_layers_json);
    let path = merged_rootfs_index_cache_path(&cache_key);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            if diag {
                eprintln!("[diag-mergedidx] MISS reading {}: {e}", path.display());
            }
            return None;
        }
    };
    let Some(key_len) = bytes
        .get(0..4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .map(|n| n as usize)
    else {
        if diag {
            eprintln!("[diag-mergedidx] MISS {}: truncated key_len", path.display());
        }
        return None;
    };
    let Some(stored_key) = bytes
        .get(4..4usize.checked_add(key_len)?)
        .and_then(|s| core::str::from_utf8(s).ok())
    else {
        if diag {
            eprintln!("[diag-mergedidx] MISS {}: truncated/invalid stored key", path.display());
        }
        return None;
    };
    if stored_key != resolved_layers_json {
        if diag {
            eprintln!(
                "[diag-mergedidx] MISS {}: key mismatch (stored {} bytes, expected {} bytes)",
                path.display(),
                stored_key.len(),
                resolved_layers_json.len()
            );
        }
        return None;
    }
    let result = litebox::fs::tar_ro::decode_merged_live_entries(bytes.get(4 + key_len..)?);
    if diag {
        match &result {
            Some(entries) => eprintln!(
                "[diag-mergedidx] HIT {} ({} entries)",
                path.display(),
                entries.len()
            ),
            None => eprintln!("[diag-mergedidx] MISS {}: decode failed", path.display()),
        }
    }
    result
}

/// Persist `entries` (a [`litebox::fs::tar_ro::TarRo::live_entries_after_merge`] result) as the
/// cache entry for `resolved_layers_json`, for [`read_merged_rootfs_index_cache`] to find on a
/// later, equivalent fork or boot. Best-effort and non-fatal, matching every other cache in this
/// pipeline: a write failure (read-only filesystem, disk full, a losing race against a sibling
/// fork writing the SAME entry concurrently) just means this pass, and every pass until someone
/// succeeds, keeps paying the real build cost -- never worse than not having this cache.
///
/// Write-temp-then-rename, not a direct write: several sibling fork children can race to
/// populate the SAME cache entry (they all compute the identical bytes, by construction), and a
/// reader must never observe a torn/partial file mid-write -- same atomicity discipline
/// `litebox_packager::oci::cache::write_cached_layer_inner` already uses one stage earlier.
fn write_merged_rootfs_index_cache(
    resolved_layers_json: &str,
    entries: &[litebox::fs::tar_ro::MergedLiveEntry],
) {
    let cache_key = merged_rootfs_index_cache_key(resolved_layers_json);
    let final_path = merged_rootfs_index_cache_path(&cache_key);
    let Some(dir) = final_path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut bytes = Vec::with_capacity(4 + resolved_layers_json.len());
    bytes.extend_from_slice(&(resolved_layers_json.len() as u32).to_le_bytes());
    bytes.extend_from_slice(resolved_layers_json.as_bytes());
    bytes.extend_from_slice(&litebox::fs::tar_ro::encode_merged_live_entries(entries));

    let tmp_path = dir.join(format!(
        ".tmp-mergedidx-{}-{}",
        std::process::id(),
        cache_key
    ));
    let diag = std::env::var_os("LITEBOX_DIAG_FORK_TIMING").is_some();
    if std::fs::write(&tmp_path, &bytes).is_ok() {
        let renamed = std::fs::rename(&tmp_path, &final_path);
        if diag {
            eprintln!(
                "[diag-mergedidx] WROTE {} ({} entries, {} bytes) ok={}",
                final_path.display(),
                entries.len(),
                bytes.len(),
                renamed.is_ok()
            );
        }
    } else {
        let _ = std::fs::remove_file(&tmp_path);
        if diag {
            eprintln!("[diag-mergedidx] write FAILED for {}", final_path.display());
        }
    }
}

/// Export the writable upper layer of a layered file system (every file the guest created or
/// modified this run) to a tar archive at `export_path`, for a later run's `--resume-from`.
///
/// Only the upper layer is walked -- the read-only lower layer (the packaged base rootfs) is
/// never re-exported, so the archive is a delta, not a full rootfs snapshot.
fn export_writable_layer<Upper, Lower>(
    fs: &litebox::fs::layered::FileSystem<Platform, Upper, Lower>,
    export_path: &std::path::Path,
) -> Result<()>
where
    Upper: litebox::fs::FileSystem,
    Lower: litebox::fs::FileSystem,
{
    let entries = litebox::fs::export::export_all(fs.upper())
        .map_err(|e| anyhow!("failed to walk writable layer: {e:?}"))?;

    let file = std::fs::File::create(export_path)
        .map_err(|e| anyhow!("failed to create {}: {e}", export_path.display()))?;
    let mut builder = tar::Builder::new(file);
    for entry in &entries {
        let tar_path = entry.path.trim_start_matches('/');
        if tar_path.is_empty() {
            continue;
        }
        let mut header = tar::Header::new_ustar();
        header.set_mode(entry.mode.bits() & 0o777);
        header.set_uid(1000);
        header.set_gid(1000);
        match entry.file_type {
            litebox::fs::FileType::Directory => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_cksum();
                builder
                    .append_data(&mut header, tar_path, std::io::empty())
                    .map_err(|e| anyhow!("failed to add {tar_path} to export tar: {e}"))?;
            }
            litebox::fs::FileType::RegularFile => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(entry.contents.len() as u64);
                header.set_cksum();
                builder
                    .append_data(&mut header, tar_path, entry.contents.as_slice())
                    .map_err(|e| anyhow!("failed to add {tar_path} to export tar: {e}"))?;
            }
            // A FIFO is metadata only, like a directory -- but it must still be archived, or a
            // cross-process fork child would receive it as a plain empty file.
            litebox::fs::FileType::Fifo => {
                header.set_entry_type(tar::EntryType::Fifo);
                header.set_size(0);
                header.set_cksum();
                builder
                    .append_data(&mut header, tar_path, std::io::empty())
                    .map_err(|e| anyhow!("failed to add {tar_path} to export tar: {e}"))?;
            }
            litebox::fs::FileType::Symlink => {
                let Some(target) = &entry.symlink_target else {
                    continue;
                };
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header
                    .set_link_name(target)
                    .map_err(|e| anyhow!("symlink target {target} invalid for {tar_path}: {e}"))?;
                header.set_cksum();
                builder
                    .append_data(&mut header, tar_path, std::io::empty())
                    .map_err(|e| anyhow!("failed to add {tar_path} to export tar: {e}"))?;
            }
            // Character devices and any future FileType variant: not archived (recreated
            // structurally by whatever consumes the import, e.g. /dev in a fresh guest boot).
            _ => {}
        }
    }
    builder
        .finish()
        .map_err(|e| anyhow!("failed to finalize {}: {e}", export_path.display()))?;
    Ok(())
}

/// Seed `fs`'s writable layer from a tar archive previously produced by
/// [`export_writable_layer`], resuming a prior session's on-disk state.
fn import_writable_layer(
    fs: &mut litebox::fs::in_mem::FileSystem<Platform>,
    resume_from: &std::path::Path,
) -> Result<()> {
    use litebox::fs::FileSystem as _;

    let file = std::fs::File::open(resume_from)
        .map_err(|e| anyhow!("failed to open {}: {e}", resume_from.display()))?;
    let mut archive = tar::Archive::new(file);
    let entries = archive
        .entries()
        .map_err(|e| anyhow!("failed to read {}: {e}", resume_from.display()))?;

    for entry_result in entries {
        let mut entry = entry_result.map_err(|e| anyhow!("failed to read tar entry: {e}"))?;
        let header_path = entry
            .path()
            .map_err(|e| anyhow!("invalid entry path in {}: {e}", resume_from.display()))?
            .to_string_lossy()
            .into_owned();
        let path = alloc::format!("/{header_path}");
        let mode_bits = entry.header().mode().unwrap_or(0o644);
        let mode = litebox::fs::Mode::from_bits_truncate(mode_bits & 0o777);

        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                // Ignore AlreadyExists: the guest's default fs layout may have already created
                // this directory (e.g. `/tmp`, `/etc`).
                let _ = fs.mkdir(&*path, mode);
            }
            // A FIFO must come back as a FIFO. Restoring it as an ordinary file -- which the
            // catch-all arm below would do -- means the process reading this layer opens a plain
            // file where the guest expects a pipe, so a read returns instant EOF instead of
            // blocking for a writer. `AlreadyExists` is fine: these archives are round-tripped
            // between a parent and its cross-process `fork()` children, so a child's export
            // restates everything it adopted.
            tar::EntryType::Fifo => match fs.make_fifo(&*path, mode) {
                Ok(()) | Err(litebox::fs::errors::MkdirError::AlreadyExists) => {}
                Err(e) => return Err(anyhow!("failed to recreate fifo {path}: {e:?}")),
            },
            tar::EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .map_err(|e| anyhow!("invalid symlink target for {path}: {e}"))?
                    .ok_or_else(|| anyhow!("symlink entry {path} has no target"))?
                    .to_string_lossy()
                    .into_owned();
                match fs.symlink(&*target, &*path) {
                    Ok(()) => {}
                    // Replace an existing link, for the same round-tripping reason as above --
                    // and matching `litebox::fs::import`, whose own abort-on-repeat cost a
                    // child's entire writable layer before it was fixed.
                    Err(litebox::fs::errors::SymlinkError::AlreadyExists) => {
                        let _ = fs.unlink(&*path);
                        fs.symlink(&*target, &*path)
                            .map_err(|e| anyhow!("failed to recreate symlink {path}: {e:?}"))?;
                    }
                    Err(e) => return Err(anyhow!("failed to recreate symlink {path}: {e:?}")),
                }
            }
            _ => {
                let mut contents = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut contents)
                    .map_err(|e| anyhow!("failed to read {path} from archive: {e}"))?;
                // Some archives (e.g. ones built by appending individual files with
                // `tarfile.open(path, 'a')` or GNU `tar -r` rather than a full
                // directory-recursive `tar -c`) omit the intermediate `Directory`
                // entries for a file's parent path. `fs.open` below requires every
                // parent component to already exist, so create them here rather than
                // assuming the archive lists directories before the files inside them
                // -- a real, reproducible panic (`PathError(MissingComponent)`) hit on
                // `advisor/probes/run_xfce_staged.sh` in a genuine archive from this
                // session without this. Ignore AlreadyExists for the same reason as the
                // `Directory` arm above.
                if let Some(parent) = std::path::Path::new(&path).parent() {
                    let mut built = String::new();
                    for component in parent.components() {
                        use std::path::Component;
                        match component {
                            Component::RootDir => built.push('/'),
                            Component::Normal(part) => {
                                if !built.ends_with('/') {
                                    built.push('/');
                                }
                                built.push_str(&part.to_string_lossy());
                                let _ = fs.mkdir(&*built, litebox::fs::Mode::from_bits_truncate(0o755));
                            }
                            _ => {}
                        }
                    }
                }
                let fd = fs
                    .open(
                        &*path,
                        litebox::fs::OFlags::WRONLY
                            | litebox::fs::OFlags::CREAT
                            | litebox::fs::OFlags::TRUNC,
                        mode,
                    )
                    .map_err(|e| anyhow!("failed to create {path} while resuming: {e:?}"))?;
                fs.write(&fd, &contents, None)
                    .map_err(|e| anyhow!("failed to write {path} while resuming: {e:?}"))?;
                fs.close(&fd)
                    .map_err(|e| anyhow!("failed to close {path} while resuming: {e:?}"))?;
            }
        }
    }
    Ok(())
}
