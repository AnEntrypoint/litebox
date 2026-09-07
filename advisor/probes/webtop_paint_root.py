"""Draw a real, moving scene on the X root window through libX11 via ctypes.

This image ships no xsetroot/xclock/xeyes, and xterm exits silently under litebox, so this is the
smallest dependable way to put genuine, CHANGING content on the display -- which is what makes a
browser screenshot meaningful evidence that the capture/encode/stream path carries actual
framebuffer pixels rather than merely running.
"""

import ctypes
import time

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
x.XCreateGC.restype = ctypes.c_void_p
x.XCreateGC.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_void_p]
x.XSetForeground.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_ulong]
x.XFillRectangle.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_void_p,
                             ctypes.c_int, ctypes.c_int, ctypes.c_uint, ctypes.c_uint]
x.XDrawRectangle.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_void_p,
                             ctypes.c_int, ctypes.c_int, ctypes.c_uint, ctypes.c_uint]
x.XDrawString.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_void_p,
                          ctypes.c_int, ctypes.c_int, ctypes.c_char_p, ctypes.c_int]
x.XLoadQueryFont.restype = ctypes.c_void_p
x.XLoadQueryFont.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
x.XSetFont.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_ulong]
x.XFlush.argtypes = [ctypes.c_void_p]

d = x.XOpenDisplay(None)
if not d:
    raise SystemExit("PAINT_FAIL: cannot open display")
scr = x.XDefaultScreen(d)
root = x.XRootWindow(d, scr)
W = x.XDisplayWidth(d, scr)
H = x.XDisplayHeight(d, scr)
gc = x.XCreateGC(d, root, 0, None)

# A bitmap font from the `misc` family, which the trimmed rootfs keeps precisely so text is
# possible here. Absence is not fatal -- the scene is still drawn, just without labels.
fid = 0
for name in (b"fixed", b"9x15", b"8x13", b"6x13"):
    f = x.XLoadQueryFont(d, name)
    if f:
        fid = ctypes.cast(f, ctypes.POINTER(ctypes.c_ulong))[0]
        x.XSetFont(d, gc, fid)
        break

print("PAINT_READY %dx%d font=%s" % (W, H, "yes" if fid else "no"), flush=True)

BG = 0x00101828
BAR = 0x001F6FEB
WHITE = 0x00FFFFFF
PALETTE = [0x00E5484D, 0x0030A46C, 0x00F5A524, 0x008E4EC6, 0x0000B5D8]

i = 0
while True:
    x.XSetForeground(d, gc, BG)
    x.XFillRectangle(d, root, gc, 0, 0, W, H)

    # Title bar
    x.XSetForeground(d, gc, BAR)
    x.XFillRectangle(d, root, gc, 0, 0, W, 48)
    if fid:
        x.XSetForeground(d, gc, WHITE)
        msg = b"litebox webtop -- Xvfb + selkies, streamed to the browser"
        x.XDrawString(d, root, gc, 16, 30, msg, len(msg))
        stamp = time.strftime("%H:%M:%S").encode()
        x.XDrawString(d, root, gc, W - 100, 30, stamp, len(stamp))

    # A row of tiles, one lit per second -- unmistakably live.
    for k in range(5):
        col = PALETTE[k] if k == (i % 5) else 0x00263043
        x.XSetForeground(d, gc, col)
        x.XFillRectangle(d, root, gc, 60 + k * 150, 120, 120, 120)

    # A box that walks across the screen.
    px = 60 + (i * 37) % max(1, W - 220)
    x.XSetForeground(d, gc, PALETTE[i % len(PALETTE)])
    x.XFillRectangle(d, root, gc, px, 320, 160, 90)
    x.XSetForeground(d, gc, WHITE)
    x.XDrawRectangle(d, root, gc, px, 320, 160, 90)
    if fid:
        lbl = ("frame %d" % i).encode()
        x.XDrawString(d, root, gc, px + 12, 370, lbl, len(lbl))

    x.XFlush(d)
    i += 1
    time.sleep(1)
