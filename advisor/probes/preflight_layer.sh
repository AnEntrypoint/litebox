#!/bin/sh
# Fail-fast preflight for a guest layer. Run this BEFORE spending a 400-second
# launch run, so a packaging problem is reported in seconds with an exact cause
# instead of showing up as a silent hang, an empty log, or a black screen.
#
# Every check prints PASS or FAIL with the specific thing that is wrong.
# Exit code is the number of failures, so it drops straight into CI or a
# pre-run gate.
#
#   sh preflight_layer.sh <extracted-layer-root> [more paths to require...]
#
# Rationale: each of the failures below has already cost this project at least
# one full launch run to discover, one at a time:
#   - a missing SONAME link (weston/Xwayland/xfce4-session died with an EMPTY log)
#   - no /etc/ld-musl-x86_64.path (so /usr/lib/weston was unsearchable)
#   - xfconfd absent from PATH (xfce4-session ran with an EMPTY session, silently)
#   - no X client at all in the layer (so nothing could ever verify X was up)

ROOT="${1:?usage: preflight_layer.sh <extracted-layer-root> [required paths...]}"
shift 2>/dev/null
fail=0
here=$(dirname "$0")

say_pass() { printf 'PASS  %s\n' "$1"; }
say_fail() { printf 'FAIL  %s\n' "$1"; fail=$((fail + 1)); }

# 1. A shell and the dynamic loader. Without these NOTHING runs, and the runner
#    reports it as an ENOENT that overflows its own stack, hiding the cause.
for f in bin/sh lib/ld-musl-x86_64.so.1; do
    if [ -e "$ROOT/$f" ]; then say_pass "$f present"
    else say_fail "$f MISSING -- tar built with cp -r drops symlinks; append the overlay onto a copy of the base tar instead"; fi
done

# 2. The musl search path. Its absence is silent: libraries in non-default
#    directories simply never resolve.
if [ -e "$ROOT/etc/ld-musl-x86_64.path" ]; then
    say_pass "ld-musl path: $(cat "$ROOT/etc/ld-musl-x86_64.path")"
else
    say_fail "etc/ld-musl-x86_64.path MISSING -- anything outside /lib:/usr/local/lib:/usr/lib will not resolve"
fi

# 3. Unresolved DT_NEEDED across every ELF. This is the big one.
if [ -f "$here/audit_layer_deps.py" ]; then
    n=$(python "$here/audit_layer_deps.py" "$ROOT" --quiet 2>/dev/null | grep -c '^MISSING')
    if [ "$n" = "0" ]; then say_pass "all ELF dependencies resolve"
    else say_fail "$n ELF(s) with unresolved dependencies -- run: python $here/fix_layer_sonames.py $ROOT"; fi
else
    say_fail "audit_layer_deps.py not found next to this script"
fi

# 4. At least one X client, so the display can actually be verified rather than
#    assumed from the existence of a socket file.
xc=""
for c in xdpyinfo xrandr xsetroot xprop xhost xfce4-about; do
    [ -e "$ROOT/usr/bin/$c" ] && { xc="$c"; break; }
done
if [ -n "$xc" ]; then say_pass "X client available for readiness checks: $xc"
else say_fail "NO X client in layer -- X readiness can only be guessed at, never verified"; fi

# 5. Binaries that a launch script will invoke by bare name must be ON PATH.
#    xfconfd is the known trap: it exists, but under /usr/lib/xfce4/xfconf/,
#    so xfce4-session starts with no configuration and silently does nothing.
for b in weston Xwayland dbus-daemon seatd xfce4-session; do
    if [ -e "$ROOT/usr/bin/$b" ] || [ -e "$ROOT/bin/$b" ]; then say_pass "$b on PATH"
    else say_fail "$b NOT on PATH"; fi
done
if [ -e "$ROOT/usr/bin/xfconfd" ]; then
    say_pass "xfconfd on PATH"
elif [ -e "$ROOT/usr/lib/xfce4/xfconf/xfconfd" ]; then
    say_fail "xfconfd exists but is NOT on PATH (/usr/lib/xfce4/xfconf/xfconfd) -- start it by full path or xfce4-session runs an EMPTY session with no error"
else
    say_fail "xfconfd MISSING entirely -- xfce4-session will launch nothing and report no error"
fi

# 6. The session configuration that names what to launch.
if [ -e "$ROOT/etc/xdg/xfce4/xfconf/xfce-perchannel-xml/xfce4-session.xml" ]; then
    say_pass "xfce4-session.xml present (defines the Failsafe session)"
else
    say_fail "xfce4-session.xml MISSING -- xfce4-session has nothing to launch"
fi

# 7. Anything else the caller demanded.
for extra in "$@"; do
    if [ -e "$ROOT/$extra" ]; then say_pass "$extra present"; else say_fail "$extra MISSING"; fi
done

printf '\n%d check(s) failed\n' "$fail"
exit "$fail"
