#!/usr/bin/bash
export HOME=/root
# exec => this shell BECOMES Xorg, so Xorg is pid 1 and is never a forked child.
# -listen tcp: X normally disables TCP; we need it so a client in a SECOND runner
# can reach this server over published host loopback (guest unix sockets are
# per-runner and cannot be shared).
exec /usr/bin/Xorg :0 -logfile /tmp/x.log -noreset -novtswitch -sharevts -listen tcp -ac
