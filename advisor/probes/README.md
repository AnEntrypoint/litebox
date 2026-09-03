# Advisor probes

- `clone_probe.c`: standalone Win32 probe for `RtlCloneUserProcess` as a fork() primitive.
  Build: `clang -O1 -o clone_probe.exe clone_probe.c -lkernel32 -lntdll`. Result on this host
  (Windows 11 10.0.26200, 2026-09-03): clone succeeds; private memory is copy-on-write
  isolated; a pre-clone `SEC_RESERVE` section view stays shared and pages committed by the
  parent AFTER the clone are visible to the child; inherited pipe handles work; both
  `NtCreateThreadEx` and kernel32 `CreateThread` work in the child; child exit code is
  delivered. Keyed events do NOT match waiters across processes (both sides time out), so
  cross-process futex needs per-waiter events, not keyed events.
- `frame_stats.py`: pixel statistics for `LITEBOX_DUMP_FRAMES` BMPs (non-black count, bbox,
  row bands, top colors). `python frame_stats.py frame_dump/xfce.29`.
- `apc_probe.c`: `QueueUserAPC2` with `QUEUE_USER_APC_FLAGS_SPECIAL_USER_APC` from a parent
  into a cloned child's thread that is spinning in user mode (no alertable wait). Result on
  this host: the export exists, the call succeeds, and the APC routine runs inside the child
  while it spins (`shared1=1234`, `shared2=1`). This is the cross-process (and intra-process)
  signal-delivery primitive: no `SuspendThread`/`SetThreadContext` race.
- `fsbase_probe.c`: when does Windows clear a user-written FS base (`wrfsbase`)? Result on
  this host: kept across 1000 plain syscalls, kept across `SwitchToThread`, kept across a
  VEH-handled access violation; CLEARED to 0 after any real context switch (`Sleep(20)`, or
  a long user-mode spin that gets preempted). So the reset rate equals the thread's
  context-switch rate: every blocking wait that deschedules and every timer preemption.
  Under a multi-process desktop that is thousands per second, each followed by a faulting
  `fs:` access and a VEH round trip. Rewriting `fs:` accesses (advisory 1.1) removes the
  dependence entirely.
- `tid_probe.c`: guest-side (Linux/musl) check of per-thread tid plumbing, the suspected cause
  of xfce4-session's hang on a futex whose expected value is exactly 0x80000000. Prints
  `gettid()`, `set_tid_address()`'s return, and musl's `pthread_self()->tid` (offset 0x30 on
  x86_64) from the main thread and a created thread; locks an errorcheck mutex on each and
  shows the raw lock word (musl stores the owner tid in the low 30 bits); finally does a
  contended lock/unlock handshake between two threads, which is the 20-line version of the
  xfce4-session hang. Build in the guest: `cc -O1 -o tid_probe tid_probe.c`. Exit code is the
  number of failed checks; a zero owner tid or a hang at the handshake localizes the bug to
  litebox's clone/set_tid_address/futex-wake path.
- `memfd_probe.c`: guest-side memfd/mmap/mremap/SCM_RIGHTS coherence checks (4 checks incl.
  the mremap growth step libwayland-server uses for wl_shm pools).
