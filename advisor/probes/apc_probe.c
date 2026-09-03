// Probe: can a parent interrupt a cloned child's thread with QueueUserAPC2 (special user APC)?
#include <windows.h>
#include <winternl.h>
#include <stdio.h>
#include <string.h>
#define STATUS_PROCESS_CLONED ((NTSTATUS)0x00000129L)
typedef struct { ULONG Length; HANDLE ProcessHandle; HANDLE ThreadHandle; CLIENT_ID ClientId; unsigned char pad[320]; } RUPI;
typedef NTSTATUS (NTAPI *RtlCloneUserProcess_t)(ULONG, PVOID, PVOID, HANDLE, RUPI *);
typedef BOOL (WINAPI *QueueUserAPC2_t)(PAPCFUNC, HANDLE, ULONG_PTR, ULONG);
typedef NTSTATUS (NTAPI *NtTerminateProcess_t)(HANDLE, NTSTATUS);
static volatile LONG *shared;
static void CALLBACK apc_fn(ULONG_PTR p) { shared[1] = (LONG)p; }
int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    HMODULE ntdll = GetModuleHandleA("ntdll.dll");
    HMODULE k32 = GetModuleHandleA("kernel32.dll");
    RtlCloneUserProcess_t clone = (RtlCloneUserProcess_t)GetProcAddress(ntdll, "RtlCloneUserProcess");
    QueueUserAPC2_t QueueUserAPC2 = (QueueUserAPC2_t)GetProcAddress(k32, "QueueUserAPC2");
    NtTerminateProcess_t NtTerminateProcess = (NtTerminateProcess_t)GetProcAddress(ntdll, "NtTerminateProcess");
    printf("QueueUserAPC2 export: %p\n", (void *)QueueUserAPC2);
    HANDLE sec = CreateFileMappingW(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0, 4096, NULL);
    shared = (volatile LONG *)MapViewOfFile(sec, FILE_MAP_ALL_ACCESS, 0, 0, 4096);
    shared[0] = 0; shared[1] = 0; shared[2] = 0;
    RUPI pi; memset(&pi, 0, sizeof pi); pi.Length = sizeof pi;
    NTSTATUS st = clone(0x2, NULL, NULL, NULL, &pi);
    if (st == STATUS_PROCESS_CLONED) {
        // child: pure user-mode spin (no alertable wait), then report
        shared[0] = 1;
        ULONGLONG t0 = GetTickCount64();
        while (shared[1] == 0 && GetTickCount64() - t0 < 5000) { /* spin in user mode */ }
        shared[2] = (shared[1] != 0) ? 1 : 2; // 1 = APC ran while spinning, 2 = timeout
        NtTerminateProcess((HANDLE)-1, 0);
        return 0;
    }
    if (st) { printf("clone failed 0x%lx\n", (unsigned long)st); return 2; }
    while (shared[0] == 0) Sleep(1);
    Sleep(50);
    BOOL ok = FALSE; DWORD err = 0;
    if (QueueUserAPC2) { ok = QueueUserAPC2(apc_fn, pi.ThreadHandle, 1234, 1 /*QUEUE_USER_APC_FLAGS_SPECIAL_USER_APC*/); err = GetLastError(); }
    printf("parent: QueueUserAPC2(special) on child thread -> ok=%d err=%lu\n", ok, err);
    WaitForSingleObject(pi.ProcessHandle, 8000);
    printf("parent: child result: shared1=%ld (expect 1234) shared2=%ld (1=APC interrupted user-mode spin)\n", shared[1], shared[2]);
    TerminateProcess(pi.ProcessHandle, 0);
    return 0;
}
