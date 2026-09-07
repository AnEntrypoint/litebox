import io
import tarfile
import time

OUT = r'C:\dev\litebox-main\.wfgy\webtop_overlay.tar'

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
      alias /usr/share/selkies/selkies-dashboard/;
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
  echo "[stack] starting mate-session"
  cd /config
  /usr/bin/dbus-launch --exit-with-session /usr/bin/mate-session > /tmp/mate.log 2>&1 &
  sleep 30
  echo "[stack] --- mate.log ---"; tail -15 /tmp/mate.log 2>&1
  echo "[stack] --- xlsclients ---"; xlsclients 2>&1 | head -12
fi

echo "[stack] STACK_READY stage=$STAGE"
sleep "$HOLD"
"""


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
    add(tar, 'etc/nginx/nginx.conf', NGINX_CONF)
    add(tar, 'start-webtop.sh', LAUNCH, mode=0o755)

print('wrote', OUT)
