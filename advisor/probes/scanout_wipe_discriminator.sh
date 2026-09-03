#!/bin/sh
# Discriminator: does content ever return after the wipe, if a fresh X client
# connects later? If yes, this is a legitimate weston repaint-clears-to-empty
# situation (a compositing/scene-graph issue), not memory corruption. If it
# stays exactly zero forever regardless of later client activity, the buffer
# is genuinely dead (memory corruption).
echo XC_START
export HOME=/root
export XDG_RUNTIME_DIR=/run/user/0
mkdir -p /root /run/user/0 /tmp/.X11-unix
chmod 1777 /tmp/.X11-unix
chmod 700 /run/user/0
rm -f /run/seatd.sock
seatd -l error > /tmp/seatd.out 2>&1 &
i=0; while [ ! -e /run/seatd.sock ] && [ "$i" -lt 30 ]; do i=$((i+1)); sleep 0.5; done
echo XC_SEATD=$i
# Redirect weston/Xwayland to files (a fast fatal error, e.g. a bad CLI arg, prints ONLY
# into a redirect file and otherwise leaves the readiness-wait loop below looking like an
# ordinary timeout with no indication why -- confirmed live this session). Every one of
# these redirect files is cat'd unconditionally at the end, not just on a detected timeout.
weston --backend=drm-backend.so --socket=wayland-0 --use-pixman --shell=desktop-shell.so > /tmp/weston.out 2>&1 &
i=0; while [ ! -e "$XDG_RUNTIME_DIR/wayland-0" ] && [ "$i" -lt 60 ]; do i=$((i+1)); sleep 0.5; done
echo XC_WESTON=$i
sleep 6
echo XC_BEFORE_FORKS
export WAYLAND_DISPLAY=wayland-0
Xwayland :1 -geometry 1920x1080 -fullscreen -noreset -glamor off > /tmp/xwayland.out 2>&1 &
echo XC_XWAYLAND_STARTED
i=0; while [ ! -e /tmp/.X11-unix/X1 ] && [ "$i" -lt 60 ]; do i=$((i+1)); sleep 0.5; done
echo XC_XSOCK=$i
unset WAYLAND_DISPLAY
DISPLAY=:1 GDK_BACKEND=x11 xfce4-about --version > /tmp/xc1.out 2>&1
echo XC_CLIENT1_RC=$?
# Wait past the expected wipe (t~16-17 from process start) before the second client.
sleep 12
echo XC_PRE_CLIENT2
DISPLAY=:1 GDK_BACKEND=x11 xfce4-about --version > /tmp/xc2.out 2>&1
echo XC_CLIENT2_RC=$?
sleep 8
echo XC_PRE_CLIENT3
DISPLAY=:1 GDK_BACKEND=x11 xterm -e /bin/true > /tmp/xc3.out 2>&1
echo XC_CLIENT3_RC=$?
sleep 8
echo XC_PRE_CLIENT4
DISPLAY=:1 GDK_BACKEND=x11 xclock -update 1 > /tmp/xc4.out 2>&1 &
sleep 10

# Surface every redirected service/client's own output unconditionally -- see the
# redirect-swallows-fatal-errors note above the weston/Xwayland spawns.
for f in seatd weston xwayland xc1 xc2 xc3 xc4; do
  echo "=== BEGIN $f.out ==="
  cat /tmp/$f.out 2>/dev/null || echo "(none)"
  echo "=== END $f.out ==="
done

echo XC_DONE
