# Webtop INSIDE litebox — target re-pointed 2026-10-10 18:15

## The target, verbatim from the user

> "we're not trying to run chrome here, we're trying to run chrome in webtop inside litebox"

and, confirmed by explicit choice:

- **Guest workload = WHOLE WEBTOP**, not chromium-only: the litebox guest boots the desktop stack
  (X + XFCE) and **chromium is launched inside it**.
- **Process model = KEEP PUSHING MULTI-PROCESS.** `--single-process` is not accepted as the
  delivered configuration even though it is the only thing that currently works. Chromium must run
  its zygote + layer-1 namespace sandbox + layer-2 seccomp-bpf. `--no-sandbox` is still never passed.

So the previously-proven result (`docs/VISIBILITY-PROVEN-2026-10-10.md`: unmodified chromium
painting a 1024x768 window on the *host's* `:1` through a stdio X11 tunnel, `--single-process`) is
now a **milestone, not the deliverable**. It proved chromium can run under the shim at all.

## Target architecture

```
HOST (this webtop container)
 └─ litebox_runner_linux_userland
      └─ guest = webtop
           ├─ Xvfb  :0            (or Xorg on litebox's emulated DRM)
           ├─ xfce4-session / xfwm4 / xfdesktop
           └─ chromium  DISPLAY=:0   multi-process, NO --no-sandbox
                 └─ zygote -> renderer (layer-1 + layer-2 sandbox)
```

**This is strictly easier than the tunnel for the display path.** Chromium and Xvfb are both guest
processes, so X11 travels over a *guest-internal* AF_UNIX socket at `/tmp/.X11-unix/X0` — no host
fd is ever needed, and **MIT-SHM / DRI3 do not have to be stripped** (a guest memfd maps fine inside
the guest). The stdio X11 tunnel, `x11pump.py` and the DRI3/MIT-SHM stripping all become
unnecessary.

**Visibility to the human** is the one piece still to solve, because guest files are not host-visible
(`docs/HARNESS-LESSONS.md`). Options, ranked:

1. **Guest screenshots to stdout** — a guest `python3`/`xwd` dumps frames as PNG bytes on stdout,
   which IS a host pipe (`litebox/src/fs/devices.rs:642-703`). Zero new litebox code.
2. **`--gui`** (`runner/lib.rs:102-108`) over litebox's emulated DRM — real for DRM guests, but
   chromium has `ozone_platform_x11`/`_wayland` and **no `_drm`**, so the guest would need Xorg on
   DRM rather than chromium directly.
3. **selkies inside the guest** — dead unless the guest gets a network (see blockers).

## Blockers known before starting

- **No guest network.** No `/dev/net/tun`, so `send_ip_packet` is `unimplemented!`
  (`litebox_platform_linux_userland/src/lib.rs:2549-2573`). Kills in-guest selkies as a transport.
- **The guest FS is in-memory, loaded from the tar** (`--initial-files`, `runner/lib.rs:60-62`).
  `/usr` here is 3483 MB; the whole thing will not fit. The rootfs must be pruned to what the
  desktop actually needs.
- **`rootfs-chr.tar` is missing `/etc/{passwd,group,hosts,nsswitch.conf,resolv.conf,localtime}`**
  (AGENTS.md "Observations"), and chromium's tid 17 retries `nsswitch.conf` + `hosts` every ~1.4 s.
  A webtop rootfs should ship them — a desktop stack is far less tolerant of their absence.
- **Multi-process chromium still dies** (`docs/CHROMIUM-NO-SANDBOX-2026-10-10.md` §8/§9). This is
  orthogonal to getting the desktop up, but both must land.
- **There is no Linux tick and no cross-process wake** (`runner/lib.rs:637`, `epoll.rs:423-440`);
  a desktop is far more fork- and IPC-heavy than chromium alone, so expect to hit these harder.

## Plan

1. **Build a pruned webtop rootfs** (`/tmp/lbx/rootfs-webtop.tar`): ldd-closure the desktop binaries,
   ship an `/etc` skeleton, fonts, `/tmp`, `/run`, `$HOME`. Target well under ~1.5 GB.
2. **Boot it incrementally**, one process at a time, each under `timeout -s KILL`, one runner:
   `Xvfb :0` alone -> `+ xfwm4` -> `+ xfce4-session` -> `+ chromium DISPLAY=:0`.
   Do not skip a step; a desktop that half-boots tells you nothing.
3. **Fix litebox, not webtop**, at every failure — that is the standing rule.
4. **Prove visibility** by (1) above, and prove liveness with two captures 30 s apart that differ.
5. **Then** drive multi-process chromium to completion inside that desktop.

## Rules that still bind

**ONE runner at a time** (4 CPUs / 7.8 GB, frozen three times), everything `nice -n 19`,
`timeout -s KILL`, check `uptime`/`free -m` before trusting any timing. Never modify chromium or
webtop — every fix lands in litebox. Build only `-p litebox_runner_linux_userland -j 2`, output to a
file, never through `tail`. Commit on `main` as `lanmower` with the
`Co-Authored-By: Claude Code <noreply@anthropic.com>` trailer.
