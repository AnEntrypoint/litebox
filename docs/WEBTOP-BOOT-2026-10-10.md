# Booting the whole webtop desktop INSIDE litebox (2026-10-10)

Goal (`docs/WEBTOP-IN-LITEBOX-2026-10-10.md`): the litebox **guest** is webtop — `Xvfb :0` +
`xfce4-session` + `chromium DISPLAY=:0`, multi-process, **no `--no-sandbox`**. X11 stays on a
*guest-internal* AF_UNIX socket at `/tmp/.X11-unix/X0`, so unlike the earlier stdio-tunnel result
(`docs/VISIBILITY-PROVEN-2026-10-10.md`) there is no host fd, no pump and no MIT-SHM/DRI3 stripping.

## 1. The rootfs (`/tmp/lbx/rootfs-webtop.tar`)

**1,057,935,360 bytes, 7,094 entries** (target was < 1.5 GB). Built by `/tmp/lbx/mkwebtop.sh` from
staging `/tmp/lbx/wt`, which is seeded by extracting the proven `rootfs-x11.tar`
(chromium + python3.14 + 388 TTF/OTF + fontconfig).

Added on top of the base:

| what | why |
|---|---|
| ldd closure of `Xvfb xfwm4 xfce4-session xfdesktop xfce4-panel xfsettingsd xfconf-query xkbcomp xwd startxfce4 xfce4-terminal xwininfo xdpyinfo xsetroot xauth xrdb dbus-{daemon,launch,run-session,uuidgen,send}` | the desktop itself |
| ldd closure of `dash` + coreutils | `xfce4-session` spawns helpers via `/bin/sh -c`; the base rootfs had **no shell at all** |
| `/usr/lib/x86_64-linux-gnu/xfce4` (4.9 M) | dlopened panel/session/xfconf modules |
| `/usr/share/X11`, `/usr/share/xkeyboard-config-2` (4.0 M) | Xkb keymaps — `/usr/share/X11/xkb` is a **symlink** to `../xkeyboard-config-2`, so both are needed |
| `/usr/share/{xfce4,fontconfig,glib-2.0,pixmaps,dbus-1,poppler}`, `/etc/{xdg,X11}` | desktop data; `rgb.txt` and the `fonts/cmap` symlinks point out of their trees |
| icons `hicolor` + `Adwaita` + `default` (21 M), themes `Default*` + `Greybird` (3.9 M) | xfwm4 needs a theme to draw decorations; `Breeze`/`elementary-xfce-hidpi` (66 M) dropped |
| `/etc/{passwd,group,hosts,nsswitch.conf,resolv.conf,hostname,localtime,timezone,machine-id}` | **the base rootfs shipped none of these**; AGENTS.md records chromium's tid 17 retrying `nsswitch.conf` + `hosts` every ~1.4 s |
| `tmp var/tmp var/log run run/user/0 home/abc root dev/shm tmp/.X11-unix tmp/.ICE-unix` | runtime dirs; `/tmp` mode 1777 |

Everything is tarred `--owner=0 --group=0 --numeric-owner` so the guest (which runs as root) owns
its own rootfs.

### Two build bugs that silently break the guest — both cost a full rebuild

1. **`cp -a src dst` where `dst` already exists nests `src` INSIDE `dst`.** Re-running the build
   script produced `usr/share/X11/X11/`, `usr/share/xfce4/xfce4/`, `usr/share/pixmaps/pixmaps/`, …
   (8 nested dirs) and left `/usr/share/X11/xkb` pointing at a path that only existed in the nested
   copy. Fix: copy the *contents* — `mkdir -p "$WT$1"; cp -aL "$1/." "$WT$1/"`.
2. **`cp -a` preserves symlinks, so a versioned `.so` name lands as a dangling link.** `ldd` reports
   `libpango-1.0.so.0 => /usr/lib/x86_64-linux-gnu/libpango-1.0.so.0`, which is a symlink to
   `…so.0.5700.0`. Copying only the link leaves **179 dangling symlinks** and the dynamic loader
   fails on every desktop binary. Fix: dereference (`cp -aL`) so every name carries real bytes.
   Cost: staging grew 698 MB -> 1023 MB, because the desktop's `libicu`/`libvte`/`libpango` payload
   genuinely has to be in the archive.

