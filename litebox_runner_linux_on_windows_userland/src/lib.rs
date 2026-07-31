// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Restrict this crate to only work on Windows. For now, we are restricting this to only x86-64
// Windows, but we _may_ allow for more in the future, if we find it useful to do so.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

extern crate alloc;

use anyhow::{Result, anyhow};
use clap::Parser;
use litebox_platform_windows_userland::WindowsUserland as Platform;
use std::path::PathBuf;

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
    let tar_data = std::fs::read(tar_file)
        .map_err(|e| anyhow!("Could not read tar file at {}: {}", tar_file.display(), e))?;

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

    shutdown.store(true, core::sync::atomic::Ordering::Relaxed);
    // `wait_on_tun`'s timeout is always capped to `MAX_TIMEOUT` (1ms), so the worker re-checks
    // `shutdown` frequently even while otherwise idle; the join below returns promptly.
    let _ = net_worker.join();

    std::process::exit(exit_code)
}
