// Probe: is RtlCloneUserProcess a usable fork() primitive on this host?
// Checks: CoW isolation of private memory, sharing of a pre-clone section view,
// inherited pipe handle usable from the child, NtCreateThreadEx (and CreateThread) in the child,
// cross-process keyed-event wake (futex substitute), grow-after-map of a SEC_RESERVE section,
// child exit code. Every step reports through the pipe so a hang is attributable.
#include <windows.h>
#include <winternl.h>
#include <stdio.h>
#include <string.h>

#define STATUS_PROCESS_CLONED ((NTSTATUS)0x00000129L)
#define RTL_CLONE_PROCESS_FLAGS_INHERIT_HANDLES 0x2

typedef struct _RTL_USER_PROCESS_INFORMATION {
    ULONG Length;
    HANDLE ProcessHandle;
    HANDLE ThreadHandle;
    CLIENT_ID ClientId;
    unsigned char ImageInformation[64];
    unsigned char pad[256];
} RTL_USER_PROCESS_INFORMATION;

typedef NTSTATUS (NTAPI *RtlCloneUserProcess_t)(ULONG, PSECURITY_DESCRIPTOR, PSECURITY_DESCRIPTOR, HANDLE, RTL_USER_PROCESS_INFORMATION *);
typedef NTSTATUS (NTAPI *NtCreateKeyedEvent_t)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, ULONG);
typedef NTSTATUS (NTAPI *NtWaitForKeyedEvent_t)(HANDLE, PVOID, BOOLEAN, PLARGE_INTEGER);
typedef NTSTATUS (NTAPI *NtReleaseKeyedEvent_t)(HANDLE, PVOID, BOOLEAN, PLARGE_INTEGER);
typedef NTSTATUS (NTAPI *NtTerminateProcess_t)(HANDLE, NTSTATUS);
typedef NTSTATUS (NTAPI *NtCreateThreadEx_t)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, HANDLE, PVOID, PVOID, ULONG, SIZE_T, SIZE_T, SIZE_T, PVOID);
typedef NTSTATUS (NTAPI *NtWaitForSingleObject_t)(HANDLE, BOOLEAN, PLARGE_INTEGER);

static volatile int g_private = 1;
static HANDLE g_wr;

static void say(const char *s) { DWORD w; WriteFile(g_wr, s, (DWORD)strlen(s), &w, NULL); }

