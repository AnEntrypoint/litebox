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
