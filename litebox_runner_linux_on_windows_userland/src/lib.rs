// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Restrict this crate to only work on Windows. For now, we are restricting this to only x86-64
// Windows, but we _may_ allow for more in the future, if we find it useful to do so.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

extern crate alloc;

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
    #[arg(long = "initial-files", value_name = "PATH_TO_TAR", value_hint = clap::ValueHint::FilePath)]
    pub initial_files: PathBuf,
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
}

struct MmappedFile {
    data: &'static [u8],
    #[expect(
        dead_code,
        reason = "kept for parity with the native-Linux runner's identical helper"
    )]
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

/// Run Linux programs with LiteBox on unmodified Windows
///
/// # Panics
///
/// Can panic if any particulars of the environment are not set up as expected. Ideally, would not
/// panic. If it does actually panic, then ping the authors of LiteBox, and likely a better error
/// message could be thrown instead.
pub fn run(cli_args: CliArgs) -> Result<()> {
    tracing_subscriber::fmt()
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .with_level(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_env_var("LITEBOX_LOG")
                .from_env_lossy(),
        )
        .init();

    let tar_file = &cli_args.initial_files;
    if tar_file.extension().and_then(|x| x.to_str()) != Some("tar") {
        anyhow::bail!("Expected a .tar file, found {}", tar_file.display());
    }
    // Pass 136: carry this run's tar path across the CreateProcessW-spawned diagnostic-fork-
    // child boundary (inherited automatically via `lpEnvironment: null`) so a child built with
    // `LITEBOX_DIAG_PROCESS_FORK_GLOBALSTATE=1` can remount the SAME rootfs a real GlobalState-
    // reconstruction probe needs -- see `process_fork::FORK_CHILD_TAR_PATH_ENV_VAR`'s doc
    // comment. Set unconditionally (cheap, a single env var) rather than gated behind the probe's
    // own flag, matching this module's existing precedent of computing cheap diagnostic inputs
    // unconditionally while gating only the logging/behavior that consumes them.
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
                "--export-writable-layer must not point at the same file as --initial-files ({}): \
                 the rootfs archive stays memory-mapped for the whole run, so exporting onto it \
                 would try to overwrite a file that's still open for reading",
                initial_files_abs.display()
            );
        }
    }
    // Memory-mapped, not heap-copied: every concurrent runner process reading
    // the same rootfs archive shares its physical pages via the OS page cache
    // instead of each holding a private copy.
    let tar_data = mmapped_file(tar_file)?.data;

    let platform = Platform::new();
    let shim_builder = litebox_shim_linux::LinuxShimBuilder::new(platform);
    let litebox = shim_builder.litebox();

    // The program path is a Unix-style path inside the tar archive.
    let prog_path = &cli_args.program_and_arguments[0];

    let initial_file_system = {
        let mut in_mem = litebox::fs::in_mem::FileSystem::new(litebox);
        // The guest's persistent identity is root, matching `Platform::init_task`'s credentials
        // below and matching how a real container's initial process runs (a fresh OCI rootfs's
        // `/`, `/etc`, `/lib`, etc. are root-owned at mode 0755, not world-writable). Without
        // this, `getuid()` would report root while the file system's own permission checks still
        // enforced a mismatched non-root identity, breaking any program (e.g. `apk`) that needs
        // to write into the rootfs's root-owned directories.
        in_mem.set_default_user(0, 0);
        in_mem.with_root_privileges(|fs| {
            use litebox::fs::FileSystem as _;
            fs.mkdir(
                "/tmp",
                litebox::fs::Mode::RWXU | litebox::fs::Mode::RWXG | litebox::fs::Mode::RWXO,
            )
            .unwrap();
            fs.chown("/tmp", Some(1000), Some(1000)).unwrap();

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

            if let Some(resume_from) = &cli_args.resume_from {
                import_writable_layer(fs, resume_from)
                    .unwrap_or_else(|e| panic!("failed to import --resume-from archive: {e}"));
            }
        });

        shim_builder.default_fs(in_mem, tar_data.into())
    };
    let initial_file_system = std::sync::Arc::new(initial_file_system);

    let shim = shim_builder.build();

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
                std::ffi::CString::new(k.bytes().chain(*b"=").chain(v.bytes()).collect::<Vec<u8>>())
                    .unwrap()
            }))
            .collect()
    } else {
        envp
    };

    let fs_for_export = cli_args
        .export_writable_layer
        .is_some()
        .then(|| initial_file_system.clone());

    let program = shim
        .load_program(
            initial_file_system,
            platform.init_task(),
            prog_path,
            argv,
            envp,
        )
        .unwrap();
    unsafe {
        litebox_platform_windows_userland::run_thread(
            program.entrypoints,
            &mut litebox_common_linux::PtRegs::default(),
        );
    }
    let exit_code = program.process.wait();

    if let Some(export_path) = &cli_args.export_writable_layer {
        let fs = fs_for_export.expect("fs_for_export set whenever export_writable_layer is set");
        export_writable_layer(&fs, export_path)
            .unwrap_or_else(|e| panic!("failed to write --export-writable-layer archive: {e}"));
    }

    shutdown.store(true, core::sync::atomic::Ordering::Relaxed);
    // `wait_on_tun`'s timeout is always capped to `MAX_TIMEOUT` (1ms), so the worker re-checks
    // `shutdown` frequently even while otherwise idle; the join below returns promptly.
    let _ = net_worker.join();

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

    let in_mem = litebox::fs::in_mem::FileSystem::new(litebox);
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

    diag_process_fork_vmem_adopt_probe(&shim, fs);
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

    diag_process_fork_task_resume_probe(shim, fs, page_manager);
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
    shim: &litebox_shim_linux::LinuxShim<Platform, litebox_shim_linux::DefaultFS<Platform>>,
    fs: std::sync::Arc<litebox_shim_linux::DefaultFS<Platform>>,
    page_manager: litebox::mm::PageManager<Platform, { litebox::mm::linux::PAGE_SIZE }>,
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

    eprintln!(
        "[process_fork_diag] task-resume-probe (child): built Task, calling run_thread with \
         rip={:#x} rsp={:#x} -- entering real guest execution",
        ctx.rip, ctx.rsp
    );
    unsafe {
        litebox_platform_windows_userland::run_thread(entrypoints, &mut ctx);
    }
    eprintln!(
        "[process_fork_diag] task-resume-probe (child): run_thread returned (guest thread terminated)"
    );
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
