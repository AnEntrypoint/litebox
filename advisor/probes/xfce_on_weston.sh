#!/bin/sh
# XFCE under WESTON rather than labwc.
#
# Why: labwc's own source (src/server.c) unconditionally does
#     wlr_output_destroy(wlr_headless_add_output(server->headless.backend, 0, 0));
# i.e. it creates a 0x0 headless output and immediately destroys it, as a documented
# workaround for virtual-output overlay. Under litebox something renders that transient
# output, and wlroots' wlr_swapchain_create asserts on `width > 0 && height > 0`, killing
# the session. That is upstream labwc behaviour, not a litebox defect: litebox's DRM
# emulation is confirmed working (1920x1080 mode enumerated and selected, dumb buffer
# allocated, pixman renderer created, swapchain tested OK on 'Virtual-1').
#
# weston, by contrast, is already proven to render a complete desktop under litebox in this
# very environment (2,073,597 non-black pixels: full background plus a full-width panel).
# So this script runs the XFCE session on the compositor half that is known to work.
#
# Run it as the runner's TOP-LEVEL program from inside the tar layer (never via
# `sh -c "..."` with a runtime decode step). On the HOST side export, in the runner's own
# environment (not --env, which only reaches the guest):
#     export LITEBOX_LOG=error
#     export LITEBOX_DUMP_FRAMES=1
#
# Layer that contains everything needed: xfce-layer31-nopanel.tar provides
# bin/weston, bin/xfce4-session, bin/xfce4-panel, bin/xfdesktop, bin/xfwm4.

set -x
export HOME=/root
export XDG_RUNTIME_DIR=/run/user/0
mkdir -p /root /run/user/0 /tmp/.X11-unix
chmod 700 /run/user/0
chmod 1777 /tmp/.X11-unix

# A stale non-socket /run/seatd.sock from a resumed writable layer blocks seatd. Always clear.
rm -f /run/seatd.sock

# --- session bus -------------------------------------------------------------------------
dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>/dev/null || true
export DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/xfce-bus
dbus-daemon --nofork --nopidfile --nosyslog --address="$DBUS_SESSION_BUS_ADDRESS" --session &
i=0
while [ ! -e /tmp/xfce-bus ] && [ "$i" -lt 60 ]; do i=$((i + 1)); sleep 0.5; done
echo "DBUS_READY_AFTER=${i}x0.5s"

# --- seat --------------------------------------------------------------------------------
seatd -l error &
i=0
while [ ! -e /run/seatd.sock ] && [ "$i" -lt 60 ]; do i=$((i + 1)); sleep 0.5; done
echo "SEATD_READY_AFTER=${i}x0.5s"

# --- compositor (the proven one) ----------------------------------------------------------
# --use-pixman: software rendering, no GL stack. Confirmed working under litebox.
weston --backend=drm-backend.so --socket=wayland-0 --use-pixman --shell=desktop-shell.so &

# Poll for weston's socket rather than sleeping a fixed interval.
i=0
while [ ! -e "$XDG_RUNTIME_DIR/wayland-0" ] && [ "$i" -lt 120 ]; do i=$((i + 1)); sleep 0.5; done
echo "WESTON_SOCKET_READY_AFTER=${i}x0.5s"
export WAYLAND_DISPLAY=wayland-0

# --- X server for the XFCE clients ---------------------------------------------------------
# Rootful + fullscreen so xfwm4 and xfdesktop get a real root window. -glamor off keeps it
# on the software path, matching the pixman compositor.
Xwayland :1 -geometry 1920x1080 -fullscreen -noreset -glamor off &

# Wait until :1 genuinely ACCEPTS connections. Socket existence alone is not enough, and a
# fixed sleep here was the original launch-ordering bug (Xwayland needs ~14s in this env).
i=0
while [ "$i" -lt 240 ]; do
    if xdpyinfo -display :1 >/dev/null 2>&1; then break; fi
    if [ ! -x /usr/bin/xdpyinfo ] && [ -e /tmp/.X11-unix/X1 ]; then break; fi
    i=$((i + 1)); sleep 0.5
done
echo "XDISPLAY_READY_AFTER=${i}x0.5s"

unset WAYLAND_DISPLAY
export DISPLAY=:1
export GDK_BACKEND=x11
export XDG_SESSION_TYPE=x11
export GDK_GL=disable
export XFSM_VERBOSE=1

# Keep panel plugins in-process: avoids spawning plugin wrappers via fork while that path
# is still under investigation.
xfconf-query -c xfce4-panel -p /force-all-internal -n -t bool -s true 2>/dev/null || true
xfconf-query -c xfwm4 -p /general/use_compositing -n -t bool -s false 2>/dev/null || true

# --- XFCE ----------------------------------------------------------------------------------
xfwm4 --display=:1 --compositor=off &
sleep 2
timeout 240 xfce4-session --display=:1 --disable-tcp
echo "XFCE4_SESSION_EXIT=$?"

echo "=== BEGIN xfce4-session verbose log ==="
cat "$HOME/.xfce4-session.verbose-log" 2>/dev/null || echo "(no verbose log written)"
echo "=== END xfce4-session verbose log ==="

ps 2>/dev/null | head -40 || true
echo TEST_DONE
