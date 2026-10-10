# How a litebox guest can get pixels onto Xvfb `:1` — ranked options + plan

Research only (2026-10-10). Nothing run, nothing committed. All source citations are
`file:line` on `main`. Cheap read-only probes only (`tar -tf`, `ls`, `xdpyinfo`, `ps`, file reads).

## 0. Environment facts, verified today (not assumed)

| fact | evidence |
|---|---|
| `DISPLAY=:1.0`, real X server at `/tmp/.X11-unix/X1` (`srwxrwxrwx abc`) | `ls -la /tmp/.X11-unix` |
| `xrandr -d :1` → `screen connected primary 1504x892` | live |
| Xvfb advertises **GLX** and **MIT-SHM**; 23 extensions | `xdpyinfo`/`xrandr` |
| host has mesa **lavapipe** Vulkan (`libvulkan_lvp.so`, `/usr/share/vulkan/icd.d/lvp_icd.json`) + full `dri/` | `ls /usr/lib/x86_64-linux-gnu` |
| `libxkbcommon-x11.so.0` present (what winit/X11 needs) | `ls` |
| **no `/dev/dri` on the host** — irrelevant, litebox *emulates* DRM in the shim | `ls /dev/dri` → ENOENT |
| no socat / x11vnc / feh / ristretto / ImageMagick / tkinter; **PIL 12.3.0 available**; `xdotool`, `xwd`, `xfconf-query`, `xfdesktop` present | `command -v` |
| XFCE is live: `xfwm4`, `xfdesktop`, `xfce4-panel`, `xfce4-session` | `ps` |

