// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Restrict this crate to only work on Windows. For now, we are restricting this to only x86-64
// Windows, but we _may_ allow for more in the future, if we find it useful to do so.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

extern crate alloc;

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

use anyhow::{Result, anyhow};
use clap::Parser;
use litebox_platform_windows_userland::WindowsUserland as Platform;
use memmap2::Mmap;
use std::path::{Path, PathBuf};

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

    /// Open a real host window and display the guest's `/dev/dri/card0` DRM output in it (see
    /// `litebox_shim_linux::syscalls::drm::DrmSubsystem` and
    /// `litebox_platform_windows_userland::presentation`) -- opt-in, since most invocations
    /// (scripted CLI usage, the common case this runner otherwise serves) have no GUI content to
    /// show and should never have a window pop up unexpectedly.
    #[arg(long = "gui")]
    pub gui: bool,

    /// Run the GUI presenter with its window HIDDEN at startup. Implies `--gui`: the whole
    /// display pipeline (window, wgpu surface, input wiring, frame capture) is created and
    /// running, there is simply nothing on screen until it is shown.
    ///
    /// This exists because a guest's GUI must not depend on a window existing. A desktop session
    /// can boot, render, and be captured (`LITEBOX_DUMP_FRAMES`) headlessly, then be revealed
    /// later -- headless and headed become the same running system observed differently, rather
    /// than two modes chosen before the guest starts.
    #[arg(long = "gui-hidden")]
    pub gui_hidden: bool,
}

struct MmappedFile {
    data: &'static [u8],
    abs_path: PathBuf,
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
        for dir in ["/run", "/var", "/var/log", "/var/cache", "/var/tmp"] {
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
pub fn run(cli_args: CliArgs) -> Result<()> {
    // The shim is `no_std` and cannot read the environment itself, so translate
    // `LITEBOX_DRM_TRACE=1` here. This logs every DRM ioctl at its single dispatch
    // point, which is what answers "is the guest still page-flipping?" -- the
    // question that separates a compositor that stopped presenting from a client
    // presenting an empty buffer.
    litebox_shim_linux::syscalls::set_drm_trace(
        std::env::var_os("LITEBOX_DRM_TRACE").is_some(),
    );

    litebox_platform_windows_userland::install_memcpy_watch_from_env();

    tracing_subscriber::fmt()
        .with_writer(|| FlushingStderr)
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .with_level(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_env_var("LITEBOX_LOG")
                .from_env_lossy(),
        )
        .init();

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
        },
    }

