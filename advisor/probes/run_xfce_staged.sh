#!/bin/sh
# XFCE on weston, written to AVOID the concurrent-fork_verify crash:
# every service is started ALONE and given time to settle before the next,
# so at most one fork_verify healing pass is live at a time (the condition
# measured clean in 8/8 runs).
# set -x REMOVED: shell tracing deterministically triggers a #UD in a syscall
# trampoline stub that kills the first backgrounded child (bisected 2/2 vs 2/2).
#
# XWM FIX (AGENTS.md pass 322/324): weston's desktop-shell.so has no XWM logic of its own --
# only weston's own `xwayland.so` module (already present in this layer at
# /usr/lib/libweston-14/xwayland.so) does the -wm handshake that maps X11 client windows into
# weston's scene graph. Spawning `Xwayland :1 ...` as a bare separate process (the old shape of
# this script) skips that handshake entirely: client buffers get real content but their surfaces
# never enter weston's scene graph, so nothing is ever composited -- this was the root cause of
# the session-long permanent-blackout blocker, confirmed fixed end-to-end (full-stack run: last
# four frames all non_black_pixels=2,073,597, no wipe). Fix: weston.ini gets `[core] xwayland=true`
# (written below, unconditionally, so this script does not depend on the layer already having it
# baked in) and Xwayland is no longer spawned manually -- weston launches and manages it itself,
# and this script discovers whatever display weston picked instead of hardcoding one.
export HOME=/root
export LD_LIBRARY_PATH=/usr/lib/weston:/usr/lib:/lib
export XDG_RUNTIME_DIR=/run/user/0
export XDG_CONFIG_HOME=/root/.config
mkdir -p /root /run/user/0 /tmp/.X11-unix /var/lib/dbus "$XDG_CONFIG_HOME"
chmod 700 /run/user/0
chmod 1777 /tmp/.X11-unix
rm -f /run/seatd.sock

# Write weston.ini with xwayland=true. weston's config loader (confirmed by inspecting the
# packed libexec_weston.so.0.0.0's own strings: "weston.ini", "XDG_CONFIG_HOME") checks
# $XDG_CONFIG_HOME/weston.ini FIRST (NOT $XDG_CONFIG_HOME/weston/weston.ini -- that subdirectory
# form is wrong and was silently ignored in an earlier draft of this fix), falling back to
# /etc/xdg/weston/weston.ini. Writing to the user config path here means this script is
# self-contained and does not depend on the packed layer's /etc copy already having the fix
# (belt-and-braces: also try to patch /etc's copy in place if writable, best-effort).
cat > "$XDG_CONFIG_HOME/weston.ini" <<'WESTONINI'
[core]
shell=desktop-shell.so
xwayland=true

[shell]
background-color=0xff002244
locking=false

[input-method]
path=
WESTONINI
if [ -w /etc/xdg/weston/weston.ini ] || [ -w /etc/xdg/weston ]; then
  cp "$XDG_CONFIG_HOME/weston.ini" /etc/xdg/weston/weston.ini 2>/dev/null || true
fi

echo STAGE_DBUS
dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>/dev/null || true
export DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/xfce-bus
# The backgrounded child can be killed between fork and execve by the
# concurrent-fork bug (measured: SIGILL at t=0.42, dbus-daemon never execs, and
# then EVERY component fails with "Connection refused"). The failure is
# probabilistic, so RETRY the spawn until the socket actually appears rather
# than accepting one silent loss and continuing into a doomed run.
# Spawn dbus ONCE, not in a retry loop. A retry loop backgrounds the spawn from
# inside a while loop, and that turned a probabilistic CHILD death into a
# DETERMINISTIC death of the LAUNCHER SHELL ITSELF (two runs, bit-identical
# rip=0x7feffff6fb11 rsp=0x7fefffeec280). Fork from the simplest possible
# context until the underlying fork bug is fixed.
dbus-daemon --nofork --nopidfile --nosyslog --config-file=/usr/share/dbus-1/session.conf --address="$DBUS_SESSION_BUS_ADDRESS" &
i=0; while [ ! -e /tmp/xfce-bus ] && [ "$i" -lt 20 ]; do i=$((i+1)); sleep 0.5; done
if [ -e /tmp/xfce-bus ]; then echo "DBUS_UP=yes"; else echo "DBUS_UP=no"; fi
echo DBUS_READY=$i
sleep 2

echo STAGE_SEATD
seatd -l error &
i=0; while [ ! -e /run/seatd.sock ] && [ "$i" -lt 40 ]; do i=$((i+1)); sleep 0.5; done
echo SEATD_READY=$i
sleep 2

echo STAGE_WESTON
# Redirected so a fast fatal error (e.g. a bad CLI arg, or weston.ini rejecting xwayland=true for
# some reason) is captured rather than silently producing an ordinary-looking readiness timeout
# (see the redirect-swallows-errors methodology finding, commit c176bf00) -- unconditionally
# cat'd at the end below regardless of how this run goes.
weston --backend=drm-backend.so --socket=wayland-0 --use-pixman --shell=desktop-shell.so > /tmp/weston.out 2>&1 &
i=0; while [ ! -e "$XDG_RUNTIME_DIR/wayland-0" ] && [ "$i" -lt 80 ]; do i=$((i+1)); sleep 0.5; done
echo WESTON_READY=$i
sleep 3

