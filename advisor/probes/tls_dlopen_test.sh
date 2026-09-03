#!/bin/sh
# Does dlopen of a TLS-using library hang here? No display stack at all.
#
# The peer's trace shows xfwm4's last syscall is a futex ENTRY at t=28.3 with no
# exit, preceded by the glibc dynamic-TLS registration pattern
# (membarrier -> rt_sigaction -> tkill -> futex). If that futex is never
# signalled, every GTK client would hang during startup dlopen, which would
# explain the whole "alive, connected, never draws" picture at a stroke.
#
# This tests it WITHOUT weston, Xwayland or XFCE: just load the same libraries a
# GTK app loads, in a plain process, and see whether the loads return.
echo TLS_START
for lib in libgtk-3.so.0 libgdk-3.so.0 libglib-2.0.so.0 libgobject-2.0.so.0 libX11.so.6; do
  echo "TLS_TRY=$lib"
  # ldd resolves and loads the library's dependency graph, which exercises the
  # same dlopen path without needing a program that links it.
  ldd /usr/lib/$lib > /dev/null 2>&1
  echo "TLS_RC_$lib=$?"
done
echo TLS_LIBS_DONE
# And a real GTK binary invoked so it must dlopen its own stack:
xfce4-about --version > /tmp/tls_about.out 2>&1
echo "TLS_ABOUT_RC=$?"
echo TLS_DONE
