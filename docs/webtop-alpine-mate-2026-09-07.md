# 2026-09-07: `linuxserver/webtop:alpine-mate` under litebox — Xvfb unblocked, dashboard served to the host, video path still blocked

This session's target was the browser-facing webtop: get it working and verify it live from a
real browser on the Windows host. That end state was **not reached**. What follows is exactly
what is now proven working, what is still broken, and the precise root causes found — including
one fix attempt that was made and then reverted, and why.

Companion doc: `webtop-debian-selkies-2026-09-06.md` covers the **debian-i3** image. This one is
the **alpine-mate** image (`.wfgy/webtop_seatd_realigned.tar`). They are different binaries on
different libcs; conclusions do not transfer automatically, and at least one previously-recorded
blocker turned out not to apply here at all (see "Refuted" below).

## The image's real launch recipe (read out of the image, not guessed)

`s6-overlay` is bypassed, per this project's standing rule. The authoritative commands come from
the image's own service definitions:

- **`svc-xorg` runs `Xvfb`, not `Xorg`.** Despite the name. `/usr/bin/Xorg` is a 275-byte `sh`
  wrapper no service ever invokes. Xvfb's argv is taken verbatim from
  `/etc/s6-overlay/s6-rc.d/svc-xorg/run`.
- **The web server is nginx**, serving the static dashboard on **port 3000** and proxying
  `/websocket` to selkies on **127.0.0.1:8082**.
- **selkies is pure Python** (`/lsiopy/bin/selkies`, a venv console script on CPython 3.14.7).
  There is no Node.js in this image at all; `node` only appears under `DEV_MODE`, which must not
  be set.
- **The desktop is MATE**, started as `dbus-launch --exit-with-session /usr/bin/mate-session`.
- Required env: `DISPLAY=:1`, `HOME=/config`, `USER=abc`, `XDG_RUNTIME_DIR=/config/.XDG`,
  `PATH` including `/lsiopy/bin`, and **`CUSTOM_WS_PORT=8082`** — selkies' own default is 8081
  (`selkies/settings.py`), while nginx proxies to 8082, so bypassing s6 without setting this
  yields a 502 that reads like a 404.

A reproducible overlay generator (nginx config, directories, staged launch script) is committed
at `advisor/probes/make_webtop_overlay.py`. It writes `.wfgy/webtop_overlay.tar`, used via
`--resume-from`. `STACK_STAGE=1..4` adds Xvfb / nginx / selkies / MATE one at a time, which is
how every attribution below was made.

## Proven working, live

- **Xvfb runs and stays up.** `xset q` succeeds against `:1`. This required the `fork()` fix
  below; before it, Xvfb died every time.
- **nginx serves the real selkies dashboard to the Windows host** through `--publish 3000:3000`:
  HTTP **200**, 762 bytes of the genuine dashboard `index.html`, repeatable, and correct under
  four concurrent requests and across a keep-alive session.

## Fixed this session

1. **`fork()` failed with ENOMEM because an out-of-range address *hint* was rejected outright.**
   `insert_mapping` (`litebox/src/mm/linux.rs`) refused any suggested range ending past
   `TASK_ADDR_MAX`, including for `FixedAddressBehavior::Hint`, which documents itself as a hint
   the platform may ignore — and which the rest of that function already treats that way.
   `Vmem::duplicate` sizes each fork group to include `reserved_extra` headroom, so a parent
   whose topmost group sits near the top of the address space produced a span ending just past
   the limit: measured live at 1,055,973,376 bytes overshooting by exactly 20,480 bytes, while
   ~128 TiB sat free. An out-of-range hint now slides down instead of failing.

   **This is the failure `webtop-debian-selkies-2026-09-06.md` calls "the single blocking item".**
   It recorded the resulting ENOMEM as "the same well-known, already-documented
   address-space-duplication hazard class" and did not trace it. It was not that hazard; it was
   an ordinary bounds bug. With it fixed, Xvfb forks `xkbcomp`, the keymap compiles, and the X
   server stays up.

2. **The host-wide `boot.lock` was leaked by every clean run.** Its release lived in a `Drop`
   impl, but `run()` exits via `std::process::exit`/`ExitProcess`, which runs no destructors —
   a fact `main.rs`'s own comment already recorded. Every successful run therefore blocked the
   next boot for the full five-minute staleness window. The lock is now the lockfile's own held
   OS handle, which Windows releases on every exit path including a hard kill.

## Refuted

**The Xvfb pid-1 SIGSEGV recorded in mutable `webtop-xvfb-crash-not-relro` does not occur on this
image.** That investigation (cr2 at `reserve_base+0x200`, `rip` inside `ld.so`) was against the
**Debian/glibc** Xvfb. The alpine/musl Xvfb never reaches a fault: with the image's own `-shmem`
flag it exits *cleanly*, self-diagnosed —

```
shmget: Function not implemented
(EE) Couldn't add screen 0
```

