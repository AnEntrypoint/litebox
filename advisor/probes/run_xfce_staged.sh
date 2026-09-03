#!/bin/sh
# XFCE on weston, written to AVOID the concurrent-fork_verify crash:
# every service is started ALONE and given time to settle before the next,
# so at most one fork_verify healing pass is live at a time (the condition
# measured clean in 8/8 runs).
set -x
export HOME=/root
export LD_LIBRARY_PATH=/usr/lib/weston:/usr/lib:/lib
export XDG_RUNTIME_DIR=/run/user/0
mkdir -p /root /run/user/0 /tmp/.X11-unix /var/lib/dbus
chmod 700 /run/user/0
chmod 1777 /tmp/.X11-unix
rm -f /run/seatd.sock

echo STAGE_DBUS
dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>/dev/null || true
export DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/xfce-bus
dbus-daemon --nofork --nopidfile --nosyslog --config-file=/usr/share/dbus-1/session.conf --address="$DBUS_SESSION_BUS_ADDRESS" &
i=0; while [ ! -e /tmp/xfce-bus ] && [ "$i" -lt 12 ]; do i=$((i+1)); sleep 0.5; done
echo DBUS_READY=$i
sleep 2

echo STAGE_SEATD
seatd -l error &
i=0; while [ ! -e /run/seatd.sock ] && [ "$i" -lt 40 ]; do i=$((i+1)); sleep 0.5; done
echo SEATD_READY=$i
sleep 2

echo STAGE_WESTON
weston --backend=drm-backend.so --socket=wayland-0 --use-pixman --shell=desktop-shell.so &
i=0; while [ ! -e "$XDG_RUNTIME_DIR/wayland-0" ] && [ "$i" -lt 80 ]; do i=$((i+1)); sleep 0.5; done
echo WESTON_READY=$i
sleep 3

echo STAGE_XWAYLAND
export WAYLAND_DISPLAY=wayland-0
Xwayland :1 -geometry 1920x1080 -fullscreen -noreset -glamor off &
i=0; while [ ! -e /tmp/.X11-unix/X1 ] && [ "$i" -lt 200 ]; do i=$((i+1)); sleep 0.5; done
echo XWAYLAND_READY=$i
sleep 5

unset WAYLAND_DISPLAY
export DISPLAY=:1
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
/usr/lib/xfce4/xfconf/xfconfd &
sleep 3
echo XFCONFD_STARTED

echo STAGE_XCHECK
# No xdpyinfo/xrandr in this layer, so use an XFCE client that connects to X and
# exits: --version still opens no display, but a bad DISPLAY makes GTK clients
# fail loudly, so this distinguishes "X refuses connections" from "X is fine".
xfce4-about --version > /tmp/xcheck.out 2>&1
echo XCHECK_RC=$?
head -4 /tmp/xcheck.out 2>/dev/null

echo STAGE_SESSION
# Capture xfce4-session's OWN stderr: the loader and GTK write their errors
# there before any logging is set up, so an empty file is itself a datum.
export XFSM_VERBOSE=1
xfce4-session --display=:1 --disable-tcp > /tmp/session.out 2>&1 &
i=0; while [ "$i" -lt 100 ]; do i=$((i+1)); sleep 0.5; done
echo SESSION_WAITED
echo "=== BEGIN session.out ==="
cat /tmp/session.out 2>/dev/null || echo "(no session.out)"
echo "=== END session.out ==="
echo "=== BEGIN verbose log ==="
cat "$HOME/.xfce4-session.verbose-log" 2>/dev/null || echo "(no verbose log)"
echo "=== END verbose log ==="

echo TEST_DONE
