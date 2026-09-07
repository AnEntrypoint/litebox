"""Paint the X root window in changing colours, using libX11 directly through ctypes.

The image ships no xsetroot/xclock/xeyes, and xterm exits silently here, so this is the smallest
dependable way to put REAL, CHANGING pixels on the display -- which is what proves the webtop's
capture/encode/stream path is carrying actual framebuffer content rather than just running.
"""

import ctypes
import ctypes.util
import time

x = ctypes.CDLL("libX11.so.6")
x.XOpenDisplay.restype = ctypes.c_void_p
x.XOpenDisplay.argtypes = [ctypes.c_char_p]
x.XDefaultScreen.restype = ctypes.c_int
x.XDefaultScreen.argtypes = [ctypes.c_void_p]
x.XRootWindow.restype = ctypes.c_ulong
x.XRootWindow.argtypes = [ctypes.c_void_p, ctypes.c_int]
x.XSetWindowBackground.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong]
x.XClearWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x.XFlush.argtypes = [ctypes.c_void_p]

d = x.XOpenDisplay(None)
if not d:
    raise SystemExit("PAINT_FAIL: cannot open display")
scr = x.XDefaultScreen(d)
root = x.XRootWindow(d, scr)
print("PAINT_READY root=%#x" % root, flush=True)

colours = [0x00FF0000, 0x0000FF00, 0x000000FF, 0x00FFFF00, 0x00FF00FF, 0x0000FFFF]
i = 0
while True:
    x.XSetWindowBackground(d, root, colours[i % len(colours)])
    x.XClearWindow(d, root)
    x.XFlush(d)
    i += 1
    time.sleep(1)
