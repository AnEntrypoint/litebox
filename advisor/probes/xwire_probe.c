// Minimal raw-X11 client: connect, create a window, map it, fill it, and report
// every reply. No Xlib, no toolkit, no headers -- just the wire protocol over a
// Unix socket, so it isolates "does X work" from "does GTK work".
//
// Why this exists: every XFCE component in this environment connects to X,
// exchanges protocol successfully, stays alive indefinitely, and never draws.
// No X client has ever put a pixel on screen here. That could be the X path
// itself or something specific to GTK, and nothing in the layer can tell them
// apart -- it ships no xdpyinfo/xrandr/xmessage/xclock, and its GTK is built
// without G_ENABLE_DEBUG.
//
// This draws a large solid rectangle. If it appears in a frame capture, the X
// path works end to end and the fault is in the toolkit or above. If it does
// not, the fault is below GTK and this is a far smaller thing to debug.
//
// Build on the host (guest toolchains are broken, see README):
//   clang --target=x86_64-unknown-linux-gnu -nostdlib -nostdinc -ffreestanding \
//         -fno-stack-protector -static -O1 -o xwire_probe xwire_probe.c
// Run as the runner's top-level program, with DISPLAY resolved to the socket
// path, e.g. /tmp/.X11-unix/X0.

typedef long i64;
typedef unsigned long u64;
typedef unsigned int u32;
typedef unsigned short u16;
typedef unsigned char u8;

static i64 sys1(i64 n, i64 a){i64 r;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a):"rcx","r11","memory");return r;}
static i64 sys3(i64 n,i64 a,i64 b,i64 c){i64 r;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c):"rcx","r11","memory");return r;}

#define SYS_read 0
#define SYS_write 1
#define SYS_socket 41
#define SYS_connect 42
#define SYS_exit 60

static unsigned slen(const char*s){unsigned n=0;while(s[n])n++;return n;}
static void out(const char*s){sys3(SYS_write,1,(i64)s,slen(s));}
static void outn(i64 v){char b[24];int i=23;b[i--]=0;int neg=v<0;u64 u=neg?(u64)(-v):(u64)v;
    if(!u)b[i--]='0';while(u){b[i--]=(char)('0'+u%10);u/=10;}if(neg)b[i--]='-';out(&b[i+1]);}

struct sockaddr_un { u16 family; char path[108]; };

