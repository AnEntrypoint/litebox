"""litebox bring-up shim for the selkies webtop stack.

Two jobs, both narrow:

1. Restore `DISPLAY`. selkies' own process ends up with no `DISPLAY` in its environment, so its
   screen probe runs `xrandr`, gets "Can't open display", cannot determine a screen name, and
   aborts the video pipeline outright:
       WARNING:gst_app_resize:Could not determine connected screen from xrandr.
       ERROR:data_websocket:CRITICAL: Could not determine screen name from xrandr. Aborting.
       ERROR:data_websocket:FATAL: Initial reconfiguration completed, but video pipeline did not start.
   Everything else in the stack (xterm, xset, a standalone asyncio xrandr) sees `:1` correctly,
   so this is specific to selkies' own environment handling. `SELKIES_DISPLAY` overrides.

2. Disable spawn sites that cannot succeed here. Each subprocess spawn is a roll of litebox's
   fork_verify healing path, and none of this work can succeed with no desktop environment and no
   PulseAudio daemon:
     * set_dpi / set_cursor_size -- shell out to xrdb / gsettings / xfconf-query.
     * pulsectl.Pulse.connect -- libpulse AUTOSPAWNS a daemon when it cannot reach a server, and
       PULSE_SERVER pointing at a dead socket does not suppress it. selkies already handles a
       PulseError here and carries on.

Deliberately NOT disabled: `resize_display` / `generate_xrandr_gtf_modeline`. An earlier revision
neutered those too, which looked harmless and silently broke the whole point -- the video
pipeline needs the screen name they obtain.

selkies ships TWO copies of these helpers (`selkies.display_utils` and duplicates inside
`selkies.selkies`); the live path is the latter, so both are covered.
"""

import os
import subprocess as _subprocess
import sys

_DISPLAY = os.environ.get("SELKIES_DISPLAY") or ":1"
if not os.environ.get("DISPLAY"):
    os.environ["DISPLAY"] = _DISPLAY


def _probe_get_new_res():
    """Report what selkies' own screen probe actually saw, on the run where it failed.

    `get_new_res` swallows every outcome into one warning line ("Could not determine connected
    screen from xrandr") that cannot distinguish the three things that produce it: `xrandr` not
    reachable, `xrandr` reachable but with no usable `DISPLAY` (its "Can't open display" goes to
    stdout, because the caller merges stderr into it), and `xrandr` succeeding but its output not
    matching. Under litebox all three were live hypotheses at once -- a standalone `xrandr`, a
    `subprocess.run` and an `asyncio` `create_subprocess_exec` all returned the correct
    `screen connected 1280x800+0+0` from the same guest, yet selkies still reported the failure.

    So this logs, from inside the failing process at the moment of failure: the tuple `get_new_res`
    returned, the `DISPLAY` that process actually has, and an independent synchronous `xrandr`
    capture for comparison. Cheap, fires at most a handful of times per session (only on client
    connect and resize), and prints nothing unless selkies gets that far.
    """
    try:
        import selkies.selkies as _sel
    except Exception as exc:  # pragma: no cover - diagnostic only
        print(f"[litebox-probe] selkies.selkies not importable: {exc!r}", file=sys.stderr, flush=True)
        return
    _orig = getattr(_sel, "get_new_res", None)
    if _orig is None:
        print("[litebox-probe] selkies.selkies has no get_new_res", file=sys.stderr, flush=True)
        return

    async def _wrapped(res_str):
        result = await _orig(res_str)
        try:
            screen_name = result[4] if len(result) > 4 else None
            sync = _subprocess.run(["xrandr"], capture_output=True, text=True)
            print(
                f"[litebox-probe] get_new_res({res_str!r}) screen_name={screen_name!r} "
                f"curr={result[0]!r} modes={result[2]!r} | DISPLAY={os.environ.get('DISPLAY')!r} "
                f"| sync xrandr rc={sync.returncode} out={sync.stdout[:220]!r} "
                f"err={sync.stderr[:220]!r}",
                file=sys.stderr,
                flush=True,
            )
        except Exception as exc:  # pragma: no cover - diagnostic only
            print(f"[litebox-probe] probe failed: {exc!r}", file=sys.stderr, flush=True)
        return result

    _sel.get_new_res = _wrapped
    print("[litebox-probe] get_new_res instrumented", file=sys.stderr, flush=True)


async def _skip(*_args, **_kwargs):
    return False


_DISABLED = ("set_dpi", "set_cursor_size", "_run_xrdb", "_run_xfconf", "_run_mate_gsettings")

for _modname in ("selkies.display_utils", "selkies.selkies"):
    try:
        _m = __import__(_modname, fromlist=["*"])
        for _n in _DISABLED:
            if hasattr(_m, _n):
                setattr(_m, _n, _skip)
    except Exception:
        pass

_probe_get_new_res()

try:
    import pulsectl

    def _no_pulse(self, *_a, **_k):
        raise pulsectl.PulseError("pulseaudio disabled for litebox bring-up (avoids autospawn fork)")

    pulsectl.Pulse.connect = _no_pulse
except Exception:
    pass

# Re-assert DISPLAY immediately before the screen probe: setting it once at import is not enough
# if anything clears it in between, and this is the one call whose failure silently kills video.
try:
    import selkies.selkies as _sk

    _orig_get_new_res = _sk.get_new_res

    async def _get_new_res(res_str):
        if not os.environ.get("DISPLAY"):
            os.environ["DISPLAY"] = _DISPLAY
        return await _orig_get_new_res(res_str)

    _sk.get_new_res = _get_new_res
except Exception:
    pass
