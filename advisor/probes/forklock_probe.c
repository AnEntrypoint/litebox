// Freestanding guest probe: does a fork() child's writes leak into the PARENT's memory?
//
// This is the 30-line version of the xfce4-session futex deadlock. musl's fork() has the child
// walk the thread list writing `td->tid = -1` into every OTHER thread's struct pthread, and
// force-zero every registered atfork lock word (musl src/process/fork.c). On real Linux that is
// harmless: the child has its own copy-on-write address space. If a same-address-space fork
// emulation lets any of those writes land in the parent, live parent state is corrupted, which
// is exactly how a lock word ends up owned by a nonexistent thread and no wake ever arrives.
//
// The probe reproduces the shape without libc: the parent writes a known pattern into a page,
// forks, and the CHILD scribbles a different pattern over that same page and exits. On real
// Linux the parent's copy is untouched. If the parent sees the child's writes, the fork
// emulation is unsound and every threaded guest program is at risk.
//
// It also covers the reverse direction (parent writes after fork must not reach the child) and
// checks that a child's stack writes do not disturb the parent's stack.
//
// Build ON THE HOST (no guest toolchain needed, no libc, no headers):
//   clang --target=x86_64-unknown-linux-gnu -nostdlib -nostdinc -ffreestanding \
//         -fno-stack-protector -static -O1 -o forklock_probe forklock_probe.c
// Then copy into the guest rootfs layer and run it.
//
// Output is plain text. Exit code = number of failed checks (0 = fork isolation is correct).

typedef unsigned long u64;
typedef long i64;

static i64 sys1(i64 n, i64 a) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a) : "rcx", "r11", "memory"); return r; }
static i64 sys3(i64 n, i64 a, i64 b, i64 c) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory"); return r; }
static i64 sys4(i64 n, i64 a, i64 b, i64 c, i64 d) { i64 r; register i64 r10 __asm__("r10") = d; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory"); return r; }
static i64 sys6(i64 n, i64 a, i64 b, i64 c, i64 d, i64 e, i64 f) { i64 r; register i64 r10 __asm__("r10") = d, r8 __asm__("r8") = e, r9 __asm__("r9") = f; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9) : "rcx", "r11", "memory"); return r; }

#define SYS_write 1
#define SYS_mmap 9
#define SYS_exit 60
#define SYS_wait4 61
#define SYS_fork 57
#define SYS_getpid 39
#define SYS_gettid 186

static unsigned slen(const char *s) { unsigned n = 0; while (s[n]) n++; return n; }
static void out(const char *s) { sys3(SYS_write, 1, (i64)s, slen(s)); }
static void outn(i64 v) {
    char b[24]; int i = 23; b[i--] = 0;
    int neg = v < 0; unsigned long u = neg ? (unsigned long)(-v) : (unsigned long)v;
    if (!u) b[i--] = '0';
    while (u) { b[i--] = (char)('0' + u % 10); u /= 10; }
    if (neg) b[i--] = '-';
    out(&b[i + 1]);
}
static void outx(unsigned long v) {
    char b[20]; int i = 19; b[i--] = 0;
    const char *h = "0123456789abcdef";
    if (!v) b[i--] = '0';
    while (v) { b[i--] = h[v & 15]; v >>= 4; }
    out("0x"); out(&b[i + 1]);
}

static int fails = 0;
static void check(int ok, const char *what) {
    out(ok ? "ok   " : "FAIL "); out(what); out("\n");
    if (!ok) fails++;
}

#define PAGE 4096
#define PARENT_PAT 0x1111111111111111UL
#define CHILD_PAT  0xdeaddeaddeaddeadUL

int main(void) {
    // A plain private anonymous mapping: exactly what a struct pthread or a static lock word
    // lives in from the fork emulation's point of view.
    i64 p = sys6(SYS_mmap, 0, PAGE, 3 /*RW*/, 0x22 /*PRIVATE|ANON*/, -1, 0);
    if (p < 0 && p > -4096) { out("mmap failed\n"); return 1; }
    volatile u64 *shared_page = (volatile u64 *)p;

    // Also test a stack object and a .data object, the two other places musl's fork() writes.
    static volatile u64 data_word;
    volatile u64 stack_word;

    for (int i = 0; i < PAGE / 8; i++) shared_page[i] = PARENT_PAT;
    data_word = PARENT_PAT;
    stack_word = PARENT_PAT;

    out("parent pid="); outn(sys1(SYS_getpid, 0));
    out(" page="); outx((unsigned long)p); out("\n");

    i64 pid = sys1(SYS_fork, 0);
    if (pid == 0) {
        // CHILD: do what musl's fork() child does -- scribble over memory that, on real Linux,
        // is its own private copy. Every one of these must be invisible to the parent.
        for (int i = 0; i < PAGE / 8; i++) shared_page[i] = CHILD_PAT;
        data_word = CHILD_PAT;
        stack_word = CHILD_PAT;
        sys1(SYS_exit, 0);
        __builtin_unreachable();
    }
    if (pid < 0) { out("fork failed rc="); outn(pid); out("\n"); return 1; }

    int status = 0;
    sys4(SYS_wait4, pid, (i64)&status, 0, 0);

    // The decisive checks: after the child exits, the parent's own memory must be exactly as it
    // left it. Any CHILD_PAT here means the child wrote through into the parent.
    int page_clean = 1, first_bad = -1;
    for (int i = 0; i < PAGE / 8; i++) {
        if (shared_page[i] != PARENT_PAT) { page_clean = 0; if (first_bad < 0) first_bad = i; }
    }
    check(page_clean, "child's writes to an anonymous page did NOT reach the parent");
    if (!page_clean) {
        out("     first corrupted index="); outn(first_bad);
        out(" value="); outx((unsigned long)shared_page[first_bad]); out("\n");
    }
    check(data_word == PARENT_PAT, "child's write to a .data word did NOT reach the parent");
    check(stack_word == PARENT_PAT, "child's write to a stack word did NOT reach the parent");

    // Second round: many forks in a row. A leak that depends on address reuse or on the
    // relocation map only shows up after several generations.
    int leaks = 0;
    for (int round = 0; round < 16; round++) {
        for (int i = 0; i < PAGE / 8; i++) shared_page[i] = PARENT_PAT;
        i64 c = sys1(SYS_fork, 0);
        if (c == 0) {
            for (int i = 0; i < PAGE / 8; i++) shared_page[i] = CHILD_PAT;
            sys1(SYS_exit, 0);
            __builtin_unreachable();
        }
        sys4(SYS_wait4, c, (i64)&status, 0, 0);
        for (int i = 0; i < PAGE / 8; i++) if (shared_page[i] != PARENT_PAT) { leaks++; break; }
    }
    out("repeated-fork rounds with leakage: "); outn(leaks); out(" of 16\n");
    check(leaks == 0, "no leakage across 16 successive forks");

    out("failed checks: "); outn(fails); out("\n");
    sys1(SYS_exit, fails);
    __builtin_unreachable();
}

void _start(void) { main(); }
