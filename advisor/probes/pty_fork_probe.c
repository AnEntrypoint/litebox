// Verifies genuine CROSS-PROCESS pty I/O over `SharedPtyTable` (litebox_shim_linux/src/
// syscalls/pty.rs, landed acb8615). That pass wired ptmx_open/pts_open/pty_master_read/write
// into a shared-arena-native fixed table (8 slots, two SharedByteRings per slot) so a pty
// allocated by one OS process is still openable-by-id and carries real bytes to/from a
// DIFFERENT OS process -- but explicitly left this live-unverified (script's interactive-stdin
// assumption broke under non-interactive redirection). This probe is the follow-up: a tiny
// freestanding guest binary, no libc, that does the whole thing itself.
//
// Shape:
//   1. open("/dev/ptmx") -> master fd (this process becomes the pty's LOCAL owning process;
//      GlobalState::ptmx_open publishes a SharedPtyTable slot for it, see pty.rs:1053).
//   2. TIOCSPTLCK(0) to unlock, TIOCGPTN to read the pty id.
//   3. fork() (raw syscall 57 -- becomes the genuine cross-process path when
//      LITEBOX_PROCESS_FORK=1 and the process is fork-eligible; falls back to the thread-based
//      path otherwise, so a run without the env var is the CONTROL for this probe, not a
//      failure).
//   4. The CHILD sleeps briefly then opens "/dev/pts/<id>" itself -- it has no local
//      pty_registry entry for an id it didn't allocate (pty fds are not carried across fork,
//      same as AF_UNIX sockets), so this open must take the SharedSlave fallback
//      (GlobalStateHandle::pts_open, pty.rs:~1098) to succeed at all.
//   5. The PARENT writes a marker string containing its own pid to the master fd AFTER the
//      fork() call returns -- proving any data observed by the child was written by a
//      genuinely separate process after the fork point, not copied along with it.
//   6. The CHILD polls its shared-transport slave fd (no real cross-process wakeup exists, see
//      pty.rs's own doc comment -- poll_shared re-checks on a bounded interval) until it reads
//      the marker or times out, then echoes exactly what it read plus its own pid to stdout.
//
// A clean run shows the child's own pid (proving separate-process rather than shared-thread
// execution) printing the exact bytes the parent's pid wrote, sourced only from the shared
// ring -- the child's local pty_registry never had this id.
//
// Build ON THE HOST (no guest toolchain needed -- see advisor/probes/README.md):
//   clang --target=x86_64-unknown-linux-gnu -nostdlib -nostdinc -ffreestanding \
//         -fno-stack-protector -static -O1 -o pty_fork_probe pty_fork_probe.c
// Run as the runner's TOP-LEVEL program from its own tar layer (never via `sh -c`):
//   -- /pty_fork_probe
// Exit code: 0 on a clean pass (child observed the parent's post-fork marker via the shared
// ring), 1 on a slave-open failure, 2 on a read-timeout (shared ring never delivered the data).

typedef long i64;
typedef unsigned long u64;
typedef unsigned int u32;
typedef int i32;

