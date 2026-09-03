#!/bin/sh
# Diagnostic variant of run_xfce_gschema.sh: extends the settle window well past
# the observed Xwayland SIGABRT (t=115.7s) and hard-hang (t=56.2s) boundaries,
# and dumps weston.out (which carries Xwayland's own stderr, since weston spawns
# it internally via xwayland=true) at the end so its crash message is actually
# captured instead of being lost to a script that already exited.
export HOME=/root
export LD_LIBRARY_PATH=/usr/lib/weston:/usr/lib:/lib
export XDG_RUNTIME_DIR=/run/user/0
mkdir -p /root /run/user/0 /tmp/.X11-unix /var/lib/dbus
chmod 700 /run/user/0
chmod 1777 /tmp/.X11-unix
rm -f /run/seatd.sock

echo STAGE_DBUS
dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>/dev/null || true
glib-compile-schemas /usr/share/glib-2.0/schemas && echo GSCHEMA_COMPILED=ok || echo GSCHEMA_COMPILED=fail

# gdk-pixbuf's loaders.cache is missing entirely from this layer -- without it,
# gdk-pixbuf cannot resolve ANY format via the normal lookup path, including its
# own built-in PNG support. This is what aborts xfce4-panel/any GTK app the
# instant it needs to decode a fallback icon (image-missing.png). Same bug
# class and same fix pattern as the glib-compile-schemas fix above.
export GDK_PIXBUF_MODULEDIR=/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders
gdk-pixbuf-query-loaders > /usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache 2>/tmp/pixbufq.out \
  && echo PIXBUF_CACHE_BUILT=ok || echo PIXBUF_CACHE_BUILT=fail

export DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/xfce-bus
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
weston --backend=drm-backend.so --socket=wayland-0 --use-pixman --shell=desktop-shell.so --logger-scopes=log,xwm-wm-x11,xwayland > /tmp/weston.out 2>&1 &
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

echo STAGE_XFCONFD
/usr/lib/xfce4/xfconf/xfconfd > /tmp/xfconfd.out 2>&1 &
sleep 4
xfconf-query -c xfce4-session -l > /tmp/xfconfq.out 2>&1
echo XFCONF_PROBE_RC=$?
echo XFCONFD_STARTED

echo STAGE_XCHECK
xfce4-about --version > /tmp/xcheck.out 2>&1
echo XCHECK_RC=$?

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

# EXTENDED SETTLE: run_xfce_xwm.sh's own wait loops only cover ~113s total by
# the time PANEL_WAITED prints, but the observed Xwayland SIGABRT happens at
# t=115.7s and the observed hard-hang at t=56.2s -- both AFTER the script's own
# wait loops would have finished waiting for their respective stage. Extend
# well past both: 90 more seconds in 5s increments, checking Xwayland/weston
# liveness each time so we know exactly which check first shows it's gone.
echo STAGE_EXTENDED_SETTLE
i=0
while [ "$i" -lt 18 ]; do
  i=$((i+1))
  sleep 5
  XWAYLAND_ALIVE=no
  WESTON_ALIVE=no
  for p in /proc/[0-9]*; do
    [ -r "$p/comm" ] || continue
    c=$(cat "$p/comm" 2>/dev/null)
    [ "$c" = "Xwayland" ] && XWAYLAND_ALIVE=yes
    [ "$c" = "weston" ] && WESTON_ALIVE=yes
  done
  echo "SETTLE_CHECK_$i t=$((i*5))s XWAYLAND_ALIVE=$XWAYLAND_ALIVE WESTON_ALIVE=$WESTON_ALIVE"
done

for f in xfwm4 xfsettingsd xfdesktop panel weston xfconfd; do
  echo "=== BEGIN $f.out ==="
  cat /tmp/$f.out 2>/dev/null || echo "(none)"
  echo "=== END $f.out ==="
done

echo TEST_DONE