`mkwebtop.sh` now ends with a dangling-symlink audit; it reports **0**.

## 2. Rungs, one process at a time

`/tmp/lbx/wtrun.sh <tag>` runs one rung under `nice -n 19 timeout -s KILL`, gated on the shared
lock, 1-minute load < 4 and MemAvailable > 2 GB. The guest program is
`/usr/bin/python3 -u /tmp/boot.py`, which forks the rung's processes, waits for
`/tmp/.X11-unix/X0`, and emits framed root dumps on **stdout** (`b"XWD%d\n" % nbytes` + raw xwd),
since guest files are not host-visible but guest stdout is (`litebox/src/fs/devices.rs:642-703`).

```
RUNG=1  Xvfb :0 -screen 0 1024x768x24 -nolisten tcp -ac -retro
RUNG=2  + xfwm4 --display :0 --replace
RUNG=3  + xfce4-session                (session starts wm/desktop/panel itself)
RUNG=4  + chromium DISPLAY=:0          (--single-process only as a stepping stone)
```

`-retro` is deliberate: it paints the classic X stipple on the root window, so a rung-1 capture is
**not** a flat black frame and a working X server is distinguishable from a blank one.

Host side: `python3 /tmp/lbx/wtshot.py out-<tag>.txt /tmp/lbx/shots` extracts and decodes the
frames to PNG with a colour histogram. Validated against a native `xwd -root` capture
(1024x768 depth 24 bpp 4, 3,148,907 bytes, `distinct=1 BLANK` for a bare native Xvfb).

## 3. Results

Every rung is one command (`RUNG=<n> HOLD=<secs> SECS=<secs> [CONN_PROBE=<n>] bash /tmp/lbx/wtrun.sh
<tag>`), which runs exactly:

```
nice -n 19 timeout -s KILL <SECS> /tmp/lbx/lbxrun --unstable \
  --env LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu:/lib64:/lib:/opt/lib \
  --env HOME=/root --env RUNG=<n> --env HOLD=<secs> \
  --env DISP=:0 --env SHOT_EVERY=25 --env CONN_PROBE=<n> \
  --program-from-tar --initial-files /tmp/lbx/rootfs-webtop.tar \
  /usr/bin/python3 -u /tmp/boot.py
```

### Rung 1 — `Xvfb :0` alone: **PASSES** (`r1b`, rc=0, out 12,600,595 B, err 576 B)

`/tmp/.X11-unix/X0` appears; `xdpyinfo` rc=0 reports `X.Org version: 21.1.21`, 22 extensions,
maximum request size 16,777,212; `xwininfo -root -tree` shows root `0x42` with 0 children. Four
`xwd -root` captures, each **3,148,907 bytes — byte-identical in size to the native healthy-Xvfb
reference**. Decoded: 1024x768 depth 24 bpp 4, `distinct=2`, 589,824 black + 196,608 white =
786,432 px, and the ASCII dump is X's classic diagonal root weave.

**A real X server is running inside litebox, on a guest AF_UNIX socket, serving real X clients.**

The first attempt (`r1`, rc=2) failed and it was **my rootfs bug, not litebox**: the archive shipped
`tmp/.X11-unix` as `0755 root-owned` while the guest runs as **uid 1000**, so Xvfb's `bind()` got
EACCES — `_XSERVTransSocketCreateListener: failed to bind listener`. Fixed by shipping 01777.

### Rung 2 — `+ xfwm4`, with `xclock` as a probe client: **client works, WM does not**
(`r2`, rc=0, out 18,898,059 B, err 5,762 B, 6 frames)

`xclock -update 1 -geometry 200x200+40+40` — a real Xaw/Xt client — opened the display, mapped a
window and **repainted continuously**:

* frames 3-6 differ from the bare-root frames 1-2 and from each other (**5 distinct of 6**);
  black 589,824 -> 560,588 and white 196,608 -> 224,639, i.e. ~30,000 px replaced inside a
  200x200 area (the weave was already 25 % white, so +28,031 white is exactly a 40,000 px window
  painted over it);
* the changed bounding box is **(40,40)-(242,242)** — the requested `200x200+40+40` plus a 2 px
  border;
