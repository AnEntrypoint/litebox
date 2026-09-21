// LD_PRELOAD interposer: traces every getenv() call made by the process it is
// preloaded into, and dumps the raw `environ` array at load time (constructor)
// and at process exit (destructor). No libc headers, no sysroot needed on the
// build host -- only raw syscalls (via glibc's own exported `syscall()`
// trampoline, resolved dynamically at load time exactly like `environ` is) and
// hand-written string helpers, so this compiles with a bare clang targeting
// x86_64-unknown-linux-gnu with no glibc dev headers/libs present on the host.
//
// getenv() defined here OVERRIDES glibc's real one process-wide (LD_PRELOAD
// symbol precedence) for every caller -- xfce4-session's own code, GTK, glib,
// X11 client libs, all of it -- while remaining semantically faithful (walks
// the live `environ`, first match wins, same as glibc). This gives a full
// trace of every single query instead of one point-in-time snapshot, which is
// strictly more evidence than an interactive breakpoint would give for the
// same investigation.
//
// Build (host, no sysroot):
//   clang --target=x86_64-unknown-linux-gnu -shared -fPIC -nostdlib \
//     -Wl,--unresolved-symbols=ignore-all -o getenv_probe.so getenv_probe.c
//
// Use: LD_PRELOAD=/path/getenv_probe.so <target binary>
// Log: appends to /tmp/getenv_trace.log (path fixed on purpose -- keeps this
// dead simple and avoids needing to plumb a filename through env vars that
// this very probe would then itself be asked to report on).

extern char **environ;
extern long syscall(long number, ...);

#define SYS_write 1
#define SYS_open 2
#define SYS_close 3
#define SYS_read 0
#define SYS_getpid 39

static int log_fd = -1;
static char g_comm[64] = "?";

static unsigned long my_strlen(const char *s) {
    unsigned long n = 0;
    while (s[n]) n++;
    return n;
}

static void log_write(const char *s) {
    if (log_fd < 0) return;
    syscall(SYS_write, log_fd, s, my_strlen(s));
}

static void log_write_n(const char *s, unsigned long n) {
    if (log_fd < 0) return;
    syscall(SYS_write, log_fd, s, n);
}

static int my_strncmp_prefix(const char *env_entry, const char *name, unsigned long name_len) {
    for (unsigned long i = 0; i < name_len; i++) {
        if (env_entry[i] != name[i]) return 0;
    }
    return env_entry[name_len] == '=';
}

// Faithful reimplementation: first entry whose "NAME=" prefix matches wins,
// returns pointer to the value (just past '='), or 0 (NULL) if not found --
// exactly glibc's own getenv() contract.
char *getenv(const char *name) {
    log_write("[getenv_probe][");
    log_write(g_comm);
    log_write("] GETENV query name=");
    log_write(name);
    if (environ) {
        unsigned long name_len = my_strlen(name);
        for (char **e = environ; *e; e++) {
            if (my_strncmp_prefix(*e, name, name_len)) {
                char *val = *e + name_len + 1;
                log_write(" result=[");
                log_write(val);
                log_write("]\n");
                return val;
            }
        }
    } else {
        log_write(" (environ itself is NULL!)\n");
        return 0;
    }
    log_write(" result=NULL(not found)\n");
    return 0;
}

static void dump_environ(const char *tag) {
    log_write("[getenv_probe] ");
    log_write(tag);
    log_write(" environ dump: ptr=");
    if (!environ) {
        log_write("NULL\n");
        return;
    }
    log_write("valid\n");
    int count = 0;
    for (char **e = environ; *e; e++) {
        log_write("  [");
        // simple decimal print of count
        char digits[12];
        int i = 0;
        int n = count;
        if (n == 0) { digits[i++] = '0'; }
        while (n > 0) { digits[i++] = (char)('0' + (n % 10)); n /= 10; }
        while (i > 0) { char c = digits[--i]; log_write_n(&c, 1); }
        log_write("] ");
        log_write(*e);
        log_write("\n");
        count++;
    }
}

__attribute__((constructor))
static void probe_init(void) {
    log_fd = (int)syscall(SYS_open, "/tmp/getenv_trace.log", 0101 /*O_WRONLY|O_CREAT*/ | 02000 /*O_APPEND*/, 0666);
    {
        int cfd = (int)syscall(SYS_open, "/proc/self/comm", 0 /*O_RDONLY*/, 0);
        if (cfd >= 0) {
            long n = syscall(SYS_read, cfd, g_comm, (unsigned long)sizeof(g_comm) - 1);
            if (n < 0) n = 0;
            g_comm[n] = 0;
            // strip trailing newline /proc/self/comm always has
            if (n > 0 && g_comm[n - 1] == '\n') g_comm[n - 1] = 0;
            syscall(SYS_close, cfd);
        }
    }
    long pid = syscall(SYS_getpid);
    char pidbuf[24];
    int i = 0, n = (int)pid;
    if (n == 0) pidbuf[i++] = '0';
    char tmp[24]; int ti = 0;
    while (n > 0) { tmp[ti++] = (char)('0' + (n % 10)); n /= 10; }
    while (ti > 0) pidbuf[i++] = tmp[--ti];
    pidbuf[i] = 0;
    log_write("[getenv_probe] ===== CONSTRUCTOR pid=");
    log_write(pidbuf);
    log_write(" =====\n");
    dump_environ("CONSTRUCTOR (earliest observable point)");
}

__attribute__((destructor))
static void probe_fini(void) {
    dump_environ("DESTRUCTOR (process exit)");
    log_write("[getenv_probe] ===== DESTRUCTOR done =====\n");
    if (log_fd >= 0) syscall(SYS_close, log_fd);
}
