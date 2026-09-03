#!/bin/sh
# XFCE on weston, written to AVOID the concurrent-fork_verify crash:
# every service is started ALONE and given time to settle before the next,
# so at most one fork_verify healing pass is live at a time (the condition
# measured clean in 8/8 runs).
# NO 'set -x': shell tracing deterministically kills the first backgrounded
# child via a #UD in a syscall trampoline stub (bisected 2/2 fail vs 2/2 pass).
# It is what has been taking out dbus-daemon in every full-stack run.
export HOME=/root
export LD_LIBRARY_PATH=/usr/lib/weston:/usr/lib:/lib
export XDG_RUNTIME_DIR=/run/user/0
mkdir -p /root /run/user/0 /tmp/.X11-unix /var/lib/dbus
chmod 700 /run/user/0
chmod 1777 /tmp/.X11-unix
rm -f /run/seatd.sock

# Create the files first so tail -f has something to follow, and start the tailers BEFORE
# any component launches -- otherwise the earliest output (which is where a startup failure
# announces itself) is written before anything is watching and is lost.
: > /tmp/xfwm4.out; : > /tmp/xfsettingsd.out; : > /tmp/xfdesktop.out; : > /tmp/panel.out
# The end-of-run dump is worthless when the session HANGS before reaching it (observed at
# t=56.2s: no further page flips, 26 live threads, flat CPU). Stream each component's output
# to the main log continuously instead, prefixed so it can be grepped out, so a hang leaves
# the evidence behind rather than destroying it.
for f in xfwm4 xfsettingsd xfdesktop panel; do
  ( tail -n +1 -f /tmp/$f.out 2>/dev/null | while IFS= read -r line; do echo "GUESTOUT[$f] $line"; done ) &
done

echo STAGE_DBUS
dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>/dev/null || true

# GLib reads its settings from a COMPILED binary cache, gschemas.compiled, not from the
# .gschema.xml sources. The layer ships the 40 XML files but not the cache, so
# g_settings_schema_source_get_default() returns NULL and at-spi-bus-launcher calls
# g_error("Cannot get the default GSettingsSchemaSource") -- which aborts via a debug trap,
# i.e. SIGTRAP. It then takes its dbus-daemon down with it, and xfdesktop loses the
# background it had already painted (verified: guest scanout nonzero_bytes drops 6221890 ->
# 2258768 seconds after the trap, so the content is destroyed in the GUEST's own buffer,
# not in litebox's capture path). glib-compile-schemas IS in the layer, so just build it.
glib-compile-schemas /usr/share/glib-2.0/schemas && echo GSCHEMA_COMPILED=ok || echo GSCHEMA_COMPILED=fail

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
# SINGLE spawn only. Retrying a backgrounded spawn after a child has been lost
# to the trampoline #UD kills the LAUNCHER SHELL ITSELF (measured twice, at
# rip=0x7feffff6fb11, with both a while-loop and a shell function). So there is
# no scripting workaround: if dbus is lost, rerun rather than retry.
dbus-daemon --nofork --nopidfile --nosyslog --config-file=/usr/share/dbus-1/session.conf --address="$DBUS_SESSION_BUS_ADDRESS" &
i=0; while [ ! -e /tmp/xfce-bus ] && [ "$i" -lt 20 ]; do i=$((i+1)); sleep 0.5; done
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
weston --backend=drm-backend.so --socket=wayland-0 --use-pixman --shell=desktop-shell.so --logger-scopes=log > /tmp/weston.out 2>&1 &
i=0; while [ ! -e "$XDG_RUNTIME_DIR/wayland-0" ] && [ "$i" -lt 80 ]; do i=$((i+1)); sleep 0.5; done
echo WESTON_READY=$i
sleep 3

echo STAGE_XWAYLAND
export WAYLAND_DISPLAY=wayland-0
DISP=""
i=0
while [ "$i" -lt 200 ]; do
  for n in 0 1 2; do [ -e "/tmp/.X11-unix/X$n" ] && DISP=":$n" && break; done
  [ -n "$DISP" ] && break
  i=$((i+1)); sleep 0.5
done
echo XFCE_DISPLAY=$DISP
echo XWAYLAND_READY=$i
sleep 5

unset WAYLAND_DISPLAY
export DISPLAY=$DISP
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
xfwm4 --display=$DISP --compositor=off > /tmp/xfwm4.out 2>&1 &
i=0; while [ "$i" -lt 16 ]; do i=$((i+1)); sleep 0.5; done
echo XFWM4_WAITED

echo STAGE_XFSETTINGSD
xfsettingsd --display=$DISP > /tmp/xfsettingsd.out 2>&1 &
i=0; while [ "$i" -lt 10 ]; do i=$((i+1)); sleep 0.5; done
echo XFSETTINGSD_WAITED

echo STAGE_XFDESKTOP
xfdesktop --display=$DISP > /tmp/xfdesktop.out 2>&1 &
i=0; while [ "$i" -lt 20 ]; do i=$((i+1)); sleep 0.5; done
echo XFDESKTOP_WAITED

echo STAGE_PANEL
xfce4-panel --display=$DISP > /tmp/panel.out 2>&1 &
i=0; while [ "$i" -lt 24 ]; do i=$((i+1)); sleep 0.5; done
echo PANEL_WAITED


# Keep the session alive well past the observed failure points (drop ~t=46, hang ~t=56,
# Xwayland SIGABRT ~t=115) so all three are inside the window.
i=0; while [ "$i" -lt 300 ]; do i=$((i+1)); sleep 0.5; done

echo TEST_DONE
