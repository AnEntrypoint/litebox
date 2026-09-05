// Does dlopen work under litebox, and specifically does gdk-pixbuf's loader module load?
//
// The pixbuf evidence trail (advisor/probes/headless-evidence/pixbuf-findings.txt) ends at a
// precise, unexplained state: a valid loaders.cache is opened and read IN FULL, then NO openat of
// libpixbufloader-xpm.so ever happens and every image format is reported "unrecognized". Four
// theories are already retired with evidence -- missing cache (generated one, no change), missing
// PNG loader (XPM ships its .so and fails identically), the whole glycin/bwrap/sandbox path (no
// decoder of any kind ever runs), and membarrier (that fix landed at 07:21; the failing evidence
// was gathered at 11:21, after it).
//
// The remaining fork in the road is whether this is a gdk-pixbuf logic bug or a litebox one. The
// decisive test is NOT another gdk-pixbuf invocation -- it is whether dlopen of that exact .so
// succeeds at all here. gdk-pixbuf loads modules through gmodule, which is dlopen underneath, and
// a silently failing dlopen produces exactly this symptom for every format at once.
//
// Answers it three ways so the result does not depend on gdk-pixbuf's own behaviour:
//   A. dlopen the loader by absolute path; print dlerror() verbatim on failure.
//   B. If it opens, dlsym the two entry points gdk-pixbuf itself looks up. A handle that opens
//      but resolves no symbols is a different bug from one that will not open.
//   C. dlopen an unrelated, definitely-present library as a CONTROL. If the control fails too,
//      dlopen is broken generally and pixbuf was never the real subject.
//
// Build ON THE HOST against musl (dynamic -- the whole point is exercising the dynamic loader):
//   zig cc -target x86_64-linux-musl -o dlopen_probe dlopen_probe.c
// or with a musl cross-toolchain:
//   x86_64-linux-musl-gcc -o dlopen_probe dlopen_probe.c
//
// Run as the runner's TOP-LEVEL program from the XFCE layer (never via `sh -c "..."` -- see
// [[isolate-harness-before-blaming-litebox]]; a runtime sh wrapper has twice produced false
// "litebox is broken" conclusions).
//
// Reading the result:
//   DLOPEN_LOADER=ok + SYM_*=ok -> the module loads fine; the defect is in gdk-pixbuf's own cache
//                                  handling, NOT litebox. Stop looking at the loader path.
//   DLOPEN_LOADER=FAIL          -> the error string names the real cause; very likely a litebox
//                                  dynamic-loader gap, and the thing to fix.
//   DLOPEN_CONTROL=FAIL too     -> dlopen is broken generally; far bigger than pixbuf.

#include <dlfcn.h>
#include <stdio.h>

static void *try_dlopen(const char *path, const char *label) {
    // Clear any stale error first: dlerror() is only meaningful immediately after a failed call.
    dlerror();
    void *h = dlopen(path, RTLD_NOW);
    if (h) {
        printf("DLOPEN_%s=ok handle=%p path=%s\n", label, h, path);
    } else {
        const char *e = dlerror();
        // The dlerror() text is the whole point -- it names the missing file/symbol/relocation.
        printf("DLOPEN_%s=FAIL path=%s err=%s\n", label, path, e ? e : "(no dlerror text)");
    }
    fflush(stdout);
    return h;
}

static void try_sym(void *h, const char *sym) {
    dlerror();
    void *p = dlsym(h, sym);
    const char *e = dlerror();
    if (p && !e) {
        printf("SYM_%s=ok addr=%p\n", sym, p);
    } else {
        printf("SYM_%s=MISSING err=%s\n", sym, e ? e : "(null symbol, no error)");
    }
    fflush(stdout);
}

int main(void) {
    printf("DLOPEN_PROBE_START\n");
    fflush(stdout);

    const char *loader =
        "/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders/libpixbufloader-xpm.so";

    void *h = try_dlopen(loader, "LOADER");
    if (h) {
        // The exact entry points gdk-pixbuf looks up in a loader module.
        try_sym(h, "fill_vtable");
        try_sym(h, "fill_info");
    }

    // CONTROL: definitely present, definitely loadable. Distinguishes "this module is special"
    // from "dlopen is broken here".
    try_dlopen("libz.so.1", "CONTROL");
    // A second control that is a real GNOME-stack library, closer in shape to the loader itself
    // (many DT_NEEDED entries) than libz is.
    try_dlopen("libgdk_pixbuf-2.0.so.0", "CONTROL2");

    printf("DLOPEN_PROBE_DONE\n");
    fflush(stdout);
    return 0;
}
