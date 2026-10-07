# AGENTS.md recompaction overflow -- 2026-10-03 (121st pass, second cut)

Everything here was dropped from `AGENTS.md` while compressing it from 30552 to 26529 bytes. It is a TRAIL:
if `AGENTS.md` contradicts anything here, `AGENTS.md` wins. Nothing here is authoritative on its own.

## Why this file exists

INVARIANT 3: when `AGENTS.md` passes 30KB, every note, memory and archive is re-read and the whole thing is
recompiled into the smallest form that still holds everything needed, and the notes/memories it absorbs are
deleted. This file is where the material that did not fit went.

## Per-commit mechanism writes (dropped from "Where things stand")

The `AGENTS.md` bullet

    Earlier (`067367b`, `19eab93`, `3bfe283`, `883cab4`, `e608959` `chroot(2)`, `2bb71cb` `seccomp(2)`,
    `8478b15`, `668d084`, `f271ed2`, `9412184`, `6beb669`, `d428acd`, `a2ebff6`, `748c4e5`, `f36621a`,
    `582cc4b`, `9014df7`, `a42d9d0`, `d944c66`, `f594693`, `2b7d7df`, `290d4d4`, `3ee1ce7`/`bff1d0b`/
    `a61ed74`, `0cda0ec`, `d383f90`/`d1dae93`, `820c2d6`)

used to carry a clause per sha. Those clauses, newest first:

- `067367b` fork padding / cross-process `/proc/<pid>`; `SECCOMP_RET_TRAP`'s 16-bit payload lands in
  `si_errno`.
- `19eab93` PID namespaces + crashpad. `3bfe283` `CLONE_NEWUSER` credentials. `883cab4` user namespaces.
- `e608959` `chroot(2)` is real: `FsState.root` is shared by `CLONE_FS`; `cwd` is root-space; dirfd-relative
  paths stay unrooted.
- `2bb71cb` `seccomp(2)`. `8478b15` (newest of the middle group). `668d084` idle trim (2.8GB -> 0.4GB).
- `f271ed2` flat rootfs index. `9412184` `only_in_own_writable_layer` + the content-snapshot `T|` carry form.
- `6beb669`, `d428acd` (per-thread root guard around `with_root_identity`), `a2ebff6`, `748c4e5`,
  `f36621a`, `582cc4b`, `9014df7`, `a42d9d0`, `d944c66`, `f594693`, `2b7d7df`, `290d4d4`,
  `3ee1ce7`/`bff1d0b`/`a61ed74` (the 32-slot wait queue and dead-holder recovery), `0cda0ec`,
  `d383f90`/`d1dae93`, `820c2d6`.

## The two freeze root causes, long form

### net_lock (`16f3e76`) -- closed

`LITEBOX_DIAG_LOCKSTALL=1` showed 16+ processes parked `val=2` on one arena mutex whose `holder_pid` was
ALIVE (295 "held by a live process" warnings). cdb had that holder's `net_worker` thread in
`Network::internal_perform_platform_interaction` blocked in `RawRwLock::read_contended`. Cause:
`perform_network_interaction` (`litebox_shim_linux/src/lib.rs:1280`) holds the arena-resident `net_lock`
across `internal_perform_platform_interaction` (`litebox/src/net/mod.rs:862`), which took the descriptor
table and entry locks the BLOCKING way; `litebox::sync::RwLock` is writer-preferring (`rwlock.rs:74-82`), so
a merely QUEUED writer blocks a reader. Fix: net-worker maintenance is best-effort --
`try_descriptor_table_mut` + `drain_entries_full_covered_by_nowait` in `attempt_to_close_queued`,
`try_descriptor_table` in `close_pending_sockets`/`drain_all_socket_channel_buffers`. chrD7: zero lockstall
warnings, ran to completion, sandboxed chromium `rc=0`.

Lock-stall tooling detail: `holder_tid` stores `GetCurrentThreadId()` next to `holder_pid` -- match it
against cdb's `~*kb` TIDs. `WaitState::commit_wait` parks with `val=1 holder_pid=0` (LOST-WAKE shape, not a
deadlock) and is silent without `LITEBOX_DIAG_LOCKSTALL=1`. A mutex at or above
`SHARED_KERNEL_HEAP_BASE` `0x7FF8_0000_0000` (`litebox_platform_windows_userland/src/lib.rs:11370`) is
genuinely cross-process. `chunk_ms=2000` means an UNBOUNDED park (a bounded wait chunks at its own deadline;
`sys_wait4` repolls at 15ms). `chrD*.err.log` is BINARY to gm codesearch -- use `.wfgy/logscan.py <file>
<term...>`.

### The event-loop freeze (`463dc29`) -- closed, and how the diagnosis went wrong for 8 passes

