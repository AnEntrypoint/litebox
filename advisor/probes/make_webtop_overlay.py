import io
import os
import sys
import tarfile
import time

# Output path and dashboard root are arguments: the ubuntu-xfce image lays the selkies
# dashboard out at its own path, and forking this builder to change one string would fork
# every other fix in it too.
#   usage: make_webtop_overlay.py [<out.tar>] [<dashboard-dir>]
OUT = sys.argv[1] if len(sys.argv) > 1 else r'C:\dev\litebox-main\.wfgy\webtop_overlay.tar'
DASHBOARD = sys.argv[2] if len(sys.argv) > 2 else '/usr/share/selkies/selkies-dashboard/'

# A complete replacement nginx.conf rather than a drop-in under http.d, because the two
# directives that matter most here -- `daemon off` and `master_process off` -- are main-context
# only and cannot be expressed from an included http-context snippet.
#
# `master_process off` is load-bearing, not tidiness: letting nginx daemonize makes it fork, and
# a forking nginx reproducibly trips litebox's fork_verify stale-pointer healing path
# (`[diag-unrecov-av] ... is_in_guest=false is_verifying=true`, i.e. a fault in litebox's OWN
# host-side code, not in nginx). Single-process nginx serves this workload perfectly well.
NGINX_CONF = """daemon off;
master_process off;
worker_processes 1;
error_log /var/log/nginx/error.log warn;
pid /run/nginx/nginx.pid;

events {
  worker_connections 256;
}

http {
  include       /etc/nginx/mime.types;
  default_type  application/octet-stream;
  access_log    off;

  # sendfile() moves bytes entirely inside the kernel; on litebox that is emulated rather than
  # native, and plain read()+write() is the better-trodden path here.
  sendfile off;

  client_body_temp_path /var/lib/nginx/body;
  proxy_temp_path       /var/lib/nginx/proxy;
  fastcgi_temp_path     /var/lib/nginx/fastcgi;
  uwsgi_temp_path       /var/lib/nginx/uwsgi;
  scgi_temp_path        /var/lib/nginx/scgi;

  server {
    listen 3000 default_server;

    # Served straight out of the image's own dashboard directory. The stock image copies this
    # to /usr/share/selkies/web during s6 init; pointing the alias at the real directory instead
    # skips a multi-thousand-file copy that would buy nothing.
    location / {
      alias __DASHBOARD__;
      index index.html index.htm;
      try_files $uri $uri/ =404;
    }

    location /websocket {
      proxy_set_header        Upgrade $http_upgrade;
      proxy_set_header        Connection "upgrade";
      proxy_set_header        Host $host;
      proxy_set_header        X-Real-IP $remote_addr;
      proxy_set_header        X-Forwarded-For $proxy_add_x_forwarded_for;
      proxy_set_header        X-Forwarded-Proto $scheme;
      proxy_http_version      1.1;
      proxy_read_timeout      3600s;
      proxy_send_timeout      3600s;
      proxy_connect_timeout   3600s;
      proxy_buffering         off;
      client_max_body_size    10M;
      proxy_pass              http://127.0.0.1:8082;
    }
  }
}
"""

