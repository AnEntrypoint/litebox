// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use litebox::fs::{FileSystem as _, Mode};
use litebox_platform_linux_userland::LinuxUserland as Platform;
use memmap2::Mmap;
use std::os::linux::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

extern crate alloc;

// Use a stable non-root guest identity instead of mirroring the host user. This keeps shim
// credentials aligned with the in-memory filesystem default user and avoids truncating high host IDs.
const DEFAULT_GUEST_UID: u16 = 1000;
const DEFAULT_GUEST_GID: u16 = 1000;

/// Run Linux programs with LiteBox on unmodified Linux
///
/// Detailed logging can be controlled via the `LITEBOX_LOG` environment variable. For example:
/// - `LITEBOX_LOG=debug` to show debug and higher level logs
/// - `LITEBOX_LOG=litebox=debug,litebox::fs=trace` for multiple filters at different levels
#[derive(Parser, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct CliArgs {
    /// The program and arguments passed to it (e.g., `python3 --version`).
    ///
    /// By default this is a path on the host filesystem. When --program-from-tar
    /// is set, it refers to a path inside the tar archive instead.
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
    /// Pre-fill files into the initial file system state
    // TODO: Might want to extend this to support full directories at some point?
    #[arg(long = "insert-file", value_hint = clap::ValueHint::FilePath,
          requires = "unstable", help_heading = "Unstable Options")]
    pub insert_files: Vec<PathBuf>,
    /// Pre-fill the files in this tar file into the initial file system state
    #[arg(long = "initial-files", value_name = "PATH_TO_TAR", value_hint = clap::ValueHint::FilePath,
          requires = "unstable", help_heading = "Unstable Options")]
    pub initial_files: Option<PathBuf>,
    /// Apply syscall-rewriter to the ELF file before running it
    ///
    /// This is meant as a convenience feature; real deployments would likely prefer ahead-of-time
    /// rewrite things to amortize costs.
    #[arg(
        long = "rewrite-syscalls",
        requires = "unstable",
        help_heading = "Unstable Options"
    )]
    pub rewrite_syscalls: bool,
    /// Connect to a TUN device with this name
    #[arg(
        long = "tun-device-name",
        requires = "unstable",
        help_heading = "Unstable Options"
    )]
    pub tun_device_name: Option<String>,
    /// Load the program binary from the tar file instead of from the host filesystem.
    ///
    /// When set, the program path refers to a path inside the tar filesystem.
    /// The binary must already be rewritten (incompatible with --rewrite-syscalls).
    /// This is used by `litebox-packager` to create fully self-contained tar bundles.
    #[arg(
        long = "program-from-tar",
        requires_all = ["unstable", "initial_files"],
        conflicts_with = "rewrite_syscalls",
        help_heading = "Unstable Options"
    )]
    pub program_from_tar: bool,
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
    abs_path: PathBuf,
}

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