static i64 sys0(i64 n) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n) : "rcx", "r11", "memory"); return r; }
static i64 sys1(i64 n, i64 a) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a) : "rcx", "r11", "memory"); return r; }
static i64 sys2(i64 n, i64 a, i64 b) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b) : "rcx", "r11", "memory"); return r; }
static i64 sys3(i64 n, i64 a, i64 b, i64 c) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory"); return r; }
static i64 sys4(i64 n, i64 a, i64 b, i64 c, i64 d) { i64 r; register i64 r10 __asm__("r10") = d; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory"); return r; }

#define SYS_read 0
#define SYS_write 1
#define SYS_open 2
#define SYS_close 3
#define SYS_ioctl 16
#define SYS_nanosleep 35
#define SYS_fork 57
#define SYS_exit 60
#define SYS_wait4 61
#define SYS_getpid 39

#define O_RDWR 2
#define TIOCGPTN   0x80045430
#define TIOCSPTLCK 0x40045431

static unsigned slen(const char *s) { unsigned n = 0; while (s[n]) n++; return n; }
static void out(const char *s) { sys3(SYS_write, 1, (i64)s, slen(s)); }
static void outbuf(const char *s, unsigned n) { sys3(SYS_write, 1, (i64)s, n); }
static void outn(i64 v) {
    char b[24]; int i = 23; b[i--] = 0;
    int neg = v < 0; u64 u = neg ? (u64)(-v) : (u64)v;
    if (!u) b[i--] = '0';
    while (u) { b[i--] = (char)('0' + u % 10); u /= 10; }
    if (neg) b[i--] = '-';
    out(&b[i + 1]);
}
static void sleep_ms(long ms) {
    i64 ts[2]; ts[0] = ms / 1000; ts[1] = (ms % 1000) * 1000000L;
    sys2(SYS_nanosleep, (i64)ts, 0);
}
static unsigned u32_to_str(u32 v, char *b) {
    char tmp[12]; int i = 0;
    if (v == 0) { b[0] = '0'; return 1; }
    while (v) { tmp[i++] = (char)('0' + v % 10); v /= 10; }
    unsigned n = 0;
    while (i) b[n++] = tmp[--i];
    return n;
}
// Byte-at-a-time copy, deliberately not a `char buf[N] = "literal"` array initializer: at -O1
// clang lowers that shape into an aligned SSE `movaps` store pair sized to the destination
// array, assuming 16-byte stack alignment that this freestanding, no-crt0 binary's `_start` ->
// `main` call chain does not actually guarantee at every call site -- live-caught as a
// deterministic `STATUS_ACCESS_VIOLATION` on exactly such a `movaps` (see
// docs/AGENTS_ARCHIVE_2026-09-22.md's pty cross-process I/O verification pass). A manual
// byte-copy loop can only ever emit ordinary scalar stores.
static unsigned copy_str(char *dst, const char *src) {
    unsigned n = 0;
    while (src[n]) { dst[n] = src[n]; n++; }
    return n;
}

static int main(void);

void _start(void) {
    i64 rc = main();
    sys1(SYS_exit, rc);
}

static int main(void) {
    i64 pid_self = sys1(SYS_getpid, 0);
    out("PROBE_START pid="); outn(pid_self); out("\n");

    i64 master = sys3(SYS_open, (i64)"/dev/ptmx", O_RDWR, 0);
    if (master < 0) { out("OPEN_PTMX_FAILED\n"); return 1; }
    out("PTMX_OPEN master_fd="); outn(master); out("\n");

    i32 zero = 0;
    i64 r = sys3(SYS_ioctl, master, TIOCSPTLCK, (i64)&zero);
    out("TIOCSPTLCK rc="); outn(r); out("\n");

    u32 ptn = 0xFFFFFFFFu;
    r = sys3(SYS_ioctl, master, TIOCGPTN, (i64)&ptn);
    out("TIOCGPTN rc="); outn(r); out(" id="); outn((i64)ptn); out("\n");
    if (r < 0 || ptn == 0xFFFFFFFFu) { out("TIOCGPTN_FAILED\n"); return 1; }

    char path[32];
    unsigned base = copy_str(path, "/dev/pts/");
    unsigned idlen = u32_to_str(ptn, path + base);
    path[base + idlen] = 0;

    i64 pid = sys1(SYS_fork, 0);
    if (pid == 0) {
        // CHILD -- a genuinely separate OS process under LITEBOX_PROCESS_FORK=1's cross-process
        // path. Deliberately has NO local pty_registry entry for `ptn`; opening it must take
        // SharedPtyTable's SharedSlave fallback.
        i64 my_pid = sys1(SYS_getpid, 0);
        sleep_ms(300); // let the parent's post-fork write land first
        out("CHILD_START pid="); outn(my_pid); out(" opening="); out(path); out("\n");
        i64 slave = sys3(SYS_open, (i64)path, O_RDWR, 0);
        if (slave < 0) {
            out("CHILD_SLAVE_OPEN_FAILED rc="); outn(slave); out("\n");
            sys1(SYS_exit, 1);
        }
        out("CHILD_SLAVE_OPEN slave_fd="); outn(slave); out("\n");

        char buf[256];
        int attempt = 0;
        i64 n = 0;
        for (attempt = 0; attempt < 130; attempt++) {
            n = sys3(SYS_read, slave, (i64)buf, sizeof(buf) - 1);
            if (n > 0) break;
            sleep_ms(20);
        }
        if (n <= 0) {
            out("CHILD_READ_TIMEOUT pid="); outn(my_pid); out("\n");
            sys1(SYS_exit, 2);
        }
        buf[n] = 0;
        out("CHILD_READ pid="); outn(my_pid); out(" bytes="); outn(n);
        out(" data=["); outbuf(buf, (unsigned)n); out("]\n");
        sys1(SYS_exit, 0);
    }

    // PARENT -- writes strictly AFTER the fork() call returned, so any byte the child observes
    // was produced by this still-running, now-separate-from-the-child process post-fork.
    out("PARENT_FORKED child_pid="); outn(pid); out("\n");
    char msg[64];
    unsigned mlen = copy_str(msg, "MARKER_FROM_PARENT_PID_");
    mlen += u32_to_str((u32)pid_self, msg + mlen);
    msg[mlen++] = '_';
    msg[mlen++] = 'A';
    msg[mlen++] = 'F';
    msg[mlen++] = 'T';
    msg[mlen++] = 'E';
    msg[mlen++] = 'R';
    msg[mlen++] = '_';
    msg[mlen++] = 'F';
    msg[mlen++] = 'O';
    msg[mlen++] = 'R';
    msg[mlen++] = 'K';
    msg[mlen] = 0;

    i64 wn = sys3(SYS_write, master, (i64)msg, mlen);
    out("PARENT_WROTE bytes="); outn(wn); out(" data=["); outbuf(msg, mlen); out("]\n");

    i32 status = 0;
    sys4(SYS_wait4, pid, (i64)&status, 0, 0);
    out("PARENT_CHILD_EXIT status="); outn(status); out("\n");
    out("PROBE_DONE\n");
    sys1(SYS_exit, 0);
    return 0;
}
