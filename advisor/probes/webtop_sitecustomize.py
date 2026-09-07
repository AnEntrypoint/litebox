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

_DISPLAY = os.environ.get("SELKIES_DISPLAY") or ":1"
if not os.environ.get("DISPLAY"):
    os.environ["DISPLAY"] = _DISPLAY


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
