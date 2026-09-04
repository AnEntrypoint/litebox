#!/bin/sh
# All DT_NEEDED of the xpm loader resolve (ldd: zero "not found"), so a broken
# dependency chain is ruled out. Remaining candidates for "cache is valid, .so
# is loadable, yet no format registers":
#   (a) gdk-pixbuf never consults our cache -- it looks somewhere else.
#   (b) the module loads but its query function is not called/found.
# GDK_PIXBUF_MODULE_FILE is the documented override for the cache LOCATION and
# is honoured by libgdk_pixbuf (string present in the binary). If pointing it
# explicitly at our generated cache fixes the load, the bug was (a): a path
# mismatch, and the fix is trivial. If not, it is (b).
echo Q_START
export HOME=/root; mkdir -p /root
gdk-pixbuf-query-loaders > /tmp/lc 2>/dev/null
echo "Q_CACHE_BYTES=$(wc -c < /tmp/lc)"
export GDK_PIXBUF_MODULE_FILE=/tmp/lc
echo "Q_MODULE_FILE=$GDK_PIXBUF_MODULE_FILE"
gdk-pixbuf-csource --raw /usr/share/themes/Daloa/xfwm4/bottom-active.xpm > /tmp/x.c 2>/tmp/x.err
echo "Q_XPM_RC=$?  bytes=$(wc -c < /tmp/x.c 2>/dev/null)"
head -2 /tmp/x.err
gdk-pixbuf-csource --raw /usr/share/directfb-1.7.7/cursor.png > /tmp/p.c 2>/tmp/p.err
echo "Q_PNG_RC=$?  bytes=$(wc -c < /tmp/p.c 2>/dev/null)"
head -2 /tmp/p.err
echo Q_DONE
