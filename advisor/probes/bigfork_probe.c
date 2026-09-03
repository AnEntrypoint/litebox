// Minimal repro for the deterministic instruction-fetch fault seen in Xwayland's keymap helper.
//
// Observed shape (2026-09-03, two independent runs): a LARGE process (Xwayland: ~296 relocation
// ranges, ~47,600 pointers healed by fixup_stale_elf_data_pointers) calls fork(), and the child
// execve()s a small binary (/bin/sh -> xkbcomp). The child then faults with rip == cr2,
// error_code=0x6 (user-mode, page-not-present) at a FIXED offset 0x1464b into its text mapping,
// i.e. an instruction FETCH fault on the fall-through of a jne into a leaq. Not pointer
// corruption; the child's text page is simply not backed while litebox's VMA table says the
// region is mapped MAYEXEC.
//
// This probe reproduces that shape WITHOUT Xwayland or any of XFCE, turning an ~80 s full-stack
// run into a ~1 s test:
//   1. Inflate the parent's address space so fork() has a lot to relocate (many separate
//      mappings, mimicking Xwayland's ~296 ranges rather than one big block).
//   2. fork(), and in the child immediately execve() a small binary.
//   3. Repeat, so a fault that needs a particular timing window still shows up.
// The parent reports each child's exit status. On real Linux every child exits 0. Under litebox,
// a child killed by SIGSEGV shows up as status 11 / signalled, which is the bug.
//
// Build ON THE HOST (no guest toolchain needed -- both guest compilers are broken, see README):
//   clang --target=x86_64-unknown-linux-gnu -nostdlib -nostdinc -ffreestanding \
//         -fno-stack-protector -static -O1 -o bigfork_probe bigfork_probe.c
// Run it as the runner's TOP-LEVEL program from a tar layer (never via `sh -c`), e.g.
//   -- /bigfork_probe /bin/busybox
// Argument 1 is the binary the child should exec (default /bin/sh). Exit code = number of
// children that did not exit cleanly.

typedef long i64;
typedef unsigned long u64;

static i64 sys1(i64 n, i64 a) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a) : "rcx", "r11", "memory"); return r; }
static i64 sys3(i64 n, i64 a, i64 b, i64 c) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory"); return r; }
static i64 sys4(i64 n, i64 a, i64 b, i64 c, i64 d) { i64 r; register i64 r10 __asm__("r10") = d; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory"); return r; }
static i64 sys6(i64 n, i64 a, i64 b, i64 c, i64 d, i64 e, i64 f) { i64 r; register i64 r10 __asm__("r10") = d, r8 __asm__("r8") = e, r9 __asm__("r9") = f; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9) : "rcx", "r11", "memory"); return r; }

#define SYS_write 1
#define SYS_mmap 9
#define SYS_execve 59
#define SYS_exit 60
#define SYS_wait4 61
#define SYS_fork 57

static unsigned slen(const char *s) { unsigned n = 0; while (s[n]) n++; return n; }
static void out(const char *s) { sys3(SYS_write, 1, (i64)s, slen(s)); }
static void outn(i64 v) {
    char b[24]; int i = 23; b[i--] = 0;
    int neg = v < 0; u64 u = neg ? (u64)(-v) : (u64)v;
    if (!u) b[i--] = '0';
    while (u) { b[i--] = (char)('0' + u % 10); u /= 10; }
    if (neg) b[i--] = '-';
    out(&b[i + 1]);
}

#define RANGES 300          /* mimic Xwayland's ~296 relocation ranges */
#define RANGE_PAGES 4
#define PAGE 4096
#define ROUNDS 12

int main(int argc, char **argv) {
    const char *target = (argc > 1) ? argv[1] : "/bin/sh";

    // 1. Inflate the address space with MANY SEPARATE writable mappings. Separate mmap calls
    //    (rather than one large one) is the point: fork's relocation works per range, and the
    //    observed failure involved ~296 of them.
    int made = 0;
    for (int i = 0; i < RANGES; i++) {
        i64 p = sys6(SYS_mmap, 0, RANGE_PAGES * PAGE, 3 /*RW*/, 0x22 /*PRIVATE|ANON*/, -1, 0);
        if (p < 0 && p > -4096) break;
        volatile u64 *q = (volatile u64 *)p;
        // Touch each page and store a self-referential pointer, so the range holds real data
        // and genuine intra-process pointers for any relocation pass to find and rewrite.
        for (int pg = 0; pg < RANGE_PAGES; pg++) {
            q[pg * (PAGE / 8)] = (u64)(void *)&q[pg * (PAGE / 8)];
        }
        made++;
    }
    out("parent: mapped ranges="); outn(made); out("\n");

    // 2/3. Fork repeatedly; each child immediately execs a small binary.
    int failures = 0;
    for (int round = 0; round < ROUNDS; round++) {
        i64 pid = sys1(SYS_fork, 0);
        if (pid == 0) {
            // Child: exec straight away, the exact shape of Xwayland spawning its keymap helper.
            char *args[3];
            args[0] = (char *)target;
            args[1] = (char *)"--help";     // busybox/sh both accept this and exit promptly
            args[2] = 0;
            char *envp[1]; envp[0] = 0;
            sys3(SYS_execve, (i64)target, (i64)args, (i64)envp);
            // execve only returns on failure.
            sys1(SYS_exit, 127);
            __builtin_unreachable();
        }
        if (pid < 0) { out("fork failed\n"); return 1; }

        int status = 0;
        sys4(SYS_wait4, pid, (i64)&status, 0, 0);

        // Decode wait status: low 7 bits = terminating signal, 0 means exited normally.
        int sig = status & 0x7f;
        int code = (status >> 8) & 0xff;
        out("round "); outn(round);
        if (sig) {
            out(" child SIGNALLED sig="); outn(sig);
            if (sig == 11) out("  <-- SIGSEGV: the bug");
            failures++;
        } else {
            out(" child exited code="); outn(code);
            // A nonzero code is fine (--help conventions vary); only a signal is the bug.
        }
        out("\n");
    }

    out("children killed by a signal: "); outn(failures); out(" of "); outn(ROUNDS); out("\n");
    sys1(SYS_exit, failures);
    __builtin_unreachable();
}

void _start(void) {
    // Recover argc/argv from the stack: at _start, rsp points at argc, then argv[].
    long *sp;
    __asm__ volatile("mov %%rsp, %0" : "=r"(sp));
    int argc = (int)sp[0];
    char **argv = (char **)&sp[1];
    main(argc, argv);
    sys1(SYS_exit, 0);
    __builtin_unreachable();
}