/// Run Linux programs with LiteBox on unmodified Linux
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

    if !cli_args.insert_files.is_empty() {
        unimplemented!(
            "this should (hopefully soon) have a nicer interface to support loading in files"
        )
    }

    // When loading from tar, the program path is a guest-internal path and must
    // be absolute — LiteBox does not resolve programs via PATH.
    if cli_args.program_from_tar && !cli_args.program_and_arguments[0].starts_with('/') {
        anyhow::bail!(
            "--program-from-tar requires an absolute path (e.g., /usr/bin/ls), \
             got: {}",
            cli_args.program_and_arguments[0]
        );
    }

    let mut cow_eligible_regions: Vec<MmappedFile> = Vec::new();

    // When --program-from-tar is set, the program binary is already in the tar file,
    // so we skip reading it from the host filesystem and skip extracting ancestor modes.
    #[allow(clippy::type_complexity)]
    let (ancestor_modes_and_users, prog_data): (
        Vec<(litebox::fs::Mode, u32)>,
        Option<alloc::borrow::Cow<'static, [u8]>>,
    ) = if cli_args.program_from_tar {
        (Vec::new(), None)
    } else {
        let prog = std::path::absolute(Path::new(&cli_args.program_and_arguments[0])).unwrap();
        if !prog.exists() {
            let mut msg = format!("program not found on host filesystem: {}", prog.display());
            if cli_args.initial_files.is_some() {
                msg.push_str(
                    "\nhint: if the program is inside the tar archive, \
                     add --program-from-tar",
                );
            }
            anyhow::bail!(msg);
        }
        let ancestors: Vec<_> = prog.ancestors().collect();
        let modes: Vec<_> = ancestors
            .into_iter()
            .rev()
            .skip(1)
            .map(|path| {
                let metadata = path.metadata().unwrap();
                (
                    litebox::fs::Mode::from_bits(metadata.st_mode()).unwrap(),
                    metadata.st_uid(),
                )
            })
            .collect();
        let file = mmapped_file(&prog)?;
        let data = if cli_args.rewrite_syscalls {
            let rewritten = litebox_syscall_rewriter::hook_syscalls_in_elf(file.data, None)
                .with_context(|| format!("failed to rewrite {}", prog.display()))?;
            rewritten.into()
        } else {
            let data = file.data.into();
            cow_eligible_regions.push(file);
            data
        };
        (modes, Some(data))
    };
    let tar_data: &'static [u8] = if let Some(tar_file) = cli_args.initial_files.as_ref() {
        if tar_file.extension().and_then(|x| x.to_str()) != Some("tar") {
            anyhow::bail!("Expected a .tar file, found {}", tar_file.display());
        }
        mmapped_file(tar_file)?.data
    } else {
        litebox::fs::tar_ro::EMPTY_TAR_FILE
    };

    // TODO(jb): Clean up platform initialization once we have https://github.com/MSRSSP/litebox/issues/24
    let platform = Platform::new(cli_args.tun_device_name.as_deref());

    for file in cow_eligible_regions {
        platform.register_cow_region(file.data, file.abs_path);
    }

    let shim_builder = litebox_shim_linux::LinuxShimBuilder::new(platform);
    let litebox = shim_builder.litebox();
    // SAFETY: `gettid` takes no pointer arguments and has no Rust-side aliasing requirements.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) }
        .try_into()
        .context("failed to convert gettid result to i32")?;
    // SAFETY: `getppid` takes no arguments and has no Rust-side aliasing requirements.
    let ppid = unsafe { libc::getppid() };
    let task_params = litebox_common_linux::TaskParams {
        pid: tid,
        ppid,
        uid: u32::from(DEFAULT_GUEST_UID),
        euid: u32::from(DEFAULT_GUEST_UID),
        gid: u32::from(DEFAULT_GUEST_GID),
        egid: u32::from(DEFAULT_GUEST_GID),
    };
    let initial_file_system = {
        let mut in_mem = litebox::fs::in_mem::FileSystem::new(litebox);

        // When loading the program from the tar, we don't need to create ancestor
        // directories or write the program binary into the in-memory FS -- the program
        // is already in the tar layer.
        if let Some(prog_data) = prog_data {
            let prog = std::path::absolute(Path::new(&cli_args.program_and_arguments[0])).unwrap();
            let ancestors: Vec<_> = prog.ancestors().collect();
            let chown_to_initial_user = |fs: &mut litebox::fs::in_mem::FileSystem<Platform>,
                                         path: &Path| {
                fs.chown(
                    path.to_str().unwrap(),
                    Some(DEFAULT_GUEST_UID),
                    Some(DEFAULT_GUEST_GID),
                )
                .unwrap();
            };
            let mut prev_user = 0;
            for (path, &mode_and_user) in ancestors
                .into_iter()
                .skip(1)
                .rev()
                .skip(1)
                .zip(&ancestor_modes_and_users)
            {
                if prev_user == 0 {
                    // require root user
                    in_mem.with_root_privileges(|fs| {
                        fs.mkdir(path.to_str().unwrap(), mode_and_user.0).unwrap();
                        if mode_and_user.1 != 0 {
                            chown_to_initial_user(fs, path);
                        }
                    });
                } else {
                    in_mem
                        .mkdir(path.to_str().unwrap(), mode_and_user.0)
                        .unwrap();
                }
                prev_user = mode_and_user.1;
            }

            let open_file = |fs: &mut litebox::fs::in_mem::FileSystem<Platform>, path, mode| {
                let fd = fs
                    .open(
                        path,
                        litebox::fs::OFlags::WRONLY | litebox::fs::OFlags::CREAT,
                        mode,
                    )
                    .unwrap();
                fs.initialize_primarily_read_heavy_file(&fd, prog_data);
                fs.close(&fd).unwrap();
            };
            let last = ancestor_modes_and_users.last().ok_or_else(|| {
                anyhow!("program path has no ancestor directories (is it the root path?)")
            })?;
            if prev_user == 0 {
                in_mem.with_root_privileges(|fs| {
                    open_file(fs, prog.to_str().unwrap(), last.0);
                    if last.1 != 0 {
                        chown_to_initial_user(fs, &prog);
                    }
                });
            } else {
                open_file(&mut in_mem, prog.to_str().unwrap(), last.0);
            }
        }
        in_mem.with_root_privileges(|fs| {
            let mode = Mode::RWXU | Mode::RWXG | Mode::RWXO;
            if let Err(err) = fs.mkdir("/tmp", mode) {
                match err {
                    litebox::fs::errors::MkdirError::AlreadyExists => {
                        fs.chmod("/tmp", mode).expect("Failed to call chmod");
                    }
                    _ => panic!(),
                }
            }
        });

        if let Some(resume_from) = &cli_args.resume_from {
            import_writable_layer(&mut in_mem, resume_from)
                .unwrap_or_else(|e| panic!("failed to import --resume-from archive: {e}"));
        }

        shim_builder.default_fs(in_mem, tar_data.into())
    };

    // We need to get the file path before enabling seccomp.
    // For --program-from-tar the path is already validated as absolute above,
    // so use it directly instead of resolving against the host CWD.
    let prog = if cli_args.program_from_tar {
        PathBuf::from(&cli_args.program_and_arguments[0])
    } else {
        std::path::absolute(Path::new(&cli_args.program_and_arguments[0])).unwrap()
    };
    let prog_path = prog.to_str().ok_or_else(|| {
        anyhow!(
            "Could not convert program path {:?} to a string",
            cli_args.program_and_arguments[0]
        )
    })?;

    let initial_file_system = std::sync::Arc::new(initial_file_system);
    let fs_for_export = cli_args
        .export_writable_layer
        .is_some()
        .then(|| initial_file_system.clone());
    // Open the export file now, before `enable_seccomp_filter()` below -- unlike the Windows
    // runner (which has no such restriction), this process's own seccomp-bpf filter stays active
    // for the rest of its lifetime, including after the guest exits, and only allows a narrow
    // O_RDONLY case of `open`/`openat`. Opening for write has to happen before that filter is
    // installed; only ordinary `write()`s (always allowed) are needed on it afterward.
    let export_file = cli_args.export_writable_layer.as_ref().map(|export_path| {
        std::fs::File::create(export_path)
            .unwrap_or_else(|e| panic!("failed to create {}: {e}", export_path.display()))
    });

    let shim = shim_builder.build();

    let shutdown = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
    let net_worker = if cli_args.tun_device_name.is_some() {
        let shim = shim.clone();
        let shutdown_clone = shutdown.clone();
        let child = litebox_platform_linux_userland::spawn_host_thread(move || {
            const DEFAULT_TIMEOUT: core::time::Duration = core::time::Duration::from_micros(100);
            const MAX_TIMEOUT: core::time::Duration = core::time::Duration::from_millis(1);
            pin_thread_to_cpu(0);

            while !shutdown_clone.load(core::sync::atomic::Ordering::Relaxed) {
                let timeout = loop {
                    match shim.perform_network_interaction() {
                        litebox::net::PlatformInteractionReinvocationAdvice::CallAgainImmediately => {}
                        litebox::net::PlatformInteractionReinvocationAdvice::WaitOnDeviceOrSocketInteraction{ timeout } => {
                            break timeout;
                        }
                    }
                };
                // TODO: We only wait for ingress packets on the TUN device and thus may block processing egress packets for up to `timeout`.
                // Set a maximum timeout to ensure we don't wait too long. Alternatively, shim could notify us when there are egress packets to process,
                // but that would require more invasive changes.
                platform.wait_on_tun(Some(timeout.unwrap_or(DEFAULT_TIMEOUT).min(MAX_TIMEOUT)));
            }
            // Final flush
            // TODO: keep running until all sockets are closed?
            while shim.perform_network_interaction().call_again_immediately() {}
        });
        Some(child)
    } else {
        None
    };

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

    litebox_platform_linux_userland::LinuxUserland::enable_seccomp_filter();

    let program = shim.load_program(initial_file_system, task_params, prog_path, argv, envp)?;

    #[cfg(feature = "lock_tracing")]
    litebox::sync::start_recording();

    unsafe {
        litebox_platform_linux_userland::run_thread(
            program.entrypoints,
            &mut litebox_common_linux::PtRegs::default(),
        );
    }

    #[cfg(feature = "lock_tracing")]
    {
        litebox::sync::stop_recording();
        let events = litebox::sync::flush_to_jsonl();
        if !events.is_empty() {
            use std::io::Write;
            if let Ok(mut file) = std::fs::File::create("/tmp/locks.jsonl") {
                for line in &events {
                    let _ = writeln!(file, "{line}");
                }
            }
        }
    }

    let exit_code = program.process.wait();

    if let Some(export_file) = export_file {
        let fs = fs_for_export.expect("fs_for_export set whenever export_writable_layer is set");
        export_writable_layer(&fs, export_file)
            .unwrap_or_else(|e| panic!("failed to write --export-writable-layer archive: {e}"));
    }

    if let Some(net_worker) = net_worker {
        shutdown.store(true, core::sync::atomic::Ordering::Relaxed);
        net_worker.join().unwrap();
    }
    std::process::exit(exit_code)
}