— because **litebox implements no SysV shared memory at all** (`shmget`/`shmat`/`shmdt`/`shmctl`
have zero references anywhere in `litebox_shim_linux`). Dropping `-shmem` lets Xvfb allocate its
framebuffer normally and it starts. Whether selkies' capture path ultimately needs real SysV shm
(and therefore MIT-SHM) is open and untested, because selkies does not get that far.

## Still blocked, with root causes

### 1. `import pixelflux` SIGSEGVs — this is what stops the video path

Isolated to a 30-second repro, no full stack needed:

```
python3 -c "print(42)"      -> 42            (CPython 3.14.7 itself is fine)
python3 -c "import selkies" -> ok            (pure-Python package is fine)
python3 -c "import pixelflux" -> Segmentation fault (139)
python3 -c "import pcmflux"   -> Segmentation fault (139)
```

`pixelflux`/`pcmflux` are selkies' native capture/encode extensions. With `LITEBOX_LOG=error`
the cause is explicit:

```
diag-reclaim: failed to recommit orphaned CoW-view flank as anonymous memory
              -- left MEM_FREE, next touch will SIGSEGV  win32_err=487
```

then a guest read of a page inside that freed range.

**Root cause: `win32_err=487` is `ERROR_INVALID_ADDRESS`, and it is an alignment failure.** The
flank recommit in `litebox_platform_windows_userland/src/lib.rs` calls `VirtualAlloc2(...,
MEM_RESERVE | MEM_COMMIT, ...)` at the flank's own base. `MEM_RESERVE` requires an
**allocation-granularity-aligned (64 KiB)** base, but a flank boundary is only ever page-aligned
— it is wherever the caller's sub-range happens to begin or end. All three flanks observed in one
run were page-aligned and none was granularity-aligned (`0xA57000`, `0xAB1000`, `0xB6B000`), so
the reservation failed every time and the flank was left `MEM_FREE`, exactly as the log says.

A **second latent defect** sits behind it: the call passes `view_mbi.Protect`, which for a CoW
view is `PAGE_WRITECOPY`/`PAGE_EXECUTE_WRITECOPY` — not a legal protection for *private*
anonymous memory. It needs mapping down to `PAGE_READWRITE`/`PAGE_EXECUTE_READWRITE`, or the
commit fails even once the address is accepted.