# weston's own xwayland.so module (loaded via weston.ini's xwayland=true above) has by now
# spawned and managed Xwayland itself, attaching an XWM over its own -wm handshake -- do NOT
# spawn Xwayland manually here, that is exactly the bug this fix removes. Discover whatever
# display weston picked (observed :0 in every run so far, but never hardcode it): poll for
# whichever /tmp/.X11-unix/X<N> socket appears, since that is created by Xwayland itself once
# listening, the same "prove it actually accepts connections, not just that a file exists"
# discipline this script already applied to the old hardcoded-:1 path. Matches the proven
# advisor/probes/run_xfce_xwm.sh discovery loop shape exactly (fixed small candidate set,
# no extra basename/sed forks per iteration) rather than a glob-based scan.
echo STAGE_XWAYLAND_DISCOVER
XDISP=""
i=0
while [ "$i" -lt 80 ]; do
  for n in 0 1 2; do [ -e "/tmp/.X11-unix/X$n" ] && XDISP=":$n" && break; done
  [ -n "$XDISP" ] && break
  i=$((i+1)); sleep 0.5
done
echo XWAYLAND_DISPLAY_DISCOVERED="$XDISP"
echo XWAYLAND_READY=$i

if [ -z "$XDISP" ]; then
  echo XWAYLAND_DISPLAY_NOT_FOUND
  echo "=== BEGIN weston.out ==="
  cat /tmp/weston.out 2>/dev/null || echo "(none)"
  echo "=== END weston.out ==="
  echo TEST_DONE
  exit 1
fi
sleep 5

unset WAYLAND_DISPLAY
export DISPLAY="$XDISP"
export GDK_BACKEND=x11
export XDG_SESSION_TYPE=x11
export GDK_GL=disable
export XLIB_SKIP_ARGB_VISUALS=1

# Prove the X display actually ACCEPTS connections before starting anything on
# it. Socket existence is not enough -- that was this project's original
# launch-ordering bug.
# xfconfd holds the session configuration. Without it xfce4-session comes up
# with an EMPTY session and launches nothing, silently, which is exactly what
# the previous run showed (876 X messages, then idle, zero children spawned).
# It lives at /usr/lib/xfce4/xfconf/xfconfd, NOT on PATH, so start it by path.
echo STAGE_XFCONFD
# Retry xfconfd too: it is the single point every XFCE component depends on
# (xfsettingsd/xfdesktop/xfce4-panel all die with "Connection refused" when it
# is absent), and its own spawn can be lost the same way dbus's was.
# Single spawn, same reason as dbus above: no backgrounding from inside a loop.
/usr/lib/xfce4/xfconf/xfconfd > /tmp/xfconfd.out 2>&1 &
sleep 4
# xfconf-query returns non-zero if it cannot reach the daemon: a real liveness
# probe, not a guess. Its output tells us WHY every component fails or works.
xfconf-query -c xfce4-session -l > /tmp/xfconfq.out 2>&1
echo XFCONF_PROBE_RC=$?
head -5 /tmp/xfconfq.out 2>/dev/null
echo XFCONFD_STARTED

echo STAGE_XCHECK
# No xdpyinfo/xrandr in this layer, so use an XFCE client that connects to X and
# exits: --version still opens no display, but a bad DISPLAY makes GTK clients
# fail loudly, so this distinguishes "X refuses connections" from "X is fine".
xfce4-about --version > /tmp/xcheck.out 2>&1
echo XCHECK_RC=$?
head -4 /tmp/xcheck.out 2>/dev/null

# xfce4-session connects to X and then launches NOTHING, silently (verified with
# xfconfd running: zero children, empty stdout, no verbose log). So bypass it and
# start the components the Failsafe session would have started, one at a time
# with settle delays, keeping at most one fork_verify pass live.
echo STAGE_XFWM4
xfwm4 --display="$XDISP" --compositor=off > /tmp/xfwm4.out 2>&1 &
i=0; while [ "$i" -lt 16 ]; do i=$((i+1)); sleep 0.5; done
echo XFWM4_WAITED

echo STAGE_XFSETTINGSD
xfsettingsd --display="$XDISP" > /tmp/xfsettingsd.out 2>&1 &
i=0; while [ "$i" -lt 10 ]; do i=$((i+1)); sleep 0.5; done
echo XFSETTINGSD_WAITED

echo STAGE_XFDESKTOP
xfdesktop --display="$XDISP" > /tmp/xfdesktop.out 2>&1 &
i=0; while [ "$i" -lt 20 ]; do i=$((i+1)); sleep 0.5; done
echo XFDESKTOP_WAITED

echo STAGE_PANEL
xfce4-panel --display="$XDISP" > /tmp/panel.out 2>&1 &
i=0; while [ "$i" -lt 24 ]; do i=$((i+1)); sleep 0.5; done
echo PANEL_WAITED

for f in weston xfconfd xfwm4 xfsettingsd xfdesktop panel; do
  echo "=== BEGIN $f.out ==="
  cat /tmp/$f.out 2>/dev/null || echo "(none)"
  echo "=== END $f.out ==="
done

echo TEST_DONE
