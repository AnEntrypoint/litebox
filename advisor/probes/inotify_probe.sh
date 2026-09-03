#!/bin/sh
# Does the missing inotify actually DEGRADE dbus, or does dbus fall back cleanly?
# "Cannot initialize inotify" is printed at startup regardless, so the message alone
# proves nothing. The functional test is service ACTIVATION: dbus reads its service
# directories to find activatable names. If it read them once at startup and only uses
# inotify to notice LATER changes, activation still works and this is cosmetic. If the
# read itself is inotify-driven, activation fails and this is load-bearing.
echo INO_START
export HOME=/root
export XDG_RUNTIME_DIR=/run/user/0
mkdir -p /root /run/user/0 /var/lib/dbus
dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>/dev/null || true
export DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/ino-bus
dbus-daemon --nofork --nopidfile --nosyslog --config-file=/usr/share/dbus-1/session.conf --address="$DBUS_SESSION_BUS_ADDRESS" > /tmp/ino-daemon.log 2>&1 &
i=0; while [ ! -e /tmp/ino-bus ] && [ "$i" -lt 30 ]; do i=$((i+1)); sleep 0.5; done
[ -e /tmp/ino-bus ] && echo INO_BUS_UP=yes || echo INO_BUS_UP=no

# 1. Basic bus round-trip.
dbus-send --session --dest=org.freedesktop.DBus --print-reply --type=method_call \
  /org/freedesktop/DBus org.freedesktop.DBus.ListNames > /tmp/ino-names.out 2>&1
echo "INO_LISTNAMES_RC=$?"

# 2. THE REAL TEST: can the bus enumerate ACTIVATABLE services? This is what reads the
#    service directories, and it is what every XFCE component depends on.
dbus-send --session --dest=org.freedesktop.DBus --print-reply --type=method_call \
  /org/freedesktop/DBus org.freedesktop.DBus.ListActivatableNames > /tmp/ino-act.out 2>&1
echo "INO_LISTACT_RC=$?"
echo "INO_ACT_COUNT=$(grep -c 'string' /tmp/ino-act.out 2>/dev/null)"
head -20 /tmp/ino-act.out

# 3. Actually ACTIVATE something on demand, the operation that matters most.
dbus-send --session --dest=org.freedesktop.DBus --print-reply --type=method_call \
  /org/freedesktop/DBus org.freedesktop.DBus.StartServiceByName \
  string:org.xfce.Xfconf uint32:0 > /tmp/ino-start.out 2>&1
echo "INO_STARTSERVICE_RC=$?"
cat /tmp/ino-start.out
echo "=== daemon log ==="
cat /tmp/ino-daemon.log 2>/dev/null | head -15
echo INO_DONE
