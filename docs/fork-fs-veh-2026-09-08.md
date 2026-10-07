# 2026-09-08: cross-process `fork()` finished, s6-overlay `/init` boots, MATE still blocked

This session's target was two things at once: finish the cross-process `fork()` work, and get the
`linuxserver/webtop:alpine-mate` desktop up. **The first is done. The second is not**, and the
remaining blockers are now located precisely rather than described vaguely. What follows is what
is proven, what was refuted, and the two specific things that stand between here and a working
desktop.

Companion docs: `webtop-alpine-mate-2026-09-07.md` (the image's real launch recipe and the earlier
MATE work), `webtop-debian-selkies-2026-09-06.md` (the debian-i3 image).

## Done: cross-process `fork()` is no longer gated

The `beyond_stdio == 0` eligibility gate is gone, and `LITEBOX_PROCESS_FORK_IGNORE_FDS` -- a
measurement-only flag that accepted losing the child's fds -- is no longer needed by anything.

| what the child inherits | mechanism |
|---|---|
| pipe, sender half | real Windows pipe, host pump thread per end |
| pipe, receiver half | same, but the pump waits until the bridge is the end's sole owner |
| regular file | reopen the same path, seek to the same offset |
| the whole writable filesystem | exported to the child at spawn, imported back at `wait4` |

The receiver half is the interesting one. On a real fork parent and child draw from ONE byte
stream, and no bridge built out of a second OS pipe reproduces that: duplicating lets both read
every byte, draining eagerly steals bytes from a reader still live in the parent. The way through
is to notice *when* that hazard exists. The parent-side bridge holds the last non-descriptor
reference to the end, so `owners() == 1` says exactly one thing -- the guest parent has closed its
own descriptor and there is no reader left to steal from. A shell closes its copy immediately
after forking a pipeline stage, so the pump waits for that and then drains. A parent that never
closes is the genuinely-shared case, where the child's fd simply never yields: wrong for a truly
shared reader, which is unsupportable either way, but never wrong by delivering bytes to the wrong
process.

`/init` on `webtop_seatd.tar` now runs with **no flags and no stubs**: 16 cross-process children,
zero uncarriable fds, zero fatal errors, through `preinit`, `s6-linux-init`, `s6-rc-compile` and
`s6-rc-init` into supervision.

Getting there needed six filesystem gaps closed, each found from a live failure:

1. **fd-relative `faccessat` returned `EINVAL`.** It predated `resolve_path_at`; `s6-rc-compile`
   reported the errno verbatim as `unable to read .../flag-essential: Invalid argument`.
2. **`in_mem::seek` refused any position past EOF**, which its own TODO already called wrong --
   `write` right above it already zero-pads the sparse case. skalibs' `cdbmake_start` seeks to
   2048 in a fresh file.
3. **`rename` refused directories.** That is how a tree is installed atomically, and `s6-rc-init`
   and `s6-linux-init` both do it.
4. **An intermediate path component that was a symlink returned `ENOTDIR`.** Not a corner case:
   `ln -s dir link; cat link/file` failed outright. `s6-rc-init` publishes its live directory by
   symlinking `/run/s6-rc` at the tree it just built.
5. **FIFOs were unimplemented** (`mknod(S_IFIFO)` -> `EPERM`), which stopped `s6-linux-init-maker`
   dead. Now a real `FileType::Fifo`; `open()` yields a pipe.
6. **The writable-layer round trip lost things**: the runner's importer restored FIFOs as plain
   files and aborted on a repeated symlink, and the parent-layer env var leaked a grandparent's
   already-deleted path down a generation.

## Two bugs found on the way that had nothing to do with forking

**The tar index build was O(entries^2).** Every entry removed anything nested under its own path
with a `retain` that walked the whole map. `live` is a `BTreeMap<String, _>`, so everything under
`parent` is the range `["parent/", "parent0")` -- a range query makes the common case one lookup.
Starting one process on the 2.5 GB webtop rootfs went from **17.3 s to 0.35 s**, against ~1 s just
to read those bytes off disk. Every litebox startup pays this. The merged filesystem is unchanged:
`find / -xdev` gives the same 53,557 paths and the same md5.

**`layered::seek` corrupted the caller's position on a lower-layer file.** A `Lower` entry's
backend fd is cached and SHARED across every `open()` of the same path -- which is why `read`
deliberately resolves `None` to this descriptor's own tracked position, and says so at length.
`seek` delegated `SEEK_CUR` straight through, resolved it against that shared cursor, and stored
the answer back as authoritative. `ftell()` is a `SEEK_CUR` of zero, so almost any guest reading
out of the read-only rootfs could hit it. It presented as `/init` looping for ever: `preinit`'s
shell holds its script fd on the tar layer, one `SEEK_CUR` rewound it to 0, and the script re-ran
from the top -- 750 forks in 300 s, alternating `s6-mkdir` and `s6-overlay-stat`, the first two
forking commands in the file.

## Not done: MATE

The server side is verified working from the host: nginx + selkies return **HTTP 200, 762 bytes**
of the genuine dashboard through `--publish 3000:3000`. `mate-session` is what fails, and there
are two distinct blockers depending on which fork path is used.

### Thread-based fork (default): an access violation inside `fork_verify`

