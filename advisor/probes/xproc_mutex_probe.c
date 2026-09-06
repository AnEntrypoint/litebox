// xproc_mutex_probe.c -- live verification of a cross-process mutex for litebox
// (ADVISORY-002 Track B step 2).
//
// Establishes, by direct measurement on this host, the facts the Rust
// implementation in litebox_platform_windows_userland/src/xproc_sync.rs depends on:
//
//   1. A pagefile-backed section mapped into TWO REAL, SEPARATE Windows processes
//      lands at DIFFERENT virtual addresses in each, so nothing in the shared word
//      may be keyed by its own address.  (Directly relevant: this is the same class
//      of bug as glibc safe-linking, ADVISORY-001 3N.)
//   2. A NAMED auto-reset Event DOES rendezvous across a real process boundary --
//      unlike keyed events, which clone_probe.c proved do NOT (both sides time out).
//   3. A hybrid lock -- atomic CAS on a word inside the shared section for the
//      uncontended fast path, falling back to the Event only when contended --
//      preserves mutual exclusion under heavy real contention from many threads in
//      BOTH processes.
//   4. Rough uncontended and contended acquire/release costs.
//
// Invariant checked: each of (NPROC * NTHREADS) threads performs NITERS
// increments of a single shared counter under the lock.  The final value must be
// EXACTLY NPROC * NTHREADS * NITERS.  A lost update, a torn word, or a missed
// wakeup all show up as a wrong final count or a hang.
//
// A second, independent invariant is checked at the same time: a "critical section
// occupancy" word is incremented on entry and decremented on exit, and every holder
// asserts it observed exactly 1.  This catches genuine mutual-exclusion violations
// that a counter-only check could mask (two threads racing on the counter can still
// produce the right total if their increments happen not to interleave).
//
// Build:  gcc -O2 -o xproc_mutex_probe.exe xproc_mutex_probe.c
// Run:    ./xproc_mutex_probe.exe            (parent; spawns the child itself)
//
// Throwaway diagnostic, per this project's no-test-files rule.  Not a crate test.

#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// Tunable at runtime so the same binary can measure both the uncontended fast
// path and a deliberately Event-heavy contended regime.  `LITEBOX_XPM_SPIN=0`
// disables the adaptive spin entirely, forcing essentially every contended
// acquire through the kernel Event -- which is the path a lost wakeup would
// actually break, and which the default (spin-resolved) regime barely exercises.
static int g_nthreads = 8;
static int g_niters   = 20000;
static int g_spin     = 200;
static int g_hold     = 0;   // artificial critical-section length, in pause ops

#define MAX_THREADS 64
#define NPROC    2

#define SEC_NAME "Local\\litebox_xpm_probe_section"
#define EVT_NAME "Local\\litebox_xpm_probe_event"
#define RDY_NAME "Local\\litebox_xpm_probe_ready"

// Lock states.  Deliberately the classic 3-state futex encoding, because it is the
// one that has no lost-wakeup window: a waiter that transitions FREE->CONTENDED (or
// LOCKED->CONTENDED) *before* sleeping guarantees the releaser, which reads the word
// with a single atomic exchange, sees CONTENDED and signals.
#define ST_FREE      0u
#define ST_LOCKED    1u
#define ST_CONTENDED 2u

typedef struct {
    volatile LONG state;        // ST_FREE / ST_LOCKED / ST_CONTENDED
    volatile LONG64 counter;    // the invariant under test
    volatile LONG occupancy;    // must be exactly 1 while a holder is inside
    volatile LONG excl_viol;    // count of observed mutual-exclusion violations
    volatile LONG64 slow_acq;   // acquires that had to block on the Event
    volatile LONG64 fast_acq;   // acquires won by the uncontended CAS
} shared_t;

static shared_t *g_shared;
static HANDLE    g_event;

// ---------------------------------------------------------------------------
// The lock itself.  This is the exact protocol the Rust implementation mirrors.
//
// Ordering rationale (x86-64 and the Interlocked* API both give full fences, but
// the protocol is written to be correct under a weaker model too):
//   - acquire uses a compare-exchange with acquire semantics on success, so the
//     critical section's loads cannot be hoisted above it.
//   - release uses an exchange with release semantics, so the critical section's
//     stores are visible before the word becomes observable as FREE.
// ---------------------------------------------------------------------------

