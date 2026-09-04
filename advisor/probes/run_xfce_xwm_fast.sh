#!/bin/sh
# EXPERIMENTAL faster variant of run_xfce_xwm.sh -- same stage structure and
# same sequential-one-service-at-a-time discipline (still avoiding the
# documented concurrent-fork_verify race, see docs/AGENTS_ARCHIVE_2026-09-03.md
# passes 168-172: a genuine, still-unfixed host-code AV race under heavy
# concurrent fork_verify single-stepping, NOT something fixable by script
# timing alone -- so this script does NOT attempt true concurrency, only
# shrinks the MARGIN on top of each stage's already-real readiness poll).
# Two kinds of change from the original:
#   1. Trailing fixed `sleep N` AFTER a successful poll-for-readiness is
#      shortened (the poll already proved the resource exists; the sleep was
#      extra settle margin on top of that, not the primary wait).
#   2. The last four stages (xfwm4/xfsettingsd/xfdesktop/panel) have NO
#      readiness signal to poll at all -- just a fixed iteration count. Their
#      iteration counts are shortened directly since there's nothing sharper
#      to poll for.
# NOT changed: the fork/spawn discipline itself (still one service at a time,
# still single-spawn-no-retry per the original script's own hard-won lessons
# about retry loops killing the launcher shell), and the poll LOOP BOUNDS
# themselves (still generous upper limits -- shrinking a loop's sleep-per-tick
# doesn't reduce safety the way shrinking a fixed post-success sleep might).
# NO 'set -x' (same reason as the original: known to kill the first
# backgrounded child via a #UD in a syscall trampoline stub).
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
i=0; while [ ! -e /tmp/xfce-bus ] && [ "$i" -lt 20 ]; do i=$((i+1)); sleep 0.5; done
if [ -e /tmp/xfce-bus ]; then echo "DBUS_UP=yes"; else echo "DBUS_UP=no"; fi
echo DBUS_READY=$i
sleep 0.5

echo STAGE_SEATD
seatd -l error &
i=0; while [ ! -e /run/seatd.sock ] && [ "$i" -lt 40 ]; do i=$((i+1)); sleep 0.5; done
echo SEATD_READY=$i
sleep 0.5

echo STAGE_WESTON
weston --backend=drm-backend.so --socket=wayland-0 --use-pixman --shell=desktop-shell.so --logger-scopes=log > /tmp/weston.out 2>&1 &
i=0; while [ ! -e "$XDG_RUNTIME_DIR/wayland-0" ] && [ "$i" -lt 80 ]; do i=$((i+1)); sleep 0.5; done
echo WESTON_READY=$i
sleep 1

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
sleep 2

unset WAYLAND_DISPLAY
export DISPLAY=$DISP
export GDK_BACKEND=x11
export XDG_SESSION_TYPE=x11
export GDK_GL=disable
export XLIB_SKIP_ARGB_VISUALS=1

echo STAGE_XFCONFD
/usr/lib/xfce4/xfconf/xfconfd > /tmp/xfconfd.out 2>&1 &
sleep 2
xfconf-query -c xfce4-session -l > /tmp/xfconfq.out 2>&1
echo XFCONF_PROBE_RC=$?
head -5 /tmp/xfconfq.out 2>/dev/null
echo XFCONFD_STARTED

echo STAGE_XCHECK
xfce4-about --version > /tmp/xcheck.out 2>&1
echo XCHECK_RC=$?
head -4 /tmp/xcheck.out 2>/dev/null

echo STAGE_XFWM4
xfwm4 --display=$DISP --compositor=off > /tmp/xfwm4.out 2>&1 &
i=0; while [ "$i" -lt 8 ]; do i=$((i+1)); sleep 0.5; done
echo XFWM4_WAITED

echo STAGE_XFSETTINGSD
xfsettingsd --display=$DISP > /tmp/xfsettingsd.out 2>&1 &
i=0; while [ "$i" -lt 5 ]; do i=$((i+1)); sleep 0.5; done
echo XFSETTINGSD_WAITED

echo STAGE_XFDESKTOP
xfdesktop --display=$DISP > /tmp/xfdesktop.out 2>&1 &
i=0; while [ "$i" -lt 10 ]; do i=$((i+1)); sleep 0.5; done
echo XFDESKTOP_WAITED

echo STAGE_PANEL
xfce4-panel --display=$DISP > /tmp/panel.out 2>&1 &
i=0; while [ "$i" -lt 12 ]; do i=$((i+1)); sleep 0.5; done
echo PANEL_WAITED

for f in xfwm4 xfsettingsd xfdesktop panel; do
  echo "=== BEGIN $f.out ==="
  cat /tmp/$f.out 2>/dev/null || echo "(none)"
  echo "=== END $f.out ==="
done

echo TEST_DONE
