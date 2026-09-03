#!/bin/sh
# Discriminator: does content ever return after the wipe, if a fresh X client
# connects later? If yes, this is a legitimate weston repaint-clears-to-empty
# situation (a compositing/scene-graph issue), not memory corruption. If it
# stays exactly zero forever regardless of later client activity, the buffer
# is genuinely dead (memory corruption).
#
# NOTE (AGENTS.md pass 322/324): the "wipe" this script was built to discriminate turned out to
# be the missing-XWM compositing bug, not memory corruption -- see run_xfce_staged.sh's own note
# for the full explanation. Fixed the same way here for consistency/future reuse: weston.ini gets
# xwayland=true, Xwayland is no longer spawned manually, and the display is discovered.
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
# Redirect weston to a file (a fast fatal error, e.g. a bad CLI arg, prints ONLY into a redirect
# file and otherwise leaves the readiness-wait loop below looking like an ordinary timeout with
# no indication why -- confirmed live this session). Every redirect file is cat'd unconditionally
# at the end, not just on a detected timeout.
weston --backend=drm-backend.so --socket=wayland-0 --use-pixman --shell=desktop-shell.so > /tmp/weston.out 2>&1 &
i=0; while [ ! -e "$XDG_RUNTIME_DIR/wayland-0" ] && [ "$i" -lt 60 ]; do i=$((i+1)); sleep 0.5; done
echo XC_WESTON=$i
sleep 6
echo XC_BEFORE_FORKS
# weston's own xwayland.so module (xwayland=true above) spawns and manages Xwayland itself now
# -- do NOT spawn it manually, that was exactly the bug. Discover whatever display it picked.
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
  for f in seatd weston; do
    echo "=== BEGIN $f.out ==="
    cat /tmp/$f.out 2>/dev/null || echo "(none)"
    echo "=== END $f.out ==="
  done
  echo XC_DONE
  exit 1
fi
unset WAYLAND_DISPLAY
DISPLAY="$XDISP" GDK_BACKEND=x11 xfce4-about --version > /tmp/xc1.out 2>&1
echo XC_CLIENT1_RC=$?
# Wait past the expected wipe (t~16-17 from process start) before the second client.
sleep 12
echo XC_PRE_CLIENT2
DISPLAY="$XDISP" GDK_BACKEND=x11 xfce4-about --version > /tmp/xc2.out 2>&1
echo XC_CLIENT2_RC=$?
sleep 8
echo XC_PRE_CLIENT3
DISPLAY="$XDISP" GDK_BACKEND=x11 xterm -e /bin/true > /tmp/xc3.out 2>&1
echo XC_CLIENT3_RC=$?
sleep 8
echo XC_PRE_CLIENT4
DISPLAY="$XDISP" GDK_BACKEND=x11 xclock -update 1 > /tmp/xc4.out 2>&1 &
sleep 10

# Surface every redirected service/client's own output unconditionally -- see the
# redirect-swallows-fatal-errors note above the weston spawn.
for f in seatd weston xc1 xc2 xc3 xc4; do
  echo "=== BEGIN $f.out ==="
  cat /tmp/$f.out 2>/dev/null || echo "(none)"
  echo "=== END $f.out ==="
done

echo XC_DONE