static DWORD WINAPI child_thread(LPVOID p) { *(volatile int *)p = 77; return 0; }

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    HMODULE ntdll = GetModuleHandleA("ntdll.dll");
    RtlCloneUserProcess_t RtlCloneUserProcess = (RtlCloneUserProcess_t)GetProcAddress(ntdll, "RtlCloneUserProcess");
    NtCreateKeyedEvent_t NtCreateKeyedEvent = (NtCreateKeyedEvent_t)GetProcAddress(ntdll, "NtCreateKeyedEvent");
    NtWaitForKeyedEvent_t NtWaitForKeyedEvent = (NtWaitForKeyedEvent_t)GetProcAddress(ntdll, "NtWaitForKeyedEvent");
    NtReleaseKeyedEvent_t NtReleaseKeyedEvent = (NtReleaseKeyedEvent_t)GetProcAddress(ntdll, "NtReleaseKeyedEvent");
    NtTerminateProcess_t NtTerminateProcess = (NtTerminateProcess_t)GetProcAddress(ntdll, "NtTerminateProcess");
    NtCreateThreadEx_t NtCreateThreadEx = (NtCreateThreadEx_t)GetProcAddress(ntdll, "NtCreateThreadEx");
    NtWaitForSingleObject_t NtWaitForSingleObject = (NtWaitForSingleObject_t)GetProcAddress(ntdll, "NtWaitForSingleObject");
    if (!RtlCloneUserProcess || !NtCreateKeyedEvent || !NtCreateThreadEx) { printf("missing ntdll exports\n"); return 2; }

    HANDLE sec = CreateFileMappingW(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE | SEC_RESERVE, 0, 1 << 20, NULL);
    volatile int *shared = (volatile int *)MapViewOfFile(sec, FILE_MAP_ALL_ACCESS, 0, 0, 1 << 20);
    if (!VirtualAlloc((LPVOID)shared, 4096, MEM_COMMIT, PAGE_READWRITE)) { printf("commit page1 failed %lu\n", GetLastError()); return 2; }
    shared[0] = 0; shared[1] = 0;

    SECURITY_ATTRIBUTES sa = { sizeof(sa), NULL, TRUE };
    HANDLE rd;
    if (!CreatePipe(&rd, &g_wr, &sa, 0)) { printf("CreatePipe failed\n"); return 2; }

    HANDLE kev = NULL;
    OBJECT_ATTRIBUTES oa; memset(&oa, 0, sizeof oa); oa.Length = sizeof oa; oa.Attributes = 0x2 /*OBJ_INHERIT*/;
    NTSTATUS st = NtCreateKeyedEvent(&kev, 0x1F0003, &oa, 0);
    if (st) { printf("NtCreateKeyedEvent failed %lx\n", (unsigned long)st); return 2; }

    LARGE_INTEGER t5; t5.QuadPart = -50000000LL; // 5 s relative
    RTL_USER_PROCESS_INFORMATION pi; memset(&pi, 0, sizeof pi); pi.Length = sizeof pi;
    printf("parent: about to clone\n");
    st = RtlCloneUserProcess(RTL_CLONE_PROCESS_FLAGS_INHERIT_HANDLES, NULL, NULL, NULL, &pi);
    if (st == STATUS_PROCESS_CLONED) {
        char msg[256];
        say("c1: child running\n");
        g_private = 2;
        shared[0] = 42;
        say("c2: wrote private+shared\n");
        volatile int t = 0;
        HANDLE th = NULL;
        NTSTATUS ts = NtCreateThreadEx(&th, 0x1FFFFF, NULL, (HANDLE)-1, (PVOID)child_thread, (PVOID)&t, 0, 0, 0, 0, NULL);
        if (ts == 0) { NtWaitForSingleObject(th, FALSE, &t5); }
        snprintf(msg, sizeof msg, "c3: NtCreateThreadEx status=0x%lx thread_ran=%d\n", (unsigned long)ts, t == 77);
        say(msg);
        NtReleaseKeyedEvent(kev, (PVOID)&shared[1], FALSE, &t5);
        say("c4: released parent\n");
        NTSTATUS ws = NtWaitForKeyedEvent(kev, (PVOID)&shared[2], FALSE, &t5);
        snprintf(msg, sizeof msg, "c5: wait status=0x%lx page2=%d (expect 4242)\n", (unsigned long)ws, ((volatile int *)((char *)shared + 4096))[0]);
        say(msg);
        // Risky kernel32 path last: CreateThread (may need CSRSS). Report only if it returns.
        volatile int t2 = 0;
        HANDLE th2 = CreateThread(NULL, 0, child_thread, (LPVOID)&t2, 0, NULL);
        if (th2) { WaitForSingleObject(th2, 5000); }
        snprintf(msg, sizeof msg, "c6: CreateThread handle=%p ran=%d\n", (void *)th2, t2 == 77);
        say(msg);
        NtTerminateProcess((HANDLE)-1, 33);
        return 33;
    }
    if (st != 0) { printf("RtlCloneUserProcess failed: 0x%lx\n", (unsigned long)st); return 2; }

    printf("parent: clone ok, child pid=%lu\n", (unsigned long)(ULONG_PTR)pi.ClientId.UniqueProcess);
    st = NtWaitForKeyedEvent(kev, (PVOID)&shared[1], FALSE, &t5);
    printf("parent: keyed wait status=0x%lx (0 = woken by child)\n", (unsigned long)st);
    void *p2 = VirtualAlloc((char *)shared + 4096, 4096, MEM_COMMIT, PAGE_READWRITE);
    if (p2) ((volatile int *)p2)[0] = 4242; else printf("parent: commit page2 failed %lu\n", GetLastError());
    st = NtReleaseKeyedEvent(kev, (PVOID)&shared[2], FALSE, &t5);
    printf("parent: release status=0x%lx\n", (unsigned long)st);
    DWORD w = WaitForSingleObject(pi.ProcessHandle, 15000);
    DWORD code = 0; GetExitCodeProcess(pi.ProcessHandle, &code);
    if (w != WAIT_OBJECT_0) { printf("parent: child did not exit, terminating\n"); TerminateProcess(pi.ProcessHandle, 99); }
    CloseHandle(g_wr);
    char buf[1024]; DWORD got = 0, total = 0;
    while (total < sizeof buf - 1 && ReadFile(rd, buf + total, sizeof buf - 1 - total, &got, NULL) && got) total += got;
    buf[total] = 0;
    printf("parent: from child:\n%s", buf);
    printf("parent: g_private=%d (expect 1, CoW) shared0=%d (expect 42) child_exit=%lu (expect 33)\n",
           g_private, shared[0], (unsigned long)code);
    return 0;
}