LAUNCH = r"""#!/bin/sh
export DISPLAY=:1
export HOME=/config
export USER=abc
export XDG_RUNTIME_DIR=/config/.XDG
export CUSTOM_WS_PORT=8082
export XCURSOR_THEME=Breeze_Light
export LANG=en_US.UTF-8
export PATH=/lsiopy/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

STAGE="${STACK_STAGE:-4}"
HOLD="${HOLD_SECS:-900}"

# Which desktop to start, as a RUNTIME parameter rather than a baked-in command.
#
# This launcher was written for `mate-session` and hard-coded it. The Xvfb/nginx/selkies stack
# underneath is desktop-environment-agnostic -- it streams whatever X surface exists -- so the
# hard-coded session was the only thing tying this overlay to one desktop, and bringing up XFCE
# would otherwise have meant a second near-identical copy to maintain in parallel. One overlay,
# selected with `--env DESKTOP_CMD=...`, keeps every fix (the non-forking dbus-daemon above all)
# shared between desktops instead of fixed once per copy.
#
# For XFCE that means `xfce4-session`, NOT `startxfce4`: startxfce4 is a wrapper whose main job
# is to start a session bus with dbus-launch and then exec xfce4-session. This script already
# owns that bus deliberately (see the dbus block below), so the wrapper would start a SECOND bus
# and hand the session a different address than the one exported here.
DESKTOP_CMD="${DESKTOP_CMD:-/usr/bin/mate-session --debug}"

mkdir -p /config/.XDG /config/Desktop /var/log/nginx /run/nginx \
  /var/lib/nginx/tmp /var/lib/nginx/logs /var/lib/nginx/body \
  /var/lib/nginx/proxy /var/lib/nginx/fastcgi /var/lib/nginx/uwsgi /var/lib/nginx/scgi
rm -f /tmp/.X1-lock

echo "[stack] stage=$STAGE starting Xvfb"
/usr/bin/Xvfb :1 -screen 0 1024x768x24 -dpi 96 \
  +extension COMPOSITE +extension DAMAGE +extension RANDR \
  +extension RENDER +extension XFIXES +extension XTEST \
  -nolisten tcp -ac -noreset > /tmp/xvfb.log 2>&1 &
sleep 15
if xset q > /dev/null 2>&1; then
  echo "[stack] XVFB_UP"
else
  echo "[stack] XVFB_FAILED -- xvfb.log follows"
  cat /tmp/xvfb.log 2>&1
fi

if [ "$STAGE" -ge 2 ]; then
  echo "[stack] starting nginx"
  /usr/sbin/nginx > /tmp/nginx.log 2>&1 &
  sleep 5
  echo "[stack] --- nginx stdout/stderr ---"; cat /tmp/nginx.log 2>&1
  echo "[stack] --- nginx error.log ---";     cat /var/log/nginx/error.log 2>&1
  if wget -q -O /tmp/idx.html -T 6 http://127.0.0.1:3000/ 2>/dev/null; then
    echo "[stack] HTTP_LOCAL_OK bytes=$(wc -c < /tmp/idx.html)"
    head -c 300 /tmp/idx.html; echo ""
  else
    echo "[stack] HTTP_LOCAL_FAIL"
  fi
fi

if [ "$STAGE" -ge 3 ]; then
  echo "[stack] starting selkies"
  /lsiopy/bin/selkies --addr=localhost --mode=websockets > /tmp/selkies.log 2>&1 &
  sleep 25
  echo "[stack] --- selkies.log ---"; tail -30 /tmp/selkies.log 2>&1
fi

if [ "$STAGE" -ge 4 ]; then
  # An explicit `dbus-daemon`, not `dbus-launch`.
  #
  # `dbus-launch --exit-with-session mate-session` is one opaque step that does three separable
  # things: fork a bus daemon, discover its address, and exec the session. When stage 4 produced a
  # completely empty `/tmp/mate.log` and a black root window, that single log could not say WHICH
  # of the three failed -- and dbus-launch is the fragile one here, since it double-forks and then
  # reads the daemon's pid back over a pipe (the `EOF in dbus-launch reading PID from bus daemon`
  # already seen on the cross-process fork path is exactly that read failing).
  #
  # Running the daemon directly makes the address an observable value rather than an implicit
  # side effect: if the bus never comes up, `DBUS_ADDR` is empty and this says so, and if it does,
  # every later failure is unambiguously mate-session's own.
  # NOT `--fork`. The daemon is backgrounded by the shell instead, so dbus-daemon itself never
  # forks and its listening socket stays in the process that created it.
  #
  # With `--fork`, mate-session hung with a completely diagnostic syscall tail: socket(AF_UNIX),
  # connect() to the bus path, sendto(1 byte) -- D-Bus's mandatory leading NUL -- sendto(18
  # bytes) starting the SASL AUTH exchange, then `ppoll(nfds=1, timeout=-1)` that never returns.
  # It reached the bus and the bus never answered. `--fork` daemonizes by creating the listening
  # socket, forking, and letting the child serve while the parent prints the address and exits;
  # litebox's fork carries pipes, regular files and the filesystem to a child but NOT sockets, so
  # the surviving child holds no listener and nobody ever accepts. (AF_UNIX itself is fine across
  # processes here -- mate-session's X11 connection to Xvfb is one, and the X census proves it
  # works.) Backgrounding from the shell puts the socket in the post-exec process, where it stays.
  echo "[stack] starting session dbus-daemon"
  rm -f /tmp/dbus-addr.txt
  /usr/bin/dbus-daemon --session --print-address > /tmp/dbus-addr.txt 2>/tmp/dbusd.log &
  # The address is written as soon as the listener is up; poll briefly rather than sleeping a
  # fixed span, so a fast start is not paid for and a slow one is not truncated.
  i=0
  while [ "$i" -lt 30 ]; do
    [ -s /tmp/dbus-addr.txt ] && break
    i=$((i + 1))
    sleep 1
  done
  DBUS_ADDR=$(cat /tmp/dbus-addr.txt 2>/dev/null)
  if [ -z "$DBUS_ADDR" ]; then
    echo "[stack] DBUS_FAILED -- dbusd.log follows"; cat /tmp/dbusd.log 2>&1
  else
    echo "[stack] DBUS_UP addr=$DBUS_ADDR"
    export DBUS_SESSION_BUS_ADDRESS="$DBUS_ADDR"
  fi

  echo "[stack] starting desktop: $DESKTOP_CMD"
  cd /config
  # `--debug` and an unbuffered stderr, because the previous invocation's silence was itself the
  # problem: mate-session logs nothing on a successful start, so an empty log was equally
  # consistent with "never ran" and "running fine". With --debug it always says something.
  # Straight to this script's own stdout through a prefixing pipe, NOT `> /tmp/mate.log`.
  #
  # With the redirect, `--debug` still produced a zero-byte log while `kill -0` said the process
  # was alive -- which leaves two incompatible readings (mate-session is silent, or its output
  # never reached the file) and no way to choose between them. The `[stack]` echoes around it
  # demonstrably reach the console, so routing mate-session the same way removes the file as a
  # variable: anything it writes now lands where output is already proven to arrive.
  $DESKTOP_CMD 2>&1 | sed "s/^/[de] /" &
  DE_PID=$!
  sleep 40
  if kill -0 "$DE_PID" 2>/dev/null; then
    echo "[stack] DE_ALIVE pid=$DE_PID"
  else
    echo "[stack] DE_EXITED pid=$DE_PID"
  fi
fi

if [ "$STAGE" -ge 5 ]; then
  # Ask the X server what is actually there, rather than inferring it from a browser screenshot.
  # A black stream is equally consistent with "no client connected" and "clients connected but
  # nothing mapped"; only the server can tell those apart. See `webtop_xcensus.py`.
  echo "[stack] --- X census ---"; python3 /xcensus.py 2>&1 | head -60
  echo "[stack] --- root grab ---"; python3 /grab_root.py 2>&1 | tail -32
fi

echo "[stack] STACK_READY stage=$STAGE"

if [ "$STAGE" -ge 5 ]; then
  # Re-census on a slow cadence while holding. A desktop that assembles late looks identical to
  # one that never assembles if the only census is taken at a single instant.
  ELAPSED=0
  while [ "$ELAPSED" -lt "$HOLD" ]; do
    sleep 60
    ELAPSED=$((ELAPSED + 60))
    echo "[stack] --- X census t=${ELAPSED}s ---"; python3 /xcensus.py 2>&1 | head -40
  done
else
  sleep "$HOLD"
fi
"""


