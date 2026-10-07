// xproc_mutex_clone_probe.c -- verify the cross-process mutex protocol under the
// ACTUAL eventual use case: `RtlCloneUserProcess` (ADVISORY-002 Track B step 5),
// rather than the ordinary `CreateProcessW` used by xproc_mutex_probe.c.
//
// Why this is a separate, necessary probe rather than a variation:
//
//   * Under CreateProcessW, the child maps the section itself, by name.  Under
//     RtlCloneUserProcess, the child inherits the parent's ENTIRE address space
//     copy-on-write -- so the crucial question is whether a section view mapped
//     BEFORE the clone remains genuinely SHARED (not CoW-privatised) afterwards.
//     clone_probe.c established this for a SEC_RESERVE section; this probe
//     re-establishes it for the ordinary pagefile-backed section the mutex uses,
//     and, more importantly, verifies that the LOCK PROTOCOL ITSELF is correct
//     across that boundary under real multi-threaded contention.
//
//   * The named Event is inherited as a handle value too (clone replicates the whole
//     handle table with the SAME handle values), so the design's "no handle ever has
//     to cross the boundary" property holds here for a second, independent reason:
//     the child does not even need to re-open it by name.  This probe deliberately
//     uses the INHERITED handle, exercising exactly that path.
//
//   * clone_probe.c's one negative result -- keyed events do NOT rendezvous across a
//     clone boundary -- is what rules out the futex-style primitive.  This probe is
//     the positive counterpart: it shows a named Event DOES.
//
// CSRSS caution (ADVISORY-002 §2 "Risk notes"): a clone child must not touch
// console/user32/GDI/COM.  This child therefore does NOT printf; it reports solely
// by writing to an inherited pipe handle, exactly as clone_probe.c does.
//
// Build: x86_64-w64-mingw32-gcc -O2 -o xproc_mutex_clone_probe.exe xproc_mutex_clone_probe.c
//
// Throwaway diagnostic, per the project's no-test-files rule.

#include <windows.h>
#include <stdio.h>
#include <string.h>

typedef LONG NTSTATUS;
#define STATUS_PROCESS_CLONED ((NTSTATUS)0x00000129L)
#define RTL_CLONE_PROCESS_FLAGS_INHERIT_HANDLES 0x00000002

typedef struct {
    HANDLE UniqueProcess;
    HANDLE UniqueThread;
} CLIENT_ID_T;

typedef struct {
    ULONG Length;
    HANDLE ProcessHandle;
    HANDLE ThreadHandle;
    CLIENT_ID_T ClientId;
    // SECTION_IMAGE_INFORMATION is not declared by the mingw headers and its
    // contents are irrelevant here; only the total struct size must be right, since
    // ntdll validates `Length`. Opaque padding, generously sized.
    BYTE ImageInformation[64];
} RTL_USER_PROCESS_INFORMATION_T;

typedef NTSTATUS(NTAPI *RtlCloneUserProcess_t)(ULONG, PVOID, PVOID, PVOID,
                                               RTL_USER_PROCESS_INFORMATION_T *);
typedef NTSTATUS(NTAPI *NtTerminateProcess_t)(HANDLE, NTSTATUS);

#define ST_FREE 0u
#define ST_LOCKED 1u
#define ST_CONTENDED 2u

#define NTHREADS 8
#define NITERS 20000

typedef struct {
    volatile LONG state;
    volatile LONG64 counter;   // guarded by the lock, incremented NON-atomically
    volatile LONG occupancy;   // must be observed as exactly 1 by every holder
    volatile LONG excl_viol;
    volatile LONG64 slow_acq;
} shared_t;

static shared_t *g_shared;
static HANDLE g_event;
static HANDLE g_wr;          // inherited pipe write end, the child's only output
static int g_spin = 200;

static void say(const char *s) {
    DWORD n;
    WriteFile(g_wr, s, (DWORD)strlen(s), &n, NULL);
}

// Identical protocol to xproc_mutex_probe.c and to the Rust implementation in
// litebox_platform_windows_userland/src/xproc_sync.rs.
static void lock_acquire(void) {
    if (InterlockedCompareExchange(&g_shared->state, ST_LOCKED, ST_FREE) == ST_FREE) return;
    for (int s = 0; s < g_spin; s++) {
        YieldProcessor();
        if (InterlockedCompareExchange(&g_shared->state, ST_LOCKED, ST_FREE) == ST_FREE) return;
    }
    InterlockedIncrement64(&g_shared->slow_acq);
    for (;;) {
        if (InterlockedExchange(&g_shared->state, ST_CONTENDED) == ST_FREE) return;
        if (WaitForSingleObject(g_event, 20000) != WAIT_OBJECT_0) {
            say("FATAL: lock wait failed/timed out -- lost wakeup\n");
            return; // let the invariant check report the resulting corruption
        }
    }
}

static void lock_release(void) {
    if (InterlockedExchange(&g_shared->state, ST_FREE) == ST_CONTENDED) SetEvent(g_event);
}