- `forklock_probe.c` (+ prebuilt `forklock_probe`): freestanding guest probe for fork
  isolation, the suspected root cause of the xfce4-session futex deadlock. musl's `fork()`
  has the CHILD write `td->tid = -1` into every other thread's `struct pthread` and zero every
  registered atfork lock word (src/process/fork.c); on real Linux that is harmless because the
  child has its own copy-on-write address space. This probe writes a known pattern into an
  anonymous page, a `.data` word and a stack word, forks, has the child overwrite all three,
  and then checks the PARENT's copies are untouched, including across 16 successive forks.
  Any corruption means the fork emulation is unsound and every threaded guest program is at
  risk of exactly this class of hang.

  **No guest toolchain required** (the guest's clang hits the ET_DYN loader panic at
  `mm.rs:1117` and its gcc's cc1 segfaults). Build on the Windows host with the bundled clang,
  which emits a static ET_EXEC with no libc, no headers and no interpreter:

      clang --target=x86_64-unknown-linux-gnu -nostdlib -nostdinc -ffreestanding \
            -fno-stack-protector -static -O1 -o forklock_probe forklock_probe.c

  Copy into the rootfs layer and run. Exit code = number of failed checks.
- `dup_probe.c`: clears risk 1 of `docs/presenter-process-design.md` — can the runner hand a
  scanout section HANDLE to a separately spawned presenter via `DuplicateHandle` with no admin
  rights? Result on this host (2026-09-03): yes. The parent creates a section, spawns the child,
  `DuplicateHandle`s into it with `DUPLICATE_SAME_ACCESS` (no `OpenProcess`, no
  `SeDebugPrivilege` — the creator already holds a full-access process handle), and passes the
  value over a pipe. The child maps it and reads the correct pixels, and its write back is
  immediately visible to the parent, so the shared scanout is live in both directions.
  Build: `clang -O1 -o dup_probe.exe dup_probe.c -lkernel32`.
- `hello_probe.c` (+ prebuilt `hello_probe`, 1,192 bytes): the smallest possible litebox guest
  program, used as a launch-reliability check. NOTE: this was written to chase a suspected ~65%
  silent-launch failure rate; that claim was RETRACTED on 2026-09-03. The silent runs were a
  test-harness artifact (a `/bin/sh -c` wrapper crashing with SIGILL), not a litebox bug --
  invoked directly from its own tar layer this probe gives 50/50 clean runs. Always launch
  probes directly (`-- /probe`), never through a runtime-built shell wrapper. It emits staged
  markers so a silent run can be classified instead of guessed at: `S` (written to stderr as the literal first thing `_start` does), `M` (stdout),
  `W` (stderr), `X` (stdout) then `exit(0)`. Expected full output is `SMWX`.
    - nothing at all  -> the guest never reached its first instruction; the failure is in the
      runner/loader before guest entry, not in any guest code.
    - `S` only        -> guest entry works; something fails immediately after the first syscall.
    - partial/interleaved -> output path or teardown is dropping writes.
  Run it 50 times capturing exit code, stdout and stderr per run. Build line is the same host
  cross-compile recipe as `forklock_probe` (see above).

## How to run these in the guest (important)

Bake the probe into its own minimal tar layer and invoke it as the runner's top-level program:

    -- /forklock_probe

Do NOT launch through `/bin/sh -c "..."` with a runtime base64-decode/chmod step. That pattern
crashed `/bin/sh` itself with SIGILL on 2026-09-03 and its restart noise was misread as a ~65%
litebox launch-failure rate (see the `hello_probe` note above). Also export
`MSYS2_ARG_CONV_EXCL="*"` in Git Bash, or MSYS2 silently rewrites `/`-prefixed arguments into
Windows paths before they reach the runner. Both harness bugs produced convincing but false
claims about litebox; isolate one variable before concluding the emulator is at fault.
- `xfce_diag_launch.sh`: drop-in replacement for the XFCE launch script, carrying the
  diagnostics that repeated runs kept omitting and removing the races that invalidated earlier
  measurements. Bake it into the tar layer and run it as the runner's TOP-LEVEL program; on the
  host `export LITEBOX_LOG=error` and `export LITEBOX_DUMP_FRAMES=1` (host env, not `--env`,
  which reaches only the guest). Differences from the previous script: sets `HOME` and
  `XDG_RUNTIME_DIR`; sets `XFSM_VERBOSE=1` and prints `$HOME/.xfce4-session.verbose-log` at the
  end, so xfce4-session names the exact startup stage it reached; POLLS for the weston socket
  and for `:1` actually accepting connections instead of `sleep 3`/`sleep 2` (Xwayland needs
  ~14 s, so the old fixed sleeps guaranteed "cannot open display"); keeps the XFCE clients off
  GL (`GDK_GL=disable`, `GALLIUM_DRIVER=softpipe`, xfwm4 compositing off) while noting that
  Xwayland's own libGL/libLLVM load happens earlier and completes fine; gives xfce4-session a
  180 s budget since it only reaches interesting work ~60 s in; and prints the surviving process
  list at the end.
- `xfce_on_weston.sh`: runs the XFCE session on WESTON instead of labwc, sidestepping an
  upstream labwc quirk. labwc's own `src/server.c` unconditionally calls
  `wlr_output_destroy(wlr_headless_add_output(server->headless.backend, 0, 0))` — creating a
  0x0 headless output and destroying it, a documented workaround for virtual-output overlay.
  Under litebox something renders that transient output and wlroots'
  `wlr_swapchain_create` asserts `width > 0 && height > 0`, killing the session ~50s in.
  That is upstream behaviour, not a litebox defect: litebox's DRM emulation is confirmed
  working (1920x1080 mode enumerated + selected, dumb buffer allocated, pixman renderer
  created, swapchain tested OK on 'Virtual-1' — all quoted from the guest's own logs).
  weston is already proven to render a complete desktop in this environment
  (2,073,597 non-black pixels). Layer `xfce-layer31-nopanel.tar` contains weston plus all
  XFCE binaries. Bake the script into the layer, run it as the runner's top-level program,
  and export `LITEBOX_LOG=error` + `LITEBOX_DUMP_FRAMES=1` in the HOST environment.
  Includes the readiness polls, HOME/XFSM_VERBOSE, GL avoidance and trailing verbose-log
  dump from `xfce_diag_launch.sh`.

## Reading interleaved guest output (important)

Guest stdout/stderr is interleaved CHARACTER-WISE with litebox's own log lines, so ordinary
`grep` on the log misses guest messages that span line boundaries. To read them:

    sed 's/\x1b\[[0-9;]*m//g' run.log | tr -d '\n' | grep -oE ".{90}PATTERN.{50}"

That single trick recovered the wlroots/labwc messages that several hours of line-based
greps had failed to find, including the `1920x1080 @ 60.000 Hz` mode line and
`manually creating headless backend`. Per-process stdout capture (advisory item 3.2) would
remove the need for it entirely.

## litebox gap: `test -S` never succeeds (no S_IFSOCK)

litebox's in-memory filesystem reports no socket file type: grepping `litebox/src/fs` and
`litebox_shim_linux/src/syscalls/file.rs` for `S_IFSOCK` / `FileType::Socket` / `is_socket`
returns nothing. So a bound unix socket is connectable by path, but `stat`/`lstat` does not
identify it as a socket and shell `test -S` is false forever.

This cost a full weston run: `xfce_on_weston.sh` originally waited with `[ -S "$sock" ]` and
timed out after 60s even though weston had started correctly and bound
`/run/user/0/wayland-0` (its own log shows "Output 'Virtual-1' enabled" and
"launching '/usr/libexec/weston-desktop-shell'"). Fixed by using `[ -e ]` throughout.

Guidance: in guest scripts always test socket readiness with `[ -e path ]`, never `[ -S path ]`.
Worth fixing properly in the FS layer — real software checks this, and stale-socket cleanup
logic (e.g. the `/run/seatd.sock` blocker found this session) cannot distinguish a stale
regular file from a live socket without it.
- `bgshell_probe.sh`: minimal repro for the guest-shell fragility that blocks every XFCE launch
  path. Backgrounds five long-lived processes, does ordinary shell work, checks they survive,
  repeats, and polls. No dbus/weston/X/XFCE involved. Prints `BG_SHELL_OK` on success. Motivated
  by pass_weston5.log, where the launch script's own `/bin/sh` took SIGILL (Exception 6 #UD,
  rip=0x7feffff7fb8a, ~449 KB below TASK_ADDR_MAX with nothing mapped there) at t=0.997s right
  after `dbus-daemon &`, so dbus never started and every downstream readiness wait timed out.
  Across recent runs FOUR distinct fatal signals were seen in guest shells — SIGILL(4),
  SIGSEGV(11), SIGTRAP(5), SIGABRT(6) — which argues the fragility is broad rather than one
  defect. Run it 20 times: the failures are intermittent, so one clean pass proves nothing.

## tramp_fork_probe.c  (advisory 3F -- USE THIS, not bgshell_probe.sh)

Fast repro (~1 s vs ~80 s) for the deterministic #UD that kills the first backgrounded service
in every launch script.

BUILT AND VERIFIED on the host: valid static ET_EXEC x86-64, entry 0x2016a0, 20 syscall sites
(so it genuinely exercises the trampoline patching path).

    clang --target=x86_64-unknown-linux-gnu -nostdlib -nostdinc -ffreestanding \
          -fno-stack-protector -static -O1 -o tramp_fork_probe tramp_fork_probe.c

Run as the runner's TOP-LEVEL program from a tar layer, never via `sh -c`.
Exit code = number of children killed by a signal; 0 means it did not reproduce.

The shape it tests: fork(), then the CHILD runs 200 syscall pairs BEFORE execve. That pre-exec
window is the whole point -- it executes trampoline stubs in the freshly relocated child, which
is where the bad bytes are. bigfork_probe.c execs immediately and therefore SKIPS this window,
which is why it does not show the bug.

Interpreting a failure: grep the run log for "fatal signal". A rip within a few hundred KB below
TASK_ADDR_MAX (0x7fefffff0000) confirms 3F, since that is the top-down band where
maybe_patch_exec_segment places stubs.

### Superseded
bgshell_probe.sh was written for an intermittent "shell fragility" problem. That framing is
withdrawn: pass_weston5 and pass_weston6 show bit-identical rip AND rsp, so the failure is
deterministic and much narrower than "backgrounding is unreliable". Prefer tramp_fork_probe.