/// Export the writable upper layer of a layered file system (every file the guest created or
/// modified this run) as a tar archive to `export_file`, for a later run's `--resume-from`.
///
/// Only the upper layer is walked -- the read-only lower layer (the packaged base rootfs) is
/// never re-exported, so the archive is a delta, not a full rootfs snapshot.
///
/// `export_file` must already be open for writing: on this platform (unlike Windows), the
/// running process's own seccomp-bpf filter (see `enable_seccomp_filter`) stays active for the
/// rest of the process's lifetime once installed and only allows a narrow `O_RDONLY` case of
/// `open`/`openat`, so opening the export path has to happen before that filter goes up -- see
/// this function's call site in `run()`.
fn export_writable_layer<Upper, Lower>(
    fs: &litebox::fs::layered::FileSystem<Platform, Upper, Lower>,
    export_file: std::fs::File,
) -> Result<()>
where
    Upper: litebox::fs::FileSystem,
    Lower: litebox::fs::FileSystem,
{
    let entries = litebox::fs::export::export_all(fs.upper())
        .map_err(|e| anyhow!("failed to walk writable layer: {e:?}"))?;

    let mut builder = tar::Builder::new(export_file);
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
        .map_err(|e| anyhow!("failed to finalize export archive: {e}"))?;
    Ok(())
}

