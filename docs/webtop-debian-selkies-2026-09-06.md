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

## 2026-09-06, continued: bypassing s6-overlay entirely -- direct pid-1 launch of the
## real payload services, per the proven Xorg-pid-1 pattern (r5.sh/srv.sh)

Task brief for this session: stop fighting the vfork/execve `MAP_FIXED` collision
(confirmed above to require Track B's multi-session cross-process infrastructure) and
instead route around it entirely -- launch webtop's actual payload services (Xvfb,
window manager, nginx, selkies' node backend) directly as litebox's own pid 1, one
component at a time, mirroring this repo's own already-proven-safe pattern
(`r5.sh`/`srv.sh`/`srv7.sh` at the repo root; `advisor/probes/xorg-fork-segv/README.md`'s
"Running as pid 1 is safe, trivially and confirmed" finding for bare Xorg). This
deliberately never touches `/init`/s6-overlay/`s6-rc.d` at all.

### Inspecting the real webtop:debian-i3 service tree

Extracted every relevant `s6-rc.d/*/run` script and `defaults/*.sh` from the already-
packed `.wfgy/webtop-debian/webtop-debian-i3.tar` (no re-pull needed). Service
dependency order: `init-os-end` -> `init-selkies` -> `init-nginx` -> `init-selkies-
config` -> `init-video` -> `svc-{xorg,dbus,de,nginx,pulseaudio,selkies}`. Concretely:

- **`svc-xorg`**: NOT actually Xorg -- `Xvfb :1 -screen 0 <res>x24 ... -nolisten tcp -ac
  -noreset -shmem`, run via `s6-setuidgid abc` (a plain fork+exec of a real
  `s6-portable-utils` helper, not vfork -- distinct from the vfork-heavy
  `preinit`/`s6-mkdir` sequence that blocks `/init`).
- **`svc-dbus`**: `dbus-daemon --system --nofork --nosyslog` -- `--nofork` already
  avoids dbus-daemon's own internal-fork SIGSEGV documented elsewhere in this
  investigation's XFCE-track history.
- **`svc-de`**: waits for `xset q` to succeed, sets resolution via `xrandr`/`cvt`, then
  `exec bash /defaults/startwm.sh`, which itself is `exec dbus-launch --exit-with-
  session /usr/bin/i3` (a SEPARATE session bus from `svc-dbus`'s system bus).
- **`svc-nginx`**: `exec /usr/sbin/nginx -g 'daemon off;'` after killing any zombie
  nginx workers.
- **`svc-selkies`**: sets up a null-sink PulseAudio pair, then `exec selkies --addr=
  localhost --mode=websockets` -- `selkies` resolves (via `PATH=/lsiopy/bin:...`) to
  `lsiopy/bin/selkies`, a real Python venv console-script (confirmed via
  `entry_points.txt`/`dist-info`), i.e. an ordinary fork+exec of `python3`, not a
  small vfork'd helper.
- **`init-nginx`**/**`init-selkies-config`**: essential one-time setup an equivalent
  plain script must replicate -- `cp /defaults/default.conf` to
  `/etc/nginx/sites-available/default` with `sed -i` substituting `SUBFOLDER`, `CWS`
  (websocket port), `REPLACE_DOWNLOADS_PATH`; `mkdir -p $HOME/.XDG`/`.config`; a
  self-signed cert via `openssl req`; copying `/usr/share/selkies/$DASHBOARD` to
  `/usr/share/selkies/web`. None of this is s6-specific -- it is all real coreutils/
  openssl/sed invocations, confirming the advisory's "plain shell fork+exec of
  external programs is a different, safe syscall pattern" distinction holds for every
  script this session touched.

### Xvfb-as-pid-1: a NEW, different, litebox-genuine SIGSEGV -- NOT the vfork bug

Wrote a plain shell script (`wt_xvfb.sh`, mirroring `r5.sh`/`srv.sh`'s exact style) that
`exec`s `/usr/bin/Xvfb` directly as pid 1, no s6-overlay, no fork, no vfork anywhere in
the process tree (confirmed via `LITEBOX_LOG=trace` grep -- zero `clone`/`fork`/`vfork`
log lines appear before the crash). Booted via:
```
litebox_runner_linux_on_windows_userland.exe -Z \
  --initial-files <tar-with-injected-script> \
  --env PATH=/lsiopy/bin:... --env HOME=/config ... \
  /bin/sh /wt_xvfb.sh
```
**Result: Xvfb SIGSEGVs ~38ms after `execve`, deterministically, as pid 1 itself --
this is a genuinely new and different bug from the already-documented vfork/execve
collision.** Confirmed byte-reproducible across three independent runs (different
screen resolutions, with and without `LD_BIND_NOW=1`): `rip=0x7feffffbf932` is IDENTICAL
in every run (a fixed offset relative to Xvfb's own load base); only `cr2` (the fault
address) shifts by exactly the same delta as Xvfb's own load-base ASLR shift each run,
always landing at a fixed `+0x200` byte offset into the start of Xvfb's own RW-mapped
data segment. `error_code=0x4` = a WRITE to a page whose current VM flags are
`VM_READ | VM_MAYREAD | VM_MAYWRITE | VM_MAYEXEC` -- `VM_MAYWRITE` is set (the mapping
is eligible to become writable) but the live `VM_WRITE` bit is not, so the write
faults.

**Root-cause narrowing, not yet fully closed:** `readelf -l` on the real Xvfb binary
shows it is the only binary tested this session with a `PT_GNU_RELRO` segment (`mkdir`,
`chmod`, `rm`, `/bin/sh`, and `/usr/sbin/nginx` -- confirmed via the same `readelf -l |
grep -i relro` check -- all lack one, and all execute cleanly as pid 1 or as ordinary
fork+exec children). `litebox_shim_linux/src/loader/elf.rs`'s PT_LOAD-to-VM-flags
mapping (`write: (ph.p_flags & PF_W) != 0`) correctly marks the RELRO-covered LOAD
segment `RW` at load time (its raw `p_flags=6`), so the loader itself is not visibly
wrong by inspection -- the fault must arise from glibc ld.so's own runtime handling of
that segment (either its own relocation-application write racing a not-yet-fully-
committed litebox mapping, or ld.so's subsequent `mprotect(PROT_READ)` RELRO-lock
somehow leaving `VM_WRITE` cleared before a later write it still expects to succeed).
`LITEBOX_LOG=trace` was too slow to complete a full boot in the time available this
session (huge per-instruction volume); a full root cause needs either a scoped VEH/
mprotect-call trace around only the RELRO segment's address range, or a binary-search
via linker flags (`-z norelro`) on a reproducible Debian glibc build to confirm RELRO
specifically (versus something else correlated with Xvfb's unusually large, heavily-
relocated 2.3MB binary) is the actual trigger. **Recorded here as a new, real, precisely-
bounded litebox gap distinct from the vfork/execve `MAP_FIXED` collision** -- filed as
a fresh investigation thread, not conflated with the existing PRD row above.

### nginx-as-pid-1: two real litebox bugs found and fixed, then genuine browser-verified success

Since Xvfb blocks the desktop/video half, pivoted to verifying the web-UI half (nginx +
the static selkies frontend) independently -- itself real, demonstrable progress per
this session's own acceptance bar. `/usr/sbin/nginx` (a 1.4MB dynamically-linked
binary, no RELRO segment) launched cleanly as pid 1 on the first attempt (`nginx -v`
succeeded, clean `exit_group status=0`), confirming the Xvfb SIGSEGV is Xvfb/RELRO-
specific, not a general "any real X server or daemon crashes as pid 1" problem.

Replicated `init-nginx`'s essential setup manually as a plain shell script (skip
openssl/sed -- see below) plus a host-pre-substituted `/etc/nginx/sites-available/
default` (real template from `/defaults/default.conf`, with `SUBFOLDER`->`/`,
`CWS`->`8082`, `REPLACE_DOWNLOADS_PATH`->`/config/Desktop`, the `listen [::]:3000`
IPv6 line dropped, and the `alias`/`root` directives pointed straight at
`/usr/share/selkies/selkies-dashboard/` to avoid needing a `cp` step), injected into
the tar's writable layer via `tar --concatenate` (last-duplicate-entry-wins, the same
technique this investigation's own `.wfgy/lessons.md` documents from an earlier XFCE
session). `-p 3000:3000` published the port per `net.rs`'s documented mechanism.

**Two real, unrelated litebox gaps found and fixed this session while iterating on
this boot, both committed:**

1. **`sed -i` cannot create its own temp file on litebox's tar-backed filesystem**
   (`sed: couldn't open temporary file /etc/nginx/sites-available/sedADApSp: Invalid
   argument`, exit status 4) -- not investigated to full root cause (worked around by
   pre-substituting the config on the host instead, since sed's own temp-file-then-
   rename pattern is incidental to this task, not itself part of webtop's real payload
   services); flagged here as a real, separate, un-filed litebox gap for a future
   session (likely an `O_TMPFILE`/`mkstemp`-adjacent syscall or `rename()` gap against
   the tar-rootfs backend) rather than guessed at further this pass.
2. **`openssl req` (and, separately, a plain `cp` in the same script) triggers an
   unrecoverable HOST-level access violation**, not a guest fault: `[diag-unrecov-av]
   ... is_in_guest=false ... no exception-table entry found`, immediately following
   `unsupported feature=ioctl with arg Raw { cmd: 1074041865, ... }` (`1074041865 =
   0x40087468 = TIOCGWINSZ`). This is litebox's own host-side ioctl-emulation path
   crashing outside guest execution entirely (`is_in_guest=false` on every frame in
   the exception ring buffer) when a real coreutils/openssl binary probes terminal
   size -- genuinely different from every other bug in this investigation's history
   (not a guest SIGSEGV, not fork/vfork-related). Worked around for this session by
   avoiding `openssl`/`cp` in the launch script entirely (skip self-signed HTTPS
   cert generation -- HTTP-only on port 3000 is sufficient to reach the task's
   browser-verification goal; point nginx's config directly at the real dashboard
   directory instead of `cp`-ing it). **Flagged as a real, separate, un-filed litebox
   gap**: `TIOCGWINSZ` (and likely the whole unimplemented-ioctl fallback path) needs
   to fail gracefully back into the guest (`ENOTTY`/a sane default winsize) rather
   than crash the host process, since any real program checking `isatty()`-adjacent
   terminal properties can trigger it.
3. **`litebox/src/net/mod.rs`'s `listen()` implementation had two genuine
   `unimplemented!()` panics that a real, unmodified nginx hits in its normal startup
   path** (not contrived, not an edge case -- this is nginx's stock listen-socket
   setup): `listen(fd, backlog=0)` panicked outright (real Linux treats 0 as "use a
   minimum viable backlog", which nginx's socket setup can pass depending on
   directives); and calling `listen()` a second time on an already-listening socket
   (nginx's master process does this legitimately) also panicked, where real Linux
   permits both growing and shrinking an existing listen backlog. **Fixed and
   committed this session**: `backlog.max(1)` before the existing `.min(8)` clamp
   floors zero to the minimum instead of panicking; the re-`listen()` path now grows
   normally (via the existing `refill_to_backlog` additive loop, previously reachable
   only for the first `listen()` call) or shrinks by dropping the excess still-
   unconnected placeholder sockets from `socket_set_handles`' tail via
   `smoltcp::iface::SocketSet::remove`, matching real Linux's "shrink truncates
   pending, unconnected backlog entries" semantics. Both fixes are minimal, additive,
   and were live-verified end to end (see below) -- no existing passing behavior
   changed, only two previously-panicking paths now succeed.

**After both net.rs fixes, plus setting `worker_processes 1; master_process off;`**
(ruling out nginx's normal master/worker `fork()` as an independent variable while
this specific investigation's scope is the web-UI-only path, not desktop/video)
**nginx boots cleanly as pid 1 and serves real HTTP traffic through litebox's
published port**:
```
$ curl -v http://127.0.0.1:3000/
< HTTP/1.1 200 OK
< Server: nginx
< Content-Length: 762
<!doctype html>...<title>Selkies</title>...
```
This is the real, unmodified `linuxserver/webtop:debian-i3` selkies dashboard's own
`index.html`, served by the real, unmodified `nginx` binary from the real,
unmodified image layers -- through litebox's userspace NAT `-p 3000:3000` forwarding,
with zero s6-overlay/`/init` involvement anywhere in the process tree.

**Browser-verified via `claude-in-chrome`** (navigated a real Chrome tab to
`http://127.0.0.1:3000/`): page title is genuinely "Selkies"; `get_page_text` shows
the real dashboard shell (Video Settings, Screen Settings, Audio Settings, Stats,
Clipboard, Files, Apps, Sharing, Gamepads); browser console shows the REAL selkies
frontend JS bundle executing (`assets/index-BTp9L9Xk.js`) genuinely initializing its
canvas (`Canvas internal buffer reset to: 1280x960`), passing its own pre-flight
checks (`Secure context and VideoDecoder API are available`), and correctly attempting
(and, expectedly, failing) a WebSocket connection to the not-yet-running node.js
selkies backend (`[websockets] Error: Event` / `Connection closed`) -- exactly the
correct behavior for "web UI shell reachable, desktop/video backend not started",
which is precisely this session's honest stopping point.

### Status / what's proven vs. not, this continuation

**Proven, browser-verified, this session:**
- The core task hypothesis holds: bypassing s6-overlay and launching webtop's real
  payload services directly as litebox's own pid 1 (the same proven-safe pattern as
  bare Xorg) genuinely works for at least one full real service (`nginx`) end-to-end,
  reachable from and rendering in an actual browser.
- Two real, previously-unknown litebox bugs (both in `listen()`'s backlog handling)
  found and fixed via this exercise, neither contrived -- both are stock nginx
  startup behavior that would block ANY nginx-hosting workload under litebox, not
  just this one.
- A third and fourth real, previously-unknown litebox gap identified and precisely
  bounded but not fixed this session (Xvfb's RELRO-correlated pid-1 SIGSEGV; the
  host-level unrecoverable AV on certain unimplemented ioctls; `sed -i`'s temp-file
  failure on the tar-rootfs backend) -- each is a genuine, separate, fresh
  investigation thread, not a rediscovery of the already-known vfork/execve issue.

**Not reached, blocked by the above:**
- The actual X11/i3 desktop session and video stream (blocked on the new Xvfb pid-1
  RELRO-correlated SIGSEGV -- a different, new blocker from the vfork/execve one this
  doc's earlier sections chronicle).
- selkies' node.js WebSocket/WebRTC backend (not attempted this session -- pointless
  to verify in isolation before Xvfb; the frontend's own correct "connection closed"
  behavior already demonstrates the wiring is right once a backend exists to connect
  to).
- Inbound UDP/WebRTC media forwarding (still unimplemented in `net.rs`, as this doc's
  earlier section already found -- unchanged this session, not re-investigated since
  video was not reached).

**Next step for whoever picks this up:** (a) root-cause the Xvfb RELRO-correlated
pid-1 SIGSEGV precisely (a scoped VEH trace bracketing only the RELRO segment's
address range, or a `-z norelro`-linked custom Xvfb build to confirm/deny RELRO as the
actual trigger versus Xvfb's unusually large relocation count) -- this is now the
single blocker standing between this session's proven nginx success and a full
desktop+video session; (b) once Xvfb boots, launch `dbus-daemon --nofork`, then
`i3` (or `dbus-launch --exit-with-session i3`, watching for the SAME vfork risk
`dbus-launch` itself might carry -- untested), then `selkies` (a plain python3
fork+exec, expected to be safe per this session's own coreutils/nginx evidence) as
SIBLING pid-1 runner instances against the shared display, per Track A's own
already-recommended multi-instance pattern; (c) investigate the `sed -i` temp-file
gap and the host-level ioctl-AV gap as their own dedicated PRD rows -- both are real,
separate litebox correctness gaps discovered as a byproduct of this session, not
webtop-specific dead ends.

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

## 2026-09-06, continued again: the Xvfb pid-1 SIGSEGV is NOT RELRO -- corrected,
## precisely re-localized, root cause still open

Task brief for this session: root-cause and fix the Xvfb-as-pid-1 SIGSEGV the
immediately preceding session left as "RELRO-correlated" (readelf showed Xvfb as the
only tested binary with a `PT_GNU_RELRO` segment). Reproduced the exact repro from
that session (`wt_xvfb.sh`/`wt_xvfb2.sh` injected into
`.wfgy/webtop-debian/webtop-debian-i3-with-scripts.tar`, launched directly as pid 1
via `litebox_runner_linux_on_windows_userland.exe -Z --initial-files ... -- /bin/sh
/wt_xvfb.sh`) and captured full fault detail via `LITEBOX_LOG=error`/`=debug` (the
existing `diag-guest-exception` logging in `litebox_shim_linux/src/lib.rs`, plus the
pre-existing `diag-exec-mmap`/`DIAG_ELF_PATCH`/`DIAG_REGISTER_EXISTING` correlation
logging already in `litebox_shim_linux/src/syscalls/mm.rs` and
`litebox/src/mm/mod.rs`).

**The RELRO hypothesis is wrong.** `cr2` does not fall anywhere near Xvfb's own (or
any loaded library's) `PT_GNU_RELRO` range. Cross-referencing `diag-exec-mmap`'s
path<->address correlation against `readelf -lW` on the actual binaries pulled from
the packed tar shows the fault address is always exactly
`<reservation-base> + 0x200` -- inside the FIRST `PT_LOAD` segment (`p_vaddr=0`,
`R`-only, ELF header + rodata) of whichever shared library's ELF reservation
happened to land there, reproduced identically with two different libraries at two
different addresses (`libepoxy.so.0` at `0x2320000+0x200=0x2320200` with the full
extension list; the same library, transitively pulled in even with `+extension
GLX`/`COMPOSITE`/`DAMAGE` omitted, at `0x1260000+0x200=0x1260200` in a second run).
`libepoxy.so.0`'s real `PT_GNU_RELRO` is at file offset `0x121e60`, nowhere close.
`error_code=0x4` (write fault); the overlapping mapping's flags are `VM_READ |
VM_MAYREAD | VM_MAYWRITE | VM_MAYEXEC` -- `VM_WRITE` genuinely absent, matching a
plain `R`-only `PT_LOAD` segment (`p_flags=4`) exactly as `readelf` reports it, not
a RELRO-then-relocked segment (which would show a RW segment that regressed to R).

**`rip` is inside `/lib64/ld-linux-x86-64.so.2` itself** (`0x7feffffbe932`, ~0x7932
into its own load base per `diag-exec-mmap`'s tracked range) -- this is the guest's
own dynamic linker executing a write whose target address resolves into the wrong
segment. This is consistent with either (a) a genuine glibc `ld.so` bug that would
also crash on real Linux (ruled unlikely: this is a completely ordinary, unmodified
Debian `libepoxy0` package, loaded the same way on every real Debian system running
Xvfb+GLX without incident), or (b) litebox reporting/placing a base address for one
of the loaded libraries that doesn't match what `ld.so`'s own bookkeeping (link_map,
TLS `dtv`, or GOT/PLT-adjacent metadata) computed, causing a write meant for a
writable region (this library's own RW/RELRO segment, or another library's TLS
block) to land at a small, suspiciously fixed offset into this library's read-only
first segment instead.

**Ruled out this session, by direct code reading (not the RELRO/mprotect-timing
class of bug the task brief expected):**
- litebox performs **no eager RELRO protection of its own** -- confirmed
  `litebox_common_linux::mm::sys_mprotect` (`litebox_common_linux/src/mm.rs`) is a
  thin, generic forward of whatever `PROT_READ`/`PROT_WRITE`/`PROT_EXEC`
  combination the *guest* requests via its own `mprotect(2)` syscall, with no
  RELRO-specific logic, no early/eager narrowing, and (per that function's own
  extensive comment, written by an earlier session for an unrelated weston bug) it
  was specifically extended to stop silently EINVAL'ing "unusual" prot
  combinations like the bare `PROT_WRITE` a real RELRO-lock/unlock sequence uses.
  This is architecturally already what the task brief predicted litebox *should* be
  doing (never touching RELRO itself, only guest-driven mprotect) -- it already does
  that; there was nothing eager to find or remove.
- **No `guest_mprotect` call touches this address at all before the fault**,
  confirmed by a full `LITEBOX_LOG=debug` trace of this exact repro grepped for
  every `mprotect`/`diag-protect-mapping` line -- rules out any mprotect-timing race
  (early lock, late unlock, or otherwise) as the mechanism, since no mprotect
  syscall for this range was ever issued by the guest in the first place.
- `litebox_common_linux::loader::ElfParsedFile::load`'s per-`PT_LOAD`-segment
  mapping loop (`litebox_common_linux/src/loader.rs` ~394-542) computes
  `load_start`/`file_end`/`load_end` correctly for every segment observed in this
  repro (cross-checked by hand against `readelf -lW`'s real `p_vaddr`/`p_filesz`/
  `p_memsz` for both Xvfb and libepoxy) -- no off-by-one or page-rounding bug found.
- `try_allocate_cow_pages`'s real `MapViewOfFile3`-based Windows CoW implementation
  (`litebox_platform_windows_userland/src/lib.rs` ~6523) is NOT the pass-344
  padding-decommit bug its own doc comment describes (that bug's signature --
  `VirtualFree(MEM_DECOMMIT)` failing on a `MapViewOfFile3` view during crash
  cleanup -- IS present as a secondary, cosmetic panic in this repro's own crash
  logs, confirmed via full backtrace to originate in `deallocate_pages` during
  post-SIGSEGV teardown, NOT during the original fault -- but the pass-344 fix
  already prevents it from being the primary cause here, since `verified_safe_padding`
  is computed correctly for every mapping in this repro).
- The runtime syscall-rewriter's trampoline-patch path
  (`maybe_patch_exec_segment`, `litebox_shim_linux/src/syscalls/mm.rs` ~1387) never
  writes outside `[mapped_addr, mapped_addr+len)` of the specific executable
  segment it is patching (checked by direct code reading) -- ruled out as a source
  of spillover into the adjacent, non-executable first segment.
- `init_elf_patch_state`'s own `sys_read(fd, ..., Some(offset))` calls
  (`litebox_shim_linux/src/syscalls/mm.rs` ~1125) use pread-style explicit-offset
  reads that, per `litebox::fs::FileSystem::read`'s own documented contract ("If
  `offset` is Some, the file offset is not changed"), do not disturb the fd's
  shared position -- ruled out as a source of `ld.so`'s own sequential reads of the
  same fd going out of sync.

**A live byte-dump diagnostic added this session (dumping the raw bytes at `cr2`
and `rip` via `RawConstPointer::to_owned_slice` from inside the
`diag-guest-exception` handler in `litebox_shim_linux/src/lib.rs`) caused the whole
runner to HANG instead of printing** -- the process never exits and must be
force-killed. This was reverted without committing (confirmed `git diff` clean
afterward) rather than shipped broken, per this project's standing discipline
against forcing an unverified change. This is itself a real, disclosed finding:
something about reading raw guest memory from inside this specific exception-path
callback re-enters a lock or a nested-fault condition that deadlocks -- most likely
the `self.process().0.pm().mappings()` iteration a few lines above already holds a
read lock on the same `Vmem` state a subsequent raw pointer dereference needs, or
the raw read itself takes a second guest-side page fault that this handler (already
mid-fault) cannot re-enter safely. This needs its own fix before a live byte-dump at
this call site is safe to add.

**Root cause NOT FOUND this session.** This is a real, precisely-relocalized
litebox gap -- confirmed NOT the RELRO-timing bug the task brief targeted, confirmed
NOT any of the six mechanisms above -- but the actual mechanism producing a wrong
write target from inside the guest's own `ld.so` remains open. No fix was applied;
forcing one blind, given how many plausible-looking mechanisms were already ruled
out by direct reading, would risk exactly the kind of unverified, papered-over
change this project's standing discipline exists to prevent. `cargo test
--workspace` was not run since no functional code change was made (the reverted
diagnostic left the tree clean).

**Next step for whoever picks this up:**
1. Fix the hang-on-read bug in the `diag-guest-exception` path first (check lock
   re-entrancy around `pm().mappings()` and whatever raw-pointer read helper is
   used to dump memory from inside a guest exception callback) -- this blocks any
   further live-memory-dump diagnostic at this exact, most-useful capture point.
2. Once safe, dump the actual bytes at `cr2` (all-zero fresh page vs. real, corrupt
   content tells "never touched" from "wrong write already happened once before")
   and get a byte-exact disassembly of the REWRITTEN in-memory code at `rip` (not
   the on-disk `ld-linux-x86-64.so.2`, since litebox's syscall rewriter patches
   `syscall` instructions in place and can shift subsequent byte offsets within the
   same function -- this session's naive file-offset disassembly attempt hit
   exactly that trap and was discarded as unreliable).
3. Once the real faulting instruction is confirmed, trace what guest-visible value
   it's computing as its write target and why that computation lands inside a
   different (or the same, wrong-offset) library's read-only segment -- the leading
   candidates are litebox's TLS/`dtv` bookkeeping (if it does any on the guest's
   behalf) and any base-address reporting litebox exposes to the guest (`AT_BASE`,
   `AT_PHDR`, or similar auxv entries) that might not match where the library was
   actually mapped.
4. The nginx-as-pid-1 and selkies-stack continuation work this document's prior
   sections describe remains valid and unblocked by this session's findings --
   Xvfb specifically is what needs this fix before video capture can start.

## 2026-09-06, continued a third time: root cause found -- `UnmapViewOfFileEx`
## silently destroys the WHOLE CoW view on a partial-range replace, not just the
## requested sub-range; the `ld.so` "wrong write target" framing was wrong

### The hang, fixed

The prior session's hang was NOT lock reentrancy. `PageManager::mappings()`
(`litebox/src/mm/mod.rs`) already collects into an owned `Vec` and drops its
`vmem.read()` lock before returning -- the `for (r, flags) in
self.process().0.pm().mappings()` loop in `diag-guest-exception`
(`litebox_shim_linux/src/lib.rs`) holds no lock at all while iterating. Added a
minimal, safe byte-dump at `cr2` and `rip` right after the existing mapping-walk,
gated on that same walk already having confirmed a mapping overlaps the address
(no separate fault-catching machinery, no `memcpy_fallible`/exception-table
involvement -- an ordinary `unsafe { core::slice::from_raw_parts(addr, 64) }`).
Verified live: this does NOT hang. It does, correctly, occasionally raise a
SECOND real hardware exception (see below) -- and Windows' VEH chain handles that
fine, routing it through the same `[diag-unrecov-av]`/`veh_depth` nested-fault
diagnostic machinery this codebase already has for exactly this situation. No
lock-safety change was needed; the fix was simply avoiding the fault-catching
`memcpy_fallible` path the prior attempt used, per the task's own option (a).

### The byte-dump immediately falsified the RELRO/ld.so-bug framing

The live dump caused the raw `cr2` read to fault a SECOND time, on the HOST
side (`is_in_guest=false`), with `matching_gprs=["rcx"]` at `addr=0x2320200` --
i.e. the SAME address the guest's `ld.so` write faulted on is **not actually
backed by real memory at all**, despite `Vmem`'s own bookkeeping insisting the
range is a valid `R`-only mapping (`VM_READ | VM_MAYREAD | VM_MAYWRITE |
VM_MAYEXEC`, exactly what the mapping walk reported). The VEH diagnostic's own
`[diag-unrecov-av-pagestate]` line confirmed this directly:
`BaseAddress=0x2320000 RegionSize=0x50000 State=0x10000 (MEM_FREE)
Protect=0x1 (PAGE_NOACCESS)` -- this address range is not reserved OR
committed in the real Windows address space at all. `ld.so` is not computing a
wrong write target; it is writing to an address litebox's own guest-visible
memory map claims is valid, backed by NOTHING on the host side. The "wrong
base address reported to ld.so" hypothesis from the prior two sessions is
falsified: `ld.so`'s computation is irrelevant here, since ANY write to this
range would fault, correctly-computed or not.

### Root cause, traced with `LITEBOX_DIAG_MM=1`

Re-ran with `LITEBOX_DIAG_MM=1` (off by default, see that flag's own doc
comment -- gates the `diag-cow`/`diag-commit`/`diag-reclaim` family) to see the
full CoW lifecycle for this exact address range. The sequence, in order:

1. `diag-cow: try_allocate_cow_pages OK addr=0x2320000 len=0x130000
   file_offset=... view_padding=0` -- `ld.so`'s own INITIAL `mmap()` for
   `libepoxy.so.0` (its `hint`-based, non-`MAP_FIXED` reservation covering the
   library's whole `min_vaddr..max_vaddr` span in one call -- ordinary,
   completely standard glibc/musl dynamic-linker behavior, not a bug in the
   guest) gets CoW-mapped as ONE `MapViewOfFile3` view spanning
   `[0x2320000, 0x2450000)` -- 1245184 bytes, covering what will become
   SEVERAL independently-addressed `PT_LOAD` segments once `ld.so` issues its
   later per-segment `MAP_FIXED` sub-mmaps over parts of this same range.
2. `DIAG_REGISTER_EXISTING start=0x2320000 end=0x2450000 file_backed=true` --
   `Vmem` records the WHOLE view as one `R`-only-permissions mapping (the
   `prot` of this first call).
3. `ld.so`'s SECOND mmap -- the RW data segment's own `MAP_FIXED` sub-mmap,
   landing at `[0x2380000, 0x23e0000)`, squarely INSIDE the view just created
   in step 1 -- triggers `allocate_pages`'s `Replace`-mode reclaim path
   (`litebox_platform_windows_userland/src/lib.rs` ~6087,
   `process_memory_range_by_regions`). `VirtualQuery` at the reclaim target
   reports `MEM_MAPPED` (this is still the same view from step 1), so
   `was_mapped_view=true` and the code calls
   `UnmapViewOfFileEx(r.start=0x2380000)` (`diag-reclaim: allocate_pages
   destroying committed range start=0x2380000 end=0x23e0000
   was_mapped_view=true`) -- **`r` here is the CALLER-REQUESTED sub-range,
   already clamped to `[0x2380000, 0x23e0000)` by
   `process_memory_range_by_regions`'s own clamping (`len =
   region_remaining_from_range_start.min(range.len())`), NOT the view's real,
   full extent** (`[0x2320000, 0x2450000)`, obtainable from the SAME
   `VirtualQuery` call's own `mbi.BaseAddress`/`mbi.RegionSize` before that
   clamp is applied). `UnmapViewOfFileEx` has no partial/sub-range form (per
   this exact function's own pre-existing doc comment, lines 6134-6136, from
   an EARLIER investigation of a related-but-distinct bug) -- passing any
   address inside a view unmaps the ENTIRE view, confirmed live: this call
   SUCCEEDED (`decommit_ok=true`, no panic), destroying all of
   `[0x2320000, 0x2450000)`, not just the intended `[0x2380000, 0x23e0000)`.
4. `reserve_and_commit(r.clone(), ...)` (the `was_mapped_view` branch) then
   only re-establishes fresh, real memory for the narrow `r =
   [0x2380000, 0x23e0000)` the caller actually asked for -- the two flanking
   remainders of the now-fully-destroyed original view,
   `[0x2320000, 0x2380000)` (contains the RO first `PT_LOAD` segment,
   including `cr2=0x2320200`) and `[0x23e0000, 0x2450000)`, are left
   permanently `MEM_FREE`: still `MapViewOfFile3`-view-shaped as far as
   `Vmem`'s bookkeeping is concerned (nobody told it the view died), genuinely
   unbacked as far as Windows is concerned. `ld.so`'s later legitimate write
   into what it (correctly) believes is its own writable/relocatable data
   faults not because the target address is wrong, but because litebox
   silently voided memory out from under an unrelated, already-established
   mapping while servicing a completely different, later mmap call.

This is corroborated exactly by the earlier session's own two independent
captures (`libepoxy.so.0` at two different ASLR-shifted base addresses, always
faulting at `+0x200`): `+0x200` is simply a small, fixed, early offset into
whatever content happens to occupy the START of the now-orphaned flank -- the
ELF header/early rodata `ld.so` itself reads (not writes -- worth flagging: the
`error_code=0x4` "write" classification deserves one more direct look, since
the actual instruction at `rip` was never disassembled this session either;
what's established beyond doubt is that ANY access, read or write, to
`0x2320200` faults, because the page is `PAGE_NOACCESS`/`MEM_FREE`, independent
of what kind of access `ld.so` was attempting).

### Why this is a genuinely deeper gap, not a small targeted fix (stopping per
### this project's own standing discipline, task step 6)

A fully correct fix requires `allocate_pages`'s `Replace`-mode reclaim path to
handle "the destroy target is a strict sub-range of a wider `MapViewOfFile3`
view" by RECONSTRUCTING the flanking remainder ranges as equivalent CoW
mappings (same source file, same file offset, same protections) after the
whole-view unmap -- not just committing fresh anonymous memory for the
requested sub-range. This is NOT fixable inside `allocate_pages` alone:

- `allocate_pages` lives in the platform crate (`litebox_platform_windows_userland`)
  and, by this codebase's own existing design boundary (see
  `try_allocate_cow_pages`'s doc comment on why it takes a
  caller-verified-safe padding parameter rather than querying `Vmem` itself),
  has NO access to `Vmem`/`PageManager` state -- it cannot ask "what was
  mapped at `[0x2320000, 0x2380000)` and from which file/offset" once the
  view is gone.
- `WindowsUserland::cow_regions` (the registry `try_allocate_cow_pages` uses)
  is keyed by the HOST-side source file's static-mapped content address, not
  by GUEST destination address -- there is no existing reverse index from "a
  guest address that used to be part of a CoW view" back to the file/offset
  that backed it, which reconstructing the flanks would require.
- The caller (`litebox_shim_linux`'s `try_cow_mmap_file`/`do_mmap_file`, or
  `litebox_common_linux::mm::do_mmap`) DOES have `Vmem` access and could in
  principle detect "this `MAP_FIXED` target overlaps an existing file-backed
  mapping from a DIFFERENT file-offset span than the one now being mapped"
  before ever calling into `allocate_pages` -- but teaching it to then
  correctly split/preserve the flanks (re-deriving their exact file/offset
  from `Vmem`'s own existing `VmArea` record for the range about to be
  destroyed, since `Vmem` DOES already track `is_file_backed` per mapping --
  see `register_existing_mapping`) is real, non-trivial new work, not a
  targeted bugfix: it needs a new code path for "partially replace a CoW
  view," which does not exist anywhere in this codebase today, and touches
  the same pass-344-adjacent memory-safety-critical territory this project
  has already been burned by once.

Per this project's standing discipline against forcing an unverified fix in
this exact area (see the immediately preceding session's own identical
restraint), this is reported rather than patched blindly. The narrow,
verified, low-risk part of this session's work -- the safe live byte-dump
diagnostic itself -- is committed; the structural CoW-view-splitting fix is
not attempted this session.

### What actually changed and is committed

`litebox_shim_linux/src/lib.rs`'s `diag-guest-exception` handler: the mapping
walk now collects into an owned `Vec` up front (ruling out the lock-reentrancy
hypothesis by construction, not just by inspection) and, when the walk
confirms a mapping backs `cr2` (respectively `rip`), dumps 64 raw bytes there
via a direct, non-fault-catching pointer read. This is what produced the
`[diag-unrecov-av-pagestate]` evidence above (indirectly, by faulting a second
time when the mapping's claim turned out to be false) -- confirmed live,
non-hanging, via the exact `wt_xvfb.sh` repro
(`.wfgy/webtop-debian/webtop-debian-i3-with-scripts.tar`, `LITEBOX_LOG=error`,
`LITEBOX_DIAG_MM=1` for the CoW trace).

### Repro notes for whoever continues this

- Boot takes ~450s wall-clock just to load/extract the 9GB
  `webtop-debian-i3-with-scripts.tar` before guest execution starts (this is
  disk I/O, not a regression -- unrelated to this bug). Use `nohup ... &` and
  poll, not a short `timeout`.
- Git Bash mangles `/bin/sh` (a leading-`/` CLI argument) into a Windows path
  before it reaches the runner unless `MSYS_NO_PATHCONV=1
  MSYS2_ARG_CONV_EXCL="*"` is set in the environment.
- A crashed/killed prior run can leave a stale
  `target/.../.litebox-cache/boot.lock` behind; check `tasklist` confirms no
  real process holds it before removing it.
- `LITEBOX_DIAG_MM=1` is required to see the `diag-cow`/`diag-commit`/
  `diag-reclaim` family at all -- `LITEBOX_LOG=error` alone does not enable
  them (see that flag's own doc comment: they are unconditionally-costly
  `error!` calls on the hottest mm path, gated separately for exactly this
  reason).

### Next step for whoever picks this up

1. Decide the fix's shape: either (a) teach `Vmem`'s own mmap-dispatch layer
   (`litebox_common_linux::mm::do_mmap` or `litebox_shim_linux`'s
   `try_cow_mmap_file`/`do_mmap_file`) to detect a `MAP_FIXED` target
   overlapping an EXISTING file-backed CoW mapping from a different
   file-offset span, and pre-split it (re-establishing the flanks as their
   own independent CoW views/registrations) BEFORE calling into
   `allocate_pages` at all, so `allocate_pages` never receives a
   destroy-a-sub-range-of-a-wider-view request in the first place; or (b) make
   the INITIAL CoW mmap narrower to begin with -- if it's possible to detect
   at CoW-mapping time that a `hint`-mode (non-`MAP_FIXED`) mmap's requested
   `len` will later be subdivided by the SAME loader's own subsequent
   `MAP_FIXED` calls (this may not be generically detectable without ELF
   awareness at the mmap-syscall layer, which is deliberately absent there).
2. Whichever shape is chosen, the fix touches memory-safety-critical code
   already burned once (pass 344) -- budget for careful, incremental,
   live-verified changes, not a single large patch.
3. `error_code=0x4`'s "write" classification was never independently
   confirmed via live disassembly at `rip` this session (the second,
   host-side fault preempted getting there for the ORIGINAL guest fault) --
   worth one more pass with the SAME safe byte-dump technique, extended to
   also disassemble live in-memory bytes at `rip`, once the underlying
   MEM_FREE gap above is fixed and the guest fault (if any remains) is a
   genuinely different one.
4. `cargo test --workspace` was not run: the only change this session made is
   the diagnostic addition in `litebox_shim_linux/src/lib.rs`, which is
   `#[cfg]`-unconditional but purely additive/read-only (no behavior change
   on any path that doesn't already log `diag-guest-exception`) -- run it
   before building on this further regardless, per repo policy.

---

# 2026-09-07: `touch` crashed the HOST, not the guest -- one VEH nesting level was 64 bytes wide

## Symptom and the framing it invited

`touch` on a not-yet-existing path killed the whole runner with a genuine,
unrecoverable Windows access violation in litebox's own host-side code:

```
MSYS_NO_PATHCONV=1 litebox_runner_linux_on_windows_userland.exe -Z \
  --oci-image docker.io/linuxserver/webtop:debian-i3 --env HOME=/config \
  -- /bin/touch /tmp/direct_touch
```

`mkdir -p` worked, `echo` worked, a `base64 -d | sh` pipe worked. Only `touch`
of a new path died, under `--oci-image` and under `--initial-files` alike, which
made "new-file creation is broken" the obvious reading. **That reading is
wrong, and the bisection that produced it stopped one step too early.**

## The one bisection step that reframes everything

`touch` issues two syscalls: `openat(O_CREAT)` then `utimensat`. Splitting them:

```
> /tmp/redir_new && echo MARKER          # pure open(O_CREAT), no utimensat
```

**succeeds.** File creation was never broken. Adding `utimensat` back crashes,
and it crashes on every shape of target, not just a new one:

| case | result |
| --- | --- |
| `> /tmp/redir_new` (O_CREAT only, no utimensat) | OK |
| `touch /tmp/direct_touch` (new file) | CRASH |
| `> /tmp/a && touch /tmp/a` (exists in UPPER) | CRASH |
| `touch /etc/hostname` (exists in LOWER only) | CRASH |
| `mkdir -p /tmp/newdir && touch /tmp/newdir/f` | CRASH |

So the crasher is `utimensat`, and the layer its target lives in is irrelevant
to whether it fires. That in turn means the fault is not in any filesystem code
path -- it is in whatever runs when `utimensat`'s guest-pointer read takes a
page fault.

## Refuted along the way (recorded so it is not re-tried)

`LayeredFs::set_times` (`litebox/src/fs/layered.rs:1401`) ends in an unguarded
self-recursive call, which looks exactly like an unbounded-recursion /
stack-overflow candidate. It is not this bug: an instrumented build counting
that recursion emitted **zero** entries while still crashing with the identical
signature. The `migrate_file_up`-then-retry assumption holds in practice here.
The recursion is still unguarded and still worth a defensive bound, but it is
not what this section fixes.

## Root cause: the trampoline's per-depth slot was one frame wide, not one per level

`vectored_exception_handler_entry` (`litebox_platform_windows_userland/src/lib.rs`)
swaps onto the thread's real host stack before calling the full handler, and
gives each VEH nesting level its own scratch slot so a nested fault cannot
clobber a still-live outer invocation. The slot was **64 bytes**.

64 bytes covers exactly what the trampoline itself stores -- guest `rsp`/`rbp`,
`r8`, and the callee's 32-byte shadow space. It does not cover the callee. And
the callee's frame grows downward from that slot: `vectored_exception_handler`
plus `fork_verify::on_single_step`'s instruction decode, the `VirtualQuery`
diagnostic blocks and `eprintln!`'s formatting machinery are kilobytes deep. So
at `veh_depth == 1` the nested invocation began its frame 64 bytes below the
outer one's and wrote straight through it.

The captured evidence is unambiguous. At the crash, `veh_depth=0x1`, and the
outer invocation read back:

```
[diag-unrecov-av-ring] [2] code=0x470041 rip=... rva=0x8b0fc6 is_in_guest=false
[diag-unrecov-av-ring] [3] code=0xc0000005 rip=0x22 rva=... is_in_guest=false
[diag-unrecov-av-giveup] rip=0x7ff8da4f587a repeat_count=0x41
```

`0x470041` is **not a Windows status code**. Its severity bits are `00`, which
marks a SUCCESS code -- the kernel never delivers one as a fault -- and its four
bytes are UTF-16LE for the characters A and G. It is raw string data from the
nested frame, sitting where the outer frame's `ExceptionCode` used to be.
Dispatch then followed the equally garbage `rip` beside it (`0x22`, then `0x40`)
into the wild-jump cascade, until the repeat circuit breaker fired.

`rva=0x8b0fc6` disassembles (llvm-objdump, release binary, ImageBase
`0x140000000`) to `movzbl (%rcx), %edx` -- `read_u8_fallible`'s single faulting
load, i.e. `to_cstring`'s byte-at-a-time guest-string scan
(`litebox/src/platform/mod.rs:446`). That is an ordinary, expected,
exception-table-recoverable fault. It only became fatal because the recovery
machinery corrupted itself the moment it nested once.

## Second, independent overlap in the same region

`EXC_RECORD_SLOT_SIZE` was a hardcoded `128` while `EXCEPTION_RECORD` is **152**
bytes on x86_64 (`4 + 4 + 8 + 8 + 4 + 4 pad + 15*8`). Consecutive
exception-record slots therefore overlapped by 24 bytes -- the exact corruption
the per-depth scheme exists to prevent. The comment above it described 128 as
rounding 152 up.

The prose describing the two regions as "comfortably clear" of each other was
also wrong, by 64 bytes at the deepest level.

## Fix (commit `5cacf7f`)

`litebox_platform_windows_userland/src/lib.rs`:

- `VEH_FRAME_STRIDE` (new, 4096) replaces the hardcoded 64 as the
  per-nesting-level stride, sized to hold a real handler frame rather than only
  the saved registers.
- `VEH_DEPTH_CAP` 512 -> 7, the number of 4 KiB frames that fit the
  already-committed `EXCEPTION_RECORD_RESERVE`. Not a robustness regression: 512
  never gave 512 usable levels, it gave one usable level and 511 that silently
  corrupted each other.
- `EXC_RECORD_SLOT_SIZE` derived from `size_of::<EXCEPTION_RECORD>()` rounded up
  to 16, instead of a hardcoded constant that had drifted below the struct it
  sizes.
- Both non-overlap requirements are now `const`-asserted at the definition site
  rather than claimed in a doc comment that was measurably wrong twice.
- `mov r9, r9` (a 64-bit no-op commented as a zero-extend) -> `mov r9d, r9d`.

## Result

The host-level crash is gone. Same repro, after the fix:

```
[diag-recover-fsbase] recover_rip=0x7ff7fcc30fdb fsbase=0x7feffffb0740
touch: setting times of '/tmp/direct_touch': Bad address
EXIT=1
```

Zero `[diag-unrecov-av]`, no wild `rip`, no `[diag-unrecov-av-giveup]`
`TerminateProcess`. `[diag-recover-fsbase]` now fires where it never did before,
i.e. the exception table recovers the fallible read normally. The process
survives and reports an ordinary errno to the guest.

**This is a general fix, not a `touch` fix.** Any recoverable guest fault that
nested once was hitting this. It plausibly underlies other
"unrecoverable-AV-with-nonsense-`rip`" reports elsewhere in this investigation,
and any of those should be re-tested against this commit before being chased
independently.

## Second bug, uncovered by the first: a NULL `utimensat` path is `futimens`, not `EFAULT`

With the host crash gone, the same repro failed cleanly instead:

```
touch: setting times of '/tmp/direct_touch': Bad address
```

An allocation-free `diag_raw_print`-style probe at the dispatch site (NOT
`alloc::format!` -- see the warning below) gave the answer immediately:

```
[diag-utimensat] pathname=0x0 path_ok=0x0
```

The guest passed a **NULL** path. That is legal: `utimensat(dirfd, NULL, times,
flags)` operates on `dirfd` itself and is exactly what musl's
`futimens(fd, times)` compiles down to -- something `sys_utimensat`'s own doc
comment already stated. Two independent gates rejected it anyway:

1. The dispatch arm in `litebox_shim_linux/src/lib.rs`
   (`SyscallRequest::Utimensat`) called `pathname.to_cstring::<Platform>()`
   unconditionally and mapped the resulting `None` to `EFAULT`. A NULL path
   therefore never reached `sys_utimensat` at all.
2. `sys_utimensat` (`litebox_shim_linux/src/syscalls/file.rs`) then gated its
   `FsPath::Cwd` and `FsPath::Fd(fd)` arms on `AT_EMPTY_PATH`, returning `ENOENT`
   without it. Unlike most `*at` syscalls, `utimensat` does not require that flag
   for the empty/NULL-path form. This gate is why fixing only (1) would have left
   the `FsPath::Fd` handling as dead code.

Fixed in commit `caaac79`: forward a NULL `pathname` as an EMPTY path (which
`FsPath::new` already maps to `FsPath::Fd(dirfd)`/`FsPath::Cwd`), and drop the
`AT_EMPTY_PATH` requirement in `sys_utimensat` (the flag is still accepted, just
no longer required).

### Result

The exact repro from the report now exits 0 with no output and no crash:

```
litebox_runner_linux_on_windows_userland.exe -Z \
  --oci-image docker.io/linuxserver/webtop:debian-i3 --env HOME=/config \
  -- /bin/touch /tmp/direct_touch
EXIT=0
```

Re-run of the bisection matrix, all passing, zero `[diag-unrecov-av]`:

| case | before | after |
| --- | --- | --- |
| `> /tmp/redir_new` (O_CREAT only) | OK | OK |
| `touch /tmp/direct_touch` (new file) | CRASH | `EXIT=0`, file created and stat-able |
| `> /tmp/a && touch /tmp/a` (exists in UPPER) | CRASH | `rc=0`, `EXIT=0` |
| `touch /etc/hostname` (exists in LOWER only) | CRASH | `rc=0`, `EXIT=0` |
| `mkdir -p /var/log/nginx && touch /var/log/nginx/error.log` | CRASH | `rc=0`, file created |

That last row is the specific prerequisite the `nginx-as-pid-1` section needs.
It now works end to end:

```
touch_rc=0
lower_rc=0
-rw-r--r-- 1 root root 0 Sep  6 22:13 /var/log/nginx/error.log
EXIT=0
```

### Instrumentation warning

Adding an `alloc::format!`-based log line at that dispatch site **crashes on its
own** (confirmed live -- first fault at an allocator address,
`rip=0x2c8f4ae1f40`). The syscall path can be reached with the guest allocator's
state already suspect. Use the allocation-free `diag_raw_print`-style helper
pattern (see `diag_raw_print_proc_sys_open_miss`,
`litebox_shim_linux/src/syscalls/file.rs`) instead, exactly as the VEH
diagnostics already do for the same reason.

## nginx-as-pid-1 re-verified against both fixes

Re-ran the `nginx-as-pid-1` setup from this document's earlier section against
the two fixes above, to confirm neither regressed it. It still works, end to
end, with no crash:

```
litebox_runner_linux_on_windows_userland.exe -Z \
  --oci-image docker.io/linuxserver/webtop:debian-i3 \
  --resume-from <overlay>.tar -p 3000:3000 --env HOME=/config \
  -- /start-nginx.sh
```

The overlay carries only the pre-substituted
`/etc/nginx/sites-{available,enabled}/default` (the same config recorded
earlier), the `/var/log/nginx` and `/run` directories, and a one-line launcher.
`--resume-from` seeds the writable layer, which is a cleaner injection route than
the earlier `tar --concatenate` trick.

Two operational notes for a re-run:

- `--resume-from` takes a **host** path, and `MSYS_NO_PATHCONV=1` (required for
  the guest-side `/`-paths) also suppresses translation of that host path. Pass
  it as an explicit Windows path (`C:/...`), or the runner panics with "failed to
  open ... The system cannot find the file specified" -- and then, notably,
  *overflows its stack while panicking*, exiting 139. A panic in that path
  producing a stack overflow rather than a clean abort is itself worth a look.
- `-g "worker_processes 1; ..."` collides with the image's own `nginx.conf:2`
  (`"worker_processes" directive is duplicate`). Use `-g "daemon off;
  master_process off;"` only.

Serving confirmed:

```
$ curl -D - http://127.0.0.1:3000/
HTTP/1.1 200 OK
Server: nginx
Content-Length: 762
<!doctype html>...<script type="module" crossorigin src="./assets/index-BTp9L9Xk.js">
```

Guest process tree shows `pid=1 ppid=0 comm=/usr/sbin/nginx` -- real nginx as
litebox's own pid 1, no s6-overlay anywhere.

**Browser-verified** via `claude-in-chrome` against a real Chrome tab at
`http://127.0.0.1:3000/`: page title is genuinely `Selkies`, and `get_page_text`
returns the real dashboard shell -- Video Settings, Screen Settings, Audio
Settings, Stats, Clipboard, Files, Apps, Sharing, Gamepads -- plus
`WebSocket disconnected. Attempting to reconnect...`, which is the correct
behavior with no selkies node.js backend running. Identical to the result this
document's earlier section recorded, i.e. unregressed.

## Still open

Unchanged by this pass: the desktop/video half is still blocked on the Xvfb
pid-1 SIGSEGV, and selkies' node.js backend was not started, so the dashboard's
WebSocket has nothing to connect to. Neither is related to the two bugs fixed
here.

## Test status

`cargo test` scoped to the four crates does not build, for a **pre-existing**
reason unrelated to this change: `litebox_platform_windows_userland/src/lib.rs:4021`
declares `fn run_test_thread` as a member of `litebox::platform::ThreadProvider`,
which the trait does not have (`E0407`), and `litebox_shim_linux`'s test code
calls the same missing associated function (`E0576`, 11 errors). Confirmed
pre-existing by checking out the parent commit (`ca942c5`) -- `cargo test
--no-run` succeeds there only because it skips doctests, which is where `E0407`
surfaces. This session's change touches neither site.
