// Fast repro for the deterministic #UD-in-a-trampoline fault that kills the first backgrounded
// service in every launch script (advisory 3F).
//
// Observed (pass_weston5.log, pass_weston6.log, bit-identical across both runs):
//   rip=0x7feffff7fb8a rsp=0xcf9f6b8 cr2=0x0 error_code=0x0 Exception(6)=#UD Signal(4) pid=7 sh
// cr2=0 and error_code=0 prove this is an invalid-opcode trap, NOT a page fault: the bytes at
// rip are mapped and readable but do not decode. rip sits 449 KB below TASK_ADDR_MAX, inside the
// top-down band where maybe_patch_exec_segment places syscall trampoline stubs. The dying task
// (pid=7) has NO execve line at all -- it is a forked child that died between fork and execve,
// while a fork_verify healing pass was still live (had_map=true, range_count=18).
//
// So the shape to reproduce is NOT "background a lot of processes" (that was the earlier,
// withdrawn "shell fragility" theory). It is much narrower:
//     a process whose text went through syscall patching
//       -> fork()
//         -> the CHILD makes syscalls (i.e. runs trampoline stubs) BEFORE execve
//
// The child making syscalls before exec is the part that matters: that is what executes a stub
// in the freshly-relocated child, which is where the bad bytes are. A child that execs
// immediately (bigfork_probe.c) skips the window and will NOT show this.
//
// Build ON THE HOST (guest toolchains are broken, see README):
//   clang --target=x86_64-unknown-linux-gnu -nostdlib -nostdinc -ffreestanding \
//         -fno-stack-protector -static -O1 -o tramp_fork_probe tramp_fork_probe.c
// Run as the runner's TOP-LEVEL program from a tar layer (never via `sh -c`).
// Exit code = number of children that died by signal. 0 means the bug did not reproduce.
//
// If a child dies, grep the run log for "fatal signal" and compare rip against
// TASK_ADDR_MAX (0x7fefffff0000). A rip within a few hundred KB below it confirms 3F.

typedef long i64;

static i64 sys1(i64 n, i64 a) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a) : "rcx", "r11", "memory"); return r; }
static i64 sys3(i64 n, i64 a, i64 b, i64 c) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory"); return r; }
static i64 sys4(i64 n, i64 a, i64 b, i64 c, i64 d) { i64 r; register i64 r10 __asm__("r10") = d; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory"); return r; }

#define SYS_write 1
#define SYS_getpid 39
#define SYS_fork 57
#define SYS_execve 59
#define SYS_exit 60
#define SYS_wait4 61
#define SYS_getppid 110

#define ROUNDS 12
#define CHILD_SYSCALLS 200   /* syscalls the child runs BEFORE exec -- the actual test */

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

int main(int argc, char **argv) {
    const char *target = (argc > 1) ? argv[1] : "/bin/true";
    out("TRAMP_PROBE_START\n");

    int failures = 0;
    for (int round = 0; round < ROUNDS; round++) {
        i64 pid = sys1(SYS_fork, 0);
        if (pid == 0) {
            // CHILD. Run many syscalls BEFORE exec. Each one goes through a patched trampoline
            // stub, which is exactly the code the fork relocation may have corrupted. If a stub
            // is bad, this loop takes #UD instead of returning.
            for (int i = 0; i < CHILD_SYSCALLS; i++) {
                sys1(SYS_getpid, 0);
                sys1(SYS_getppid, 0);
            }
            out("  child: survived pre-exec syscalls\n");
            char *args[2]; args[0] = (char *)target; args[1] = 0;
            char *envp[1]; envp[0] = 0;
            sys3(SYS_execve, (i64)target, (i64)args, (i64)envp);
            sys1(SYS_exit, 127);
            __builtin_unreachable();
        }
        if (pid < 0) { out("fork failed\n"); return 1; }

        int status = 0;
        sys4(SYS_wait4, pid, (i64)&status, 0, 0);
        int sig = status & 0x7f;
        out("round "); outn(round);
        if (sig) {
            out(" child SIGNALLED sig="); outn(sig);
            if (sig == 4) out("  <-- SIGILL: advisory 3F reproduced");
            failures++;
        } else {
            out(" child exited code="); outn((status >> 8) & 0xff);
        }
        out("\n");
    }

    out("children killed by a signal: "); outn(failures); out(" of "); outn(ROUNDS); out("\n");
    out("TRAMP_PROBE_DONE\n");
    sys1(SYS_exit, failures);
    __builtin_unreachable();
}

void _start(void) {
    long *sp;
    __asm__ volatile("mov %%rsp, %0" : "=r"(sp));
    main((int)sp[0], (char **)&sp[1]);
    sys1(SYS_exit, 0);
    __builtin_unreachable();
}
