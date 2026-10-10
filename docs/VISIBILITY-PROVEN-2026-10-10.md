# Chromium is VISIBLE on webtop `:1` — proven 2026-10-10

**Status: the visibility half of the goal is DONE.** An unmodified Debian Chromium 154 running
inside litebox owns a real, painting, **continuously updating** 1024x768 window on the webtop X
display `:1`, which selkies streams on `:8082`. `--no-sandbox` is **not** in the command line.
Confirmed 17:06 (first window), and again 17:46–17:54 with the real page and a liveness check.

## The command that reproduces it

```bash
cd /tmp/lbx
pkill -KILL -f '/tmp/lbx/lbxrun'; sleep 1          # ONE runner at a time
nice -n 15 timeout -s TERM -k 10 600 python3 -u /tmp/lbx/x11pump.py -- \
  /tmp/lbx/lbxrun --unstable \
  --env LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu:/lib64:/lib:/opt/lib \
  --env HOME=/tmp --env DISPLAY=:1 \
  --program-from-tar --initial-files /tmp/lbx/rootfs-x11.tar \
  /usr/bin/python3 -u /tmp/x11proxy.py -- \
  /usr/lib/chromium/chromium --ozone-platform=x11 --disable-gpu --no-first-run \
  --disable-dev-shm-usage --disable-breakpad --disable-crash-reporter \
  --no-default-browser-check --user-data-dir=/tmp/udd-x11 --window-size=1024,768 \
  --enable-logging=stderr --single-process \
  --disable-features=WebContentsForceDark file:///page.html \
  > /tmp/lbx/log-final.txt 2>&1
# in a second shell, while it runs:
bash /tmp/lbx/watchshot.sh final 590 30     # xwd every 30 s + md5, -> watch-final.txt
DISPLAY=:1 xwininfo -root -tree | grep -i chromium
```

`MODE=chromium EXTRA=--disable-features=WebContentsForceDark bash /tmp/lbx/x11run.sh <tag> <secs>`
is the same thing (it also builds the tar and runs the watcher separately).

`/tmp/lbx/lbxrun` is a **symlink** to `target/release/litebox_runner_linux_userland`. It is not
cosmetic: another agent sweeps with `pkill -KILL -f litebox_runner`, whose pattern matches the
pump's own argv, and that SIGKILLed run `final1` at 250 s. One runner is still all that runs.

Guest program is `python3 /tmp/x11proxy.py`: it binds `/tmp/.X11-unix/X1` **inside litebox**,
accepts, and execs chromium unmodified. **Zero Rust changes** — no shim, platform or runner edits,
no cargo, no commits. Only the three Python files plus `--initial-files` injection.

## Evidence

### 1. A managed window on the real desktop

```
0x2a00002 "litebox chromium - Chromium": ("chromium-browser (/tmp/udd-x11)" "Chromium-browser")
          1024x768+0+0  +10+37
```

1024x768 is exactly the `--window-size` passed; `+10+37` means **xfwm4 framed it** — a managed
window, not an override-redirect surface. Parented under root next to `xfce4-panel`, `xfdesktop`
and `Xfce Terminal`, so it is on the same desktop selkies streams.

### 2. The real page renders (`page.html`: yellow `#ffd400` + `hello`)

`page.html` is `<body style="background:#ffd400">` with `<h1>hello</h1>`, a 260x260 animating box,
and a JS tick counter. Capture #4, decoded with `xwdshot.py`:

| element | pixels | geometry |
|---|---|---|
| `#ffd400` page field | 545,200 (69.3%) | y 148..763 (below the toolbar) |
| `#e81123` animated box | 67,600 | x 36..295, y 312..571 — exactly 260x260, one run per row |
| white browser chrome | 81,418 | y 6..142 |
| black text ink | ~10.4k | y ~200..265 and ~600..660 |

Min-pooled ASCII of the heading band reads **`hello`**; the lower band reads **`tick …`**. Fonts
render (the image carries 384 TTF/OTF and `etc/fonts/conf.d`).

### 3. It is live, not one frozen frame

Watcher captures every 30 s, `md5` of each `.xwd`:

```
17:46:32 #1 md5=9ee6b868…   (pre-first-paint, all white)
17:47:03 #2 md5=fb714ebd…   box #0066ff blue 67,600 px
17:47:33 #3 md5=2affa7a3…
17:48:05 #4 md5=3f54d2cb…   box #e81123 red  67,600 px
17:48:35 #5 md5=f39ed988…
```

