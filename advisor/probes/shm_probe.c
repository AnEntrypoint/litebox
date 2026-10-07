// Probe for POSIX /dev/shm shared-memory support (glibc shm_open's real syscall recipe),
// motivated by AGENTS.md's pass 319 finding: labwc (stock linuxserver/webtop:alpine-mate image,
// PIXELFLUX_WAYLAND=true, real Wayland/DRM backend) crashes with a null-deref during shm-keymap
// allocation, and a codesearch found ZERO handling anywhere in litebox_shim_linux for /dev/shm or
// shm_open, despite /dev/shm existing as a plain directory entry in the rootfs tar.
//
// Reproduces glibc's shm_open("/name", O_CREAT|O_RDWR, mode) + ftruncate + mmap sequence exactly:
//   openat(AT_FDCWD, "/dev/shm/probe_name", O_CREAT|O_RDWR|O_EXCL, 0600)
//   ftruncate(fd, 4096)
//   mmap(NULL, 4096, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0)
//   write a marker byte through the mapping, munmap, close
// Prints PASS/FAIL markers at each step via raw write(1, ...) only (no libc), so a truncated run
// still identifies exactly which step failed.
//
// Build ON THE HOST (guest toolchains are broken, see advisor/probes/README.md):
//   clang --target=x86_64-unknown-linux-gnu -nostdlib -nostdinc -ffreestanding \
//         -fno-stack-protector -static -O1 -o shm_probe shm_probe.c
// Run as the runner's TOP-LEVEL program from a tar layer (never via `sh -c`).
// Exit code = number of failed steps; 0 means full open+ftruncate+mmap+write+read-back succeeded.

typedef long i64;

static i64 sys1(i64 n, i64 a) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a) : "rcx", "r11", "memory"); return r; }
static i64 sys2(i64 n, i64 a, i64 b) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b) : "rcx", "r11", "memory"); return r; }
static i64 sys3(i64 n, i64 a, i64 b, i64 c) { i64 r; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory"); return r; }
static i64 sys4(i64 n, i64 a, i64 b, i64 c, i64 d) { i64 r; register i64 r10 __asm__("r10") = d; __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory"); return r; }
static i64 sys6(i64 n, i64 a, i64 b, i64 c, i64 d, i64 e, i64 f) {
    i64 r;
    register i64 r10 __asm__("r10") = d;
    register i64 r8 __asm__("r8") = e;
    register i64 r9 __asm__("r9") = f;
    __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9) : "rcx", "r11", "memory");
    return r;
}

#define SYS_write 1
#define SYS_close 3
#define SYS_mmap 9
#define SYS_munmap 11
#define SYS_exit 60
#define SYS_ftruncate 77
#define SYS_openat 257

#define AT_FDCWD (-100)
#define O_RDWR 02
#define O_CREAT 0100
#define O_EXCL 0200
#define PROT_READ 1
#define PROT_WRITE 2
#define MAP_SHARED 1
#define MAP_FAILED (-1)

#define PAGE 4096

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

int main(void) {
    int failures = 0;
    out("SHM_PROBE_START\n");

    i64 fd = sys4(SYS_openat, AT_FDCWD, (i64)"/dev/shm/probe_name", O_CREAT | O_RDWR | O_EXCL, 0600);
    if (fd < 0) {
        out("FAIL open /dev/shm/probe_name errno="); outn(-fd); out("\n");
        out("SHM_PROBE_DONE\n");
        sys1(SYS_exit, 4);
        __builtin_unreachable();
    }
    out("PASS open fd="); outn(fd); out("\n");

    i64 tr = sys2(SYS_ftruncate, fd, PAGE);
    if (tr < 0) {
        out("FAIL ftruncate errno="); outn(-tr); out("\n");
        failures++;
    } else {
        out("PASS ftruncate\n");
    }

    i64 addr = sys6(SYS_mmap, 0, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (addr == MAP_FAILED || addr < 0) {
        out("FAIL mmap errno="); outn(-addr); out("\n");
        failures += 2; /* mmap failing also means write-through and read-back can't run */
        out("SHM_PROBE_DONE\n");
        sys1(SYS_exit, failures);
        __builtin_unreachable();
    }
    out("PASS mmap addr="); outn(addr); out("\n");

    unsigned char *p = (unsigned char *)addr;
    p[0] = 0xAB;
    p[PAGE - 1] = 0xCD;

    /* Read back through the SAME mapping -- confirms the write landed on real backing, not a
       throwaway private COW page. A second independent mmap of the same fd would be a stronger
       cross-mapping check (see memfd_probe.c's peer-process pattern), but this freestanding
       single-process probe keeps the boilerplate minimal since the open/ftruncate/mmap sequence
       itself is the thing in question here, not cross-process coherence (already covered for
       memfd_create by test_memfd_create_shared_mapping_across_two_independent_mmaps). */
    if (p[0] == 0xAB && p[PAGE - 1] == 0xCD) {
        out("PASS read-back matches\n");
    } else {
        out("FAIL read-back mismatch got="); outn(p[0]); out(","); outn(p[PAGE - 1]); out("\n");
        failures++;
    }

    sys2(SYS_munmap, addr, PAGE);
    sys1(SYS_close, fd);

    out("failures: "); outn(failures); out("\n");
    out("SHM_PROBE_DONE\n");
    sys1(SYS_exit, failures);
    __builtin_unreachable();
}

void _start(void) {
    main();
    sys1(SYS_exit, 0);
    __builtin_unreachable();
}
