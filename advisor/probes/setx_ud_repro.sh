#!/bin/sh
# dbustest2 + the two things the full script has that it lacked: set -x tracing
# and LD_LIBRARY_PATH. Isolates which one makes the dbus spawn take #UD.
set -x
export HOME=/root
export LD_LIBRARY_PATH=/usr/lib/weston:/usr/lib:/lib
export XDG_RUNTIME_DIR=/run/user/0
mkdir -p /root /run/user/0 /tmp/.X11-unix /var/lib/dbus
chmod 700 /run/user/0
chmod 1777 /tmp/.X11-unix
rm -f /run/seatd.sock
dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>/dev/null || true
export DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/xfce-bus
dbus-daemon --nofork --nopidfile --nosyslog --config-file=/usr/share/dbus-1/session.conf --address="$DBUS_SESSION_BUS_ADDRESS" &
i=0; while [ ! -e /tmp/xfce-bus ] && [ "$i" -lt 20 ]; do i=$((i+1)); sleep 0.5; done
if [ -e /tmp/xfce-bus ]; then echo DT3_BUS_UP; else echo DT3_BUS_DOWN; fi
echo DT3_DONE
