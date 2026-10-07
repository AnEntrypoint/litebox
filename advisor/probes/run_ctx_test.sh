#!/bin/sh
# weston + Xwayland, then a RAW X client (no Xlib, no GTK) that creates a
# window, maps it, and fills it bright green. Isolates "does the X path work"
# from "does GTK work" -- no X client has ever drawn a pixel in this
# environment, and nothing in the layer could tell those two apart.
echo XW_START
export HOME=/root
export XDG_RUNTIME_DIR=/run/user/0
mkdir -p /root /run/user/0 /tmp/.X11-unix
chmod 700 /run/user/0
chmod 1777 /tmp/.X11-unix
rm -f /run/seatd.sock
# thunar (and any GTK/dbus client) needs a machine-id and a session bus, exactly
# as the XFCE launchers provide. Without them it fails with "Cannot spawn a
# message bus without a machine-id" and never draws -- which would make this A/B
# meaningless, since the XFCE components DO get these and thunar would not.
mkdir -p /var/lib/dbus
dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>/dev/null || true
export DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/ab-bus
dbus-daemon --nofork --nopidfile --nosyslog --config-file=/usr/share/dbus-1/session.conf --address="$DBUS_SESSION_BUS_ADDRESS" &
i=0; while [ ! -e /tmp/ab-bus ] && [ "$i" -lt 20 ]; do i=$((i+1)); sleep 0.5; done
[ -e /tmp/ab-bus ] && echo AB_DBUS_UP=yes || echo AB_DBUS_UP=no
seatd -l error &
i=0; while [ ! -e /run/seatd.sock ] && [ "$i" -lt 30 ]; do i=$((i+1)); sleep 0.5; done
weston --backend=drm-backend.so --socket=wayland-0 --use-pixman --shell=desktop-shell.so --logger-scopes=log > /tmp/weston.out 2>&1 &
i=0; while [ ! -e "$XDG_RUNTIME_DIR/wayland-0" ] && [ "$i" -lt 60 ]; do i=$((i+1)); sleep 0.5; done
echo XW_WESTON_READY
sleep 6
SOCK=""
i=0
while [ "$i" -lt 90 ]; do
  for n in 0 1 2; do [ -e "/tmp/.X11-unix/X$n" ] && SOCK="/tmp/.X11-unix/X$n" && break; done
  [ -n "$SOCK" ] && break
  i=$((i+1)); sleep 0.5
done
echo "XW_XSOCK=$SOCK"
[ -z "$SOCK" ] && { echo XW_NO_XSOCK; exit 1; }
sleep 4
# A/B on the SAME server, in the SAME run: a raw X client (known to work) then
# a plain GTK app that is NOT a panel or desktop component. If the raw window
# appears and the GTK one does not, the fault is in GTK/its stack rather than
# anything XFCE-specific -- which decides whether this is one bug or two.
# CONTEXT TEST. xfce4-about exits in 1.7s in a BARE guest, so the GTK stack's
# dlopen and TLS registration are fine on their own. Run the SAME binary inside
# the full display stack: if it now hangs, the display-stack context is the
# trigger and we have a ~2s reproduction instead of a 60s one.
echo CTX_START
DISPLAY=:0 GDK_BACKEND=x11 xfce4-about --version > /tmp/ctx1.out 2>&1
echo "CTX_ABOUT_VERSION_RC=$?"
DISPLAY=:0 GDK_BACKEND=x11 xfce4-about > /tmp/ctx2.out 2>&1 &
CTXPID=$!
echo CTX_ABOUT_SPAWNED
i=0; while [ "$i" -lt 60 ]; do i=$((i+1)); sleep 0.5; done
echo CTX_WAITED
echo "=== BEGIN ctx out ==="
cat /tmp/ctx1.out /tmp/ctx2.out 2>/dev/null | head -12
echo "=== END ctx out ==="
echo CTX_DONE