int main(int argc, char **argv) {
    const char *path = (argc > 1) ? argv[1] : "/tmp/.X11-unix/X0";
    out("XWIRE_START\n");

    i64 fd = sys3(SYS_socket, 1 /*AF_UNIX*/, 1 /*SOCK_STREAM*/, 0);
    if (fd < 0) { out("XWIRE_SOCKET_FAIL\n"); sys1(SYS_exit, 1); }

    struct sockaddr_un sa;
    sa.family = 1;
    for (int i = 0; i < 108; i++) sa.path[i] = 0;
    for (unsigned i = 0; i < slen(path) && i < 107; i++) sa.path[i] = path[i];
    if (sys3(SYS_connect, fd, (i64)&sa, 2 + 108) < 0) {
        out("XWIRE_CONNECT_FAIL\n"); sys1(SYS_exit, 2);
    }
    out("XWIRE_CONNECTED\n");

    // Connection setup: byte order 'l', protocol 11.0, no auth.
    u8 req[12] = {'l',0, 11,0, 0,0, 0,0, 0,0, 0,0};
    sys3(SYS_write, fd, (i64)req, 12);

    // The reply's first byte is 1 on success, 0 on refusal.
    static u8 buf[16384];
    // A single read() on a socket is NOT guaranteed to return the whole reply.
    // Observed live: one run returned the full ~1KB setup and the window appeared;
    // another returned only 8 bytes, after which parsing root/visual out of the
    // unfilled buffer produced garbage ids and nonsense events. Read the 8-byte
    // header, then loop until the declared remaining length has actually arrived.
    i64 got = 0;
    while (got < 8) {
        i64 r = sys3(SYS_read, fd, (i64)(buf + got), 8 - got);
        if (r <= 0) { out("XWIRE_SETUP_HDR_FAIL\n"); sys1(SYS_exit, 3); }
        got += r;
    }
    unsigned want = 8 + 4u * (unsigned)(*(u16 *)(buf + 6));
    if (want > sizeof buf) want = sizeof buf;
    while (got < (i64)want) {
        i64 r = sys3(SYS_read, fd, (i64)(buf + got), (i64)want - got);
        if (r <= 0) break;
        got += r;
    }
    i64 n = got;
    if (n < (i64)want) {
        out("XWIRE_SETUP_INCOMPLETE got="); outn(n);
        out(" want="); outn(want); out("\n");
        sys1(SYS_exit, 3);
    }
    out("XWIRE_SETUP_STATUS="); outn(buf[0]); out(" bytes="); outn(n); out("\n");
    if (buf[0] != 1) { out("XWIRE_SETUP_REFUSED\n"); sys1(SYS_exit, 4); }

    // Parse just enough of the setup reply to get a resource id base and a root
    // window: release(8) id_base(12) id_mask(16) ... then vendor, formats, screens.
    u32 id_base = *(u32*)(buf + 12);
    u16 vendor_len = *(u16*)(buf + 24);
    u8 num_formats = buf[29];
    unsigned off = 40 + ((vendor_len + 3) & ~3u) + 8 * num_formats;
    u32 root = *(u32*)(buf + off);
    u32 root_visual = *(u32*)(buf + off + 32);
    u16 root_w = *(u16*)(buf + off + 20);
    u16 root_h = *(u16*)(buf + off + 22);
    out("XWIRE_ROOT="); outn(root);
    out(" size="); outn(root_w); out("x"); outn(root_h);
    out(" visual="); outn(root_visual); out("\n");

    u32 wid = id_base | 1;
    u32 gc  = id_base | 2;

    // CreateWindow: opcode 1, 8 + n words. depth=0 (copy from parent).
    u32 cw[12];
    cw[0] = (1) | (0 << 8) | (10 << 16);           // opcode, depth 0, length 10
    cw[1] = wid; cw[2] = root;
    cw[3] = (0) | (0 << 16);                        // x=0 y=0
    cw[4] = (600) | (400 << 16);                    // w=600 h=400
    cw[5] = (0) | (1 << 16);                        // border 0, class InputOutput
    cw[6] = root_visual;
    cw[7] = (1 << 1) | (1 << 11);                   // BackPixel | EventMask
    cw[8] = 0x00FF00FF;                             // background: bright magenta
    cw[9] = (1 << 15);                              // ExposureMask
    sys3(SYS_write, fd, (i64)cw, 40);
    out("XWIRE_CREATEWINDOW_SENT\n");

    // MapWindow: opcode 8, length 2.
    u32 mw[2] = { (8) | (2 << 16), wid };
    sys3(SYS_write, fd, (i64)mw, 8);
    out("XWIRE_MAPWINDOW_SENT\n");

    // CreateGC + PolyFillRectangle so the window has real content even if the
    // background pixel alone is not enough to generate damage.
    u32 cg[5] = { (55) | (4 << 16), gc, wid, (1 << 2), 0x0000FF00 };
    sys3(SYS_write, fd, (i64)cg, 20);
    u32 fr[7] = { (70) | (5 << 16), wid, gc, (0) | (0 << 16), (600) | (400 << 16), 0, 0 };
    sys3(SYS_write, fd, (i64)fr, 20);
    out("XWIRE_FILL_SENT\n");

    // Drain replies/errors. An X error reply starts with byte 0 and carries the
    // error code in byte 1 -- that is what says a request was rejected.
    for (int round = 0; round < 40; round++) {
        n = sys3(SYS_read, fd, (i64)buf, sizeof buf);
        if (n <= 0) { out("XWIRE_READ_END n="); outn(n); out("\n"); break; }
        for (i64 i = 0; i + 32 <= n; i += 32) {
            if (buf[i] == 0) {
                out("XWIRE_X_ERROR code="); outn(buf[i+1]);
                out(" major="); outn(buf[i+10]);
                out(" minor="); outn(*(u16*)(buf+i+8)); out("\n");
            } else {
                out("XWIRE_EVENT type="); outn(buf[i] & 0x7f); out("\n");
            }
        }
    }
    out("XWIRE_DONE\n");
    sys1(SYS_exit, 0);
    __builtin_unreachable();
}

void _start(void) {
    long *sp; __asm__ volatile("mov %%rsp, %0" : "=r"(sp));
    main((int)sp[0], (char **)&sp[1]);
    sys1(SYS_exit, 0);
    __builtin_unreachable();
}
