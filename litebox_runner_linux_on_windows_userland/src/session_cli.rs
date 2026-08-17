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

fn usage() -> ! {
    eprintln!(
        "usage: litebox_runner_linux_on_windows_userland session <subcommand>\n\
         \n\
         subcommands:\n\
         \x20 start --rootfs <path.tar> [--] <program> [args...]\n\
         \x20 send <id> <key-string>\n\
         \x20 screen <id> [--ansi]\n\
         \x20 history <id> [--since <offset>]\n\
         \x20 list\n\
         \x20 kill <id>"
    );
    std::process::exit(2);
}

fn run(args: &[String]) -> i32 {
    let Some(sub) = args.first() else { usage() };
    let runner_exe = current_exe_string();
    let client = match connect_or_spawn(&runner_exe) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: could not connect to session daemon: {e}");
            return 1;
        }
    };

    match sub.as_str() {
        "start" => cmd_start(&client, &args[1..]),
        "send" => cmd_send(&client, &args[1..]),
        "screen" => cmd_screen(&client, &args[1..]),
        "history" => cmd_history(&client, &args[1..]),
        "list" => cmd_list(&client),
        "kill" => cmd_kill(&client, &args[1..]),
        other => {
            eprintln!("error: unknown session subcommand '{other}'");
            usage()
        }
    }
}

fn cmd_start(client: &litebox_session_daemon::client::DaemonClient, args: &[String]) -> i32 {
    let mut rootfs: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--rootfs" | "--initial-files" => {
                i += 1;
                rootfs = args.get(i).cloned();
            }
            "--" => {
                rest.extend_from_slice(&args[i + 1..]);
                break;
            }
            other => rest.push(other.to_string()),
        }
        i += 1;
    }
    let Some(rootfs) = rootfs else {
        eprintln!("error: session start requires --rootfs <path.tar>");
        return 2;
    };
    if rest.is_empty() {
        eprintln!("error: session start requires a program to run");
        return 2;
    }
    let program = rest.remove(0);
    let req = litebox_session_daemon::protocol::Request::CreateSession {
        rootfs,
        program,
        args: rest,
    };
    match client.call_with_timeout(&req, litebox_session_daemon::client::CREATE_SESSION_CALL_TIMEOUT) {
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

fn cmd_send(client: &litebox_session_daemon::client::DaemonClient, args: &[String]) -> i32 {
    let [session_id, key_string] = args else {
        eprintln!("error: session send requires <id> <key-string>");
        return 2;
    };
    let bytes = match litebox_session_daemon::keys::encode(key_string) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: could not decode key-string: {e}");
            return 2;
        }
    };
    let req = litebox_session_daemon::protocol::Request::SendInput {
        session_id: session_id.clone(),
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

fn cmd_screen(client: &litebox_session_daemon::client::DaemonClient, args: &[String]) -> i32 {
    let Some(session_id) = args.first() else {
        eprintln!("error: session screen requires <id>");
        return 2;
    };
    // `--ansi` (ANSI-preserving render) is not yet exposed by the daemon's `GetScreen` response
    // (plain-text only, per `docs/session-daemon-design.md`'s phase 3 follow-up list) -- accepted
    // and silently ignored today rather than a hard parse error, so scripts written against the
    // eventual flag don't need editing once it lands.
    let req = litebox_session_daemon::protocol::Request::GetScreen {
        session_id: session_id.clone(),
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

fn cmd_history(client: &litebox_session_daemon::client::DaemonClient, args: &[String]) -> i32 {
    let Some(session_id) = args.first() else {
        eprintln!("error: session history requires <id>");
        return 2;
    };
    let mut since: Option<u64> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--since" {
            i += 1;
            since = args.get(i).and_then(|s| s.parse().ok());
        }
        i += 1;
    }
    let req = litebox_session_daemon::protocol::Request::GetHistory {
        session_id: session_id.clone(),
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

fn cmd_kill(client: &litebox_session_daemon::client::DaemonClient, args: &[String]) -> i32 {
    let Some(session_id) = args.first() else {
        eprintln!("error: session kill requires <id>");
        return 2;
    };
    let req = litebox_session_daemon::protocol::Request::KillSession {
        session_id: session_id.clone(),
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