Symptom: a parent's event loop stopped when a spawned child exited. This was chased as "spawning `xset`
wedges the loop" (sub5) and then "cumulative state from an unreaped / bulk-output child" (sub7), because:

- sub6 FALSIFIED the spawn itself: a lone `create_subprocess_exec("/usr/bin/xset","q")` (no DISPLAY) + 600
  ticks ran clean to the harness cap -- child rc=1 at t~4.2s, `LOOP post i=412 t=419.6`, **419 ticks**, zero
  lockstall lines.
- sub7 re-ran it as four cumulative arms (A bare xset; B `sleep2`+xset; C `dd200k`+xset; D both+xset). Arm A
  clean (6/6 ticks, `ARM A DONE t=6.3 rc=1`), then `B-sleep2 pid=9 t=6.5` / `B-xset pid=11 t=12.8` and the
  loop stopped, with `val=1 holder_pid=0 chunk_ms=2000` parks at t=37/43/67/73s on `ThreadId(21)/(7)/(2)`.

Both framings were wrong. Root cause: a `ForkPipeBridge::Source` pump (`30d8f43`) forwarded bytes buffered in
the parent's pipe after 500ms of stagnation, on the theory that "nobody is consuming them" -- but
"stagnant" only means the read has not happened YET. That silently emptied any inherited read end the guest
still meant to read. libuv's GLOBAL SIGNAL LOCK is exactly that shape: a pipe holding exactly ONE byte, and
`uv__signal_lock()` is `read(pipefd[0], &c, 1)`. One carried fork emptied it, so the next SIGCHLD blocked in
`uv_run` for ever. Forwarding is now restricted to the carried end that is the child's stdin (`fd == 0`).

Why the first fork never reproduced it: the lock pipe is created lazily at first signal use, so the FIRST
fork cannot carry it -- which is why sub6's lone spawn ran 419 clean ticks.

Measurement technique that finally saw it (sub15/sub16): probe with a **zero-timeout `select()`** from a
3s ticker thread -- never `read`, which consumes the byte you are testing for. sub15: fd 5 `readable=True` at
wall 3.0 and 6.0, `False` from 9.0 onward, while python's main thread parked in `read(5, buf, 1)`. sub16:
`readable=True` on all 37 ticks and both arms complete. Confirm the fd is the pipe with `os.fstat` +
`stat.S_ISFIFO` (`/proc/<pid>/fd` shows non-path fds as `anon_inode:[fdN]`, so it cannot tell you).
`faulthandler.dump_traceback_later(15, repeat=True, exit=False)` distinguishes "frozen in `uv_run` (C)" from
"frozen in python".

`463dc29` also added `LITEBOX_DIAG_LOCKSTALL_TRACE`, whose operands must be read from `orig_rax` -- `rax`
holds the previous return value at that point.

## Child exit / watchdog detail dropped from AGENTS.md

- The external fault watchdog: 500ms poll, `TerminateProcess` a process whose CPU delta stayed <=10ms for
  15s, gated on the sticky `Local\litebox-fault-armed-<pid>` event (`process_fork.rs:5236-5336`), set ONLY by
  the two VEH self-terminate paths (`lib.rs:2319`, `:2623`) -- and a NULL `OpenEventW` handle means ALWAYS
  ARMED (`:5243`). A healthy `Xvfb` blocked in `select()` qualifies. The observed `Killed` Xvfbs in sub1/
  sub2/sub3 were the harness cap, not this. The real gate needs a cross-process in-VEH/in-syscall flag ANDed
  in. Disarm with `LITEBOX_DIAG_NO_EXTERNAL_FAULT_WATCHDOG=1` / `LITEBOX_DIAG_NO_FAULT_WATCHDOG=1`.
- A cross-process child whose exit code lacks the `0xC0DE` marker decodes as SIGKILL, so `rc=137` also means
  "host process died some other way" -- not only the harness `taskkill /F /T`.
- Python cannot install its own SIGCHLD handler under asyncio: `loop.add_signal_handler(SIGCHLD, ...)` raises
  `RuntimeError: it is used by the event loop to track subprocesses` (sub3).
- `sys_wait4` runs `import_cross_process_writable_layer` (a real tar read, `process.rs:3175`) on reap.

## Chromium: smaller open items

- ~1 per run: `rebuilding a carried SCM_RIGHTS fd failed errno=ENOENT spec=F|.../Local Storage/leveldb/LOG`.
- GWP-ASan `MapRegion` EEXIST occasionally.
- `--crash-dumps-dir` does not exist in Debian's chromium build.

## Selkies: the full 2.0.0 flag list (selk7 printed the real `--help`)

