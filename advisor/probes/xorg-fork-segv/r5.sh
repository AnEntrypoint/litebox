#!/bin/sh
export HOME=/root DISPLAY=:0
# Xorg via fork + IMMEDIATE exec (the pattern proven to survive for xsetroot/xkbcomp),
# NOT via `&` and NOT as pid 1. If Xorg SIGSEGVs here too, the write-to-RO-exec
# fault is about being a FORKED CHILD, independent of the & vs exec distinction.
/bin/sh -c "exec /usr/bin/Xorg :0 -logfile /tmp/x.log -noreset -novtswitch -sharevts" &
sleep 50
/bin/sh -c "exec /usr/bin/xsetroot -solid navy"; echo "SETROOT=$?"
sleep 15
echo "=== R5_DONE ==="
