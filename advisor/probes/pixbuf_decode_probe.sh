#!/bin/sh
# libgdk_pixbuf has libglycin-2.so.0 as a DIRECT DT_NEEDED dep and its
# gdk_pixbuf__glycin_* symbols are NOT exported -- so the glycin loader is a
# built-in, needing no loaders.cache entry. The decode must therefore be failing
# inside glycin at runtime. glycin uses Rust `tracing`; capture its stderr with
# RUST_LOG and check whether the sandbox fallback engages after the EPERM fix.
echo M_START
export HOME=/root; mkdir -p /root /run/user/0
export XDG_RUNTIME_DIR=/run/user/0
export RUST_LOG=trace
export G_MESSAGES_DEBUG=all
# glycin talks to its decoder over D-Bus, so give it a session bus.
mkdir -p /var/lib/dbus
dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>/dev/null || true
export DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/m-bus
dbus-daemon --nofork --nopidfile --nosyslog --config-file=/usr/share/dbus-1/session.conf --address="$DBUS_SESSION_BUS_ADDRESS" >/dev/null 2>&1 &
i=0; while [ ! -e /tmp/m-bus ] && [ "$i" -lt 20 ]; do i=$((i+1)); sleep 0.5; done
[ -e /tmp/m-bus ] && echo M_BUS=up || echo M_BUS=down
gdk-pixbuf-pixdata /usr/share/directfb-1.7.7/cursor.png /tmp/o.pixdata 2>&1 | head -30
[ -s /tmp/o.pixdata ] && echo "M_OK_BYTES=$(wc -c < /tmp/o.pixdata)" || echo M_EMPTY
echo M_DONE
