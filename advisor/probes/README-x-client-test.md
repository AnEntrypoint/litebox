# Two-runner X client test (PREPARED, NOT YET RUN TO COMPLETION)

Status: **endpoint 2 (Debian/XFCE via the litebox display) NOT achieved.** This
documents a prepared experiment, not a result.

## Why two runners

Every fork-without-exec child dies in glibc's safe-linking `REVEAL_PTR`
(`__libc_malloc+0x76`, `xor (%rax),%rsi`) -- see ADVISORY 3N. Confirmed by the
guest itself printing `malloc(): unaligned tcache chunk detected` and
`malloc(): unaligned fastbin chunk detected`.

Measured consequences:
- **pid 1 never dies.** The runner execs it directly; it is never a forked child.
- **fork+exec is safe** (`mkdir`, `chmod`, `/bin/sh`, `xkbcomp` all exit 0).
- **fork-without-exec dies**, including bash's own `&` for any backgrounded job.

So Xorg must be pid 1 in one runner, and the client must be pid 1 in another.
There is no single-runner arrangement that gets a client onto the server without
a fork-without-exec.

## What is already proven (unmodified code, no fix applied)

- Xorg as pid 1: zero crashes, stable for minutes across multiple boots.
- Full display stack: wgpu adapter/device/queue, `present_mode=Mailbox`, surface
  configured, DRM framebuffer at exactly 1920x1080x4 = 8,294,400 bytes.
- `LITEBOX_DUMP_FRAMES` writes real 8,294,454-byte BMPs.
- TCP X11 reachable at the host boundary: `--publish 6000:6000` yields a
  netstat-confirmed `127.0.0.1:6000 LISTENING`.
- Frames are `non_black_pixels=0` -- correct for a server with no client.
  Baseline artifact: `baseline_xorg_pid1_black.bmp`.

## Procedure

Server (holds the boot lock):

    LITEBOX_LOG=error LITEBOX_DUMP_FRAMES=1 \
      litebox_runner --unstable --oci-image linuxserver/webtop:debian-xfce \
      --gui-hidden --publish 6000:6000 --resume-from x3.tar \
      -- /usr/bin/bash /srv.sh

where `/srv.sh` ends with `exec /usr/bin/Xorg :0 -logfile /tmp/x.log -noreset \
-novtswitch -sharevts -listen tcp -ac`. The `exec` is essential: the shell
BECOMES Xorg, so Xorg is pid 1 and is never a forked child.

Client (needs `build_min_x_client.sh` first):

    litebox_runner --unstable --initial-files clmin.tar \
      --env DISPLAY=10.0.2.2:0 -- /usr/bin/xsetroot -solid navy

## RESULT: THE TWO-RUNNER APPROACH IS STRUCTURALLY IMPOSSIBLE

Ran to completion 2026-09-06. The client boots, loads all libraries, and reaches
the X connection attempt -- then fails:

    /usr/bin/xsetroot:  unable to open display '10.0.0.1:0'

This is NOT a configuration error and no address works. Two runners cannot reach
each other, by construction, per `litebox_platform_windows_userland/src/net.rs`
`send_ip_packet` (~line 999):

- `127.0.0.1` is intercepted and looped straight back into the guest's OWN
  smoltcp receive queue. It never leaves the guest. The comment is explicit:
  "nothing is ever listening on a real Windows 127.0.0.1 socket on the guest's
  behalf -- the guest's own listening socket lives entirely inside this same
  process's smoltcp stack."
- `10.0.0.1` (the gateway, `GATEWAY_IP_ADDR`) is not loopback, so it goes to the
  NAT gateway thread -- which "only knows how to proxy to REAL external
  destinations via REAL Windows sockets". The host's own `127.0.0.1:6000` is not
  a real external destination.

So `--publish` is strictly INBOUND (host -> guest) and outbound NAT is strictly
OUTBOUND-to-external. There is no guest -> host-loopback path. The server's
`127.0.0.1:6000` listener is reachable from a host browser (that is how endpoint
1 works) but NOT from another guest.

Closing this route would need a real change: teach the NAT gateway to proxy
guest-initiated connections to host loopback, or give the two runners a shared
transport. Neither is a configuration tweak.

## Superseded blocker (resolved, kept for history)

The boot lock is host-wide (correctly, as of 828f726c) and refuses the second
runner. This test needs a narrow opt-in (e.g. `LITEBOX_ALLOW_CONCURRENT_BOOT=1`)
to proceed. Deleting the lock file is NOT an acceptable workaround: concurrent
boots caused two real host memory crises and produce symptoms indistinguishable
from a hang.

Cost at peak: one full server load (~7GB, unavoidable) plus a ~20MB client.

## Pass criterion

A pass is **not** "more pixels". Compare against `baseline_xorg_pid1_black.bmp`.
The server frame must go from `non_black_pixels=0` to a solid navy fill.

Even a pass would **not** be endpoint 2 -- it is one client painting a root
window, not a rendered XFCE desktop. It would show the remaining gap is "run more
clients" rather than anything architectural.
