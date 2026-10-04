# 2026-10-04j -- recompact of AGENTS.md: prose moved out of the index

Companion to `AGENTS.md` (which is a CURRENT-STATE index; this holds the same facts in prose). Blocks
below were moved out of `AGENTS.md` verbatim during the 2026-10-04j recompact because they are
recipes, not rules.

## XWD grab: decode with the MASKS, never by byte position

Header `struct.unpack(">24I", raw[:96])`: `f[11]` = bytes_per_line, `f[3]` depth, `f[4]`/`f[5]` w/h,
`f[7]` byte_order, **`f[14]`/`f[15]`/`f[16]` = r/g/b masks** (`f[13]` is `visual_class` -- using
13/14/15 rotates every colour, which is how apps1 reported xterm's red as `0,0,255`). Pixel words are
little-endian when `byte_order == 0`, masks `00ff0000/0000ff00/000000ff`, so **in memory the bytes
are B,G,R**. Pixels start at `max(len(raw) - w*h*bpp, hsize + ncolors*12)`. **Every `blue=0.00%` from
chrD19 and earlier is INVALID.** **A GTK app's 10x10 leader window cannot be grabbed** (`xwd -id` ->
`BadMatch` `X_GetImage` on it -- apps1's "mousepad failed" was that artifact); grab the named
top-level window, or `-root` as ground truth.

## selkies' capture REQUIRES MIT-SHM at the X server

`run_shm_capture` HARD-FAILS on `shm_query_version` (`pixelflux/src/x11/mod.rs:1097-1138`) -- no
XGetImage fallback. The client then hits `ERROR:ws:FATAL: Initial reconfiguration completed, but video
pipeline did not start` (`websockets_mode.py:3936`); **the real diagnosis is the line above it**,
`Failed to start capture for 'primary': <e>` (`:5685`). After a FATAL `video_active` stays set.

## Apps: verified to paint (apps1/apps3 on `47cefb2`, zero panics)

Measured by xwd with the mask-aware decoder: `xterm` red `255,0,0` 97.4%, `xedit` white 94.2%,
`xcalc` 2 colours 83/17, `xeyes` 155 colours, `xclock` 188 colours, **`mousepad` (GTK3) white 87.4% +
grey246 7.2%**, **`thunar` (GTK3) 1855 colours white 60.9%**, chromium `--app` (xwd `blue=99.79%`,
CDP `99.78%`).
Absent from the image (untestable, not broken): `gedit`, `evince`, `pcmanfm`, `firefox-esr`,
`galculator`, `leafpad`, `import`/`convert`. Present, untested: `xfce4-terminal`.
Probes `.wfgy/apps1.sh` (rotated decoder) / `apps3.sh` (correct): no `timeout(1)`, no xfce4-session,
MIT-SHM ON, `/tmp/.X11-unix` 1777 before Xvfb, census in-guest.

## The gm daemon can be down, and a dispatch then times out instead of failing

One shared `agentplug-runner spool` daemon per machine serves ALL registered projects, and it
self-recycles after 1h fully idle. If the MCP server process predates the supervisor/watchdog change
(`ee5855a`), nothing revives it: dispatches sit `queued_not_yet_claimed` until the timeout and
`mcp__gm__gm` returns `timed_out: true` with `daemon: alive=false`. Revive with one
`agentplug-runner spool` whose cwd is the project (a fresh daemon pid then serves both projects); a
dispatch that outlived its daemon is still retrievable with `resume_task`. Fixed in `c:\dev\gm`
(`9504d9b`, gm-mcp `8571615`): the poll loop re-asks for a runner on every wake, self-throttled, and
`runnerEnsureInFlight` stops ~50 contending runners stacking behind one cold start (~11s warm /
~100s cold).