    let rootfs_source = if let Some(image_ref) = &cli_args.oci_image {
        eprintln!("Pulling OCI image (runtime, in-memory): {image_ref}");
        // `pull_layers_in_memory` pulls, decompresses, AND rewrites each layer's ELFs one layer
        // at a time internally -- never holding more than one layer's raw+rewritten bytes at
        // once. Rewriting again here would be redundant (and re-introduce the same
        // all-layers-at-once memory spike this function was changed to avoid).
        let pulled = litebox_packager::oci::pull_layers_in_memory(image_ref, true)
            .map_err(|e| anyhow!("failed to pull OCI image {image_ref}: {e}"))?;
        RootfsSource::OciLayers {
            layers: pulled.layers,
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
                import_writable_layer(fs, resume_from)
                    .unwrap_or_else(|e| panic!("failed to import --resume-from archive: {e}"));
            });
        }

        match rootfs_source {
            RootfsSource::Tar { mmap } => shim_builder.default_fs(in_mem, mmap.data.into()),
            RootfsSource::OciLayers { layers } => {
                shim_builder.default_fs_multi_layer(in_mem, layers)
            }
        }
    };
    let initial_file_system = std::sync::Arc::new(initial_file_system);

    let shim = shim_builder.build();

    // `--gui`: open a real host window and wire the guest's DRM page-flips into it. The
    // `Presenter`'s own event loop (`Presenter::run`) blocks its calling thread for the window's
    // entire lifetime -- see `presentation.rs`'s module doc comment for why that thread must NOT
    // be this one (which goes on to call `run_thread` directly to execute the guest) -- so it gets
    // its own dedicated OS thread, matching the `net_worker` pattern just below. Frames are pushed
    // into it from `DrmSubsystem::page_flip` (inside the guest-execution thread, whichever thread
    // that ends up being for a given guest process) via the `FrameSender` handle, never by the
    // presenter thread reaching back into guest state itself.
    //
    // The `JoinHandle` is kept (not detached) so this function can wait for the WINDOW's own
    // lifetime, not just the guest's: a real GUI stays on screen after the program that drew into
    // it exits (exactly like a real X11 client disconnecting doesn't close the X server) -- without
    // this, `std::process::exit` below tears the presenter thread down the instant the guest
    // process finishes, which reliably raced the presenter's own async `resumed()`/first-frame
    // setup and produced a window that never actually appeared, confirmed live.
    // `--gui-hidden` implies `--gui`: it selects the window's INITIAL visibility, not whether the
    // presenter exists.
    let gui_requested = cli_args.gui || cli_args.gui_hidden;
    let gui_start_hidden = cli_args.gui_hidden;
    let gui_presenter_thread = gui_requested.then(|| {
        // `winit::EventLoop` (inside `Presenter`) is genuinely not `Send` on Windows -- it must be
        // BOTH created and run on the same OS thread, per winit's own platform requirement -- so
        // `Presenter::new()` happens INSIDE the spawned closure, not before it. The `FrameSender`
        // handle (which IS `Send`+`Clone`, see its own doc comment) crosses the thread boundary
        // the other way, via a one-shot channel, so `add_drm_flip_callback` below can be wired up
        // on the main thread without blocking on the presenter thread's own startup.
        let (sender_tx, sender_rx) = std::sync::mpsc::channel();
        let input_shim = shim.clone();
        // Default `std::thread::spawn` stack (1 MiB on Windows) is not enough headroom for this
        // thread's real work: `Presenter::new()`/`resumed()` create a real Win32 window plus a
        // wgpu `Instance`/`Adapter`/`Device`/`Surface`, and `Presenter::run` then drives winit's
        // event loop for the window's whole lifetime -- confirmed live as the actual overflowing
        // thread (a genuine SEH stack-overflow crash reproduced with a real guest DRM client,
        // `docs/wayland-drm-backend-probe/`, only with `--gui` set; the guest-execution thread
        // itself was ruled out first by reproducing successfully with `--gui` OMITTED). This is a
        // debug-build-specific cost (wgpu/winit's own deep, heavily-monomorphized generic call
        // chains are dramatically more stack-hungry unoptimized -- confirmed live: a `--release`
        // build never overflows even at 8 MiB, run repeatedly; a `dev` build still intermittently
        // overflowed at 64 MiB before this larger budget), not an unbounded-growth bug -- 256 MiB
        // is a deliberately generous fixed ceiling for a single always-present background thread,
        // not a per-guest or per-frame cost that could ever compound.
        const PRESENTER_THREAD_STACK_SIZE: usize = 256 * 1024 * 1024;
        let handle = std::thread::Builder::new()
            .name("litebox-gui-presenter".to_owned())
            .stack_size(PRESENTER_THREAD_STACK_SIZE)
            .spawn(move || {
            let mut presenter =
                match litebox_platform_windows_userland::presentation::Presenter::new() {
                    Ok(p) => p,
                    Err(e) => {
                        litebox_util_log::warn!(error:? = e; "failed to create GUI presenter");
                        return;
                    }
                };
            if gui_start_hidden {
                presenter = presenter.hidden_at_startup();
            }
            // Forward real keyboard/mouse events captured by winit into the guest's
            // `/dev/input/event0` queue, exactly mirroring how DRM page-flips are forwarded the
            // other way (guest -> host) via `add_drm_flip_callback` below. This is what makes a
            // `--gui` guest genuinely interactive rather than render-only.
            presenter.set_input_consumer(move |signal| match signal {
                litebox_platform_windows_userland::presentation::InputSignal::Key(code, value) => {
                    input_shim.push_input_key(code, value);
                }
                litebox_platform_windows_userland::presentation::InputSignal::Rel(code, value) => {
                    input_shim.push_input_rel(code, value);
                }
                litebox_platform_windows_userland::presentation::InputSignal::RelMotion(dx, dy) => {
                    input_shim.push_input_rel_motion(dx, dy);
                }
            });
            let _ = sender_tx.send(presenter.sender());
            if let Err(e) = presenter.run() {
                litebox_util_log::warn!(error:? = e; "GUI presenter event loop exited with an error");
            }
        })
            .expect("failed to spawn GUI presenter thread");
        if let Ok(sender) = sender_rx.recv() {
            // `LITEBOX_GUI_VISIBILITY_FILE`: a one-byte control file polled on a background
            // thread, letting the window be hidden and shown WHILE THE GUEST RUNS, from outside
            // the process (`echo 0 > file` hides, `echo 1 > file` shows).
            //
            // A control file rather than a keyboard shortcut or a signal: a shortcut cannot reach
            // a window that is currently hidden (the case that most needs it), and this runner
            // has no guest-facing control channel to overload. Polling rather than a filesystem
            // watch keeps it dependency-free and is trivially cheap at this interval; the file's
            // CONTENT is the desired state, not a toggle, so a repeated write is idempotent and a
            // caller never has to know the current state.
            if let Some(path) = std::env::var_os("LITEBOX_GUI_VISIBILITY_FILE") {
                let sender = sender.clone();
                std::thread::Builder::new()
                    .name("litebox-gui-visibility".to_owned())
                    .spawn(move || {
                        let mut last: Option<bool> = None;
                        loop {
                            if let Ok(text) = std::fs::read_to_string(&path) {
                                let want = match text.trim() {
                                    "0" | "hide" | "hidden" => Some(false),
                                    "1" | "show" | "visible" => Some(true),
                                    // Anything else (including a partially-written file caught
                                    // mid-write) is ignored rather than guessed at.
                                    _ => None,
                                };
                                if let Some(want) = want
                                    && last != Some(want)
                                {
                                    litebox_util_log::warn!(visible:? = want; "gui: visibility change requested");
                                    sender.set_visible(want);
                                    last = Some(want);
                                }
                            }
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                    })
                    .expect("failed to spawn GUI visibility watcher thread");
            }
            // Presentation ONLY. Frame capture is a separate observer registered below, not an
            // inline step here: when the flip slot held a single callback, capture had to be
            // smuggled into this closure (running before `sender.send` took ownership) so that a
            // stuck presenter could not starve it. Observers are additive now, so capture is
            // genuinely independent -- it no longer depends on this closure running at all, and
            // duplicating it here would write every frame twice.
            shim.add_drm_flip_callback(move |bytes, width, height, pitch, _pixel_format| {
                // The dumb buffer this slice points into is host-visible shared memory the guest
                // can `mmap` and keep writing to, and `DrmSubsystem::notify_flip_callback` (see
                // its own SAFETY note) unmaps it the instant every registered callback -- this one
                // included -- returns. `sender.send` only QUEUES the frame for later, async
                // presentation on a different thread, well after this callback (and the mapping
                // backing `bytes`) is gone -- so a copy out of `bytes` is genuinely required here,
                // not an incidental cost to shave off. What IS avoidable is a FRESH heap
                // allocation for that copy on every single flip: `take_free_buffer` reclaims the
                // `Vec` a previous, already-superseded frame no longer needs (see `FrameSender`'s
                // own doc comment for the free-list this comes from), so steady-state flipping at
                // a fixed resolution allocates only once, not per frame.
                let mut owned = sender.take_free_buffer();
                owned.clear();
                owned.extend_from_slice(bytes);
                let frame = litebox_platform_windows_userland::presentation::Frame {
                    width,
                    height,
                    pitch,
                    bytes: owned,
                };
                sender.send(frame);
            });
        }
        handle
    });
    // `LITEBOX_DUMP_FRAMES` verification path, registered INDEPENDENTLY of `--gui`:
    // frame capture must not require a working host window/wgpu presenter at all -- the presenter
    // thread is a genuinely separate, independently flaky subsystem (real Win32 window + wgpu
    // device/surface setup racing guest DRM startup, see the `--gui` doc comments above), and
    // tying frame verification to it means a presenter hang silently blocks every other
    // diagnostic too.
    //
    // This deliberately no longer excludes the `--gui` case. Flip observers are ADDITIVE (see
    // `add_drm_flip_callback`), so a windowed run can capture the very same frames it displays.
    // Previously the single-callback slot meant registering this one REPLACED the presenter's,
    // so the two had to be gated against each other -- which disabled capture in exactly the
    // situation it is most useful: proving what the on-screen window is actually showing, and
    // telling "the guest never drew" apart from "the guest drew and presentation lost it".
    if std::env::var_os("LITEBOX_DUMP_FRAMES").is_some() {
        shim.add_drm_flip_callback(move |bytes, width, height, pitch, _pixel_format| {
            let frame = litebox_platform_windows_userland::presentation::Frame {
                width,
                height,
                pitch,
                bytes: bytes.to_vec(),
            };
            litebox_platform_windows_userland::presentation::dump_frame_diagnostic(&frame);
        });
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
                match net_shim.perform_network_interaction() {
                    litebox::net::PlatformInteractionReinvocationAdvice::CallAgainImmediately => {}
                    litebox::net::PlatformInteractionReinvocationAdvice::WaitOnDeviceOrSocketInteraction { timeout } => {
                        break timeout;
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

    // `--gui`: keep the process (and its window) alive until the user closes it, matching real
    // desktop application behavior -- the guest program that drew the window's content has
    // already exited by this point (this line only runs after `program.process.wait()` above),
    // exactly like a real X11/Wayland client disconnecting from the display server does not close
    // the server or its windows. `Presenter::run`'s event loop only returns once
    // `WindowEvent::CloseRequested` fires (the user clicked the window's close button), so this
    // join is exactly the wait needed -- no polling, no arbitrary timeout.
    if let Some(handle) = gui_presenter_thread {
        let _ = handle.join();
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
    if !litebox_platform_windows_userland::process_fork::diag_process_fork_globalstate_enabled() {
        return;
    }
    let Some(tar_path) = std::env::var_os(
        litebox_platform_windows_userland::process_fork::FORK_CHILD_TAR_PATH_ENV_VAR,
    ) else {
        eprintln!(
            "[process_fork_diag] globalstate-probe (child): no tar path arrived via {}, skipping",
            litebox_platform_windows_userland::process_fork::FORK_CHILD_TAR_PATH_ENV_VAR
        );
        return;
    };
    eprintln!(
        "[process_fork_diag] globalstate-probe (child): attempting standalone GlobalState construction"
    );

    let tar_data = match mmapped_file(&tar_path) {
        Ok(f) => f.data,
        Err(e) => {
            eprintln!(
                "[process_fork_diag] globalstate-probe (child): failed to mmap tar at {}: {e}",
                PathBuf::from(&tar_path).display()
            );
            return;
        }
    };

    let platform = Platform::new();
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
    let fs = shim_builder.default_fs(in_mem, tar_data.into());
    let fs = std::sync::Arc::new(fs);

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

    diag_process_fork_vmem_adopt_probe(platform, &shim, fs);
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
) {
    use litebox_platform_windows_userland::process_fork as pf;

    if !pf::diag_process_fork_vmem_adopt_enabled() {
        return;
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

    let (tracked_count, tracked_brk) = page_manager.tracked_region_summary();
    let tracked = page_manager.tracked_regions();

    // Region-by-region equality, not merely a count: the point of this probe is proving the
    // reconstructed bookkeeping MATCHES the parent's real layout, boundaries and permissions
    // included (the CONTENTS at those addresses are already correct by construction, having been
    // `WriteProcessMemory`'d there verbatim -- this is the Rust-level bookkeeping catching up).
    let mut sorted_expected = expected.clone();
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

    diag_process_fork_task_resume_probe(platform, shim, fs, page_manager, relocations);
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
) {
    use litebox_platform_windows_userland::process_fork as pf;

    if !pf::diag_process_fork_task_resume_enabled() {
        return;
    }
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
    std::thread::spawn(move || {
        const DEFAULT_TIMEOUT: core::time::Duration = core::time::Duration::from_micros(100);
        const MAX_TIMEOUT: core::time::Duration = core::time::Duration::from_millis(1);
        loop {
            let timeout = loop {
                match net_shim.perform_network_interaction() {
                    litebox::net::PlatformInteractionReinvocationAdvice::CallAgainImmediately => {}
                    litebox::net::PlatformInteractionReinvocationAdvice::WaitOnDeviceOrSocketInteraction { timeout } => {
                        break timeout;
                    }
                }
            };
            platform.wait_on_tun(Some(timeout.unwrap_or(DEFAULT_TIMEOUT).min(MAX_TIMEOUT)));
        }
    });

    eprintln!(
        "[process_fork_diag] task-resume-probe (child, winpid={}): built Task, set fs_base={:#x}, calling \
         run_thread with rip={:#x} rsp={:#x} -- entering real guest execution",
        std::process::id(),
        gprs.fs_base,
        ctx.rip,
        ctx.rsp
    );
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
    if let Some(tar_path) = std::env::var_os(pf::FORK_CHILD_TAR_PATH_ENV_VAR) {
        let export_path = pf::cross_process_writable_export_path(
            std::path::Path::new(&tar_path),
            std::process::id(),
        );
        match export_writable_layer(&fs_for_export, &export_path) {
            Ok(()) => eprintln!(
                "[process_fork_diag] task-resume-probe (child): exported writable layer to {}",
                export_path.display()
            ),
            Err(e) => eprintln!(
                "[process_fork_diag] task-resume-probe (child): failed to export writable layer to {}: {e}",
                export_path.display()
            ),
        }
    }

    eprintln!(
        "[process_fork_diag] task-resume-probe (child): exiting with encoded status {encoded:#x}"
    );
    std::process::exit(encoded.cast_signed());
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
            tar::EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .map_err(|e| anyhow!("invalid symlink target for {path}: {e}"))?
                    .ok_or_else(|| anyhow!("symlink entry {path} has no target"))?
                    .to_string_lossy()
                    .into_owned();
                fs.symlink(&*target, &*path)
                    .map_err(|e| anyhow!("failed to recreate symlink {path}: {e:?}"))?;
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
