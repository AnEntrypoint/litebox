#!/usr/bin/bash
export HOME=/root
# swap in the wrapper (writable layer overlays the read-only image)
cp /usr/bin/xkbcomp /usr/bin/xkbcomp.real 2>/dev/null
cp /wrap.sh /usr/bin/xkbcomp 2>/dev/null
chmod +x /usr/bin/xkbcomp /usr/bin/xkbcomp.real 2>/dev/null
exec /usr/bin/Xorg :0 -logfile /tmp/x.log -noreset -novtswitch -sharevts
