// Guest-side conformance probe (Linux/musl): memfd + shared mappings must be coherent.
// Build in the guest: cc -O1 -o memfd_probe memfd_probe.c
// All checks pass on real Linux. Under litebox (as of 2026-09-03) the mmap-time sync and the
// handle-replacing ftruncate make checks 2-4 fail. Exit code = number of failed checks.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <pthread.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>

#define PAGE 4096
static int fails = 0;
#define CHECK(cond, ...) do { if (cond) printf("ok   " __VA_ARGS__); else { fails++; printf("FAIL " __VA_ARGS__); } printf("\n"); } while (0)

static int send_fd(int sock, int fd) {
    char c = 'x'; struct iovec iov = { &c, 1 };
    char buf[CMSG_SPACE(sizeof(int))]; memset(buf, 0, sizeof buf);
    struct msghdr m = { 0 }; m.msg_iov = &iov; m.msg_iovlen = 1; m.msg_control = buf; m.msg_controllen = sizeof buf;
    struct cmsghdr *cm = CMSG_FIRSTHDR(&m); cm->cmsg_level = SOL_SOCKET; cm->cmsg_type = SCM_RIGHTS; cm->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cm), &fd, sizeof fd);
    return sendmsg(sock, &m, 0) == 1 ? 0 : -1;
}
static int recv_fd(int sock) {
    char c; struct iovec iov = { &c, 1 };
    char buf[CMSG_SPACE(sizeof(int))];
    struct msghdr m = { 0 }; m.msg_iov = &iov; m.msg_iovlen = 1; m.msg_control = buf; m.msg_controllen = sizeof buf;
    if (recvmsg(sock, &m, MSG_CMSG_CLOEXEC) != 1) return -1;
    struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
    if (!cm || cm->cmsg_type != SCM_RIGHTS) return -1;
    int fd; memcpy(&fd, CMSG_DATA(cm), sizeof fd); return fd;
}
static int pattern_ok(const unsigned char *p, size_t len, unsigned char seed) {
    for (size_t i = 0; i < len; i++) if (p[i] != (unsigned char)(seed + (i % 251))) return 0;
    return 1;
}
static void fill(unsigned char *p, size_t len, unsigned char seed) { for (size_t i = 0; i < len; i++) p[i] = (unsigned char)(seed + (i % 251)); }

struct peer { int sock; };
static int sock_pair[2];
static pthread_barrier_t bar;

static void *peer_main(void *arg) {
    (void)arg;
    int fd = recv_fd(sock_pair[1]);
    CHECK(fd >= 0, "peer received fd over SCM_RIGHTS (fd=%d errno=%d)", fd, errno);
    // check 2: bytes the sender wrote THROUGH ITS MAPPING before we mapped are visible
    unsigned char *b = mmap(NULL, 4 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(b != MAP_FAILED, "peer mmap 4 pages (errno=%d)", errno);
    CHECK(b != MAP_FAILED && pattern_ok(b, 4 * PAGE, 1), "check2: peer sees sender's mapping writes (no mmap-time wipe)");
    pthread_barrier_wait(&bar);            // sender grows file + mremaps + writes tail
    pthread_barrier_wait(&bar);
    // check 3: grow via mremap on our side, see the sender's tail writes
    unsigned char *b2 = mremap(b, 4 * PAGE, 8 * PAGE, MREMAP_MAYMOVE);
    CHECK(b2 != MAP_FAILED, "peer mremap 4->8 pages (errno=%d)", errno);
    if (b2 != MAP_FAILED) {
        CHECK(pattern_ok(b2, 4 * PAGE, 1), "check3a: head intact after grow");
        CHECK(pattern_ok(b2 + 4 * PAGE, 4 * PAGE, 2), "check3b: peer sees sender's tail writes through the grown mapping");
        // check 4: read() path agrees with the mapping
        unsigned char *rb = malloc(8 * PAGE);
        ssize_t n = pread(fd, rb, 8 * PAGE, 0);
        CHECK(n == 8 * PAGE && pattern_ok(rb, 4 * PAGE, 1) && pattern_ok(rb + 4 * PAGE, 4 * PAGE, 2), "check4: pread() returns what the mappings hold (n=%zd)", n);
        free(rb);
    }
    pthread_barrier_wait(&bar);
    return NULL;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int fd = syscall(SYS_memfd_create, "probe", 1 /*MFD_CLOEXEC*/);
    CHECK(fd >= 0, "memfd_create (errno=%d)", errno);
    CHECK(ftruncate(fd, 4 * PAGE) == 0, "ftruncate 4 pages (errno=%d)", errno);
    unsigned char *a = mmap(NULL, 4 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(a != MAP_FAILED, "sender mmap (errno=%d)", errno);
    fill(a, 4 * PAGE, 1);                  // write through the mapping, never via write()
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sock_pair) == 0, "socketpair");
    pthread_barrier_init(&bar, NULL, 2);
    pthread_t t; pthread_create(&t, NULL, peer_main, NULL);
    CHECK(send_fd(sock_pair[0], fd) == 0, "sender sent fd (errno=%d)", errno);
    pthread_barrier_wait(&bar);            // peer has mapped and checked
    // grow the file, grow our mapping, write into the tail
    int r = posix_fallocate(fd, 0, 8 * PAGE);
    if (r == EINVAL || r == EOPNOTSUPP || r == ENOSYS) r = ftruncate(fd, 8 * PAGE) ? errno : 0;
    CHECK(r == 0, "grow file to 8 pages (r=%d)", r);
    unsigned char *a2 = mremap(a, 4 * PAGE, 8 * PAGE, MREMAP_MAYMOVE);
    CHECK(a2 != MAP_FAILED, "sender mremap 4->8 pages (errno=%d)", errno);
    if (a2 != MAP_FAILED) { CHECK(pattern_ok(a2, 4 * PAGE, 1), "check1: sender's own head intact after grow"); fill(a2 + 4 * PAGE, 4 * PAGE, 2); }
    pthread_barrier_wait(&bar);
    pthread_barrier_wait(&bar);            // peer done
    pthread_join(t, NULL);
    printf("%d check(s) failed\n", fails);
    return fails;
}
