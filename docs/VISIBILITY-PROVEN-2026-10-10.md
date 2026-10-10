# Chromium is VISIBLE on webtop `:1` — proven 2026-10-10, 17:06

**Status: the visibility half of the goal is DONE.** An unmodified Debian Chromium 154 running
inside litebox owns a real, painting 1024x768 window on the webtop X display `:1`, which selkies
streams on `:8082`. `--no-sandbox` is **not** in the command line.

## The command that did it

Host pump (`/tmp/lbx/x11pump.py`, agent `a7b0870e95c08ec55`) runs:

```
litebox_runner_linux_userland --unstable
  --env LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu:/lib64:/lib:/opt/lib --env HOME=/tmp
  --env DISPLAY=:1 --env XCLIENT_SECS=45
  --program-from-tar --initial-files /tmp/lbx/rootfs-x11.tar
  /usr/bin/python3 -u /tmp/x11proxy.py --
  /usr/lib/chromium/chromium --ozone-platform=x11 --disable-gpu --no-first-run
  --disable-dev-shm-usage --disable-breakpad --disable-crash-reporter
  --no-default-browser-check --user-data-dir=/tmp/udd-x11
  --window-size=1024,768 --enable-logging=stderr --single-process
  file:///page.html
```

Guest program is `python3 /tmp/x11proxy.py`; it binds `/tmp/.X11-unix/X1` **inside litebox**,
accepts, and execs chromium unmodified. Zero Rust changes: no shim, platform or runner edits.

## Evidence

`DISPLAY=:1 xwininfo -root -tree`:

```
0x2a00002 "litebox chromium - Chromium": ("chromium-browser (/tmp/udd-x11)" "Chromium-browser")
          1024x768+0+0  +10+37
```

1024x768 is exactly the `--window-size` passed; `+10+37` means **xfwm4 has framed it** — it is a
managed window on the real desktop, not an override-redirect surface.

`DISPLAY=:1 xwd -id 0x2a00002` → 3,148,800 bytes of pixels, **2400 distinct colours**. A 16x12
downsample of the client area (sampled at +32,+32 in each 64 px cell):

```
fde3d3 fde3d3 ... ffffff ffffff      <- page background / top strip
ffffff x16                             <- content area
00d4ff x4  ff6600 x8  00d4ff x4
00d4ff x4  ff6600 x8  00d4ff x4
2311e8 x4  51a600 x8  2311e8 x4
2311e8 x4  51a600 x8  2311e8 x4        (repeats to the bottom)
```

Four saturated colour blocks (cyan `#00D4FF`, orange `#FF6600`, blue `#2311E8`, green `#51A600`)
with crisp quadrant edges — **composited, coloured, correctly laid out pixels**, not a blank or
garbage window. (`/tmp/lbx/chr-window-rgb.png`, `/tmp/lbx/win.xwd`.)

## Why this route and not the others

Verified dead ends, restated so nobody re-walks them:

- **No network in the guest.** No `/dev/net/tun`, so `send_ip_packet` is `unimplemented!`
  (`litebox_platform_linux_userland/src/lib.rs:2549-2573`) → X11-over-TCP is impossible.
- **Spill is inert on this container.** `LITEBOX_SHARED_WRITE_PREFIXES` is ANDed with
  `platform.spill_available()` (`file_spill.rs:324-326`), whose default is `false`
  (`litebox/src/platform/mod.rs:1961-1963`) and which only
  `litebox_platform_windows_userland/src/lib.rs:15421` overrides. So there is no host-visible
  *file* channel either — guest stdio is the only one.
- **A guest AF_UNIX socket never becomes a host fd** (`sys_connect` → `lookup` in the in-memory
  `global.unix_addr_table`, `unix.rs:1983`). Hence the tunnel.

## What the tunnel had to do

From `[pump]` lines in `/tmp/lbx/log-gu2.txt`:

- `cid 1 setup request 12 bytes; byte-order <` → `setup reply 2596 bytes status 1` — the X
  connection handshake completes through the pipe.
- Chromium then queries BIG-REQUESTS, DRI3, GLX, RANDR, RENDER, MIT-SCREEN-SAVER, SHAPE, MIT-SHM,
  SYNC, XFIXES, XInputExtension, XKEYBOARD, XTEST.
- **`DRI3` and `MIT-SHM` are stripped (`present=0`)** — mandatory, because a guest can never map a
  host shm fd or a dma-buf. Chromium falls back to plain `PutImage`.

## Still open

1. **Multi-process.** This run uses `--single-process`. Without it, the renderers take
   `TerminateSelfOnDisconnect` → `_exit(0)` on a 0-byte `recvmsg` of the Mojo bootstrap channel
   (`docs/CHROMIUM-NO-SANDBOX-2026-10-10.md` §8). The single-process mode is why this works.
2. **The captured window is the agent's colour test page, not `/tmp/lbx/page.html`** (`#ffd400`
   with `<h1>hello</h1>` — `#ffd400` appears 0 times in the capture). Re-run with `page.html` to
   confirm arbitrary content renders, and confirm `--dump-dom` and the window agree.
3. **Live refresh.** Prove the window updates (a `<meta refresh>` or a JS animation), which is what
   "visible" really means for a user watching selkies.