Chromium rootfs (`/tmp/lbx/rootfs-chr.tar`, listing cached at `/tmp/lbx/chr-tar-list.txt`, 2604 entries):
- X **client** libs only: `libX11.so.6`, `libxcb.so.1`, `libXext`, `libXfixes`, `libXdamage`,
  `libXcomposite`, `libXrandr`, `libxkbcommon.so.0`, `libxcb-shm`, `libX11-xcb`, `libgbm`, `libdrm`
  (+ chromium's own ANGLE `usr/lib/chromium/libEGL.so`, `libGLESv2.so`).
- **No X server binary.** No `Xorg`, `Xvfb`, `Xwayland`, `weston`, `x11vnc` anywhere in the tar.
- `usr/bin/` contains exactly one program: **`python3`** (the driver trick already used per
  `docs/LINUX-CHROMIUM.md`: "run `/usr/bin/python3 -u -c <script>` as the program").
- Chromium binary contains `ozone_platform_x11` **and** `ozone_platform_wayland`; **no**
  `ozone_platform_drm`, no `ozone_platform_headless` symbol (binary is stripped; `--headless=old`
  works natively so a headless path exists, but it is not the "drm" one).

## 1. Every plausible path, assessed

### (a) Share `/tmp/.X11-unix/X1` into the guest → **DOES NOT WORK TODAY. Full stop.**

Three separate mechanisms were asked about; none of them can bind a *host socket* into the guest.

- **`SPILLED_PREFIXES`** — `litebox_shim_linux/src/syscalls/file_spill.rs:13` is a hard-coded list of
  7 *chromium config/cache* prefixes (`/tmp/.config/chromium`, `/tmp/org.chromium.`, …). It is file
  **content** sharing between host processes, not a path bind.
- **`LITEBOX_SHARED_WRITE_PREFIXES`** — `file_spill.rs:306-321` adds host-named prefixes on top, read
  from the **host** env via `platform.env_value`. But `file_spill.rs:324-326`
  (`spill_enabled_for`) ANDs it with `platform.spill_available()`, whose default is `false`
  (`litebox/src/platform/mod.rs:1961-1963`) and which is overridden **only** in
  `litebox_platform_windows_userland/src/lib.rs:15421`. **There is no Linux override → spill is
  completely inert on this container.** (Consistent with `docs/LINUX-CHROMIUM.md`'s "guest files are
  not host-visible" and with the reported `LITEBOX_SHARED_WRITE_PREFIXES` segfault.)
- **`--initial-files`** (`litebox_runner_linux_userland/src/lib.rs:60-62`, used at :402/:435) mmaps a
  tar and pre-fills the **in-memory** guest FS. It can create a *regular file* named
  `/tmp/.X11-unix/X1`, never a socket.
- **`--insert-file`** (`lib.rs:56-58`) is `unimplemented!()` at `lib.rs:372-376`.

**And even if the path existed, `connect()` would never reach the host.** Crux, asked for
explicitly:

- `sys_connect` → `UnixStream::connect` (`unix.rs:1955`). First the local table: `self.lookup(task,
  &addr)` (`unix.rs:1983`) — a lookup in `global.unix_addr_table`, an **in-memory** map keyed by the
  path *bytes*.
- On miss it tries `connect_cross_process` (`unix.rs:2052`): posts a request into
  `global.unix_shared_connect_queue` and waits for another **litebox process's** `accept()` to claim
  it. Presence is checked in `global.unix_addr_presence` (`unix.rs:2066`), registered by
  `listen()` at `unix.rs:283-289`. Still 100 % litebox-internal.
- If neither shows a listener: `refused_or_missing` (`unix.rs:2030-2041`) → `ENOENT`/`ECONNREFUSED`.
- `net.rs:2326` is the only other ancillary/connect-ish site and it rejects ancillary data on
  **non**-Unix sockets.

**Nowhere in the tree does a guest AF_UNIX socket become a host fd.** Verdict: option (a) in its
"share the socket path" form is dead today; it would require a new platform primitive (host
`connect()` on a unix path) + a new shim file kind wrapping a host fd + seccomp allow-list entries
(the `memfd_create`/`ftruncate` precedent in `docs/linux-native-drm-gui-probe/README.md` shows that
shape of change), **plus** solving MIT-SHM: XShm hands the client a host shm fd over SCM_RIGHTS, and
a guest cannot map a host fd. Not worth it while a no-litebox-change alternative exists (option F).

### (a′) X11 over TCP to the host — **DEAD.**
`phy.rs:166-183`: a non-local packet goes to `platform.send_ip_packet`, implemented at
`litebox_platform_linux_userland/src/lib.rs:2549-2573` — `unimplemented!("networking without tun is
unimplemented")` unless `--tun-device-name` was given, and `--tun-device-name` opens
`/dev/net/tun` (`lib.rs:258-302`) which **does not exist in this container** (`docs/LINUX-TEST-SUITE.md`:
"no `/dev/net/tun`"). Guest↔host TCP is impossible in both directions.

### (b) `--gui` presenter + `/dev/dri/card0` — the *display half* is real; the *chromium half* is not.

- `--gui` (`litebox_runner_linux_userland/src/lib.rs:102-108`) opens a **real host X11/Wayland
  window** via winit/wgpu titled `litebox virtual display`, showing whatever the guest drew into
  litebox's emulated DRM dumb buffer.
- Run-verified on Linux/X11: `docs/linux-presenter-run-probe/README.md` (real `xwininfo -root -tree`
  window, twice) and `docs/linux-native-drm-gui-probe/README.md` (guest `drmgui.c` →
  `CREATE_DUMB`/`ADDFB2`/`SETCRTC`/`PAGE_FLIP` → window pixels screenshot-confirmed byte-exact,
  solid green and solid blue). Caveat: both were run on the **Windows/WSL2 host with WSLg**, not in
  this container. Here the prerequisites look better, not worse (real Xvfb, GLX, lavapipe,
  `libxkbcommon-x11`).
- **But chromium cannot feed it**: the chromium binary has **no `ozone_platform_drm`**. Its only
  real-window backends are x11 and wayland. So (b) is usable only as the *last mile* for something
  else that renders into DRM (i.e. options C/E below), never for chromium directly.

### (c) An X server *inside* the guest — **not viable.**
- No `Xorg`/`Xvfb`/`Xwayland` in the rootfs (tar listing above); adding one means the whole
  `ldd`+`objdump`+`litebox_syscall_rewriter` rootfs build of
  `docs/x11-server-probe/README.md`.
- That probe is exactly the cautionary tale: it took **six rounds** and never reached a serving
  display. Round 5 proved Xorg's `modesetting` driver *can* talk to litebox's `/dev/dri/card0`
  (`(II) modeset(0): using /dev/dri/card0`, `virtual-1920x1080`), which is great news for
  litebox's DRM, but round 5 also died at `Initializing extension DRI2` (SIGSEGV at `0x8`,
  root-caused in round 6 as Xorg's own non-defensive behaviour with no glamor), and
  `_XSERVTransmkdir: euid != 0` blocks `/tmp/.X11-unix` creation because the runner hardcodes
  guest uid 1000 (`litebox_runner_linux_userland` `DEFAULT_GUEST_UID`).
- Effort: weeks. Probability: low.

### (d) headless screenshot / CDP → PNG out → show it on `:1` — **works, and partly already proven.**

Egress (the thing AGENTS.md calls impossible) is real in two forms:
1. **guest stdout/stderr → host**, via `platform::StdioProvider::write_to`
   (`litebox/src/fs/devices.rs:670-703` maps `Device::Stdout/Stderr` onto host streams). This is how
   every `log-*.txt` exists. There is a matching read path (`devices.rs:642-643`).
2. **`--export-writable-layer <tar>`** (`runner lib.rs:92-96`) — guest's whole writable layer to a
   host tar, but **only on a clean exit** (`mxt.sh` sends SIGTERM to make it flush).

And CDP already half-works under litebox: `/tmp/lbx/cdp.py` (run as the guest `python3`, driving
chromium over `--remote-debugging-pipe` on fds 3/4) printed
`RECV fallback ... {"method": "Browser.getVersion", "id": 1}` / `HANDSHAKE_OK`, then
`TIMEOUT Target.getTargets` — i.e. the browser process answers CDP; only target enumeration
(= renderers) stalls, which is the known multi-process hang. `--single-process` loads the page
(`docs/LINUX-CHROMIUM.md`), so `--single-process` + CDP is the combination to try.

Host side: PIL 12.3.0 is available; there is no image viewer and no tkinter, so the display step is
either `xfconf-query` (set the XFCE backdrop — guaranteed visible through selkies) or the **host's
own** `/usr/bin/google-chrome` showing a `file://` page with a 2 s `<meta refresh>` on the PNG
(a real, live-updating window owned by the host, not by the guest).

### (e) In-guest Wayland compositor → litebox DRM → `--gui` window — **proven pieces, huge integration.**

`docs/wayland-drm-backend-probe/` is a *working* smithay compositor on litebox's DRM: phase 10 is
live-verified end-to-end (`CONNECTED` → `CLIENT_ACCEPTED` → `COMMIT_SHM_OK first4=[11,22,33,44]` →
`DUMB_COPY_OK` → `SETCRTC_OK`), and `src/desktop.rs` adds `xdg_wm_base`+`wl_seat`+`wl_output`
(live-verified `LISTENING path=/tmp/litebox-wayland-0` → `RUNNING`). The phase-4 blocker
(`sendmsg` `SCM_RIGHTS` → `EINVAL`) is **no longer true**: `unix.rs:651` now carries
"A file descriptor donated via `SCM_RIGHTS` ancillary data", and `docs/LINUX-CHROMIUM.md` lists
cross-process memfd visibility "incl. over SCM_RIGHTS" as proven (probe35).

Chromium *does* ship `ozone_platform_wayland`. But chromium's Wayland client is far more demanding
than the probe's globals: keyboard seat (the probe deliberately omits `seat.add_keyboard()` because
xkbcommon's C keymap code tripped the rewriter), `wl_data_device_manager`, `wp_viewporter`,
`zxdg_output_manager_v1`, `zwp_text_input_manager_v3`, and EGL/ANGLE on Wayland. Plus the
compositor must be co-resident with chromium in one litebox session, and chromium multi-process is
still broken. Effort: very high. Probability: low-medium.

### (f) **X11 protocol tunnelled over the guest's stdin/stdout** — no litebox changes at all. ⭐

The guest program is `python3`. Python can:
1. `socket.socket(AF_UNIX)` + `bind("/tmp/.X11-unix/X1")` + `listen()` **inside the guest** — that
   is a first-class litebox feature (the wayland probe literally did `LISTENING
   path=/tmp/litebox-wayland-0`),
2. `os.fork()` chromium as a **child** (litebox fork is fixed — `7d5c9be7`, `dd94fef9`),
3. relay X11 bytes between chromium and the host over **stdout (guest→host) and stdin
   (host→guest)**, which are already host-visible pipes (`devices.rs:642-703`),
4. strip `MIT-SHM` from the connection-setup reply so chromium's xcb falls back to plain
   `PutImage` — necessary because a guest memfd can never be shared with the host X server.

Host side: a ~120-line Python pump that runs the runner with pipes, connects to the *real*
`/tmp/.X11-unix/X1`, and shuttles framed bytes. Result: **chromium gets a genuine X11 connection,
so its window is a real xfwm4-managed window on `:1`, streamed by selkies** — with zero shim,
platform or runner changes.

Residual risks: X11 over a byte pipe is chatty (flow control needed); chromium must not use
DRI3/dma-buf (`--disable-gpu`); the multi-process hang still has to be dodged (`--single-process`
works for page load); guest stdout carries both tunnel bytes and logs → use stdout for the framed
tunnel only, keep chromium's `--enable-logging=stderr`.

## 2. Ranking — probability × effort (score = P × 1/E, higher is better)

| # | option | P(works) | effort | score | gives |
|---|---|---|---|---|---|
| **1** | **(f) X11 over stdio tunnel** | 0.55 | M (2 files of Python, 0 Rust) | **best** | **a real live chromium window on `:1`** |
| 2 | **(d) CDP/headless PNG → host window** | 0.70 | S–M | 2nd | chromium's rendering visible (image, refreshable) |
| 3 | **(a) host AF_UNIX passthrough (new shim/platform code)** | 0.45 | L (platform primitive + new file kind + seccomp + SHM) | 3rd | real live chromium window; permanent fix |
| 4 | **(e) in-guest wayland compositor → DRM → `--gui`** | 0.25 | XL | 4th | litebox-titled window showing chromium |
| 5 | **(c) in-guest Xorg/Xvfb** | 0.10 | XL (6 probe rounds and still not serving) | last | guest X server (then still needs b or f) |
| — | (a′) TCP X11 · (b) chromium→DRM | 0.0 | — | — | refuted above (no tun / no drm ozone) |

**Winner: (f), with (d) run first as the cheap proof that pixels exist.** They share the same
harness (guest `python3` driver + host pump), so (d) de-risks (f) rather than competing with it.

## 3. Plan for the winner

### Stage 0 — harness (no chromium)
`x11pump.py` (host) + `x11proxy.py` (guest, injected via `--initial-files` into
`rootfs-chr.tar` at `/tmp/x11proxy.py`).
*Guest proxy*: create `/tmp/.X11-unix`, `bind("/tmp/.X11-unix/X1")`, `listen(8)`; for each accepted
conn, spawn a pump thread: read → `sys.stdout.buffer.write(frame)` → flush; stdin reader demuxes
frames back to the right conn. Frame = `b"X11C" + u32 conn_id + u32 len + payload`.
Strip MIT-SHM: on the first reply from the server (the connection-setup reply), parse
`num-screens`/`vendors` and drop the `MIT-SHM` entry from the extension list; answer any later
`MIT-SHM` `QueryExtension` with `present=0`.
*Host pump*: `connect("/tmp/.X11-unix/X1")`; `subprocess.Popen([RUNNER, ...], stdin=PIPE,
stdout=PIPE)`; thread A: server socket → runner stdin; thread B: runner stdout → parse frames →
right server socket (one socket per conn_id).
**Prove Stage 0 without chromium**: guest program = `python3 /tmp/x11proxy.py --client
/usr/bin/python3` where the "client" is a 20-line raw-xcb test that opens a connection, creates a
window and fills it. A coloured window appearing on `:1` (check with `xwininfo -root -tree | grep
-i litebox` and `xwd`) proves the tunnel end-to-end before touching chromium.

### Stage 1 — proof that chromium produces pixels (option d, ~1 hour, insurance)
```
# guest driver: python3 that forks chromium with --remote-debugging-pipe, walks the CDP
# handshake (already proven: HANDSHAKE_OK), Page.navigate, Page.captureScreenshot,
# prints base64 to stdout.
RUNNER=/config/workspace/litebox/target/release/litebox_runner_linux_userland
timeout -s TERM -k 10 90 $RUNNER --unstable --env HOME=/tmp --env DISPLAY=:1 \
  --program-from-tar --initial-files /tmp/lbx/rootfs-chr.tar \
  /usr/bin/python3 -u /tmp/drive_cdp.py -- \
    /usr/lib/chromium/chromium --headless=new --single-process --disable-gpu \
    --no-first-run --disable-dev-shm-usage --disable-breakpad --disable-crash-reporter \
    --user-data-dir=/tmp/udd-cdp --window-size=800,600 --hide-scrollbars \
    file:///page.html
```
Host decodes with PIL and (i) writes `/tmp/lbx/shot.png`, (ii) sets the desktop backdrop
(`xfconf-query -c xfce4-desktop -p /backdrop/screen0/monitor0/image-path -s /tmp/lbx/shot.png`)
or (iii) opens `file:///tmp/lbx/view.html` (2 s meta-refresh `<img>`) in the **host's**
`/usr/bin/google-chrome --app=... --window-size=800,600`. Visible through selkies immediately.
Also try the cheaper `--screenshot=/tmp/shot.png` variant + `--export-writable-layer`
(`mxt.sh` already SIGTERMs to make it flush) as a cross-check.

### Stage 2 — the real window (option f)
```
timeout -s TERM -k 10 120 $RUNNER --unstable --env HOME=/tmp --env DISPLAY=:1 \
  --program-from-tar --initial-files /tmp/lbx/rootfs-chr.tar \
  /usr/bin/python3 -u /tmp/x11proxy.py -- \
    /usr/lib/chromium/chromium --ozone-platform=x11 --disable-gpu --no-first-run \
    --disable-dev-shm-usage --disable-breakpad --disable-crash-reporter \
    --no-default-browser-check --user-data-dir=/tmp/udd-x11 \
    --window-size=1024,768 --enable-logging=stderr file:///page.html
```
with `x11pump.py` on the host side owning the runner's stdin/stdout. Then
`xwininfo -root -tree | grep -i chromium` and `xwd -id <id>` to confirm a real, mapped window.
Start with `--single-process` (the only configuration that loads a page today); retry
multi-process once the cross-process hang is fixed, since that is the configuration the goal
actually demands (`--no-sandbox` never passed).

### Stage 3 — if (f) stalls on X11 protocol details
Fall back to (a): add `Platform::connect_unix_socket(host_path) -> Option<OwnedFd>` in
`litebox_platform_linux_userland` (seccomp allow-list entries for `socket`/`connect`, per the
`memfd_create` precedent), a new shim file kind wrapping that host fd, and gate it behind a
`LITEBOX_HOST_UNIX_PATHS=/tmp/.X11-unix/` env flag read via `platform.env_value` — keeping the
same host-side MIT-SHM-stripping bridge from Stage 0 in front of it.

## 4. Guardrails for whoever runs this
- 4 CPUs / 7.8 GB, and the machine froze three times: **one** runner at a time, `timeout -s TERM
  -k 10`, sweep leftovers first (`pkill -KILL -f 'litebox_run''ner_lin'`), never pipe a build
  through `tail` while a runner holds the exe.
- `DISPLAY=:1` must reach the runner's host process (it already is `:1.0`), but `LITEBOX_*` flags
  come from the **host** env only — `--env DISPLAY=:1` is what the *guest* sees.
- `--program-from-tar` needs an absolute guest path; no `PATH` resolution (`runner lib.rs:380-386`).
- Never pass `--no-sandbox` in the delivered configuration (goal statement), and never modify
  chromium or its rootfs.
