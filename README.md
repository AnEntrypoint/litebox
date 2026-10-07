# LiteBox

> A security-focused library OS

> [!NOTE]  
> This project is currently actively evolving and improving. While we are
> working toward a stable release, some APIs and interfaces may change as the
> design continues to mature. You are welcome to explore and experiment, but if
> you need long-term stability, it may be best to wait for a stable release, or
> be prepared to adapt to updates along the way.

LiteBox is a sandboxing library OS that drastically cuts down the interface to the host, thereby reducing attack surface.  It focuses on easy interop of various "North" shims and "South" platforms.  LiteBox is designed for usage in both kernel and non-kernel scenarios.

LiteBox exposes a Rust-y [`nix`](https://docs.rs/nix)/[`rustix`](https://docs.rs/rustix)-inspired "North" interface when it is provided a `Platform` interface at its "South".  These interfaces allow for a wide variety of use-cases, easily allowing for connection between any of the North--South pairs.

Example use cases include:
- Running unmodified Linux programs on Windows
- Sandboxing Linux applications on Linux
- Run programs on top of SEV SNP
- Running OP-TEE programs on Linux
- Running on LVBS

![LiteBox and related projects](./.figures/litebox.svg)

## This build: real interactive Linux shells

This checkout carries a set of fixes on top of upstream LiteBox that make
LiteBox usable as a genuine, interactive Linux userland -- not just for
running a single non-interactive command, but for driving a real shell
session the way a human would: typing at a prompt, running
`apk`/package-manager workflows, and using REPLs like Node's that depend on
raw-mode terminal I/O, job control, and correct multithreaded stdio. Most of
these fixes live in `litebox_shim_linux` (the "North" shim shared by every
platform LiteBox runs on) and so apply equally to
`litebox_runner_linux_on_windows_userland` and `litebox_runner_linux_userland`
(the native-Linux runner); a few are specific to one target and are called
out explicitly below.

Concretely, this build fixes (all landed on `main`, CI-verified):

- **Real interactive keyboard input (Windows-specific).** Typed keystrokes
  are now correctly delivered to the guest shell instead of being silently
  dropped or hanging the process (a missing epoll wakeup path and a Windows
  console cooked-mode/CPR-reply bug).
- **`setRawMode`/raw terminal mode** (used by Node's REPL, `less`, `vim`,
  Python's `readline`, and any program that manages its own line editing) no
  longer crashes with `ENOTTY`.
- **Job control.** `TIOCSPGRP`/`TIOCGPGRP` are implemented, so shells no
  longer fall back to `can't access tty; job control turned off`.
- **A deep, multi-stage `fork()` correctness fix (Windows-specific).**
  LiteBox duplicates a forked child's address space to new host addresses on
  Windows (Windows can't give two "processes" the same addresses in one host
  process; this doesn't apply to the native-Linux runner, which uses a real
  `fork()`), which left a class of stale, untranslated pointers reachable
  after `fork()` -- fixed for both code pages (a `STATUS_PRIVILEGED_INSTRUCTION`
  crash on chained shell commands) and argv/data pointers (intermittent, and
  in one case perfectly deterministic per-command-length, corruption of a
  freshly exec'd command's arguments).
- **`chmod`/`fchmod`/`fchmodat`, `utimensat`/`futimens`, and `flock`**, which
  were previously unimplemented (`ENOSYS`) despite the underlying filesystem
  layer already supporting them.
- **A real userspace NAT gateway (Windows-specific)** for guest network
  access, needing neither Administrator privileges nor a driver, so
  `apk`/`curl`/etc. can reach the real network.
- **Multithreaded process correctness**: a lost-wakeup race in `poll()`, an
  unbounded UDP NAT flow leak, orphan-process reparenting, a process-exit fd
  leak that could hang pipe readers, and a missing `FUTEX_REQUEUE`
  implementation that could deadlock a multithreaded guest process (e.g.
  Node/V8) on exit.
- **Concurrent stdio correctness**: guest writes to stdout/stderr from
  different threads of the same process (as V8/libuv do heavily) are now
  serialized, so output from one thread can no longer be spliced mid-write
  into another thread's output.
- **Missing syscalls that real-world programs call in practice**:
  `sched_getparam`/`sched_setparam`/`sched_getscheduler`/`sched_setscheduler`,
  and `clock_gettime`/`clock_getres` support for
  `CLOCK_PROCESS_CPUTIME_ID`/`CLOCK_THREAD_CPUTIME_ID`/`CLOCK_MONOTONIC_RAW`/
  `CLOCK_REALTIME_COARSE`/`CLOCK_BOOTTIME` (V8's own startup code aborts the
  whole process if `clock_gettime` returns an error, which it previously did
  for these clock IDs).
- **`setrlimit`/`prlimit64` correctness.** Calling `setrlimit`/`prlimit64` for
  any resource other than `RLIMIT_NOFILE` (e.g. `ulimit -c 0`, which is
  extremely common in shell entrypoint scripts) used to panic and crash the
  whole runner; it's now accepted for every resource. Separately,
  `RLIMIT_SIGPENDING` -- which is actually enforced, unlike most rlimits --
  defaulted to a limit of `0`, silently dropping every real-time/queued
  signal a guest process sent; it now defaults to a realistic Linux value.
- **Baked-in agent-sandbox tooling.** The published Alpine base image now
  also includes `git`, `bash`, `curl`, `openssh-client`, and `tzdata`
  alongside `nodejs`/`npm`/`python3`/`py3-pip`/`build-base`, so common
  agent-workload needs (cloning a repo, running a `#!/bin/bash` npm
  postinstall script, fetching a file, using Python's `zoneinfo` for
  anything timezone-aware -- Alpine/musl doesn't ship timezone data by
  default) don't require an extra `apk add` round-trip.
- **Unix98 pseudoterminal (pty) support.** `/dev/ptmx`, `TIOCGPTN`,
  `TIOCSPTLCK` (`unlockpt`), and `/dev/pts/<id>` now work, with real duplex
  master/slave byte forwarding and shared `termios`/window-size/foreground-
  pgid state -- previously `TIOCGPTN` unconditionally returned `ENOTTY` and
  there was no pty subsystem at all. This is what lets `node-pty`,
  `pexpect`/`ptyprocess`, `tmux`, and `script` allocate and drive a pty
  inside the guest. Input-side line discipline (kernel-side canonical-mode
  input buffering, echo, ^C/^Z/^\ signal generation) is not implemented --
  every consumer that puts the pty into raw mode itself (which is what all
  of the above do) is unaffected, but a guest shell relying on the kernel
  to echo typed characters back in cooked mode will not see that echo.
  Output-side processing is partially implemented: a fresh pty defaults to
  `OPOST|ONLCR` (matching real Linux), so a plain `\n` written by an
  ordinary program that doesn't manage its own raw mode (`ls`, `git log`,
  a Python script's `print()`) comes out `\r\n` on the master side --
  without this, any real terminal UI reading the master (VS Code's pty
  panel, ttyd/wetty, xterm.js) would render that output as an unreadable
  "staircase".
- **`setsid()` and `TIOCSCTTY`.** These were entirely missing -- `setsid()`
  wasn't implemented as a syscall at all, and `TIOCSCTTY` fell through to a
  hard `EINVAL`. Since glibc's `login_tty()` (the primitive under
  `forkpty()`/`openpty()`-based tools -- `node-pty`, Python's
  `os.forkpty()`, tmux, `script`) always calls exactly this pair right after
  `fork()`, this was a hard failure at session-open time for every one of
  the tools the pty support above exists to serve, not just a degraded-
  behavior gap. Both are implemented now.
- **Three more crash-on-ordinary-usage panics fixed.** `readlink("/proc/self/fd/<N>")`
  crashed the whole runner for any fd other than 0/1/2 (hit by e.g. Python's
  `os.readlink(f"/proc/self/fd/{fd}")`, used by introspection/sandboxing
  libraries); a single `read()` of more than 512KiB from a pipe/socket/pty
  crashed the runner (hit by e.g. reading a subprocess's stdout in one large
  read); and `mmap(MAP_SHARED | PROT_WRITE)` on a file-backed fd crashed the
  runner (hit by e.g. Python's `mmap.mmap(fd, len, mmap.MAP_SHARED,
  mmap.PROT_WRITE)`). All three now return the correct errno instead of
  panicking.
- **`fork()` now inherits the parent's rlimits.** Every freshly forked
  child used to get program-start default resource limits regardless of
  what the parent had configured via `setrlimit()` beforehand -- so a
  supervisor process that lowered e.g. `RLIMIT_NOFILE` before spawning a
  child got a child that silently wasn't bounded by it. `fork()`/`clone()`
  now copies the parent's current limits into the child, matching real
  Linux.
- **`kill(0, sig)` / `kill(-pgid, sig)` / `kill(-1, sig)` no longer hard-fail,
  and now reach a whole live group, not just self.** `kill()` has no
  registry of *arbitrary* other live guest processes to deliver to, but
  these forms target the caller's own process group or "everyone the
  caller may signal," and self is always a real member. Previously they
  failed with `ESRCH` unconditionally, even in the common case of a script
  signaling its own group during cleanup; they now deliver to self, which
  is exactly correct whenever no other process happens to share the group
  -- and additionally reach any live `fork()`ed child that's been moved
  into that same group via `setpgid()` (the standard shell-job-control /
  process-supervisor pattern of putting a whole spawned pipeline into one
  group, then killing the group to tear the pipeline down), reusing the
  exact same child-delivery mechanism described next. A group target that
  matches neither self nor any reachable child correctly still fails with
  `ESRCH`, matching real Linux's behavior for a pgid with zero members.
- **`kill(child_pid, sig)` now works for a live, shim-known direct child.**
  This shim still has no general pid -> process registry (see the previous
  bullet), so a genuinely arbitrary remote pid still correctly fails with
  `ESRCH` -- but a `fork()`ed child of the calling process *is* always
  reachable (it's already tracked, for `wait4`/`waitpid`), and this is by
  far the single most common real-world use of cross-process `kill()`: a
  supervisor/process-manager sending `SIGTERM`/`SIGKILL` to a worker it
  spawned. Previously this failed with `ESRCH` unconditionally, exactly
  like a truly unreachable pid. The signal is queued into the child's own
  process-directed pending set and the child's live threads are woken via
  the same `interrupt()` mechanism `exit_group`/`kill_other_threads`
  already use for same-process delivery -- a blocked syscall in the child
  (e.g. a `futex` wait) now genuinely returns `EINTR` and processes the
  signal, not just something that sits unnoticed in a queue. One accepted
  imprecision: the sender can't check the child's `SIG_IGN` disposition
  before queuing (that state lives on the child's own thread context, not
  reachable from the parent), so an ignored signal still costs the child
  one spurious wakeup before self-correcting at delivery time, instead of
  never disturbing it at all.
- **`setpgid()`/`getpgid()` can now also target a live direct child**, not
  just self -- the same reachability the two bullets above rely on. This is
  the standard shell-job-control sequence for setting up a pipeline
  (`cmd1 | cmd2 | cmd3`): the shell forks each stage, then calls
  `setpgid(child_pid, pipeline_pgid)` on each one from the parent side
  *before* letting them run, to put the whole pipeline in one process group
  up front. Previously any pid other than self was rejected with `ESRCH`
  unconditionally, which broke that exact sequence for a child.
- **`fork()` no longer leaks signal state between parent and child.**
  `clone_for_new_task` (used by both thread-creation and `fork()`) shared the
  same `shared_pending` queue and `handlers`/`sigaction` table between parent
  and child unconditionally -- correct for a same-process thread, but wrong
  for a genuine `fork()`, where POSIX requires the child to start with an
  independent (copied) signal disposition table and an empty pending-signal
  set. Previously, a signal sent to the parent process after `fork()` could
  be silently consumed by the child instead (or vice versa), and a
  `sigaction()` call in either process after `fork()` would incorrectly
  change the other's handler too -- both are classic patterns in real
  daemons/supervisors (e.g. a process that forks a worker and then adjusts
  its own `SIGCHLD`/`SIGTERM` handling). `fork()` now gives the child its own
  independent pending-signal queue and a snapshotted copy of the handler
  table, while ordinary thread creation continues to correctly share both
  with the rest of its process.
- **Two more crash-on-ordinary-usage panics fixed, and `ppoll`/`epoll_pwait`
  gained real sigmask support.** `open(path, O_TRUNC, ...)` on a path that
  turns out to be an existing directory (e.g. shell redirection into a
  directory, `cmd > /some/dir`) crashed the runner instead of returning
  `EISDIR`, because the underlying `TruncateError::IsDirectory` case fell
  through an incomplete error-conversion match. Separately, `ppoll()` and
  `epoll_pwait()` -- the standard signal-safe-polling idiom used by many
  event loops/daemons to avoid the self-pipe race -- unconditionally
  panicked whenever called with a real signal mask, even though `pselect()`
  already correctly supported one; both now reuse the same
  temporary-signal-mask mechanism `pselect()` uses, instead of panicking.
- **Three more crash-on-ordinary-usage panics fixed**, found via a
  systematic survey of remaining `unimplemented!()`/`todo!()` sites
  reachable from ordinary syscall usage. An unexpected read failure (e.g.
  genuine `EIO` from the backing filesystem) partway through the
  `mmap()`-file-contents-copy fallback path crashed the runner instead of
  returning `EIO`; a signal (e.g. a timer) landing mid-copy during that same
  path also crashed the runner, even though real Linux's `mmap()` is never
  interruptible by a signal in the first place -- it's now retried
  internally instead, matching what a real caller would observe.
  `fcntl(F_GETLK/F_SETLK/F_SETLKW)` (POSIX record locks) on a pipe or socket
  fd crashed the runner instead of returning `EINVAL`, which is what real
  Linux returns since record locks only apply to regular files.
  `ioctl(fd, FIOCLEX)` on a pipe or socket fd crashed the runner instead of
  setting close-on-exec, even though doing so needs the exact same
  descriptor-table update already used for every other fd type.
- **AF_UNIX socket "autobind."** Calling `bind()` on a Unix domain socket
  with no address at all (`addrlen == sizeof(sa_family_t)`) -- used by some
  IPC libraries to get a peer-identifiable address before `connect()`ing out
  without caring what the address actually is -- used to unconditionally
  panic (`todo!("autobind for unnamed unix socket")`). It now assigns an
  abstract-namespace address in the same format real Linux uses (a leading
  NUL byte followed by 5 lowercase hex digits, see `unix(7)`), unique per
  call via a shim-wide counter.
- **Persistent, resumable state on the native Linux runner too.** The
  `--export-writable-layer`/`--resume-from` flags -- walk every file the
  guest created or modified this run into a delta tar archive on exit, and
  seed a later run's writable layer from one -- previously only existed on
  `litebox_runner_linux_on_windows_userland`, despite
  `litebox_runner_linux_userland` (the native-Linux runner) building the
  exact same layered filesystem underneath. Every native-Linux run used to
  start fresh and silently discard all guest writes on exit; it now supports
  both flags identically. One native-Linux-specific wrinkle the Windows
  runner doesn't have: this process's own seccomp-bpf sandbox (see
  `enable_seccomp_filter`) stays active for the rest of the process's
  lifetime once installed, including after the guest exits, and only allows
  a narrow `O_RDONLY` case of `open`/`openat` -- so the export file is opened
  for writing *before* the filter goes up, and only `write()`s (always
  allowed) happen on it afterward.

  **Safe for multi-agent fan-out from one shared checkpoint.** `--resume-from`
  only ever reads its archive (no locking, no write-back) into a fresh,
  per-process in-memory filesystem, so any number of independent runner
  invocations can resume from the *same* `--resume-from` archive
  concurrently with zero interference -- e.g. an orchestrator spawning N
  parallel agent runs from one common base snapshot. The one thing an
  orchestrator must do itself: give each parallel invocation a **distinct**
  `--export-writable-layer` path. Two invocations racing on the *same*
  export path don't fail cleanly or simply "last write wins" -- each
  independently truncates the file on open and issues many sequential
  `write()`s as it walks the writable layer, so a collision produces
  silently corrupted, byte-interleaved tar output in both archives. This
  isn't a bug to fix so much as an inherent property of two writers sharing
  one file path; avoid it with a per-branch path convention (e.g.
  `out/agent-<id>.tar`).
- **Graceful seccomp filter fallback in containerized environments.**
  `enable_seccomp_filter()` gracefully logs a warning when the host Linux kernel or container sandbox prevents applying seccomp-bpf filters (e.g., returning `ENOSYS`), allowing LiteBox and its test suites to continue executing smoothly in stock Alpine/containerized Linux environments.
- **`ECHO` (raw-mode terminal echo).** Bytes written to a pty's master (what
  typing at a keyboard looks like from the shim's perspective) are now
  echoed back to the master's own read side when the pty's termios has
  `ECHO` set -- e.g. `stty -icanon echo`, or any consumer that explicitly
  opts into it via `TCSETS`. `ECHO` is never set by default, so this doesn't
  change behavior for `node-pty`/`pexpect`/`ptyprocess`/most modern pty
  libraries, which put the pty into full raw mode (`ECHO` off) themselves
  immediately after opening it. This is *raw-mode* echo only: there is
  still no canonical-mode input buffering (no backspace/erase editing --
  that needs a buffer of not-yet-"readable" bytes this module doesn't have)
  and no `ISIG` special characters (^C/^Z/^\, which need cross-process
  signal delivery, still an open architectural gap -- see below).
- **`open(path, O_NONBLOCK)` no longer crashes for `/dev/stdin`,
  `/dev/stdout`, `/dev/stderr`, or `/dev/urandom`.** These four paths
  previously `unimplemented!()`'d unconditionally the moment `O_NONBLOCK`
  was set, crashing the whole runner -- a real-world trigger is libuv/Node
  reopening `/dev/stdin` to get a private fd for `setRawMode`-style termios
  work (the same reopen pattern documented above for `StdioStream`
  metadata), which some libuv code paths open non-blocking. None of these
  four devices actually need this fix to be *non-blocking-aware* internally
  to open successfully: `/dev/stdout`/`/dev/stderr`/`/dev/urandom` never
  block in the first place, and `/dev/stdin` -- the one device that
  genuinely can, via the platform's blocking stdin read -- already had a
  correct `O_NONBLOCK`/`EAGAIN` path one layer up in the shim's `read()`
  handling, but only for the bootstrap fd 0; a freshly reopened
  `/dev/stdin` fd carried no status-flags metadata for that check to
  consult, so it silently ignored `O_NONBLOCK` even after the crash was
  fixed. Both are now fixed together: the panic is gone, and a reopened
  `/dev/stdin` is tagged with its real open flags so `O_NONBLOCK` is
  honored (returns `EAGAIN` on an empty read) exactly like fd 0 already
  did.
- **`connect()`/`bind()`/`sendto()`/`sendmsg()` no longer crash on an
  `AF_INET6` or `AF_NETLINK` sockaddr.** `read_sockaddr_from_user` --
  the shared helper every address-taking socket syscall routes a
  userspace sockaddr buffer through -- unconditionally panicked
  (`todo!("unsupported family ...")`) for any family other than
  `AF_UNIX`/`AF_INET`. `AddressFamily` is a closed, 4-variant enum (any
  other wire value already correctly failed with `EAFNOSUPPORT` one line
  above the old panic site), so `AF_INET6`/`AF_NETLINK` were the only two
  values that could ever reach it -- not exotic, since IPv6 is often the
  *default* outcome of DNS resolution (e.g. an `AAAA` record winning
  happy-eyeballs, or a guest dialing `::1`/`[::]` for "localhost"), and
  `socket(AF_INET6, ...)` itself already correctly returns
  `EAFNOSUPPORT` rather than crashing -- so the actual gap was reachable
  the moment *any* fd (not necessarily an IPv6 one) was handed an
  `AF_INET6`/`AF_NETLINK` sockaddr as a syscall argument. It now returns
  `EAFNOSUPPORT`, matching both the family-parsing fallback right above
  it and real Linux's behavior for a family the target socket doesn't
  support.
- **`madvise()` no longer crashes for any behavior beyond
  `MADV_NORMAL`/`MADV_DONTNEED`/`MADV_FREE`/`MADV_DONTFORK`/`MADV_DOFORK`.**
  Every other advice value -- `MADV_WILLNEED`, `MADV_RANDOM`/`SEQUENTIAL`,
  `MADV_HUGEPAGE`/`NOHUGEPAGE`, `MADV_DONTDUMP`, `MADV_WIPEONFORK`, and
  others -- unconditionally panicked, crashing the whole runner on
  something as ordinary as Python's `mmap.madvise(mmap.MADV_WILLNEED)` or
  an allocator (jemalloc, musl/glibc) issuing a hugepage hint. These are
  all advisory-only on real Linux: a real kernel accepts every one of
  them as a no-op success even on a config that doesn't act on the hint
  (e.g. `MADV_HUGEPAGE` succeeds even without transparent hugepages
  configured), so they now return success rather than panicking.
  `MADV_REMOVE` (requires a shmem/tmpfs-backed mapping, which litebox
  doesn't support) and `MADV_HWPOISON`/`MADV_SOFT_OFFLINE` (privileged
  memory-error-injection testing operations litebox has no machinery to
  honor) now fail cleanly with `EINVAL` instead. The match is exhaustive
  with no wildcard fallback, so a future `MadviseBehavior` addition fails
  to compile instead of silently reintroducing the same panic.
- **`socket()`/`accept()` on an `AF_INET`/`AF_INET6` socket no longer
  crash at the process's fd-table limit.** Both unconditionally panicked
  (`unimplemented!()`) whenever `insert_raw_fd` failed because the fd
  table was already at its `RLIMIT_NOFILE`/shim-wide limit -- an
  ordinary, guest-triggerable condition (a busy server `accept()`ing past
  its fd limit under load, which real Linux itself reports as `EMFILE`
  from `accept()`, or a guest explicitly lowering its own
  `RLIMIT_NOFILE` then calling `socket()`), not something that should
  crash the runner. Both now return `EMFILE` while cleanly tearing down
  the network-subsystem-side socket state that was already allocated
  before the fd-insert failure (so a failed `socket()`/`accept()` doesn't
  leak a `smoltcp` socket-set slot), matching the already-correct
  `AF_UNIX` sibling arm in both functions.
- **`SO_KEEPALIVE` on a non-TCP socket no longer crashes.**
  `setsockopt(SOL_SOCKET, SO_KEEPALIVE, ...)` on a UDP (or any non-TCP)
  socket unconditionally panicked instead of succeeding as a no-op. Real
  Linux accepts `SO_KEEPALIVE` on any socket type -- it's a generic
  `SOL_SOCKET` option that simply has no effect on a connectionless
  protocol, not an error -- and the software-only flag `getsockopt`
  reads back was already updated before the deferred TCP-specific step
  failed, so there was genuinely nothing left to do. Reachable via
  ordinary code that sets a common socket-option baseline before
  checking the actual protocol (e.g. `s=socket(AF_INET,SOCK_DGRAM);
  setsockopt(s,SOL_SOCKET,SO_KEEPALIVE,&1,4)`).
- **Stock Alpine Linux XFCE / Graphical Desktop Environment Support**:
  LiteBox supports running Wayland/DRM compositors (`labwc` + XFCE session tools) directly on top of LiteBox's Linux syscall shim.
  The software DRM/KMS subsystem (`/dev/dri/card0`) handles dumb buffer creation (`DRM_IOCTL_MODE_CREATE_DUMB`), mapping (`DRM_IOCTL_MODE_MAP_DUMB`), framebuffer attachment (`DRM_IOCTL_MODE_ADDFB2`), and page flips (`DRM_IOCTL_MODE_PAGE_FLIP`), presenting flipped pixel buffers via host `wgpu` surfaces.

A ready-to-run bundle (the Windows runner exe plus a packaged Alpine rootfs)
is built by [`.github/workflows/release-windows-alpine.yml`](.github/workflows/release-windows-alpine.yml).

## Contributing

See the following files for details:

- [CONTRIBUTING.md](./CONTRIBUTING.md)
- [CODE_OF_CONDUCT.md](./CODE_OF_CONDUCT.md)
- [SECURITY.md](./SECURITY.md)
- [SUPPORT.md](./SUPPORT.md)

## License

MIT License.  See [./LICENSE](./LICENSE) for details.

## Trademarks

This project may contain trademarks or logos for projects, products, or services. Authorized use of Microsoft 
trademarks or logos is subject to and must follow 
[Microsoft's Trademark & Brand Guidelines](https://www.microsoft.com/en-us/legal/intellectualproperty/trademarks/usage/general).
Use of Microsoft trademarks or logos in modified versions of this project must not cause confusion or imply Microsoft sponsorship.
Any use of third-party trademarks or logos are subject to those third-party's policies.