Minimal repro, ~90 s, no nginx or selkies needed:

    litebox_runner ... --initial-files .wfgy/webtop_mate.tar --resume-from .wfgy/webtop_overlay.tar \
      -- /bin/sh -c 'Xvfb :1 ... & sleep 12; dbus-launch --exit-with-session mate-session & sleep 60'

Reliably dies. The primary fault is an access violation on a load inside
`fork_verify::on_single_step`, resolved through the PDB (`rva 0x573386` ->
`on_single_step+0x3da6`, `mov rcx,[rbx]`); the VEH then re-enters at depth 2 and resumes with a
corrupted context, landing at `rip=0x4`.

**It is a race, not a logic error.** The same repro under `cdb` completed cleanly every time,
`DONE` reached, zero AVs -- a debugger's serialised exception delivery closes the window.

**The mechanism is a TOCTOU gap in `read_usize_fault_tolerant`.** It is
`if !is_readable(addr) { None } else { read_unaligned(addr) }` -- a `VirtualQuery` followed by an
ordinary load. That is a check, not fault tolerance, and another thread unmapping the page in the
gap makes the load fault. `mate-session` has many threads doing `dlopen`/`munmap`.

Three things were ruled out by measurement, not reasoning:

- **Not the VEH depth counter.** This crate's own `VEH_DEPTH_CAP` comment records an unexplained
  runaway to ~3271. Live data says `veh_depth=2` with zero `.Lsearch` bail-outs.
- **Not `ThreadHandle::interrupt`.** `LITEBOX_DIAG_INTERRUPT=1` over a full run shows it is called
  zero times.
- **Not fixable by disabling verification.** `LITEBOX_FORKVERIFY_OFF=1` dies earlier, before Xvfb
  starts: the healing is load-bearing, not diagnostic.

**The obvious fix does not work, and why is the useful part.** Rewriting
`read_usize_fault_tolerant` onto `memcpy_fallible` -- so the load carries a real `.extable` entry
instead of a pre-check -- *moved the mate-session fault out of `on_single_step`*, confirming the
diagnosis. But it broke the ordinary fork path outright: `echo A; (echo B); echo C` died with an
access violation after printing `A`. Reverted.

The reason is structural. The exception table is consulted BY the vectored exception handler, so a
fault taken inside a fallible accessor that is itself running inside the handler is a NESTED fault.
`vectored_exception_handler`'s own recovery block carries an open question about exactly this --
"whether its own surrounding assumptions (register state, stack alignment) hold when entered via
an injected `Rip` write rather than a normal in-function jump". That question now has a concrete
consequence and a concrete repro.

**Next step:** make the nested-VEH path a safe landing point for an exception-table recovery, then
put `read_usize_fault_tolerant`/`write_usize_fault_tolerant` back on `memcpy_fallible`. The
regression above is the test: it must print `A B C` and exit 0.

### Cross-process fork: no crash, but no X server

Running the same repro with `LITEBOX_PROCESS_FORK=1` produces **zero AVs** -- the fork_verify race
is gone, because a cross-process child gets an identity relocation map and healing is a no-op.
Xvfb then fails to come up (`XVFB_FAILED`, `xvfb.log` empty, `dbus-launch` reporting
`EOF in dbus-launch reading PID from bus daemon`).

This is the expected consequence of a gap already recorded in the companion doc: guest processes do
not share a loopback namespace, and AF_UNIX sockets are per-process in the shim. An X server that
lands in a separate OS process is unreachable by clients in the parent -- `/tmp/.X11-unix/X1` is
not a shared object. The same limitation is why the committed FIFO implementation is per-process
(see `open_fifo`'s doc comment) and why an attempt to bridge FIFOs over Windows named pipes was
written and then dropped rather than shipped half-working.

**Next step:** a host-side transport shared by every process of one guest, used for AF_UNIX
sockets (and reusable for FIFOs). That single piece would let the whole desktop run on the
cross-process path, which is already crash-free for this workload.

## Also fixed this session, in the exception handler itself

**No environment lookup is reachable from the VEH any more.** `fork_verify::on_single_step`
consulted `LITEBOX_VEH_TRACE` and `LITEBOX_DIAG_ALLOC_VEC` on EVERY trapped instruction, and
`vectored_exception_handler` queried five more per fault. On Windows `std::env::var_os` allocates
and enters ntdll's process-wide environment critical section; doing that inside a VEH is the same
hazard that made `ThreadHandle::interrupt` deadlock the whole guest (a MATE session frozen with one
thread parked in `RtlQueryEnvironmentVariable` and six queued behind it). Twelve gates now resolve
once from `WindowsUserland::new()`, before any guest thread exists.

Per-call-site caching was tried first and is NOT sufficient: a lazily-initialised cache still
performs its one real lookup wherever it is first reached, which for a single-step gate is inside
the handler.

**`ThreadHandle::interrupt` no longer redirects a thread caught inside the global allocator.** Its
retry budget existed precisely to avoid that, but on exhaustion it fell through and did it anyway
-- the thing its own comment calls unrecoverable. It now sets the (advisory) interrupt flag and
returns, delivering at the target's next safe point. Latent: instrumentation shows this path never
runs in the MATE workload.

## Measurement note

The session's starting build cannot reach `XVFB_UP` on this image within 480 s, where the current
build reaches it in ~12 s. Any before/after comparison on the webtop must account for that; the
`mate-session` fault is not demonstrably a regression because the baseline never gets far enough to
reach it.
