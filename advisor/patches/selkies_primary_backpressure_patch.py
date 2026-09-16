#!/usr/bin/env python3
"""Guest-side patch: close the primary-display ping-starvation gap in selkies.py.

WHY. selkies (selkies-project/selkies @ 348bc4f61da66198573e7e57db9a266aca1991d5,
src/selkies/selkies.py, the pin docker-baseimage-selkies uses) runs its keepalive
ping and its video frames over the SAME websocket transport buffer. For the
'primary' display -- the only display mode a single-client webtop deployment like
this project's ever exercises -- _video_chunk_sender's send path is
`websockets.broadcast(primary_viewers, data_chunk)`. websockets' own broadcast()
docstring is explicit: "pushes the message synchronously to all connections even
if their write buffers are overflowing. There's no backpressure." Separately,
selkies already has a working, fast-reacting (0.5s poll, 4s stall timeout)
app-level backpressure system (_run_frame_backpressure_logic) that sets each
client's 'backpressure_enabled' to False on ACK stall or frame-desync -- but the
'primary' branch of _video_chunk_sender only used that flag to gate per-frame RTT
bookkeeping, never the actual broadcast() call (the parallel 'secondary'-display
branch a few lines below DOES gate its send on the same flag -- this is a real
asymmetry, not a design choice). Net effect: once a client falls behind, primary
frames keep getting written into its transport buffer with literally zero
backpressure from either layer, and that backlog's own bytes queue ahead of the
ping's bytes on the wire, so the pong can miss ping_timeout (20s) purely from
already-queued video data -- killing an otherwise-healthy connection.

THE FIX (applied to a live-fetched copy of the pinned selkies.py, reasoned
through against the real websockets.asyncio.connection.Connection/broadcast()
source -- not run against a live boot; selkies had a 0/7 recent bind rate this
session from an unrelated, already-tracked issue, see AGENTS.md "ET_EXEC
finding"). Two changes, both confined to _video_chunk_sender's 'primary' branch:

1. Actually gate the broadcast() call on 'backpressure_enabled', matching the
   'secondary' branch's existing (correct) behavior -- this alone lets the
   already-working 0.5-4s ACK-stall/frame-desync detector stop feeding a
   falling-behind client before its backlog can grow unbounded.
2. Defense in depth for the gap between backpressure_check_interval_s polls:
   check each viewer's real transport.get_write_buffer_size() (a live asyncio
   Transport method -- Connection.transport is a real asyncio.Transport per
   connection_made(), connection.py:1013) and skip that one frame for that one
   client if already backlogged past VIDEO_BACKLOG_DROP_THRESHOLD_BYTES
   (default 256 KiB, env-tunable via SELKIES_VIDEO_BACKLOG_LIMIT_BYTES). 256 KiB
   is sized to clear comfortably below any realistic sustained throughput this
   deployment could see in well under the 20s ping_timeout (>1s even at a bad
   256 kbps), while staying above a single H.264 keyframe's typical size at
   1280x800 so ordinary IDR frames are not spuriously dropped under merely
   transient jitter -- this does NOT touch websockets' own send_data()/drain()/
   ping()/keepalive() code (third-party, pinned, correct on its own terms); it
   only stops selkies' own primary-display sender from feeding more bytes into
   an already-backlogged transport, so the shared buffer a ping's bytes could
   ever queue behind is bounded instead of unbounded.

USAGE. Run inside the guest, after selkies is installed but before it is first
launched (this deployment's .wfgy/webtop_stack.sh embeds an inline copy of this
same patch and runs it right after DBUS_UP, before the selkies supervisor loop
starts -- keep the two in sync if this file changes). Idempotent: a re-run on an
already-patched file is a no-op. Never modifies the file if its content has
drifted from the exact block this targets -- reports and exits nonzero instead,
rather than risk corrupting an unexpected selkies.py version.

VERIFICATION (once a stable boot exists again -- see AGENTS.md for the current
selkies boot-rate blocker, unrelated to this fix):
  - Cheap/offline: this script's own SELKIES_PATCH_APPLIED log line, plus
    `python3 -m py_compile <patched selkies.py>` (this script already does this
    before writing).
  - Live: throttle host->browser bandwidth below the encoder's real output rate
    for >20s (Windows QoS policy, or read the client side of the websocket
    slowly to simulate a backed-up consumer), ideally forcing an IDR mid-throttle
    (resize or reconnect) to inject one oversized single-frame write. Pre-fix,
    `sk.log` should show `keepalive ping timeout` inside that window. Post-fix,
    watch for `Backpressure TRIGGERED for 'primary'` (now load-bearing on the
    actual send, not just a log line) and confirm no ping timeout fires while
    the throttle holds; frame drops are expected and correct in that state.
"""

import ast
import os
import shutil
import sys