**A fix was attempted and REVERTED.** The obvious approach — reserve the whole former view once,
then `MEM_COMMIT` each piece inside it — panicked in `do_query_on_region` ("The handle is
invalid", os error 6). The reason is instructive and is now recorded as a comment at the site:
`view_mbi.BaseAddress` is **not** the view's allocation base. `VirtualQuery` reports the base of
the contiguous *same-attribute* page range, which can begin mid-view, so bounds derived from it
are themselves unaligned and a single "whole view" reservation is no more legal than the
per-flank ones. **A correct fix must use the real allocation base** (`view_mbi.AllocationBase`,
or track each view's base at map time) and reserve from there. The tree is left at the original
behaviour plus that explanation; no half-fix is in place.

### 2. MATE trips the `fork_verify` host-side access violation

`dbus-launch --exit-with-session mate-session` reproduces
`[diag-unrecov-av] ... is_in_guest=false is_verifying=true`. Both flags together mean the fault
is in litebox's **own host-side code**, inside the fork-verify stale-pointer healing path — not
in guest code. This matches the existing PRD row
`mate-session-avs-are-in-fork-verify-not-guest` and is unchanged by this session's fork fix,
which addressed allocation, not verification. A daemonizing nginx reproduces the same fault,
which is why the committed nginx config sets `master_process off`.

### 3. Chrome cannot reach `localhost` on this host — verification gap, not a litebox bug

Isolated by direct construction:

| target | PowerShell | Chrome (claude-in-chrome) |
|---|---|---|
| litebox webtop, `127.0.0.1:3000` | 200 | error page |
| plain host Python server, `127.0.0.1:8099` | 200 | error page |
| `https://example.com` | — | renders fine |

Chrome fails on a **host-native** server just as it fails on litebox's published port, and
succeeds on an external site. So this is a Chrome/extension localhost restriction, entirely
independent of litebox, and it is the reason no browser screenshot of the dashboard exists in
this session despite the dashboard being served correctly.

## Lying instruments found (each cost real time here)

Consistent with this project's recurring theme, four diagnostics reported nothing or something
false:

1. **`xdpyinfo` is not in this image.** Using it as the Xvfb readiness probe reported a perfectly
   healthy X server as `XVFB_FAILED`. `xset` is present and is the correct probe.
2. **`netstat` reports nothing** because litebox does not emulate `/proc/net/tcp`; it prints
   headers and an error, so "no listeners" was meaningless.
3. **Python block-buffers stdout when not a tty**, so `selkies.log` was empty for 25 s of a live
   run. `PYTHONUNBUFFERED=1` is required to see anything.
4. **A shell pipeline hides the exit code of the command that matters.** `cargo build ... | tail`
   reported success while `cargo` was in fact not on `PATH` at all. Use `${PIPESTATUS[0]}`.

Separately, a parallel review lane established that on Windows the `error_code` in guest-fault
diagnostics is **synthesized, not hardware** (`4 | (write << 1)`, with present-bit hardcoded to
0), so its not-present half is fabricated. On the fork-verify path it is synthesized twice, from
litebox's own `is_write` guess. Several open mutables reason from that field; their conclusions
need other evidence.

## Environment note

`C:\\Users\\user\\.cargo` was missing entirely this session (no shim, no registry cache) while
`.rustup` was intact, so `cargo` was not on `PATH`. Builds were run through the toolchain binary
directly:

```
CARGO_HOME=C:\\Users\\user\\.cargo RUSTUP_HOME=C:\\Users\\user\\.rustup \\
  ~/.rustup/toolchains/stable-x86_64-pc-windows-msvc/bin/cargo.exe build --release -p <crate>
```

Also: an orphaned no-argument `litebox_runner` process (a fault-watchdog child that outlived its
parent) held an exclusive lock on the runner `.exe` and made every rebuild fail with "Access is
denied (os error 5)". Worth checking for before blaming the build.

## Second and third fix attempts on the flank bug — also reverted, but they narrowed it a lot

After the first attempt (above), two further attempts were made and reverted. They are worth
recording because they convert "the flank recommit is misaligned" into a much sharper statement
of what the real obstacle is.

**Attempt 2 — recover the view's true extent, then reserve it once.** `view_mbi.AllocationBase`
*is* the view's own base and *is* allocation-granularity aligned; the view's true end can be
recovered by walking `VirtualQuery` forward from it while `AllocationBase` keeps matching. That
part worked and is the right technique. It also fixes a second, previously-unnoticed defect in
the existing code: the flanks were computed from `BaseAddress`/`RegionSize`, i.e. only the
maximal same-attribute run, so for a multi-segment view the flanks *under-reported* the destroyed
area and some orphaned bytes had nothing even attempting to restore them.

**It still panicked**, and the reason is the real finding:

```
diag-region-op-fail: operation failed on region start=0xAC6000 end=0xAC7000
                     mbi_state=MEM_FREE last_error=487
panicked at lib.rs:6102: operation failed on region 0xAC6000-0xAC7000
```

with a backtrace through `allocate_pages <- insert_mapping <- create_mapping <- create_pages <-
do_mmap` — i.e. an ordinary later guest `mmap`. `0xAC6000` is exactly the END of a flank that had
just been restored successfully.

**Restoring a flank correctly BREAKS the next allocation that rounds into the same 64 KiB
granule.** `reserve_and_commit` rounds its `MEM_RESERVE` *out* to allocation granularity
(`round_down_to_granu(start) .. round_up_to_granu(end)`), which is what makes it work for
arbitrary page-aligned addresses in the first place. Once a flank occupies part of that granule,
the rounded reservation overlaps it, Windows refuses with `ERROR_INVALID_ADDRESS` (487), the
operation returns false, and `process_memory_range_by_regions` asserts. So the pre-existing
behaviour — the flank silently failing to be restored and staying `MEM_FREE` — is precisely what
was keeping subsequent allocations working. The latent SIGSEGV and the panic are two faces of the
same missing design.

**Attempt 3 — let `reserve_and_commit` tolerate an already-reserved granule** (on a failed
`MEM_RESERVE` at an explicit address, fall through and try `MEM_COMMIT` anyway, on the theory
that the space is already ours). Insufficient: the commit fails too, because the granule is only
*partly* covered by the flank reservation, so the remainder is genuinely `MEM_FREE` and there is
nothing to commit into.

### What a correct fix therefore has to do

Not a local patch to the recommit call. The allocator needs reservation bookkeeping that these
two paths share, so that "this granule is already reserved, and here is how much of it" is a
question with an answer — either by tracking reservations explicitly, or by making
`reserve_and_commit` reserve *only* the granule remainder it actually needs rather than the whole
rounded-out span. Until then, restoring flanks and keeping later `mmap`s working are mutually
exclusive in this code.

The tree is left at the original behaviour, verified like-for-like after the revert:
`import pixelflux` SIGSEGVs (139), the runner exits 0, and **zero** panics; Xvfb still comes up.

### One more pre-existing bug found while doing this

Running `python3` **directly as pid 1** (`runner ... -- /lsiopy/bin/python3 -c "import
pixelflux"`) panics at HEAD with exit 101, where the same command wrapped in `/bin/sh -c` gives a
clean guest-side SIGSEGV and a 0 exit. That difference is present without any of this session's
changes and is not explained here — but it means the harness shape changes the failure mode, so
compare like with like when measuring this area (this project's own standing
"isolate the harness" lesson, in a new place).

## RESOLVED: the CoW path was the cause, and it is now off by default

The flank work above eventually paid off, but the shipped fix is simpler than the flank repair
itself.

**Fourth attempt at the flank fix worked.** Two pieces were both required, and each is useless
without the other:

1. Recover the view's true extent from `AllocationBase` (granularity-aligned, unlike
   `BaseAddress`) by walking `VirtualQuery` forward while `AllocationBase` matches, then reserve
   the whole former view in ONE call -- **with its end rounded UP to allocation granularity**.
   That round-up is the piece attempts 2 and 3 were missing: leaving the reservation at the
   view's exact end strands the remainder of the final granule as free-but-unreservable, so the
   next `mmap` landing there can neither reserve it (our reservation already occupies the
   granule) nor commit into it (that tail was never reserved).
2. Let `reserve_and_commit` fall through to `MEM_COMMIT` when its `MEM_RESERVE` fails with
   `ERROR_INVALID_ADDRESS` at an explicit address, since the granule may already be reserved by
   the whole-view reservation above.

With both, `import pixelflux` stopped SIGSEGVing and produced an honest Python error instead:
`ImportError: Error relocating .../libplacebo-...so.338: dovi_rpu_get_header: symbol not found`
-- with `libdovi-867af97d.so.3.3.1` present and correct in the very same directory. Memory-safe,
but the flanks are restored as ZERO-FILL, and for a shared library the lost bytes are real
content. The symbol was missing because the pages holding it had been zeroed.

**So the flank repair converts a crash into silent data loss.** That is strictly better, and it
is as far as this approach can go: reconstructing a flank as an equivalent CoW mapping needs
guest-address -> file/offset tracking that does not exist anywhere in this codebase (`VmArea`
records only `is_file_backed: bool`).

**Decisive A/B.** Bypassing `try_cow_mmap_file` entirely:

```
CoW enabled  -> import pixelflux: ImportError (symbol not found);  import pcmflux: SIGSEGV
CoW disabled -> import pixelflux: PX_OK (rc 0);                    import pcmflux: PC_OK (rc 0)
```

**Shipped fix: `LITEBOX_COW_MMAP`, defaulting OFF.** The CoW fast path is now opt-in. Two
independent reasons, both measured rather than argued:

- It is **incorrect** here, per the above.
- It is **not buying anything**. AGENTS.md's own "Windows CoW-mmap performance" section already
  records the optimisation as having "zero practical effect" on real tar-packed execs, because
  `MapViewOfFile3` needs 64 KiB file-offset alignment while real ELF `PT_LOAD` offsets are only
  page-aligned. It succeeds mainly on deliberately realignment-padded images -- precisely where
  it now does damage.

The flank fix is kept, because it is a genuine correctness improvement for anyone who opts back
in with `LITEBOX_COW_MMAP=1` to continue the alignment/CoW investigation the open PRD rows
describe.

### With CoW off, selkies runs

`selkies.log` reaches, in full:

```
INFO:data_websocket:pcmflux library found. Audio capture is available.
INFO:data_websocket:pixelflux library found. Striped encoding modes available.
INFO:main:SelkiesStreamingApp initialized: encoder=x264enc, display=1024x768
INFO:main:All main components initialized. Running server...
'port': 8082
```

and it goes on to spawn its real `xdotool`/`xclip` helper processes. A TCP connect to the
published 8082 from the host succeeds, so the listener is genuinely up.

### A second packaging bug found on the way: empty-file dedup corrupts the image

`webtop_seatd_realigned.tar` deduplicates content-identical files into symlinks. That includes
**zero-byte** files, so every empty file in the image was symlinked to an arbitrary other empty
file. Concretely:

```
/usr/lib/python3.14/urllib/__init__.py -> /lib/apk/db/lock
```

which made `import urllib.parse` fail with `PermissionError: [Errno 13]`, killing selkies before
it started. Use `C:\dev\litebox-webtop\webtop_seatd.tar` (not deduplicated) instead. With CoW
off, the realigned tar's alignment padding buys nothing anyway. **The packager's dedup pass must
exclude zero-length files** -- they are "content-identical" to each other only vacuously.

## The remaining blocker: guest loopback is not shared between guest processes

With everything above, the dashboard serves and selkies runs, but the browser still shows
`WebSocket disconnected`. The console gives the exact reason:

```
WebSocket connection to 'ws://127.0.0.1:3000/websockets' failed:
  Error during WebSocket handshake: Unexpected response code: 502
```

502 means nginx could not reach selkies at `127.0.0.1:8082`. Evidence that this is a
cross-process loopback gap, not a NAT or nginx bug:

- From the HOST, through `--publish`, nginx serves `/` with HTTP 200 reliably, including under
  four concurrent requests and across a keep-alive session.
- From the HOST, a TCP connect to a published 8082 reaches selkies' listener fine.
- From INSIDE the guest, `wget http://127.0.0.1:3000/` fails every single time (`HTTP_LOCAL_FAIL`
  in every staged run in this document), even though that is the very nginx that answers the host
  correctly.
- The previous session's control test -- nginx proxying to a second `server` block **inside the
  same nginx process** -- succeeded. That is intra-process loopback.

Taken together: loopback works within one guest process and not between two. That is consistent
with `litebox/src/net/mod.rs`'s own structure, where the `LocalPortAllocator` is an
"independent RNG-seeded instance per-`Network`" -- each guest process gets its own network stack,
so one process's `127.0.0.1` listener is simply not in another process's namespace.

**Do not re-investigate `net.rs`'s NAT path for this.** The previous session cleared it by direct
construction and this session's host-side 200s corroborate that independently. The gap is that
guest processes do not share a loopback namespace.

Binding selkies to `0.0.0.0` and proxying to the guest's own `10.0.0.2` was tried and is NOT a
workaround: it stopped nginx serving the host correctly as well.

**Measurement caveat for whoever picks this up:** by the end of this session free host memory had
fallen from 6.6 GB to 1.67 GB (runner ~1.5 GB + Chrome ~1.5 GB + editors), and at that point every
run began timing out including configurations that had been reliable minutes earlier. That is the
host-exhaustion condition AGENTS.md already warns not to misattribute to litebox. Check
`FreePhysicalMemory` before trusting any timing or hang observed in this area.

## BROWSER-VERIFIED: the WebSocket control plane works end to end; video is blocked on one abort

Routing the reverse proxy to the HOST removes the only hop that needed guest-internal
networking, and with that the stack is verifiable from a real browser. `advisor/probes/hostproxy.py`
serves the dashboard's static files and tunnels `/websockets` straight to selkies via its
`--publish`ed port. Everything that makes the desktop -- Xvfb, xterm, selkies, pixelflux/pcmflux
-- still runs entirely inside litebox; only the reverse proxy moved, and litebox's own `--publish`
is already a host-side NAT.

**Verified live in Chrome** (`http://127.0.0.1:8090/`), from the browser console:

```
[websockets] Connection opened!
[websockets] Sent initial settings (resolutions are physical) to server
[websockets] Sent initial clipboard request (cr) to server.
[websockets] Started sending client metrics every 500ms.
[websockets] Started sending backpressure ACKs every 50ms.
Initializing Input system...
```

and server-side:

```
INFO:data_websocket:Data WebSocket Server listening on port 8082
INFO:data_websocket:Legacy client ('10.0.0.1', 49158) connected. Role: controller
INFO:data_websocket:Data WebSocket connected from ('10.0.0.1', 49158)
```

The client renders its cursor and reaches **"Waiting for stream..."** -- the correct client-side
rendering of "connected, no frames yet". So the dashboard, the WebSocket upgrade, the control
plane, the metrics/backpressure loop and the input system all work through litebox.

### The one remaining blocker: PulseAudio aborts selkies the instant a client connects

```
INFO:data_websocket:Sending last known cursor to new client
INFO:data_websocket:Attempting to establish PulseAudio connection...
Assertion 'r == 0 || r == 95' failed at ../src/pulsecore/mutex-posix.c:57,
  function pa_mutex_new(). Aborting.
```

selkies dies there, before any frame is captured. That is why the client sits at "Waiting for
stream...".

`95` is `ENOTSUP`. `pa_mutex_new` tolerates only success or `ENOTSUP` and aborts the whole
process on any other errno.

**Disabling audio does not avoid it.** `--audio-enabled=false --microphone-enabled=false
--clipboard-enabled=false` (and the matching `SELKIES_*` env vars) were all tried; the
"Attempting to establish PulseAudio connection..." line still runs on client connect, so this
path is not gated by those settings.

**Root-caused by direct measurement, and fixed.** No compiler is needed to settle this: Python's
`ctypes` can call the pthread functions directly in-guest, which pins the value exactly.

```
python3 -c "import ctypes; libc=ctypes.CDLL(None); ...
            libc.pthread_mutexattr_setprotocol(a, proto)"

   proto 0 (PRIO_NONE)     setprotocol -> 0
   proto 1 (PRIO_INHERIT)  setprotocol -> 22   <-- EINVAL, and PulseAudio aborts on it
   proto 2 (PRIO_PROTECT)  setprotocol -> 95   (ENOTSUP, from musl itself)
```

`pa_mutex_new` asserts `r == 0 || r == ENOTSUP` on exactly the `PRIO_INHERIT` call, so 22 kills
the process. musl gets that 22 from litebox: it probes support by issuing `futex(FUTEX_LOCK_PI)`,
and `parse_futex` rejected every unknown futex op with `EINVAL`.

Re-running the probe against each candidate errno showed the guest-visible value tracks this
syscall's errno one-for-one (`EINVAL 22 -> 22`, `ENOSYS 38 -> 38`, `ENOTSUP 95 -> 95`) -- this
musl reports the probe's errno straight through rather than mapping it. So the fix is to return
**`ENOTSUP`/`EOPNOTSUPP` (95)** for the six priority-inheritance futex ops (6, 7, 8, 11, 12, 13).
That is also the accurate errno on its own terms: `ENOSYS` means "syscall not implemented", but
`futex` *is* implemented -- just not these operations.

**Verified:** with the change, `setprotocol(PRIO_INHERIT)` returns 95, the value PulseAudio
accepts.

This is the same errno-contract class AGENTS.md already records, where a `clone()` namespace-flag
`EINVAL` silently broke all PNG/JPEG decoding through glycin's sandbox fallback. Getting a
refusal errno wrong breaks unrelated features.

**Still to confirm end to end:** that selkies now survives the PulseAudio connect and streams
frames to the browser. The syscall-level fix is measured, but the full-stack confirmation was not
obtained, because by this point free host memory had fallen to ~1.3 GB (from 6.6 GB at session
start) and every run became unreliable regardless of code -- see the measurement caveat below.
Re-run the stack with more free memory; the remaining path is short.

If it does turn out to be an errno-contract bug, note that it is the SAME class AGENTS.md already
records: a `clone()` namespace-flag `EINVAL` once silently broke all PNG/JPEG decoding through
glycin's sandbox fallback. Getting a refusal errno wrong breaks unrelated features.

## Final state: everything up to the encoder works; frames blocked on the fork_verify AV

With the PulseAudio abort fixed, the chain was walked all the way to selkies' capture path, and
the remaining blocker is unambiguous.

### A trimmed rootfs, because host memory decides whether a run completes at all

`advisor/probes/make_webtop_min_rootfs.py` cuts the 2.4 GiB payload to **1.1 GiB** by dropping
Chromium, mesa's Vulkan drivers, the Docker/containerd/cmake toolchain, and locale/icon/theme/
wallpaper data -- none of which this stack reaches. With it, a full stack comes up in ~50 s
instead of timing out. Two things must NOT be dropped, both learned by breaking them:

* **libgallium + libLLVM + /usr/lib/dri.** Xvfb links `libGL` even when started without GLX, and
  `libGL` needs gallium, which needs LLVM. Removing them fails Xvfb at load.
* **GNU tar long-name headers.** Writing the trimmed archive with Python's default
  `GNU_FORMAT` produced files that were present in the archive but *unresolvable at runtime*:
  `Error loading shared library libglslang-default-resource-limits-24bc816e.so.15.2.0 ... (needed
  by libplacebo-...so)`, for a file the archive demonstrably contained at the right path and size.
  GNU format stores an over-long name (>100 chars) in a separate `././@LongLink` header, and
  **litebox's tar reader does not implement that extension** -- it sees a truncated name. Writing
  `USTAR_FORMAT`, which splits long names across the `prefix`/`name` fields, fixes it completely.
  That is a real, previously-unrecorded litebox gap in its own right: any GNU-format tar with
  paths over 100 characters will silently lose those files.

### Three more litebox gaps found on the way

* **`waitid` is unimplemented** (`OSError: [Errno 38]`). CPython's asyncio uses it to reap
  subprocesses, so `proc.communicate()` never completes; selkies' clipboard monitor then times out
  after 1 s and retries forever, which floods the log and starves the process. `--clipboard-enabled=false`
  avoids it; implementing `waitid` is the real fix.
* **`--resume-from` cannot shadow a path that already exists in the base layer.** A patched
  `selkies/display_utils.py` placed in the overlay was simply not seen -- the guest kept executing
  the base-layer version. The previous session recorded this for symlinks; it holds for ordinary
  regular files too. The workaround used here is a `PYTHONPATH=/patch` `sitecustomize.py` at a
  path with no base-layer counterpart.
* **`/proc/stat` is absent**, so psutil's system monitor raises. Non-fatal.

### The blocker: every subprocess spawn is a dice roll on the fork_verify AV

`[diag-unrecov-av] ... is_in_guest=false is_verifying=true` -- litebox's own host-side code
faulting inside the fork-verify stale-pointer healing path. It was hit at three independent
points, and removing each one only moved the failure to the next:

1. **PulseAudio autospawn.** `pulsectl.Pulse(...)` connects with `autospawn=True`, which forks to
   start a daemon. Avoided with `PULSE_SERVER=unix:/nonexistent/pulse.sock`, after which the
   connection fails cleanly and selkies continues.
2. **DPI application on client connect.** selkies probes for a DE session binary and shells out to
   xrdb/gsettings/xfconf-query. Removing those binaries is NOT sufficient -- the "generic xrdb
   fallback" still forks before failing to exec. Neutralised via the `sitecustomize` shim.
3. **Clipboard.** `xclip` spawned once a second, forever (see `waitid` above).

Each fix got further; none removed the class. The AV is non-deterministic -- the same
configuration reached "PulseAudio connection failed" cleanly on one run and AV'd on the next --
which matches this bug's long-recorded character.

**So the honest statement is:** the webtop's dashboard, WebSocket upgrade, control plane, input
system, gamepad/evdev interposers and selkies' own encoder initialisation all work under litebox
and are browser-verified. Video frames do not flow because selkies cannot survive long enough to
start capturing, and what kills it is the pre-existing `fork_verify` host-side access violation on
subprocess spawn -- the same architectural gap
`webtop-debian-selkies-2026-09-06.md` identifies as the single blocking item, and which
`ADVISORY-001` argues the in-process relocated-copy fork cannot be made correct for.

Fixing that -- Track B's genuine cross-process child spawning, per `ADVISORY-002-d-zero-fork.md`
section 6 -- is what stands between this project and a live desktop in the browser. Everything
else on the path is now done and verified.

### Three-way A/B of every available fork mode -- all three break, differently

Run against the same stack, same binary, one variable changed each time. This is the clearest
statement of the blocker, and it is measured rather than argued:

| mode | result |
|---|---|
| **default** (fork_verify healing ON) | `[diag-unrecov-av] is_in_guest=false is_verifying=true` -- litebox's OWN host-side code faults inside the healing path. Non-deterministic: the same config survived one run and died the next. |
| **`LITEBOX_FORKVERIFY_OFF=1`** | **Zero host AVs** -- the crash genuinely disappears. But the forked child then runs with unhealed stale pointers and dies guest-side instead: `[diag-ud-entry] ... raw_code=0xc0000096`, `Illegal instruction`, `Segmentation fault`, and Xvfb never comes up (`XVFB_FAIL`). |
| **`LITEBOX_PROCESS_FORK=1`** (Track B, cross-process) | Immediate `[diag-unrecov-av-terminate]`, before Xvfb starts. Matches this repo's own code comments recording prior `LITEBOX_PROCESS_FORK=1` runs crashing on this host. |

The second row is the important one: it shows the healing pass is **load-bearing**, not merely
defensive -- turning it off does not reveal a working fork underneath, it reveals the stale
pointers the healing exists to paper over. That is `ADVISORY-001`'s "unsound by construction"
claim demonstrated directly rather than reasoned about, and it is why no amount of avoiding
individual spawn sites fixes this: the class cannot be dodged, only made rarer.

## `waitid` implemented -- asyncio subprocesses now work, and the host AVs go to zero

Following the fork-mode A/B above, the picture changed once the *spawn sites* were removed one at
a time rather than the fork mechanism being blamed wholesale.

Neutralising the three avoidable spawn sites (via `PYTHONPATH=/patch` `sitecustomize.py`, kept at
a path with no base-layer counterpart because `--resume-from` cannot shadow one that has one) took
`[diag-unrecov-av]` from 122 occurrences per run to **zero**, and selkies then advanced from
"client connected" all the way into:

```
INFO:data_websocket:Initial setup or dimensional change detected. Performing full display reconfiguration.
INFO:data_websocket:Starting display reconfiguration...
INFO:data_websocket:Layout calculated: Total Size=1320x816
OSError: [Errno 38] Function not implemented          <- os.waitid
```

So the next blocker was not the fork bug at all: **`waitid` was unimplemented**. CPython's asyncio
reaps every subprocess with `os.waitid(P_PID, pid, WEXITED | WNOWAIT)` and then a separate
`waitpid`; without it that thread dies, `communicate()` never completes, and every asyncio
subprocess hangs forever. That is what left the display reconfiguration unfinished -- and it is
also why selkies' clipboard monitor respawned `xclip` once a second indefinitely.

`sys_waitid` is now implemented (`litebox_shim_linux/src/syscalls/process.rs`), modelled on
`sys_wait4` but honouring the two things that make `waitid` different: it reports through a
`siginfo_t` rather than a packed status word, and `WNOWAIT` observes a child *without* reaping it,
so the caller's own follow-up `waitpid` still succeeds.

Verified in-guest, directly:

```
WAITID_OK 2 1 0            si_pid=2, si_code=CLD_EXITED, si_status=0
WAITPID_AFTER 0            WNOWAIT correctly left the child reapable
ASYNCIO_OK b'async-child'  asyncio.create_subprocess_exec + communicate() works end to end
```

and `wait4` is unregressed (exit-status propagation and `sleep 1 & wait` both still correct).

## Where this actually stands

Working and browser-verified: the dashboard, the WebSocket upgrade, the full control plane
(settings, metrics, backpressure ACKs), the input system, the gamepad/evdev interposers, selkies'
own initialisation with `pixelflux`/`pcmflux` loaded, and -- with the spawn sites neutralised --
**zero host-side access violations**.

Not yet witnessed: video frames in the browser. Not because of a known code defect any more; the
remaining obstacle in this session was the host itself. Free memory oscillated between ~2.5 GB and
~1.2 GB as the runner, Chrome and the editors competed, and below roughly 2 GB the guest wedges
mid-startup in a way that is indistinguishable from a hang -- exactly the condition AGENTS.md
warns not to misattribute to litebox. Every run that had enough memory to reach the client-connect
stage got further than the one before it.

**To finish this:** free host memory (close browsers/editors, or run on a machine with more than
16 GB), then re-run the recipe below. The trimmed rootfs plus the fixes above are all committed;
nothing else is known to be missing between here and a frame.

## Two more real blockers found and fixed; the video pipeline's own abort is now root-caused

Continuing past the `waitid` fix, selkies got far enough to reveal two further defects.

### `resize_mapping` panicked on an ordinary out-of-space condition

```
thread '<unnamed>' panicked at litebox\src\mm\linux.rs:1906:22:
internal error: entered unreachable code
```

`resize_mapping`'s in-place-expand path treated `AboveMaxAddress`/`BelowMinAddress` from
`insert_mapping` as `unreachable!()`. They are not: `new_end` comes from the caller's requested
size, so an `mremap`-style growth near the top of the address space reaches them normally. Real
Linux answers that with `ENOMEM`. This turned an ordinary capacity condition into a host-side
panic that killed the whole guest, and it fired the moment selkies grew a mapping there. Now
reported as `OutOfMemory`; only genuine misalignment (impossible by construction here) stays
`unreachable!()`. With it fixed, selkies reaches "All main components initialized. Running
server..." in ~25 s with **zero** host AVs.

### The video pipeline aborted because selkies' process had no `DISPLAY`

This is the one that was actually stopping frames, and it took a diagnostic wrapper to see rather
than inference:

```
[shim] get_new_res returned no screen_name; DISPLAY=None IS_WAYLAND=False
[shim] raw xrandr rc=1 len=20: b"Can't open display 
"
```

selkies determines its screen name by running `xrandr` and matching `(\S+) connected`
(`selkies/selkies.py`'s `get_new_res`). With no `DISPLAY` in its environment that call returns
"Can't open display", no screen name is found, and the pipeline aborts outright:

```
WARNING:gst_app_resize:Could not determine connected screen from xrandr.
ERROR:data_websocket:CRITICAL: Could not determine screen name from xrandr. Aborting.
ERROR:data_websocket:FATAL: Initial reconfiguration completed, but video pipeline did not start.
```

Everything else in the stack sees `:1` correctly -- `xterm` renders, `xset q` succeeds, and a
standalone asyncio `xrandr` from the same guest returns the full 129-byte output including
`screen connected 1024x768+0+0`. So this is specific to selkies' own environment handling, not a
litebox gap. `advisor/probes/webtop_sitecustomize.py` restores it (overridable with
`SELKIES_DISPLAY`).

**Two of my own mistakes are recorded here because they cost real time and would cost it again:**
first, an earlier revision of that shim also neutered `resize_display`/`generate_xrandr_gtf_modeline`,
which looks harmless and silently removes the very call the video pipeline depends on; second, I
dropped `+extension RANDR` from the Xvfb argv while simplifying, which produces the *identical*
"Could not determine connected screen" symptom for a completely different reason. Both are easy
to reintroduce.

### The reproducible recipe

```
python advisor/probes/make_webtop_min_rootfs.py     # 2.4 GiB -> ~983 MiB
python advisor/probes/make_webtop_overlay.py        # nginx conf + dirs + launch script
#   ... then append advisor/probes/webtop_sitecustomize.py into the overlay as patch/sitecustomize.py

litebox_runner_linux_on_windows_userland.exe --unstable   --initial-files .wfgy/webtop_min.tar --resume-from .wfgy/webtop_overlay.tar   --publish 8081:8081   --env PYTHONPATH=/patch --env DISPLAY=:1 --env HOME=/config   --env PATH=/lsiopy/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin   -- /bin/sh -c 'Xvfb :1 -screen 0 1024x768x24 -dpi 96        +extension COMPOSITE +extension DAMAGE +extension RANDR +extension RENDER        +extension XFIXES +extension XTEST -nolisten tcp -ac -noreset & sleep 12;      xterm -geometry 100x30+30+30 -bg black -fg green -e sh -c "while true; do date; sleep 1; done" &      sleep 6; selkies --addr=localhost --mode=websockets        --audio-enabled=false --microphone-enabled=false --clipboard-enabled=false'

SELKIES_PORT=8081 python advisor/probes/hostproxy.py   # then open http://127.0.0.1:8090/
```

**Watch the port.** selkies binds its own default 8081 here; `CUSTOM_WS_PORT` did not take effect
in this configuration, so the proxy must target whatever
`data_websocket:... listening on port N` actually reports.

### Honest status

Not confirmed: frames rendering in the browser. Every code-level blocker found has been fixed and
each fix verified on its own, and the last one (`DISPLAY`) was identified from its own diagnostic
rather than guessed -- but no run after that fix stayed healthy long enough to reach the client's
first frame.

The obstacle is the host, not a known defect. Free memory oscillated between ~2.9 GB and ~0.4 GB
across these runs as the runner (~1.4 GB), Chrome and the editors competed; below roughly 2 GB the
guest stalls mid-startup -- consistently at gamepad initialisation in the last attempts -- in a
way indistinguishable from a hang. Runs that had the memory reached "Running server" in 25 s;
runs that did not never got there at all. Re-run the recipe above with more free memory to
confirm.

## Next steps, in dependency order

1. Fix the flank recommit properly, using the view's real allocation base and mapping
   copy-on-write protections down to their private-memory equivalents. Repro is
   `python3 -c "import pixelflux"`, ~30 s. This unblocks selkies, and selkies is the whole video
   path.
2. Then bring up MATE, which needs the `fork_verify` host-side AV addressed
   (`mate-session-avs-are-in-fork-verify-not-guest`). A lighter WM already present in the image
   (`openbox`, `labwc`) may be worth trying first purely to get window content on `:1`.
3. Browser verification needs Chrome to be allowed to reach `localhost` on this host.