* between two captures 25 s apart, 904-1,005 px change **inside** that window, and the diff is
  hand-shaped (a vertical and a diagonal line) — the clock hands moved.

That is the liveness artifact: a moving, repainting X client rendered entirely inside litebox.

**But `xfwm4` (Gtk) did not manage it**: `(xfwm4:6): Gtk-WARNING **: 20:15:05.475: cannot open
display: :0`, and no title bar appears on the xclock window. The window manager is the one piece
of rung 2 that fails, and it is Gtk — xlib clients (`xdpyinfo`, `xwininfo`, `xclock`) all connect
fine. Note the timestamp: it is ~50 s after xfwm4 was spawned, so it did not fail immediately, it
**stalled and then gave up** — the shape of a connection that never completes, i.e. the
cross-process wake gap (`docs/CROSS-PROCESS-WAKE-2026-10-10.md`), not a missing library.

Repeated as `r2b` (rc=0, out 15,749,107 B, 5 frames, 4 distinct): xclock paints identically
(frames 3-5, 220-225 distinct colours) and `xfwm4` fails identically
(`20:28:14.456: cannot open display: :0`). Two failures in a row rules out "flaky 1-in-6".

### Rung 2 with `-noreset` (`r2c`, rc=0) — the hang is gone, `xfwm4` dies on a missing data file

`CONN_PROBE=6`, same binary, only the flag added:

```
connprobe 0..5 elapsed=0.06/0.06/0.07/0.06/0.07/0.06 rc 0 bytes 2365   (was: probe 1 = 40 s FAILED)
```

`xfwm4` no longer reports `cannot open display` — it connects, creates its offscreen window
(`0x400001 10x10+-20+-20`, same as `r3`) and then dies:

```
(xfwm4:12): dbind-WARNING: AT-SPI: Error retrieving accessibility bus address: ... Spawn.ExecFailed
(xfwm4:12): xfwm4-WARNING **: 20:58:49.156: Missing defaults file
[boot] DIED /usr/bin/xfwm4 pid 12 rc 1
```

**Another rootfs gap, not litebox**: `dpkg -L xfwm4` puts that file at `/usr/share/xfwm4/defaults`
(1,679 B) and the archive did not contain `/usr/share/xfwm4` at all. Added; a diff of every host path
under `/usr/share/{xfce4,xfwm4,dbus-1,themes/Default*,themes/Greybird,icons/default}` and `/etc/xdg`
against the archive then reports **972 checked, 0 missing**, so the desktop data is now complete.

`xclock` still paints and still repaints (2 frames, both distinct, 217 colours, black 589,824 ->
560,611 = the 40,000 px window over the weave).

### The "2nd connection" blocker was **Xvfb's own server reset** — a launch flag, not litebox

`SOCK_PROBE=N` times the three phases of a raw AF_UNIX connect (no libX11 in the way). `sp2`
(`SOCK_PROBE=4`, default flags) and `sp3` (identical but `XVFB_FLAGS="... -noreset"`) differ only in
that one flag:

| | probe 0 | probe 1 | probe 2 | probe 3 | after |
|---|---|---|---|---|---|
| **`sp2`** default flags | `n=8` | `n=0` (EOF) | `ENOENT` | `ENOENT` | `xdpyinfo` 60 s timeout, `xwd` "unable to open display" |
| **`sp3`** `+ -noreset` | `n=8` | `n=8` | `n=8` | `n=8` | `xdpyinfo` rc=0, `xwd` 3,148,907 B |

`sp3` logged, per probe, `postprobe <i> exists True dir ['X0'] xvfb_rc None` — the listener file and
Xvfb both survive all six connections. In `sp2` the socket file was **gone** two probes later and
never came back within 60 s.

Cause: **an Xserver resets when the last client disconnects unless `-noreset`** — it drops every
connection, unlinks `/tmp/.X11-unix/X0` and re-binds. Each probe client connects, reads its 8-byte
setup reply and closes, so probe 0's close pulls the display out from under probe 1. That is the
whole shape of the earlier table: `xdpyinfo` (1st client, closes) → `xwininfo`/`xfwm4` (2nd client,
arrives into the reset) → stall, then the 3rd onward are fine because the reset has finished.