static DWORD WINAPI worker(LPVOID a) {
    (void)a;
    for (int i = 0; i < NITERS; i++) {
        lock_acquire();
        if (InterlockedIncrement(&g_shared->occupancy) != 1)
            InterlockedIncrement(&g_shared->excl_viol);
        LONG64 v = g_shared->counter;
        g_shared->counter = v + 1;
        InterlockedDecrement(&g_shared->occupancy);
        lock_release();
    }
    return 0;
}

static void run_threads(void) {
    HANDLE th[NTHREADS];
    for (int i = 0; i < NTHREADS; i++) th[i] = CreateThread(NULL, 0, worker, NULL, 0, NULL);
    WaitForMultipleObjects(NTHREADS, th, TRUE, INFINITE);
    for (int i = 0; i < NTHREADS; i++) CloseHandle(th[i]);
}

int main(void) {
    HMODULE ntdll = GetModuleHandleA("ntdll.dll");
    RtlCloneUserProcess_t RtlCloneUserProcess =
        (RtlCloneUserProcess_t)GetProcAddress(ntdll, "RtlCloneUserProcess");
    NtTerminateProcess_t NtTerminateProcess =
        (NtTerminateProcess_t)GetProcAddress(ntdll, "NtTerminateProcess");
    if (!RtlCloneUserProcess) {
        printf("missing RtlCloneUserProcess\n");
        return 2;
    }

    // Ordinary pagefile-backed shared section -- exactly what the mutex lives in.
    // Mapped BEFORE the clone, which is the property under test.
    HANDLE sec = CreateFileMappingW(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0, 65536, NULL);
    if (!sec) { printf("CreateFileMapping err=%lu\n", GetLastError()); return 2; }
    g_shared = (shared_t *)MapViewOfFile(sec, FILE_MAP_ALL_ACCESS, 0, 0, 65536);
    if (!g_shared) { printf("MapViewOfFile err=%lu\n", GetLastError()); return 2; }
    memset((void *)g_shared, 0, sizeof(shared_t));

    SECURITY_ATTRIBUTES sa = {sizeof(sa), NULL, TRUE};
    HANDLE rd;
    if (!CreatePipe(&rd, &g_wr, &sa, 0)) { printf("CreatePipe failed\n"); return 2; }

    // The Event is created inheritable and is NOT named here: the clone replicates
    // the handle table with identical handle values, so the child uses the very same
    // HANDLE with no re-open. (The Rust implementation names it instead, which also
    // works under clone and additionally works under CreateProcessW.)
    g_event = CreateEventA(&sa, FALSE, FALSE, NULL);
    if (!g_event) { printf("CreateEvent err=%lu\n", GetLastError()); return 2; }

    printf("parent: shared section at %p, cloning...\n", (void *)g_shared);
    fflush(stdout);

    RTL_USER_PROCESS_INFORMATION_T pi;
    memset(&pi, 0, sizeof(pi));
    pi.Length = sizeof(pi);
    NTSTATUS st = RtlCloneUserProcess(RTL_CLONE_PROCESS_FLAGS_INHERIT_HANDLES, NULL, NULL, NULL, &pi);

    if (st == STATUS_PROCESS_CLONED) {
        // ---- CHILD ---- no printf, no console, no user32: pipe only.
        char m[160];
        snprintf(m, sizeof m, "child: shared section at %p (same VA as parent, by CoW clone)\n",
                 (void *)g_shared);
        say(m);
        run_threads();
        say("child: threads done\n");
        NtTerminateProcess((HANDLE)-1, 0);
        return 0;
    }
    if (st != 0) { printf("RtlCloneUserProcess failed 0x%lx\n", (unsigned long)st); return 2; }

    // ---- PARENT ----
    printf("parent: clone ok, child pid=%lu\n", (unsigned long)(ULONG_PTR)pi.ClientId.UniqueProcess);
    fflush(stdout);

    run_threads();

    DWORD w = WaitForSingleObject(pi.ProcessHandle, 60000);
    DWORD code = 1;
    GetExitCodeProcess(pi.ProcessHandle, &code);
    if (w != WAIT_OBJECT_0) {
        printf("parent: child did not exit in time, terminating\n");
        TerminateProcess(pi.ProcessHandle, 99);
    }

    CloseHandle(g_wr);
    char buf[1024];
    DWORD got = 0, total = 0;
    while (total < sizeof buf - 1 && ReadFile(rd, buf + total, sizeof buf - 1 - total, &got, NULL) && got)
        total += got;
    buf[total] = 0;
    printf("%s", buf);

    LONG64 expect = (LONG64)2 * NTHREADS * NITERS;
    printf("\n=== RESULT (RtlCloneUserProcess) ===\n");
    printf("child exit code : %lu\n", code);
    printf("counter         : %lld\n", g_shared->counter);
    printf("expected        : %lld\n", expect);
    printf("excl violations : %ld\n", g_shared->excl_viol);
    printf("slow (blocked)  : %lld\n", g_shared->slow_acq);

    int ok = (g_shared->counter == expect) && (g_shared->excl_viol == 0) && (code == 0);
    printf("VERDICT         : %s\n", ok ? "PASS" : "FAIL");
    return ok ? 0 : 1;
}
