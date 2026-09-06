# 2026-09-06: stock linuxserver/webtop:debian-i3, real selkies stack (no PIXELFLUX_WAYLAND bypass) -- blocked at s6-overlay preinit by the known vfork/execve MAP_FIXED collision

## Goal

Get browser-based access to a stock, unmodified **Debian**-based `linuxserver/webtop`
image working under litebox, with **video encoding for the display** -- i.e. the
image's real `selkies` stack (nginx + node.js + WebRTC screen streamer), not the
`PIXELFLUX_WAYLAND=true` bypass used in the prior `alpine-mate` session (Pass 319+,
`docs/AGENTS_ARCHIVE_2026-09-05.md` ~line 4763-4783), which skips `selkies`/nginx/
node entirely and paints directly via `labwc`. This session deliberately did **not**
set that env var.

## Tag selection: `debian-xfce` does not exist

Docker Hub's `linuxserver/webtop` tag list (`hub.docker.com/v2/repositories/linuxserver/webtop/tags`,
queried live this session) currently has no `debian-xfce` tag at all -- the XFCE
desktop variant is `ubuntu-xfce` only. The Debian-based variant is `debian-i3`
(i3 window manager). Since `selkies` (the browser-facing WebRTC streamer this task
targets) is desktop-environment-agnostic -- it streams whatever X/Wayland surface is
running, independent of window manager -- `debian-i3` is the correct stock choice to
satisfy "Debian-based" without inventing a non-existent tag or substituting a
non-Debian distro.

## Pull and pack

```
litebox_packager.exe --oci-image docker.io/linuxserver/webtop:debian-i3 \
  -o .wfgy/webtop-debian/webtop-debian-i3.tar -v
```

17 layers, dominant layer ~910MB. Final tar: **73,433 entries, 8.6GB** (much larger
than `alpine-mate`'s 2.6GB -- this Debian image bundles `chromium`, `docker`/`dockerd`,
`python3.13`, `git`, and PyAV's full ffmpeg-linked `.so` set under
`lsiopy/lib/python3.13/site-packages/av.libs/`, none of which alpine-mate carried).
All ELFs rewrite cleanly; zero rewrite failures. Tar is intentionally **not** committed
(far over the 50MB limit) -- lives in `.wfgy/webtop-debian/`, a scratch directory.

The packager's generated `litebox/config_and_run.sh` confirms this is the real,
unmodified selkies-bearing image (not a stripped-down variant): it sets
`SELKIES_ENCODER=x264enc,jpeg`, `SELKIES_INTERPOSER=/usr/lib/selkies_joystick_interposer.so`,
`SELKIES_WAYLAND_SOCKET_INDEX=2`, `DISPLAY=:1`, and falls through to `exec '/init'`
(s6-overlay) when no explicit command is given -- exactly the production boot path.

## Network/port-forwarding mechanism (confirmed, not yet exercised end-to-end)

Read `litebox_platform_windows_userland/src/net.rs` in full. litebox's guest network
stack is a **userspace NAT gateway** (`smoltcp` on both the guest and gateway side,
bridged via an in-process loopback queue) -- there is no real Windows network adapter
or TUN device involved, matching QEMU's `-netdev user`/gVisor's netstack approach so
that no Administrator privileges are needed.

- **Outbound** (guest-initiated TCP/UDP: `apk`, `curl`, DNS) is transparently proxied
  to real, unprivileged Winsock sockets -- already working, used throughout the OCI
  pull path.
- **Inbound** (host-initiated, i.e. a Windows browser reaching a guest server) is
  **opt-in and TCP-only**: `litebox_runner_linux_on_windows_userland.exe -p
  HOST_PORT:GUEST_PORT` (or `LITEBOX_PUBLISH=host:guest[,host:guest...]`) binds a real
  `127.0.0.1:<host_port>` `TcpListener` and forwards each accepted connection into a
  new guest-bound `smoltcp` TCP flow -- the direct analogue of `docker run -p` /
  QEMU slirp's `hostfwd`. Deliberately binds loopback only, not `0.0.0.0`.
