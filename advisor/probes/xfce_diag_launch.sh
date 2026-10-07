#!/bin/sh
# XFCE launch script with the diagnostics the advisor has been asking for, and without the
# fixed-sleep races that invalidated earlier measurements.
#
# Run it as the runner's TOP-LEVEL program from inside the tar layer (never via `sh -c "..."`
# with a runtime base64/chmod step -- that pattern crashed /bin/sh with SIGILL and produced a
# false "litebox launch failure" result). On the host side, EXPORT these in the runner's own
# environment (not --env, which only reaches the guest):
#     export LITEBOX_LOG=error
#     export LITEBOX_DUMP_FRAMES=1
#
# What this fixes relative to xfce_full_retest.sh:
#   1. HOME is set, so xfce4-session's verbose log has a defined location.
#   2. XFSM_VERBOSE=1, so xfce4-session writes its own stage-by-stage startup trace.
#   3. Readiness POLLS instead of fixed sleeps. `sleep 2` after Xwayland raced its ~14 s
#      startup, so xfwm4 died with "cannot open display" in every earlier run.
#   4. GL avoided in the XFCE clients (GDK_GL=disable, softpipe, compositing off). Note this
#      does NOT remove Xwayland's own libGL/libLLVM load, which happens earlier and completes.
#   5. Generous budget: xfce4-session only reaches its interesting work ~60 s in.
#   6. Prints the verbose log at the end so it lands in the run log even if the writable layer
#      is not exported.

# NO 'set -x'. Shell tracing writes a trace line before every command, and the
# extra syscalls that generates -- interleaved with fork -- deterministically
# kill the first backgrounded child via a #UD in a syscall trampoline stub
# (bisected: 2/2 fail with it, 2/2 pass without). In these launchers that
# child is dbus-daemon, so tracing silently takes out the session bus and
# every component then fails with 'Connection refused'. Use explicit echo
# markers at stage boundaries instead. Repro: advisor/probes/setx_ud_repro.sh
export HOME=/root
mkdir -p /root /tmp/.X11-unix /run/user/0
chmod 1777 /tmp/.X11-unix

export XDG_RUNTIME_DIR=/run/user/0
chmod 700 /run/user/0

# --- compositor -------------------------------------------------------------------------
weston --backend=drm-backend.so --socket=wayland-0 --use-pixman --shell=desktop-shell.so &

# Wait for weston's socket rather than sleeping a fixed interval.
i=0
while [ ! -S "$XDG_RUNTIME_DIR/wayland-0" ] && [ "$i" -lt 120 ]; do
    i=$((i + 1))
    sleep 0.5
done
echo "WESTON_SOCKET_READY_AFTER=${i}x0.5s"

# --- X server ---------------------------------------------------------------------------
Xwayland :1 -geometry 1920x1080 -fullscreen -noreset -glamor off &

# Wait until :1 actually ACCEPTS connections. Socket existence alone is not enough: the file
# appears before the server is listening, which is what the old `sleep 2` was racing.
i=0
while [ "$i" -lt 240 ]; do
    if xdpyinfo -display :1 >/dev/null 2>&1; then
        break
    fi
    # Fall back to socket existence if xdpyinfo is not installed in this rootfs.
    if [ ! -x /usr/bin/xdpyinfo ] && [ -S /tmp/.X11-unix/X1 ]; then
        break
    fi
    i=$((i + 1))
    sleep 0.5
done
echo "XDISPLAY_READY_AFTER=${i}x0.5s"

unset WAYLAND_DISPLAY
export DISPLAY=:1
export GDK_BACKEND=x11
export XDG_SESSION_TYPE=x11

# Keep the XFCE clients off the GL path. (Xwayland's own GL load already happened above.)
export GDK_GL=disable
export GALLIUM_DRIVER=softpipe
unset LIBGL_ALWAYS_SOFTWARE

# xfce4-session's own startup trace -> $HOME/.xfce4-session.verbose-log
export XFSM_VERBOSE=1

# --- window manager ---------------------------------------------------------------------
# Compositing off removes a second GL consumer; ignore failure if xfconfd is not up yet.
xfconf-query -c xfwm4 -p /general/use_compositing -n -t bool -s false 2>/dev/null || true
xfwm4 --display=:1 --compositor=off &
sleep 2

# --- session ------------------------------------------------------------------------------
timeout 180 xfce4-session --display=:1 --disable-tcp
echo "XFCE4_SESSION_EXIT=$?"

# Surface the verbose trace in the run log regardless of whether the layer gets exported.
echo "=== BEGIN xfce4-session verbose log ==="
cat "$HOME/.xfce4-session.verbose-log" 2>/dev/null || echo "(no verbose log written)"
echo "=== END xfce4-session verbose log ==="

# What is still alive at the end tells you whether anything survived.
ps 2>/dev/null | head -40 || true
echo TEST_DONE
