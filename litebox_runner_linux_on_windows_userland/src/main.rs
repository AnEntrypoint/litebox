// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Restrict this crate to only work on Windows. For now, we are restricting this to only x86-64
// Windows, but we _may_ allow for more in the future, if we find it useful to do so.

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn main() -> anyhow::Result<()> {
    use clap::Parser as _;
    use litebox_runner_linux_on_windows_userland::CliArgs;

    let raw_args: Vec<String> = std::env::args().skip(1).collect();

    // `--session-daemon <runner-exe>` is this binary's own daemon-mode entry point (spawned
    // detached by `session_cli::dispatch`'s `connect_or_spawn` the first time a `session ...`
    // subcommand finds no daemon listening) -- intercepted before `CliArgs::parse()` for the same
    // reason `session ...` itself is: it isn't shaped like a normal guest-run invocation.
    if raw_args.first().map(String::as_str) == Some("--session-daemon") {
        let runner_exe = raw_args.get(1).cloned().unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        });
        litebox_session_daemon::run_daemon(runner_exe);
    }

    // `session start/send/screen/history/list/kill` -- the phase-3 CLI subcommands. Handled on
    // the raw argv (not through `CliArgs`) because `CliArgs`'s `program_and_arguments` is a
    // `trailing_var_arg` positional that clap cannot cleanly coexist with a `session` subcommand
    // in one derive struct. `dispatch` calls `std::process::exit` and never returns when it
    // handles the invocation; it's a no-op for every other argv shape, so normal guest runs fall
    // through unaffected.
    litebox_runner_linux_on_windows_userland::session_cli::dispatch(&raw_args);

    litebox_platform_windows_userland::install_memcpy_watch_from_env();

    if litebox_platform_windows_userland::process_fork::is_wait4_probe_child() {
        litebox_platform_windows_userland::process_fork::run_wait4_probe_child();
    }

    if litebox_platform_windows_userland::process_fork::is_diagnostic_resume_child() {
        // Runs BEFORE `run_diagnostic_resume_child()` (which prints `RESUME_CHILD_READY_MARKER`
        // and, for the real-resume probe, immediately parks this thread for cross-process
        // injection without ever returning to this call site) -- the parent's own
        // `resume_and_observe`/`inject_and_observe` stop reading this child's stdout pipe the
        // moment they see that marker, so any diagnostic output emitted AFTER it would never
        // reach the parent. See `diag_process_fork_globalstate_probe`'s own doc comment.
        litebox_runner_linux_on_windows_userland::diag_process_fork_globalstate_probe();
        litebox_platform_windows_userland::process_fork::run_diagnostic_resume_child();
        return Ok(());
    }

    litebox_runner_linux_on_windows_userland::run(CliArgs::parse())
}

#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn main() {
    eprintln!("This program is only supported on Windows x86_64");
    std::process::exit(1);
}