- **Inbound UDP forwarding is explicitly NOT implemented** (net.rs's own module doc,
  line ~45: "not needed for the TCP-based web-UI use case this feature was built
  for"). This is a **known, real gap for this task's WebRTC requirement**: selkies'
  video/audio media path is WebRTC, which needs inbound UDP (ICE/STUN candidate
  gathering, SRTP media) to actually reach a real browser once nginx/node get far
  enough to try. Not yet reached this session (blocked earlier, see below), but
  flagged here so the next session doesn't have to rediscover it: getting nginx's
  HTTP(S) UI to load will need nothing more than `-p 3000:3000`, but getting the
  actual **video stream** (not just the page shell) to render will additionally need
  either (a) extending `NatGateway` with inbound UDP forwarding (symmetric to the
  existing TCP `--publish` path, given `UdpFlow`/`pump_udp` already exist for the
  outbound direction), or (b) confirming selkies can be configured to only need
  outbound-initiated UDP (unlikely for WebRTC's normal ICE flow, but selkies may
  support a TURN-relay-like mode -- not investigated this session).

## Boot attempt: real `/init`, no PIXELFLUX_WAYLAND

```
litebox_runner_linux_on_windows_userland.exe -Z \
  --initial-files .wfgy/webtop-debian/webtop-debian-i3.tar \
  --env PATH=/lsiopy/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
  --env HOME=/config --env LANG=en_US.UTF-8 --env TERM=xterm \
  --env S6_CMD_WAIT_FOR_SERVICES_MAXTIME=0 --env S6_VERBOSITY=2 \
  --env DISPLAY=:1 --env START_DOCKER=false --env PUID=1000 --env PGID=1000 \
  /init
```

(`START_DOCKER=false` set deliberately -- the image's own `dockerd`/nested-Docker
feature is out of scope for this task and would add an unrelated, large failure
surface; `PIXELFLUX_WAYLAND` deliberately absent.)

**Result: segfault within the first ~60ms of guest execution**, inside s6-overlay's
own `preinit` stage, before Xvfb/openbox/selkies/nginx/anything display-related is
ever reached.

## Root cause: the already-known vfork/execve `MAP_FIXED`-adjacency collision, now confirmed on a mainstream production init tree

Full trace (`LITEBOX_LOG=debug LITEBOX_DIAG_FATALDUMP=1`, preserved at
`.wfgy/webtop-debian/boot2.log`):

1. `s6-overlay-suexec` (pid 1) `vfork`s + execs `preinit`
   (`.../s6-overlay-3.2.1.0/libexec/preinit`) -- `clone` flags `0x4100`
   (`CLONE_VM|CLONE_VFORK`), confirmed via the debug log's own
   `clone: spawned new task ... flags=CloneFlags(16640)`.
2. `preinit` (pid 2) then `clone`s `s6-mkdir` (pid 3) -- flags `0x4000`
   (`CLONE_VFORK`).
3. `s6-mkdir`'s own `execve` fails while mapping its ET_EXEC PT_LOAD segment at its
   link-time-fixed address:
   ```
   DIAG_MMAP failed tid=3 addr=4194304 len=155648 flags=18 error=MapError(AddressInUse)
   sys_execve: load_program failed after point of no return, killing process with SIGSEGV
     tid=3 path=/command/s6-mkdir error=LoadError(Map(Errno(17 = EEXIST: File exists)))
   ```
   `4194304 = 0x400000`, the standard static/ET_EXEC link base -- already occupied
   because the vfork-duplicated address space `s6-mkdir` inherited (transitively,
   from `preinit`, itself vfork-duplicated from `s6-overlay-suexec`) still has that
   range mapped.
4. litebox correctly detects the failure **after the point of no return** (the old
   program's mappings are already torn down) and kills the process with `SIGSEGV`
   rather than leaving it in a corrupted half-loaded state -- the honest thing to do,
   not a bug in the detection/handling itself. `s6-overlay-suexec` reports this
   accurately as `child failed with exit code 139`, and s6-overlay's whole preinit
   stage aborts.

**This is not a new bug.** It is the exact, extensively pre-documented
`vfork-parent-wakes-during-nested-child-execve` investigation (see `.gm/prd.yml`,
same id) -- previously reproduced only via `gcc`/`cc1` (a nested-vfork C compiler
invocation). That row's own history already records:

- The real mechanism: litebox's `sys_clone` never honors `CLONE_VM` for
  `CLONE_VFORK` the way real Linux does (real Linux shares one address space for the
  vfork window; litebox always builds the child via `pm.duplicate()`, a full eager
  copy) -- so a numeric collision between the child's new ELF's fixed addresses and
  the duplicated copy's own occupied ranges is a litebox-specific artifact with no
  real-Linux analogue.
- Two concrete fix attempts already tried and found insufficient, live-verified,
  code reverted both times:
  1. Naive `pm` reference-sharing (skip `pm.duplicate()` for the vfork child):
     confirmed *actively unsafe* by code inspection -- `sys_execve`'s `ElfLoader::load`
     maps the new ELF directly onto the process's existing `pm` in place, so a shared
     `pm` would have the child's execve corrupt the *parent's own live memory*
     mid-vfork-window, strictly worse than today's clean collision-and-kill.
  2. Selective stack-only `Vmem::duplicate` (only duplicate the vfork child's stack
     coherence group, not the whole address space): implemented, compiled, and
     live-tested against the real `gcc`/`cc1` repro -- **produced the byte-for-byte
     identical collision**, because Windows' `Hint`-mode mapping placement
     (`litebox_platform_windows_userland/src/lib.rs` ~3567-3579) has zero bias toward
     a high address once its suggested (high, already-parent-occupied) source address
     is unavailable, and falls back to `base_addr = null` (OS picks *any* address) --
     which, for a *smaller* duplicated footprint, if anything makes a low-address
     placement colliding with a small ET_EXEC's fixed range *more* likely, not less.

**What is new this session**: confirmation that this collision class is not confined
to a C-compiler's own nested-vfork pattern -- it fires on `s6-overlay`'s own
`preinit`/`s6-mkdir` sequence, i.e. **the very first few syscalls of any
`s6-overlay`-based container's init tree**. `s6-mkdir` is one of several tiny static
`execline`/`s6-portable-utils` helper binaries (`s6-echo`, `s6-touch`, and
`execline`'s own built-ins are structurally identical: small static ET_EXEC binaries,
spawned via `vfork`+`exec` from a supervisor process for cheapness) that s6-overlay's
init tree invokes constantly. This raises the practical priority of that PRD row
substantially: it is not a rare C-toolchain edge case, it is a near-certain blocker
for **any** s6-overlay-based container image under litebox, webtop included.

## Why this session stopped here

Per this row's own already-recorded next steps, a real fix needs one of:

1. A new `FixedAddressBehavior` variant (or equivalent) that biases `Hint`-mode
   placement toward a *high* address (matching a stack's conventional placement)
   instead of falling back to an unbiased `null`/OS-chosen address once the
   suggested source address is unavailable -- scoped to
   `litebox_platform_windows_userland/src/lib.rs` ~3556-3610.
2. The larger, structurally correct fix: genuine temporary `CLONE_VM` sharing for
   the `CLONE_VFORK` case (the child temporarily borrows the parent's live `pm`/
   address-space ownership instead of eagerly duplicating it, matching real Linux's
   `mm_struct`-swap-on-execve semantics), transferring or discarding ownership at
   execve/exit. This eliminates the whole collision *class* rather than
   incrementally reducing its probability, and would very likely fix every member of
   this bug family (webtop's `s6-mkdir`, `gcc`'s `cc1`, and any other vfork+small-ELF
   pattern) at the root, at the cost of real address-space-ownership-transfer design
   work (safe rollback if `_exit` is called instead of `execve`, fd-table/thread-
   identity interaction).

Both are real, non-trivial, multi-session-scale engineering efforts against a
primitive (`Vmem::duplicate`'s coherence-group relocation) that has already caused
one real corruption bug when handled naively (per that PRD row's own history) -- not
something to force blind in this pass. Per this project's standing discipline
(documented repeatedly across this row's own history) against forcing an increasingly
complex, unverified change, this session did **not** attempt a third fix.

## Status / what's proven vs. not

**Proven this session:**
- `linuxserver/webtop:debian-i3` (stock, unmodified, real Debian base with the real
  `selkies` stack) pulls and packs cleanly through litebox's existing OCI pipeline at
  production scale (8.6GB, 73k entries, zero rewrite failures).
- litebox's `-p`/`LITEBOX_PUBLISH` TCP port-forwarding mechanism exists and is
  documented as the correct tool for exposing nginx's port 3000 to a host browser,
  once boot gets that far.
- The `PIXELFLUX_WAYLAND` bypass is confirmed to be purely an **image-side**
  behavior switch (the image's own `init-selkies-config` script checks it), not any
  litebox mechanism -- omitting it is sufficient to keep selkies in scope, no litebox
  flag needed to "not bypass" it.

**Not reached, blocked by the above:**
- s6-overlay's `preinit` stage (before Xvfb, before selkies, before nginx/node.js
  start at all).
- Everything downstream of that: the actual selkies/nginx/node WebRTC stack, its
  UDP/ICE requirements, and browser verification via `claude-in-chrome`.

**Next step for whoever picks this up:** land one of the two fixes above for
`vfork-parent-wakes-during-nested-child-execve`, re-run this exact boot command, and
continue from wherever s6-overlay's preinit gets to next (there will likely be
further, currently-undiscovered blockers once past this one, given how early it is
in the boot sequence -- this is genuinely the very first non-trivial syscall
interaction of the whole container). The inbound-UDP-forwarding gap noted above will
also need addressing before WebRTC video can reach a real browser, once boot
progresses far enough to matter.

## 2026-09-06, continued: the genuine CLONE_VM fix was already on `main` -- and still does not fix this

Picked this up expecting to implement fix option 2 above (genuine temporary `CLONE_VM`
sharing for `CLONE_VFORK`). Before writing any code, `grep`'d for `CLONE_VFORK`/
`pm.duplicate` in `litebox_shim_linux/src/syscalls/process.rs` per this row's own
standing instruction to read the current state first -- and found that fix **already
implemented and committed**, apparently by a session that ran concurrently with or
just after this doc's own initial boot-repro session and never cross-referenced it:
commits `99a49a5` ("Implement genuine CLONE_VM address-space sharing for
CLONE_VFORK", 2026-08-26) and `38231f0` ("Detach vfork()'d child's PageManager
before ordinary exit, not just execve", 2026-08-28) -- both **before** this very
doc's initial boot-repro session (2026-09-06), meaning that session hit the bug on a
binary that already had the fix compiled in and did not realize it, or built from a
stale binary. Neither this doc nor the PRD row's own history mentioned the fix
existing, so this continuation re-discovered it via source inspection rather than
being told.

The implementation itself is well-designed and matches this row's own "NEXT STEP"
sketch closely: `Process::pm` became `Mutex<Arc<PageManager>>`; a `CLONE_VFORK`
clone (regardless of whether `CLONE_VM` is also set -- confirmed correct, since real
`vfork()` sometimes issues just `CLONE_VFORK` alone, e.g. s6-overlay's own
`preinit`-to-`s6-mkdir` clone, flags `0x4000`, no `CLONE_VM` bit) sets the child's
`dest_pm` to `Arc::clone` of the parent's live `pm` instead of duplicating it;
`Process::detach_pm_for_vfork_execve` swaps in a brand-new, empty `PageManager` at
the top of `sys_execve`'s point-of-no-return section (and, per the second commit,
also on ordinary `_exit`/thread-drop, covering the "child crashes/exits without
execve" case this row's own task brief flagged as a real risk) -- so the child never
touches the parent's live memory during its own execve or exit teardown.

**Rebuilt fresh and re-ran the exact boot command from this doc's top section.**
`cargo build --release` against the default `target/` failed with a Windows
`Access is denied` removing the old `.exe` -- a genuinely unkillable zombie
`litebox_runner_linux_on_windows_userland.exe` process (PID 7640, `Get-Process`
reported `HasExited: True` yet `tasklist`/file-lock behavior said otherwise) was
holding a handle; worked around by building into a separate `CARGO_TARGET_DIR`
(`target-vforkfix/`) rather than fighting the zombie. **The identical crash from
this doc's very first section reproduces byte-for-byte on top of the already-landed
fix**: `s6-mkdir` (pid 3) still dies with `DIAG_MMAP failed ... error=MapError(AddressInUse)`
/ `sys_execve: load_program failed after point of no return, killing process with
SIGSEGV ... error=LoadError(Map(Errno(17 = EEXIST)))` at guest address `4194304`
(`0x400000`). Full log: `.wfgy/webtop-debian/boot3.log`.

**Root-caused precisely why the already-landed fix doesn't work**, via
`allocate_pages: DIAG claim_range` correlation in the debug log:
`litebox_platform_windows_userland` runs every guest "process" as an ordinary host
**thread sharing one real Windows process's own address space** (the crate's own
comments confirm this explicitly, lib.rs ~3993-4206) -- guest virtual addresses
literally ARE host virtual addresses, with no per-guest-process real address-space
isolation at all. This is the entire reason `fork()` needs `Vmem::duplicate()`'s
address-relocation machinery to begin with. Genuine `CLONE_VM` sharing (same
`Arc<PageManager>`, same real memory) is completely fine for the SHARING half of
vfork's contract -- but the DETACH half is structurally impossible to satisfy in
this model: `detach_pm_for_vfork_execve` swaps in a bookkeeping-empty
`PageManager::new()`, but the REAL Windows memory the old shared `pm` had mapped
(e.g. wherever `s6-overlay-suexec`'s own ELF was originally loaded, inherited
transitively through the vfork chain to `s6-mkdir`) is never actually freed --
it CAN'T be, because that same physical memory still legitimately belongs to the
live, merely-suspended PARENT (`preinit`), which will resume using it once the
child execve's or exits. So `s6-mkdir`'s own `NoReplace`-mode fixed mapping at its
ET_EXEC link address genuinely, physically collides with real committed Windows
memory -- `allocate_pages`'s `has_committed_page && NoReplace => AddressInUse` check
(lib.rs ~6015-6018) consults only the OS's real page-commit state, never the
`PageManager`/`CLAIMED_RANGES` bookkeeping this fix operates on -- so the collision
happens one full layer below where a `PageManager`-object swap can ever reach.
Traced further: there is no way to distinguish "memory only the detaching child's
old view needed" from "memory the still-live parent needs kept", because before
detach both processes held literally the same `Arc<PageManager>` (same allocation,
same refcount) -- dropping the child's clone of that `Arc` on detach correctly
leaves the parent's own reference (and the real memory it points to) fully intact,
but there was never a child-exclusive subset of that memory to reclaim in the first
place. A `PageManager`-level fix structurally cannot close this gap.

**This means the real fix needs actual OS-level process separation for the vfork
child once it detaches, not another address-space bookkeeping change** --
`litebox_shim_linux`/`litebox_platform_windows_userland` already has exactly this
mechanism, just not wired up for vfork: `spawn_cross_process_fork_child` (behind
`LITEBOX_PROCESS_FORK=1`) spawns a REAL separate Windows process for a plain
`fork()` child when its fd table is simple enough (`fd_complexity.beyond_stdio ==
0`). This exact `s6-mkdir` repro fails that eligibility gate (`beyond_stdio=1`,
confirmed in this session's own log) so it never reaches that path today even
though it exists. The most promising next step for a dedicated follow-up session is
investigating whether that mechanism can be extended (or a vfork-specific sibling
built) to cover the vfork-detach-at-execve moment specifically, since vfork's own
narrow contract (parent fully suspended, child only touches its own stack before
execve/exit) may make its fd/register-transfer precondition easier to satisfy than
general `fork()`'s.

**No code was changed or reverted this session** -- the existing fix
(99a49a5/38231f0) remains committed on `main`, is a correct and reasonable partial
step (its bookkeeping/safety half is sound), just insufficient alone to fix the
actual collision. Consistent with this row's own standing discipline (and this
session's own task brief) against forcing a fourth fix attempt blind without full
understanding: the real blocker (litebox's single shared real address space,
previously unidentified by any prior session on this row) is now precisely located,
which is genuine forward progress even without new code this pass. `.gm/prd.yml`'s
`vfork-parent-wakes-during-nested-child-execve` row has the full technical
trace. Full regression test suite (`cargo test --workspace`) was not run this
session since no code changed.

## Follow-up session: investigated extending `spawn_cross_process_fork_child` to
## `CLONE_VFORK` specifically -- found it structurally cannot work yet, no code changed

Task brief for this session: extend the cross-process fork mechanism
(`spawn_cross_process_fork_child`) to cover the `s6-mkdir` vfork case specifically,
reasoning that vfork's simpler fd-sharing contract (child only touches its own stack
before execve/exit, parent fully suspended for the whole window) might let it dodge
the `fd_complexity.beyond_stdio == 0` gate that blocks general `fork()`. Read
`advisor/ADVISORY-002-d-zero-fork.md` in full and traced every `fd_complexity`/
`beyond_stdio` call site plus the actual `spawn_cross_process_fork_child`/
`spawn_process_fork_child` implementation before writing any code. Conclusion:
**do not implement this yet -- three independent, load-bearing gaps make it unsafe
to ship, not merely hard.** No code changed this session.

### Gap 1: vfork's own relocation map is empty, and the cross-process spawner needs
### a real one

Today's vfork path (`do_clone`, `litebox_shim_linux/src/syscalls/process.rs` ~2529-2542)
deliberately does NOT call `pm().duplicate()` at all for `CLONE_VFORK` -- it shares
the parent's `Arc<PageManager>` directly and builds an explicitly EMPTY
`AddressRelocations` (`from_raw_parts_for_diagnostic` with every vec `Vec::new()`),
because there is nothing to relocate when nothing was duplicated. But
`spawn_cross_process_fork_child` (`litebox_platform_windows_userland/src/lib.rs:8403`)
feeds that same `relocations` object's `.group_relocations()` and `.vma_layout()`
straight into `spawn_process_fork_child` as the literal list of memory ranges to
`WriteProcessMemory` into the new child process (`process_fork.rs:1106-1135`). For
vfork's empty relocations, both of those are empty vectors -- the mechanism as it
exists today would spawn a new Windows process and copy **zero bytes of guest
memory** into it. Routing vfork through this path requires first building a real
enumeration of the parent's VMAs for the vfork case (identity-mapped, since nothing
should relocate) -- not just flipping the `beyond_stdio` gate. This is new work, not
a two-line change.

### Gap 2: the cross-process path has no vfork-suspend/signal contract at all, and
### returns before the code that implements one

`do_clone`'s cross-process branch (`process.rs:3190-3209`) `return`s immediately on
success, before ever reaching `vfork_child_process.wait_for_vfork_done()` at
`process.rs:3394-3396` -- that call only exists on the thread-based path taken
further down the same function. Wiring a vforked call into the cross-process branch
today would silently skip the entire "parent blocks until child execve/exits"
contract vfork's whole point depends on: the parent would return from `vfork()`
immediately, racing the child exactly the way real Linux never allows and exactly
the hazard `wait_for_vfork_done`/`signal_vfork_done` exist to prevent. There is no
existing cross-process signal for "child reached execve" (as distinct from "child
exited", which the existing `cross_process_children`/`wait4` HANDLE-wait bridge
already covers) -- this would need a new primitive built on
`litebox_platform_windows_userland/src/xproc_sync.rs`'s `CrossProcessEvent` (a named
Windows event; the primitive exists and is well-suited, but nothing wires it to an
execve-success or _exit callback in the child today), plumbed through both the
child's `sys_execve`/exit path and the parent's `wait_for_vfork_done`.

### Gap 3: the cross-process child has no shim-level state at all -- it cannot
### actually run `s6-mkdir`'s execve, only a diagnostic no-op

This is the decisive gap. Traced `spawn_process_fork_child`
(`process_fork.rs:1106`) through to what the spawned child process actually runs:
it re-execs the runner binary itself with `LITEBOX_PROCESS_FORK_CHILD=1`-style env
vars, landing in `run_diagnostic_resume_child()` (`process_fork.rs:143`), which
constructs a bare `WindowsUserland` platform instance and parks for a raw
`SetThreadContext` register injection (`park_for_real_resume_injection`) -- the
guest's CPU resumes mid-instruction with **no `Task`, no `Process`, no fd table, no
mount/VFS state, no `GlobalState` at all** reconstructed for it. The one place that
DOES build a fresh shim stack in the child,
`diag_process_fork_globalstate_probe`/`diag_process_fork_vmem_adopt_probe`/
`diag_process_fork_task_resume_probe` (`litebox_runner_linux_on_windows_userland/
src/lib.rs:1152-1334`), is explicitly diagnostic-only (gated behind three separate
`LITEBOX_DIAG_PROCESS_FORK_*` env vars, never set by the production
`spawn_cross_process_fork_child` call path) and even then constructs a **brand new,
empty** `GlobalState`/`DefaultFS` by re-reading the guest rootfs tar from scratch
(`FORK_CHILD_TAR_PATH_ENV_VAR`) -- it does not, and structurally cannot yet,
transplant the parent's live mount state (any writes/mounts already made during
boot), its live fd table (fds 0/1/2 need to resolve to the SAME pipe/tty/socket
objects the parent has open, not fresh ones), or its pid/tid so `wait4` continues
working. `s6-mkdir`'s real execve needs exactly this state to resolve its own path
and inherit working stdio. This confirms `advisor/ADVISORY-002-d-zero-fork.md`
section 3.3/3.4's "the hard part is the pointer-rich kernel-equivalent state living
in one process's private heap" is not merely a general-fork concern -- vfork's
simpler fd contract does not remove the need for the child to have *some* working
shim state to make its next syscall at all, and today it has none.

### Why this rules out a narrow vfork-only carve-out, per this session's own brief

The task brief explicitly asked me to determine whether vfork's simpler semantics
(no long-lived shared-fd-table concurrency, child immediately execve/_exit-bound)
make cross-process spawning safer or easier than the general fork case. The answer,
confirmed by tracing the actual code rather than assuming from the contract alone:
vfork's semantics remove the *concurrency* problem (no racing writer on a shared fd
table, since the parent is fully suspended) but do **not** remove the *existence*
problem -- the child still needs a real, working shim-level process object
(`Task`/`Process`/fd table/mount state) to execute even its own imminent `execve`
syscall, and building that transplant is exactly `ADVISORY-002`'s Track B items 2-4
(cross-process `RawMutex`, fixed-base shared kernel heap behind `SafeZoneAllocator`,
HANDLE indirection for fd objects) -- none of which exist yet, for vfork or general
fork alike. `ADVISORY-002` section 6 says explicitly: "Do not build the vfork-only
shortcut as the general fork: the XFCE daemons fork without exec and the 'parent
parked until child exits' semantic deadlocks them" -- read in light of this
session's tracing, that warning is really about not diverging from Track B's
ordered plan, and the same ordered plan (presenter split -> cross-process
`RawMutex` -> fixed-base shared kernel heap -> relax `beyond_stdio` incrementally)
is the correct path for the vfork case too, not a shortcut around it. Attempting a
narrow vfork-specific version of steps 2-4 above, scoped smaller than the full
general-fork version, might be tractable as dedicated future work, but is a
substantial new design-and-build effort in its own right (a new cross-process
execve-done signal, a real vfork-specific VMA enumeration, and at minimum a
transplanted fd 0/1/2 + mount-state-visible-enough-for-path-resolution shim child)
-- not a gate flip, and not safely forceable blind in one session per this
project's own standing discipline (three prior fix attempts already failed on this
exact row; a fourth blind one would compound the debt, which is exactly what this
session's brief asked me to avoid if the design didn't hold up).

### Status

No code changed this session (`git status --porcelain` clean, confirmed). The
`vfork-parent-wakes-during-nested-child-execve` PRD row's real blocker (single
shared real Windows address space, identified by the immediately preceding session)
stands unchanged; this session adds that the previously-suggested next step
("investigate extending `spawn_cross_process_fork_child` for vfork") is not a
small follow-up but requires Track B's cross-process kernel-state infrastructure to
exist first, for either vfork or general fork. The webtop boot repro
(`linuxserver/webtop:debian-i3`'s real `/init`) was not re-run this session, since
no code changed that could affect its outcome -- it remains blocked at the exact
point documented in the immediately preceding session (s6-mkdir's execve hitting
`AddressInUse`/`EEXIST` at `0x400000`). Recommended next step for a dedicated
session: start Track B at the top (`docs/presenter-process-design.md`'s presenter
split, ADVISORY-002 section 6 step 1), not at the `beyond_stdio` gate -- the gate is
a downstream consequence of the missing shared-kernel-state infrastructure, not an
independent knob.
