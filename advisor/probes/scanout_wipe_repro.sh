#!/bin/sh
# Minimal: weston alone, then ONE fork, then watch the scanout.
# If the buffer wipes here, the culprit is any fork -- not Xwayland, not XFCE,
# and the repro drops from ~500s to ~40s.
#
# XWM FIX (AGENTS.md pass 322/324): the permanent scanout wipe this script was built to chase
# turned out to be caused by manually spawning `Xwayland :1 ...` as a bare separate process --
# weston's desktop-shell.so has no XWM logic of its own, only weston's own `xwayland.so` module
# does the -wm handshake that maps X11 client windows into weston's scene graph. Fixed the same
# way as run_xfce_staged.sh: weston.ini gets xwayland=true (written below), Xwayland is no longer
# spawned manually, and the display weston picks is discovered instead of hardcoded.
echo XC_START
export HOME=/root
export XDG_RUNTIME_DIR=/run/user/0
export XDG_CONFIG_HOME=/root/.config
mkdir -p /root /run/user/0 /tmp/.X11-unix "$XDG_CONFIG_HOME"
chmod 1777 /tmp/.X11-unix
chmod 700 /run/user/0
rm -f /run/seatd.sock

# weston's config loader checks $XDG_CONFIG_HOME/weston.ini FIRST (confirmed via the packed
# libexec_weston.so.0.0.0's own strings), not a "weston/" subdirectory -- falls back to
# /etc/xdg/weston/weston.ini.
cat > "$XDG_CONFIG_HOME/weston.ini" <<'WESTONINI'
[core]
shell=desktop-shell.so
xwayland=true

[shell]
background-color=0xff002244
locking=false

[input-method]
path=
WESTONINI
if [ -w /etc/xdg/weston/weston.ini ] || [ -w /etc/xdg/weston ]; then
  cp "$XDG_CONFIG_HOME/weston.ini" /etc/xdg/weston/weston.ini 2>/dev/null || true
fi

seatd -l error > /tmp/seatd.out 2>&1 &
i=0; while [ ! -e /run/seatd.sock ] && [ "$i" -lt 30 ]; do i=$((i+1)); sleep 0.5; done
echo XC_SEATD=$i
# --logger-scopes turns on weston's own diagnostics. Capturing its stderr
# separately is what names WHY a committed surface is not composited,
# instead of us inferring it from memory contents.
weston --backend=drm-backend.so --socket=wayland-0 --use-pixman --shell=desktop-shell.so --logger-scopes=log,xwm > /tmp/weston.out 2>&1 &
i=0; while [ ! -e "$XDG_RUNTIME_DIR/wayland-0" ] && [ "$i" -lt 60 ]; do i=$((i+1)); sleep 0.5; done
echo XC_WESTON=$i
sleep 6
echo XC_BEFORE_FORKS
# weston's own xwayland.so module (xwayland=true above) spawns and manages Xwayland itself now
# -- do NOT spawn it manually, that was exactly the bug. Discover whatever display it picked
# (observed :0 in every run so far, never hardcode it) by polling for the X11 socket it creates.
XDISP=""
i=0
while [ "$i" -lt 60 ]; do
  for n in 0 1 2; do [ -e "/tmp/.X11-unix/X$n" ] && XDISP=":$n" && break; done
  [ -n "$XDISP" ] && break
  i=$((i+1)); sleep 0.5
done
echo XC_XWAYLAND_DISPLAY_DISCOVERED="$XDISP"
echo XC_XSOCK=$i
if [ -z "$XDISP" ]; then
  echo XC_NO_DISPLAY_FOUND
  echo "=== BEGIN weston.out ==="
  cat /tmp/weston.out 2>/dev/null || echo "(none)"
  echo "=== END weston.out ==="
  echo XC_DONE
  exit 1
fi
unset WAYLAND_DISPLAY
DISPLAY="$XDISP" GDK_BACKEND=x11 xfce4-about --version > /tmp/xc.out 2>&1
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
