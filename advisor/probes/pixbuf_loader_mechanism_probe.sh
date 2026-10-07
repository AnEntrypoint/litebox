#!/bin/sh
# XPM fails too, though libpixbufloader-xpm.so IS in the loaders dir. So this is
# not about PNG or glycin -- NO loader is being found. The most likely reason is
# that gdk-pixbuf looks up loaders through loaders.cache, and there is no cache
# (or an empty one), so it registers nothing at all and every format is
# "unrecognized". Test: generate a cache, then retry.
echo O_START
export HOME=/root; mkdir -p /root
D=/usr/lib/gdk-pixbuf-2.0/2.10.0
echo "O_CACHE_BEFORE=$(ls -la $D/loaders.cache 2>&1 | head -1)"
gdk-pixbuf-query-loaders > /tmp/lc 2>/tmp/lc.err
echo "O_QUERY_RC=$?  cache_bytes=$(wc -c < /tmp/lc)"
echo "O_QUERY_STDERR:"; head -3 /tmp/lc.err
echo "O_CACHE_HEAD:"; head -12 /tmp/lc
cp /tmp/lc "$D/loaders.cache"
echo "--- retry XPM with a real cache in place ---"
gdk-pixbuf-csource --raw /usr/share/themes/Daloa/xfwm4/bottom-active.xpm > /tmp/x.c 2>/tmp/x.err
echo "O_XPM_RC=$?  bytes=$(wc -c < /tmp/x.c 2>/dev/null)"
head -2 /tmp/x.err
echo "--- retry PNG ---"
gdk-pixbuf-csource --raw /usr/share/directfb-1.7.7/cursor.png > /tmp/p.c 2>/tmp/p.err
echo "O_PNG_RC=$?  bytes=$(wc -c < /tmp/p.c 2>/dev/null)"
head -2 /tmp/p.err
echo O_DONE
