# Session daemon: agent-driven multi-session TTY control

## Goal

An agent (Claude Code or similar, calling a CLI repeatedly with no persistent
in-process state) needs to:

1. Start multiple independent guest terminal sessions, each with its own pty.
2. Address each session by a stable ID across separate CLI invocations.
3. Send input to a specific session, including control sequences (Esc, arrows,
   Enter) needed to drive full-screen apps like `vi`.
4. Retrieve a session's current rendered screen (what a real terminal would
   show right now) or its scrollback/history.
5. Do this reliably enough to drive `vi` end-to-end (open, navigate, edit,
   save, quit) via nothing but repeated CLI calls.

The state (which sessions exist, their pty byte streams, their rendered
screens) must live outside the CLI process, because the CLI process exits
after every invocation. This requires a persistent background daemon.

## What litebox already has

`litebox_shim_linux/src/syscalls/pty.rs` implements Unix98 pty allocation
(`/dev/ptmx`, `TIOCGPTN`, `TIOCSPTLCK`, `/dev/pts/<id>`) entirely inside the
guest shim, with:

- Duplex master/slave byte channels (`Channel`, 8192-byte ring buffers each
  direction).
- Shared `PtyPair` state (termios, winsize, foreground pgid) visible from
  both sides, matching real Linux devpts semantics.
- Output-side line discipline: slave writes get `OPOST|ONLCR` applied
  (`\n` -> `\r\n`) by default, matching a real freshly-allocated Linux pty.
  This is exactly what an xterm-style consumer of the master side expects:
  ordinary programs (`ls`, `git log`, `print()`) that don't manage raw mode
  themselves render correctly.
- Input-side raw-mode echo (`ECHO` without `ICANON`) is implemented; full
  canonical-mode line editing (backspace/erase, `ISIG` signal chars) is not.
  This doesn't block a terminal-emulator client: `vi`, `bash`, and friends
  all put the pty in raw mode themselves and do their own line editing, which
  is the standard behavior real terminal-emulator libraries (node-pty,
  pexpect) already depend on.
