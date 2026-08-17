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
