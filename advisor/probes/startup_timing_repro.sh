#!/bin/sh
# Minimal repro for the "client startup is extremely slow" investigation
# (AGENTS.md priority 2). Same launch sequence as run_xfce_xwm.sh (XWM fix +
# no set -x) but instruments wall-clock time around each stage with `date`
# in guest-visible epoch seconds, and stops right after ONE xfce4-about
# --version call so a single run answers "where does the time go" without
# needing the full desktop.
export HOME=/root
export LD_LIBRARY_PATH=/usr/lib/weston:/usr/lib:/lib
export XDG_RUNTIME_DIR=/run/user/0
mkdir -p /root /run/user/0 /tmp/.X11-unix /var/lib/dbus
chmod 700 /run/user/0
chmod 1777 /tmp/.X11-unix
rm -f /run/seatd.sock

t() { echo "TS $(date +%s.%N) $1"; }

t STAGE_DBUS_START
dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>/dev/null || true
export DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/xfce-bus
dbus-daemon --nofork --nopidfile --nosyslog --config-file=/usr/share/dbus-1/session.conf --address="$DBUS_SESSION_BUS_ADDRESS" &
i=0; while [ ! -e /tmp/xfce-bus ] && [ "$i" -lt 20 ]; do i=$((i+1)); sleep 0.2; done
t DBUS_UP

t STAGE_SEATD_START
seatd -l error &
i=0; while [ ! -e /run/seatd.sock ] && [ "$i" -lt 40 ]; do i=$((i+1)); sleep 0.2; done
t SEATD_UP

t STAGE_WESTON_START
weston --backend=drm-backend.so --socket=wayland-0 --use-pixman --shell=desktop-shell.so --logger-scopes=log > /tmp/weston.out 2>&1 &
i=0; while [ ! -e "$XDG_RUNTIME_DIR/wayland-0" ] && [ "$i" -lt 80 ]; do i=$((i+1)); sleep 0.2; done
t WESTON_SOCKET_UP

export WAYLAND_DISPLAY=wayland-0
DISP=""
i=0
while [ "$i" -lt 200 ]; do
  for n in 0 1 2; do [ -e "/tmp/.X11-unix/X$n" ] && DISP=":$n" && break; done
  [ -n "$DISP" ] && break
  i=$((i+1)); sleep 0.2
done
t "XWAYLAND_SOCKET_UP disp=$DISP"

unset WAYLAND_DISPLAY
export DISPLAY=$DISP
export GDK_BACKEND=x11
export XDG_SESSION_TYPE=x11
export GDK_GL=disable
export XLIB_SKIP_ARGB_VISUALS=1

t STAGE_XFCONFD_START
/usr/lib/xfce4/xfconf/xfconfd > /tmp/xfconfd.out 2>&1 &
i=0; while [ "$i" -lt 20 ]; do
  xfconf-query -c xfce4-session -l >/dev/null 2>&1 && break
  i=$((i+1)); sleep 0.2
done
t "XFCONFD_UP wait_iters=$i"

t CLIENT_START
xfce4-about --version > /tmp/xcheck.out 2>&1
rc=$?
t "CLIENT_DONE rc=$rc"
cat /tmp/xcheck.out

echo TEST_DONE