- `TIOCSCTTY`/`setsid()` support for `forkpty()`-style controlling-terminal
  attachment (verified in `pty.rs`'s own test suite).
- `TIOCGWINSZ`/`TIOCSWINSZ` shared between master and slave.

**Conclusion: the guest-side pty already gives raw byte access to the master
side** -- exactly the byte stream a real terminal emulator (xterm, alacritty,
wezterm) would read from a pty master on real Linux. The piece that does NOT
exist is anything on the **host** (Windows) side that (a) reads that master
byte stream out of the guest process, (b) parses it into a rendered 2D
screen buffer, (c) keeps that state alive across CLI invocations, and (d)
exposes it over IPC. That is the entire scope of this feature.

Known fragility (from `FINDINGS.txt` passes 150-158): fork-heavy pipelines
(`apk add jq && echo hello | jq -R .`) can crash the guest shell's own
thread via a jump-to-null in `switch_to_guest_sysret`, root-caused to a CFG
(Control Flow Guard) indirect-call-target trap, not yet fixed on main. This
is orthogonal to pty/session-daemon work but worth flagging: a session
daemon that runs guest shells under heavy fork/pipe load (e.g. `apk add` to
provision a session's rootfs) can hit this. Avoid guest-side `fork()`-heavy
provisioning inside a live session where possible (pre-bake rootfs images
instead), and don't treat a crash there as a session-daemon bug.

## Terminal emulator: use a crate, don't write one

Writing a correct VT100/xterm parser (SGR colors, cursor positioning,
alternate screen buffer, scroll regions, wide/combining chars, OSC/DCS
sequences) is a multi-month undertaking with a huge edge-case surface --
exactly the kind of thing `vi` will hit immediately (alternate screen,
cursor addressing, screen clearing).

Chosen crate: **`vt100`** (crates.io, `vt100 = "0.16"`).

- Pure `bytes-in -> screen-grid-out` library: `vt100::Parser::process(&[u8])`
  feeds it a byte stream; `parser.screen()` returns a `Screen` with
  `cell(row, col)`, `cursor_position()`, `contents()` (plain text render),
  and `contents_formatted()` (ANSI-reproducing render).
- No I/O, no threading, no async -- a `Parser` is a plain struct that can
  live inside the daemon's per-session state with zero extra glue.
- Mature and widely used (originally extracted from `wezterm`-adjacent
  tooling; used by `pijul`, various CI log renderers, and test-fixture tools
  that need exact terminal-screen assertions).
- No existing workspace dependency on it or on `alacritty_terminal`/
  `wezterm-term` (checked all `Cargo.toml` files in the workspace) -- this is
  a new dependency. It is small (no transitive deps beyond `unicode-width`),
  which keeps the "net-smaller wins" cost low.

Rejected: `alacritty_terminal` (heavier, pulls in Alacritty's own event/grid
abstractions designed for a live GUI renderer, not for a simple render-to-
string daemon) and hand-rolled parsing (rejected per the standing "use a
proven library" guidance -- VT100/xterm parsing correctness is not something
worth re-deriving).

## Architecture

### Process model: one litebox guest process per session

Each session = one independently-running
`litebox_runner_linux_on_windows_userland.exe` guest process, with its own
pty pair allocated inside that guest's shim. Rejected: multiplexing several
sessions inside one guest process. Reasons:

- litebox's guest process model (per `FINDINGS.txt`'s fork/thread
  investigation) already has known fragility around fork/thread/exception
  interaction under load. Adding N independent shell sessions as N threads
  inside *one* guest process multiplies the surface area of exactly the bug
  class currently being chased (cross-thread `PtRegs`/stack corruption).
  One-guest-process-per-session keeps sessions blast-radius-isolated: a
  crash in session A's guest process cannot corrupt session B's.
  Process isolation is also what real Windows/Linux terminal multiplexers
  (tmux, Windows Terminal) already assume, so this matches the host OS's own
  fault boundary.
- Each guest process already gets a full Windows process's worth of Job
  Object / handle isolation for free, which is what "kill_session" needs to
  be simple, unconditional and reliable (`TerminateProcess`), not a bespoke
  in-process teardown of one thread among several sharing an address space.

Cost: N sessions = N OS processes = higher memory footprint than
in-process multiplexing. Acceptable: an agent driving `vi` interactively
needs at most a handful of concurrent sessions, not hundreds.

### Daemon

A single persistent Windows process (`litebox_runner_linux_on_windows_userland.exe
session daemon-run`, normally auto-spawned detached by the first `session
start`/`session send`/etc. CLI invocation that finds no daemon listening,
matching the "start once, address by ID forever after" requirement).

Per-session daemon state:

```rust
struct Session {
    id: SessionId,                 // stable, e.g. a short random hex string
    child: std::process::Child,    // the litebox guest process
    pty_master: HANDLE,            // Windows handle wrapping the guest's exposed pty master
    parser: vt100::Parser,         // live screen-buffer state
    history: Vec<u8>,              // raw scrollback (everything ever written to master)
    reader_thread: JoinHandle<()>, // drains pty_master -> parser.process() + history push
}

struct Daemon {
    sessions: Mutex<HashMap<SessionId, Session>>,
}
```

Reader thread per session: blocking-reads the guest's pty-master byte
stream as it arrives, feeds every chunk to both `parser.process(chunk)`
(live screen state) and `history.extend_from_slice(chunk)` (scrollback).
This is the same "drain in a background thread, mutate shared state under a
lock" pattern already used throughout the fork/pty work
(`FINDINGS.txt` passes 111/122/136/137/141 use pipe-based cross-process
draining for diagnostics) -- no new pattern invented here, same shape reused.

**Exposing the guest's pty master to the host process**: the guest process
(`litebox_runner_linux_on_windows_userland.exe`) needs a way to hand its
internal pty-master byte stream out to the *daemon* process that spawned it.
Two options:

1. **stdio inheritance**: spawn the guest process with its stdin/stdout
   wired to the pty master fd inside the guest (i.e. the guest's entry
   program is attached to the pty as its controlling terminal per the
   existing `TIOCSCTTY` support, and the *daemon* reads the guest process's
   own stdout Windows pipe as if it were the pty master). This requires the
   guest runner to open a pty pair internally, attach the launched program
   (`vi`, `bash`, whatever) to the *slave* side as its controlling tty, and
   forward the *master* side's bytes to the guest process's real stdout
   (which Windows already wires to the daemon via `CreatePipe`, the same
   mechanism `process_fork.rs` already uses for stdio). This is the simplest
   option: it needs zero new IPC primitive, reusing exactly the anonymous-pipe
   stdio plumbing already in `litebox_platform_windows_userland::process_fork`.
   Input direction is symmetric: bytes written to the daemon's write end of
   the guest's stdin pipe get forwarded by the guest runner into the pty
   master.
2. **A new named-pipe/socket surface exported by the guest runner** for the
   pty master specifically, independent of stdio. More flexible (allows
   multiple independent streams per guest process later) but is unneeded
   complexity for a first implementation and duplicates option 1's stdio
   channel for no immediate benefit.

**Recommendation: option 1.** Add a new CLI mode to
`litebox_runner_linux_on_windows_userland.exe`, e.g. `--pty-mode`, that:
   - allocates a pty pair in the guest shim,
   - execs the requested program attached to the pty slave as controlling
     terminal (mirroring `login_tty()` / `forkpty()`'s `setsid()` +
     `TIOCSCTTY` sequence the pty test suite already exercises),
   - spawns a forwarding loop copying pty-master bytes to the process's real
     stdout, and copying the process's real stdin to the pty master.

The daemon then just spawns this guest process normally (as
`process_fork.rs` already knows how to do) and treats its stdout/stdin pipes
exactly as the "pty master" for IPC purposes -- no new Windows IPC primitive
required for this leg.

### IPC surface: daemon <-> CLI

A single Windows named pipe, `\\.\pipe\litebox-session-daemon`, framed
request/response (length-prefixed JSON, matching the simplicity of the
pipe-based cross-process communication already proven in this project's
fork-diagnostics work). One connection per CLI invocation (the CLI is a
thin, short-lived client: connect, send one request, read one response,
disconnect).

Requests:

```
CreateSession { rootfs: PathBuf, program: String, args: Vec<String> }
  -> { session_id: String }
SendInput { session_id: String, bytes: Vec<u8> }
  -> { ok: bool }
GetScreen { session_id: String }
  -> { rows: u16, cols: u16, cursor: (u16, u16), text: String }   // vt100 Screen::contents()
GetHistory { session_id: String, since: Option<u64> }
  -> { bytes: Vec<u8>, cursor: u64 }   // cursor = byte offset for incremental polling
ListSessions {}
  -> { sessions: Vec<{ id: String, program: String, alive: bool }> }
KillSession { session_id: String }
  -> { ok: bool }
```

`GetScreen` calls `vt100::Screen::contents()` (or `contents_formatted()` if
an agent wants ANSI-preserving output) against the session's live `Parser`
state -- this IS the "what would a real terminal show right now" view,
correctly reflecting cursor movement, screen clears, and full-screen-app
redraws (e.g. `vi`'s alternate-screen repaint), because `vt100` tracks all
of that from the raw byte stream.

`GetHistory` returns the raw scrollback buffer (everything ever written to
the master), optionally sliced from a byte offset so an agent can poll
incrementally without re-fetching everything each call.

### CLI subcommand surface

Thin client, one round-trip per invocation:

```
litebox_runner_linux_on_windows_userland.exe session start \
    --rootfs <path> [--] <program> [args...]
  -> prints session ID to stdout

litebox_runner_linux_on_windows_userland.exe session send <id> "<key-string>"
  -> sends decoded bytes, prints nothing on success

litebox_runner_linux_on_windows_userland.exe session screen <id>
  -> prints the current rendered screen (plain text, one line per row)

litebox_runner_linux_on_windows_userland.exe session screen <id> --ansi
  -> prints the current rendered screen with ANSI escapes preserved
     (for a human piping into a real terminal to eyeball it)

litebox_runner_linux_on_windows_userland.exe session history <id> [--since <offset>]
  -> prints raw scrollback bytes (or the slice since <offset>)

litebox_runner_linux_on_windows_userland.exe session list
  -> table: id, program, alive/dead

litebox_runner_linux_on_windows_userland.exe session kill <id>

litebox_runner_linux_on_windows_userland.exe session daemon-start   (usually implicit)
litebox_runner_linux_on_windows_userland.exe session daemon-stop
```

### Key-encoding mini-language for `session send`

An agent needs to express "press Escape, then type `:wq`, then press
Enter" as one CLI string argument. Design: a small `<Name>` tag language
layered over literal UTF-8 text, parsed left-to-right:

- Any text not inside `<...>` is sent as literal UTF-8 bytes.
- `<Esc>` -> `0x1b`
- `<Enter>` / `<CR>` -> `\r` (matches what a real terminal sends for Enter;
  the pty's `ICRNL`/cooked-mode input translation, where implemented, or the
  program's own raw-mode handling, expects `\r` not `\n` for Enter)
- `<Tab>` -> `\t`
- `<Backspace>` -> `0x7f` (DEL, what real terminals send for backspace)
- `<Up>` `<Down>` `<Left>` `<Right>` -> `\x1b[A` `\x1b[B` `\x1b[D` `\x1b[C`
  (standard xterm cursor sequences)
- `<Home>` `<End>` `<PageUp>` `<PageDown>` `<Delete>` -> the standard xterm
  CSI sequences for each.
- `<C-x>` for `x` in `a`-`z` -> the corresponding control byte
  (`Ctrl-A` = `0x01` ... `Ctrl-Z` = `0x1a`), covering `<C-c>` (SIGINT-style
  interrupt byte the pty forwards), `<C-d>` (EOF), etc.
- `<0x1b>` / `<0x07>` -- literal hex escape for anything not covered by a
  named tag, so the language is never a hard ceiling.
- `<<` -- literal `<` (escape for the tag-open character itself).

Example: `vi`'s open-edit-save-quit sequence entirely as CLI calls:

```
session start --rootfs alpine.tar vi myfile.txt
session send <id> "iHello, world<Esc>"
session send <id> ":wq<Enter>"
session screen <id>            # confirm back at shell prompt
```

This mini-language is intentionally small and literal-text-first (unlike,
say, tmux's `send-keys -l` vs non-literal mode ambiguity) so an agent never
has to guess whether a string will be interpreted specially -- only `<...>`
sequences are special, everything else is sent byte-for-byte.

## Implementation phasing (do NOT build this all in one pass)

1. **This pass**: design doc (this file) + standalone
   `pty_bytes_to_screen` proof-of-concept: add the `vt100` dependency,
   verify live that feeding it real bytes captured from a real litebox pty
   session produces a correct rendered screen for a simple case.
2. Guest-side `--pty-mode` CLI addition to
   `litebox_runner_linux_on_windows_userland` (attach launched program to a
   pty slave as controlling terminal, forward master bytes over stdio).
3. Daemon skeleton: process spawn/track + named-pipe IPC server, no vt100
   integration yet (`GetHistory`/`ListSessions`/`KillSession` only).
4. Wire `vt100::Parser` into the daemon's per-session reader thread; add
   `GetScreen`.
5. CLI subcommands (`session start/send/screen/history/list/kill`) as a
   thin named-pipe client, plus the key-encoding mini-language parser.
6. End-to-end live verification: drive `vi` through open/edit/save/quit
   purely via repeated CLI invocations, confirm `session screen` at each
   step matches what a human would see in a real terminal.

Each phase should be its own pass with its own live-execution verification,
per this project's standing discipline against building large features
blind in one shot.

## Phase 2 (DONE, committed): guest `--pty-mode` + daemon + IPC surface

Implements phases 2-4 of the plan above (guest-side `--pty-mode`, daemon
skeleton, and `vt100` wiring/`GetScreen`) in one pass, plus a direct test
client standing in for phase 5's CLI subcommands. Live-verified against the
real `C:\dev\litebox-windows-alpine\alpine-rootfs.tar` bundle.

### Guest-side `--pty-mode` (`litebox_shim_linux` + `litebox_runner_linux_on_windows_userland`)

Reality diverged from this doc's original "add a new CLI mode to the
*runner*" framing in one respect: the actual `login_tty()`-equivalent
sequence (`open(/dev/ptmx)` -> `TIOCSPTLCK` -> `TIOCGPTN` -> `setsid()` ->
`TIOCSCTTY` -> `dup2` onto fds 0/1/2) had to live *inside*
`litebox_shim_linux`, not the runner crate, because the `Task` type that
owns all of this (`sys_open`/`sys_ioctl`/`sys_setsid`/`sys_dup`) is private
to that crate and only ever reachable from a guest syscall context. The
runner has no host-side way to drive those syscalls directly.

Concretely:

- `LinuxShim::load_program_attach_pty` (new, `litebox_shim_linux/src/lib.rs`):
  like `load_program`, but after ELF loading succeeds, calls a new
  `Task::attach_pty_stdio` (in `litebox_shim_linux/src/syscalls/pty.rs`)
  that allocates a fresh pty pair via the existing (internal) `new_pty_pair`,
  unlocks it, sets a real (24x80, not zero) default `Winsize` (see "Known
  limitations" below for why), replaces fds 0/1/2 with independent dups of
  the slave, and performs `setsid()` + `TIOCSCTTY`. Returns the new pty's id.
- The pty's **master** side is registered in a new shim-wide
  `GlobalState::daemon_pty_masters` registry (keyed by pty id), separate
  from the existing guest-driven `pty_registry` (which only ever tracks the
  *slave*, on the assumption a guest-side `/dev/ptmx` opener holds the
  master itself). Two new `pub fn` methods on `LinuxShim`,
  `pty_master_read`/`pty_master_write`, look the master up in that registry
  and read/write it using a throwaway, call-local
  `litebox::event::wait::WaitState` -- deliberately callable from ANY host
  thread with no `Task` in scope, mirroring the exact pattern
  `LinuxShim::perform_network_interaction` (the pre-existing `net_worker`
  background thread) already established for driving shim-internal I/O from
  outside a guest context.
- The slave is *also* registered in the ordinary `pty_registry` (not just
  `daemon_pty_masters`), purely so `GlobalState::hangup_slave` (called at
  real process exit) can find it the same way it finds every other pty's
  slave -- without this, `pty_master_read` blocks forever past guest exit,
  since real Linux's "hang up the master's read side when the last slave
  closes at process death" semantics only fire through that lookup.
- `litebox_runner_linux_on_windows_userland`: new `--pty-mode` CLI flag.
  When set, `run()` calls `load_program_attach_pty` instead of
  `load_program`, spawns two background threads mirroring the existing
  `net_worker` pattern (one draining `pty_master_read` into this process's
  real `stdout`, one copying real `stdin` into `pty_master_write`), then
  calls `run_thread` exactly as before. The daemon (below) spawns this exe
  with piped stdio and treats that pipe pair as the pty master's byte
  stream -- confirming design option 1 from this doc's "Exposing the
  guest's pty master to the host process" section, unchanged from the
  original plan.

### Daemon (`litebox_session_daemon`, new crate)

New workspace crate (`std`, Windows-only, excluded from the no_std CI check
alongside `litebox_termemu` for the same reason: host-side only, never runs
in the guest). Three modules:

- `protocol.rs`: the `Request`/`Response` enums exactly as specified in this
  doc's "IPC surface" section (`CreateSession`/`SendInput`/`GetScreen`/
  `GetHistory`/`ListSessions`/`KillSession`), `serde`-derived, wire format
  is a 4-byte little-endian length prefix + that many bytes of JSON (as this
  doc proposed) over `\\.\pipe\litebox-session-daemon`.
- `pipe_io.rs`: `read_message`/`write_message` framing helpers over a raw
  `HANDLE`, using `ReadFile`/`WriteFile` directly -- matching this project's
  existing raw-Win32-API style (`process_fork.rs`) rather than pulling in
  `tokio` or `interprocess` for a genuinely small win. No prior named-pipe
  *server* pattern existed anywhere in the workspace (confirmed via a full
  grep before starting) -- `CreateNamedPipeW`/`ConnectNamedPipe` in
  `lib.rs`'s `create_and_accept_one_instance` is the first one.
- `session.rs`: `Session` (one real guest process, spawned via
  `std::process::Command` with piped stdio -- the anonymous-pipe stdio
  plumbing this doc's option 1 called for, no new spawn primitive needed)
  and `Registry` (`Arc<Mutex<HashMap<SessionId, Session>>>`, IDs are a
  simple incrementing counter formatted as a string, matching this doc's
  "stable ID... simple incrementing counter or UUID is fine"). Each
  session's reader thread drains the guest's real stdout into a
  `litebox_termemu::TerminalEmulator`, exactly mirroring the "drain in a
  background thread, mutate shared state under a lock" shape this doc's
  "Daemon" section called out from the fork-diagnostics precedent.
- `lib.rs`: `run_daemon(runner_exe)` -- accept loop, one thread per
  connected client (a client may send several requests over one connection;
  the design doc's "one connection per CLI invocation" client shape is
  still what a future thin CLI would do, but the server itself doesn't
  assume it).
- `examples/session_client.rs`: the phase-2 verification harness (per this
  doc's phasing plan step 1's own precedent of a standalone proof-of-concept
  example) -- connects, creates several sessions, drives them, and asserts
  on the real returned screen/history text. NOT a test file (no `#[test]`,
  never run via `cargo test`) -- run directly via
  `cargo run -p litebox_session_daemon --example session_client -- <rootfs.tar>`
  against a separately-started `litebox_session_daemon.exe` instance, per
  this project's live-execution-only verification discipline.

### Live verification performed

Daemon started against the real `litebox_runner_linux_on_windows_userland.exe`
and the real Alpine bundle; `session_client` example run against it and
confirmed, by reading the actual returned text (not just "didn't crash"):

1. `CreateSession { program: "/bin/echo", args: ["hello"] }` ->
   `GetHistory` returns text containing `hello`.
2. `CreateSession { program: "/bin/cat" }` -> `SendInput` with
   `"echo test from daemon\n"` -> `GetScreen` returns text containing
   `"echo test from daemon"` (the pty echoing `cat`'s own stdin back via its
   stdout, round-tripped correctly through the vt100 emulator).
3. Two simultaneous `/bin/cat` sessions -> input sent to session A appears
   in session A's `GetScreen` and is CONFIRMED ABSENT from session B's
   `GetScreen` -- session isolation verified, not assumed.
4. `ListSessions` reflects all created sessions; `KillSession` on each
   returns `ok: true` and the underlying guest processes are confirmed
   gone (`Get-Process` after the run shows none left).
5. Separately, `/usr/bin/vi <file>` under `--pty-mode` (direct runner
   invocation, not yet routed through the daemon in this pass, but the same
   underlying pty machinery) was confirmed to draw a correct alternate-
   screen VT100 stream (tilde-filled empty lines, status line, `~[m` etc.) --
   this is the design's actual target scenario (driving `vi`) and the pty
   plumbing underneath it works.

### Known limitations (narrow, scoped, not blocking this phase)

- **Bare interactive `/bin/sh`/`ash` with no `-c` hangs under `--pty-mode`.**
  Root-caused to something in busybox `ash`'s interactive-mode startup path
  specifically (not the pty plumbing itself, and not `sh -c "..."`, which
  works instantly): `attach_pty_stdio` completes and `run_thread` is
  entered, but the guest's first thread never returns and never writes a
  single byte to the pty, even with a real (non-zero) winsize and
  `TERM=xterm` set. `/bin/cat`, `/bin/echo`, and `/usr/bin/vi` -- none of
  which rely on `ash`'s own job-control/interactive-prompt machinery -- all
  work correctly and promptly under the identical pty setup. This is
  scoped as a follow-up rather than blocking: it doesn't block driving `vi`
  (the doc's actual stated goal), and a session daemon can front real
  interactive work via `sh -c` (or any non-`ash` shell, or `vi` itself)
  without hitting it. Needs further root-causing (likely something in
  `ash`'s own terminal-size/job-control probe sequence hanging waiting on
  a response the shim doesn't provide) before bare interactive `ash` is
  claimed to work.
- **`\x1b[6n` (Device Status Report / cursor-position query) has no
  responder.** A program that asks "where is the cursor" and blocks on the
  answer (some of `ash`'s own startup path, in earlier debugging, before
  landing on a fixed winsize) will stall, since nothing on the daemon or
  guest side answers this query today. `litebox_termemu`'s `vt100::Parser`
  tracks cursor position internally and could answer this if wired up
  (either guest-side, echoing a synthesized response back into the pty
  master input path, or daemon-side); not implemented this phase.
- **CLI subcommands (`session start`/`send`/`screen`/`history`/`list`/
  `kill`, the key-encoding mini-language, daemon auto-spawn-on-first-use)
  are still phase 3+, unimplemented.** `session_client.rs` proves the wire
  protocol and daemon logic work; the thin CLI wrapper described in this
  doc's "CLI subcommand surface" section does not exist yet.

## Phase 3 should do next

1. **CLI subcommands.** Thin named-pipe client matching this doc's "CLI
   subcommand surface" section, added to
   `litebox_runner_linux_on_windows_userland`'s `CliArgs` as a `session`
   subcommand (start/send/screen/history/list/kill/daemon-start/
   daemon-stop), including the key-encoding mini-language parser (`<Esc>`,
   `<Enter>`, `<C-x>`, etc.) and daemon auto-spawn-on-first-use (a `session
   start`/`send`/etc. invocation that finds no daemon listening on
   `\\.\pipe\litebox-session-daemon` should spawn one detached, matching
   this doc's original "start once, address by ID forever after"
   requirement -- `litebox_session_daemon::run_daemon` already exists and
   is directly callable/spawnable for this).
2. **Root-cause the bare-interactive-`ash` hang.** Narrow it further than
   phase 2 did (busybox `ash`'s interactive-startup code path specifically)
   -- likely worth building busybox with debug symbols or instrumenting the
   shim's syscall dispatch to log every syscall a hung `ash` process last
   issued before going silent, to find exactly which syscall/ioctl it's
   blocked in.
3. **Wire up `\x1b[6n` (DSR) response**, at minimum as a documented
   follow-up even if not required to unblock (2) -- some real programs
   query cursor position and will otherwise hang the same way.
4. **End-to-end `vi` drive test purely via CLI invocations** (open, `i`,
   type text, `<Esc>`, `:wq<Enter>`, confirm `session screen` shows the
   expected file content at each step) -- this doc's phase 6, deferred
   until (1) exists.
5. Consider whether `GetScreen`'s ANSI-preserving variant
   (`contents_formatted()`, already exposed by `litebox_termemu` as
   `render_screen`, alongside the plain-text `render_screen_plain` this
   phase's `GetScreen` uses) should be a `session screen --ansi` flag once
   the CLI exists, per this doc's original "CLI subcommand surface"
   section.
