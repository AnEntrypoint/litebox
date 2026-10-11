# Getting multi-process chromium to paint inside litebox (2026-10-10)

The deliverable is a *painted* chromium window: multi-process, no `--no-sandbox`, started inside
the webtop guest. Rung 4 gets a **managed, correctly-sized, correctly-titled** window
(`0xe00003 "litebox chromium - Chromium" 1023x767+0+0` inside xfwm4's frame `0x201d5a`) whose
`xwd -id` grab is uniform — it is mapped and has never drawn. This file collects the measurements
that bound *where* the break is.

## 1. A native control paints: software compositing works in this container

```
/usr/lib/chromium/chromium --no-sandbox --headless=old --disable-gpu --disable-dev-shm-usage \
  --screenshot=/tmp/lbx/nat-shot.png --window-size=1024,768 file:///tmp/lbx/page.html
```
`rc=0`, PNG **10,015 B, 1024x768, distinct=248**, top `(255,212,0)` 706,009 px / `(0,102,255)`
67,600 px / `(0,0,0)` 10,748 px = the real page (yellow field, blue text).

So `vkCreateInstance: Found no drivers!` and `drmGetDevices2() has not found any devices` — both
logged by chromium inside litebox — are **not** fatal natively. "Chromium cannot composite without
a GPU in this container" is refuted. A blank window inside litebox is ours, or an ozone/X11
presentation gap; it is not the environment refusing to rasterize.

## 2. The measurement tool is trustworthy: `xwd -id` returns real content natively

Native `Xvfb :9 -screen 0 1024x768x24 -nolisten tcp -ac -retro -noreset` + `xclock -update 1
-geometry 200x200+40+40`, then:

| grab | bytes | decoded | distinct | top colours |
|---|---|---|---|---|
| `xwd -id 0x20000a` (xclock's own window) | 166,395 | 202x202 | **218** | `(255,255,255)` 38,210 / `(0,0,0)` 1,382 |
| `xwd -root` | 3,148,907 | 1024x768 | 225 | `(0,0,0)` 560,600 / `(255,255,255)` 224,607 |

A per-window grab of a window that has drawn is unambiguously non-uniform. **A black `xwd -id`
grab inside litebox is therefore a real finding, not an artifact of the tool or of the server.**
(Root grabs cannot make this distinction — that was the r4c mistake.)

## 3. Ruled out by inspection

- **SysV shared memory / XShm** — implemented: `shmget` has 22 hits in
  `litebox_shim_linux/src/syscalls/mm.rs`. XShm presentation is available to chromium.
- **A missing software GL stack in the guest** — the guest rootfs ships
  `/usr/lib/chromium/{libvk_swiftshader.so,vk_swiftshader_icd.json,libEGL.so,libGLESv2.so,libvulkan.so.1}`
  plus `icudtl.dat`, `resources.pak`, `chrome_{100,200}_percent.pak`, `locales`,
  `v8_context_blob.bin`. So `--enable-unsafe-swiftshader` is live in the guest, not inert.
- **The 2 ms wake-word park busy-looping** — read `park_on_unix_wake_words`
  (`epoll.rs:63`): it parks in shares of the *caller's own remaining deadline* and re-checks the
  word set between shares. It sleeps; it does not spin. `EpollFile::wait` (`epoll.rs:1292`) takes
  the bounded-repoll loop only when the set holds a Unix/Socket/Pty/stdin/evdev member, and
  returns `TimedOut` only once `cx.remaining_timeout()` is actually exhausted.

## 4. The live hypothesis: multi-process needs the GPU process; `--single-process` does not

The only chromium window that ever painted used **`--single-process`** (2400 distinct colours;
`docs/VISIBILITY-PROVEN-2026-10-10.md`). `--single-process` has **no GPU process and no
cross-process viz compositor** — the compositor runs in the browser process and swaps into the X
window in-process. Multi-process chromium must get its frames from the GPU process over Mojo, and
the GPU process must bring up a GL/Vulkan context. That is the one structural difference, and it is
also the axis on which every other multi-process failure in this project has lived (item 2,
Mojo/IPC fidelity).

Consequence: the cheapest falsification is **not** more X experiments but a flag that removes the
GPU process from the picture while staying multi-process:

- **`--disable-gpu-compositing`** — software compositing in the browser process, GPU process out of
  the frame path. The strongest single candidate.
- `--in-process-gpu` — GPU in the browser process (documented to hang rc=124 PRE-`3291ce69`;
  re-measure before trusting that row).
- `--use-gl=angle --use-angle=swiftshader --enable-unsafe-swiftshader` — force the software GL path
  explicitly instead of letting the GPU process fail into it.

Flags are ours to choose; only `--no-sandbox` is forbidden in the delivered configuration.

## 5. The probe that separates compositor from presentation (`/tmp/lbx/hl.py`)

Guest driver run as `/usr/bin/python3 -u -c "$(cat /tmp/lbx/hl.py)"` (no re-tar needed; it reuses
`rootfs-webtop12.tar`). Three sequential phases, each multi-process with **no** `--no-sandbox`, each
`--headless=old --screenshot=/tmp/home/shot-<tag>.png --virtual-time-budget=20000
--window-size=1024,768 file:///page.html`:

| phase | extra flag | what a painted PNG proves |
|---|---|---|
| a | — | renderer + compositor + Mojo all work inside litebox; the blank window is purely ozone/X11 presentation |
| b | `--enable-unsafe-swiftshader` | same, if the GPU process needed SwiftShader to come up |
| c | `--no-sandbox` (control) | if only (c) paints, the gap is in the sandbox path, not the compositor |

The guest has no PIL, so the driver prints each PNG as base64 on stdout and the host histograms it.
Launcher `/tmp/lbx/wtrun-hl.sh` (a copy of `wtrun.sh` with the program line swapped and
`TAR` defaulting to `rootfs-webtop12.tar`); its gate is `wtrun.sh`'s, unmodified.

## 6. Runs so far

- **hl1** (3 headless phases: plain, `+ --enable-unsafe-swiftshader`, `+ --no-sandbox`) — **no PNG in
  160.9 s in ANY phase**, rc=None. Native control for the same command line: **1 s, rc=0, 11,602 B
  PNG**. So the sandbox is excluded (phase c fails too) and software compositing is not the problem
  natively.
- **hl2** — **REFUTES the `/dev/shm` hypothesis AND invalidates hl1's reading.** Two phases, both
  `--headless=old --dump-dom --screenshot`, differing only by `--disable-dev-shm-usage`:
  `shm` (default) and `tmp` (`--disable-dev-shm-usage`). **Both identical: `elapsed 311.2 rc=None
  dom_at=None, dom bytes=0, png FAILED`.** `/dev/shm exists=True writable=True` in the guest.
  **Old headless does not merely fail to composite inside litebox — it never loads the page at all
  (dom=0), with or without dev-shm.** So hl1's "no PNG" was never a compositor measurement; the
  break with `--headless=old` is upstream of first paint entirely. Any paint measurement must use
  `--headless=new` (the only headless mode that has ever completed a load here: DOM at 19.67 s).
  Second consequence: the native control used `--disable-dev-shm-usage`, so "litebox's per-process
  `/tmp` breaks cross-process shm" cannot be what separates them either.
- **r5a** (rung 5: `xfwm4` + chromium, no session) — NULL RESULT: `out-r5a.txt` holds exactly one
  frame (`md5=cc81d3e05725`, distinct=2 = the bare root weave, captured at t≈25 s before chromium
  could map) and its stdout stops 165 B after `[boot] spawn /usr/lib/chromium/chromium ...` — no
  `poll_dead`/`winlist`/`proclist`/`shot` ever printed. Says nothing about painting.
- **r4g** (rung 4 + `--enable-unsafe-swiftshader`, `-noreset`, tar12, `SECS=1500`) — stalled the
  same way: 2 frames (2 × 3,148,907 B) then stdout stops right after chromium's spawn; 14 runner
  processes and 1-min load ~37-44 while it ran. SwiftShader rasterizing 1024x768 in software on 4
  `nice -n 19` CPUs starves the guest driver, so a stalled run here is not evidence of a hang in
  litebox — it is the starvation the orphan lesson warns about.

## 7. Harness loss and the rebuilt probe (2026-10-11)

`/tmp/lbx` (the runner copy, `wtrun*.sh`, `boot.py`, every rootfs tarball, and `hl*.py`) was **gone**
after the container was recreated at 10:52 on 2026-10-11 — `/tmp` now holds another session's
selkies/gmweb files. The webtop rootfs seed (`rootfs-x11.tar`) does not survive anywhere on disk, so
rung 4 cannot be re-run until it is rebuilt from the recipe in `docs/WEBTOP-BOOT-2026-10-10.md` §1.

Rebuilt for the headless questions, which need no X:
- `scratchpad/lbx/mkmin.sh` builds `minroot.tar` (≈600 MB): host chromium 154.0.8037.92 + its ldd
  closure, dash/coreutils/base64, `/usr/lib/chromium` resources, fonts, `/opt/lib` (the host's
  `libudev` is a FAKE stub that must be shipped as `libudev.so.1` → `libudev.so.1.0.0-fake`), and
  `page.html`. Owners 0:0 with `--numeric-owner`; `/tmp`, `/tmp/home`, `/tmp/xdg` are 1777.
- `scratchpad/lbx/probe.sh` runs ONE headless mode inside litebox and reports the DOM and PNG as they
  appear (chromium does not exit after `--dump-dom`, so it is killed at the first PNG or 240 s).
- `scratchpad/lbx/run.sh <tag> <secs> <mode> [flags]` is the gated launcher (one runner at a time,
  lock `/tmp/lbx/.runner-lock`, `pgrep -x lbxrun`, load < 4, MemAvailable > 2 GB).

Smoke test: `chromium --version` inside litebox → `Chromium 154.0.8037.92`, rc=0.
