// socketpair -> fork -> child EXECVEs /bin/sh which writes to the inherited fd.
// fork alone is proven fine (sp3); this isolates execve as the variable, using a
// REAL dynamically-linked binary so the exec actually succeeds.
typedef long i64; typedef unsigned long u64;
static i64 sys1(i64 n,i64 a){i64 r;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a):"rcx","r11","memory");return r;}
static i64 sys3(i64 n,i64 a,i64 b,i64 c){i64 r;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c):"rcx","r11","memory");return r;}
static i64 sys4(i64 n,i64 a,i64 b,i64 c,i64 d){i64 r;register i64 r10 __asm__("r10")=d;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10):"rcx","r11","memory");return r;}
#define SYS_write 1
#define SYS_close 3
#define SYS_fork 57
#define SYS_execve 59
#define SYS_exit 60
#define SYS_wait4 61
#define SYS_socketpair 53
#define SYS_nanosleep 35
static unsigned slen(const char*s){unsigned n=0;while(s[n])n++;return n;}
static void out(const char*s){sys3(SYS_write,1,(i64)s,slen(s));}
static void outn(i64 v){char b[24];int i=23;b[i--]=0;int neg=v<0;u64 u=neg?(u64)(-v):(u64)v;if(!u)b[i--]='0';while(u){b[i--]=(char)('0'+u%10);u/=10;}if(neg)b[i--]='-';out(&b[i+1]);}
static void naptime(long ms){ long ts[2]; ts[0]=ms/1000; ts[1]=(ms%1000)*1000000L; sys3(SYS_nanosleep,(i64)ts,0,0); }

int main(void){
    int sv[2];
    if (sys4(SYS_socketpair, 1, 1, 0, (i64)sv) < 0) { out("socketpair failed\n"); sys1(SYS_exit,1); }
    out("socketpair fds="); outn(sv[0]); out(","); outn(sv[1]); out("\n");

    i64 pid = sys1(SYS_fork, 0);
    if (pid == 0) {
        // Child: exec a REAL binary that writes to the inherited fd. /bin/sh's
        // redirection (>&N) exercises exactly the inherited-fd path glycin uses.
        char *args[4]; args[0]=(char*)"/bin/sh"; args[1]=(char*)"-c";
        args[2]=(char*)"sleep 1; echo AUTH >&4 && echo '  POST-EXEC child wrote AFTER parent close: OK' || echo '  POST-EXEC child FAILED (EPIPE?) after parent close'";
        args[3]=0;
        char *envp[1]; envp[0]=0;
        sys3(SYS_execve,(i64)"/bin/sh",(i64)args,(i64)envp);
        out("  execve FAILED\n");
        sys1(SYS_exit,127);
    }
    naptime(200);
    sys1(SYS_close, sv[1]);       // parent drops its copy, as glib does
    int st=0; sys4(SYS_wait4, pid, (i64)&st, 0, 0);
    out("child status="); outn(st); out("\n");
    out("DONE\n");
    sys1(SYS_exit,0);
    __builtin_unreachable();
}
void _start(void){ main(); sys1(SYS_exit,0); __builtin_unreachable(); }
