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
/xwire_probe "$SOCK"
echo "XW_PROBE_RC=$?"
i=0; while [ "$i" -lt 20 ]; do i=$((i+1)); sleep 0.5; done
echo XW_RAW_SETTLED

export DISPLAY=${SOCK##*/X}
export DISPLAY=":$DISPLAY"
export GDK_BACKEND=x11
echo "XW_GTK_DISPLAY=$DISPLAY"
thunar --daemon > /tmp/thunar.out 2>&1 &
i=0; while [ "$i" -lt 20 ]; do i=$((i+1)); sleep 0.5; done
thunar / > /tmp/thunar2.out 2>&1 &
echo XW_THUNAR_SPAWNED
i=0; while [ "$i" -lt 90 ]; do i=$((i+1)); sleep 0.5; done
echo XW_GTK_SETTLED
echo "=== BEGIN thunar.out ==="
head -20 /tmp/thunar.out /tmp/thunar2.out 2>/dev/null || echo "(none)"
echo "=== END thunar.out ==="
echo XW_DONE
