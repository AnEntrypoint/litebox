// Minimal launch-reliability probe: the smallest possible litebox guest program.
// Writes staged markers so a silent run can be classified rather than guessed at:
//   S = first instruction of _start reached (before anything else at all)
//   M = main entered
//   W = a second write completed
//   X = about to call exit
// Any run that prints nothing never reached the guest's first instruction, which makes it a
// runner/loader-side failure rather than anything the guest program did.
// Build on the host:
//   clang --target=x86_64-unknown-linux-gnu -nostdlib -nostdinc -ffreestanding \
//         -fno-stack-protector -static -O1 -o hello_probe hello_probe.c
typedef long i64;
static i64 sys3(i64 n, i64 a, i64 b, i64 c) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory"); return r; }
static i64 sys1(i64 n, i64 a) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a) : "rcx", "r11", "memory"); return r; }
#define W2(s) sys3(1, 2, (i64)(s), sizeof(s) - 1)   /* write to stderr, unbuffered */
#define W1(s) sys3(1, 1, (i64)(s), sizeof(s) - 1)   /* write to stdout */

void _start(void) {
    W2("S");            // absolutely first thing the guest ever does
    W1("M");
    W2("W");
    W1("X\n");
    sys1(60, 0);
    __builtin_unreachable();
}
