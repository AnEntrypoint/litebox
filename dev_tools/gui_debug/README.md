# GUI/XFCE debugging tools

Reusable tooling extracted from real, repeated friction points hit across many sessions
debugging litebox's DRM/wgpu/weston/labwc/XFCE stack on Windows. Each tool exists because the
same manual workaround was independently rediscovered multiple times before being scripted here.

## `screenshot-litebox.ps1`

Screenshots the litebox `--gui` window reliably. `SetForegroundWindow`/`AttachThreadInput`
routinely fail to bring the window to front in this dev environment (Windows' foreground-lock
protection rejects the request when this session isn't the OS's currently-focused process) --
several past sessions burned multiple tool calls each on wrong-window captures before finding
that `SetWindowPos(HWND_TOPMOST)` (which does not require foreground-focus permission) reliably
works instead. Also polls for the window to appear (so it can be called right after launching the
runner) and fails loudly instead of silently capturing the wrong thing.

```
powershell -File dev_tools\gui_debug\screenshot-litebox.ps1 -OutFile scratchpad\shot.png
```

## `clean-scratch.ps1`

Frees `%TEMP%` disk space consumed by stale `litebox-*` scratch directories from past debugging
sessions. This project's own live-repro workflow writes multi-GB scratch tars into `%TEMP%`
routinely; across sessions these accumulate silently until the drive is full, which then presents
as confusing, unrelated failures (a runner segfault reading its own `--resume-from` archive
because it ran out of space mid-read) rather than an obvious "disk full" error. This session alone
recovered >70GB this way, three separate times, before writing this script.

```
powershell -File dev_tools\gui_debug\clean-scratch.ps1            # dry run
powershell -File dev_tools\gui_debug\clean-scratch.ps1 -Apply     # actually removes it
```

## `symbolize-crash.js`

Automates the manual "cross-reference a crash `rip`/fault address against `fork_verify`'s own
logged relocation table" technique this project's debugging history has done by hand several
times (see `AGENTS.md`'s "libcairo"/"libwayland-client" symbolization passes) -- capture a
`[fork_verify] begin: ranges=[...]` line via `LITEBOX_VEH_TRACE=1`, then translate a crash address
between source-space (parent, pre-fork) and destination-space (child, post-relocation) in one
step instead of doing the `dest_base + (addr - source_range.start)` arithmetic by eye.

```
node dev_tools/gui_debug/symbolize-crash.js ranges.log 0xa800733
```

Once you have the SOURCE-space address, the next step (also manual today, not yet scripted) is
extracting the real binary/`.so` from the layer tar and disassembling at the computed file offset
relative to the library's own load base -- see `AGENTS.md`'s libcairo entry for a worked example
of that final step.

## Log-level tradeoffs (reference, not a script)

Repeatedly re-learned this session; recorded here so it doesn't need re-deriving:

| Level | Speed | What it's for |
| --- | --- | --- |
| `LITEBOX_LOG=error` | Fastest | Baseline pass/fail, crash-or-not. Most likely to actually reproduce a timing-sensitive race -- heavier logging often changes timing enough to avoid triggering one. |
| `LITEBOX_LOG=debug` | Slow, but tractable | Full syscall tracing. Good middle ground for narrowing WHERE in a process's execution something happens (which syscall, which fd) without VEH_TRACE's per-instruction overhead. |
| `LITEBOX_VEH_TRACE=1` | Very slow | Per-single-step `[veh] RAWREGS ...` register dumps and `[fork_verify] ...` diagnostics. Needed for exact crash-site registers and the `begin: ranges=` relocation table `symbolize-crash.js` consumes -- but the overhead itself can suppress the very race you're trying to catch. Only reach for this once a repro is either deterministic or you can afford several attempts. |
| `LITEBOX_DIAG_FATALDUMP=1` | Very slow | Heaviest diagnostic tier; single-steps everything. Same overhead caveat as VEH_TRACE, often more so. |
| `LITEBOX_DIAG_ALLOC_VEC=1` | Adds `[diag_veh_entry]`/`[diag_tf]`/`[diag_stale_ptr]`/`[diag_bigfault]` | Gates several fork_verify-adjacent diagnostics added across sessions; combine with the above as needed. |

**A crash that reproduces reliably at `error` level but stops reproducing once you turn on
tracing is not "fixed by observation" -- it means the race is real and timing-sensitive.** Don't
conclude a bug is gone because a heavily-instrumented run didn't hit it; re-confirm at `error`
level with several repeated runs before trusting a "clean" result.
