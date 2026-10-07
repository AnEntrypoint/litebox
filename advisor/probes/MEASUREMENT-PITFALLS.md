# Measurement pitfalls in this environment

Six traps that each produced a confident, wrong conclusion during the XFCE
investigation. All six are cheap to avoid once named.

## 1. Absence of a binary cannot be inferred from `ls`

To test whether a command exists in the guest, look for the **absence of a
"not found" error** in the run log. Several argv0 values differing only by
directory (`/bin/X`, `/usr/bin/X`, `/usr/local/bin/X`) are ordinary PATH
search, not duplicate or missing binaries. Reading that pattern as absence
produced a false "`timeout` is missing, every guard has been silently
bypassed" claim that would have triggered a pointless audit of prior results.

## 2. Frame timestamps must be checked against component start times

`LITEBOX_DUMP_FRAMES` captures on page flip, and weston only flips on damage,
so a component that has drawn and gone idle produces no frames at all. A frame
captured before a component started cannot show its output. Interpreting one
that predated `xfdesktop` by 18 seconds produced a confident "xfdesktop draws
nothing" verdict that had to be retracted.

## 3. A run that stops mid-log with zero faults was cut off

The Bash tool caps foreground commands near two minutes. Background these runs.
A missing stage marker usually means the script was still executing that line,
not that it failed.

## 4. `--help` does not open a display

A GTK program invoked with `--help` parses options and exits without touching
the X server, so a zero exit code from it says nothing about whether the screen
is usable. Clients have to be run for real.

## 5. Extract tars the way the runner resolves them

Tars built by appending overlays contain the same path more than once (e.g.
`./etc/foo` and `etc/foo`). Extraction takes the last member, so grepping the
first match can manufacture a false regression report. Better still, rebuild
tars from a single clean tree so each path appears once.

## 6. A pixel count cannot tell a desktop from a background fill

`non_black_pixels` scores a 1920x1080 flat fill identically to a fully
populated desktop. Use `decode_frame.py` to report what a frame contains, or
`diff_frames.py` to attribute content to a specific process by diffing two
frames across its startup.

## Known layer gaps that force inference

The guest layer ships no `xdpyinfo`, `xrandr`, `xwininfo`, `xprop` or
`xlsclients`, and its GTK is built without `G_ENABLE_DEBUG` so `GTK_DEBUG` is
ignored. There is currently no way to query the X server's state directly.
`GDK_SYNCHRONIZE=1` still works, since GDK's X error reporting is always
compiled in. Adding `xdpyinfo` and `xrandr` would remove a large amount of
guesswork.
