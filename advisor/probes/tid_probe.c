// Guest-side probe (Linux/musl): is the per-thread tid plumbing correct?
// A musl pthread_mutex stores self->tid in the lock word's low 30 bits, so a thread whose
// tid field is 0 produces a lock word of exactly 0x80000000 once another thread waits on it,
// which is the signature seen in xfce4-session's hang.
// Build in the guest: cc -O1 -o tid_probe tid_probe.c -lpthread   (musl: -lpthread is a no-op)
// Exit code = number of failed checks.
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <pthread.h>
#include <sys/syscall.h>

static int fails = 0;
#define CHECK(cond, ...) do { if (cond) printf("ok   " __VA_ARGS__); else { fails++; printf("FAIL " __VA_ARGS__); } printf("\n"); } while (0)

// musl x86_64 struct pthread: self, dtv, prev, next, sysinfo, canary, then tid.
// Offset 0x30 holds `int tid` on musl 1.2.x/x86_64. Verified against pthread_impl.h field order.
#define MUSL_TID_OFF 0x30
static int musl_self_tid(void) { return *(int *)((char *)pthread_self() + MUSL_TID_OFF); }

static void report(const char *who) {
    int kernel_tid = (int)syscall(SYS_gettid);
    int self_tid = musl_self_tid();
    printf("%-8s pid=%d gettid=%d pthread_self.tid=%d\n", who, getpid(), kernel_tid, self_tid);
    CHECK(kernel_tid > 0, "%s: gettid() is nonzero", who);
    CHECK(self_tid > 0, "%s: musl pthread_self()->tid is nonzero", who);
    CHECK(self_tid == kernel_tid, "%s: pthread_self()->tid == gettid() (%d vs %d)", who, self_tid, kernel_tid);
}

// Lock an errorcheck mutex (owner-tracking) and show the raw lock word.
static void lockword(const char *who) {
    pthread_mutex_t m;
    pthread_mutexattr_t a;
    pthread_mutexattr_init(&a);
    pthread_mutexattr_settype(&a, PTHREAD_MUTEX_ERRORCHECK);
    pthread_mutex_init(&m, &a);
    pthread_mutex_lock(&m);
    int word = ((int *)&m)[1];              // musl pthread_mutex_t: _m_type then _m_lock
    int owner = word & 0x3fffffff;
    printf("%-8s errorcheck mutex locked: word=0x%08x owner_tid=%d\n", who, (unsigned)word, owner);
    CHECK(owner == (int)syscall(SYS_gettid), "%s: lock word records this thread's tid", who);
    CHECK(pthread_mutex_unlock(&m) == 0, "%s: unlock succeeds (no EPERM: owner matched)", who);
}

static void *child(void *arg) { (void)arg; report("child"); lockword("child"); return NULL; }

static pthread_mutex_t shared;

// Contender: blocks on a mutex main holds, so main's unlock must wake it. A hang here is the
// same failure mode as xfce4-session's (a waiter that is never woken).
static void *contend(void *p) {
    (void)p;
    pthread_mutex_lock(&shared);
    printf("child2   acquired contended mutex, word=0x%08x\n", (unsigned)((int *)&shared)[1]);
    pthread_mutex_unlock(&shared);
    return NULL;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int x = 0;
    int r = (int)syscall(SYS_set_tid_address, &x);
    printf("main     set_tid_address returned %d (should equal gettid)\n", r);
    CHECK(r == (int)syscall(SYS_gettid), "main: set_tid_address returns this tid");
    report("main");
    lockword("main");

    pthread_t t;
    CHECK(pthread_create(&t, NULL, child, NULL) == 0, "pthread_create");
    pthread_join(t, NULL);

    // Cross-thread contention: main holds the lock while a second thread blocks on it, so main's
    // unlock must issue a FUTEX_WAKE that reaches the waiter. A hang here reproduces
    // xfce4-session's failure in 20 lines instead of a 940k-line trace.
    pthread_mutexattr_t a2; pthread_mutexattr_init(&a2);
    pthread_mutexattr_settype(&a2, PTHREAD_MUTEX_ERRORCHECK);
    pthread_mutex_init(&shared, &a2);
    pthread_mutex_lock(&shared);
    pthread_t t2;
    CHECK(pthread_create(&t2, NULL, contend, NULL) == 0, "pthread_create (contender)");
    sleep(1);
    printf("main     contended word before unlock=0x%08x (waiters bit expected)\n", (unsigned)((int *)&shared)[1]);
    pthread_mutex_unlock(&shared);
    pthread_join(t2, NULL);
    CHECK(1, "contended lock/unlock handshake completed (no deadlock)");

    printf("%d check(s) failed\n", fails);
    return fails;
}