def read_probe(name):
    """Read a sibling probe file so the overlay is reproducible from the repo alone.

    These three files used to be injected into the overlay tar by hand, which meant the committed
    builder produced an overlay that was missing them and the working overlay could not be
    regenerated from source. Reading them here makes this script the single definition of what the
    overlay contains.
    """
    with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), name), encoding='utf-8') as f:
        return f.read()


def add(tar, name, data, mode=0o644, isdir=False):
    ti = tarfile.TarInfo(name)
    ti.mtime = int(time.time())
    ti.uid = 0
    ti.gid = 0
    if isdir:
        ti.type = tarfile.DIRTYPE
        ti.mode = 0o755
        ti.size = 0
        tar.addfile(ti)
    else:
        b = data.encode('utf-8')
        ti.type = tarfile.REGTYPE
        ti.mode = mode
        ti.size = len(b)
        tar.addfile(ti, io.BytesIO(b))


with tarfile.open(OUT, 'w', format=tarfile.GNU_FORMAT) as tar:
    for d in ['config', 'config/.XDG', 'config/Desktop',
              'var', 'var/log', 'var/log/nginx',
              'var/lib', 'var/lib/nginx', 'var/lib/nginx/logs', 'var/lib/nginx/body',
              'var/lib/nginx/tmp', 'var/lib/nginx/proxy', 'var/lib/nginx/fastcgi',
              'var/lib/nginx/uwsgi', 'var/lib/nginx/scgi',
              'run', 'run/nginx',
              'etc', 'etc/nginx']:
        add(tar, d, None, isdir=True)
    add(tar, 'etc/nginx/nginx.conf', NGINX_CONF.replace('__DASHBOARD__', DASHBOARD))
    add(tar, 'start-webtop.sh', LAUNCH, mode=0o755)
    # `/patch` goes on the guest's PYTHONPATH, so CPython's own `site` module imports
    # `sitecustomize` before any application code runs -- the only hook that reaches selkies
    # early enough to fix its environment.
    add(tar, 'patch', None, isdir=True)
    add(tar, 'patch/sitecustomize.py', read_probe('webtop_sitecustomize.py'))
    add(tar, 'paint_root.py', read_probe('webtop_paint_root.py'))
    add(tar, 'grab_root.py', read_probe('webtop_grab_root.py'))
    add(tar, 'xcensus.py', read_probe('webtop_xcensus.py'))

print('wrote', OUT)