OLD_BLOCK = """\
                if display_id == 'primary':
                    secondary_websockets = {
                        client_info.get('ws')
                        for did, client_info in self.display_clients.items()
                        if did != 'primary' and client_info.get('ws')
                    }
                    primary_viewers = self.clients - secondary_websockets

                    if not primary_viewers:
                        queue.task_done()
                        continue
                    now = time.monotonic()
                    for client_ws in primary_viewers:
                        for primary_client_info in self.display_clients.values():
                            if primary_client_info.get('ws') is client_ws:
                                if primary_client_info.get('backpressure_enabled', True):
                                    primary_client_info['sent_timestamps'][frame_id] = now
                                    primary_client_info['last_sent_frame_id'] = frame_id
                                    if len(primary_client_info['sent_timestamps']) > SENT_FRAME_TIMESTAMP_HISTORY_SIZE:
                                        primary_client_info['sent_timestamps'].popitem(last=False)
                                break
                    try:
                        websockets.broadcast(primary_viewers, data_chunk)
                        self._bytes_sent_in_interval += len(data_chunk) * len(primary_viewers)
                    except Exception as e:
                        data_logger.error(f"Error during primary broadcast: {e}")
"""

NEW_BLOCK = """\
                if display_id == 'primary':
                    secondary_websockets = {
                        client_info.get('ws')
                        for did, client_info in self.display_clients.items()
                        if did != 'primary' and client_info.get('ws')
                    }
                    primary_viewers = self.clients - secondary_websockets

                    if not primary_viewers:
                        queue.task_done()
                        continue
                    now = time.monotonic()
                    # PING-STARVATION FIX (2026-09-16): websockets.broadcast() applies NO
                    # backpressure by its own documentation. This branch used to compute
                    # 'backpressure_enabled' per viewer but never acted on it before
                    # broadcasting -- unlike the secondary-display branch below, which
                    # already gates its send on the same flag. Gate the send here too, and
                    # add a direct transport-write-buffer check as a faster-reacting safety
                    # net for the gap between backpressure_check_interval_s polls, so a
                    # falling-behind client's backlog can never grow unbounded and starve
                    # its own keepalive ping past ping_timeout.
                    sendable_viewers = set()
                    for client_ws in primary_viewers:
                        primary_client_info = None
                        for candidate_info in self.display_clients.values():
                            if candidate_info.get('ws') is client_ws:
                                primary_client_info = candidate_info
                                break
                        if primary_client_info is None or not primary_client_info.get('backpressure_enabled', True):
                            continue
                        transport = getattr(client_ws, 'transport', None)
                        if transport is not None:
                            try:
                                backlog_bytes = transport.get_write_buffer_size()
                            except Exception:
                                backlog_bytes = 0
                            if backlog_bytes > VIDEO_BACKLOG_DROP_THRESHOLD_BYTES:
                                continue
                        primary_client_info['sent_timestamps'][frame_id] = now
                        primary_client_info['last_sent_frame_id'] = frame_id
                        if len(primary_client_info['sent_timestamps']) > SENT_FRAME_TIMESTAMP_HISTORY_SIZE:
                            primary_client_info['sent_timestamps'].popitem(last=False)
                        sendable_viewers.add(client_ws)

                    if sendable_viewers:
                        try:
                            websockets.broadcast(sendable_viewers, data_chunk)
                            self._bytes_sent_in_interval += len(data_chunk) * len(sendable_viewers)
                        except Exception as e:
                            data_logger.error(f"Error during primary broadcast: {e}")
"""

CONST_ANCHOR = "SENT_FRAME_TIMESTAMP_HISTORY_SIZE = 1000\n"
CONST_INSERT = (
    "SENT_FRAME_TIMESTAMP_HISTORY_SIZE = 1000\n"
    "# PING-STARVATION FIX (2026-09-16): see selkies_primary_backpressure_patch.py.\n"
    "VIDEO_BACKLOG_DROP_THRESHOLD_BYTES = int(\n"
    "    os.environ.get(\"SELKIES_VIDEO_BACKLOG_LIMIT_BYTES\", 262144)\n"
    ")\n"
)

MARKER = "PING-STARVATION FIX (2026-09-16)"


def find_selkies_file():
    try:
        import selkies.selkies as m  # noqa: F401  (guest-side import; not resolvable on host)
    except Exception as e:
        print(f"[patch] SELKIES_PATCH_SKIPPED reason=import_failed error={e}")
        return None
    return m.__file__


def main():
    path = find_selkies_file()
    if not path:
        return 1

    src = open(path, "r", encoding="utf-8").read()

    if MARKER in src:
        print(f"[patch] SELKIES_PATCH_ALREADY_APPLIED path={path}")
        return 0

    if OLD_BLOCK not in src or CONST_ANCHOR not in src:
        print(
            f"[patch] SELKIES_PATCH_SKIPPED reason=source_mismatch path={path} "
            "-- selkies.py has drifted from the pinned 348bc4f6 source this patch "
            "targets; not touching the file. Re-derive OLD_BLOCK/CONST_ANCHOR "
            "against the real installed file before retrying."
        )
        return 1

    patched = src.replace(CONST_ANCHOR, CONST_INSERT, 1)
    patched = patched.replace(OLD_BLOCK, NEW_BLOCK, 1)

    try:
        ast.parse(patched)
    except SyntaxError as e:
        print(f"[patch] SELKIES_PATCH_FAILED reason=syntax_error_after_patch error={e}")
        return 1

    backup = path + ".pre-backpressure-patch.bak"
    if not os.path.exists(backup):
        shutil.copy2(path, backup)

    tmp = path + ".tmp-backpressure-patch"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(patched)
    os.replace(tmp, path)

    print(f"[patch] SELKIES_PATCH_APPLIED path={path} backup={backup}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
