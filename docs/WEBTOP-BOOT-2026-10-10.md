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

_(filled in below as each rung is measured)_
