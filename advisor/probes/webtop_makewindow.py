"""Create and map one real X window, then hold it open.

The window census kept reporting `total=0` with a window manager supposedly running, which is
equally consistent with "the WM failed" and "no client can create a window at all". Those need
different fixes, so this settles it with the smallest possible client: XCreateSimpleWindow +
XMapWindow + XFlush, then idle so a census taken from another process can see it.
"""

import ctypes
import sys
import time

x = ctypes.CDLL("libX11.so.6")
x.XOpenDisplay.restype = ctypes.c_void_p
x.XOpenDisplay.argtypes = [ctypes.c_char_p]
x.XDefaultScreen.restype = ctypes.c_int
x.XDefaultScreen.argtypes = [ctypes.c_void_p]
x.XRootWindow.restype = ctypes.c_ulong
x.XRootWindow.argtypes = [ctypes.c_void_p, ctypes.c_int]
x.XWhitePixel.restype = ctypes.c_ulong
x.XWhitePixel.argtypes = [ctypes.c_void_p, ctypes.c_int]
x.XBlackPixel.restype = ctypes.c_ulong
x.XBlackPixel.argtypes = [ctypes.c_void_p, ctypes.c_int]
x.XCreateSimpleWindow.restype = ctypes.c_ulong
x.XCreateSimpleWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                                  ctypes.c_uint, ctypes.c_uint, ctypes.c_uint,
                                  ctypes.c_ulong, ctypes.c_ulong]
x.XMapWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x.XStoreName.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_char_p]
x.XFlush.argtypes = [ctypes.c_void_p]

d = x.XOpenDisplay(None)
if not d:
    sys.exit("MAKEWIN_FAIL: cannot open display")
scr = x.XDefaultScreen(d)
root = x.XRootWindow(d, scr)
win = x.XCreateSimpleWindow(d, root, 100, 100, 400, 300, 2,
                            x.XBlackPixel(d, scr), x.XWhitePixel(d, scr))
if not win:
    sys.exit("MAKEWIN_FAIL: XCreateSimpleWindow returned 0")
x.XStoreName(d, win, b"litebox-probe-window")
x.XMapWindow(d, win)
x.XFlush(d)
print(f"MAKEWIN_OK win=0x{win:x}", flush=True)
time.sleep(int(sys.argv[1]) if len(sys.argv) > 1 else 60)