static void lock_acquire(void)
{
    // Fast path: FREE -> LOCKED.  One atomic, no kernel transition.
    if (InterlockedCompareExchange(&g_shared->state, ST_LOCKED, ST_FREE) == ST_FREE) {
        InterlockedIncrement64(&g_shared->fast_acq);
        return;
    }

    // Brief adaptive spin before paying for the kernel object.  Real contention on
    // a short critical section is usually resolved here.
    for (int spin = 0; spin < g_spin; spin++) {
        YieldProcessor();
        if (InterlockedCompareExchange(&g_shared->state, ST_LOCKED, ST_FREE) == ST_FREE) {
            InterlockedIncrement64(&g_shared->fast_acq);
            return;
        }
    }

    InterlockedIncrement64(&g_shared->slow_acq);

    // Slow path.  Mark CONTENDED and sleep.  The unconditional exchange (rather
    // than a CAS from LOCKED) is what closes the lost-wakeup window: whatever the
    // previous state was, after this the word is CONTENDED, so a releaser running
    // concurrently *must* observe CONTENDED and signal.  If the exchange returns
    // ST_FREE we in fact just acquired the lock (leaving it marked CONTENDED, which
    // costs one spurious signal on release and is always safe).
    for (;;) {
        if (InterlockedExchange(&g_shared->state, ST_CONTENDED) == ST_FREE) {
            return;
        }
        DWORD w = WaitForSingleObject(g_event, 10000);
        if (w == WAIT_TIMEOUT) {
            // A timeout here would indicate a genuine lost wakeup.  Report loudly
            // rather than silently retrying, per the no-masking rule.
            fprintf(stderr, "FATAL: lock wait timed out -- lost wakeup (state=%ld)\n",
                    g_shared->state);
            fflush(stderr);
            ExitProcess(90);
        }
        if (w != WAIT_OBJECT_0) {
            fprintf(stderr, "FATAL: WaitForSingleObject -> %lu err=%lu\n", w, GetLastError());
            fflush(stderr);
            ExitProcess(91);
        }
    }
}

static void lock_release(void)
{
    // Single exchange reads the old state and frees the lock atomically.  Reading
    // and then storing separately would reintroduce the lost-wakeup window.
    if (InterlockedExchange(&g_shared->state, ST_FREE) == ST_CONTENDED) {
        if (!SetEvent(g_event)) {
            fprintf(stderr, "FATAL: SetEvent err=%lu\n", GetLastError());
            fflush(stderr);
            ExitProcess(92);
        }
    }
}

// ---------------------------------------------------------------------------

static DWORD WINAPI worker(LPVOID arg)
{
    (void)arg;
    for (int i = 0; i < g_niters; i++) {
        lock_acquire();

        // Mutual-exclusion witness.  If any other thread (in either process) is
        // inside at the same time, occupancy exceeds 1 and we record a violation.
        if (InterlockedIncrement(&g_shared->occupancy) != 1) {
            InterlockedIncrement(&g_shared->excl_viol);
        }

        // Non-atomic read-modify-write, deliberately.  This is the whole point:
        // it is correct ONLY if the lock genuinely excludes across processes.
        LONG64 v = g_shared->counter;
        v = v + 1;
        g_shared->counter = v;

        // Artificially lengthen the critical section when asked, so waiters
        // genuinely pile up on the Event rather than resolving in the spin.
        for (int h = 0; h < g_hold; h++) YieldProcessor();

        InterlockedDecrement(&g_shared->occupancy);

        lock_release();
    }
    return 0;
}

static void run_threads(void)
{
    HANDLE th[MAX_THREADS];
    for (int i = 0; i < g_nthreads; i++) {
        th[i] = CreateThread(NULL, 0, worker, NULL, 0, NULL);
        if (!th[i]) {
            fprintf(stderr, "CreateThread failed err=%lu\n", GetLastError());
            ExitProcess(3);
        }
    }
    WaitForMultipleObjects((DWORD)g_nthreads, th, TRUE, INFINITE);
    for (int i = 0; i < g_nthreads; i++) CloseHandle(th[i]);
}

static HANDLE open_shared(int is_child, HANDLE *out_map)
{
    HANDLE map;
    if (is_child) {
        map = OpenFileMappingA(FILE_MAP_ALL_ACCESS, FALSE, SEC_NAME);
        if (!map) {
            fprintf(stderr, "child: OpenFileMapping err=%lu\n", GetLastError());
            ExitProcess(4);
        }
    } else {
        map = CreateFileMappingA(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE,
                                 0, 65536, SEC_NAME);
        if (!map) {
            fprintf(stderr, "parent: CreateFileMapping err=%lu\n", GetLastError());
            ExitProcess(4);
        }
    }
    *out_map = map;
    void *view = MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, 65536);
    if (!view) {
        fprintf(stderr, "MapViewOfFile err=%lu\n", GetLastError());
        ExitProcess(5);
    }
    g_shared = (shared_t *)view;
    return map;
}

