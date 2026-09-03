#!/bin/sh
# Minimal: weston alone, then ONE fork, then watch the scanout.
# If the buffer wipes here, the culprit is any fork -- not Xwayland, not XFCE,
# and the repro drops from ~500s to ~40s.
echo XC_START
export HOME=/root
export XDG_RUNTIME_DIR=/run/user/0
mkdir -p /root /run/user/0 /tmp/.X11-unix
chmod 1777 /tmp/.X11-unix
chmod 700 /run/user/0
rm -f /run/seatd.sock
seatd -l error &
i=0; while [ ! -e /run/seatd.sock ] && [ "$i" -lt 30 ]; do i=$((i+1)); sleep 0.5; done
echo XC_SEATD=$i
# --logger-scopes turns on weston's own diagnostics. Capturing its stderr
# separately is what names WHY a committed surface is not composited,
# instead of us inferring it from memory contents.
weston --backend=drm-backend.so --socket=wayland-0 --use-pixman --shell=desktop-shell.so --logger-scopes=log > /tmp/weston.out 2>&1 &
i=0; while [ ! -e "$XDG_RUNTIME_DIR/wayland-0" ] && [ "$i" -lt 60 ]; do i=$((i+1)); sleep 0.5; done
echo XC_WESTON=$i
sleep 6
echo XC_BEFORE_FORKS
# Start Xwayland instead of plain forks: the ONE difference from weston_only.
export WAYLAND_DISPLAY=wayland-0
Xwayland :1 -geometry 1920x1080 -fullscreen -noreset -glamor off &
i=1
echo XC_XWAYLAND_STARTED
# Wait for the X socket, then connect ONE X client. That connection is what
# makes Xwayland fork xkbcomp (a ~46,000-pointer heal, vastly larger than a
# shell fork) -- the event the full-stack wipe always coincides with.
i=0; while [ ! -e /tmp/.X11-unix/X1 ] && [ "$i" -lt 60 ]; do i=$((i+1)); sleep 0.5; done
echo XC_XSOCK=$i
unset WAYLAND_DISPLAY
DISPLAY=:1 GDK_BACKEND=x11 xfce4-about --version > /tmp/xc.out 2>&1
echo XC_CLIENT_RC=$?
i=0; while [ "$i" -lt 20 ]; do i=$((i+1)); sleep 0.5; done
echo "=== BEGIN weston.out ==="
cat /tmp/weston.out 2>/dev/null | tail -60 || echo "(none)"
echo "=== END weston.out ==="
# Redirected-service output is otherwise invisible to the main log: a fast fatal
# error here (e.g. a bad CLI arg) prints ONLY into the redirect file, and the
# readiness-wait loops above just look like an ordinary timeout with no
# indication why (confirmed live this session with a weston arg typo). Always
# surface it, unconditionally, not just on a detected timeout.
echo "=== BEGIN xc.out ==="
cat /tmp/xc.out 2>/dev/null || echo "(none)"
echo "=== END xc.out ==="
i=0; while [ "$i" -lt 40 ]; do i=$((i+1)); sleep 0.5; done
echo XC_DONE
