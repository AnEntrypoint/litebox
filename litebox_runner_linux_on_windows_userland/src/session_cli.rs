// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! `session start/send/screen/history/list/kill` -- the thin, agent-facing CLI subcommands
//! described in `docs/session-daemon-design.md`'s "CLI subcommand surface" section. Each
//! subcommand is a short-lived process: connect to the daemon (auto-spawning it via
//! `--session-daemon` if none is listening), send one request over the same named-pipe wire
//! protocol `litebox_session_daemon::examples::session_client` already live-verified, print the
//! result, exit. No state survives between invocations except what lives in the daemon.
//!
//! Dispatched from `main()` before `CliArgs::parse()` -- the top-level `CliArgs` uses a
//! `trailing_var_arg` positional (`program_and_arguments`) that clap cannot cleanly coexist with
//! a `session` subcommand inside one derive struct, so `session ...` is intercepted on the raw
//! `argv` first, matching how `--session-daemon` (this binary's own daemon-mode entry point) is
//! already intercepted the same way.

use clap::{Parser, Subcommand};
use litebox_session_daemon::client::connect_or_spawn;
use litebox_session_daemon::protocol::Response;

/// Handles a `session ...` invocation and exits the process; a no-op for every other argv shape
/// (`main()` falls through to the normal guest-run path in that case).
pub fn dispatch(args: &[String]) {
    if args.first().map(String::as_str) != Some("session") {
        return;
    }
    std::process::exit(run(&args[1..]));
}

fn current_exe_string() -> String {
    std::env::current_exe()
        .expect("current_exe() must succeed")
        .to_string_lossy()
        .into_owned()
}

#[derive(Parser)]
#[command(name = "litebox_runner_linux_on_windows_userland session", no_binary_name = true)]
struct SessionCli {
    #[command(subcommand)]
    sub: SessionSub,
}

#[derive(Subcommand)]
enum SessionSub {
    /// Start a new session running `program` inside `rootfs`.
    Start {
        #[arg(long, alias = "initial-files")]
        rootfs: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        program_and_args: Vec<String>,
    },
    /// Send a key-string to a running session.
    Send { session_id: String, key_string: String },
    /// Print a session's current screen contents.
    Screen {
        session_id: String,
        /// Not yet exposed by the daemon's `GetScreen` response (plain-text only, per
        /// `docs/session-daemon-design.md`'s phase 3 follow-up list) -- accepted and silently
        /// ignored today rather than a hard parse error, so scripts written against the eventual
        /// flag don't need editing once it lands.
        #[arg(long)]
        ansi: bool,
    },
    /// Print a session's scrollback history.
    History {
        session_id: String,
        #[arg(long)]
        since: Option<u64>,
    },
    /// List all known sessions.
    List,
    /// Kill a running session.
    Kill { session_id: String },
}

fn run(args: &[String]) -> i32 {
    let cli = match SessionCli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            e.print().ok();
            return 2;
        }
    };
    let runner_exe = current_exe_string();
    let client = match connect_or_spawn(&runner_exe) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: could not connect to session daemon: {e}");
            return 1;
        }
    };

    match cli.sub {
        SessionSub::Start { rootfs, program_and_args } => {
            cmd_start(&client, rootfs, program_and_args)
        }
        SessionSub::Send { session_id, key_string } => cmd_send(&client, &session_id, &key_string),
        SessionSub::Screen { session_id, .. } => cmd_screen(&client, &session_id),
        SessionSub::History { session_id, since } => cmd_history(&client, &session_id, since),
        SessionSub::List => cmd_list(&client),
        SessionSub::Kill { session_id } => cmd_kill(&client, &session_id),
    }
}

fn cmd_start(
    client: &litebox_session_daemon::client::DaemonClient,
    rootfs: String,
    mut program_and_args: Vec<String>,
) -> i32 {
    if program_and_args.is_empty() {
        eprintln!("error: session start requires a program to run");
        return 2;
    }
    let program = program_and_args.remove(0);
    let req = litebox_session_daemon::protocol::Request::CreateSession {
        rootfs,
        program,
        args: program_and_args,
    };
    match client.call_with_timeout(
        &req,
        litebox_session_daemon::client::CREATE_SESSION_CALL_TIMEOUT,
    ) {
        Ok(Response::CreateSession { session_id }) => {
            println!("{session_id}");
            0
        }
        Ok(Response::Error { message }) => {
            eprintln!("error: {message}");
            1
        }
        Ok(other) => {
            eprintln!("error: unexpected daemon response: {other:?}");
            1
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn cmd_send(
    client: &litebox_session_daemon::client::DaemonClient,
    session_id: &str,
    key_string: &str,
) -> i32 {
    let bytes = match litebox_session_daemon::keys::encode(key_string) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: could not decode key-string: {e}");
            return 2;
        }
    };
    let req = litebox_session_daemon::protocol::Request::SendInput {
        session_id: session_id.to_string(),
        bytes,
    };
    match client.call(&req) {
        Ok(Response::SendInput { ok: true }) => 0,
        Ok(Response::SendInput { ok: false }) => {
            eprintln!("error: send failed (session dead or unknown?)");
            1
        }
        Ok(Response::Error { message }) => {
            eprintln!("error: {message}");
            1
        }
        Ok(other) => {
            eprintln!("error: unexpected daemon response: {other:?}");
            1
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn cmd_screen(client: &litebox_session_daemon::client::DaemonClient, session_id: &str) -> i32 {
    let req = litebox_session_daemon::protocol::Request::GetScreen {
        session_id: session_id.to_string(),
    };
    match client.call(&req) {
        Ok(Response::GetScreen { text, .. }) => {
            print!("{text}");
            0
        }
        Ok(Response::Error { message }) => {
            eprintln!("error: {message}");
            1
        }
        Ok(other) => {
            eprintln!("error: unexpected daemon response: {other:?}");
            1
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn cmd_history(
    client: &litebox_session_daemon::client::DaemonClient,
    session_id: &str,
    since: Option<u64>,
) -> i32 {
    let req = litebox_session_daemon::protocol::Request::GetHistory {
        session_id: session_id.to_string(),
        since,
    };
    match client.call(&req) {
        Ok(Response::GetHistory { bytes, .. }) => {
            use std::io::Write as _;
            let _ = std::io::stdout().write_all(&bytes);
            0
        }
        Ok(Response::Error { message }) => {
            eprintln!("error: {message}");
            1
        }
        Ok(other) => {
            eprintln!("error: unexpected daemon response: {other:?}");
            1
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn cmd_list(client: &litebox_session_daemon::client::DaemonClient) -> i32 {
    match client.call(&litebox_session_daemon::protocol::Request::ListSessions) {
        Ok(Response::ListSessions { sessions }) => {
            println!("{:<12} {:<8} PROGRAM", "ID", "ALIVE");
            for s in sessions {
                println!(
                    "{:<12} {:<8} {}",
                    s.id,
                    if s.alive { "yes" } else { "no" },
                    s.program
                );
            }
            0
        }
        Ok(other) => {
            eprintln!("error: unexpected daemon response: {other:?}");
            1
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn cmd_kill(client: &litebox_session_daemon::client::DaemonClient, session_id: &str) -> i32 {
    let req = litebox_session_daemon::protocol::Request::KillSession {
        session_id: session_id.to_string(),
    };
    match client.call(&req) {
        Ok(Response::KillSession { ok: true }) => 0,
        Ok(Response::KillSession { ok: false }) => {
            eprintln!("error: kill failed (unknown session id?)");
            1
        }
        Ok(other) => {
            eprintln!("error: unexpected daemon response: {other:?}");
            1
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}
