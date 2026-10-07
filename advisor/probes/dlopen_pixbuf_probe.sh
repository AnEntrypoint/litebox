#!/bin/sh
# Does gdk-pixbuf's loader module actually LOAD under litebox?
#
# The evidence trail (advisor/probes/headless-evidence/pixbuf-findings.txt) ends at a precise,
# unexplained state: a valid loaders.cache is opened and read IN FULL, then NO openat of
# libpixbufloader-xpm.so (or of any loaders/ path) ever happens, and every image format is
# reported "unrecognized". Three theories were already retired with evidence -- missing cache
# (generated one, no change), missing PNG loader (XPM has its .so and fails identically), and the
# whole glycin/bwrap/sandbox path (no decoder of any kind ever runs). A fourth, membarrier, is
# also dead: that fix landed at 07:21 and the failing evidence was gathered at 11:21, after it.
#
# So the open question is whether this is a gdk-pixbuf logic bug or a litebox one, and the
# decisive test is not another gdk-pixbuf invocation -- it is whether dlopen of that exact .so
# succeeds AT ALL here. gdk-pixbuf loads modules through gmodule, which is dlopen underneath. A
# silently failing dlopen would produce exactly this symptom for every format at once.
#
# This asks that question directly, three ways, so the answer does not depend on gdk-pixbuf's
# own behaviour:
#   A. dlopen the loader .so by absolute path and report dlerror() verbatim on failure.
#   B. If it opens, dlsym the two entry points gdk-pixbuf itself looks up. A handle that opens
#      but has no symbols is a different bug from one that will not open.
#   C. dlopen an unrelated, definitely-present library as a CONTROL. If the control also fails,
#      dlopen is broken generally and this was never a gdk-pixbuf problem at all; if only the
#      loader fails, the problem is specific to that module.
#
# Run as the runner's TOP-LEVEL program from the XFCE layer (never via `sh -c "..."`; see
# [[isolate-harness-before-blaming-litebox]] -- a runtime `sh -c` wrapper has twice produced
# false "litebox is broken" conclusions).
#
# Reading the result:
#   DLOPEN_LOADER=ok + SYM_*=ok  -> module loads fine; the defect is in gdk-pixbuf's own
#                                   cache handling, NOT litebox. Stop looking at the loader path.
#   DLOPEN_LOADER=FAIL           -> the error string names the real cause; this is very likely a
#                                   litebox loader/relocation gap and is the thing to fix.
#   DLOPEN_CONTROL=FAIL too      -> dlopen is broken generally; far bigger than pixbuf.

echo P_START
export HOME=/root
mkdir -p /root

D=/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders
echo "P_LOADER_DIR_LISTING:"
ls -la "$D" 2>&1 | head -10

# Build a tiny C probe in the guest ONLY if a compiler exists; both guest compilers are known
# broken here, so the python fallback below is the expected path, not a contingency.
LOADER="$D/libpixbufloader-xpm.so"
echo "P_LOADER_PATH=$LOADER"
echo "P_LOADER_EXISTS=$([ -f "$LOADER" ] && echo yes || echo no)"

# python3 + ctypes gives real dlopen/dlsym without needing a working compiler.
if command -v python3 >/dev/null 2>&1; then
    echo "P_METHOD=python3-ctypes"
    python3 - "$LOADER" <<'PY'
import ctypes, ctypes.util, sys
loader = sys.argv[1]

def try_dlopen(path, label):
    try:
        h = ctypes.CDLL(path, mode=ctypes.RTLD_NOW)
        print("DLOPEN_%s=ok handle=%s" % (label, bool(h._handle)))
        return h
    except OSError as e:
        # The dlerror() text is the whole point -- it names the missing symbol/file/relocation.
        print("DLOPEN_%s=FAIL err=%s" % (label, e))
        return None

h = try_dlopen(loader, "LOADER")
if h is not None:
    # These are the exact entry points gdk-pixbuf looks up in a loader module.
    for sym in ("fill_vtable", "fill_info"):
        try:
            getattr(h, sym)
            print("SYM_%s=ok" % sym)
        except AttributeError as e:
            print("SYM_%s=MISSING err=%s" % (sym, e))

# CONTROL: an unrelated library that is definitely present and definitely loadable.
# If this fails too, dlopen itself is broken and pixbuf was never the real subject.
try_dlopen("libz.so.1", "CONTROL")
PY
else
    echo "P_METHOD=none  (python3 absent -- cannot run the decisive dlopen test)"
fi

echo P_DONE