Real names: `--enable-clipboard`, `--printing-enabled`, `--print-spool-path`,
`--gamepad-enabled`/`--audio-enabled`/`--microphone-enabled`/`--webcam-enabled`, `--mode`,
`--enable-basic-auth`, `--unix-socket`, `--port`, `--addr`, `--web-root`. `--clipboard-enabled` does not
exist. `CUSTOM_WS_PORT` is a 1.x name 2.0.0 ignores.

Superseded selkies framings, kept only because they explain runs in the logs:
- "The `--publish` path is unhealthy" -- FALSE. The host fetched selkies' `index.html` (786B),
  `assets/index-*.js` (**650KB in 0.53s**), css, `manifest.json`, `icon.png`, `/api/status`, all 200; the
  host browser loaded the app with 15/15 200 and zero console errors and then sat on "Waiting for stream...".
- "Asyncio cannot serve on litebox" -- MEASUREMENT ERROR, corrected by aio9. Four server arms on one guest
  (uvloop + `start_server(sock=)`, uvloop + `start_server(host,port)`, uvloop + aiohttp `SockSite`, plain
  `SelectorEventLoop`), each probed twice: **8/8 `code=200`**. aio5-aio7 passed an `(reader, writer)`
  callback to `create_server`, which takes a no-arg PROTOCOL FACTORY, so every accept raised `TypeError`.
- "Selkies stalls at startup" -- was the 8080/8081 port mismatch plus basic-auth-with-no-password, and then
  the `463dc29` subprocess wedge.
- `start_server()` (`stream_server.py:3020`) only logs "running on" after `runner.setup()` +
  `_start_sites()`. `_run_command` (`websockets_mode.py:4508`) is `create_subprocess_exec` + PIPE/PIPE +
  `wait_for(communicate(), 10.0)`; `printing.py:241` adds a `preexec_fn` (`prctl(PR_SET_PDEATHSIG)`) that
  fails under litebox, so cupsd can never start.
- The litebox log clock starts at runner start; a guest script's own `t0` is ~8s later.

## Absorbed memories (deleted; their load-bearing lines now live in AGENTS.md)

- **No autonomous `git push`** (`feedback-no-autonomous-git-push`, 2026-09-22): a dispatched agent
  investigating the `xfce4-session` `DE_FAILED` bug ran `git pull --rebase origin main` (which rewrote local
  commit SHAs) and then pushed to `https://github.com/AnEntrypoint/litebox.git` unapproved. Several
  concurrent sessions on this machine share that remote, so an autonomous push can race a peer's uncommitted
  work. Local commits are expected and fine; push is a separately gated step. -> now in "Repo hygiene".
- **gm codesearch is fixed** (`gm-codesearch-fixed-in-gm-plugkit`, 2026-10-02/03): `codesearch` used to miss
  phrases committed at HEAD and time out at 120s. Both fixed in the `rs-plugkit` guest (`C:\dev\gm`, branch
  `harden/signed-updates`): dual mode runs an exhaustive phrase scan (`phrase_hits` in the response, plus a
  score boost for a chunk containing the query verbatim), and an index pass writes text chunks before it buys
  bert vectors instead of blocking on the embedder. Measured: warm dual query 3.9-8.6s (`stage_ms`,
  `full_response: true`), `bm25_rank` 4580ms -> 37ms, `index_pass` 16620ms -> ~1s, digest reaches a complete
  non-partial `v3:<hash>:files=N`. The guest is sideloaded at `~/.agentplug/plugins/gm.wasm`; a reinstall
  cold-starts its in-wasm caches, so the first dispatch after a rebuild is slower. `literal` and `filename`
  modes are exhaustive and sub-second. -> now in "Docs and tooling map".
- **Subagent model: Sonnet** (`subagent-model-sonnet`, 2026-09-15): the user said "we want to use sonnet, not
  opus" after several `Agent` dispatches defaulted to `model: "opus"`. -> now in "Standing lessons".
- **Web search: Google, not DuckDuckGo** (`web-search-google-not-duckduckgo`): if Google is blocked or
  bot-detected, fall back to camoufox; never to DDG. -> now in "Standing lessons".

## Corrections made during this compaction

- `AGENTS.md` cited `CROSS_PROCESS_FORK_CONCURRENCY_CAP` = 6 at `process.rs:3583`; it is at
  `process.rs:3600` (verified by gm codesearch, `mode: "dual"`).
- `AGENTS.md`'s Open #1 still said "**Selkies never reaches \"running on\"** -- re-test now that `463dc29`
  landed" while its own Selkies section recorded chrD8 streaming to a host browser. The stale item was
  replaced with the real remaining leak (a Source pump parking in `end.read()` when the child dies first).
- `SHARED_UNIX_CONN_CAPACITY: usize = 4096` verified at `litebox_shim_linux/src/syscalls/unix.rs:3726`;
  `const SPILLED_PREFIXES` verified at `litebox_shim_linux/src/syscalls/file_spill.rs:10`.