int main(int argc, char **argv)
{
    int is_child = (argc > 1 && strcmp(argv[1], "child") == 0);

    // Config via environment so parent and child agree with no extra IPC -- the
    // child inherits the environment from CreateProcess automatically.
    {
        const char *e;
        if ((e = getenv("LITEBOX_XPM_THREADS"))) g_nthreads = atoi(e);
        if ((e = getenv("LITEBOX_XPM_ITERS")))   g_niters   = atoi(e);
        if ((e = getenv("LITEBOX_XPM_SPIN")))    g_spin     = atoi(e);
        if ((e = getenv("LITEBOX_XPM_HOLD")))    g_hold     = atoi(e);
        if (g_nthreads < 1 || g_nthreads > MAX_THREADS) g_nthreads = 8;
        if (g_niters < 1) g_niters = 20000;
        if (g_spin < 0) g_spin = 0;
        if (g_hold < 0) g_hold = 0;
    }

    HANDLE map;
    open_shared(is_child, &map);

    // FACT 1: report the mapped address in each process.  These are expected to
    // differ, which is exactly why the shared word must be address-agnostic.
    printf("%s: shared view mapped at %p (pid %lu)\n",
           is_child ? "child " : "parent", (void *)g_shared, GetCurrentProcessId());
    fflush(stdout);

    // FACT 2: a NAMED auto-reset Event, opened independently by each process.
    // No handle duplication, no inheritance -- the kernel object is found by name.
    // Auto-reset (bManualReset = FALSE), initially non-signaled.
    g_event = CreateEventA(NULL, FALSE, FALSE, EVT_NAME);
    if (!g_event) {
        fprintf(stderr, "CreateEvent err=%lu\n", GetLastError());
        return 6;
    }

    HANDLE ready = CreateEventA(NULL, TRUE, FALSE, RDY_NAME); // manual-reset barrier
    if (!ready) {
        fprintf(stderr, "CreateEvent(ready) err=%lu\n", GetLastError());
        return 6;
    }

    PROCESS_INFORMATION pi;
    memset(&pi, 0, sizeof(pi));

    if (!is_child) {
        memset((void *)g_shared, 0, sizeof(shared_t));

        // Spawn a genuinely separate Windows process via ordinary CreateProcessW.
        char cmd[MAX_PATH * 2];
        snprintf(cmd, sizeof(cmd), "\"%s\" child", argv[0]);
        STARTUPINFOA si;
        memset(&si, 0, sizeof(si));
        si.cb = sizeof(si);
        if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi)) {
            fprintf(stderr, "CreateProcess err=%lu\n", GetLastError());
            return 7;
        }
        // Let the child map and open before releasing both into the hammer.
        Sleep(300);
        SetEvent(ready);
    } else {
        if (WaitForSingleObject(ready, 30000) != WAIT_OBJECT_0) {
            fprintf(stderr, "child: ready barrier failed\n");
            return 8;
        }
    }

    LARGE_INTEGER freq, t0, t1;
    QueryPerformanceFrequency(&freq);
    QueryPerformanceCounter(&t0);

    run_threads();

    QueryPerformanceCounter(&t1);
    double secs = (double)(t1.QuadPart - t0.QuadPart) / (double)freq.QuadPart;
    long long ops = (long long)g_nthreads * g_niters;
    printf("%s: %lld acquire/release pairs in %.3f s = %.0f ns/pair (contended, 2 procs x %d thr)\n",
           is_child ? "child " : "parent", ops, secs, secs * 1e9 / (double)ops, g_nthreads);
    fflush(stdout);

    if (!is_child) {
        WaitForSingleObject(pi.hProcess, 120000);
        DWORD code = 1;
        GetExitCodeProcess(pi.hProcess, &code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);

        LONG64 expect = (LONG64)NPROC * g_nthreads * g_niters;
        printf("\n=== RESULT === (threads=%d iters=%d spin=%d hold=%d)\n",
               g_nthreads, g_niters, g_spin, g_hold);
        printf("child exit code : %lu\n", code);
        printf("counter         : %lld\n", g_shared->counter);
        printf("expected        : %lld\n", expect);
        printf("excl violations : %ld\n", g_shared->excl_viol);
        printf("fast acquires   : %lld\n", g_shared->fast_acq);
        printf("slow (blocked)  : %lld\n", g_shared->slow_acq);

        int ok = (g_shared->counter == expect) && (g_shared->excl_viol == 0) && (code == 0);
        printf("VERDICT         : %s\n", ok ? "PASS" : "FAIL");
        fflush(stdout);

        CloseHandle(g_event);
        CloseHandle(ready);
        UnmapViewOfFile((void *)g_shared);
        CloseHandle(map);
        return ok ? 0 : 1;
    }

    CloseHandle(g_event);
    CloseHandle(ready);
    UnmapViewOfFile((void *)g_shared);
    CloseHandle(map);
    return 0;
}