/// Seed `fs`'s writable layer from a tar archive previously produced by
/// [`export_writable_layer`], resuming a prior session's on-disk state.
fn import_writable_layer(
    fs: &mut litebox::fs::in_mem::FileSystem<Platform>,
    resume_from: &Path,
) -> Result<()> {
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
                // this directory (e.g. `/tmp`).
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

/// Pin the current thread to a specific CPU core
fn pin_thread_to_cpu(cpu: usize) {
    unsafe {
        let mut set = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);

        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &raw const set) != 0 {
            eprintln!("Warning: Failed to pin thread to CPU core {cpu}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates `/tmp` world-writable, exactly like `run()`'s own bootstrap step (see the
    /// `in_mem.with_root_privileges(|fs| { ... fs.mkdir("/tmp", ...) ... })` block above) --
    /// this is the realistic writable location a guest process (which always runs as a single
    /// fixed, non-root uid/gid in this shim) can actually create content under, both on a fresh
    /// run and after a `--resume-from` import.
    fn bootstrap_tmp(fs: &mut litebox::fs::in_mem::FileSystem<Platform>) {
        fs.with_root_privileges(|fs| {
            fs.mkdir("/tmp", Mode::RWXU | Mode::RWXG | Mode::RWXO)
                .unwrap();
        });
    }

    /// Regression test for the `--export-writable-layer`/`--resume-from` round trip: writes a
    /// file (as the guest's own non-root identity, under `/tmp`, matching how a real guest
    /// process can actually write) into a fresh guest filesystem's writable upper layer, exports
    /// that layer to a tar archive, then imports it into a brand new filesystem (simulating a
    /// later, independent run resuming from it, including the same `/tmp` bootstrap step `run()`
    /// always does before importing) and checks the file, its directory, and a symlink all
    /// survive with the right contents/target. This exercises the exact
    /// `export_writable_layer`/`import_writable_layer` logic wired into `run()`, without needing
    /// a full CLI invocation or a compiled guest binary.
    #[test]
    fn export_then_import_writable_layer_round_trips_files_dirs_and_symlinks() {
        let platform = Platform::new(None);
        let shim_builder = litebox_shim_linux::LinuxShimBuilder::new(platform);
        let litebox = shim_builder.litebox();

        let mut in_mem = litebox::fs::in_mem::FileSystem::new(litebox);
        bootstrap_tmp(&mut in_mem);
        in_mem
            .mkdir("/tmp/data", Mode::RWXU | Mode::RWXG | Mode::RWXO)
            .unwrap();
        let fd = in_mem
            .open(
                "/tmp/data/greeting.txt",
                litebox::fs::OFlags::WRONLY | litebox::fs::OFlags::CREAT,
                Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH,
            )
            .unwrap();
        in_mem.write(&fd, b"hello from a prior run", None).unwrap();
        in_mem.close(&fd).unwrap();
        in_mem
            .symlink("greeting.txt", "/tmp/data/link_to_greeting")
            .unwrap();
        let fs = shim_builder.default_fs(in_mem, litebox::fs::tar_ro::EMPTY_TAR_FILE.into());

        let export_path = std::env::temp_dir().join(format!(
            "litebox_export_import_roundtrip_test_{}.tar",
            std::process::id()
        ));
        let export_file = std::fs::File::create(&export_path).unwrap();
        export_writable_layer(&fs, export_file).unwrap();

        let platform2 = Platform::new(None);
        let shim_builder2 = litebox_shim_linux::LinuxShimBuilder::new(platform2);
        let litebox2 = shim_builder2.litebox();
        let mut fresh_in_mem = litebox::fs::in_mem::FileSystem::new(litebox2);
        bootstrap_tmp(&mut fresh_in_mem);
        import_writable_layer(&mut fresh_in_mem, &export_path).unwrap();

        let fd = fresh_in_mem
            .open(
                "/tmp/data/greeting.txt",
                litebox::fs::OFlags::RDONLY,
                Mode::empty(),
            )
            .unwrap();
        let mut buf = [0u8; 64];
        let n = fresh_in_mem.read(&fd, &mut buf, None).unwrap();
        assert_eq!(&buf[..n], b"hello from a prior run");
        fresh_in_mem.close(&fd).unwrap();

        let link_target = fresh_in_mem
            .read_link("/tmp/data/link_to_greeting")
            .unwrap();
        assert_eq!(link_target, "greeting.txt");

        std::fs::remove_file(&export_path).unwrap();
    }
}
