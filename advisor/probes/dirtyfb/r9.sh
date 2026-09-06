#!/bin/sh
# ENDPOINT-2 TEST with DIRTYFB implemented.
# Xorg is a FORKED child (the case the placement_floor fix repaired).
# Clients are plain fork+exec -- no wrapper, no retry loop, no second runner.
export HOME=/root DISPLAY=:0
/bin/sh -c "exec /usr/bin/Xorg :0 -logfile /tmp/x.log -noreset -novtswitch -sharevts" &
# Poll for the X socket rather than a fixed sleep.
i=0
while [ $i -lt 60 ]; do
  [ -e /tmp/.X11-unix/X0 ] && break
  i=$((i+1)); /usr/bin/sleep 1
done
echo "=== XSOCK after ${i}s ==="
/usr/bin/sleep 5

# Phase 1: solid navy + forced root repaint.
/bin/sh -c "exec /usr/bin/xsetroot -solid navy"; echo "NAVY=$?"
/bin/sh -c "exec /usr/bin/xrefresh"; echo "REFRESH1=$?"
/usr/bin/sleep 5

# Phase 2: a DIFFERENT colour, so a real capture must differ from phase 1.
/bin/sh -c "exec /usr/bin/xsetroot -solid red"; echo "RED=$?"
/bin/sh -c "exec /usr/bin/xrefresh"; echo "REFRESH2=$?"
/usr/bin/sleep 5

# Phase 3: a client that redraws on its own timer, not just a property setter.
/usr/bin/xclock &
/usr/bin/sleep 15
echo "=== R9_DONE ==="
