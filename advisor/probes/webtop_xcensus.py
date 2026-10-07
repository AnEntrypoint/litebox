"""Census the X server's window tree and its client list, through libX11 via ctypes.

`xlsclients` is not in this image and `xwininfo` cannot be relied on either, so "is anything
actually connected to the display" was previously unanswerable -- stage 4 reported an empty
`mate.log` and a black screenshot, which is equally consistent with "mate-session never started"
and "mate-session is running fine and simply logs nothing". Those need different fixes, so
guessing between them is not an option.

This asks the server directly. `XQueryTree` on the root lists every child window that exists;
`XGetWindowAttributes` says which of them are actually mapped (i.e. would paint); WM_CLASS and
WM_NAME say what program each one belongs to. A session where mate-session is alive but its
components failed shows a handful of unmapped utility windows; a session where nothing ever
connected shows an empty tree. `XGetSelectionOwner` for the ICCCM manager selections then says
whether anything ever claimed to BE the window manager or the session manager, which is the
single most diagnostic fact about a desktop that will not appear.
"""

import ctypes
import sys

x = ctypes.CDLL("libX11.so.6")


class XWindowAttributes(ctypes.Structure):
    # Only the leading fields are named; the tail is padding to the real struct size so that
    # XGetWindowAttributes cannot overrun the buffer on any sane ABI.
    _fields_ = [
        ("x", ctypes.c_int), ("y", ctypes.c_int),
        ("width", ctypes.c_int), ("height", ctypes.c_int),
        ("border_width", ctypes.c_int), ("depth", ctypes.c_int),
        ("visual", ctypes.c_void_p), ("root", ctypes.c_ulong),
        ("c_class", ctypes.c_int),
        ("bit_gravity", ctypes.c_int), ("win_gravity", ctypes.c_int),
        ("backing_store", ctypes.c_int),
        ("backing_planes", ctypes.c_ulong), ("backing_pixel", ctypes.c_ulong),
        ("save_under", ctypes.c_int), ("colormap", ctypes.c_ulong),
        ("map_installed", ctypes.c_int), ("map_state", ctypes.c_int),
        ("_tail", ctypes.c_byte * 128),
    ]


x.XOpenDisplay.restype = ctypes.c_void_p
x.XOpenDisplay.argtypes = [ctypes.c_char_p]
x.XDefaultScreen.restype = ctypes.c_int
x.XDefaultScreen.argtypes = [ctypes.c_void_p]
x.XRootWindow.restype = ctypes.c_ulong
x.XRootWindow.argtypes = [ctypes.c_void_p, ctypes.c_int]
x.XQueryTree.argtypes = [ctypes.c_void_p, ctypes.c_ulong,
                         ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong),
                         ctypes.POINTER(ctypes.POINTER(ctypes.c_ulong)),
                         ctypes.POINTER(ctypes.c_uint)]
x.XGetWindowAttributes.argtypes = [ctypes.c_void_p, ctypes.c_ulong,
                                   ctypes.POINTER(XWindowAttributes)]
x.XFetchName.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_char_p)]
x.XInternAtom.restype = ctypes.c_ulong
x.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
x.XGetSelectionOwner.restype = ctypes.c_ulong
x.XGetSelectionOwner.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x.XGetWindowProperty.argtypes = [
    ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_long, ctypes.c_long,
    ctypes.c_int, ctypes.c_ulong,
    ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_int),
    ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong),
    ctypes.POINTER(ctypes.POINTER(ctypes.c_ubyte)),
]

IS_UNMAPPED, IS_VIEWABLE = 0, 2
ANY_PROPERTY_TYPE = 0

d = x.XOpenDisplay(None)
if not d:
    sys.exit("XCENSUS_FAIL: cannot open display")
scr = x.XDefaultScreen(d)
root = x.XRootWindow(d, scr)


def text_property(win, name):
    """Read a string property, tolerating its absence -- most windows have neither."""
    atom = x.XInternAtom(d, name.encode(), 1)
    if not atom:
        return ""
    actual_type = ctypes.c_ulong()
    actual_format = ctypes.c_int()
    nitems = ctypes.c_ulong()
    bytes_after = ctypes.c_ulong()
    data = ctypes.POINTER(ctypes.c_ubyte)()
    rc = x.XGetWindowProperty(d, win, atom, 0, 256, 0, ANY_PROPERTY_TYPE,
                              ctypes.byref(actual_type), ctypes.byref(actual_format),
                              ctypes.byref(nitems), ctypes.byref(bytes_after),
                              ctypes.byref(data))
    if rc != 0 or not data:
        return ""
    raw = bytes(bytearray(data[i] for i in range(nitems.value)))
    x.XFree(data)
    # WM_CLASS is two NUL-separated strings; join them so both instance and class are visible.
    return ".".join(p.decode("latin-1") for p in raw.split(b"\0") if p)


r_root = ctypes.c_ulong()
r_parent = ctypes.c_ulong()
children = ctypes.POINTER(ctypes.c_ulong)()
n = ctypes.c_uint()
ok = x.XQueryTree(d, root, ctypes.byref(r_root), ctypes.byref(r_parent),
                  ctypes.byref(children), ctypes.byref(n))
if not ok:
    sys.exit("XCENSUS_FAIL: XQueryTree failed")

mapped = 0
print("XCENSUS_WINDOWS total=%d" % n.value)
for i in range(n.value):
    w = children[i]
    attrs = XWindowAttributes()
    if not x.XGetWindowAttributes(d, w, ctypes.byref(attrs)):
        continue
    state = {IS_UNMAPPED: "unmapped", 1: "unviewable", IS_VIEWABLE: "VIEWABLE"}.get(
        attrs.map_state, str(attrs.map_state))
    if attrs.map_state == IS_VIEWABLE:
        mapped += 1
    name = ctypes.c_char_p()
    x.XFetchName(d, w, ctypes.byref(name))
    title = name.value.decode("latin-1") if name.value else ""
    if name.value:
        x.XFree(name)
    print("  win=0x%x %dx%d+%d+%d %s class=%r name=%r cmd=%r" % (
        w, attrs.width, attrs.height, attrs.x, attrs.y, state,
        text_property(w, "WM_CLASS"), title, text_property(w, "WM_COMMAND")))
print("XCENSUS_MAPPED %d of %d" % (mapped, n.value))

# The manager selections. A running window manager owns WM_S<screen>; a session manager
# advertises itself on the root's SM_CLIENT_ID / _MATE_SESSION properties. Zero owners with a
# non-empty window list means clients connected but no desktop was ever assembled.
for sel in ("WM_S%d" % scr, "MANAGER", "_NET_SYSTEM_TRAY_S%d" % scr):
    atom = x.XInternAtom(d, sel.encode(), 1)
    owner = x.XGetSelectionOwner(d, atom) if atom else 0
    print("XCENSUS_SELECTION %s owner=0x%x" % (sel, owner))
for prop in ("_NET_SUPPORTING_WM_CHECK", "_NET_WM_NAME", "_NET_CLIENT_LIST", "SM_CLIENT_ID"):
    print("XCENSUS_ROOTPROP %s=%r" % (prop, text_property(root, prop)))
