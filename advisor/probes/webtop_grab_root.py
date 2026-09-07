"""Grab the X root window and print it as coarse ASCII.

Verifying that a desktop actually PAINTED normally means running the whole capture/encode/stream
stack and looking at a browser -- which on a memory-tight host competes with the very session
being tested. This does the same job for a fraction of the cost: one XGetImage of the root window,
downsampled to a character grid, printed to stdout. It answers "did anything draw, and roughly
what shape is it" without selkies, a proxy, or a browser being involved at all.
"""

import ctypes
import sys

x = ctypes.CDLL("libX11.so.6")
x.XOpenDisplay.restype = ctypes.c_void_p
x.XOpenDisplay.argtypes = [ctypes.c_char_p]
x.XDefaultScreen.restype = ctypes.c_int
x.XDefaultScreen.argtypes = [ctypes.c_void_p]
x.XRootWindow.restype = ctypes.c_ulong
x.XRootWindow.argtypes = [ctypes.c_void_p, ctypes.c_int]
x.XDisplayWidth.restype = ctypes.c_int
x.XDisplayHeight.restype = ctypes.c_int
x.XDisplayWidth.argtypes = [ctypes.c_void_p, ctypes.c_int]
x.XDisplayHeight.argtypes = [ctypes.c_void_p, ctypes.c_int]
x.XGetImage.restype = ctypes.c_void_p
x.XGetImage.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                        ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_int]
x.XGetPixel.restype = ctypes.c_ulong
x.XGetPixel.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int]

ZPIXMAP = 2
ALLPLANES = 0xFFFFFFFF

d = x.XOpenDisplay(None)
if not d:
    sys.exit("GRAB_FAIL: cannot open display")
scr = x.XDefaultScreen(d)
root = x.XRootWindow(d, scr)
W = x.XDisplayWidth(d, scr)
H = x.XDisplayHeight(d, scr)

img = x.XGetImage(d, root, 0, 0, W, H, ALLPLANES, ZPIXMAP)
if not img:
    sys.exit("GRAB_FAIL: XGetImage returned NULL")

COLS, ROWS = 78, 26
RAMP = " .:-=+*#%@"
nonblack = 0
total = 0
rows = []
for r in range(ROWS):
    line = []
    for c in range(COLS):
        px = x.XGetPixel(img, (c * W) // COLS, (r * H) // ROWS)
        rr, gg, bb = (px >> 16) & 0xFF, (px >> 8) & 0xFF, px & 0xFF
        lum = (rr * 299 + gg * 587 + bb * 114) // 1000
        total += 1
        if lum > 8:
            nonblack += 1
        line.append(RAMP[min(len(RAMP) - 1, lum * len(RAMP) // 256)])
    rows.append("".join(line))

print("GRAB_OK %dx%d nonblack=%d/%d (%.1f%%)" % (W, H, nonblack, total, 100.0 * nonblack / total),
      flush=True)
for line in rows:
    print("|" + line + "|", flush=True)
