// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Runner for [`litebox_syscall_rewriter`]

use clap::Parser;
use std::io::Read as _;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::PathBuf;

/// Rewrite ELF files to hook syscalls
#[derive(Parser, Debug)]
struct CliArgs {
    /// Path to input ELF binary
    input_binary: PathBuf,
    /// Path to output the generated binary (default = <INPUT_BINARY>.hooked)
    #[arg(short = 'o', long = "output")]
    output_binary: Option<PathBuf>,
    /// Absolute address to set in the trampoline (default = 0)
    #[arg(long)]
    trampoline_addr: Option<u64>,
    /// Accept an x86-64 binary containing syscall sites the patcher could not redirect
    /// (`InsufficientBytesBeforeOrAfter` -- too little surrounding code to fit the redirect,
    /// an occasional data-dependent codegen constraint) instead of rejecting it outright. Each
    /// such site is still replaced with a trapping `icebp; hlt` so it faults rather than
    /// escaping to the host kernel if ever reached -- this flag only changes whether the
    /// resulting binary is written out for inspection/use, never that safety property. Prints
    /// each trapped site's address to stderr so the caller can judge reachability before
    /// trusting the output. Not supported for AArch64 binaries (see
    /// `litebox_syscall_rewriter::hook_syscalls_in_elf_allow_trapped_sites`'s own doc comment
    /// for why).
    #[arg(long)]
    allow_trapped_sites: bool,
}

fn copy_file_permissions(
    input_file: &std::fs::File,
    output_file: &std::fs::File,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        output_file.set_permissions(std::fs::Permissions::from_mode(
            input_file.metadata()?.mode(),
        ))?;
    }
    #[cfg(windows)]
    {
        let input_metadata = input_file.metadata()?;
        let perms = input_metadata.permissions();
        output_file.set_permissions(perms)?;
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli_args = CliArgs::parse();
    let mut input_binary = std::fs::File::open(&cli_args.input_binary)?;
    let mut input_binary_bytes = vec![];
    input_binary.read_to_end(&mut input_binary_bytes)?;
    let output_binary = if cli_args.allow_trapped_sites {
        let (output, skipped) = litebox_syscall_rewriter::hook_syscalls_in_elf_allow_trapped_sites(
            &input_binary_bytes,
            cli_args.trampoline_addr,
        )?;
        for addr in &skipped {
            eprintln!("warning: trapped unpatchable syscall site at {addr:#x} (icebp;hlt)");
        }
        output
    } else {
        litebox_syscall_rewriter::hook_syscalls_in_elf(
            &input_binary_bytes,
            cli_args.trampoline_addr,
        )?
    };
    let output_path = cli_args.output_binary.unwrap_or_else(|| {
        cli_args.input_binary.with_file_name(
            cli_args
                .input_binary
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
                + ".hooked",
        )
    });
    let mut file = std::fs::File::create(output_path)?;
    copy_file_permissions(&input_binary, &file)?;
    file.write_all(&output_binary)?;
    Ok(())
}