So `xfwm4`'s `cannot open display: :0` and the 40 s `connprobe 1` are **not** the cross-process wake
gap and **not** a litebox defect. Three corrections follow:

* the fix is a flag we choose — `-noreset` is what every real Xvfb deployment passes;
* a real desktop never triggers it anyway (xfwm4/xfdesktop stay connected for the life of the
  session), so it would not have blocked the delivered configuration;
* `10fa53cd` is exonerated by a better experiment than the one that indicted it, and the same
  applies to my own "rank-1 unbounded park" theory — neither was ever in this path.

**Still unexplained (a real litebox question, corner case):** after a reset the listener never came
back for the rest of `sp2` (60 s+), and Xvfb printed no fatal error. Re-binding `/tmp/.X11-unix/X0`
after an unlink may not work in litebox. Worth one probe (spaced-out connects, no `-noreset`), but
it cannot block the desktop.

`CONN_PROBE=N` opens N sequential X connections, one fresh process each, and times them (`cp1`,
6 iterations, 40 s timeout each):

```
connprobe 0 elapsed=0.08 rc 0
connprobe 1 elapsed=40.02 FAILED TimeoutExpired   <-- the 2nd connection, never completed
connprobe 2 elapsed=0.06 rc 0
connprobe 3 elapsed=0.16 rc 0
connprobe 4 elapsed=0.16 rc 0
connprobe 5 elapsed=0.17 rc 0
```

Three independent runs put the hang at the **same place**:

| run | 1st client | 2nd client | 3rd+ |
|---|---|---|---|
| `r1`  | `xdpyinfo` 0.1 s | `xwininfo` **timed out at 20 s** | ok |
| `r2`  | `xdpyinfo` ok | `xfwm4` **stalled ~50 s, gave up** | `xclock` ok |
| `r2b` | `xdpyinfo` ok | `xfwm4` **stalled ~22 s, gave up** | `xclock` ok |
| `cp1` | probe 0 0.08 s | probe 1 **40 s timeout** | probes 2-5 ~0.15 s |

### Rung 3 — `+ xfce4-session` (`r3`, rc=0, out 25,196,597 B, err 10,308 B)

Run on the 20:30 binary (cross-wake fix in). Two things changed at once:

* **`xfwm4` connected.** No `cannot open display` this time, and the tree gained xfwm4's offscreen
  helper window:

  ```
  Root window id: 0x42
      2 children:
      0x40000c "xclock": ("xclock" "XClock")  200x200+40+40  +40+40
         1 child:
         0x40000d (has no name): ()  200x200+0+0  +41+41
      0x600001 (has no name): ()  10x10+-20+-20  +-20+-20
  ```

  So a window manager process runs inside litebox and stays up. It did not manage `xclock`
  (no reparenting frame), and xclock is still unframed.

