"""Host-side reverse proxy for the litebox webtop.

Serves the selkies dashboard's static files and tunnels /websockets straight through to selkies
inside the guest via its --publish'd port. This exists to route AROUND a real litebox gap, not to
hide it: guest processes do not share a loopback namespace, so the in-guest nginx cannot reach
the in-guest selkies at 127.0.0.1:8082 (it gets a 502) even though BOTH are reachable from the
host through --publish. Moving the reverse proxy to the host removes the only hop that needed
guest-internal networking. Everything that actually makes the desktop -- Xvfb, selkies, its
pixelflux/pcmflux encoders -- still runs entirely inside litebox.

The dashboard hard-derives its WebSocket URL from window.location.host (`ws://<host>/<base>websockets`,
with no override parameter), which is why the static files and the websocket must be served from
one origin rather than simply pointed at the published port.
"""

import os
import socket
import sys
import threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'dashboard')
UPSTREAM = ('127.0.0.1', int(os.environ.get('SELKIES_PORT', '8082')))
LISTEN_PORT = int(os.environ.get('PROXY_PORT', '8090'))


def pump(src, dst):
    try:
        while True:
            b = src.recv(65536)
            if not b:
                break
            dst.sendall(b)
    except OSError:
        pass
    finally:
        for s in (src, dst):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


class Handler(SimpleHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def __init__(self, *a, **kw):
        super().__init__(*a, directory=ROOT, **kw)

    def log_message(self, fmt, *args):  # keep the console readable
        pass

    def _tunnel(self):
        """Hand the raw connection to selkies, headers and all."""
        try:
            up = socket.create_connection(UPSTREAM, timeout=15)
        except OSError as e:
            self.send_error(502, f'upstream {UPSTREAM} unreachable: {e}')
            return

        # Replay the request line and headers exactly as received, then let the two sockets talk
        # directly -- a WebSocket upgrade is just HTTP followed by an opaque byte stream, so no
        # framing awareness is needed here.
        head = [f'{self.command} {self.path} {self.request_version}\r\n']
        for k, v in self.headers.items():
            head.append(f'{k}: {v}\r\n')
        head.append('\r\n')
        try:
            up.sendall(''.join(head).encode('latin-1'))
        except OSError as e:
            self.send_error(502, f'upstream write failed: {e}')
            up.close()
            return

        self.close_connection = True
        client = self.connection
        t = threading.Thread(target=pump, args=(up, client), daemon=True)
        t.start()
        pump(client, up)
        t.join(timeout=1)

    def do_GET(self):
        if self.path.startswith('/websockets') or self.path.startswith('/websocket'):
            self._tunnel()
            return
        super().do_GET()

    def do_POST(self):
        self._tunnel()


def main():
    if not os.path.isdir(ROOT):
        sys.exit(f'dashboard root not found: {ROOT}')
    srv = ThreadingHTTPServer(('127.0.0.1', LISTEN_PORT), Handler)
    srv.daemon_threads = True
    print(f'serving {ROOT} on http://127.0.0.1:{LISTEN_PORT}/  ->  websockets to {UPSTREAM[0]}:{UPSTREAM[1]}',
          flush=True)
    srv.serve_forever()


if __name__ == '__main__':
    main()
