#!/bin/sh
# Stock gdk-pixbuf 2.44.7 is built with GDK_PIXBUF_USE_GIO_MIME: it detects format via GIO
# MIME sniffing, NOT magic bytes. GIO needs /usr/share/mime/mime.cache, which the layer does
# not ship -- update-mime-database IS present but was never run. Without the cache, EVERY
# format is "unrecognized" regardless of loaders, which is exactly the symptom measured.
# With SOCK_SEQPACKET now implemented, glycin's decoder transport should also work.
echo MM_START
export HOME=/root; mkdir -p /root /run/user/0 /var/lib/dbus
export XDG_RUNTIME_DIR=/run/user/0
ls /usr/share/mime/mime.cache >/dev/null 2>&1 && echo MM_CACHE_BEFORE=present || echo MM_CACHE_BEFORE=absent
update-mime-database /usr/share/mime > /tmp/umd.err 2>&1
echo "MM_UPDATE_RC=$?"
head -3 /tmp/umd.err
ls -la /usr/share/mime/mime.cache 2>&1 | head -1
dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>/dev/null || true
export DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/mm-bus
dbus-daemon --nofork --nopidfile --nosyslog --config-file=/usr/share/dbus-1/session.conf --address="$DBUS_SESSION_BUS_ADDRESS" >/dev/null 2>&1 &
i=0; while [ ! -e /tmp/mm-bus ] && [ "$i" -lt 20 ]; do i=$((i+1)); sleep 0.5; done
echo "--- decode a real PNG ---"
gdk-pixbuf-csource --raw /usr/share/directfb-1.7.7/cursor.png > /tmp/p.c 2>/tmp/p.err
echo "MM_PNG_RC=$?  bytes=$(wc -c < /tmp/p.c 2>/dev/null)"
head -3 /tmp/p.err
echo MM_DONE