5 captures, 5 distinct md5s, and **the same 67,600-pixel box is blue in #2 and red in #4** — the
CSS keyframe animation is being composited and repainted through the tunnel. The earlier 600 s run
(`final2`) produced 19 captures, **18 distinct md5s**.

### 4. Where the X11 conversation gets to

From `[pump]` lines: `setup request 12 bytes` → `setup reply 2596 bytes status 1`, then
BIG-REQUESTS, DRI3, GLX, RANDR, RENDER, MIT-SCREEN-SAVER, SHAPE, MIT-SHM, SYNC, XFIXES,
XInputExtension, XKEYBOARD, XTEST. **DRI3 and MIT-SHM are answered `present=0`** (mandatory — a
guest can never map a host shm segment or dma-buf), and chromium falls back to plain
**`PutImage`** — the last request on the painting connection in the 600 s run was
`cid 3 seq 400 PutImage`. Nothing in the conversation fails; there is no error or Bad* reply at
the point the window goes quiet.

## Blockers hit, and the workaround for each

1. **No path from guest to a host socket.** No `/dev/net/tun`, so X11-over-TCP is impossible; spill
   is inert on this container (`LITEBOX_SHARED_WRITE_PREFIXES` is ANDed with
   `platform.spill_available()`, default `false`); and a guest AF_UNIX connect never leaves the
   in-memory `unix_addr_table`. **Workaround:** guest stdio *is* host pipes
   (`litebox/src/fs/devices.rs:642-703`) — carry raw X11 bytes over them, framed
   `b"X11C"|u32 cid|u32 len|payload`. Host→guest is genuinely writable: the 2596-byte setup reply
   and every extension reply reach the guest over the runner's **stdin** (with stdin at
   `/dev/null` instead, the proxy logs `stdin EOF after 0 frames`).
2. **Chromium auto-dark painted the page black.** The first good run showed 70% pure black with
   white text: chromium applied auto-dark to the light page, hiding `#ffd400` entirely. Fixed with
   `<meta name="color-scheme" content="only light">` in the page **and**
   `--disable-features=WebContentsForceDark`. After that: 69.3% `#ffd400`.
3. **My own capture decoder was wrong twice, and one bug hid the other.** (a) `xwd` puts the
   colormap (`ncolors*12` bytes, here 256*12 = 3072) between the header and the pixels, and
   `header_size` does **not** count it — starting at 107 sheared every row and split a 260x260 box
   across opposite window edges. (b) A flat-colour calibration window cannot reveal that, because a
   flat fill is shift-invariant; and my calibration colour was magenta, which is symmetric under
   the R/B swap that `byte_order == 0` requires. Calibrate with an asymmetric colour
   (`/tmp/lbx/calib.py`, target `(0,136,255)` reads back `(0,136,255)`).
4. **CPU decides whether this works at all.** Chromium under litebox paints in ~3 min at load
   < 6; at load 10.2 (`gu3`) it never even reached X in 260 s. Launch only when load < 8.

## Still open

1. **Multi-process is untested and the sandbox is not exercised.** This works with
   `--single-process`, which means chromium never engages its sandbox (zero `seccomp` lines in any
   log). `--no-sandbox` is absent, but so is the sandbox. Multi-process dies in the Mojo bootstrap
   (`docs/CHROMIUM-NO-SANDBOX-2026-10-10.md` §8) and other agents own that.
2. **The window dies with the runner.** It exists only while the `timeout` budget lasts; at
   600 s the window is destroyed (`Bad Drawable` afterwards). "Visible to a person watching
   selkies" is therefore bounded by the run length — there is no way to detach it today.
3. **No input path.** The tunnel carries client→server bytes; there is no host→guest way to inject
   key/mouse events beyond what the pump forwards from the guest itself.

## Files

| file | role |
|---|---|
| `/tmp/lbx/x11proxy.py` | guest: binds `/tmp/.X11-unix/X1`, execs client, frames over stdio |
| `/tmp/lbx/x11pump.py` | host: one real X socket per conn, strips MIT-SHM/DRI3/XFree86-DRI |
| `/tmp/lbx/x11run.sh` | wiring; `MODE=raw|dom|chromium`, `EXTRA=` for extra chromium flags |
| `/tmp/lbx/x11client.py` | Stage 0 raw-X11 client (no xcb in the guest) |
| `/tmp/lbx/xwdshot.py` | xwd → PNG + colour histogram (colormap offset fixed) |
| `/tmp/lbx/watchshot.sh` | periodic `xwd` + md5, the liveness harness |
| `/tmp/lbx/calib.py` | host-side known-colour window, calibrates the decoder |