* **`xfce4-session` exited rc=1**, and the cause is my rootfs, not litebox: the archive is written
  `--owner=0 --group=0`, so `/root` (0700) and `/run/user/0` are **root-owned** inside the guest
  while the guest runs as **uid 1000**:

  ```
  dbus[15]: Unable to set up transient service directory: XDG_RUNTIME_DIR "/run/user/0" is owned by uid 0, not our uid 1000
  Unable to create /root/.dbus
  Unable to create /root/.dbus/session-bus
  ```

  (same root cause as fontconfig's `No writable cache directories`). Fix: the driver now creates
  and exports `HOME=/tmp/home`, `XDG_RUNTIME_DIR=/tmp/xdg`, `XDG_{CONFIG,CACHE,DATA}_HOME` under
  it — `/tmp` is world-writable and demonstrably so, since Xvfb creates `/tmp/.X0-lock` and
  `/tmp/.X11-unix/X0` in it.

### Rung 3 with `-noreset` + the HOME fix (`r3b`, rc=0, out 15,753,287 B, err 6,144 B)

**The window manager now genuinely manages.** `xwininfo -root -tree` after rung 2:

```
0x2018fb (has no name): 210x234+40+40        <- xfwm4's FRAME around xclock
    16 children:
    0x80000c "xclock" 200x200+5+29           <- the client, inset by the frame
    0x20190a..0x201906  21x29 x5             <- the five title-bar buttons
    0x201903  194x29  (title bar)
    0x2018fd/0x2018fc  5x189  (left/right border)
    0x2018fe  178x5   (bottom border)
    0x201900/0x2018ff 16x16   (corners)
0x200035..0x200038  1x768 / 1024x1 at the screen edges   <- xfwm4's own edge windows
```

That is a real WM decoration built by `xfwm4` inside litebox: frame, title bar, five buttons, four
borders, two corners — `xclock` requested `200x200+40+40` and now sits inside a `210x234` frame. It
was unframed in every earlier run.

**`xfce4-session` then starts the desktop and aborts.** It spawned `xfwm4` (pid 48),
`xfsettingsd` (52), `xfce4-panel` (59), `migrate` (67) and died **rc=134 (SIGABRT)**. Two rootfs
gaps, no litebox involvement:

1. **The donor image ships gdk-pixbuf 2.44.5 with no loaders at all** —
   `gdk-pixbuf-query-loaders` lists zero modules and there is no `libpixbufloader*.so` anywhere on
   the host. So every icon load fails, GTK falls back to `image-missing.svg`, that fails too, and
   `gtkiconhelper.c:495` asserts: `Failed to load
   /usr/share/icons/Adwaita/scalable/status/image-missing.svg: Unrecognized image file`, alongside
   libwnck's `xutils.c:1510: assertion failed: (base)`.
2. **`/usr/bin/iceauth` was never packaged**, so `xfce4-session`'s ICE setup fails
   (`sh: 1: /usr/bin/iceauth: not found`) and `xfsettingsd`/`xfce4-panel` are then
   `ICE connection ... rejected`. `libsystray.so` also wanted `libdbusmenu-gtk3.so.4`.

Fixed as one packaging pass (`/tmp/lbx/mkwebtop-add.sh`): `iceauth`; the `ldd` closure of every
dlopened xfce4 module (117 libs); the SVG pixbuf loader from `librsvg2-common` (fetched with
`apt-get download` — the package is simply not installed on this host) plus a `loaders.cache`;
`/usr/share/mime`; and `gtk-update-icon-cache` on all three icon themes.

### Rung 4 — chromium inside the desktop, multi-process, no `--no-sandbox` (`r4a`)

`rc=124` (hit `SECS=900` during the hold), out 12,612,862 B, err 9,067 B. **45 guest processes**, and
the guest's own `/proc` census is the artifact — this is a desktop, not one binary:

```
73 /usr/bin/xfce4-session          30,47 xfconfd            21,44 dbus-launch
63 xfce4-panel                     52 xfsettingsd           74 xfdesktop
38 xfwm4                           71 .../panel/migrate     37 iceauth
78/79/85 /usr/lib/chromium/chromium                 <- browser processes
80/81/82 chrome_crashpad_handler
86/87/91/92/116 chromium --type=zygote              <- BOTH zygotes
119 /proc/self/exe --type=utility --utility-sub-type=network.mojom.NetworkService
```

`xfce4-session` no longer aborts (the icon/`iceauth` pass fixed it) and chromium runs
**multi-process with two zygotes, no `--no-sandbox`**, inside the guest display.

`xwininfo -root -tree` at rung 4 — **21 root children**, including

```
0x1c00001 "chromium": ("chromium" "Chromium")       <- chromium has a window on :0
0x1600001 "xfdesktop": ("xfdesktop" "Xfdesktop")
0x1800001 "xfsettingsd"   0x1200001 "xfce4-panel"   0x1400001 "migrate"
0xe00001  "xfce4-session" 0x200001 "xfwm4"
0x2018fc  210x234+40+40   16 children  <- xfwm4's frame around xclock
```

Frames: 4 captured, 3 with content, **231 distinct colours**. What painted:

* **the root is now solid black** (737,989 of 786,432 px) where the bare server showed the weave
  (589,824 black + 196,608 white) — that is `xfdesktop` setting the root background, i.e. the
  desktop component did real work on the display;
* **xclock inside its xfwm4 frame** — every non-weave colour lies inside bbox (40,40)-(249,273),
  exactly the 210x234 frame.

What had not painted when the capture was taken: `xfce4-panel` and chromium both show only their
10x10+10+10 group-leader stubs, and no panel bar or browser content appears in the frame. Chromium
had 15 s here; the other track measures ~19.7 s to first DOM. Honest reading: **the desktop and a
multi-process chromium are up inside litebox and own X windows; whether they paint is not yet
proven.** Re-run with a 60 s settle and a 240 s hold.

### Rung 4 with a 60 s settle and a 240 s hold (`r4c`, rc=0) — chromium has a real, managed 1023x767 window, and the screen still reads black

`rc=0`, out 44,115,119 B, err 17,071 B, 14 root frames of 3,148,907 B each, **65 guest processes** at
the final census. The run ended on its own (`SECS=1500`), so nothing here is a truncated arm.

**The 10x10 "chromium" of `r4a` was a group leader, not the window.** The final tree has 18 root
children, and among them:

```
0x201d5a (has no name): ()  1023x767+5+5  +5+5          <- xfwm4's frame, 16 children
    0xe00003 "litebox chromium - Chromium": ("chromium-browser (/tmp/home/udd)" "Chromium-browser")  1023x767+0+0  +5+5
0x1200001 "chromium": ("chromium" "Chromium")  10x10+10+10        <- the group leader stub
```

`0xe00003` is **full screen, reparented into an xfwm4 frame with titlebar buttons and corner
handles, and titled `litebox chromium - Chromium`** — the title is `page.html`'s `<title>`, so
chromium navigated the document and set its window title from the DOM. Multi-process, no
`--no-sandbox`, inside webtop, inside litebox. **`r4a`'s "chromium showed only a 10x10 stub" was a
settle-time artifact: the window needs ~60 s to be mapped and managed.**

**But every frame from #3 on is 786,432 black pixels, `distinct=1`** — and xclock vanishes with
them (frame 2 has 38,253 white px of clock face, frame 3 has none). Two independent clients do not
both stop painting; something covers the screen. The trees name it: `0x201974` goes from
`10x49+0+0` (tree 1, before the session) to **`1023x767+0+0`** (trees 2 and 3), full screen, and it
stacks **above** chromium's window, which was mapped later — only a redirected/overlay window can do
that. The log lines up: `(xfwm4:52) Could not find a screen to manage, exiting`, then
`(xfwm4:6) Another compositing manager is running on screen 0`. The session's `xfwm4` took
`_NET_WM_CM_Sn`, created the composite overlay, exited, and left a fullscreen overlay mapped with
nobody painting into it; the selection stays owned, so nobody can take it back.

So the display is not broken — it is **covered by a compositing overlay**, and with windows
redirected, both `xwd -root` and `xwd -id` read that overlay. Two changes, both ours:

* `xfwm4 --display :0 --replace --compositor=off` for the instance boot.py spawns;
* xfconf seeded **before** `xfce4-session` starts, since the session spawns its own unflagged
  `xfwm4`: `$XDG_CONFIG_HOME/xfce4/xfconf/xfce-perchannel-xml/xfwm4.xml` with
  `use_compositing` = false;
* and the capture that separates *covered* from *unpainted*: `xwd -id <window>` grabs on the
  biggest windows (chromium, xclock, the fullscreen one), not just `-root`.

### With compositing off (`r4d`, rc=0, out 76,390,799 B) — the overlay is gone, and chromium's window is still empty

`xfwm4 --display :0 --replace --compositor=off` for the instance boot.py spawns, plus
`xfconf seeded /tmp/home/.config/xfce4/xfconf/xfce-perchannel-xml/xfwm4.xml bytes 207` before
`xfce4-session` starts (`use_compositing` = false), plus `xwd -id` grabs on the biggest windows.
31 frames; 65 guest processes; the run ends on its own.

**The black overlay is gone.** Frames 3-31 read `distinct=3`: **777,497 px of (206,206,206)** — a
flat grey field with the 1 px black edge windows xfwm4 puts at the screen borders
(`0x200035 1x768+-1+0`, `0x200037 1024x1+0+-1`) — plus 6,701 black and 2,234 white. No frame is
all-black any more.

**But the two `xwd -id` grabs settle the question the coordinator asked, against the covering
theory:**

| grab | size | result |
|---|---|---|
| `0xe00002 "litebox chromium - Chromium"` (chromium's window) | 1019x763 | `md5=ef53401dd535`, `distinct=1`, **uniform (206,206,206)** |
| `0x201c43` (the unnamed fullscreen window) | 1019x763 | **the same md5** — byte-identical to chromium's |
| `0x80000c "xclock"` | 200x200 | `distinct=1`, **uniform black** |

Chromium's grab is byte-identical to the fullscreen window behind it, so the pixels in
chromium's region are still the ones painted underneath — **chromium's window is mapped
fullscreen and has never drawn**, and everything below it (xclock, panel) is hidden behind that
undrawn region. A window that is mapped but unpainted leaves the framebuffer showing what was
there before, which is why the whole screen is one flat grey. So: not a compositing overlay, not
stacking — **multi-process chromium maps a correctly managed 1023x767 window and never
composites into it.** (`--single-process` is the configuration that did paint here:
`docs/VISIBILITY-PROVEN-2026-10-10.md`, 2400 distinct colours.) Per-window grabs are what
settled it; root grabs alone could not.

Run 4 (`r4e`, rc=11, out 3,156,212 B) is the loader fix landing in the wrong place: moving
`loaders.cache` from `.../2.10.0/loaders/` to `.../2.10.0/` (where gdk-pixbuf's compiled-in
default looks) did **not** stop `(xfce4-session pid 32 rc 134)` on
`gtkiconhelper.c:495 ... Failed to load .../image-missing.svg: No image loaders are configured.
Used config: Config { image_loader: {}, image_editor: {} }`. That empty config is **glycin**, not
gdk-pixbuf: the rootfs ships `libglycin-2.so.0` but none of `/usr/libexec/glycin-loaders/2+/`
or `/usr/share/glycin-loaders/2+/conf.d/`, so GTK has no loader process to spawn.

### The icon loader is **glycin** (`r4f`, rc=0, out 76,149,005 B) — and with it the panel paints

Packaging `/usr/libexec/glycin-loaders/2+/{glycin-svg,glycin-image-rs}` and
`/usr/share/glycin-loaders/2+/conf.d/{glycin-svg,glycin-image-rs}.conf` into the archive (their ldd
closure, 36.3 MB, was already there — only 8.3 MB was new) removes **every** `gtkiconhelper.c:495`
assertion: `grep -c gtkiconhelper` on the guest's stderr is **0**, where `r4d` had them and `r4e`
lost `xfce4-session` to one (`rc 134`).

The pixels follow. Root frames go from `distinct=3` (`r4d`) to **`distinct=202`**, and the
non-background pixels sit in **one 21-row band, y 0..20, x 0..1016** — the panel bar:

```
non-grey bbox: x 0..1016  y 0..20      rows with the most: 9, 13, 8, 10, 12, 11, 14
1,500 non-(206,206,206) px inside the bar, colours (133,133,133), (201,201,201), ...
```

So **the desktop paints its own background and a panel bar with icon content**, and it survives the
whole 240 s hold (`rc=0`, 0 icon assertions). Chromium's own grab is unchanged: `md5=b5fa14cbec08`,
1024x744, `distinct=1`, uniform (206,206,206).

### Rung 5 (chromium on a bare `xfwm4`, no session) — `r5a` WEDGES, and it wedges with a 300-process guest census

Isolating chromium from the desktop: `Xvfb` + `xfwm4 --compositor=off` + `xclock` + chromium, no
`xfce4-session`. The rung-2 census reports **315 guest processes, 303 of them
`/usr/bin/xfwm4 --display :0 --replace --compositor=off`** with pids 44…631 (step 2), and after
`shot rung2` the guest stops producing output entirely (out file frozen 7 min, killed at 10 min).

The storm is **guest-side bookkeeping, not host forks**: the host held only 19 `lbxrun` processes
and load 1.3-2.0 the whole time, so those ~300 entries are not 300 host processes. They are also
not threads of a healthy run — `r4d`, where the same `xfwm4` (pid 6) lived through the whole run,
counted 14 / 29 / 47 / 81. So whatever the entries are, they appear only when our `xfwm4` runs
without the session behind it.

Unresolved, and worth one short run (30 s settle, no hold) to see whether the storm predates
chromium: **it is not known whether the wedge causes the storm or the storm causes the wedge.**
Either way, rung 5 is not yet a usable isolation, so the chromium question went back to rung 4 with
different chromium flags instead.

### `r4g` (`--enable-unsafe-swiftshader`) WEDGES at rung 3 — no verdict, and the wedge is not new

Adding `--enable-unsafe-swiftshader` to the chromium flags did not get far enough to be measured:
the guest's stdout froze after `shot rung3` for 10 minutes (host load 34-38 the whole time) and the
run had to be killed. **Only 2 frames exist: `md5=cc81d3e05725` `distinct=2` (the bare weave) and
`md5=eb31b729dacd` `distinct=10`** — so `xfdesktop` had not even painted the background yet, let
alone chromium. **No chromium `xwd -id` grab was produced, so swiftshader is untested here.**

`r5a` wedged the same way (one frame, md5 `cc81d3e05725`, stdout stopping 165 B after the chromium
spawn line). Both are null results, not evidence.

### The ~500-process guest census is litebox-internal, not 500 host forks

Census sizes: `r4c` 18/33/51/65, `r4d` 14/29/47/81, **`r4f` 12/499/513/515**, `r4g` 299/505. The
extra entries all carry one parent's argv (`/usr/bin/xfwm4 --display :0 --replace
--compositor=off`, pids 30,32,34…~630 step 2) and appear only after the glycin loaders landed.

Measured at rung 3 of `r4g`: **guest census 505, host `lbxrun` processes 14.** Five hundred
processes cannot hide in fourteen, so these are **litebox-internal process objects**, not host
forks. Two explanations remain, and they are distinguishable:

* a guest-pid allocator that hands out a fresh pid per `clone()` — including the
  `CLONE_FS|CLONE_FILES` ones that stay in-process — and never reuses (`r4c`/`r4d`, which used the
  same driver without the glycin loaders, stayed at 65/81, so whatever clones these are, the
  loaders are what triggers them);
* real forks that exit and are never reaped, leaving `/proc` entries behind.

`rootfs-webtop13.tar` ships a census that prints `ppid` and the `/proc/<pid>/stat` state for every
entry, plus a `procs by (ppid,state)` tally — state `Z` means unreaped, anything else means live
guest objects. That one run answers it; until then the count is recorded as an observation only.

### The 2nd-connect hang is NOT the data plane (measured, both ways) — superseded above

`CONN_PROBE=6` re-run on the 20:30 binary with the cross-wake fix **on** (`cp3`, switch unset):

```
connprobe 0 elapsed=0.06 rc 0
connprobe 1 elapsed=40.02 FAILED TimeoutExpired   <-- unchanged
connprobe 2 elapsed=0.06 rc 0
...
```

Identical to `cp1`/`cp2` on the pre-fix binary, so `10fa53cd` does not touch this — as its author
said. The rendezvous has a **15 s** bound (`SHARED_UNIX_CROSS_CONNECT_TIMEOUT`, `unix.rs:4111`) and
returns `ECONNREFUSED` on expiry, so a stall *inside* it could not present as a 40 s hang that
never returns; the stall must be elsewhere. `SOCK_PROBE=N` was added to time the three phases of a
raw AF_UNIX connect (connect / write / read) separately, which the `xdpyinfo` probe cannot split.

### Litebox finding so far (not yet fixed): a tar's mode bits above 0777 are dropped

The archive entry for `tmp/` is `drwxrwxrwt` (01777); the guest reports `0o777` (`[boot] chmod
FAILED /tmp ... mode is 0o777`). Cause:

* `litebox_runner_linux_userland/src/lib.rs:840`
  `let mode = litebox::fs::Mode::from_bits_truncate(mode_bits & 0o777);`
* the same code in the Windows runner uses **`& 0o7777`**
  (`litebox_runner_linux_on_windows_userland/src/lib.rs:3064`)

so `S_ISUID`/`S_ISGID`/`S_ISVTX` are silently lost on this platform only. `Mode` does define
`SVTX` (`litebox/src/fs/mod.rs:280`), so only the mask is wrong. Consequence here: `/tmp` is not
sticky inside the guest (harmless for a single-user desktop, but a setuid-root binary unpacked
from a tar would lose its bit). Not yet fixed because the desktop does not need it to boot;
recorded as the first candidate litebox fix.
